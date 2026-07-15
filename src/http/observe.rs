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
    let nav = s.nav_state.lock().unwrap().clone();
    // Is nav answering from WINDING-BLIND (inverted-art) ground in this zone? (#375, D-2)
    //
    // D-2 (#375) made the floor predicate `is_standable` FACING-BLIND: a surface is ground on its
    // flatness + headroom, whichever way its art is wound — because some zones bake real, walkable
    // ground from INVERTED (down-facing) art (the qcat live wedge stood on exactly such a walkway,
    // which the old facing filter deleted). That is correct, but it means nav can no longer VERIFY a
    // floor's facing there — it is standing on unverified-winding ground. `facing_blind_hits` counts
    // each query answered from a down-facing surface, so the agent can SEE it.
    //
    // (This REPLACES the old `nav_degraded`/`inverted_floor_art` signal, which counted the
    // `column_bottom` recovery valve firing. D-2 deleted that valve — so if this were left reading the
    // dead counter it would always be `null`, i.e. "every nav query answered from PROPERLY WOUND
    // floors," which is a confident falsehood in exactly the inverted-art zones (permafrost/highpass/
    // neriakc/qcat) where nav is now on winding-blind ground. A degraded/unverified mode must never be
    // silent, so the signal moves with the mechanism.)
    //
    // `null` = every standable surface answered so far faced UP (properly wound). Non-null = nav has
    // answered `queries` times from down-facing (inverted-art) ground since zone load.
    let nav_support = s.shared_collision.read().unwrap().as_ref().and_then(|col| {
        let hits = col.facing_blind_hits();
        (hits > 0).then(|| serde_json::json!({
            "reason": "facing_blind_ground",
            "queries": hits,
            "detail": "parts of this zone's collision mesh are wound INVERTED (down-facing where \
                       ground should face up). Since D-2 (#375) nav accepts such surfaces as floor on \
                       flatness + headroom (they ARE walkable — the qcat wedge proved it), but their \
                       true facing is unverified, so routes through those areas are planned on \
                       winding-blind ground. Not an error; an honest 'this footing is unverified'.",
        }))
    });
    // Tiered clearance (#358): routes are normally planned with a body-width of margin from walls
    // and drops. When the ONLY route to a goal is one that threads a narrow door or a tight bridge
    // with no margin to spare, the planner falls back to the minimum clearance (exactly the
    // character's own collision radius) — still genuinely walkable, but riskier. Report it: an agent
    // that is being handed tight routes deserves to know it is, rather than just noticing it falls
    // off things more often.
    let nav_tight = s.shared_collision.read().unwrap().as_ref().and_then(|col| {
        let n = col.tight_plans();
        (n > 0).then(|| serde_json::json!({
            "reason": "minimum_clearance_fallback",
            "routes": n,
            "detail": "no route existed at the preferred clearance, so these routes were planned at \
                       the MINIMUM (the character's own collision radius) — they fit, but with no \
                       margin from the walls and drops they pass. Expect tight doorways/bridges.",
        }))
    });
    // The fine 2u steering tier's last honest word (#382). `null` when it is threading cleanly — a
    // healthy tier says nothing, exactly like `nav_support` / `nav_tight`.
    let nav_local = nav.local.as_ref().filter(|l| l.state != "threaded").map(|l| serde_json::json!({
        "state":       l.state,
        "reason":      l.reason,
        "stuck_ticks": l.stuck_ticks,
        "plan_us":     l.plan_us,
        "detail": match l.state.as_str() {
            "no_way_through" => "the FINE 2u planner CLOSED its whole 40u window without finding a way \
                                 along the committed coarse route. The corridor is not threadable from \
                                 here. This is a LOCAL fact — it does NOT mean the goal is unreachable \
                                 (the coarse route is being re-planned around it, #246).",
            "exhausted"      => "the FINE 2u planner was CUT SHORT before closing its window (node cap). \
                                 This is 'I don't know', NOT 'there is no way through' — the walker is \
                                 steering on the best partial it has.",
            "planner_dead"   => "the fine-tier worker thread has DIED. Steering has degraded to the \
                                 COARSE 8u route only for the rest of this session: the character keeps \
                                 walking, but without 2u detail it will handle thin ramps and narrow \
                                 openings worse. This is a client fault; restart to recover it.",
            _                => "",
        },
    }));
    let (guild_name, guild_id, guild_rank) = {
        let g = s.guild.lock().unwrap();
        (g.guild_name.clone(), g.guild_id, g.guild_rank)
    };
    // Built here as locals (not inline in the big `json!` below) so the player object's macro
    // expansion stays under serde_json's recursion limit — the object is large and each inline
    // nested `json!` deepens it. `elapsed_ms` / `ago_secs` are still measured at read time (#343).
    let casting = player.casting.as_ref().map(|c| serde_json::json!({
        "spell_id":   c.spell_id,
        "spell_name": c.spell_name,
        "cast_ms":    c.cast_ms,
        "elapsed_ms": c.started.elapsed().as_millis() as u64,
    }));
    let last_cast = player.last_cast.as_ref().map(|o| serde_json::json!({
        "spell_id":   o.spell_id,
        "spell_name": o.spell_name,
        "outcome":    o.outcome,
        "text":       o.text,
        "ago_secs":   o.at.elapsed().as_secs(),
    }));
    let mut out = serde_json::json!({
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
            // #361: false means a merchant buy is in flight/unconfirmed or a detected desync hasn't
            // been re-verified yet — `currency` above may not match the server's real balance.
            "coin_verified": player.coin_verified,
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
            // #292/#409: the last consider's result for the CURRENT target — difficulty tier,
            // attitude enum, and the target's actual level. The PlayerState projection already
            // computes these (gated on a live target_id); they MUST be surfaced here or an agent
            // asking "how tough / what attitude is my target" gets a confident null even though the
            // consider succeeded (the exact #409 agent-honesty regression — the con reached the
            // GameState but never the JSON).
            "target_con":      player.target_con,
            "target_attitude": player.target_attitude,
            "target_level":    player.target_level,
            // Death state for a headless agent (#284/#406). `dead` = currently slain (held until
            // POST /v1/lifecycle/respawn); `killed_by` + `died_ago_secs` persist for a window after
            // death (through a respawn). These are the documented way an agent detects it died and
            // must revive — omitting them let a slain character report `dead: null` forever while the
            // "You have been slain" chat line fired, i.e. a lie by omission (#406). All computed in
            // the projection at read time.
            "dead":          player.dead,
            "killed_by":     player.killed_by,
            "died_ago_secs": player.died_ago_secs,
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
            //   world_responsive / last_world_response_ms — #371: is the WORLD alive, not just the
            //                        socket? Attached to this "player" object just below (kept out of
            //                        this literal only because it is already at the json! recursion
            //                        limit). See there for the contract.
            "connected":          health.connected,
            "link_age_ms":        health.link_age_ms,
            "last_packet_age_ms": health.last_packet_age_ms,
            "snapshot_age_ms":    health.snapshot_age_ms,
            // Navigation (#166, #337). `nav_state` is the state; `nav_reason` is the machine-readable
            // WHY behind a terminal one. The pair exists because the old single overloaded `blocked`
            // could not tell an agent whether the goal was unreachable, whether the planner had
            // simply given up, or whether the walker was physically wedged — so an unreachable goal
            // presented as a silent permanent freeze, which disguised the real nav root cause for
            // months. See docs/http-api.md ("Navigation state") for the full contract.
            //   no_path          — DEFINITIVE: no route exists (nav_reason: goal_not_walkable |
            //                      search_closed | start_isolated | no_geometry). Pick another goal.
            //   search_exhausted — the planner GAVE UP (search_node_cap). This is
            //                      "I don't know", NOT "no". Try a nearer waypoint.
            //   blocked          — a route exists but the walker physically cannot follow it.
            //   blocked          — a route exists but the walker physically cannot follow it
            //                      (nav_reason: walker_stalled | local_no_way_through |
            //                      fall_would_be_lethal). `local_no_way_through` means the FINE 2u
            //                      tier CLOSED its 40u window without finding a way along the coarse
            //                      corridor — the corridor is genuinely not threadable here, which is
            //                      a different fact from "the walker slid into something" (#382).
            "nav_state":   nav.state,
            "nav_reason":  nav.reason,
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
            "casting":     casting,
            "last_cast":   last_cast,
        },
        // Nav footing verification for THIS zone (#375, D-2). `null` when every standable surface so
        // far faced UP (properly wound); an object naming the winding-blind (inverted-art) ground when
        // nav has answered from a down-facing surface (see `nav_support` above). RENAMED from the old
        // `nav_degraded`/`inverted_floor_art`, whose mechanism (the column_bottom valve) D-2 deleted.
        "nav_support": nav_support,
        "nav_tight": nav_tight,
        // The FINE 2u STEERING tier (#382). `null` while it is healthy (a complete fine route to its
        // carrot) or has not yet answered. Non-null when the tier that is actually steering the
        // character cannot see a way through the next 40u — and it says WHICH kind of cannot:
        //
        //   no_way_through — the 40u window's frontier CLOSED. There is genuinely no way along the
        //                    committed coarse corridor from here (the 8u grid skimmed something).
        //                    Falsifiable, and *local*: it says nothing about whether the GOAL is
        //                    reachable. The walker keeps steering on the coarse route and re-plans it
        //                    (#246).
        //   exhausted      — the search was CUT SHORT (node cap). "I DON'T KNOW", not "no".
        //   planner_dead   — the fine worker thread died. Steering has degraded to coarse-only for
        //                    the rest of the session; the walker keeps walking, but with 8u detail.
        //
        // This field exists because until #382 the fine tier's failure was INVISIBLE: it ran under a
        // 150ms wall clock, so "did not reach the carrot" meant either "impassable" or "ran out of
        // clock" with no way to ask which, and `nav_state` said a confident `navigating` throughout.
        // The clock is gone; the ambiguity went with it.
        "nav_local": nav_local,
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
    });
    // #371 — attached here (not inside the literal above, which is already at the json! recursion
    // limit). `connected: true` only proves the SOCKET ACKs; a zone that is still ticking but not
    // servicing our packets (a stuck per-client dispatch / script, or a very slow tick) keeps ACKing
    // while producing no application output for us, which is indistinguishable from a quiet zone by
    // the passive clocks. An active liveness probe (a request the zone main loop must service)
    // settles it:
    //   world_responsive        — false ONLY when a probe went unanswered past PROBE_TIMEOUT_SECS on
    //                             a still-ACKing link. An idle-but-alive zone stays true (the probe
    //                             is answered). USE THIS, not last_packet_age_ms, to judge whether the
    //                             world is unresponsive. True before the first probe fires (no verdict
    //                             yet). NOTE: this catches the still-ticking-but-unresponsive case; a
    //                             TOTAL zone freeze stops ACKs too and is already `connected: false`.
    //   last_world_response_ms  — since the world last PROVED it processed something for us (a probe
    //                             reply or spontaneous packet), whichever is fresher.
    if let Some(player) = out.get_mut("player").and_then(|p| p.as_object_mut()) {
        player.insert("world_responsive".into(),       serde_json::json!(health.world_responsive));
        player.insert("last_world_response_ms".into(), serde_json::json!(health.last_world_response_ms));
    }
    Json(out)
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
    let items  = s.inventory.lock().unwrap().clone();
    let player = s.player();
    Json(serde_json::json!({
        "count": items.len(),
        "items": items,
        "currency": currency_json(player.coin),
        // #361: see the /debug field of the same name — false means `currency` may not match the
        // server's real balance right now (a merchant buy in flight, or an unreconciled desync).
        "coin_verified": player.coin_verified,
    }))
}

// `deny_unknown_fields`: same rationale as `EventsQuery` in events.rs (eqoxide#363) — a typo'd
// `?kidn=npc` must fail loudly instead of silently degrading `kind` to `None` (i.e. "no filter",
// returning the whole log) and reporting a plain 200.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
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

    /// #371 — THE unresponsive-world lie, end to end through the real `health()` projection. The link
    /// is ACKing (a datagram just landed → `connected: true`), the world has been application-silent
    /// for 30s, and an active liveness probe sent 15s ago was never answered (bound is 10s).
    /// `connected` alone would tell the agent the world is fine; `world_responsive: false` is the
    /// honest signal that the zone is not servicing our packets (still-ticking-but-unresponsive; a
    /// total freeze would already be `connected: false`).
    #[tokio::test]
    async fn debug_reports_world_unresponsive_when_a_probe_goes_unanswered_while_the_link_acks() {
        let state = empty_state();
        {
            let mut h = state.net_health.lock().unwrap();
            h.last_datagram     = std::time::Instant::now(); // link is demonstrably alive (ACKing)...
            h.last_packet       = ago(30);                   // ...but the world has produced nothing...
            h.last_probe_sent   = Some(ago(15));             // ...and our probe (15s ago) went...
            h.last_probe_reply  = None;                      // ...unanswered, past the 10s bound.
        }
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["connected"], serde_json::json!(true),
            "the socket is still ACKing — connected must stay honest about the LINK");
        assert_eq!(p["world_responsive"], serde_json::json!(false),
            "an unanswered probe on a live link is a WEDGED world — the #371 signal must fire");
    }

    /// #371, the false-alarm we must NOT raise (the #343 trap in reverse): a legitimately idle world
    /// — 45s with no spontaneous packet — whose probe IS answered stays `world_responsive: true`,
    /// while `last_packet_age_ms` still honestly reports the 45s of app-silence (the probe reply does
    /// NOT reset it).
    #[tokio::test]
    async fn debug_reports_idle_but_answered_world_as_responsive() {
        let state = empty_state();
        {
            let mut h = state.net_health.lock().unwrap();
            h.last_datagram    = std::time::Instant::now();
            h.last_packet      = ago(45);          // no spontaneous world output for 45s (normal idle)
            h.last_probe_sent  = Some(ago(20));
            h.last_probe_reply = Some(ago(2));     // ...but the probe was answered 2s ago → alive
        }
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["connected"], serde_json::json!(true));
        assert_eq!(p["world_responsive"], serde_json::json!(true),
            "an idle world that answers the probe is alive — must not false-alarm on app-silence");
        assert!(p["last_packet_age_ms"].as_u64().unwrap() >= 45_000,
            "the probe reply must NOT reset last_packet_age_ms — its 'world quiet' meaning is preserved");
        assert!(p["last_world_response_ms"].as_u64().unwrap() < 3_000,
            "proof-of-life is fresh (probe answered 2s ago), even though spontaneous traffic is 45s stale");
    }

    /// Before any probe has fired, `world_responsive` defers to the passive signals rather than
    /// asserting a liveness it never measured — it must default to true, not a phantom wedge.
    #[tokio::test]
    async fn debug_defaults_world_responsive_true_before_the_first_probe() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_packet = ago(20);
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["world_responsive"], serde_json::json!(true),
            "no probe sent yet → no verdict → true (read connected/last_packet_age_ms instead)");
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

    // --- Target / death STATE must reach /observe/debug, not just the GameState (#409/#406/#408) ---
    //
    // These exercise the REAL projection path the live bugs slip through: apply the actual packet
    // (OP_Consider / OP_Death / a target then a zone-in) to a GameState, then hit the axum /debug
    // route and assert the JSON. The unit tests in packet_handler.rs mutate+read ONE GameState and
    // so never see that the hand-built /debug JSON dropped the field on the floor — that gap is
    // exactly what #400 skipped and what these close.

    /// OP_Consider reply (RoF2 Consider_Struct): playerid@0, targetid@4, faction@8, level@12.
    fn consider_reply(player_id: u32, target_id: u32, faction: u32, level: u32) -> Vec<u8> {
        let mut p = vec![0u8; 20];
        p[0..4].copy_from_slice(&player_id.to_le_bytes());
        p[4..8].copy_from_slice(&target_id.to_le_bytes());
        p[8..12].copy_from_slice(&faction.to_le_bytes());
        p[12..16].copy_from_slice(&level.to_le_bytes());
        p
    }

    /// OP_Death (Death_S): spawn_id@0 (the dying entity), killer_id@4.
    fn death_reply(spawn_id: u32, killer_id: u32) -> Vec<u8> {
        let mut p = vec![0u8; 32];
        p[0..4].copy_from_slice(&spawn_id.to_le_bytes());
        p[4..8].copy_from_slice(&killer_id.to_le_bytes());
        p
    }

    /// #409: after a successful consider of the CURRENT target, `/observe/debug` must expose the
    /// structured con result — difficulty tier, attitude enum, and the target's level. On main these
    /// are computed by the projection but NEVER serialized by `get_debug`, so an agent reads `null`
    /// though `apply_consider` ran and the con succeeded. RED on main (fields absent), GREEN after.
    #[tokio::test]
    async fn debug_surfaces_consider_result_for_current_target_409() {
        let state = empty_state();
        set_gs(&state, |gs| {
            gs.player_id = 1;
            // The target's REAL level (12) comes from the spawn — deliberately different from the
            // consider reply's ConsiderColor field (13 = red) to prove the two are sourced separately.
            let mut npc = crate::game_state::tests::make_entity(136, "Caleah_Herblender000", 0.0, 0.0, 0.0, true);
            npc.level = 12;
            gs.upsert_entity(npc);
            gs.set_target(136);
            // faction 8 = "threatening", ConsiderColor 13 = "red".
            crate::eq_net::packet_handler::apply_consider(gs, &consider_reply(gs.player_id, 136, 8, 13));
        });
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["target_con"], serde_json::json!("red"),
            "target_con must reach /observe/debug after a successful consider (#409)");
        assert_eq!(p["target_attitude"], serde_json::json!("threatening"),
            "target_attitude must reach /observe/debug after a successful consider (#409)");
        assert_eq!(p["target_level"], serde_json::json!(12),
            "target_level must reach /observe/debug (#409)");
    }

    /// #406: after the character is slain (OP_Death for our own spawn), `/observe/debug` must report
    /// `dead: true` + `killed_by` + `died_ago_secs`. On main the death message fires (log path) but
    /// these STATE fields are never serialized by `get_debug`, so a held corpse reports `dead: null`
    /// forever. RED on main (fields absent → null), GREEN after.
    #[tokio::test]
    async fn debug_surfaces_death_state_after_slain_406() {
        let state = empty_state();
        set_gs(&state, |gs| {
            gs.player_id = 42;
            gs.max_hp = 34;
            gs.upsert_entity(crate::game_state::tests::make_entity(66, "Guard_Doradek000", 0.0, 0.0, 0.0, true));
            crate::eq_net::packet_handler::apply_death(gs, &death_reply(42, 66)); // our spawn, killed by 66
        });
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["dead"], serde_json::json!(true),
            "a slain character must report dead:true on /observe/debug (#406)");
        assert_eq!(p["killed_by"], serde_json::json!("Guard_Doradek000"),
            "killed_by must be surfaced so an agent knows to respawn (#406)");
        assert!(p["died_ago_secs"].as_u64().is_some(),
            "died_ago_secs must be present after a death (#406)");
    }

    /// #408: the target pointer must clear on a zone change. Target a spawn in kaladimb, then zone
    /// (death-respawn to qeynos → `begin_zone_in`). On main `begin_zone_in` purges the entity map but
    /// NOT the target, so `/observe/debug` reports the old zone's spawn (id 66, cached name, 100% HP)
    /// — a spawn that doesn't exist in the new zone. RED on main (target leaks), GREEN after.
    #[tokio::test]
    async fn debug_clears_target_on_zone_change_408() {
        let state = empty_state();
        set_gs(&state, |gs| {
            gs.zone_name = "kaladimb".into();
            gs.upsert_entity(crate::game_state::tests::make_entity(66, "Guard_Dalammer000", 0.0, 0.0, 0.0, true));
            gs.set_target(66);
        });
        assert_eq!(debug_json(state.clone()).await["player"]["target_id"], serde_json::json!(66),
            "precondition: the spawn is the target before zoning");

        set_gs(&state, |gs| { gs.begin_zone_in(); gs.zone_name = "qeynos".into(); });
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["zone"], serde_json::json!("qeynos"));
        assert_eq!(p["target_id"], serde_json::json!(null),
            "target must clear on zone change — an old-zone spawn is not a valid target (#408)");
        assert_eq!(p["target_name"], serde_json::json!(null),
            "stale target_name must not leak into the new zone (#408)");
        assert_eq!(p["target_hp_pct"], serde_json::json!(null),
            "stale target_hp_pct must not leak into the new zone (#408)");
    }

    fn push_message(state: &HttpState, kind: &str, text: &str) {
        state.messages.lock().unwrap().push(MessageEntry {
            kind: kind.to_string(), text: text.to_string(), keywords: vec![],
        });
    }

    async fn get(state: HttpState, uri: &str) -> axum::response::Response {
        let app = router().with_state(state);
        app.oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap()
    }

    /// eqoxide#363: a typo'd query param (`?kidn=npc` instead of `?kind=npc`) must be rejected with
    /// an explicit 400 naming the bad field, NOT silently ignored so `kind` falls back to `None`
    /// (no filter) and the caller gets the whole message log back looking like a normal 200.
    #[tokio::test]
    async fn typoed_query_param_is_rejected_not_silently_dropped() {
        let state = empty_state();
        push_message(&state, "npc", "Well met, traveler.");
        push_message(&state, "chat", "someone: hi");
        let resp = get(state, "/messages?kidn=npc").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST,
            "a typo'd/unknown query param must be an explicit failure, not a silent 200 over the whole log");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let msg = String::from_utf8_lossy(&bytes);
        assert!(msg.contains("kidn"), "the 400 body should name the offending field, got: {msg}");
    }

    /// The happy path must not regress: a correctly-spelled `kind` still filters normally.
    #[tokio::test]
    async fn valid_kind_param_still_works() {
        let state = empty_state();
        push_message(&state, "npc", "Well met, traveler.");
        push_message(&state, "chat", "someone: hi");
        let resp = get(state, "/messages?kind=npc").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["messages"][0]["kind"], "npc");
    }
}
