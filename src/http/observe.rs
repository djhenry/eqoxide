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
        .route("/spells", get(get_spells))
        .route("/doors", get(get_doors))
        .route("/zone_points", get(get_zone_points))
        .route("/quests", get(get_quests))
        .route("/quests/log", get(get_quest_log))
}

async fn get_debug(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let cam   = s.snapshot.lock().unwrap().clone();
    let player = s.player_info.lock().unwrap().clone();
    Json(serde_json::json!({
        "player": {
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
            "target_id":   player.target_id,
            "target_name": player.target_name,
            "target_hp_pct": player.target_hp_pct,
            "connected":   player.connected,
            "last_packet_age_ms": player.last_packet_age_ms,
        },
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

/// GET /v1/observe/doors — list the current zone's doors (id, name, position, opentype, open state).
async fn get_doors(State(s): State<HttpState>) -> Json<Vec<DoorView>> {
    Json(s.doors_shared.lock().unwrap().clone())
}

/// GET /v1/observe/zone_points — returns all zone exit points received from the server.
async fn get_zone_points(State(s): State<HttpState>) -> Json<Vec<crate::game_state::ZonePoint>> {
    Json(s.zone_points.lock().unwrap().clone())
}

/// GET /v1/observe/quests — the agent's "quests near me" view for the current zone. Lists quest
/// givers (data/quests.json) with location, distance, whether they're loaded (in spawn range), what
/// they want (turn-in items), and reward XP. Combine with /observe/entities + /navigate/goto to walk
/// to a giver and /interact/give to hand in. See docs/autonomous-play.md.
async fn get_quests(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let player = s.player_info.lock().unwrap().clone();
    let zone = player.zone.clone();
    let (px, py) = (player.pos_east, player.pos_north);
    // clean live names -> position, to flag loaded givers + use their live coords
    let live: HashMap<String, (f32, f32, f32)> = s.entity_positions.lock().unwrap().iter()
        .map(|(k, v)| (clean_entity_name(k), *v))
        .collect();
    let mut givers: Vec<serde_json::Value> = crate::quests::givers_in(&zone).into_iter()
        .map(|(name, g)| {
            let live_pos = live.get(&name).copied();
            let pos = live_pos.map(|(x, y, z)| [x, y, z]).unwrap_or([g.x, g.y, g.z]);
            let dist = ((pos[0] - px).powi(2) + (pos[1] - py).powi(2)).sqrt();
            serde_json::json!({
                "name": name,
                "npc_id": g.npc_id,
                "pos": pos,
                "loaded": live_pos.is_some(),
                "distance": dist.round(),
                "turn_in": g.turn_in,
                "wanted": g.wanted,
                "reward_xp": g.reward_xp,
                "hail": g.hail,
            })
        })
        .collect();
    givers.sort_by(|a, b| {
        let (da, db) = (a["distance"].as_f64().unwrap_or(1e9), b["distance"].as_f64().unwrap_or(1e9));
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });
    Json(serde_json::json!({ "zone": zone, "player": [px, py], "count": givers.len(), "quest_givers": givers }))
}

/// GET /v1/observe/quests/log — the player's NATIVE quest journal (EQ Task system), pushed by the
/// server via OP_TaskDescription/OP_TaskActivity. Each task has a title, description, reward, and
/// objectives with live progress (done_count/goal_count). Distinct from /observe/quests (old-style
/// Lua turn-in quests) — together they cover both kinds of EQ quests.
async fn get_quest_log(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let tasks = s.task_log.lock().unwrap().clone();
    Json(serde_json::json!({ "active_count": tasks.len(), "tasks": tasks }))
}
