//! `/v1/interact/*` — NPC/world interaction: hail, say, loot, give (turn-in), doors, sit/stand.

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::Response,
    routing::post,
    Json, Router,
};
use tokio::sync::oneshot;
use std::time::Duration;
use crate::command_state::{CommandResult, GiveOk};
use super::*;

/// HTTP-side await budget for POST /v1/interact/give (#448). Set GREATER than the net-side worst-case
/// verdict time — the two `tick_give` timeouts run in SEQUENCE, so the net side delivers a verdict by
/// ≈ (GIVE_ACK_TIMEOUT_TICKS + GIVE_FINISH_TIMEOUT_TICKS) × ~150ms ≈ 6s (see `action_loop`). Awaiting
/// 8s here guarantees the NET verdict (Resolved/Unconfirmed from the state machine) reaches the caller
/// rather than a vaguer HTTP-elapsed 202 firing first — the two-timeout ordering landmine.
pub(crate) const GIVE_HTTP_TIMEOUT_SECS: u64 = 8;

/// A quick `(StatusCode, String)` plain-text response, so the small error paths stay terse.
fn text(status: StatusCode, body: impl Into<String>) -> Response {
    Response::builder().status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body.into())).unwrap()
}

/// A JSON response with an explicit status.
fn json(status: StatusCode, value: serde_json::Value) -> Response {
    Response::builder().status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string())).unwrap()
}

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/hail", post(post_hail))
        .route("/say", post(post_say))
        .route("/loot", post(post_loot))
        .route("/give", post(post_give))
        .route("/click_door", post(post_door_click))
        .route("/sit", post(post_sit))
        .route("/stand", post(post_stand))
        .route("/dialogue", post(post_dialogue))
        .route("/read", post(post_read))
}

/// POST /v1/interact/read — read a book or note. Body: `{"slot": N}` where N is the item's
/// inventory wire slot (from GET /v1/observe/inventory; the item must have a non-empty `filename`).
/// Sends OP_ReadBook; the server replies with the text, which appears at GET /v1/observe/item_text
/// (and in the message log under the "book" kind). (#288)
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadBody {
    slot: i32,
}

async fn post_read(
    State(s): State<HttpState>,
    body: Result<Json<ReadBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"slot\":N}".into()),
    };
    // Validate against the last-published inventory so a bad slot fails fast with a clear message,
    // rather than being silently dropped by the nav thread.
    let readable = s.inventory_slots.inventory.lock().unwrap().iter()
        .find(|i| i.slot == b.slot)
        .map(|i| !i.filename.is_empty());
    match readable {
        Some(true) => {
            s.command.request_read_book(b.slot);
            tracing::info!("read: queued book slot={}", b.slot);
            (StatusCode::OK, format!("reading item in slot {}", b.slot))
        }
        Some(false) => (StatusCode::CONFLICT, format!("item in slot {} is not readable", b.slot)),
        None => (StatusCode::NOT_FOUND, format!("no item in slot {}", b.slot)),
    }
}

/// POST /v1/interact/dialogue — click one of the NPC-dialogue choices from GET
/// /v1/observe/dialogue. Body is either `{"index": N}` (position in the choices list) or
/// `{"text": "..."}` (matched case-insensitively against a choice's label). Sends an
/// OP_ItemLinkClick so the server resolves the saylink and treats it as our reply to the NPC. (#120)
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DialogueBody {
    index: Option<usize>,
    text:  Option<String>,
}

async fn post_dialogue(
    State(s): State<HttpState>,
    body: Result<Json<DialogueBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"index\":N} or {\"text\":\"...\"}".into()),
    };
    let choices = s.interact.dialogue.lock().unwrap().clone();
    if choices.is_empty() {
        return (StatusCode::CONFLICT, "no dialogue choices available".into());
    }
    let chosen = if let Some(i) = b.index {
        choices.get(i).cloned()
    } else if let Some(t) = &b.text {
        choices.iter().find(|c| c.text.eq_ignore_ascii_case(t.trim())).cloned()
    } else {
        return (StatusCode::BAD_REQUEST, "provide {\"index\":N} or {\"text\":\"...\"}".into());
    };
    match chosen {
        Some(c) => {
            let label = c.text.clone();
            s.command.request_dialogue_click(c);
            tracing::info!("dialogue: queued click {:?}", label);
            (StatusCode::OK, format!("clicking '{}'", label))
        }
        None => (StatusCode::NOT_FOUND, "no matching dialogue choice".into()),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HailBody {
    /// NPC to hail (fuzzy-matched against /observe/entities). Omit to hail the nearest NPC.
    name: Option<String>,
}

/// POST /v1/interact/hail — say "Hail, <name>" so a nearby NPC fires its hail/quest script.
/// Body: {"name":"Guard Phaeton"} (fuzzy) or {} to hail the nearest NPC.
/// The NPC must be within ~200 units (server-enforced say range).
async fn post_hail(
    State(s): State<HttpState>,
    OptionalJson(body): OptionalJson<HailBody>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let requested = body.and_then(|b| b.name);
    let positions = s.world.entity_positions.lock().unwrap();

    let resolved: Option<String> = if let Some(name) = &requested {
        // Exact (clean) match first, then fuzzy substring.
        let nl = name.to_lowercase();
        positions.keys()
            .find(|k| clean_entity_name(k).to_lowercase() == nl)
            .or_else(|| positions.keys().find(|k| k.to_lowercase().contains(&nl)))
            .cloned()
    } else {
        // Nearest NPC to the player. Camera focus = [east, north, height] =
        // [server_x, server_y, server_z]; entities stored as (server_x, server_y, z).
        let focus = s.camera.snapshot.lock().unwrap().focus;
        positions.iter()
            .filter(|(k, _)| !k.contains("zone_controller"))
            .map(|(k, &(ex, ny, _))| {
                let de = ex - focus[0];
                let dn = ny - focus[1];
                (k.clone(), de * de + dn * dn)
            })
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(k, _)| k)
    };

    match resolved {
        Some(key) => {
            let display_name = clean_entity_name(&key);
            // Resolve the NPC's spawn_id so the nav thread can target it before saying — the
            // server only fires EVENT_SAY on the player's current target (#130).
            let spawn_id = s.world.entity_ids.lock().unwrap().get(&key).copied();
            s.command.request_hail(display_name.clone(), spawn_id);
            tracing::info!("hail: queued hail to {:?} (spawn_id={:?})", display_name, spawn_id);
            (StatusCode::OK, format!("hailing {}", display_name))
        }
        None => {
            let msg = match &requested {
                Some(n) => format!("No NPC matching {:?}", n),
                None => "No NPCs known to hail".to_string(),
            };
            (StatusCode::NOT_FOUND, msg)
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SayBody {
    text: String,
}

/// POST /v1/interact/say {"text":"..."} — say arbitrary text on the Say channel. Used for quest
/// keyword follow-ups (e.g. say "shipment" after an NPC mentions [shipment]).
async fn post_say(
    State(s): State<HttpState>,
    body: Result<Json<SayBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let text = match body {
        Ok(Json(b)) => b.text,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"text\":\"...\"}".into()),
    };
    if text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "empty text".into());
    }
    s.command.request_say(text.clone());
    tracing::info!("say: queued {:?}", text);
    (StatusCode::OK, format!("saying {}", text))
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LootBody {
    /// Corpse spawn id to loot directly.
    id:   Option<u32>,
    /// Corpse name to fuzzy-match (corpses are named like "a_rat000's corpse").
    name: Option<String>,
}

/// A spawn's entity-list key names a corpse (the only class this endpoint is allowed to queue —
/// eqoxide#346: a live mob or a nonexistent spawn must never be silently "looted").
fn is_corpse_key(key: &str) -> bool {
    key.to_lowercase().contains("corpse")
}

fn queue_loot(s: &HttpState, name: String, id: u32) -> (StatusCode, String) {
    s.command.request_loot(id);
    tracing::info!("loot: queued corpse {:?} (spawn_id={})", name, id);
    (StatusCode::OK, format!("looting {} (spawn_id={})", clean_entity_name(&name), id))
}

/// POST /v1/interact/loot — open a corpse and take all its items, reusing the auto-loot machinery
/// (OP_LootRequest → echo each OP_LootItem → OP_EndLootRequest). Must be near the corpse; looted
/// items land in inventory (see GET /v1/observe/inventory). Body: {"id":N} for a specific corpse
/// spawn id, {"name":"..."} to fuzzy-match a corpse name, or {} for the nearest corpse.
///
/// Every path (id / name / nearest) is restricted to entities whose key names a corpse — eqoxide#346
/// found that the explicit `id`/`name` paths had NO such check, so an unknown id defaulted to
/// `format!("spawn {}", id)` and a 200, and a name like "rat" could match a live `a_rat01` standing
/// next to `a_rat00's corpse`. A nonexistent id or a name matching no corpse is 404; a name matching
/// more than one corpse is 409 (ambiguous) rather than silently picking one.
async fn post_loot(
    State(s): State<HttpState>,
    OptionalJson(body): OptionalJson<LootBody>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let b = body.unwrap_or_default();
    if let Some(id) = b.id {
        let ids = s.world.entity_ids.lock().unwrap();
        let found = ids.iter().find(|(_, &v)| v == id).map(|(k, _)| k.clone());
        drop(ids);
        return match found {
            Some(key) if is_corpse_key(&key) => queue_loot(&s, key, id),
            Some(key) => (StatusCode::NOT_FOUND,
                format!("spawn_id {} is not a corpse ({})", id, clean_entity_name(&key))),
            None => (StatusCode::NOT_FOUND, format!("no spawn with id {}", id)),
        };
    }
    if let Some(name) = &b.name {
        let ids = s.world.entity_ids.lock().unwrap();
        let nl = name.to_lowercase();
        let matches: Vec<(String, u32)> = ids.iter()
            .filter(|(k, _)| is_corpse_key(k)
                && (k.to_lowercase().contains(&nl) || clean_entity_name(k).to_lowercase().contains(&nl)))
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        drop(ids);
        return match matches.len() {
            0 => (StatusCode::NOT_FOUND, format!("no corpse matching {:?}", name)),
            1 => { let (key, id) = matches[0].clone(); queue_loot(&s, key, id) }
            n => (StatusCode::CONFLICT,
                format!("ambiguous corpse name {:?} matches {} corpses — use {{\"id\":N}}", name, n)),
        };
    }
    // Nearest corpse to the player (camera focus = player pos).
    let focus = s.camera.snapshot.lock().unwrap().focus;
    let positions = s.world.entity_positions.lock().unwrap();
    let ids = s.world.entity_ids.lock().unwrap();
    let resolved = positions.iter()
        .filter(|(k, _)| is_corpse_key(k))
        .map(|(k, &(x, y, _))| {
            let (dx, dy) = (x - focus[0], y - focus[1]);
            (k.clone(), dx * dx + dy * dy)
        })
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .and_then(|(k, _)| ids.get(&k).map(|&id| (k, id)));
    drop(positions);
    drop(ids);
    match resolved {
        Some((name, id)) => queue_loot(&s, name, id),
        None => (StatusCode::NOT_FOUND, "no corpse found to loot".into()),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GiveBody {
    /// NPC name to hand the item to (fuzzy-matched, like /merchant/buy and /combat/target/name).
    npc: String,
    /// Inventory slot holding the item to give (e.g. 23 for a general/bag slot, or 30 if it's
    /// already on the cursor).
    from: u32,
}

/// POST /v1/interact/give {"npc":"<name>","from":N} — hand inventory item in slot N to the named NPC
/// and complete an EQ quest turn-in (trade-window flow). Must be within trade range. The nav thread
/// runs a multi-tick state machine: it puts the item on the cursor + sends OP_TradeRequest, waits
/// for OP_TradeRequestAck, then moves the item into the NPC trade slot + sends OP_TradeAcceptClick.
///
/// A3 Migration 2 (#448) — Command-with-result: this no longer returns a premature "queued" 200. It
/// AWAITS the real outcome (up to 8s) and reports it honestly:
///   • 200 — the turn-in was CONFIRMED: OP_FinishTrade arrived AND the item actually LEFT inventory
///     (verify-transfer, #486). Body: `{status:"given", item, npc_id}`.
///   • 409 — REFUSED before sending: a give was already in flight (singleton-in-flight). Body:
///     `{status:"refused", reason}`. No second trade was started.
///   • 202 — the outcome is UNKNOWN or the item did NOT transfer. This covers the no-ack abort, the
///     ITEM-MISMATCH case (item returned on the cursor with NO OP_FinishTrade), a lost reply, a zone
///     change mid-give, AND — the #486 fix — a give where OP_FinishTrade DID arrive but the NPC
///     REJECTED / was OUT OF RANGE, returning the item to the player (OP_FinishTrade only ends the
///     trade SESSION; it does NOT prove acceptance). The body says so explicitly. A 202 MUST NOT be
///     read as success — that is the whole honesty invariant of A3 (see `crate::command_state::result`).
async fn post_give(
    State(s): State<HttpState>,
    body: Result<Json<GiveBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err((code, msg)) = require_live_session(&s) { return text(code, msg); }
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return text(StatusCode::BAD_REQUEST, "provide {\"npc\":\"...\",\"from\":N}"),
    };
    // Resolve the NPC, then DROP the entity map lock before awaiting — never hold a std Mutex across
    // an `.await`.
    let found = {
        let ids = s.world.entity_ids.lock().unwrap();
        let nl = b.npc.to_lowercase();
        ids.iter()
            .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl))
            .map(|(k, &id)| (k.clone(), id))
    };
    let (key, id) = match found {
        Some(hit) => hit,
        None => return text(StatusCode::NOT_FOUND, format!("no NPC matching {:?}", b.npc)),
    };

    // Park the give with a result channel and await the TRUE outcome (park → fulfil → timeout).
    let (tx, rx) = oneshot::channel::<CommandResult<GiveOk>>();
    s.command.request_give_await(id, b.from, tx);
    tracing::info!("give: awaited give queued — npc {:?} (spawn_id={}) from_slot={}", key, id, b.from);

    match tokio::time::timeout(Duration::from_secs(GIVE_HTTP_TIMEOUT_SECS), rx).await {
        // A REAL OP_FinishTrade landed — the NPC accepted the item.
        Ok(Ok(CommandResult::Resolved(GiveOk { npc_id, item_name }))) => json(
            StatusCode::OK,
            serde_json::json!({
                "status": "given",
                "item": item_name,
                "npc_id": npc_id,
            }),
        ),
        // A pre-send rejection: another give was already in flight (singleton-in-flight).
        Ok(Ok(CommandResult::Refused(reason))) => json(
            StatusCode::CONFLICT,
            serde_json::json!({ "status": "refused", "reason": reason }),
        ),
        // Unconfirmed, channel closed (Sender dropped — disconnect), or elapsed: the outcome is
        // genuinely UNKNOWN. The no-ack abort and the ITEM-MISMATCH case (item returned on the cursor,
        // no OP_FinishTrade) both land here. MUST NOT read as success — 202 with an explicit body.
        _ => json(
            StatusCode::ACCEPTED,
            serde_json::json!({
                "status": "unconfirmed",
                "message": "give sent to the NPC, but the outcome is UNKNOWN — no OP_FinishTrade \
                            confirmation arrived. The NPC may not have accepted the item (a quest \
                            turn-in the item doesn't match returns it to you with no confirmation), \
                            the trade may have timed out, or the reply was lost. Re-check GET \
                            /v1/observe/inventory for the item before assuming it succeeded.",
                "npc": clean_entity_name(&key),
                "from": b.from,
            }),
        ),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DoorClickBody { door_id: Option<u8>, name: Option<String> }

/// POST /v1/interact/click_door {"door_id": N}  or  {"name": "DOOR1"} (case-insensitive name match).
async fn post_door_click(
    State(s): State<HttpState>,
    body: axum::extract::Json<DoorClickBody>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let id = if let Some(id) = body.door_id {
        Some(id)
    } else if let Some(name) = &body.name {
        let up = name.to_uppercase();
        s.interact.doors_shared.lock().unwrap().iter()
            .find(|d| d.name.to_uppercase() == up)
            .map(|d| d.door_id)
    } else {
        None
    };
    match id {
        Some(id) => {
            s.command.request_door_click(id);
            (StatusCode::OK, format!("clicking door {}", id))
        }
        None => (StatusCode::BAD_REQUEST,
                 "provide {\"door_id\":N} or {\"name\":\"...\"}".into()),
    }
}

/// POST /v1/interact/sit — sit down (mana/HP regen).
async fn post_sit(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    s.command.request_sit(true);
    (StatusCode::OK, "sit queued".into())
}

/// POST /v1/interact/stand — stand up.
async fn post_stand(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    s.command.request_sit(false);
    (StatusCode::OK, "stand queued".into())
}

#[cfg(test)]
mod tests {
    use super::router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use crate::http::quests::tests::empty_state;

    fn seed_npc(state: &crate::http::HttpState, key: &str, id: u32, pos: (f32, f32, f32)) {
        state.world.entity_positions.lock().unwrap().insert(key.to_string(), pos);
        state.world.entity_ids.lock().unwrap().insert(key.to_string(), id);
    }

    // --- hail: a malformed name must not silently fall back to "nearest NPC" -------------------

    #[tokio::test]
    async fn hail_no_body_hails_nearest_npc() {
        let state = empty_state();
        seed_npc(&state, "Guard_Phaeton000", 5, (1.0, 1.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/hail").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(command.take_hail().is_some());
    }

    #[tokio::test]
    async fn hail_malformed_name_is_400_and_does_not_hail_nearest() {
        let state = empty_state();
        seed_npc(&state, "Guard_Phaeton000", 5, (1.0, 1.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/hail")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":5}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(command.take_hail().is_none(),
            "a malformed name must not silently fall through to hailing the nearest NPC");
    }

    /// eqoxide#341: a typo'd key ("nmae" instead of "name") must 400 — not be silently ignored by
    /// serde (leaving `name` at its default `None`) and fall through to hailing the nearest NPC.
    #[tokio::test]
    async fn hail_unknown_key_is_400_and_does_not_hail_nearest() {
        let state = empty_state();
        seed_npc(&state, "Guard_Phaeton000", 5, (1.0, 1.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/hail")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"nmae":"Guard"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(command.take_hail().is_none(),
            "a typo'd key must not silently fall through to hailing the nearest NPC");
    }

    // --- loot: a malformed id must not silently fall back to "nearest corpse" ------------------

    #[tokio::test]
    async fn loot_no_body_loots_nearest_corpse() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/loot").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_loot(), Some(9));
    }

    #[tokio::test]
    async fn loot_malformed_id_is_400_and_does_not_loot_nearest() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":"not-a-number"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(command.take_loot().is_none(),
            "a malformed id must not silently fall through to looting the nearest corpse");
    }

    /// eqoxide#341: a typo'd key ("idd" instead of "id") must 400 — not be silently ignored by serde
    /// (leaving `id` at its default `None`) and fall through to looting the nearest corpse.
    #[tokio::test]
    async fn loot_unknown_key_is_400_and_does_not_loot_nearest() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"idd":9}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(command.take_loot().is_none(),
            "a typo'd key must not silently fall through to looting the nearest corpse");
    }

    // --- loot: eqoxide#346 — every path must be restricted to an actual corpse -----------------
    //
    // Baseline on `main` before this fix: {"id":999999} (no such spawn) returned 200
    // "looting spawn 999999", and {"name":"<a live mob>"} happily queued that live mob for
    // looting because the id/name paths never checked `.contains("corpse")` (only the
    // zero-body "nearest corpse" path did).

    #[tokio::test]
    async fn loot_nonexistent_id_is_404_not_200() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":999999}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(command.take_loot().is_none());
    }

    #[tokio::test]
    async fn loot_live_mob_id_is_404_not_a_corpse() {
        let state = empty_state();
        // A live mob (non-corpse key) standing near a corpse.
        seed_npc(&state, "a_rat01", 11, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":11}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND,
            "an id that resolves to a live mob (not a corpse) must never be queued for looting");
        assert!(command.take_loot().is_none());
    }

    #[tokio::test]
    async fn loot_live_mob_name_is_404_not_a_corpse() {
        let state = empty_state();
        seed_npc(&state, "a_rat01", 11, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"a_rat01"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND,
            "a name that only matches a live mob (not a corpse) must never be queued for looting");
        assert!(command.take_loot().is_none());
    }

    #[tokio::test]
    async fn loot_ambiguous_name_is_409_not_a_silent_pick() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        seed_npc(&state, "a_rat001's corpse", 10, (3.0, 3.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"rat"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT,
            "a name matching multiple corpses must be reported as ambiguous, not silently resolved");
        assert!(command.take_loot().is_none());
    }

    #[tokio::test]
    async fn loot_id_matching_a_corpse_still_works() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":9}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_loot(), Some(9));
    }

    #[tokio::test]
    async fn loot_unambiguous_name_matching_a_corpse_still_works() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"a_rat000"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_loot(), Some(9));
    }

    // ── A3 Migration 2 (#448): POST /v1/interact/give reports the TRUE outcome, not a queued 200 ──

    use crate::command_state::{CommandResult, GiveOk};

    /// A give to a nonexistent NPC 404s before parking anything.
    #[tokio::test]
    async fn give_unknown_npc_is_404() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/give").header("content-type", "application/json")
            .body(Body::from(r#"{"npc":"Nobody","from":23}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(command.take_give_await().is_none(), "a 404 must not park a give");
    }

    /// SUCCESS: the server confirms the turn-in (OP_FinishTrade, delivered here as `Resolved`) → 200
    /// with the honest receipt body (item/npc_id).
    #[tokio::test]
    async fn give_confirmed_is_200_with_the_receipt() {
        let state = empty_state();
        seed_npc(&state, "Priest_of_Mischief000", 11, (1.0, 1.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let task = tokio::spawn(async move {
            app.oneshot(Request::post("/give").header("content-type", "application/json")
                .body(Body::from(r#"{"npc":"Mischief","from":23}"#)).unwrap()).await.unwrap()
        });
        // Wait for the handler to park its Sender, then deliver the confirmed receipt.
        let (npc_id, from_slot, tx) = loop {
            if let Some(p) = command.take_give_await() { break p; }
            tokio::task::yield_now().await;
        };
        assert_eq!((npc_id, from_slot), (11, 23));
        tx.send(CommandResult::Resolved(GiveOk { npc_id: 11, item_name: "Bone Chips".into() })).unwrap();

        let resp = task.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "given");
        assert_eq!(v["item"], "Bone Chips");
        assert_eq!(v["npc_id"], 11);
    }

    /// REFUSAL: a give already in flight (singleton-in-flight, delivered as `Refused`) → 409.
    #[tokio::test]
    async fn give_refused_is_409() {
        let state = empty_state();
        seed_npc(&state, "Priest_of_Mischief000", 11, (1.0, 1.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let task = tokio::spawn(async move {
            app.oneshot(Request::post("/give").header("content-type", "application/json")
                .body(Body::from(r#"{"npc":"Mischief","from":23}"#)).unwrap()).await.unwrap()
        });
        let (_n, _s, tx) = loop {
            if let Some(p) = command.take_give_await() { break p; }
            tokio::task::yield_now().await;
        };
        tx.send(CommandResult::Refused("a give is already in flight; retry".into())).unwrap();

        let resp = task.await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "refused");
    }

    /// NO-CONFIRMATION SILENCE — THE HONESTY PROOF at the HTTP boundary. Nothing ever fires the parked
    /// Sender (the net-side `tick_give` verdict is exercised in the action_loop tests); here the 8s
    /// timeout elapses in virtual time (`start_paused`) → **202**, NOT 200. The body must say the
    /// outcome is UNKNOWN. The Sender is HELD (not dropped) across the wait, so this exercises the
    /// genuine ELAPSED branch, not a channel-closed shortcut.
    #[tokio::test(start_paused = true)]
    async fn give_with_no_confirmation_is_202_unknown_never_success() {
        let state = empty_state();
        seed_npc(&state, "Priest_of_Mischief000", 11, (1.0, 1.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let task = tokio::spawn(async move {
            app.oneshot(Request::post("/give").header("content-type", "application/json")
                .body(Body::from(r#"{"npc":"Mischief","from":23}"#)).unwrap()).await.unwrap()
        });
        // Take the parked Sender and HOLD it — the server's silence, faithfully modelled.
        let held = loop {
            if let Some(p) = command.take_give_await() { break p; }
            tokio::task::yield_now().await;
        };

        let resp = task.await.unwrap(); // 8s timeout elapses in virtual time
        assert_ne!(resp.status(), StatusCode::OK,
            "a give with no OP_FinishTrade MUST NOT be reported as success — the A3 invariant");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "unconfirmed");
        let msg = v["message"].as_str().unwrap();
        assert!(msg.contains("UNKNOWN"), "the body must state the outcome is unknown");
        drop(held);
    }
}
