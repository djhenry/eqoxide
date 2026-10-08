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
use eqoxide_zone_geometry::collision::SharedCollision;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Bind the agent socket synchronously, so the caller can fail the launch on an unavailable path
/// (matching `--api-port`'s fail-fast precedent in `src/main.rs`) instead of a background thread
/// dying quietly with nothing but a tracing line to show for it.
///
/// A stale/pre-existing file at `socket_path` (left behind by a crashed prior run) is removed
/// first, best-effort — `remove_file` unlinks the path itself rather than following it, so a
/// symlink planted at the path can't redirect this into removing something else. The umask is
/// restricted around the `bind` call itself, not fixed up afterward with a separate `chmod`: a
/// bind-then-chmod sequence leaves a real window where the socket is briefly reachable at
/// whatever mode the ambient umask produces, before this process's own user-only intent lands.
/// Local-control-plane socket, unauthenticated at the protocol level (spec §4) — the filesystem
/// permission bit is this socket's only access control.
pub fn bind_agent_socket(socket_path: &Path) -> std::io::Result<std::os::unix::net::UnixListener> {
    if socket_path.exists() {
        tracing::warn!(
            "agent-plugin-host: {} already exists, removing a stale/pre-existing socket file \
             before binding",
            socket_path.display()
        );
        let _ = std::fs::remove_file(socket_path);
    }
    // SAFETY: umask(2) has no preconditions and affects only this process's file-creation mode;
    // restored immediately after the one `bind` call it's guarding.
    let old_umask = unsafe { libc::umask(0o077) };
    let result = std::os::unix::net::UnixListener::bind(socket_path);
    unsafe { libc::umask(old_umask) };
    result
}

/// Start accepting connections on the given, already-bound socket, on its own thread (mirrors
/// `eqoxide_http::spawn_camera_server`'s own-thread-plus-own-tokio-runtime pattern). One
/// connection is served at a time (spec §10) — see this module's doc comment / this task's plan
/// entry for why that's the right choice for a single-character session. `listener` must come
/// from [`bind_agent_socket`] — binding is the caller's responsibility precisely so a bind
/// failure can be fatal to the launch rather than silent.
pub fn spawn_agent_plugin_host(
    camera: CameraSlots,
    command: CommandState,
    game_state: GameStateSnapshot,
    shared_collision: SharedCollision,
    spells: Arc<SpellDb>,
    net_thread_dead: NetThreadDeadShared,
    socket_path: PathBuf,
    listener: std::os::unix::net::UnixListener,
) {
    std::thread::Builder::new()
        .name("agent-plugin-host".into())
        .spawn(move || {
            listener.set_nonblocking(true).expect("set agent socket nonblocking");
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("agent-plugin-host tokio runtime");
            rt.block_on(async move {
                let listener = match tokio::net::UnixListener::from_std(listener) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!(
                            "agent-plugin-host: failed to adopt bound socket {}: {e}",
                            socket_path.display()
                        );
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

        let listener = bind_agent_socket(&socket_path).expect("bind test socket");
        spawn_agent_plugin_host(
            CameraSlots::for_test(),
            CommandState::default(),
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default())),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(SpellDb::default()),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            socket_path.clone(),
            listener,
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

        let listener = bind_agent_socket(&socket_path).expect("bind test socket");
        spawn_agent_plugin_host(
            CameraSlots::for_test(),
            CommandState::default(),
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default())),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(SpellDb::default()),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            socket_path.clone(),
            listener,
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
