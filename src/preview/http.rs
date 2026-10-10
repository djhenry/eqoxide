use super::{
    camera::Camera,
    state::{Failure, Model, RawFrame, DEADLINE},
};
use axum::{
    body::Body,
    extract::{rejection::JsonRejection, State},
    http::{header, Response, StatusCode},
    response::{IntoResponse, Response as HttpResponse},
    routing::{get, post},
    Json, Router,
};
use image::ImageEncoder;
use serde_json::{json, Value};
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Instant,
};
#[derive(Clone)]
pub(super) struct Shared {
    pub model: Arc<Mutex<Model>>,
    pub inspection: Value,
    pub wake: Arc<dyn Fn() -> Result<(), ()> + Send + Sync>,
}
impl IntoResponse for Failure {
    fn into_response(self) -> HttpResponse {
        (
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({"error":{"code":self.code,"message":self.message}})),
        )
            .into_response()
    }
}
pub(super) fn router(state: Shared) -> Router {
    Router::new()
        .route("/v1/observe/debug", get(debug))
        .route("/v1/camera", get(camera).post(set_camera))
        .route("/v1/observe/frame", get(frame))
        .route("/v1/lifecycle/exit", post(exit))
        .fallback(unsupported)
        .method_not_allowed_fallback(unsupported)
        .with_state(state)
}
async fn unsupported() -> Failure {
    Failure::new(
        404,
        "unsupported_in_preview",
        "operation unavailable in static visual preview",
    )
}
fn unavailable() -> Failure {
    Failure::new(
        503,
        "preview_state_unavailable",
        "preview state is unavailable",
    )
}
fn camera_snapshot(m: &Model) -> Value {
    json!({"active":m.active,"requested":m.requested,"submitted":m.submitted,"outcomes":m.outcomes,"queue_capacity":8,"queue_deadline_seconds":10,"outcome_history_capacity":32})
}
async fn camera(State(s): State<Shared>) -> Result<Json<Value>, Failure> {
    Ok(Json(camera_snapshot(
        &*s.model.lock().map_err(|_| unavailable())?,
    )))
}
async fn debug(State(s): State<Shared>) -> Result<Json<Value>, Failure> {
    let m = s.model.lock().map_err(|_| unavailable())?;
    let mut j = s.inspection.clone();
    j["mode"] = json!("static_visual_preview");
    j["phase"] = json!(m.phase);
    j["terminal_error"] = json!(m.terminal_error);
    j["connected"] = json!(false);
    j["world_responsive"] = json!(false);
    j["world_ready"] = json!(false);
    j["collision"] = json!({"available":false});
    j["navigation"] = json!({"available":false});
    j["camera"] = camera_snapshot(&m);
    j["capture"] = json!({"in_flight":m.capture_id.is_some(),"last_completed":m.captured,"viewport":m.viewport,"max_dimension":2048,"deadline_seconds":10});
    Ok(Json(j))
}
async fn set_camera(
    State(s): State<Shared>,
    body: Result<Json<Camera>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), Failure> {
    let Json(camera) = body.map_err(|e| Failure::new(400, "invalid_camera_json", e.body_text()))?;
    let revision = s
        .model
        .lock()
        .map_err(|_| unavailable())?
        .queue_camera(camera, Instant::now())?;
    if (s.wake)().is_err() {
        s.model
            .lock()
            .map_err(|_| unavailable())?
            .fail("preview event loop is unavailable".into());
        return Err(unavailable());
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"status":"queued","camera_revision":revision})),
    ))
}
async fn exit(State(s): State<Shared>) -> Result<Json<Value>, Failure> {
    s.model.lock().map_err(|_| unavailable())?.stop();
    (s.wake)().map_err(|_| unavailable())?;
    Ok(Json(
        json!({"status":"shutdown_requested","mode":"static_visual_preview"}),
    ))
}
struct Lease {
    state: Shared,
    id: u64,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut m) = self.state.model.lock() {
            m.release_capture(self.id);
        }
    }
}
async fn frame(State(s): State<Shared>) -> Result<HttpResponse, Failure> {
    let now = Instant::now();
    let deadline = now + DEADLINE;
    let (id, rx) = s
        .model
        .lock()
        .map_err(|_| unavailable())?
        .reserve_capture(now)?;
    let lease = Lease {
        state: s.clone(),
        id,
    };
    (s.wake)().map_err(|_| unavailable())?;
    let raw = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), rx)
        .await
        .map_err(|_| Failure::new(504, "capture_timeout", "capture deadline elapsed"))?
        .map_err(|_| Failure::new(503, "capture_failed", "capture producer stopped"))??;
    let proof = raw.proof.clone();
    let viewport = [raw.width, raw.height];
    // The lease stays in the worker through encoding, even if the HTTP caller cancels
    // or times out. New requests cannot multiply unfinished encoder workers.
    let job = tokio::task::spawn_blocking(move || {
        let mut png = Vec::new();
        encode_png(raw, &mut png)?;
        Ok::<_, Failure>((png, lease))
    });
    let (png, _lease) = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), job)
        .await
        .map_err(|_| Failure::new(504, "capture_timeout", "PNG encoding deadline elapsed"))?
        .map_err(|e| Failure::new(503, "png_encoding_failed", e.to_string()))??;
    if Instant::now() >= deadline {
        return Err(Failure::new(
            504,
            "capture_timeout",
            "capture deadline elapsed",
        ));
    }
    {
        s.model
            .lock()
            .map_err(|_| unavailable())?
            .complete_capture(id, viewport, proof.clone())?;
    }
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "no-store")
        .header("X-Preview-Frame-ID", proof.frame_id.to_string())
        .header(
            "X-Preview-Camera-Revision",
            proof.camera_revision.to_string(),
        )
        .body(Body::from(png))
        .map_err(|e| Failure::new(500, "capture_response_failed", e.to_string()))?)
}
fn encode_png(mut raw: RawFrame, writer: impl Write) -> Result<(), Failure> {
    if Instant::now() >= raw.deadline {
        return Err(Failure::new(
            504,
            "capture_timeout",
            "capture deadline elapsed before PNG encoding",
        ));
    }
    let bytes = (raw.width as usize)
        .checked_mul(raw.height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| Failure::new(413, "capture_size_limit", "capture byte size overflow"))?;
    if raw.width == 0
        || raw.height == 0
        || raw.width > 2048
        || raw.height > 2048
        || raw.bytes.len() != bytes
    {
        return Err(Failure::new(
            503,
            "invalid_readback",
            "capture readback dimensions/bytes mismatch",
        ));
    }
    if raw.bgra {
        for p in raw.bytes.chunks_exact_mut(4) {
            p.swap(0, 2);
        }
    }
    image::codecs::png::PngEncoder::new(writer)
        .write_image(
            &raw.bytes,
            raw.width,
            raw.height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| Failure::new(503, "png_encoding_failed", e.to_string()))?;
    if Instant::now() >= raw.deadline {
        return Err(Failure::new(
            504,
            "capture_timeout",
            "capture deadline elapsed during PNG encoding",
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::camera::Camera;
    use axum::http::Request;
    use std::time::Duration;
    use tower::ServiceExt;
    fn shared() -> Shared {
        Shared {
            model: Arc::new(Mutex::new(Model::new(Camera {
                eye: [0., -4., 2.],
                target: [0.; 3],
                fov_degrees: 60.,
            }))),
            inspection: json!({"source":"fixture.glb"}),
            wake: Arc::new(|| Ok(())),
        }
    }
    async fn value(response: axum::response::Response) -> Value {
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap(),
        )
        .unwrap()
    }
    #[tokio::test]
    async fn debug_is_disconnected_from_first_request_and_game_writes_are_refused() {
        let state = shared();
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/observe/debug")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let j = value(r).await;
        assert_eq!(j["connected"], false);
        assert_eq!(j["world_ready"], false);
        assert_eq!(j["collision"]["available"], false);
        let r = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/move/goto")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(value(r).await["error"]["code"], "unsupported_in_preview");
        assert_eq!(state.model.lock().unwrap().active.camera_revision, 0);
    }
    #[tokio::test]
    async fn camera_json_is_strict_and_valid_requests_only_acknowledge_queueing() {
        let state = shared();
        let app = router(state.clone());
        for body in [
            r#"{"eye":[1,2,3],"target":[0,0,0],"fov_degrees":60,"unexpected":true}"#,
            r#"{"eye":[0,0,0],"target":[0,0,0],"fov_degrees":60}"#,
            r#"{"eye":[3e38,3e38,3e38],"target":[-3e38,-3e38,-3e38],"fov_degrees":60}"#,
        ] {
            let r = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/camera")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        }
        let r = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/camera")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"eye":[1,-4,2],"target":[0,0,0],"fov_degrees":60}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        assert_eq!(value(r).await["status"], "queued");
        assert!(state.model.lock().unwrap().submitted.is_none());
    }
    #[tokio::test]
    async fn exit_sets_shutdown_without_gameplay_work() {
        let state = shared();
        let r = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/lifecycle/exit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(state.model.lock().unwrap().shutdown);
    }
    fn raw(deadline: Instant, bgra: bool) -> RawFrame {
        RawFrame {
            bytes: vec![30, 20, 10, 255],
            width: 1,
            height: 1,
            bgra,
            proof: super::super::state::Submission {
                frame_id: 7,
                camera_revision: 3,
                camera: super::super::camera::Camera {
                    eye: [1., -4., 2.],
                    target: [0.; 3],
                    fov_degrees: 60.,
                },
            },
            deadline,
        }
    }
    #[test]
    fn actual_png_encoding_checks_deadline_bytes_and_bgra_color() {
        let mut png = Vec::new();
        encode_png(raw(Instant::now() + DEADLINE, true), &mut png).unwrap();
        assert_eq!(
            image::load_from_memory(&png)
                .unwrap()
                .into_rgba8()
                .get_pixel(0, 0)
                .0,
            [10, 20, 30, 255]
        );
        assert_eq!(
            encode_png(
                raw(Instant::now() - Duration::from_secs(1), false),
                Vec::new()
            )
            .unwrap_err()
            .status,
            504
        );
        let mut broken = raw(Instant::now() + DEADLINE, false);
        broken.bytes.pop();
        assert!(encode_png(broken, Vec::new()).is_err());
    }
    #[test]
    fn png_writer_failure_is_an_error_instead_of_successful_empty_image() {
        struct Fails;
        impl std::io::Write for Fails {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("output unavailable"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            encode_png(raw(Instant::now() + DEADLINE, false), Fails)
                .unwrap_err()
                .code,
            "png_encoding_failed"
        );
    }
    #[tokio::test]
    async fn concurrent_capture_refuses_busy_and_propagates_map_error() {
        let state = shared();
        {
            let mut m = state.model.lock().unwrap();
            m.phase = "ready";
            m.viewport = [1, 1];
        }
        let app = router(state.clone());
        let request = tokio::spawn(
            app.clone().oneshot(
                Request::builder()
                    .uri("/v1/observe/frame")
                    .body(Body::empty())
                    .unwrap(),
            ),
        );
        tokio::task::yield_now().await;
        let busy = app
            .oneshot(
                Request::builder()
                    .uri("/v1/observe/frame")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(busy.status(), StatusCode::CONFLICT);
        let pending = state.model.lock().unwrap().capture_pending.take().unwrap();
        let _ = pending
            .tx
            .send(Err(Failure::new(503, "map_failed", "map rejected")));
        let response = request.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(value(response).await["error"]["code"], "map_failed");
        assert!(state.model.lock().unwrap().capture_id.is_none());
    }
    #[tokio::test]
    async fn successful_capture_headers_name_the_completed_readback() {
        let state = shared();
        {
            let mut m = state.model.lock().unwrap();
            m.phase = "ready";
            m.viewport = [1, 1];
        }
        let request = tokio::spawn(
            router(state.clone()).oneshot(
                Request::builder()
                    .uri("/v1/observe/frame")
                    .body(Body::empty())
                    .unwrap(),
            ),
        );
        tokio::task::yield_now().await;
        let pending = state.model.lock().unwrap().capture_pending.take().unwrap();
        let _ = pending.tx.send(Ok(raw(Instant::now() + DEADLINE, true)));
        let response = request.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["X-Preview-Frame-ID"], "7");
        assert_eq!(response.headers()["X-Preview-Camera-Revision"], "3");
        assert_eq!(response.headers()["Cache-Control"], "no-store");
        let png = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        assert_eq!(
            image::load_from_memory(&png)
                .unwrap()
                .into_rgba8()
                .get_pixel(0, 0)
                .0,
            [10, 20, 30, 255]
        );
    }
    #[tokio::test(start_paused = true)]
    async fn capture_deadline_returns_timeout_without_image_success() {
        let state = shared();
        {
            let mut m = state.model.lock().unwrap();
            m.phase = "ready";
            m.viewport = [1, 1];
        }
        let request = tokio::spawn(
            router(state.clone()).oneshot(
                Request::builder()
                    .uri("/v1/observe/frame")
                    .body(Body::empty())
                    .unwrap(),
            ),
        );
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(11)).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), request)
                .await
                .expect("capture must terminate at its deadline")
                .unwrap()
                .unwrap()
                .status(),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert!(state.model.lock().unwrap().capture_id.is_none());
    }

    #[tokio::test]
    async fn raw_readback_followed_by_unavailable_window_cannot_return_image_success() {
        let state = shared();
        {
            state.model.lock().unwrap().window_changed([1, 1], false);
        }
        let request = tokio::spawn(
            router(state.clone()).oneshot(
                Request::builder()
                    .uri("/v1/observe/frame")
                    .body(Body::empty())
                    .unwrap(),
            ),
        );
        tokio::task::yield_now().await;
        let pending = state.model.lock().unwrap().capture_pending.take().unwrap();
        let _ = pending.tx.send(Ok(raw(Instant::now() + DEADLINE, true)));
        state.model.lock().unwrap().window_changed([0, 0], false);
        let response = request.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            value(response).await["error"]["code"],
            "capture_unavailable"
        );
        assert!(state.model.lock().unwrap().captured.is_none());
    }
}
