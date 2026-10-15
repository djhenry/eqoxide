use super::{
    camera::Camera,
    state::{
        Failure, Model, RawFrame, CAMERA_QUEUE_CAPACITY, CAPTURE_MAX_DIMENSION, DEADLINE,
        OUTCOME_HISTORY_CAPACITY,
    },
};
use axum::{
    body::Body,
    extract::{rejection::JsonRejection, State},
    http::{header, Response, StatusCode},
    middleware::{self, Next},
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
    pub inspection: Arc<Value>,
    pub port: u16,
    pub wake: Arc<dyn Fn() -> Result<(), ()> + Send + Sync>,
}
impl Shared {
    fn wake_or_fail(&self) -> Result<(), Failure> {
        if (self.wake)().is_err() {
            self.model
                .lock()
                .map_err(|_| unavailable())?
                .fail_if_active("preview event loop is unavailable".into());
            return Err(unavailable());
        }
        Ok(())
    }
    pub(super) fn report_runtime_failure(&self, message: String) {
        if let Ok(mut m) = self.model.lock() {
            m.fail_if_active(message);
        }
        let _ = (self.wake)();
    }
}
impl IntoResponse for Failure {
    fn into_response(self) -> HttpResponse {
        (
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({"error":self.code,"message":self.message})),
        )
            .into_response()
    }
}
async fn authorize(
    State(state): State<Shared>,
    request: axum::extract::Request,
    next: Next,
) -> HttpResponse {
    let headers = request.headers();
    let mut authorities = headers.get_all(header::HOST).iter();
    let host = authorities.next().and_then(|value| value.to_str().ok());
    let numeric = format!("127.0.0.1:{}", state.port);
    let named = format!("localhost:{}", state.port);
    if authorities.next().is_some() || !host.is_some_and(|value| value == numeric || value == named)
    {
        return Failure::new(
            403,
            "invalid_preview_authority",
            "Host must identify this preview's exact loopback port",
        )
        .into_response();
    }
    if headers.contains_key(header::ORIGIN) {
        return Failure::new(
            403,
            "browser_origin_refused",
            "browser-origin requests are unavailable in preview control",
        )
        .into_response();
    }
    if request.method() == axum::http::Method::POST {
        let mut types = headers.get_all(header::CONTENT_TYPE).iter();
        let content_type = types.next().and_then(|value| value.to_str().ok());
        if types.next().is_some() || !content_type.is_some_and(json_content_type) {
            return Failure::new(
                415,
                "json_content_type_required",
                "preview POST requests require a single application/json Content-Type",
            )
            .into_response();
        }
    }
    next.run(request).await
}
fn json_content_type(value: &str) -> bool {
    let mut parts = value.split(';');
    if !parts
        .next()
        .is_some_and(|part| part.trim().eq_ignore_ascii_case("application/json"))
    {
        return false;
    }
    let charset = parts.next();
    if parts.next().is_some() {
        return false;
    }
    charset.is_none_or(|part| {
        part.split_once('=').is_some_and(|(key, value)| {
            key.trim().eq_ignore_ascii_case("charset")
                && (value.trim().eq_ignore_ascii_case("utf-8")
                    || value.trim().eq_ignore_ascii_case("\"utf-8\""))
        })
    })
}

pub(super) fn router(state: Shared) -> Router {
    Router::new()
        .route("/v1/observe/debug", get(debug))
        .route("/v1/camera", get(camera).post(set_camera))
        .route("/v1/observe/frame", get(frame))
        .route("/v1/lifecycle/exit", post(exit))
        .fallback(unsupported)
        .method_not_allowed_fallback(unsupported)
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
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
    json!({"active":m.active,"requested":m.requested,"submitted":m.submitted,"outcomes":m.outcomes,"queue_capacity":CAMERA_QUEUE_CAPACITY,"queue_deadline_seconds":DEADLINE.as_secs(),"outcome_history_capacity":OUTCOME_HISTORY_CAPACITY})
}
async fn camera(State(s): State<Shared>) -> Result<Json<Value>, Failure> {
    Ok(Json(camera_snapshot(
        &*s.model.lock().map_err(|_| unavailable())?,
    )))
}
async fn debug(State(s): State<Shared>) -> Result<Json<Value>, Failure> {
    let m = s.model.lock().map_err(|_| unavailable())?;
    let mut j = (*s.inspection).clone();
    j["mode"] = json!("static_visual_preview");
    j["phase"] = json!(m.phase);
    j["occluded"] = json!(m.occluded);
    j["terminal_error"] = json!(m.terminal_error);
    j["connected"] = json!(false);
    j["world_responsive"] = json!(false);
    j["world_ready"] = json!(false);
    j["collision"] = json!({"available":false});
    j["camera"] = camera_snapshot(&m);
    j["capture"] = json!({"in_flight":m.capture_id.is_some(),"last_completed":m.captured,"viewport":m.viewport,"max_dimension":CAPTURE_MAX_DIMENSION,"deadline_seconds":DEADLINE.as_secs()});
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
    s.wake_or_fail()?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"status":"queued","camera_revision":revision})),
    ))
}
async fn exit(State(s): State<Shared>) -> Result<Json<Value>, Failure> {
    s.model.lock().map_err(|_| unavailable())?.stop();
    // Shutdown is already committed; polling also observes it if the wake fails.
    let _ = (s.wake)();
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
    s.wake_or_fail()?;
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
        || raw.width > CAPTURE_MAX_DIMENSION
        || raw.height > CAPTURE_MAX_DIMENSION
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
    use crate::preview::{camera::Camera, state::Phase};
    use axum::http::Request;
    use std::time::Duration;
    use tower::ServiceExt;
    fn request_builder() -> axum::http::request::Builder {
        Request::builder()
            .header(header::HOST, "127.0.0.1:8771")
            .header(header::CONTENT_TYPE, "application/json")
    }
    fn shared() -> Shared {
        Shared {
            model: Arc::new(Mutex::new(Model::new(Camera {
                eye: [0., -4., 2.],
                target: [0.; 3],
                fov_degrees: 60.,
            }))),
            inspection: Arc::new(json!({"source":"fixture.glb"})),
            port: 8771,
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
    async fn router_rejects_wrong_missing_and_duplicate_authority_before_any_handler() {
        for host in [
            None,
            Some("example.test:8771"),
            Some("127.0.0.1:8772"),
            Some("localhost"),
            Some("localhost:08771"),
            Some("127.0.0.1:8771,localhost:8771"),
        ] {
            for route in [
                "/v1/observe/debug",
                "/v1/observe/frame",
                "/v1/camera",
                "/unknown",
            ] {
                let state = shared();
                let mut builder = Request::builder().uri(route);
                if let Some(host) = host {
                    builder = builder.header(header::HOST, host);
                }
                let response = router(state.clone())
                    .oneshot(builder.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::FORBIDDEN, "{host:?} {route}");
                assert!(state.model.lock().unwrap().capture_id.is_none());
            }
        }
        let state = shared();
        let response = router(state.clone())
            .oneshot(
                request_builder()
                    .header(header::HOST, "localhost:8771")
                    .method("POST")
                    .uri("/v1/lifecycle/exit")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!state.model.lock().unwrap().shutdown);
    }

    #[tokio::test]
    async fn router_refuses_all_origins_and_non_json_or_duplicate_post_content_types() {
        for origin in ["https://example.test", "http://127.0.0.1:8771", "null", ""] {
            for (method, route) in [
                ("GET", "/v1/observe/debug"),
                ("POST", "/v1/lifecycle/exit"),
                ("OPTIONS", "/v1/camera"),
            ] {
                let state = shared();
                let response = router(state.clone())
                    .oneshot(
                        request_builder()
                            .method(method)
                            .uri(route)
                            .header(header::ORIGIN, origin)
                            .body(Body::from("{}"))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::FORBIDDEN);
                assert!(!state.model.lock().unwrap().shutdown);
            }
        }
        for content_type in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
            Some("application/json;garbage"),
            Some("application/json; charset=ascii"),
        ] {
            let state = shared();
            let mut builder = Request::builder()
                .header(header::HOST, "127.0.0.1:8771")
                .method("POST")
                .uri("/v1/lifecycle/exit");
            if let Some(value) = content_type {
                builder = builder.header(header::CONTENT_TYPE, value);
            }
            let response = router(state.clone())
                .oneshot(builder.body(Body::from("{}")).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{content_type:?}"
            );
            assert!(!state.model.lock().unwrap().shutdown);
        }
        let state = shared();
        let response = router(state.clone())
            .oneshot(
                request_builder()
                    .header(header::CONTENT_TYPE, "application/json")
                    .method("POST")
                    .uri("/v1/lifecycle/exit")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(!state.model.lock().unwrap().shutdown);
    }

    #[tokio::test]
    async fn invalid_header_bytes_fail_and_json_charset_is_explicitly_supported() {
        for header_name in [header::HOST, header::CONTENT_TYPE] {
            let state = shared();
            let mut request = request_builder()
                .method("POST")
                .uri("/v1/lifecycle/exit")
                .body(Body::from("{}"))
                .unwrap();
            request.headers_mut().insert(
                header_name.clone(),
                axum::http::HeaderValue::from_bytes(&[0xff]).unwrap(),
            );
            let response = router(state.clone()).oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                if header_name == header::HOST {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::UNSUPPORTED_MEDIA_TYPE
                }
            );
            assert!(!state.model.lock().unwrap().shutdown);
        }
        for content_type in [
            "application/json",
            "application/json; charset=utf-8",
            "APPLICATION/JSON; charset=\"UTF-8\"",
        ] {
            let state = shared();
            let response = router(state.clone())
                .oneshot(
                    Request::builder()
                        .header(header::HOST, "localhost:8771")
                        .header(header::CONTENT_TYPE, content_type)
                        .method("POST")
                        .uri("/v1/lifecycle/exit")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(state.model.lock().unwrap().shutdown);
        }
    }

    #[tokio::test]
    async fn router_accepts_only_actual_port_native_authorities_and_reports_shared_bounds() {
        for hostname in ["127.0.0.1", "localhost"] {
            let mut state = shared();
            state.port = 43210;
            let response = router(state.clone())
                .oneshot(
                    Request::builder()
                        .header(header::HOST, format!("{hostname}:43210"))
                        .uri("/v1/observe/debug")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let j = value(response).await;
            assert_eq!(j["phase"], "initializing");
            assert_eq!(j["camera"]["queue_capacity"], CAMERA_QUEUE_CAPACITY);
            assert_eq!(j["camera"]["queue_deadline_seconds"], DEADLINE.as_secs());
            assert_eq!(
                j["camera"]["outcome_history_capacity"],
                OUTCOME_HISTORY_CAPACITY
            );
            assert_eq!(j["capture"]["max_dimension"], CAPTURE_MAX_DIMENSION);
            assert_eq!(j["capture"]["deadline_seconds"], DEADLINE.as_secs());
        }
    }

    #[tokio::test]
    async fn debug_is_disconnected_from_first_request_and_game_writes_are_refused() {
        let state = shared();
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(
                request_builder()
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
                request_builder()
                    .method("POST")
                    .uri("/v1/move/manual")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(value(r).await["error"], "unsupported_in_preview");
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
                    request_builder()
                        .method("POST")
                        .uri("/v1/camera")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        }
        let r = app
            .oneshot(
                request_builder()
                    .method("POST")
                    .uri("/v1/camera")
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
                request_builder()
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
            m.phase = Phase::Ready;
            m.viewport = [1, 1];
        }
        let app = router(state.clone());
        let request = tokio::spawn(
            app.clone().oneshot(
                request_builder()
                    .uri("/v1/observe/frame")
                    .body(Body::empty())
                    .unwrap(),
            ),
        );
        tokio::task::yield_now().await;
        let busy = app
            .oneshot(
                request_builder()
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
        assert_eq!(value(response).await["error"], "map_failed");
        assert!(state.model.lock().unwrap().capture_id.is_none());
    }
    #[tokio::test]
    async fn successful_capture_headers_name_the_completed_readback() {
        let state = shared();
        {
            let mut m = state.model.lock().unwrap();
            m.phase = Phase::Ready;
            m.viewport = [1, 1];
        }
        let request = tokio::spawn(
            router(state.clone()).oneshot(
                request_builder()
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
            m.phase = Phase::Ready;
            m.viewport = [1, 1];
        }
        let request = tokio::spawn(
            router(state.clone()).oneshot(
                request_builder()
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
            state
                .model
                .lock()
                .unwrap()
                .window_changed([1, 1], Some(false));
        }
        let request = tokio::spawn(
            router(state.clone()).oneshot(
                request_builder()
                    .uri("/v1/observe/frame")
                    .body(Body::empty())
                    .unwrap(),
            ),
        );
        tokio::task::yield_now().await;
        let pending = state.model.lock().unwrap().capture_pending.take().unwrap();
        let _ = pending.tx.send(Ok(raw(Instant::now() + DEADLINE, true)));
        state
            .model
            .lock()
            .unwrap()
            .window_changed([0, 0], Some(false));
        let response = request.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(value(response).await["error"], "capture_unavailable");
        assert!(state.model.lock().unwrap().captured.is_none());
    }
    #[tokio::test]
    async fn shutdown_acknowledges_committed_stop_when_wake_fails() {
        let mut s = shared();
        s.wake = Arc::new(|| Err(()));
        let response = exit(State(s.clone())).await.unwrap().into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let m = s.model.lock().unwrap();
        assert!(m.shutdown);
        assert_eq!(m.phase, Phase::Stopping);
        assert!(m.terminal_error.is_none());
    }
    #[tokio::test]
    async fn frame_wake_failure_stops_live_model_and_releases_capture() {
        let mut s = shared();
        s.model.lock().unwrap().window_changed([1, 1], Some(false));
        s.wake = Arc::new(|| Err(()));
        assert_eq!(
            frame(State(s.clone())).await.unwrap_err().code,
            "preview_state_unavailable"
        );
        let m = s.model.lock().unwrap();
        assert_eq!(m.phase, Phase::Failed);
        assert!(m.shutdown);
        assert!(m.capture_id.is_none());
    }
    #[tokio::test]
    async fn failure_json_matches_common_api_envelope() {
        let j = value(Failure::new(503, "test_failure", "explanation").into_response()).await;
        assert_eq!(j, json!({"error":"test_failure", "message":"explanation"}));
    }

    #[tokio::test]
    async fn debug_reports_unknown_occlusion_without_claiming_visibility() {
        let s = shared();
        s.model.lock().unwrap().window_changed([640, 480], None);
        let j = debug(State(s)).await.unwrap().0;
        assert_eq!(j["phase"], "ready");
        assert_eq!(j.get("occluded"), Some(&Value::Null));
    }
    #[tokio::test]
    async fn poisoned_model_refuses_all_stateful_operations() {
        let s = shared();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _m = s.model.lock().unwrap();
            panic!("injected model mutation panic");
        }));
        assert!(s.model.is_poisoned());
        assert_eq!(camera(State(s.clone())).await.unwrap_err().status, 503);
        assert_eq!(debug(State(s.clone())).await.unwrap_err().status, 503);
        assert_eq!(frame(State(s.clone())).await.unwrap_err().status, 503);
        assert_eq!(exit(State(s.clone())).await.unwrap_err().status, 503);
        let c = super::super::camera::Camera {
            eye: [1., -4., 2.],
            target: [0.; 3],
            fov_degrees: 60.,
        };
        assert_eq!(
            set_camera(State(s), Ok(Json(c))).await.unwrap_err().status,
            503
        );
    }
    #[test]
    fn runtime_failures_stop_active_preview_but_preserve_intentional_teardown() {
        let s = shared();
        s.report_runtime_failure("active GPU error".into());
        {
            let m = s.model.lock().unwrap();
            assert_eq!(m.phase, Phase::Failed);
            assert_eq!(m.terminal_error.as_deref(), Some("active GPU error"));
        }
        s.report_runtime_failure("late device loss".into());
        assert_eq!(
            s.model.lock().unwrap().terminal_error.as_deref(),
            Some("active GPU error")
        );
        let s = shared();
        s.model.lock().unwrap().stop();
        s.report_runtime_failure("teardown GPU error".into());
        let m = s.model.lock().unwrap();
        assert_eq!(m.phase, Phase::Stopping);
        assert!(m.terminal_error.is_none());
    }
    #[tokio::test]
    async fn camera_and_frame_wake_shutdown_races_preserve_clean_stop() {
        for capture in [false, true] {
            let mut s = shared();
            s.model.lock().unwrap().window_changed([1, 1], None);
            let model = s.model.clone();
            s.wake = Arc::new(move || {
                model.lock().unwrap().stop();
                Err(())
            });
            if capture {
                assert!(frame(State(s.clone())).await.is_err());
            } else {
                let c = super::super::camera::Camera {
                    eye: [1., -4., 2.],
                    target: [0.; 3],
                    fov_degrees: 60.,
                };
                assert!(set_camera(State(s.clone()), Ok(Json(c))).await.is_err());
            }
            let m = s.model.lock().unwrap();
            assert_eq!(m.phase, Phase::Stopping);
            assert!(m.shutdown);
            assert!(m.terminal_error.is_none());
            assert!(m.capture_id.is_none());
        }
    }
}
