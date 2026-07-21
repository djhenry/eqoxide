//! `/v1/lifecycle/*` — session control: camp out (and optionally shut the client down).

use axum::{extract::State, http::StatusCode, routing::post, Router};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/camp", post(post_camp))
        .route("/exit", post(post_exit))
        .route("/respawn", post(post_respawn))
}

/// POST /v1/lifecycle/respawn — revive a slain character at its bind point. The client holds a dead
/// character in the slain state (it no longer auto-respawns) so an agent can inspect `dead` /
/// `killed_by` in /v1/observe/debug and recover its corpse before continuing; this releases it. A
/// no-op (but still 200) if the character isn't currently dead. (#284)
async fn post_respawn(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let dead = s.player().dead;
    s.command.request_respawn();
    if dead {
        tracing::info!("respawn: requested via POST /v1/lifecycle/respawn");
        (StatusCode::OK, "respawning at bind point".into())
    } else {
        (StatusCode::OK, "not currently dead (respawn will apply on the next death)".into())
    }
}

/// POST /v1/lifecycle/camp — toggle a camp. Starts a camp if none is running, or cancels the one in
/// progress (same as the HUD Camp button and the `/camp` chat keyword). A completed camp shuts the
/// client down cleanly with no linkdead; a cancel keeps the client in-world.
///
/// #477: guarded like the other WRITE commands. Camp is DRAINED by the gameplay net thread
/// (`camp_apply` → `camp_expired` → the shutdown flag in `gameplay.rs`); if that thread has exited
/// (the #470/#477 zombie) a camp would return 200 "then shutting down" that never happens — the exact
/// false-success class this fixes. Unlike `/v1/lifecycle/exit`, camp has NO watchdog to force the
/// shutdown, so a dead session must be reported honestly; use `/v1/lifecycle/exit` to tear a zombie
/// session down.
async fn post_camp(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let camping = s.lifecycle.camp_until.lock().unwrap().is_some();
    s.command.request_camp(CampCmd::Toggle);
    if camping {
        tracing::info!("camp: cancel requested via POST /v1/lifecycle/camp");
        (StatusCode::OK, "cancelling camp".into())
    } else {
        tracing::info!("camp: start requested via POST /v1/lifecycle/camp");
        (StatusCode::OK, "camping out (~30s), then shutting down".into())
    }
}

/// POST /v1/lifecycle/exit — camp out, then cleanly shut down. Requests a camp (`CampCmd::Start`,
/// idempotent): the gameplay loop sends OP_Camp, stays connected ~30s for EQEmu's camp timer to set
/// `instalog`, then sets the shutdown flag so the disconnect leaves NO linkdead ghost (instant
/// re-login). The render loop's `about_to_wait` then exits the winit event loop on the MAIN thread
/// and the process exits via `main`.
///
/// The watchdog is a last resort if the gameplay/render loop is wedged. It must outlast the camp
/// (CAMP_DURATION ≈ 30s) so it never force-kills mid-camp (which WOULD linkdead); 45s gives margin.
async fn post_exit(State(s): State<HttpState>) -> (StatusCode, &'static str) {
    tracing::info!("exit: camp-and-shutdown requested via POST /v1/lifecycle/exit");
    s.command.request_camp(CampCmd::Start);
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(45)).await;
        tracing::warn!("exit: watchdog timeout — loop unresponsive, forcing process exit");
        // Label it (#380): this exit fires EXACTLY when the render/gameplay loop is already wedged.
        // An unlabelled exit(0) here would leave a post-mortem with "no clean-shutdown record, no
        // panic, no signal, fresh heartbeat" — which the crash module documents as meaning
        // OOM-kill. A wedge would then be confidently misreported as an OOM: an agent-honesty
        // violation inside the agent-honesty fix. The reason string is the whole point.
        eqoxide_crash::exit("render-loop-wedged", 0);
    });
    (StatusCode::OK, "camping out, then shutting down (~30s)")
}
