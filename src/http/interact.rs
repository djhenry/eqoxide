//! `/v1/interact/*` — NPC/world interaction: hail, say, loot, give (turn-in), doors, sit/stand.

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use super::*;

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
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"slot\":N}".into()),
    };
    // Validate against the last-published inventory so a bad slot fails fast with a clear message,
    // rather than being silently dropped by the nav thread.
    let readable = s.inventory.lock().unwrap().iter()
        .find(|i| i.slot == b.slot)
        .map(|i| !i.filename.is_empty());
    match readable {
        Some(true) => {
            *s.read_book.lock().unwrap() = Some(b.slot);
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
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"index\":N} or {\"text\":\"...\"}".into()),
    };
    let choices = s.dialogue.lock().unwrap().clone();
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
            *s.dialogue_click.lock().unwrap() = Some(c);
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
    let requested = body.and_then(|b| b.name);
    let positions = s.entity_positions.lock().unwrap();

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
        let focus = s.snapshot.lock().unwrap().focus;
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
            let spawn_id = s.entity_ids.lock().unwrap().get(&key).copied();
            *s.hail.lock().unwrap() = Some((display_name.clone(), spawn_id));
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
    let text = match body {
        Ok(Json(b)) => b.text,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"text\":\"...\"}".into()),
    };
    if text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "empty text".into());
    }
    *s.say.lock().unwrap() = Some(text.clone());
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
    *s.loot.lock().unwrap() = Some(id);
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
    let b = body.unwrap_or_default();
    if let Some(id) = b.id {
        let ids = s.entity_ids.lock().unwrap();
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
        let ids = s.entity_ids.lock().unwrap();
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
    let focus = s.snapshot.lock().unwrap().focus;
    let positions = s.entity_positions.lock().unwrap();
    let ids = s.entity_ids.lock().unwrap();
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
/// The server replies OP_FinishTrade on completion; if no ack arrives within ~3s the give is aborted.
async fn post_give(
    State(s): State<HttpState>,
    body: Result<Json<GiveBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"npc\":\"...\",\"from\":N}".into()),
    };
    let ids = s.entity_ids.lock().unwrap();
    let nl = b.npc.to_lowercase();
    let found = ids.iter()
        .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl))
        .map(|(k, &id)| (k.clone(), id));
    match found {
        Some((key, id)) => {
            *s.give.lock().unwrap() = Some((id, b.from));
            tracing::info!("give: queued npc {:?} (spawn_id={}) from_slot={}", key, id, b.from);
            (StatusCode::OK, format!("giving slot {} to {} (spawn_id={})", b.from, clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no NPC matching {:?}", b.npc)),
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
    let id = if let Some(id) = body.door_id {
        Some(id)
    } else if let Some(name) = &body.name {
        let up = name.to_uppercase();
        s.doors_shared.lock().unwrap().iter()
            .find(|d| d.name.to_uppercase() == up)
            .map(|d| d.door_id)
    } else {
        None
    };
    match id {
        Some(id) => {
            *s.door_click.lock().unwrap() = Some(id);
            (StatusCode::OK, format!("clicking door {}", id))
        }
        None => (StatusCode::BAD_REQUEST,
                 "provide {\"door_id\":N} or {\"name\":\"...\"}".into()),
    }
}

/// POST /v1/interact/sit — sit down (mana/HP regen).
async fn post_sit(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.sit.lock().unwrap() = Some(true);
    (StatusCode::OK, "sit queued".into())
}

/// POST /v1/interact/stand — stand up.
async fn post_stand(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.sit.lock().unwrap() = Some(false);
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
        state.entity_positions.lock().unwrap().insert(key.to_string(), pos);
        state.entity_ids.lock().unwrap().insert(key.to_string(), id);
    }

    // --- hail: a malformed name must not silently fall back to "nearest NPC" -------------------

    #[tokio::test]
    async fn hail_no_body_hails_nearest_npc() {
        let state = empty_state();
        seed_npc(&state, "Guard_Phaeton000", 5, (1.0, 1.0, 0.0));
        let hail = state.hail.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/hail").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(hail.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn hail_malformed_name_is_400_and_does_not_hail_nearest() {
        let state = empty_state();
        seed_npc(&state, "Guard_Phaeton000", 5, (1.0, 1.0, 0.0));
        let hail = state.hail.clone();
        let app = router().with_state(state);
        let req = Request::post("/hail")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":5}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(hail.lock().unwrap().is_none(),
            "a malformed name must not silently fall through to hailing the nearest NPC");
    }

    /// eqoxide#341: a typo'd key ("nmae" instead of "name") must 400 — not be silently ignored by
    /// serde (leaving `name` at its default `None`) and fall through to hailing the nearest NPC.
    #[tokio::test]
    async fn hail_unknown_key_is_400_and_does_not_hail_nearest() {
        let state = empty_state();
        seed_npc(&state, "Guard_Phaeton000", 5, (1.0, 1.0, 0.0));
        let hail = state.hail.clone();
        let app = router().with_state(state);
        let req = Request::post("/hail")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"nmae":"Guard"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(hail.lock().unwrap().is_none(),
            "a typo'd key must not silently fall through to hailing the nearest NPC");
    }

    // --- loot: a malformed id must not silently fall back to "nearest corpse" ------------------

    #[tokio::test]
    async fn loot_no_body_loots_nearest_corpse() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let loot = state.loot.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/loot").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*loot.lock().unwrap(), Some(9));
    }

    #[tokio::test]
    async fn loot_malformed_id_is_400_and_does_not_loot_nearest() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let loot = state.loot.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":"not-a-number"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(loot.lock().unwrap().is_none(),
            "a malformed id must not silently fall through to looting the nearest corpse");
    }

    /// eqoxide#341: a typo'd key ("idd" instead of "id") must 400 — not be silently ignored by serde
    /// (leaving `id` at its default `None`) and fall through to looting the nearest corpse.
    #[tokio::test]
    async fn loot_unknown_key_is_400_and_does_not_loot_nearest() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let loot = state.loot.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"idd":9}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(loot.lock().unwrap().is_none(),
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
        let loot = state.loot.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":999999}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(loot.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn loot_live_mob_id_is_404_not_a_corpse() {
        let state = empty_state();
        // A live mob (non-corpse key) standing near a corpse.
        seed_npc(&state, "a_rat01", 11, (2.0, 2.0, 0.0));
        let loot = state.loot.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":11}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND,
            "an id that resolves to a live mob (not a corpse) must never be queued for looting");
        assert!(loot.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn loot_live_mob_name_is_404_not_a_corpse() {
        let state = empty_state();
        seed_npc(&state, "a_rat01", 11, (2.0, 2.0, 0.0));
        let loot = state.loot.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"a_rat01"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND,
            "a name that only matches a live mob (not a corpse) must never be queued for looting");
        assert!(loot.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn loot_ambiguous_name_is_409_not_a_silent_pick() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        seed_npc(&state, "a_rat001's corpse", 10, (3.0, 3.0, 0.0));
        let loot = state.loot.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"rat"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT,
            "a name matching multiple corpses must be reported as ambiguous, not silently resolved");
        assert!(loot.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn loot_id_matching_a_corpse_still_works() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let loot = state.loot.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":9}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*loot.lock().unwrap(), Some(9));
    }

    #[tokio::test]
    async fn loot_unambiguous_name_matching_a_corpse_still_works() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let loot = state.loot.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"a_rat000"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*loot.lock().unwrap(), Some(9));
    }
}
