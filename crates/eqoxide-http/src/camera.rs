//! `/v1/camera/*` — get/set the orbit camera and reset it to the default follow view.

use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/", get(get_camera).post(post_camera))
        .route("/reset", post(post_camera_reset))
}

async fn get_camera(State(s): State<HttpState>) -> Json<CameraSnapshot> {
    Json(s.camera.snapshot.lock().unwrap().clone())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CameraSetBody {
    azimuth:   Option<f32>,
    elevation: Option<f32>,
    radius:    Option<f32>,
}

/// POST /v1/camera {"azimuth":..,"elevation":..,"radius":..} — set orbit camera. The focus always tracks the player (#1002), so it is not settable;
/// `deny_unknown_fields` makes a `focus` key a 400 rather than a 200 the render loop would discard.
async fn post_camera(
    State(s): State<HttpState>,
    body: Result<Json<CameraSetBody>, axum::extract::rejection::JsonRejection>,
) -> StatusCode {
    match body {
        Ok(Json(b)) => {
            *s.camera.cmd_tx.lock().unwrap() = Some(CameraCmd::Set {
                azimuth:   b.azimuth,
                elevation: b.elevation,
                radius:    b.radius,
            });
            StatusCode::OK
        }
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

/// POST /v1/camera/reset — reset the camera to the default follow view.
async fn post_camera_reset(State(s): State<HttpState>) -> StatusCode {
    *s.camera.cmd_tx.lock().unwrap() = Some(CameraCmd::Reset);
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use crate::testkit::empty_state;

    fn post(body: &str) -> Request<Body> {
        Request::post("/").header("content-type", "application/json").body(Body::from(body.to_string())).unwrap()
    }

    /// #1002: the focus tracks the player every tick, so a caller-supplied one can never persist.
    /// It must be refused (400, nothing queued) rather than answered 200 and then discarded.
    #[tokio::test]
    async fn focus_is_refused_not_silently_discarded_1002() {
        let state = empty_state();
        let cmd = state.camera.cmd_tx.clone();
        let resp = router().with_state(state).oneshot(post(r#"{"focus":[1.0,2.0,3.0]}"#)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(cmd.lock().unwrap().is_none(), "a refused camera set must not queue a command");
    }

    #[tokio::test]
    async fn angles_and_radius_are_still_queued() {
        let state = empty_state();
        let cmd = state.camera.cmd_tx.clone();
        let resp = router().with_state(state).oneshot(post(r#"{"azimuth":1.0,"radius":9.0}"#)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(cmd.lock().unwrap().is_some());
    }
}
