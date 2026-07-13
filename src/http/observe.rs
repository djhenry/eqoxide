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
    let text = s.player().book_text;
    Json(serde_json::json!({ "text": text }))
}

async fn get_debug(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let cam   = s.snapshot.lock().unwrap().clone();
    // Projected from the network thread's GameState, and freshness measured RIGHT NOW — not read
    // out of a struct some other loop published whenever it last felt like running (#343).
    let player = s.player();
    let health = s.health();
    let frame_profile = *s.frame_profile.lock().unwrap();
    let nav_state = s.nav_state.lock().unwrap().clone();
    // Is nav running in a KNOWN-DEGRADED mode in this zone? (#229/#329)
    //
    // A floor is an up-facing triangle. Some zones bake real, walkable ground from INVERTED
    // (down-facing) art — across the 34 cached zones the heaviest users are feerrott (667 firings),
    // everfrost (614) and poknowledge (454) — so the floor-normal filter would delete it. Nav's
    // safety valve admits the column's BOTTOM-MOST surface as ground when the filter leaves the
    // column with no floor at all (nothing lies beneath ground; a ceiling always has a floor under
    // it). That keeps the zone navigable, but the answer came from mis-wound art and the surface's
    // true facing is unverified — a degraded answer, not a wrong one, and the agent must be able to
    // SEE that rather than be quietly handed it.
    //
    // `null` = the filter has answered every nav query in this zone from properly wound floors.
    // Non-null = the fallback has fired `queries` times since zone load. A `tracing::warn!` is not
    // observable to an agent driving this client over HTTP; a client that quietly answers from a
    // degraded code path is lying by omission. So report it here, where the agent can see it.
    let nav_degraded = s.shared_collision.read().unwrap().as_ref().and_then(|col| {
        let hits = col.fallback_hits();
        (hits > 0).then(|| serde_json::json!({
            "reason": "inverted_floor_art",
            "queries": hits,
            "detail": "parts of this zone's collision mesh are wound INVERTED (down-facing where \
                       ground should face up), so nav cannot verify a floor's facing there and has \
                       fallen back to accepting the column's lowest surface as ground. Routes \
                       through those areas are planned on unverified ground and may be less \
                       reliable (#329/#353).",
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
            // Connection health, all computed at READ time (#8, #343). Three independent failures,
            // three independent signals — a frozen world can no longer masquerade as a live one,
            // because nothing has to be RUNNING for these to be right:
            //   connected          — is the LINK up? (false after CONN_STALE_SECS with no datagram)
            //   link_age_ms        — since any inbound datagram, session ACKs included.
            //   last_packet_age_ms — since the last WORLD update. Reaches 40s+ on an idle session
            //                        with a perfectly healthy link, so do NOT read it as a
            //                        disconnect — that's what `connected` is for.
            //   snapshot_age_ms    — since OUR network thread last ticked. If this is large, every
            //                        other field in this payload is stale and must not be trusted.
            "connected":          health.connected,
            "link_age_ms":        health.link_age_ms,
            "last_packet_age_ms": health.last_packet_age_ms,
            "snapshot_age_ms":    health.snapshot_age_ms,
            "nav_state":   nav_state,
            // Spellcasting (#348). `casting` is non-null ONLY while our own cast bar is running;
            // `last_cast` is how the previous cast ended (cast_completed / cast_interrupted /
            // cast_fizzled / cast_failed, plus cast_ended_unexplained — the client's INFERENCE when
            // the server ended the cast without ever saying why) and survives it. Before this,
            // casting was tracked internally and published NOWHERE — an agent could not tell a spell
            // that landed from one that fizzled, was interrupted, or never started. The same
            // transitions are pushed onto /v1/events/combat as they happen.
            //
            // `elapsed_ms` / `ago_secs` are measured HERE, at read time — the projection above
            // carries the raw `Instant`s and never measures them. Same rule as `health()`: an age is
            // only true at the moment it is read (#343).
            "casting":     player.casting.as_ref().map(|c| serde_json::json!({
                "spell_id":   c.spell_id,
                "spell_name": c.spell_name,
                "cast_ms":    c.cast_ms,
                "elapsed_ms": c.started.elapsed().as_millis() as u64,
            })),
            "last_cast":   player.last_cast.as_ref().map(|o| serde_json::json!({
                "spell_id":   o.spell_id,
                "spell_name": o.spell_name,
                "outcome":    o.outcome,
                "text":       o.text,
                "ago_secs":   o.at.elapsed().as_secs(),
            })),
        },
        // Nav health for THIS zone. `null` when nav is running normally; an object naming the
        // degraded mode when it is not (see `nav_degraded` above). An agent must be able to tell
        // "no route exists" from "this zone's pathing is known-unreliable".
        "nav_degraded": nav_degraded,
        // Per-phase frame timings (ms, EMA-smoothed); all zero unless --profile / EQ_PROFILE=1.
        // Render-owned — the one field here the render loop legitimately publishes.
        "frame_profile": frame_profile,
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
    let coin  = s.player().coin;
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
    let mem = s.player().mem_spells;
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
    let skills = s.player().skills;
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
    let player = s.player();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::quests::tests::{ago, empty_state, set_gs};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn debug_json(state: HttpState) -> serde_json::Value {
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get("/debug").body(Body::empty()).unwrap()).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// #343 regression — THE lie. The connection is dead: no packet has arrived for a minute, and
    /// consequently NOTHING has re-published anything (this is precisely the state the old code
    /// could not represent, because `connected` was computed inside `render_frame` and the render
    /// loop's wake signal is "a packet arrived"). `/debug` must still say `connected: false`, with a
    /// `last_packet_age_ms` that reflects real elapsed time — derived when the agent ASKS, so no
    /// publisher has to be alive for the answer to be honest.
    #[tokio::test]
    async fn debug_reports_disconnected_when_the_world_froze_and_nothing_republished() {
        let state = empty_state();
        // The world as it was when the link was still up — a sitting character, full HP.
        set_gs(&state, |gs| {
            gs.player_name = "Gmkblr".into();
            gs.zone_name   = "qeynos".into();
            gs.hp_pct      = 100.0;
            gs.sitting     = true;
        });
        // ...and then silence: 60s with NO datagram at all (not even a session ACK — the link is
        // genuinely gone), and no publish of any kind.
        {
            let mut h = state.net_health.lock().unwrap();
            h.last_datagram = ago(60);
            h.last_packet   = ago(60);
            h.last_tick     = ago(60);
        }

        let v = debug_json(state).await;
        let p = &v["player"];
        assert_eq!(p["connected"], serde_json::json!(false),
            "a session with no server packet for 60s must NOT report connected:true (#343)");
        assert!(p["last_packet_age_ms"].as_u64().unwrap() >= 60_000,
            "last_packet_age_ms must track real elapsed time, got {}", p["last_packet_age_ms"]);
        assert!(p["snapshot_age_ms"].as_u64().unwrap() >= 60_000,
            "snapshot_age_ms must expose that our own publisher stopped, got {}", p["snapshot_age_ms"]);
        // The stale world is still served (last known good) — but it is now clearly LABELLED stale.
        assert_eq!(p["hp_pct"], serde_json::json!(100.0));
    }

    /// The healthy case must not regress: a packet just landed → connected, ages near zero.
    #[tokio::test]
    async fn debug_reports_connected_while_packets_are_flowing() {
        let state = empty_state();
        set_gs(&state, |gs| gs.player_name = "Gmkblr".into());

        let v = debug_json(state).await;
        let p = &v["player"];
        assert_eq!(p["connected"], serde_json::json!(true));
        assert!(p["last_packet_age_ms"].as_u64().unwrap() < 1_000);
        assert!(p["snapshot_age_ms"].as_u64().unwrap() < 1_000);
    }

    /// `last_packet_age_ms` must ADVANCE between two reads of an otherwise-idle client. This is the
    /// exact live symptom of #343: with the value computed at publish time it stayed frozen at the
    /// same number across consecutive polls whenever the render loop slept.
    #[tokio::test]
    async fn last_packet_age_advances_between_reads_with_no_publisher_running() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_packet = ago(5);
        let first = debug_json(state.clone()).await["player"]["last_packet_age_ms"].as_u64().unwrap();
        // Nothing renders, nothing publishes, no packet arrives — just time passing.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let second = debug_json(state).await["player"]["last_packet_age_ms"].as_u64().unwrap();
        assert!(second > first,
            "last_packet_age_ms froze at {first} across two reads — it is not being derived at read time (#343)");
    }

    /// The two clocks are independent signals and must be reported as such: a live client whose
    /// SERVER went quiet is not the same failure as a client whose own network thread wedged.
    #[tokio::test]
    async fn server_silence_and_publisher_stall_are_distinguishable() {
        let state = empty_state();
        // The link is dead (no datagrams at all), but our network thread is fine and still ticking.
        {
            let mut h = state.net_health.lock().unwrap();
            h.last_datagram = ago(30);
            h.last_packet   = ago(30);
        }
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["connected"], serde_json::json!(false));
        assert!(p["last_packet_age_ms"].as_u64().unwrap() >= 30_000);
        assert!(p["snapshot_age_ms"].as_u64().unwrap() < 1_000,
            "our own publisher is fine — snapshot_age_ms must not blame it for the link's silence");
    }

    /// The OTHER half of honesty, found by live-testing #343: a character sitting alone in an empty
    /// zone receives NO application packet for 40+ seconds while the session layer keeps ACKing
    /// away. That is an IDLE session, not a dead one. Deriving `connected` from application traffic
    /// would report it as disconnected — swapping #343's false `true` for an equally damaging false
    /// `false`, and sending an agent into a pointless reconnect loop. `connected` therefore tracks
    /// the LINK clock, and `last_packet_age_ms` is left free to say "the world is quiet".
    #[tokio::test]
    async fn a_quiet_world_on_a_live_link_is_still_connected() {
        let state = empty_state();
        {
            let mut h = state.net_health.lock().unwrap();
            h.last_packet   = ago(45);                    // the world has nothing to say...
            h.last_datagram = std::time::Instant::now();  // ...but the link is demonstrably alive.
        }
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["connected"], serde_json::json!(true),
            "a quiet world on a live link must NOT be reported as disconnected (#343)");
        assert!(p["last_packet_age_ms"].as_u64().unwrap() >= 45_000,
            "...while still honestly reporting that no world update has arrived for 45s");
        assert!(p["link_age_ms"].as_u64().unwrap() < 1_000);
    }

    /// The player block is a projection of the NETWORK thread's GameState, with no render loop in
    /// the path: a state change published by the network thread is visible to the very next read
    /// even though no frame was ever drawn (#343).
    #[tokio::test]
    async fn player_view_tracks_the_network_snapshot_without_any_render() {
        let state = empty_state();
        set_gs(&state, |gs| { gs.hp_pct = 100.0; gs.target_id = Some(7); });
        let v = debug_json(state.clone()).await;
        assert_eq!(v["player"]["hp_pct"], serde_json::json!(100.0));
        assert_eq!(v["player"]["target_id"], serde_json::json!(7));

        set_gs(&state, |gs| { gs.hp_pct = 12.0; gs.target_id = None; });
        let v = debug_json(state).await;
        assert_eq!(v["player"]["hp_pct"], serde_json::json!(12.0));
        assert_eq!(v["player"]["target_id"], serde_json::json!(null));
        assert_eq!(v["player"]["target_name"], serde_json::json!(null));
    }
}
