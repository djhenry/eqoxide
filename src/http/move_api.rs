//! `/v1/move/*` — movement: walk to a target/coords and stop, follow a target, stop, cross a zone line.
//!
//! NOTE (#267, revised #328): `/goto`, `/manual`, `/zone_cross`, … all take an *optional* JSON body
//! via [`OptionalJson`]. It judges "was a body sent" from the raw bytes, not the `Content-Type`
//! header, so — unlike the old `Option<axum::Json<T>>` — a body sent without the header still gets
//! parsed (or a clear 400) instead of being silently ignored. A body that IS present but fails to
//! parse (bad syntax, or a field out of range like `zone_id: 99999`) always 400s naming the problem;
//! it is never downgraded to "no body" (bodyless requests like `/stop`, `/jump` are unaffected either way).

use axum::{extract::State, http::StatusCode, routing::post, Router};
use std::collections::HashMap;
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/goto", post(post_goto))
        .route("/follow", post(post_follow))
        .route("/stop", post(post_stop))
        .route("/zone_cross", post(post_zone_cross))
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

/// POST /v1/move/manual — drive the CharacterController directly (like WASD), bypassing A*: escape a
/// spot where `goto` returns no_path (#188), or swim up/down in water with `up` (#207). Body:
/// `{east, north, up, jump, duration_ms}`. Takes priority over any in-progress `/goto` (which it
/// cancels) but yields to real keyboard input.
async fn post_manual(
    State(s): State<HttpState>,
    OptionalJson(body): OptionalJson<ManualBody>,
) -> (StatusCode, String) {
    let b = body.unwrap_or_default();
    let dir = [b.east.unwrap_or(0.0), b.north.unwrap_or(0.0)];
    let up = b.up.unwrap_or(0.0).clamp(-1.0, 1.0);
    let jump = b.jump.unwrap_or(false);
    let ms = b.duration_ms.unwrap_or(400).min(5000);
    if dir[0] == 0.0 && dir[1] == 0.0 && up == 0.0 && !jump {
        return (StatusCode::BAD_REQUEST, "provide a direction {east,north}, {up:-1..1} (swim), and/or {\"jump\":true}".into());
    }
    *s.manual_move.lock().unwrap() = Some(ManualMove {
        dir, up, jump,
        until: std::time::Instant::now() + std::time::Duration::from_millis(ms),
    });
    (StatusCode::OK, format!("manual move dir=({:.1},{:.1}) up={up:.1} jump={jump} for {ms}ms", dir[0], dir[1]))
}

/// POST /v1/move/jump — a single hop in place (a discrete convenience over `/manual` with only
/// `jump`). Clears any `/goto` and pops the character up — on land it's a jump; in water it swims
/// upward toward the surface (#207), e.g. to lift off a pool floor.
async fn post_jump(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.manual_move.lock().unwrap() = Some(ManualMove {
        dir: [0.0, 0.0], up: 0.0, jump: true,
        until: std::time::Instant::now() + std::time::Duration::from_millis(400),
    });
    (StatusCode::OK, "jump".into())
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct MoveBody {
    name:  Option<String>,
    /// Map coordinates (Brewall .txt values) = negated server x/y. goto only.
    map_x: Option<f32>,
    map_y: Option<f32>,
    /// Raw server coordinates. goto only.
    x:     Option<f32>,
    y:     Option<f32>,
    z:     Option<f32>,
    /// Route around KOS/hostile NPC aggro range (#242). Since the client has no broad faction data,
    /// this avoids ALL live NPC camps (soft bias, never fails the route). `true` (default) keeps the
    /// historical avoidance; `false` routes straight through (e.g. to walk INTO a mob).
    avoid_aggro:  Option<bool>,
    /// Extra berth (world units) to give each NPC beyond the ~50u default aggro radius, for routing
    /// more conservatively around dangerous pulls. Default 0.
    aggro_buffer: Option<f32>,
}

/// Apply the request's aggro-avoidance knobs to the shared nav setting the walker reads (#242). Only
/// overrides a field when the request provides it, so omitting them leaves the current setting.
fn apply_avoid_opts(nav_avoid: &crate::http::NavAvoidShared, avoid_aggro: Option<bool>, aggro_buffer: Option<f32>) {
    let mut o = nav_avoid.lock().unwrap();
    if let Some(e) = avoid_aggro  { o.enabled = e; }
    if let Some(b) = aggro_buffer { o.buffer  = b.clamp(0.0, 500.0); }
}

impl MoveBody {
    /// True when any coordinate field is present (used to reject coords on /follow).
    fn has_coords(&self) -> bool {
        self.x.is_some() || self.y.is_some() || self.z.is_some()
            || self.map_x.is_some() || self.map_y.is_some()
    }
}

/// Resolve a (fuzzy) entity name to its (key, position) from the live entity table.
/// Match order: exact key → clean-name equality → substring, capturing the matched KEY so a
/// follow can later re-resolve the same entity's live position.
fn resolve_name(
    name: &str,
    positions: &HashMap<String, (f32, f32, f32)>,
) -> Option<(String, (f32, f32, f32))> {
    let nl = name.to_lowercase();
    positions.get_key_value(name).map(|(k, &p)| (k.clone(), p))
        .or_else(|| positions.iter()
            .find(|(k, _)| clean_entity_name(k).to_lowercase() == nl)
            .map(|(k, &p)| (k.clone(), p)))
        .or_else(|| positions.iter()
            .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl)
                || k.to_lowercase().contains(&nl))
            .map(|(k, &p)| (k.clone(), p)))
}

/// Resolve the player's current target (the player's `target_id`) to its (key, position).
/// Returns `Err((status, msg))` when there is no target, or the target isn't in the live tables.
fn resolve_current_target(
    target_id: Option<u32>,
    ids: &HashMap<String, u32>,
    positions: &HashMap<String, (f32, f32, f32)>,
) -> Result<(String, (f32, f32, f32)), (StatusCode, String)> {
    let target_id = target_id
        .ok_or((StatusCode::BAD_REQUEST, "no target; provide a name or coords".to_string()))?;
    let key = ids.iter()
        .find(|(_, &id)| id == target_id)
        .map(|(k, _)| k.clone())
        .ok_or((StatusCode::NOT_FOUND, format!("current target (spawn {target_id}) is not in view")))?;
    let pos = positions.get(&key).copied()
        .ok_or((StatusCode::NOT_FOUND, format!("current target {key:?} has no known position")))?;
    Ok((key, pos))
}

/// POST /v1/move/goto — walk to a target and STOP on arrival; never chases (goto_entity=None).
/// Body: {"name":...} | {"x","y","z"} | {"map_x","map_y"} | {} (default: current target).
async fn post_goto(
    State(s): State<HttpState>,
    OptionalJson(body): OptionalJson<MoveBody>,
) -> (StatusCode, String) {
    let b = body.unwrap_or_default();

    let target: (f32, f32, f32) = if let Some(name) = &b.name {
        match resolve_name(name, &s.entity_positions.lock().unwrap()) {
            Some((_key, pos)) => pos,
            None => return (StatusCode::NOT_FOUND, format!("No entity named {name:?}")),
        }
    } else if let (Some(mx), Some(my)) = (b.map_x, b.map_y) {
        // map_x = -server_x, map_y = -server_y (Brewall map coords).
        (-mx, -my, b.z.unwrap_or(3.75))
    } else if let (Some(x), Some(y), Some(z)) = (b.x, b.y, b.z) {
        (x, y, z)
    } else {
        // No name/coords → default to the player's current target (one-time snapshot).
        let target_id = s.player().target_id;
        let ids = s.entity_ids.lock().unwrap();
        let positions = s.entity_positions.lock().unwrap();
        match resolve_current_target(target_id, &ids, &positions) {
            Ok((_key, pos)) => pos,
            Err(e) => return e,
        }
    };

    // Apply aggro-avoidance knobs for this route (#242).
    apply_avoid_opts(&s.nav_avoid, b.avoid_aggro, b.aggro_buffer);
    // Set the position, then clear any chase — goto walks to a fixed point and stops.
    *s.goto_target.lock().unwrap() = Some(target);
    *s.goto_entity.lock().unwrap() = None;
    tracing::info!("move/goto: target set to ({:.1},{:.1},{:.1})", target.0, target.1, target.2);
    (StatusCode::OK, format!("navigating to ({:.1},{:.1},{:.1})", target.0, target.1, target.2))
}

/// POST /v1/move/follow — walk to a named entity and KEEP FOLLOWING (goto_entity=Some) until
/// canceled. Body: {"name":...} | {} (default: current target). Coordinates are rejected (400).
async fn post_follow(
    State(s): State<HttpState>,
    OptionalJson(body): OptionalJson<MoveBody>,
) -> (StatusCode, String) {
    let b = body.unwrap_or_default();

    if b.has_coords() {
        return (StatusCode::BAD_REQUEST,
            "follow requires a name or the current target, not coordinates (use /v1/move/goto)".into());
    }

    let (key, pos) = if let Some(name) = &b.name {
        match resolve_name(name, &s.entity_positions.lock().unwrap()) {
            Some(kp) => kp,
            None => return (StatusCode::NOT_FOUND, format!("No entity named {name:?}")),
        }
    } else {
        let target_id = s.player().target_id;
        let ids = s.entity_ids.lock().unwrap();
        let positions = s.entity_positions.lock().unwrap();
        match resolve_current_target(target_id, &ids, &positions) {
            Ok(kp) => kp,
            Err(e) => return e,
        }
    };

    // Position first, then the chase key: the nav thread re-resolves the key's live position each
    // tick (eqoxide#88) and homes in as the entity moves.
    *s.goto_target.lock().unwrap() = Some(pos);
    *s.goto_entity.lock().unwrap() = Some(key.clone());
    tracing::info!("move/follow: chasing {:?} from ({:.1},{:.1},{:.1})", key, pos.0, pos.1, pos.2);
    (StatusCode::OK, format!("following {}", clean_entity_name(&key)))
}

/// POST /v1/move/stop — cancel any active goto/follow. Idempotent. Clears goto_target and
/// goto_entity; the nav thread then clears nav_intent next tick via its "no goto ⇒ no nav" invariant.
async fn post_stop(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.goto_target.lock().unwrap() = None;
    *s.goto_entity.lock().unwrap() = None;
    tracing::info!("move/stop: navigation cancelled");
    (StatusCode::OK, "navigation stopped".into())
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ZoneCrossBody {
    /// Destination zone id to cross to. Omit (or 0) to take the nearest zone line. Deliberately
    /// wider than the wire `u16` zone id so an out-of-range value (e.g. 99999) parses as a normal
    /// field instead of failing the whole body — that failure used to collapse the entire request
    /// to "no body", silently defaulting to `zone_id=0` (walk to the nearest zone line) and
    /// returning 200 instead of rejecting the bogus id (eqoxide#328). It's range-checked below,
    /// alongside the "no zone line to that id" check, with the same reachable-zone_ids message.
    zone_id: Option<u32>,
    /// Route around NPC aggro range on the way to the zone line (#242). See `MoveBody`.
    avoid_aggro:  Option<bool>,
    aggro_buffer: Option<f32>,
}

/// Sorted, de-duplicated set of zone_ids reachable via a zone line from the current zone.
fn reachable_zone_ids(zps: &[crate::game_state::ZonePoint]) -> Vec<u16> {
    let mut ids: Vec<u16> = zps.iter().map(|zp| zp.zone_id).filter(|&z| z != 0).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// POST /v1/move/zone_cross — WALK to a zone line, then cross it (OP_ZONE_CHANGE fires on arrival).
/// It does NOT teleport — the character navigates to the DRNTP zone-line region on foot, so a
/// success response only means the crossing was QUEUED, not that the zone changed (#267). Poll
/// `/v1/observe/debug` (`zone` + `nav_state`) to confirm arrival: if the walker wedges before
/// reaching the line (e.g. a nav trap), the zone won't change even though this returned 200.
/// Body: {"zone_id": 1} to cross to a specific zone, or {} for the nearest line.
///
/// A specific `zone_id` that has no zone line from the current zone is REJECTED with 400 (and the
/// list of reachable zone_ids) instead of silently doing nothing / crossing a nearby line — so the
/// caller knows the destination wasn't honored (eqoxide#47). NOTE this only checks that a zone LINE
/// exists, not that the walker can physically reach it.
async fn post_zone_cross(
    State(s): State<HttpState>,
    OptionalJson(body): OptionalJson<ZoneCrossBody>,
) -> (StatusCode, String) {
    let b = body.unwrap_or_default();
    apply_avoid_opts(&s.nav_avoid, b.avoid_aggro, b.aggro_buffer);
    let zone_id = b.zone_id.unwrap_or(0);
    if zone_id != 0 {
        // A zone_id that doesn't fit the wire u16 (e.g. 99999) can never match a zone line, so
        // fold it into the same "not reachable" rejection as an in-range-but-unreachable id —
        // same message shape either way — instead of a separate generic range error (eqoxide#328).
        let reachable = reachable_zone_ids(&s.zone_points.lock().unwrap());
        let is_reachable = u16::try_from(zone_id).is_ok_and(|z| reachable.contains(&z));
        if !is_reachable {
            let msg = if reachable.is_empty() {
                format!("zone_id {zone_id} is not reachable: no zone lines are known for the current \
                         zone yet (still loading, or this zone has none)")
            } else {
                format!("zone_id {zone_id} is not reachable from the current zone; reachable zone_ids: {reachable:?}")
            };
            tracing::info!("zone_cross: rejected unreachable zone_id={zone_id} (reachable={reachable:?})");
            return (StatusCode::BAD_REQUEST, msg);
        }
    }
    let zone_id = zone_id as u16; // safe: either 0, or validated above to fit u16 and be reachable
    *s.zone_cross.lock().unwrap() = Some(zone_id);
    tracing::info!("zone_cross: flagged for OP_ZONE_CHANGE (target zone_id={zone_id})");
    // Honest, async-aware response (#267): the client WALKS to the zone line, it does not teleport, so
    // this 200 means "accepted", not "arrived". Tell the caller how to observe the real outcome — a bare
    // "queued" read as success while a wedged character went nowhere.
    (StatusCode::OK, format!(
        "zone_cross to zone_id={zone_id} accepted — walking to the zone line (async, not a teleport). \
         Poll GET /v1/observe/debug: the `zone` field changes on success. Every failure is now reported \
         honestly in `nav_state` (+`nav_reason`): `no_path` = no route to the line EXISTS (definitive), \
         `search_exhausted` = the planner gave up ('I don't know', not 'no'), `blocked` = a route exists \
         but the walker physically wedged. See docs/http-api.md 'Navigation state'."))
}

#[cfg(test)]
mod tests {
    use super::{reachable_zone_ids, resolve_name, resolve_current_target, router};
    use axum::http::StatusCode;
    use axum::body::Body;
    use axum::http::Request;
    use std::collections::HashMap;
    use tower::ServiceExt;
    use crate::game_state::ZonePoint;
    use crate::http::quests::tests::{empty_state, set_gs};

    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn zp(zone_id: u16) -> ZonePoint {
        ZonePoint { iterator: 0, server_x: 0.0, server_y: 0.0, server_z: 0.0, heading: 0.0, zone_id }
    }

    fn positions() -> HashMap<String, (f32, f32, f32)> {
        let mut m = HashMap::new();
        m.insert("a_rat00".to_string(), (10.0, 20.0, 3.0));
        m.insert("Guard_Phaeton00".to_string(), (5.0, 6.0, 7.0));
        m
    }

    #[test]
    fn reachable_ids_are_sorted_deduped_and_drop_zero() {
        let zps = vec![zp(9), zp(1), zp(9), zp(0)];
        let r = reachable_zone_ids(&zps);
        assert_eq!(r, vec![1, 9], "sorted, de-duplicated, no 0: {r:?}");
        assert!(!r.contains(&24), "an unconnected zone (24) is not reachable");
        assert!(reachable_zone_ids(&[]).is_empty(), "no zone points → nothing reachable");
    }

    #[test]
    fn resolve_name_matches_by_clean_name_and_captures_key() {
        let pos = positions();
        let (key, p) = resolve_name("a rat", &pos).expect("clean-name match");
        assert_eq!(key, "a_rat00");
        assert_eq!(p, (10.0, 20.0, 3.0));
    }

    #[test]
    fn resolve_name_returns_none_for_unknown() {
        assert!(resolve_name("a dragon", &positions()).is_none());
    }

    #[test]
    fn resolve_current_target_errs_when_no_target() {
        let (status, _) = resolve_current_target(None, &HashMap::new(), &HashMap::new()).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn resolve_current_target_errs_when_not_in_view() {
        let mut ids = HashMap::new();
        ids.insert("a_rat00".to_string(), 42u32);
        // target_id 99 has no matching entity key.
        let (status, _) = resolve_current_target(Some(99), &ids, &positions()).unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn resolve_current_target_returns_key_and_pos() {
        let mut ids = HashMap::new();
        ids.insert("a_rat00".to_string(), 42u32);
        let (key, p) = resolve_current_target(Some(42), &ids, &positions()).expect("resolved");
        assert_eq!(key, "a_rat00");
        assert_eq!(p, (10.0, 20.0, 3.0));
    }

    // --- zone_cross: eqoxide#328 regression coverage -----------------------------------------

    /// The exact repro from #328: a `zone_id` that overflows `u16` must 400, not silently collapse
    /// to "no body" → `zone_id=0` → 200 + walk to the nearest line.
    #[tokio::test]
    async fn zone_cross_out_of_range_zone_id_is_400_with_reachable_list() {
        let state = empty_state();
        state.zone_points.lock().unwrap().extend([zp(1), zp(2), zp(38)]);
        let zc = state.zone_cross.clone();
        let app = router().with_state(state);
        let req = Request::post("/zone_cross")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"zone_id":99999}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let text = body_text(resp).await;
        assert!(text.contains("reachable zone_ids: [1, 2, 38]"), "message: {text}");
        assert!(zc.lock().unwrap().is_none(), "an out-of-range zone_id must not queue a zone cross");
    }

    /// The out-of-range message must have the SAME shape as the pre-existing in-range-but-unreachable
    /// rejection (requirement from #328) — same wording, same reachable-list format.
    #[tokio::test]
    async fn zone_cross_out_of_range_and_in_range_unreachable_share_message_shape() {
        let state = empty_state();
        state.zone_points.lock().unwrap().extend([zp(1), zp(2), zp(38)]);
        let app = router().with_state(state.clone());
        let req = Request::post("/zone_cross")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"zone_id":12345}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let in_range_text = body_text(resp).await;
        assert!(in_range_text.contains("reachable zone_ids: [1, 2, 38]"), "message: {in_range_text}");

        let app2 = router().with_state(state);
        let req2 = Request::post("/zone_cross")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"zone_id":99999}"#)).unwrap();
        let resp2 = app2.oneshot(req2).await.unwrap();
        let out_of_range_text = body_text(resp2).await;
        let shape = |s: &str| s.replacen("12345", "X", 1).replacen("99999", "X", 1);
        assert_eq!(shape(&in_range_text), shape(&out_of_range_text),
            "in-range-unreachable and out-of-range should read identically apart from the id: \
             {in_range_text:?} vs {out_of_range_text:?}");
    }

    #[tokio::test]
    async fn zone_cross_valid_reachable_zone_id_is_200_and_queues() {
        let state = empty_state();
        state.zone_points.lock().unwrap().extend([zp(1), zp(2), zp(38)]);
        let zc = state.zone_cross.clone();
        let app = router().with_state(state);
        let req = Request::post("/zone_cross")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"zone_id":2}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*zc.lock().unwrap(), Some(2));
    }

    /// A genuinely absent body is the legitimate "nearest zone line" request — must keep working.
    #[tokio::test]
    async fn zone_cross_no_body_defaults_to_nearest_line() {
        let state = empty_state();
        let zc = state.zone_cross.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/zone_cross").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*zc.lock().unwrap(), Some(0));
    }

    /// Syntactically-broken JSON (not just an out-of-range field) must also 400, not silently no-op.
    #[tokio::test]
    async fn zone_cross_malformed_json_syntax_is_400_and_does_not_queue() {
        let state = empty_state();
        let zc = state.zone_cross.clone();
        let app = router().with_state(state);
        let req = Request::post("/zone_cross")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"zone_id":}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(zc.lock().unwrap().is_none());
    }

    /// serde_json's streaming Deserializer stops at the end of the FIRST value, so without an
    /// explicit `de.end()` a body like `{"zone_id":45} lolwut` (or two concatenated objects) parses
    /// as a valid request and the garbage is silently ignored — the same silent-acceptance class as
    /// #328, in a smaller form. `axum::Json` rejects both; so must we.
    #[tokio::test]
    async fn zone_cross_trailing_garbage_after_json_is_400_and_does_not_queue() {
        for body in [r#"{"zone_id":2} lolwut"#, r#"{"zone_id":2}{"zone_id":38}"#] {
            let state = empty_state();
            state.zone_points.lock().unwrap().extend([zp(2), zp(38)]);
            let zc = state.zone_cross.clone();
            let app = router().with_state(state);
            let req = Request::post("/zone_cross")
                .header("content-type", "application/json")
                .body(Body::from(body)).unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "body {body:?} must be rejected");
            assert!(zc.lock().unwrap().is_none(), "body {body:?} must not queue a zone cross");
        }
    }

    /// eqoxide#341: a typo'd key ("zone_idd" instead of "zone_id") must 400 — not be silently
    /// ignored by serde (leaving `zone_id` at its default `None`/0) and fall through to walking to
    /// the nearest zone line.
    #[tokio::test]
    async fn zone_cross_unknown_key_is_400_and_does_not_queue() {
        let state = empty_state();
        state.zone_points.lock().unwrap().extend([zp(1), zp(2), zp(38)]);
        let zc = state.zone_cross.clone();
        let app = router().with_state(state);
        let req = Request::post("/zone_cross")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"zone_idd":2}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(zc.lock().unwrap().is_none(),
            "a typo'd key must not silently fall through to walking to the nearest zone line");
    }

    // --- goto: a malformed body must not silently fall back to "current target" ----------------

    #[tokio::test]
    async fn goto_malformed_coordinate_is_400_not_silently_defaulted() {
        let state = empty_state();
        // A current target IS set — under the old Option<Json<T>> bug this is exactly the
        // "meaningful default" a malformed body would silently fall through to.
        set_gs(&state, |gs| gs.target_id = Some(42));
        let goto_target = state.goto_target.clone();
        let app = router().with_state(state);
        let req = Request::post("/goto")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"x":"not-a-number","y":1.0,"z":2.0}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST,
            "a malformed field must 400, not fall through to the current-target default");
        assert!(goto_target.lock().unwrap().is_none());
    }

    /// eqoxide#341: a typo'd key ("nmae" instead of "name") must 400 — not be silently ignored by
    /// serde (leaving `name` at its default `None`) and fall through to the current-target default.
    #[tokio::test]
    async fn goto_unknown_key_is_400_not_silently_defaulted() {
        let state = empty_state();
        state.entity_ids.lock().unwrap().insert("a_rat00".into(), 42);
        state.entity_positions.lock().unwrap().insert("a_rat00".into(), (10.0, 20.0, 3.0));
        set_gs(&state, |gs| gs.target_id = Some(42));
        let goto_target = state.goto_target.clone();
        let app = router().with_state(state);
        let req = Request::post("/goto")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"nmae":"a rat"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST,
            "a typo'd key must 400, not fall through to the current-target default");
        assert!(goto_target.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn goto_no_body_falls_back_to_current_target() {
        let state = empty_state();
        state.entity_ids.lock().unwrap().insert("a_rat00".into(), 42);
        state.entity_positions.lock().unwrap().insert("a_rat00".into(), (10.0, 20.0, 3.0));
        set_gs(&state, |gs| gs.target_id = Some(42));
        let goto_target = state.goto_target.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/goto").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*goto_target.lock().unwrap(), Some((10.0, 20.0, 3.0)));
    }

    // --- follow: a malformed body must not silently fall back to "current target" --------------

    #[tokio::test]
    async fn follow_malformed_name_is_400_not_silently_defaulted() {
        let state = empty_state();
        state.entity_ids.lock().unwrap().insert("a_rat00".into(), 42);
        state.entity_positions.lock().unwrap().insert("a_rat00".into(), (10.0, 20.0, 3.0));
        // A current target IS set — the old Option<Json<T>> bug would silently chase IT instead of
        // reporting the malformed "name" field.
        set_gs(&state, |gs| gs.target_id = Some(42));
        let goto_entity = state.goto_entity.clone();
        let goto_target = state.goto_target.clone();
        let app = router().with_state(state);
        let req = Request::post("/follow")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":5}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST,
            "a malformed name must 400, not fall through to following the current target");
        assert!(goto_entity.lock().unwrap().is_none());
        assert!(goto_target.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn follow_no_body_falls_back_to_current_target() {
        let state = empty_state();
        state.entity_ids.lock().unwrap().insert("a_rat00".into(), 42);
        state.entity_positions.lock().unwrap().insert("a_rat00".into(), (10.0, 20.0, 3.0));
        set_gs(&state, |gs| gs.target_id = Some(42));
        let goto_entity = state.goto_entity.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/follow").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(goto_entity.lock().unwrap().as_deref(), Some("a_rat00"));
    }

    /// The pre-existing "coords are not allowed on /follow" 400 must survive the extractor swap.
    #[tokio::test]
    async fn follow_with_coords_is_still_400() {
        let state = empty_state();
        let app = router().with_state(state);
        let req = Request::post("/follow")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"x":1.0,"y":2.0,"z":3.0}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let text = body_text(resp).await;
        assert!(text.contains("not coordinates"), "message: {text}");
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
}
