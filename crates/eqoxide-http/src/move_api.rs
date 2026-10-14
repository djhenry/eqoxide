//! `/v1/move/*` — direct movement: drive the controller like WASD (`/manual`) or hop (`/jump`).
//!
//! Navigation (goto, follow, zone-cross) is not part of the client: it lives in the agent harness,
//! which drives these same verbs over the Agent Plugin API.
//!
//! `/manual` takes an *optional* JSON body via [`OptionalJson`]. It judges "was a body sent" from the
//! raw bytes, not the `Content-Type` header, so a body sent without the header still gets parsed (or
//! a clear 400) instead of being silently ignored. A body that IS present but fails to parse always
//! 400s naming the problem; it is never downgraded to "no body".

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use super::*;

/// A `text/plain` response (for require_live_session errors and malformed-body 4xx).
fn text(status: StatusCode, body: impl Into<String>) -> Response {
    Response::builder().status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body.into())).unwrap()
}

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/manual", post(post_manual))
        .route("/jump", post(post_jump))
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ManualBody {
    /// World movement direction, matching `/v1/observe/debug` `pos`: `east` = +server_x (= pos.x),
    /// `north` = +server_y (= pos.y) — the zone-wide EQ convention used everywhere in the client.
    /// (#267: the previous doc had these swapped.) Any magnitude; it's normalized. Zero/omitted =
    /// stand in place (e.g. a jump with no movement).
    east:  Option<f32>,
    north: Option<f32>,
    /// Vertical axis for SWIMMING, `-1..1` (`+1` = up toward the surface, `-1` = dive). Only has an
    /// effect while the character is in water; ignored on land (#207).
    up:    Option<f32>,
    jump:  Option<bool>,
    /// How long to drive the controller, in ms (default 400, clamped to 5000). The render loop
    /// applies the intent every frame until this elapses, then movement stops.
    duration_ms: Option<u64>,
}

/// POST /v1/move/manual — drive the CharacterController directly (like WASD), or swim up/down in
/// water with `up` (#207). Body: `{east, north, up, jump, duration_ms}`. Yields to real keyboard input.
async fn post_manual(
    State(s): State<HttpState>,
    OptionalJson(body): OptionalJson<ManualBody>,
) -> Response {
    if let Err((code, msg)) = require_live_session(&s) { return text(code, msg); }
    // #644: a corpse cannot be driven manually
    if let Err((code, msg)) = require_alive(&s) { return text(code, msg); }
    // #884: refuse while the controller is frozen, BEFORE the manual-move slot is written — an
    // accepted `ManualMove` the controller will never read is a queued action with no outcome.
    if let Some(r) = crate::MoveGate::read(&s).refusal() { return r; }
    let b = body.unwrap_or_default();
    let dir = [b.east.unwrap_or(0.0), b.north.unwrap_or(0.0)];
    let up = b.up.unwrap_or(0.0).clamp(-1.0, 1.0);
    let jump = b.jump.unwrap_or(false);
    let ms = b.duration_ms.unwrap_or(400).min(5000);
    if dir[0] == 0.0 && dir[1] == 0.0 && up == 0.0 && !jump {
        return text(StatusCode::BAD_REQUEST, "provide a direction {east,north}, {up:-1..1} (swim), and/or {\"jump\":true}");
    }
    s.camera.request_manual_move(ManualMove {
        dir, up, jump,
        wish_heading: None,
        until: std::time::Instant::now() + std::time::Duration::from_millis(ms),
    });
    text(StatusCode::OK, format!("manual move dir=({:.1},{:.1}) up={up:.1} jump={jump} for {ms}ms", dir[0], dir[1]))
}

/// POST /v1/move/jump — a single hop in place (a discrete convenience over `/manual` with only
/// `jump`). On land it's a jump; in water it swims upward toward the surface (#207), e.g. to lift
/// off a pool floor.
async fn post_jump(State(s): State<HttpState>) -> Response {
    if let Err((code, msg)) = require_live_session(&s) { return text(code, msg); }
    if let Err((code, msg)) = require_alive(&s) { return text(code, msg); } // #644: a corpse cannot jump
    if let Some(r) = crate::MoveGate::read(&s).refusal() { return r; } // #884
    s.camera.request_manual_move(ManualMove {
        dir: [0.0, 0.0], up: 0.0, jump: true,
        wish_heading: None,
        until: std::time::Instant::now() + std::time::Duration::from_millis(400),
    });
    text(StatusCode::OK, "jump")
}

#[cfg(test)]
mod tests {
    use super::router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use crate::testkit::{empty_state, set_gs};
    use eqoxide_core::game_state::{ControllerHold, ControllerHoldReason, HeldMotion};

    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        serde_json::from_str(&body_text(resp).await).expect("JSON body")
    }

    #[tokio::test]
    async fn manual_and_jump_while_dead_are_rejected() {
        for (path, body) in [("/manual", r#"{"east":1.0}"#), ("/jump", "")] {
            let state = empty_state();
            set_gs(&state, |gs| gs.player_dead = true);
            let app = router().with_state(state);
            let mut rb = Request::post(path);
            if !body.is_empty() { rb = rb.header("content-type", "application/json"); }
            let resp = app.oneshot(rb.body(Body::from(body)).unwrap()).await.unwrap();
            assert_eq!(resp.status(), StatusCode::CONFLICT, "{path} on a corpse must be rejected");
            assert!(body_text(resp).await.contains("dead"), "{path} must name the `dead` condition");
        }
    }

    // --- manual: a malformed body must be reported honestly, not as "no direction given" -------

    #[tokio::test]
    async fn manual_malformed_body_reports_malformed_not_missing_direction() {
        let state = empty_state();
        let app = router().with_state(state);
        let req = Request::post("/manual")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"east":"north"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let text = body_text(resp).await;
        assert!(text.contains("malformed JSON body"),
            "message should name the real cause, not the unrelated \"provide a direction\" default-validation text: {text}");
    }

    // --- #884: no movement endpoint may report progress while the physics step is frozen --------

    /// Put a hold of `reason` in force, as `ActionLoop::stream_position` mirrors it.
    fn held(state: &crate::HttpState, reason: ControllerHoldReason) {
        set_gs(state, |gs| gs.player_hold = Some(ControllerHold { reason, secs: 88.15 }));
    }

    /// Every `/v1/move/*` verb, with a REQUEST THAT WOULD OTHERWISE SUCCEED and a probe that reports
    /// whether the client queued anything for it.
    #[allow(clippy::type_complexity)]
    fn gated_movement_endpoints() -> Vec<(&'static str, &'static str, fn(&crate::HttpState) -> bool)> {
        vec![
            ("/manual", r#"{"east":1.0}"#,
                (|s| s.camera.manual_move.lock().unwrap().is_some()) as fn(&crate::HttpState) -> bool),
            ("/jump",   "",
                |s| s.camera.manual_move.lock().unwrap().is_some()),
        ]
    }

    /// **#884 REACH CONTROL for the matrix below.** `gated_movement_endpoints()` is a hand-written
    /// list, and a table is only a universal over what it lists: a new `/v1/move/*` verb could ship
    /// ungated and every assertion here would still pass. So derive the endpoint set from
    /// `router()`'s own source and require every registered route to be in the table.
    ///
    /// The marker is built with `concat!` so the string this scan searches for does not itself occur
    /// in this test. The `assert!`s on the marker count and the extracted count ARE the reach
    /// control: they fail loudly if the scan ever stops matching the code.
    #[test]
    fn every_move_route_is_classified_gated_884() {
        const SRC: &str = include_str!("move_api.rs");
        let marker = concat!("fn ", "router() -> Router<HttpState> {");

        assert_eq!(
            SRC.matches(marker).count(),
            1,
            "#884 reach control: the router signature this scan keys on occurs {} times, not once — \
             the scan is reading an unknown region and its verdict means nothing. Re-point it.",
            SRC.matches(marker).count()
        );
        let body = SRC
            .split_once(marker)
            .expect("marker counted once above")
            .1;
        let body = body
            .split_once("\n}\n")
            .expect("#884 reach control: router() has no closing brace at column 0")
            .0;

        let registered: Vec<&str> = body
            .match_indices(".route(\"")
            .map(|(i, m)| {
                let rest = &body[i + m.len()..];
                &rest[..rest.find('"').expect("route literal is terminated")]
            })
            .collect();

        assert_eq!(
            registered.len(),
            2,
            "#884 reach control: extracted {} routes from router(), expected 2 ({:?}). Either a \
             route was added/removed — in which case classify it below and update this count — or \
             the extraction broke and the classification assert underneath is vacuous.",
            registered.len(),
            registered
        );

        let gated: Vec<&str> = gated_movement_endpoints().iter().map(|(p, ..)| *p).collect();
        for path in &registered {
            assert!(
                gated.contains(path),
                "#884: `/v1/move{path}` is registered by router() but not in \
                 gated_movement_endpoints(). If it can drive the body, it must refuse while the \
                 controller's step is frozen — an unclassified movement verb defaults to answering \
                 200 for motion that cannot happen, which is the bug this issue is about."
            );
        }
        for path in &gated {
            assert!(
                registered.contains(path),
                "#884: gated_movement_endpoints() lists `{path}`, which router() does not register. \
                 The matrix below is asserting against a route that no longer exists."
            );
        }
    }

    /// **#884, the universal.** Exhaustive over `ControllerHoldReason::ALL` × every movement
    /// endpoint. Both directions are asserted from ONE rule — `reason.motion()` — so the test cannot
    /// be satisfied by a gate that refuses everything:
    ///
    /// * `AllFrozen` ⇒ `409`, `"status":"held"`, the hold named, **and nothing queued**.
    /// * `LateralStillReaches` ⇒ NOT refused, and the request really is queued. Refusing here would
    ///   remove an `underworld_no_recovery` body's only client-API exit (`/v1/move/manual` walking
    ///   it out) and would be its own false "you cannot move".
    #[tokio::test]
    async fn no_movement_endpoint_reports_progress_while_the_step_is_frozen_884() {
        for &reason in ControllerHoldReason::ALL {
            for (path, body, queued) in gated_movement_endpoints() {
                let state = empty_state();
                held(&state, reason);
                let app = router().with_state(state.clone());
                let mut rb = Request::post(path);
                if !body.is_empty() { rb = rb.header("content-type", "application/json"); }
                let resp = app.oneshot(rb.body(Body::from(body)).unwrap()).await.unwrap();
                let status = resp.status();
                let text = body_text(resp).await;
                match reason.motion() {
                    HeldMotion::AllFrozen => {
                        assert_eq!(status, StatusCode::CONFLICT,
                            "POST {path} under `{}` must be REFUSED, not answered as accepted \
                             work: got {status} {text}", reason.as_str());
                        let j: serde_json::Value = serde_json::from_str(&text)
                            .unwrap_or_else(|e| panic!("POST {path} refusal must be JSON an agent \
                                 can branch on, not prose: {e} in {text:?}"));
                        assert_eq!(j["status"], "held",
                            "POST {path} needs a machine-branchable `held` token: {j}");
                        assert_eq!(j["hold"]["reason"], reason.as_str(),
                            "POST {path} must name WHICH hold: {j}");
                        assert!(!queued(&state),
                            "POST {path} refused under `{}` but still queued the work — an \
                             accepted-and-inert request is the same lie one step later",
                            reason.as_str());
                    }
                    HeldMotion::LateralStillReaches => {
                        assert_ne!(status, StatusCode::CONFLICT,
                            "POST {path} under `{}` must NOT be refused — a lateral wish still \
                             reaches this body, and `/v1/move/manual` walking it out is its only \
                             client-API exit: got {status} {text}", reason.as_str());
                        assert_eq!(status, StatusCode::OK, "POST {path}: {status} {text}");
                        assert!(queued(&state),
                            "POST {path} answered 200 under `{}` but queued nothing",
                            reason.as_str());
                    }
                }
            }
        }
    }

    /// The under-fire control for the matrix above: with NO hold in force every one of the same
    /// requests is accepted and queued. Without this, a gate that refused unconditionally would
    /// satisfy the `AllFrozen` half and look like a pass.
    #[tokio::test]
    async fn every_movement_endpoint_still_works_with_no_hold_884() {
        for (path, body, queued) in gated_movement_endpoints() {
            let state = empty_state();
            assert!(state.game_state.load().player_hold.is_none(), "fixture starts unheld");
            let app = router().with_state(state.clone());
            let mut rb = Request::post(path);
            if !body.is_empty() { rb = rb.header("content-type", "application/json"); }
            let resp = app.oneshot(rb.body(Body::from(body)).unwrap()).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "POST {path} unheld: {}", body_text(resp).await);
            assert!(queued(&state), "POST {path} unheld must queue the work");
        }
    }

    /// **The matrix above grades itself, and this is the anchor that stops it.** It derives its
    /// expectation from `reason.motion()` — the SAME function the gate consults — so a wrong
    /// `motion()` mapping would satisfy it in both directions and look green. This one pins the HTTP
    /// consequence of the `UnderworldNoRecovery` arm to a literal.
    #[tokio::test]
    async fn manual_under_an_underworld_hold_is_still_accepted_884() {
        let state = empty_state();
        held(&state, ControllerHoldReason::UnderworldNoRecovery);
        let app = router().with_state(state.clone());
        let resp = app.oneshot(Request::post("/manual")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"east":1.0}"#)).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK,
            "refusing /manual here would remove this body's ONLY client-API exit (movement.rs's \
             `last_resort_placement` doc) and would itself be a false \"you cannot move\": {}",
            body_text(resp).await);
        assert!(state.camera.manual_move.lock().unwrap().is_some(),
            "…and it must really be queued, not merely 200'd");
    }

    /// The refusal's `hold` object and `GET /v1/observe/debug`'s `player.hold` must be the SAME
    /// text, because they are the same fact. Two hand-built disclosures drift — which is why
    /// `PlayerHoldView::of` is the only constructor.
    #[tokio::test]
    async fn the_refusal_and_the_debug_endpoint_publish_one_hold_disclosure_884() {
        let state = empty_state();
        held(&state, ControllerHoldReason::EmbeddedNoRecovery);
        let from_debug = serde_json::to_value(
            crate::PlayerState::from_game_state(&state.game_state.load()).hold.expect("hold set")
        ).unwrap();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/jump").body(Body::empty()).unwrap()).await.unwrap();
        let j = body_json(resp).await;
        assert_eq!(j["hold"], from_debug,
            "the 409's hold and player.hold must be byte-identical: {j} vs {from_debug}");
    }
}
