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
/// connection is served at a time — see `src/lib.rs`'s accept loop, added in Task 17.
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
                let _ = (&camera, &command, &game_state, &shared_collision, &spells, &net_thread_dead, &listener);
            });
        })
        .expect("spawn agent-plugin-host thread");
}
