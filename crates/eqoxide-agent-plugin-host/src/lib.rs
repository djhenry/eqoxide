//! In-client plumbing for the Agent Plugin API (spec §4): owns the Unix domain socket server,
//! performs the version handshake, latches each `Step` into the existing `CameraSlots`/
//! `CommandState` mailboxes, and pushes an `Observation` every tick. Deliberately thin — no
//! policy/decision logic lives here (spec §4, §12).

pub mod legal_actions;
pub mod observation_builder;
pub mod session;

use eqoxide_command::CommandState;
use eqoxide_core::spells::SpellDb;
use eqoxide_ipc::{CameraSlots, GameStateSnapshot, NetThreadDeadShared};
use eqoxide_nav::collision::SharedCollision;
use std::path::PathBuf;
use std::sync::Arc;

/// Bind the agent socket and start accepting connections on its own thread (mirrors
/// `eqoxide_http::spawn_camera_server`'s own-thread-plus-own-tokio-runtime pattern). One
/// connection is served at a time (spec §10) — see this module's doc comment / this task's plan
/// entry for why that's the right choice for a single-character session.
pub fn spawn_agent_plugin_host(
    camera: CameraSlots,
    command: CommandState,
    game_state: GameStateSnapshot,
    shared_collision: SharedCollision,
    spells: Arc<SpellDb>,
    net_thread_dead: NetThreadDeadShared,
    socket_path: PathBuf,
) {
    std::thread::Builder::new()
        .name("agent-plugin-host".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("agent-plugin-host tokio runtime");
            rt.block_on(async move {
                if socket_path.exists() {
                    let _ = std::fs::remove_file(&socket_path);
                }
                let listener = match tokio::net::UnixListener::bind(&socket_path) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!("agent-plugin-host: failed to bind {}: {e}", socket_path.display());
                        return;
                    }
                };
                tracing::info!("agent-plugin-host: listening on {}", socket_path.display());
                loop {
                    let (stream, _addr) = match listener.accept().await {
                        Ok(pair) => pair,
                        Err(e) => {
                            tracing::warn!("agent-plugin-host: accept failed: {e}");
                            continue;
                        }
                    };
                    tracing::info!("agent-plugin-host: agent connected");
                    session::run(
                        stream,
                        camera.clone(),
                        command.clone(),
                        game_state.clone(),
                        shared_collision.clone(),
                        spells.clone(),
                        net_thread_dead.clone(),
                    )
                    .await;
                    tracing::info!("agent-plugin-host: agent disconnected");
                }
            });
        })
        .expect("spawn agent-plugin-host thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_core::game_state::GameState;

    #[tokio::test]
    async fn spawn_agent_plugin_host_binds_a_socket_file_that_accepts_a_connection() {
        let dir = std::env::temp_dir().join(format!("eqoxide-agent-host-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("test.sock");

        spawn_agent_plugin_host(
            CameraSlots::for_test(),
            CommandState::default(),
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default())),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(SpellDb::default()),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            socket_path.clone(),
        );

        // Give the spawned thread a moment to bind. Polling instead of a fixed sleep so this isn't
        // flaky under load.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !socket_path.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(socket_path.exists(), "the socket file must exist once the thread has bound it");

        let connected = tokio::net::UnixStream::connect(&socket_path).await;
        assert!(connected.is_ok(), "a client must be able to connect to the bound socket");

        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// Fix 10d: a client that connects and drops (without completing the handshake) must not wedge
    /// the accept loop — a second client must still be able to connect and complete a handshake
    /// afterward, within a bounded time.
    #[tokio::test]
    async fn a_dropped_connection_does_not_prevent_a_later_client_from_connecting_and_handshaking() {
        use eqoxide_agent_protocol::framing::{decode_line, encode_line};
        use eqoxide_agent_protocol::handshake::{Hello, HandshakeReply, PROTOCOL_VERSION};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let dir = std::env::temp_dir()
            .join(format!("eqoxide-agent-host-reconnect-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("test.sock");

        spawn_agent_plugin_host(
            CameraSlots::for_test(),
            CommandState::default(),
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default())),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(SpellDb::default()),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            socket_path.clone(),
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !socket_path.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(socket_path.exists());

        // First client: connect, then drop immediately without sending Hello. The server's
        // handshake read sees this as an immediate EOF and returns promptly (no need to wait out
        // HANDSHAKE_TIMEOUT here) — session::run() returns, and the accept loop (spec §10 serves
        // one connection at a time) goes back to accepting. A client that instead stays connected
        // but silent is what exercises HANDSHAKE_TIMEOUT itself; that path is covered directly by
        // session::tests::handshake_times_out_on_a_connection_that_never_sends_hello.
        let first = tokio::net::UnixStream::connect(&socket_path).await.expect("first connect");
        drop(first);

        // Second client, connected right after: must be able to complete a full handshake well
        // within a bounded time, proving the accept loop kept accepting instead of wedging behind
        // the first (abandoned) connection's session.
        let second_result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let stream = tokio::net::UnixStream::connect(&socket_path).await.expect("second connect");
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = tokio::io::BufReader::new(read_half);
            write_half
                .write_all(encode_line(&Hello { protocol_version: PROTOCOL_VERSION }).unwrap().as_bytes())
                .await
                .unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            decode_line::<HandshakeReply>(&line).unwrap()
        })
        .await;

        assert_eq!(
            second_result.expect("a second client must be able to connect and handshake"),
            HandshakeReply::Accepted
        );

        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_dir(&dir);
    }
}
