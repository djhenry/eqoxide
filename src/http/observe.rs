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
        .route("/zone_entrances", get(get_zone_entrances))
        // Deprecated alias for /zone_entrances (its content was always the entrance/arrival list).
        .route("/zone_points", get(get_zone_entrances))
        .route("/zone_exits", get(get_zone_exits))
        .route("/item_text", get(get_item_text))
        .route("/who", get(get_who))
}

/// GET /v1/observe/item_text — the text of the most recently read book/note (from
/// POST /v1/interact/read). Returns `{"text": "..."}` once a book has been read this session, or
/// `{"text": null}` if none has. Newlines are decoded from RoF2's backtick marker. (#288)
async fn get_item_text(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let text = s.player_info.lock().unwrap().book_text.clone();
    Json(serde_json::json!({ "text": text }))
}

async fn get_debug(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let cam   = s.snapshot.lock().unwrap().clone();
    let player = s.player_info.lock().unwrap().clone();
    let nav_state = s.nav_state.lock().unwrap().clone();
    // Is nav running in a KNOWN-DEGRADED mode in this zone? (#229/#329)
    //
    // A floor is an up-facing triangle, which is only meaningful if the zone's collision mesh is
    // consistently wound. That is validated at zone load; if a zone ever FAILS the check, the
    // floor-normal filter is switched off for it and nav reverts to the old facing-blind behaviour
    // — in which a CEILING can be selected as the floor to stand on (the exact #329 bug: A* planned
    // routes across qcat's ceiling plane). That fallback is deliberate — deleting every real floor
    // in a mis-wound zone would be worse — but it MUST NOT be silent. A `tracing::warn!` is not
    // observable to an agent driving this client over HTTP, and a client that quietly answers from a
    // known-broken code path is lying by omission. So report it here, where the agent can see it.
    //
    // `null` = healthy. Every zone shipped today clears the validation bar by >=6 points, so this is
    // a latent guard rather than a live condition.
    let nav_degraded = s.shared_collision.read().unwrap().as_ref().and_then(|col| {
        (!col.floor_normals_ok()).then(|| serde_json::json!({
            "reason": "floor_normals_unvalidated",
            "detail": "this zone's collision mesh failed the winding check, so nav cannot tell a \
                       floor from a ceiling; routes may be planned across ceilings and be unwalkable \
                       (#329). Pathing in this zone is UNRELIABLE.",
        }))
    });
    let (guild_name, guild_id, guild_rank) = {
        let g = s.guild.lock().unwrap();
        (g.guild_name.clone(), g.guild_id, g.guild_rank)
    };
    Json(serde_json::json!({
        "player": {
            "name":       player.name,
            "zone":       player.zone,
            // Guild identity (#295): empty name / id 0 = not in a guild.
            "guild":      guild_name,
            "guild_id":   guild_id,
            "guild_rank": guild_rank,
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
            "nav_state":   nav_state,
        },
        // Nav health for THIS zone. `null` when nav is running normally; an object naming the
        // degraded mode when it is not (see `nav_degraded` above). An agent must be able to tell
        // "no route exists" from "this zone's pathing is known-unreliable".
        "nav_degraded": nav_degraded,
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

    // 10s: a debug build's readback + 1024px PNG encode can exceed 2s when the
    // render loop is saturated, which made captures 503 while frames were fine.
    match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
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

/// One enriched player row for GET /v1/observe/who.
#[derive(serde::Serialize)]
struct WhoView {
    name:  String,
    level: u32,
    /// Class name (e.g. "Wizard"), empty when the player is anonymous.
    class: String,
    /// Race code (e.g. "HUM"), empty when the player is anonymous.
    race:  String,
    /// Numeric zone id the player is in (0 when anonymous).
    zone_id: u32,
    /// Guild name, empty if none.
    guild: String,
    anon:  bool,
}

/// GET /v1/observe/who — server-wide `/who all` roster of everyone currently online. Triggers an
/// OP_WhoAllRequest and awaits the OP_WhoAllResponse (so an agent can see which fellow agents/players
/// are online before coordinating). Returns `{online: [{name, level, class, race, zone_id, guild,
/// anon}]}`. 503 if no response arrives in time. (#300)
async fn get_who(State(s): State<HttpState>) -> Response {
    let (tx, rx) = oneshot::channel::<Vec<crate::game_state::WhoEntry>>();
    *s.who_req.lock().unwrap() = Some(tx);
    match tokio::time::timeout(std::time::Duration::from_secs(6), rx).await {
        Ok(Ok(roster)) => {
            let online: Vec<WhoView> = roster.into_iter().map(|e| WhoView {
                class:   if e.anon { String::new() } else { crate::eq_net::packet_handler::class_name(e.class).to_string() },
                race:    if e.anon { String::new() } else { crate::eq_net::protocol::eq_race_to_code(e.race).to_string() },
                name: e.name, level: e.level, zone_id: e.zone_id, guild: e.guild, anon: e.anon,
            }).collect();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::json!({ "online": online }).to_string()))
                .unwrap()
        }
        _ => Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("no /who response (not connected, or server did not reply in time)"))
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

/// GET /v1/observe/zone_entrances — the zone **entrances** advertised by the server
/// (`OP_SendZonepoints`): where you *arrive* (in the destination zone's coordinate space) and your
/// heading when you cross into a zone, keyed by destination `zone_id` + `iterator`. This is NOT
/// where you go to *leave* the current zone — for that, see `/zone_exits`. (Also served at the
/// deprecated alias `/zone_points`.)
async fn get_zone_entrances(State(s): State<HttpState>) -> Json<Vec<crate::game_state::ZonePoint>> {
    Json(s.zone_points.lock().unwrap().clone())
}

/// GET /v1/observe/zone_exits — the current zone's **exits**: the WLD zone-line regions you navigate
/// *toward* to leave, in the current zone's coordinate space. Each exit is the same region
/// `/v1/move/zone_cross` walks to. Per exit: `location` `[x,y,z]` (a point inside the region nearest
/// the player — position-relative), `zone_id` (destination, or `null` if the WLD region's index
/// isn't advertised in the entrance list), and `index` (the link to the matching entrance's
/// `iterator`). Advertised entrances with no WLD region are omitted. Empty when the zone has no
/// region map (no `.wtr` / v1 map) or no collision is loaded yet.
async fn get_zone_exits(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let player = s.player_info.lock().unwrap().clone();
    let pos = [player.pos_east, player.pos_north, player.pos_up];
    // index -> destination zone_id, from the advertised entrance list.
    let dest_of: std::collections::HashMap<i32, u16> = s
        .zone_points
        .lock()
        .unwrap()
        .iter()
        .map(|zp| (zp.iterator as i32, zp.zone_id))
        .collect();
    let mut exits = Vec::new();
    if let Some(col) = s.shared_collision.read().unwrap().as_ref() {
        for index in col.zone_line_indices() {
            let location = col
                .find_zone_line_near(Some(index), pos)
                .map(|(_, p)| serde_json::json!([p[0], p[1], p[2]]));
            exits.push(serde_json::json!({
                "index": index,
                "zone_id": dest_of.get(&index),
                "location": location,
            }));
        }
    }
    Json(serde_json::json!(exits))
}
