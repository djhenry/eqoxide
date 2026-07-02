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
}

/// POST /v1/interact/dialogue — click one of the NPC-dialogue choices from GET
/// /v1/observe/dialogue. Body is either `{"index": N}` (position in the choices list) or
/// `{"text": "..."}` (matched case-insensitively against a choice's label). Sends an
/// OP_ItemLinkClick so the server resolves the saylink and treats it as our reply to the NPC. (#120)
#[derive(serde::Deserialize)]
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
struct HailBody {
    /// NPC to hail (fuzzy-matched against /observe/entities). Omit to hail the nearest NPC.
    name: Option<String>,
}

/// POST /v1/interact/hail — say "Hail, <name>" so a nearby NPC fires its hail/quest script.
/// Body: {"name":"Guard Phaeton"} (fuzzy) or {} to hail the nearest NPC.
/// The NPC must be within ~200 units (server-enforced say range).
async fn post_hail(
    State(s): State<HttpState>,
    body: Option<Json<HailBody>>,
) -> (StatusCode, String) {
    let requested = body.and_then(|Json(b)| b.name);
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
struct LootBody {
    /// Corpse spawn id to loot directly.
    id:   Option<u32>,
    /// Corpse name to fuzzy-match (corpses are named like "a_rat000's corpse").
    name: Option<String>,
}

/// POST /v1/interact/loot — open a corpse and take all its items, reusing the auto-loot machinery
/// (OP_LootRequest → echo each OP_LootItem → OP_EndLootRequest). Must be near the corpse; looted
/// items land in inventory (see GET /v1/observe/inventory). Body: {"id":N} for a specific corpse
/// spawn id, {"name":"..."} to fuzzy-match a corpse name, or {} for the nearest corpse.
async fn post_loot(
    State(s): State<HttpState>,
    body: Option<Json<LootBody>>,
) -> (StatusCode, String) {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    // Resolve to a corpse spawn id: explicit id > fuzzy name > nearest corpse.
    let resolved: Option<(String, u32)> = if let Some(id) = b.id {
        let name = s.entity_ids.lock().unwrap().iter()
            .find(|(_, &v)| v == id).map(|(k, _)| k.clone())
            .unwrap_or_else(|| format!("spawn {}", id));
        Some((name, id))
    } else if let Some(name) = &b.name {
        let ids = s.entity_ids.lock().unwrap();
        let nl = name.to_lowercase();
        ids.iter()
            .find(|(k, _)| k.to_lowercase().contains(&nl) || clean_entity_name(k).to_lowercase().contains(&nl))
            .map(|(k, &id)| (k.clone(), id))
    } else {
        // Nearest corpse to the player (camera focus = player pos).
        let focus = s.snapshot.lock().unwrap().focus;
        let positions = s.entity_positions.lock().unwrap();
        let ids = s.entity_ids.lock().unwrap();
        positions.iter()
            .filter(|(k, _)| k.to_lowercase().contains("corpse"))
            .map(|(k, &(x, y, _))| {
                let (dx, dy) = (x - focus[0], y - focus[1]);
                (k.clone(), dx * dx + dy * dy)
            })
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .and_then(|(k, _)| ids.get(&k).map(|&id| (k, id)))
    };
    match resolved {
        Some((name, id)) => {
            *s.loot.lock().unwrap() = Some(id);
            tracing::info!("loot: queued corpse {:?} (spawn_id={})", name, id);
            (StatusCode::OK, format!("looting {} (spawn_id={})", clean_entity_name(&name), id))
        }
        None => (StatusCode::NOT_FOUND, "no corpse found to loot".into()),
    }
}

#[derive(serde::Deserialize)]
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
