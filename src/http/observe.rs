//! `/v1/observe/*` — read-only world/player state for the agent.

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, StatusCode},
    response::Response,
    routing::get,
    Json, Router,
};
use std::collections::HashMap;
use tokio::sync::oneshot;
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/debug", get(get_debug))
        .route("/frame", get(get_frame))
        .route("/entities", get(get_entities))
        .route("/inventory", get(get_inventory))
        .route("/messages", get(get_messages))
        .route("/dialogue", get(get_dialogue))
        .route("/spells", get(get_spells))
        .route("/skills", get(get_skills))
        .route("/doors", get(get_doors))
        .route("/zone_points", get(get_zone_points))
}

async fn get_debug(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let cam   = s.snapshot.lock().unwrap().clone();
    let player = s.player_info.lock().unwrap().clone();
    Json(serde_json::json!({
        "player": {
            "name":       player.name,
            "zone":       player.zone,
            "race":       player.race,
            "class":      player.class,
            "level":      player.level,
            "pos":        [player.pos_east, player.pos_north, player.pos_up],
            "heading_ccw": player.heading_ccw,
            "heading_cw":  player.heading_cw,
            "server_corrections": player.server_corrections,
            "currency":    currency_json(player.coin),
            "hp_pct":      player.hp_pct,
            "hp":          player.cur_hp,
            "hp_max":      player.max_hp,
            "mana_pct":    player.mana_pct,
            "mana":        player.cur_mana,
            "mana_max":    player.max_mana,
            "xp_pct":      player.xp_pct,
            "spawn_id":    player.player_id,
            "target_id":   player.target_id,
            "target_name": player.target_name,
            "target_hp_pct": player.target_hp_pct,
            "connected":   player.connected,
            "last_packet_age_ms": player.last_packet_age_ms,
        },
        // Per-phase frame timings (ms, EMA-smoothed); all zero unless --profile / EQ_PROFILE=1.
        "frame_profile": player.frame_profile,
        "camera": {
            "azimuth_deg":   cam.azimuth.to_degrees(),
            "elevation_deg": cam.elevation.to_degrees(),
            "radius":        cam.radius,
            "focus":         cam.focus,
            "mode":          cam.mode,
        },
    }))
}

/// GET /v1/observe/frame — returns the current rendered frame as a PNG.
async fn get_frame(State(s): State<HttpState>) -> Response {
    let (tx, rx) = oneshot::channel::<Vec<u8>>();
    *s.frame_req.lock().unwrap() = Some(tx);

    match tokio::time::timeout(std::time::Duration::from_secs(2), rx).await {
        Ok(Ok(png_bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/png")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(png_bytes))
            .unwrap(),
        _ => Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("renderer not ready"))
            .unwrap(),
    }
}

/// GET /v1/observe/entities — returns {name: [x,y,z], ...} for all known entities.
async fn get_entities(State(s): State<HttpState>) -> Json<HashMap<String, [f32; 3]>> {
    let positions = s.entity_positions.lock().unwrap();
    let out: HashMap<_, _> = positions.iter()
        .map(|(k, &(x, y, z))| (k.clone(), [x, y, z]))
        .collect();
    Json(out)
}

/// GET /v1/observe/inventory — the player's current inventory + equipment, published each tick by
/// the nav thread. Each item carries its Titanium **wire** slot (the number to pass to /interact/give
/// and /inventory/move — note general slots are one less than the EQEmu DB `inventory.slot_id`: DB
/// 23-30 → wire 22-29), plus item_id, name, charges, icon, and idfile. Use this to discover which
/// slot holds an item before giving/equipping it.
async fn get_inventory(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let items = s.inventory.lock().unwrap().clone();
    let coin  = s.player_info.lock().unwrap().coin;
    Json(serde_json::json!({
        "count": items.len(),
        "items": items,
        "currency": currency_json(coin),
    }))
}

#[derive(serde::Deserialize)]
struct MessagesQuery {
    /// Filter to a single message channel, e.g. ?kind=npc for NPC dialogue only.
    kind: Option<String>,
}

/// GET /v1/observe/messages — the in-game message log as machine-readable text (oldest→newest, last
/// ~50 lines), published each tick by the nav thread. This is how an agent reads **NPC dialogue**:
/// each line has a `kind` ("npc" = NPC say/emote, plus "chat", "combat", "system", "exp", "loot",
/// "trade", "zone"), the `text`, and any `[bracketed]` quest `keywords` to say back via POST
/// /v1/interact/say. Filter with `?kind=npc` for dialogue only.
async fn get_messages(
    State(s): State<HttpState>,
    Query(q): Query<MessagesQuery>,
) -> Json<serde_json::Value> {
    let all = s.messages.lock().unwrap();
    let filtered: Vec<&MessageEntry> = match &q.kind {
        Some(k) => all.iter().filter(|m| m.kind == *k).collect(),
        None    => all.iter().collect(),
    };
    Json(serde_json::json!({ "count": filtered.len(), "messages": filtered }))
}

/// GET /v1/observe/dialogue — the current clickable NPC-dialogue choices (saylinks from the most
/// recent NPC message, e.g. a Soulbinder's "[bind your soul]"). `index` is the argument POSTed to
/// /v1/interact/dialogue to click that choice. Empty when no NPC has offered choices. (#120)
async fn get_dialogue(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let choices = s.dialogue.lock().unwrap();
    let list: Vec<_> = choices.iter().enumerate()
        .map(|(i, c)| serde_json::json!({ "index": i, "text": c.text }))
        .collect();
    Json(serde_json::json!({ "count": list.len(), "choices": list }))
}

/// GET /v1/observe/spells — the 9 memorized gems with names. Empty gem = spell id 0 or 0xFFFFFFFF.
async fn get_spells(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let mem = s.player_info.lock().unwrap().mem_spells;
    let gems: Vec<_> = mem.iter().enumerate().map(|(i, &id)| {
        if id == 0 || id == 0xFFFF_FFFF {
            serde_json::json!({ "gem": i, "spell_id": null, "name": null })
        } else {
            let name = s.spells.get(id).map(|x| x.name.clone());
            serde_json::json!({ "gem": i, "spell_id": id, "name": name })
        }
    }).collect();
    Json(serde_json::json!({ "gems": gems }))
}

/// GET /v1/observe/skills — the player's skills with current values (eqoxide#99). `value == 0`
/// means untrained. Ids/names are the RoF2 skill enum (`crate::skills`); an agent uses this to
/// decide what to train at a guildmaster and to notice when a skill is capped.
async fn get_skills(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let skills = s.player_info.lock().unwrap().skills.clone();
    let list: Vec<_> = (0..crate::skills::NUM_SKILLS).map(|id| {
        let value = skills.get(id).copied().unwrap_or(0);
        serde_json::json!({ "id": id, "name": crate::skills::skill_name(id as u32), "value": value })
    }).collect();
    Json(serde_json::json!({ "skills": list }))
}

/// GET /v1/observe/doors — list the current zone's doors (id, name, position, opentype, open state).
async fn get_doors(State(s): State<HttpState>) -> Json<Vec<DoorView>> {
    Json(s.doors_shared.lock().unwrap().clone())
}

/// GET /v1/observe/zone_points — returns all zone exit points received from the server.
async fn get_zone_points(State(s): State<HttpState>) -> Json<Vec<crate::game_state::ZonePoint>> {
    Json(s.zone_points.lock().unwrap().clone())
}
