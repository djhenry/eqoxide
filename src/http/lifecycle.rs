//! `/v1/lifecycle/*` — session control: camp out (and optionally shut the client down).

use axum::{extract::State, http::StatusCode, routing::post, Router};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/camp", post(post_camp))
        .route("/exit", post(post_exit))
}

/// POST /v1/lifecycle/camp — toggle a camp. Starts a camp if none is running, or cancels the one in
/// progress (same as the HUD Camp button and the `/camp` chat keyword). A completed camp shuts the
/// client down cleanly with no linkdead; a cancel keeps the client in-world.
async fn post_camp(State(s): State<HttpState>) -> (StatusCode, &'static str) {
    let camping = s.camp_until.lock().unwrap().is_some();
    *s.camp.lock().unwrap() = Some(CampCmd::Toggle);
    if camping {
        tracing::info!("camp: cancel requested via POST /v1/lifecycle/camp");
        (StatusCode::OK, "cancelling camp")
    } else {
        tracing::info!("camp: start requested via POST /v1/lifecycle/camp");
        (StatusCode::OK, "camping out (~30s), then shutting down")
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
    *s.camp.lock().unwrap() = Some(CampCmd::Start);
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(45)).await;
        tracing::warn!("exit: watchdog timeout — loop unresponsive, forcing process exit");
        std::process::exit(0);
    });
    (StatusCode::OK, "camping out, then shutting down (~30s)")
}
