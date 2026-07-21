//! `/v1/move/*` — movement: walk to a target/coords and stop, follow a target, stop, cross a zone line.
//!
//! NOTE (#267, revised #328): `/goto`, `/manual`, `/zone_cross`, … all take an *optional* JSON body
//! via [`OptionalJson`]. It judges "was a body sent" from the raw bytes, not the `Content-Type`
//! header, so — unlike the old `Option<axum::Json<T>>` — a body sent without the header still gets
//! parsed (or a clear 400) instead of being silently ignored. A body that IS present but fails to
//! parse (bad syntax, or a field out of range like `zone_id: 99999`) always 400s naming the problem;
//! it is never downgraded to "no body" (bodyless requests like `/stop`, `/jump` are unaffected either way).

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use std::collections::HashMap;
use super::*;
use crate::name_match::{distance_between, resolve_in_world, MatchQuality, NameMatch};

/// A `text/plain` response (for require_live_session errors and malformed-body 4xx). Mirrors
/// `http::combat`'s local helper — `/goto` and `/follow` now answer with JSON on success (#513).
fn text(status: StatusCode, body: impl Into<String>) -> Response {
    Response::builder().status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body.into())).unwrap()
}

/// An `application/json` response.
fn json(status: StatusCode, value: serde_json::Value) -> Response {
    Response::builder().status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string())).unwrap()
}

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
    if let Err(e) = require_live_session(&s) { return e; }
    let b = body.unwrap_or_default();
    let dir = [b.east.unwrap_or(0.0), b.north.unwrap_or(0.0)];
    let up = b.up.unwrap_or(0.0).clamp(-1.0, 1.0);
    let jump = b.jump.unwrap_or(false);
    let ms = b.duration_ms.unwrap_or(400).min(5000);
    if dir[0] == 0.0 && dir[1] == 0.0 && up == 0.0 && !jump {
        return (StatusCode::BAD_REQUEST, "provide a direction {east,north}, {up:-1..1} (swim), and/or {\"jump\":true}".into());
    }
    s.camera.request_manual_move(ManualMove {
        dir, up, jump,
        until: std::time::Instant::now() + std::time::Duration::from_millis(ms),
    });
    (StatusCode::OK, format!("manual move dir=({:.1},{:.1}) up={up:.1} jump={jump} for {ms}ms", dir[0], dir[1]))
}

/// POST /v1/move/jump — a single hop in place (a discrete convenience over `/manual` with only
/// `jump`). Clears any `/goto` and pops the character up — on land it's a jump; in water it swims
/// upward toward the surface (#207), e.g. to lift off a pool floor.
async fn post_jump(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    s.camera.request_manual_move(ManualMove {
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
fn apply_avoid_opts(nav_avoid: &crate::NavAvoidShared, avoid_aggro: Option<bool>, aggro_buffer: Option<f32>) {
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

/// Resolve the player's CURRENT TARGET to a [`NameMatch`], so the "no name/coords" default of
/// `/goto` and `/follow` discloses which spawn it actually resolved to, exactly like a by-name call.
///
/// ⚠️ Acquires both world tables in the CANONICAL order — `entity_positions` BEFORE `entity_ids` —
/// matching `ActionLoop::sync_entities`. See [`resolve_in_world`] for why the inverse order is a
/// whole-client deadlock.
///
/// `quality` is `Exact` and `candidates` is 1 by construction: a target is identified by a definite
/// spawn id, so there is nothing ambiguous to disclose.
fn current_target_match(
    s: &HttpState,
    player_pos: Option<(f32, f32, f32)>,
) -> Result<NameMatch, (StatusCode, String)> {
    let target_id = s.player().target_id;
    let (key, pos) = {
        let positions = s.world.entity_positions.lock().unwrap(); // 1st — canonical order
        let ids = s.world.entity_ids.lock().unwrap();             // 2nd
        resolve_current_target(target_id, &ids, &positions)?
    };
    Ok(NameMatch {
        id: target_id.expect("resolve_current_target Ok implies a target_id"),
        name: clean_entity_name(&key),
        key,
        quality: MatchQuality::Exact,
        pos: Some(pos),
        distance: distance_between(player_pos, Some(pos)),
        candidates: 1,
    })
}

/// POST /v1/move/goto — walk to a target and STOP on arrival; never chases (goto_entity=None).
/// Body: {"name":...} | {"x","y","z"} | {"map_x","map_y"} | {} (default: current target).
///
/// #513 (agent-honesty): when the goal is resolved from a NAME (or defaults to the current target),
/// the response DISCLOSES the matched entity — `matched:{id, name, quality, distance?}` — so the
/// caller can confirm the fuzzy name-resolution picked the intended spawn. The routed goal and the
/// disclosed `matched` derive from ONE `NameMatch`, so the coordinates the character walks to can
/// never disagree with the entity named in the response. `quality` is `"exact"` (a case-insensitive
/// name match, always preferred over any nearer partial one) or `"fuzzy"` (only a substring match
/// existed — the agent should verify before trusting it). For a raw-coordinate goal there is no
/// entity, so `matched` is `null`.
async fn post_goto(
    State(s): State<HttpState>,
    OptionalJson(body): OptionalJson<MoveBody>,
) -> Response {
    if let Err((code, msg)) = require_live_session(&s) { return text(code, msg); }
    let b = body.unwrap_or_default();
    let player_pos = s.player_pos();

    // Resolve the goal to `(coords, Option<NameMatch>)`. The matched entity (when any) is the SAME
    // value the goal coordinates come from, so the disclosure can't drift from the routed target.
    let (target, matched): ((f32, f32, f32), Option<NameMatch>) = if let Some(name) = &b.name {
        match resolve_in_world(&s.world, name, player_pos) {
            Some(m) => match m.pos {
                Some(pos) => (pos, Some(m)),
                // Matched an entity that has an id but no known position — can't navigate to it.
                // Honest failure rather than a bogus goal (lockstep tables make this unreachable in
                // practice, but never silently invent coordinates).
                None => return json(StatusCode::NOT_FOUND, serde_json::json!({
                    "status": "not_found",
                    "message": format!("entity {:?} has no known position to navigate to", m.name),
                })),
            },
            None => return json(StatusCode::NOT_FOUND, serde_json::json!({
                "status": "not_found",
                "message": format!("No entity named {name:?}"),
            })),
        }
    } else if let (Some(mx), Some(my)) = (b.map_x, b.map_y) {
        // map_x = -server_x, map_y = -server_y (Brewall map coords). Raw coords → no matched entity.
        ((-mx, -my, b.z.unwrap_or(3.75)), None)
    } else if let (Some(x), Some(y), Some(z)) = (b.x, b.y, b.z) {
        ((x, y, z), None)
    } else {
        // No name/coords → default to the player's current target (one-time snapshot). Disclose it
        // too: the agent should still be able to confirm WHICH spawn "the current target" resolved to.
        match current_target_match(&s, player_pos) {
            Ok(m) => (m.pos.expect("current_target_match always carries a position"), Some(m)),
            Err((code, msg)) => return text(code, msg),
        }
    };

    // Apply aggro-avoidance knobs for this route (#242).
    apply_avoid_opts(&s.nav.nav_avoid, b.avoid_aggro, b.aggro_buffer);
    // Set the position, then clear any chase — goto walks to a fixed point and stops. `request_goto`
    // stamps a fresh goal identity (state → `pending`, bumped `goal_id`) SYNCHRONOUSLY, so a read
    // right after this can never return the PREVIOUS goto's terminal state (#349).
    let goal_id = s.command.request_goto(target);
    tracing::info!("move/goto: target set to ({:.1},{:.1},{:.1}) [goal #{goal_id}] matched={:?}",
        target.0, target.1, target.2, matched.as_ref().map(|m| (&m.name, m.id, m.quality)));
    // Echo the goal id so the caller can correlate a later `nav_state` read to THIS request: a
    // terminal state on GET /v1/observe/debug is only about the goal it reports in `nav_goal_id`.
    // #579: if the zone's collision grid isn't built yet, NO route can be planned — say so here
    // rather than letting the caller read "navigating" as "a walkable route was found". The goal is
    // still accepted: the walker holds it at `nav_state: "zone_loading"` and plans for real the
    // moment the assets land.
    let assets_pending = (!s.zone_assets.lock().unwrap().is_ready()).then(|| {
        "the zone's terrain/collision are NOT loaded yet, so nothing has been routed — nav_state \
         will read \"zone_loading\" until GET /v1/observe/debug reports zone_assets.state == \
         \"ready\", then this goal is planned normally. (If it reads \"failed\", it never will.)"
    });
    json(StatusCode::OK, serde_json::json!({
        "status": "navigating",
        "goal": [target.0, target.1, target.2],
        "goal_id": goal_id,
        "matched": matched.map(|m| m.to_json()),
        "zone_assets_pending": assets_pending,
        "note": "poll GET /v1/observe/debug; nav_state is honest only for this nav_goal_id (goal_id)",
    }))
}

/// POST /v1/move/follow — walk to a named entity and KEEP FOLLOWING (goto_entity=Some) until
/// canceled. Body: {"name":...} | {} (default: current target). Coordinates are rejected (400).
///
/// #513 review (F3): this now resolves through the SAME [`resolve_in_world`] path as `/goto` and
/// carries the same `matched` disclosure. Previously `/follow` matched over `entity_positions`
/// while `/goto` matched over `entity_ids` — two independently-seeded `HashMap`s — so with N
/// equally-named spawns the two endpoints could pick DIFFERENT entities for the same name (they
/// agreed only ~1 time in N), and `/follow` disclosed nothing, so the agent could not detect the
/// divergence. One resolver, one selection rule, one disclosure.
async fn post_follow(
    State(s): State<HttpState>,
    OptionalJson(body): OptionalJson<MoveBody>,
) -> Response {
    if let Err((code, msg)) = require_live_session(&s) { return text(code, msg); }
    let b = body.unwrap_or_default();

    if b.has_coords() {
        return text(StatusCode::BAD_REQUEST,
            "follow requires a name or the current target, not coordinates (use /v1/move/goto)");
    }

    let player_pos = s.player_pos();
    let matched = if let Some(name) = &b.name {
        match resolve_in_world(&s.world, name, player_pos) {
            Some(m) if m.pos.is_some() => m,
            Some(m) => return json(StatusCode::NOT_FOUND, serde_json::json!({
                "status": "not_found",
                "message": format!("entity {:?} has no known position to follow", m.name),
            })),
            None => return json(StatusCode::NOT_FOUND, serde_json::json!({
                "status": "not_found",
                "message": format!("No entity named {name:?}"),
            })),
        }
    } else {
        match current_target_match(&s, player_pos) {
            Ok(m) => m,
            Err((code, msg)) => return text(code, msg),
        }
    };

    let pos = matched.pos.expect("checked above");
    // Position first, then the chase key: the nav thread re-resolves the key's live position each
    // tick (eqoxide#88) and homes in as the entity moves.
    let goal_id = s.command.request_follow(matched.key.clone(), pos);
    tracing::info!("move/follow: chasing {:?} from ({:.1},{:.1},{:.1}) [goal #{goal_id}]",
        matched.key, pos.0, pos.1, pos.2);
    json(StatusCode::OK, serde_json::json!({
        "status": "following",
        "goal_id": goal_id,
        "matched": matched.to_json(),
    }))
}

/// POST /v1/move/stop — cancel any active goto/follow. Idempotent. Clears goto_target and
/// goto_entity; the nav thread then clears nav_intent next tick via its "no goto ⇒ no nav" invariant.
async fn post_stop(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    // Reset nav_state to `idle` under a fresh goal id SYNCHRONOUSLY (#349): before this, `/stop`
    // returned "navigation stopped" while nav_state still read the cancelled goal's `arrived`.
    let goal_id = s.command.request_stop();
    tracing::info!("move/stop: navigation cancelled [goal #{goal_id}]");
    (StatusCode::OK, format!("navigation stopped [goal_id={goal_id}]"))
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
fn reachable_zone_ids(zps: &[eqoxide_core::game_state::ZonePoint]) -> Vec<u16> {
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
    if let Err(e) = require_live_session(&s) { return e; }
    let b = body.unwrap_or_default();
    apply_avoid_opts(&s.nav.nav_avoid, b.avoid_aggro, b.aggro_buffer);
    let zone_id = b.zone_id.unwrap_or(0);
    if zone_id != 0 {
        // A zone_id that doesn't fit the wire u16 (e.g. 99999) can never match a zone line, so
        // fold it into the same "not reachable" rejection as an in-range-but-unreachable id —
        // same message shape either way — instead of a separate generic range error (eqoxide#328).
        let reachable = reachable_zone_ids(&s.world.zone_points.lock().unwrap());
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
    // Reset nav_state to `pending` under a fresh goal id SYNCHRONOUSLY (#349), so a read right after
    // this 200 can't see the previous nav's terminal state before the walker drains the request.
    let goal_id = s.command.request_zone_cross(zone_id);
    tracing::info!("zone_cross: flagged for OP_ZONE_CHANGE (target zone_id={zone_id}) [goal #{goal_id}]");
    // Honest, async-aware response (#267): the client WALKS to the zone line, it does not teleport, so
    // this 200 means "accepted", not "arrived". Tell the caller how to observe the real outcome — a bare
    // "queued" read as success while a wedged character went nowhere.
    (StatusCode::OK, format!(
        "zone_cross to zone_id={zone_id} accepted [goal_id={goal_id}] — walking to the zone line (async, not a teleport). \
         Poll GET /v1/observe/debug: the `zone` field changes on success. Every failure is now reported \
         honestly in `nav_state` (+`nav_reason`): `no_path` = no route to the line EXISTS (definitive), \
         `search_exhausted` = the planner gave up ('I don't know', not 'no'), `blocked` = a route exists \
         but the walker physically wedged. See docs/http-api.md 'Navigation state'."))
}

#[cfg(test)]
mod tests {
    use super::{reachable_zone_ids, resolve_current_target, router};
    use axum::http::StatusCode;
    use axum::body::Body;
    use axum::http::Request;
    use std::collections::HashMap;
    use tower::ServiceExt;
    use eqoxide_core::game_state::ZonePoint;
    use crate::testkit::{empty_state, set_gs};

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
        state.world.zone_points.lock().unwrap().extend([zp(1), zp(2), zp(38)]);
        let zc = state.nav.zone_cross.clone();
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
        state.world.zone_points.lock().unwrap().extend([zp(1), zp(2), zp(38)]);
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
        state.world.zone_points.lock().unwrap().extend([zp(1), zp(2), zp(38)]);
        let zc = state.nav.zone_cross.clone();
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
        let zc = state.nav.zone_cross.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/zone_cross").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*zc.lock().unwrap(), Some(0));
    }

    /// Syntactically-broken JSON (not just an out-of-range field) must also 400, not silently no-op.
    #[tokio::test]
    async fn zone_cross_malformed_json_syntax_is_400_and_does_not_queue() {
        let state = empty_state();
        let zc = state.nav.zone_cross.clone();
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
            state.world.zone_points.lock().unwrap().extend([zp(2), zp(38)]);
            let zc = state.nav.zone_cross.clone();
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
        state.world.zone_points.lock().unwrap().extend([zp(1), zp(2), zp(38)]);
        let zc = state.nav.zone_cross.clone();
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
        let goto_target = state.nav.goto_target.clone();
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
        state.world.entity_ids.lock().unwrap().insert("a_rat00".into(), 42);
        state.world.entity_positions.lock().unwrap().insert("a_rat00".into(), (10.0, 20.0, 3.0));
        set_gs(&state, |gs| gs.target_id = Some(42));
        let goto_target = state.nav.goto_target.clone();
        let app = router().with_state(state);
        let req = Request::post("/goto")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"nmae":"a rat"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST,
            "a typo'd key must 400, not fall through to the current-target default");
        assert!(goto_target.lock().unwrap().is_none());
    }

    // ── #513: /move/goto discloses the MATCHED entity so the caller can confirm the resolution ───

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        serde_json::from_str(&body_text(resp).await).unwrap()
    }

    /// goto by an EXACT name: 200, and `matched` discloses the resolved id/name/quality. The routed
    /// goal (`goto_target`) equals the matched entity's position — disclosure can't disagree with
    /// where the character actually walks.
    #[tokio::test]
    async fn goto_by_name_discloses_matched_entity() {
        let state = empty_state();
        state.world.entity_ids.lock().unwrap().insert("a_rat003".into(), 55);
        state.world.entity_positions.lock().unwrap().insert("a_rat003".into(), (10.0, 20.0, 3.0));
        let goto_target = state.nav.goto_target.clone();
        let app = router().with_state(state);
        let req = Request::post("/goto")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"a rat"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let j = body_json(resp).await;
        assert_eq!(j["matched"]["id"], 55);
        assert_eq!(j["matched"]["name"], "a rat");
        assert_eq!(j["matched"]["quality"], "exact");
        assert_eq!(*goto_target.lock().unwrap(), Some((10.0, 20.0, 3.0)),
            "the goal must be the disclosed entity's position");
    }

    /// #513 INVARIANT under the near-miss shape: an exact match beside a nearer fuzzy decoy must
    /// route to — and disclose — the EXACT entity. MUTATION CHECK: drop the exact preference in
    /// `resolve_entity` and this goes RED (goal + matched id become the decoy's).
    #[tokio::test]
    async fn goto_by_name_prefers_exact_over_fuzzy_decoy() {
        let state = empty_state();
        {
            let mut ids = state.world.entity_ids.lock().unwrap();
            ids.insert("a_rat003".into(), 55);
            ids.insert("dire_a_rat004".into(), 66); // fuzzy: contains "a rat"
        }
        {
            let mut pos = state.world.entity_positions.lock().unwrap();
            pos.insert("a_rat003".into(), (10.0, 20.0, 3.0));
            pos.insert("dire_a_rat004".into(), (999.0, 999.0, 3.0));
        }
        let goto_target = state.nav.goto_target.clone();
        let app = router().with_state(state);
        let req = Request::post("/goto")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"a rat"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let j = body_json(resp).await;
        assert_eq!(j["matched"]["id"], 55);
        assert_eq!(*goto_target.lock().unwrap(), Some((10.0, 20.0, 3.0)),
            "must route to the exact match, never the distant fuzzy decoy");
    }

    /// A raw-coordinate goal has no entity: `matched` is null (honest — not a fabricated match).
    #[tokio::test]
    async fn goto_by_coords_has_null_matched() {
        let state = empty_state();
        let app = router().with_state(state);
        let req = Request::post("/goto")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"x":1.0,"y":2.0,"z":3.0}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let j = body_json(resp).await;
        assert!(j["matched"].is_null(), "raw coords have no matched entity: {j}");
    }

    /// #513 review F3: `/goto` and `/follow` must resolve an AMBIGUOUS name to the SAME spawn.
    ///
    /// The regression this guards: `/follow` matched over `entity_positions` while `/goto` matched
    /// over `entity_ids` — two independently-seeded HashMaps — so with N equally-named spawns they
    /// agreed only ~1 time in N, and `/follow` disclosed nothing so the divergence was undetectable.
    /// Repeated, because randomized hash order is exactly what made the old bug intermittent.
    #[tokio::test]
    async fn goto_and_follow_resolve_an_ambiguous_name_to_the_same_spawn() {
        for _ in 0..64 {
            let rows = [
                ("a_gnoll000", 100u32, (5000.0, 0.0, 0.0)),
                ("a_gnoll001", 101, (4000.0, 0.0, 0.0)),
                ("a_gnoll002", 102, (10.0, 0.0, 0.0)), // nearest → both must choose THIS
                ("a_gnoll003", 103, (3000.0, 0.0, 0.0)),
                ("a_gnoll004", 104, (2000.0, 0.0, 0.0)),
            ];
            let seed = |state: &crate::HttpState| {
                let mut pos = state.world.entity_positions.lock().unwrap();
                let mut ids = state.world.entity_ids.lock().unwrap();
                for (k, id, p) in rows {
                    pos.insert(k.into(), p);
                    ids.insert(k.into(), id);
                }
            };
            // Player position must be KNOWN for a distance-based pick (#513 F4).
            let mk = || {
                let st = empty_state();
                seed(&st);
                set_gs(&st, |gs| {
                    gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0;
                    gs.player_pos_known = true;
                });
                st
            };

            let g = mk();
            let goto_target = g.nav.goto_target.clone();
            let rg = router().with_state(g).oneshot(Request::post("/goto")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"a gnoll"}"#)).unwrap()).await.unwrap();
            let gj = body_json(rg).await;

            let f = mk();
            let goto_entity = f.nav.goto_entity.clone();
            let rf = router().with_state(f).oneshot(Request::post("/follow")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"a gnoll"}"#)).unwrap()).await.unwrap();
            let fj = body_json(rf).await;

            assert_eq!(gj["matched"]["id"], fj["matched"]["id"],
                "goto and follow must resolve the same name to the SAME spawn");
            assert_eq!(gj["matched"]["id"], 102, "both must pick the NEAREST equal candidate");
            assert_eq!(gj["matched"]["candidates"], 5);
            assert_eq!(fj["matched"]["candidates"], 5, "follow must disclose ambiguity too");
            assert_eq!(*goto_target.lock().unwrap(), Some((10.0, 0.0, 0.0)));
            assert_eq!(goto_entity.lock().unwrap().as_deref(), Some("a_gnoll002"));
        }
    }

    /// #513 review F4: with the player's position NOT yet known (just zoned in), `distance` must be
    /// OMITTED rather than silently measured from the zone origin.
    #[tokio::test]
    async fn goto_omits_distance_when_player_position_is_unknown() {
        let state = empty_state(); // player_pos_known defaults to false
        state.world.entity_ids.lock().unwrap().insert("a_rat003".into(), 55);
        state.world.entity_positions.lock().unwrap().insert("a_rat003".into(), (300.0, 400.0, 0.0));
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/goto")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"a rat"}"#)).unwrap()).await.unwrap();
        let j = body_json(resp).await;
        assert_eq!(j["matched"]["id"], 55);
        assert!(j["matched"].get("distance").is_none(),
            "distance must be omitted while our own position is unknown, not measured from the origin: {j}");
    }

    /// The companion: once the server HAS given us a position, `distance` is reported and real.
    #[tokio::test]
    async fn goto_reports_distance_once_player_position_is_known() {
        let state = empty_state();
        state.world.entity_ids.lock().unwrap().insert("a_rat003".into(), 55);
        state.world.entity_positions.lock().unwrap().insert("a_rat003".into(), (300.0, 400.0, 0.0));
        set_gs(&state, |gs| {
            gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0;
            gs.player_pos_known = true;
        });
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/goto")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"a rat"}"#)).unwrap()).await.unwrap();
        let j = body_json(resp).await;
        assert_eq!(j["matched"]["distance"], 500.0, "3-4-5 triangle scaled ×100");
    }

    /// Honest-404 preserved: goto to a nonexistent name 404s and queues no nav goal.
    #[tokio::test]
    async fn goto_by_nonexistent_name_is_404_and_queues_nothing() {
        let state = empty_state();
        state.world.entity_ids.lock().unwrap().insert("a_rat003".into(), 55);
        state.world.entity_positions.lock().unwrap().insert("a_rat003".into(), (10.0, 20.0, 3.0));
        let goto_target = state.nav.goto_target.clone();
        let app = router().with_state(state);
        let req = Request::post("/goto")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"a dragon"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(goto_target.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn goto_no_body_falls_back_to_current_target() {
        let state = empty_state();
        state.world.entity_ids.lock().unwrap().insert("a_rat00".into(), 42);
        state.world.entity_positions.lock().unwrap().insert("a_rat00".into(), (10.0, 20.0, 3.0));
        set_gs(&state, |gs| gs.target_id = Some(42));
        let goto_target = state.nav.goto_target.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/goto").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*goto_target.lock().unwrap(), Some((10.0, 20.0, 3.0)));
    }

    // --- follow: a malformed body must not silently fall back to "current target" --------------

    #[tokio::test]
    async fn follow_malformed_name_is_400_not_silently_defaulted() {
        let state = empty_state();
        state.world.entity_ids.lock().unwrap().insert("a_rat00".into(), 42);
        state.world.entity_positions.lock().unwrap().insert("a_rat00".into(), (10.0, 20.0, 3.0));
        // A current target IS set — the old Option<Json<T>> bug would silently chase IT instead of
        // reporting the malformed "name" field.
        set_gs(&state, |gs| gs.target_id = Some(42));
        let goto_entity = state.nav.goto_entity.clone();
        let goto_target = state.nav.goto_target.clone();
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
        state.world.entity_ids.lock().unwrap().insert("a_rat00".into(), 42);
        state.world.entity_positions.lock().unwrap().insert("a_rat00".into(), (10.0, 20.0, 3.0));
        set_gs(&state, |gs| gs.target_id = Some(42));
        let goto_entity = state.nav.goto_entity.clone();
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
