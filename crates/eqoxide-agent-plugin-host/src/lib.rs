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

    fn empty_camera_slots() -> CameraSlots {
        CameraSlots {
            cmd_tx: std::sync::Arc::new(std::sync::Mutex::new(None)),
            snapshot: std::sync::Arc::new(std::sync::Mutex::new(eqoxide_ipc::CameraSnapshot {
                mode: eqoxide_ipc::CameraMode::AutoFollow,
                azimuth: 0.0,
                elevation: 0.0,
                radius: 0.0,
                focus: [0.0, 0.0, 0.0],
                eye: [0.0, 0.0, 0.0],
                occluded: false,
                still_blocked: false,
                drawn_frame: None,
                drawn_at: None,
            })),
            frame_req: std::sync::Arc::new(std::sync::Mutex::new(None)),
            manual_move: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[tokio::test]
    async fn spawn_agent_plugin_host_binds_a_socket_file_that_accepts_a_connection() {
        let dir = std::env::temp_dir().join(format!("eqoxide-agent-host-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("test.sock");

        spawn_agent_plugin_host(
            empty_camera_slots(),
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
}
