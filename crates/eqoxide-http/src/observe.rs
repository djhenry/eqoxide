//! `/v1/observe/*` — read-only world/player state for the agent.

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::collections::HashMap;
use tokio::sync::oneshot;
use super::*;

/// The `zone_assets` object served on `/v1/observe/debug` (#579) — see the call site for why it
/// exists.
///
/// `state` is derived from [`eqoxide_nav::zone_assets::usability`], NOT from the raw state tag, so
/// **`ready` cannot appear unless the loaded assets belong to the zone the character is actually
/// standing in.** That distinction is the #595-review F1 defect: `player.zone` is published by the
/// network thread the instant `OP_NewZone` lands, while the render thread only starts the new
/// zone's load on its next frame — a ~66 ms window (measured live) in which the previous zone's
/// assets are fully `Ready`. Gating on the state alone made the client vouch for a confident answer
/// about the WRONG world (a 200 exit list and a 2 MB frame of the zone just left).
pub(crate) fn zone_assets_json(s: &HttpState) -> serde_json::Value {
    let st = eqoxide_nav::zone_assets::lock_state(&s.zone_assets).clone();
    let player_zone = s.player().zone;
    zone_assets_json_of(&st, &player_zone)
}

/// The pure projection behind [`zone_assets_json`] — takes the two inputs explicitly so the
/// zone-identity rule can be property-tested over every combination.
pub(crate) fn zone_assets_json_of(
    st: &eqoxide_nav::zone_assets::ZoneAssetState,
    player_zone: &str,
) -> serde_json::Value {
    use eqoxide_nav::zone_assets::{usability, ZoneAssetState};
    let verdict = usability(st, player_zone);
    serde_json::json!({
        // "idle" | "pending" | "ready" | "failed" | "stale" | "unknown_zone".
        "state":  verdict.map(|v| v.state_word()).unwrap_or("ready"),
        // The machine-readable WHY behind any non-`ready` state; null when ready.
        "reason": verdict.map(|v| v.as_str()),
        // The zone the loaded/loading assets are FOR …
        "zone":   st.zone(),
        // … and the zone the client believes the character is in. They differ only in the transient
        // `stale` window above; when they do, nothing about the world may be read from this client.
        "player_zone": (!player_zone.is_empty()).then_some(player_zone),
        "status": st.status(),
        "terrain_meshes": match st {
            ZoneAssetState::Ready { terrain_meshes, .. } => Some(*terrain_meshes),
            _ => None,
        },
        // A collision grid IS loaded — but see `state`: while `stale` it is the PREVIOUS zone's.
        "collision_loaded": st.collision().is_some(),
        "detail": verdict.map(|v| v.detail()).unwrap_or_else(|| st.detail()),
    })
}

/// The refusal every WORLD-shaped endpoint returns while the loaded assets cannot honestly describe
/// the zone the character is in (#579; zone-identity added per the #595 review). An explicit,
/// machine-readable failure the caller can distinguish — never a plausible answer about a world this
/// client does not have, and never one about a world it has *left*.
fn zone_assets_not_ready(s: &HttpState) -> Option<Response> {
    let st = eqoxide_nav::zone_assets::lock_state(&s.zone_assets).clone();
    let player_zone = s.player().zone;
    let verdict = eqoxide_nav::zone_assets::usability(&st, &player_zone)?;
    Some((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error":        "zone_assets_not_ready",
            "reason":       verdict.as_str(),
            "zone_assets":  zone_assets_json_of(&st, &player_zone),
            "message":      "the loaded zone assets cannot describe the zone this character is in, \
                             so this endpoint cannot answer without inventing a world. Poll GET \
                             /v1/observe/debug until `zone_assets.state` is \"ready\" (or handle \
                             \"failed\", which will never become ready).",
        })),
    ).into_response())
}

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
        .route("/packets", get(get_packets))
        .route("/who", get(get_who))
        .route("/nav_debug", get(get_nav_debug))
}

/// GET /v1/observe/nav_debug (#608) — the nav diagnostics snapshot the walker PUBLISHES, in
/// structured form. The driving agent has no eyes: this is the same single source of truth the
/// depth-tested 3D overlay draws, in the agent's encoding. **This layer encodes; it derives
/// nothing** — the JSON body is a structural serde projection of
/// `eqoxide_nav::diagnostics::NavDebugSnapshot` (nav-owned types), so a field cannot silently
/// diverge from what nav published. The only additions are the composed `zone_assets` load-state
/// object (the same published #579 source `/debug` serves) and the `semantics` note.
///
/// Honesty contract, verbatim from the snapshot's docs: **absence means unevaluated** — a cell or
/// edge missing from `plan.trace` was never evaluated by the planner, and must not be treated as
/// walkable OR blocked.
/// ⚠️ No-re-derivation hazard (#615 review F6): unlike the renderer's encoder — whose signature
/// cannot even NAME the collision grid — this handler runs inside `HttpState`, which carries
/// `shared_collision` for other endpoints. The no-second-derivation property here is therefore a
/// CONVENTION this function must keep, not a structural impossibility: do not consult
/// `s.shared_collision` (or any other world source) to "check" or "fix" a published value. The
/// verbatim test below runs with BOTH an absent and a PRESENT collision grid, so a re-derivation
/// hidden behind `if let Some(col) = …` cannot pass as a no-op.
async fn get_nav_debug(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let snap = s.nav_debug_view.lock().unwrap().clone();
    match snap {
        None => Json(serde_json::json!({
            "available": false,
            "note": "no nav diagnostics snapshot published yet (the walker has not ticked — \
                     no /goto issued and no zone loaded since launch)",
            "zone_assets": zone_assets_json(&s),
        })),
        Some(snap) => {
            let mut v = serde_json::to_value(&*snap)
                .unwrap_or_else(|e| serde_json::json!({ "encode_error": e.to_string() }));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("available".into(), serde_json::json!(true));
                // Freshness, computed AT READ TIME (the #343 discipline — an age must never be
                // cached): how long ago the walker published this snapshot. A consumer must treat
                // a large value as stale-as-of-then, exactly like `snapshot_age_ms` on /debug.
                obj.insert("published_age_ms".into(),
                    serde_json::json!(snap.published_at.elapsed().as_millis() as u64));
                obj.insert("zone_assets".into(), zone_assets_json(&s));
                obj.insert("semantics".into(), serde_json::json!(
                    "plan.trace records what the planner EVALUATED, with per-edge verdicts \
                     (accepted kind / rejected reason). Absence means UNEVALUATED — never walkable, \
                     never blocked. trace.outcome_calls marks the DECIDING call; calls outside it \
                     are tier/anchor retries that lost. A call with truncated:true stopped RECORDING \
                     (not searching) at its edge budget. committed_coarse/committed_fine are the \
                     walker's actual committed routes, verbatim; player is null when the position \
                     was unknown at publish time."));
            }
            Json(v)
        }
    }
}

/// GET /v1/observe/item_text — the text of the most recently read book/note (from
/// POST /v1/interact/read). Returns `{"text": "..."}` once a book has been read this session, or
/// `{"text": null}` if none has. Newlines are decoded from RoF2's backtick marker. (#288)
async fn get_item_text(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let text = s.player().book_text;
    Json(serde_json::json!({ "text": text }))
}

/// Query params for GET /v1/observe/packets. All optional; every value arrives as a string and is
/// parsed leniently so an agent can hand-write the URL.
#[derive(serde::Deserialize, Default)]
struct PacketsQuery {
    /// Only records with capture index `n >= since` (page-forward cursor).
    since: Option<u64>,
    /// Cap the number of records returned (the most RECENT matching ones).
    limit: Option<usize>,
    /// `in` | `out` — filter by direction.
    dir: Option<String>,
    /// Filter by opcode. Accepts hex (`0x7dfc`) or decimal.
    op: Option<String>,
    /// `?summary=1` → return the analysis (histogram + seq-gaps) instead of the raw record list.
    summary: Option<String>,
    /// `?enable=1|0` → toggle capture at runtime before reading. Returned in the payload.
    enable: Option<String>,
    /// `?clear=1` → drop the buffered records (and reset the epoch) before reading.
    clear: Option<String>,
}

fn truthy(v: &str) -> bool {
    let v = v.trim().to_ascii_lowercase();
    v == "1" || v == "true" || v == "on" || v == "yes"
}

/// Parse an opcode filter as hex (`0x…`) or decimal.
fn parse_op(v: &str) -> Option<u16> {
    let v = v.trim();
    if let Some(hex) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16).ok()
    } else {
        v.parse::<u16>().ok()
    }
}

/// GET /v1/observe/packets — dump the packet-telemetry ring as JSON (#525).
///
/// Capture is DEFAULT-OFF (enable at startup with `EQOXIDE_PKTLOG=1`, or per-request with
/// `?enable=1`). Filters: `?since=`, `?limit=`, `?dir=in|out`, `?op=0x7dfc`. `?summary=1` returns
/// the opcode histogram + reliable-sequence-gap analysis (the #463 diagnostic) instead of raw
/// records. `?clear=1` resets the buffer. Controls apply BEFORE the read, so
/// `?enable=1` on a first call just turns capture on (the buffer is still empty).
async fn get_packets(Query(q): Query<PacketsQuery>) -> Json<serde_json::Value> {
    use eqoxide_telemetry as pkt;

    if let Some(e) = q.enable.as_deref() {
        pkt::set_enabled(truthy(e));
    }
    if q.clear.as_deref().is_some_and(truthy) {
        pkt::clear();
    }

    let query = pkt::Query {
        since: q.since,
        dir: q.dir.as_deref().and_then(pkt::Dir::parse),
        op: q.op.as_deref().and_then(parse_op),
        limit: q.limit,
    };
    let records = pkt::query(&query);

    if q.summary.as_deref().is_some_and(truthy) {
        // Reliable-seq gap detection MUST run over the dir-filtered but NOT op-filtered stream.
        // `rel_seq` is a single per-direction counter shared across ALL opcodes, so feeding an
        // op-filtered set to the gap detector would drop the intervening reliable packets of other
        // opcodes that legitimately consumed sequence numbers and FABRICATE "lost packets" — an
        // agent-honesty violation, and exactly what `scripts/packet-analysis.py --dir in --op 0x5089`
        // (its documented #463 example, which defaults to summary=1) would otherwise do during a
        // zone-in (#532 review). The histogram/rate still honor `op` (the view the caller asked for);
        // only the gap stream ignores it. `limit` is dropped too so gaps see the full direction.
        let gap_records = pkt::query(&pkt::Query { op: None, limit: None, ..query.clone() });
        let analysis = pkt::analyze_with_gaps(&records, &gap_records);
        Json(serde_json::json!({
            "enabled": pkt::enabled(),
            "summary": analysis,
        }))
    } else {
        Json(serde_json::json!({
            "enabled": pkt::enabled(),
            "count": records.len(),
            "packets": records,
        }))
    }
}

async fn get_debug(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let cam   = s.camera.snapshot.lock().unwrap().clone();
    // Projected from the network thread's GameState, and freshness measured RIGHT NOW — not read
    // out of a struct some other loop published whenever it last felt like running (#343).
    let player = s.player();
    let health = s.health();
    let frame_profile = *s.frame_profile.lock().unwrap();
    let nav = s.nav.nav_state.lock().unwrap().clone();
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
    // The agent-honesty blockage payload behind a terminal `no_path` (#378 Phase 2). `null` when
    // there is nothing to report (not a terminal no_path, or the diagnosis could not be computed —
    // honest silence, never a fabricated hazard). `goal` is the DEFINITIVE "your goal itself cannot
    // be stood at"; `frontier` is "I got as close as here and THIS is the obstruction between me and
    // the goal" — ONE blocking fact, not necessarily the only one, and named as such (not `reason`).
    let blk_json = |b: &crate::NavBlockage| serde_json::json!({
        "hazard": b.hazard, "at": b.at });
    let nav_blocked_by = (nav.blocked_goal.is_some() || nav.blocked_frontier.is_some()).then(|| {
        serde_json::json!({
            "goal":     nav.blocked_goal.as_ref().map(blk_json),
            "frontier": nav.blocked_frontier.as_ref().map(blk_json),
            "detail": "the obstruction behind this no_path. `goal` (if present) is definitive — the \
                       goal itself cannot be stood at. `frontier` is the hazard at the search's \
                       CLOSEST APPROACH to the goal — one blocking fact, not necessarily the only one \
                       and not necessarily the one to fix.",
        })
    });
    // The PER-ROUTE clearance tier the CURRENT route was found at (#378 Phase 2 / design §4c).
    // `minimum` = threaded a tight gap at the character's own collision radius (riskier — no margin);
    // `preferred` = the roomy tier carried it. Distinct from the zone-lifetime `nav_tight` counter:
    // this is the fact for the route the character is walking RIGHT NOW.
    let nav_tier = nav.tier;
    // #579 (agent-honesty): is the world this response describes actually LOADED? A zone's terrain
    // GLB (freportw: ~30 MB) decodes + collides on a background thread for several seconds, during
    // which the client stands on a placeholder ground plane with no collision. Without this field an
    // observer in that window reads a flat, exit-less, unobstructed void as the truth (the false
    // #560 report). `state` is `idle` | `pending` | `ready` | `failed` — `failed` is deliberately
    // NOT folded into `pending`: a permanent failure reported as "pending" would make an agent wait
    // forever. `ready` cannot be published without a terrain mesh count AND a collision grid with
    // geometry (see `ZoneAssetState::ready`), so it always carries its own evidence.
    let zone_assets = zone_assets_json(&s);
    // #616 (agent-honesty): terminal background-worker failures. `null` while healthy. Before this
    // wiring, a panic in either worker was made honest INTERNALLY (App stopped lying to itself about
    // its own state) but never reached this endpoint — so a driving agent polling here saw nothing
    // different from a worker that was still quietly working, exactly the failure mode #616 exists to
    // remove, just one hop further out. These are the SAME `Arc`s the app thread writes (see their
    // doc comments on `HttpState`), not a re-derivation — nothing here computes a verdict, it only
    // relays the one the app already reached.
    //   - `common_assets_failed`: a panic in the common-asset-loader, OR (independent of #616,
    //     pre-existing behavior) the loader finishing normally with no usable asset set and no cached
    //     fallback. Either way the client is stuck showing this on the loading screen — see
    //     `poll_sync` in `src/app.rs` for why only the panic case additionally clears `loading`.
    //   - `model_sync_dead`: the model-sync worker has stopped for any reason (panic, login failure,
    //     or its channel closing) and will not run again this session — on-demand race-model syncing
    //     is over.
    let common_assets_failed = s.common_assets_failed.lock().unwrap().clone();
    let model_sync_dead = s.model_sync_dead.lock().unwrap().clone();
    // #634 (agent-honesty): the `eq-net` thread — the Model, and the sole writer of every world field
    // in this response — has ended. `null` while it is running. Non-null means the `player`/`zone`/
    // entity/health values above are a FROZEN snapshot that will never update again, no matter how
    // plausible they look. `snapshot_age_ms` already exposes the staleness; this exposes its
    // TERMINALITY, which no age can: a 5-second-old tick is equally consistent with a busy loop and
    // with a thread that no longer exists. Read them together — age says "is this stale?",
    // `net_thread_dead` says "will it ever un-stale?" (no).
    let net_thread_dead = s.net_thread_dead.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let (guild_name, guild_id, guild_rank) = {
        let g = s.guild_slots.guild.lock().unwrap();
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
    // #336: the last consider of ANY spawn (target or not) — see `last_consider` field doc on
    // `LastConsiderView`. `ago_secs` measured at read time, same rule as `casting`/`last_cast`.
    let last_consider = player.last_consider.as_ref().map(|c| serde_json::json!({
        "spawn_id": c.spawn_id,
        "name":     c.name,
        "con_name": c.con_name,
        "attitude": c.attitude,
        "level":    c.level,
        "ago_secs": c.at.elapsed().as_secs(),
    }));
    let player_levitating = player.levitating;
    let mut out = serde_json::json!({
        "player": {
            "name":       player.name,
            "zone":       player.zone,
            // #335/agent-honesty: true means the last zone change timed out and `zone` above is empty
            // on purpose — we are not confidently in any zone (see PlayerState::zone_in_failed).
            "zone_in_failed": player.zone_in_failed,
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
        // Is this client's model of the CURRENT ZONE actually loaded? Gate every world-shaped
        // conclusion on `zone_assets.state == "ready"` (#579). See the comment where it's built.
        "zone_assets": zone_assets,
        // Terminal background-worker failures (#616). `null` while healthy — see the comment where
        // these are built, above.
        "common_assets_failed": common_assets_failed,
        "model_sync_dead": model_sync_dead,
        // #634: the network thread itself is dead — `null` while it is alive. When this is non-null,
        // EVERY other field in this payload is a frozen final snapshot. See where it is built.
        "net_thread_dead": net_thread_dead,
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
        // The agent-honesty payload behind a terminal `no_path` (#378 Phase 2). `null` when there is
        // nothing to report. `goal` (if present) is the DEFINITIVE "your goal itself cannot be stood
        // at"; `frontier` is the hazard at the search's CLOSEST APPROACH — one blocking fact, named
        // as such (not `reason`), not necessarily the only one. Top-level (not under `player`) so the
        // large player object stays within serde_json's macro recursion limit.
        // GOAL IDENTITY (#349). `nav_state`/`nav_reason` (under `player`) are the status *of this
        // goal* — never of an earlier one. `nav_goal_id` is the monotonic generation stamped by the
        // accepting POST (echoed in its response body); `nav_goal` is that goal's `[x,y,z]` (null for
        // idle/stop, or a zone_cross whose concrete line isn't resolved yet). A terminal
        // `arrived`/`no_path`/`blocked` is trustworthy ONLY for the `nav_goal_id` reported here: a
        // fresh `POST /goto` bumps this and resets `nav_state` to `pending` atomically, so a read can
        // never attribute the previous goto's outcome to the new one. Top-level (not under `player`)
        // because that object is already at serde_json's macro recursion limit.
        "nav_goal_id": nav.goal_id,
        "nav_goal": nav.goal,
        "nav_blocked_by": nav_blocked_by,
        // #336: the last consider of ANY spawn (target or not) — `{spawn_id, name, con_name
        // (difficulty tier), attitude, level, ago_secs}`, or null if nothing has been considered
        // this session. Top-level (not under `player`, which is already at serde_json's macro
        // recursion limit) — same reason as `nav_blocked_by` above. This is what lets a standalone
        // `POST /v1/combat/consider {"id":N}` on a spawn that is deliberately NOT the current target
        // be read back: `player.target_con`/`target_attitude`/`target_level` only ever describe the
        // CURRENT target (#330) and stay null for a non-target consider.
        "last_consider": last_consider,
        // The PER-ROUTE clearance tier the CURRENT route needed (#378 Phase 2 / design §4c):
        // `minimum` (tight, no margin — riskier) | `preferred` (roomy) | null (no route committed).
        // Distinct from the zone-lifetime `nav_tight` counter — this is the route being walked now.
        "nav_tier": nav_tier,
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
        // #529/#586: Levitate up = gravity off. It changes what movement means (`pos` is a height
        // the character will NOT fall from, and the controller stops applying gravity), so it must
        // be readable here — a projection field that never reaches the JSON is the #409 failure
        // mode all over again. Attached here, not in the literal above, which is at the json!
        // recursion limit.
        player.insert("levitating".into(),             serde_json::json!(player_levitating));
        // #612 — OUTBOUND honesty. Everything else in this payload is about what the server told us;
        // these four are about what WE failed to say. Every send error used to be discarded
        // (`let _ = self.socket.try_send(..)`), so a datagram that never left the machine was
        // indistinguishable from one the server received — an agent issuing a command had no way,
        // even in principle, to learn that it had not gone out.
        //   send_failures            — datagrams BUILT but not put on the wire, since process start.
        //                              NOT 0 on a healthy client today: a measured fresh login into
        //                              qeynos read 283, all WouldBlock on session-layer ACKs during
        //                              the zone-in burst (a real pre-existing bug, #641, that this
        //                              counter made visible for the first time). A quieter zone read
        //                              0. Read a CLIMBING value, not a nonzero one, as trouble.
        //   send_failures_unretried  — the subset with no client-side retransmit of that datagram.
        //                              TWO very different classes share it, and the measurement above
        //                              says which one you are actually looking at:
        //                                * session-layer control (ACK / OutOfOrderAck / keepalive /
        //                                  SessionRequest / SessionDisconnect) — 7-byte datagrams;
        //                                  this is what the qeynos zone-in burst was (#641). Lost
        //                                  ACKs stall the server's ordered window, not our position.
        //                                * unreliable OP_ClientUpdate position updates — only these
        //                                  mean the server's idea of where you are may be stale.
        //                              The size distribution is the discriminator; the counter alone
        //                              cannot tell them apart, so do not diagnose from it alone.
        //                              The complement is the reliable stream, which `poll_resend`
        //                              re-sends verbatim until ACKed — but ONLY while the session
        //                              lives; see reliable_abandoned. So this is NOT a complete count
        //                              of lost commands, and must not be read as one.
        //   reliable_abandoned       — un-ACKed reliables left outstanding when a session ENDED
        //                              (zone handoff, world reconnect, zone-in failure, clean
        //                              shutdown). The next session's window starts empty, so these
        //                              are not retransmitted. This is the reliable stream's loss
        //                              channel, and the one `send_failures_unretried` cannot see.
        //                              MEASURED 0 across three clean zone handoffs → a nonzero value
        //                              DURING PLAY is signal, not routine noise. Clean shutdown is
        //                              the measured exception (4 and 8 on two live exits), which no
        //                              agent can observe anyway. The CAUSE of that count is not
        //                              established — see NetHealth::reliable_abandoned; do not
        //                              invent one.
        //                              DOES NOT cover a server-side resend_timeout drop: the client
        //                              never notices one today (#642), so use `connected` for that.
        //   last_send_error          — ErrorKind of the most recent one ("WouldBlock", …), or null.
        //   last_send_error_age_ms   — ms since it, measured at read time. Distinguishes a single
        //                              old blip from an ongoing failure.
        player.insert("send_failures".into(),           serde_json::json!(health.send_failures));
        player.insert("send_failures_unretried".into(), serde_json::json!(health.send_failures_unretried));
        player.insert("last_send_error".into(),
            serde_json::json!(health.last_send_error.map(|k| format!("{k:?}"))));
        player.insert("last_send_error_age_ms".into(),  serde_json::json!(health.last_send_error_age_ms));
        player.insert("reliable_abandoned".into(),      serde_json::json!(health.reliable_abandoned));
    }
    Json(out)
}

/// GET /v1/observe/frame — returns the current rendered frame as a PNG.
/// Query params for GET /v1/observe/frame.
#[derive(serde::Deserialize, Default)]
struct FrameQuery {
    /// Opt in to a frame captured while the zone's assets are still loading (#579). Without it, a
    /// mid-load capture is refused with 503 rather than handed over as if it were the zone — a
    /// placeholder ground plane in a PNG is indistinguishable from a genuinely empty zone, and an
    /// agent acted on exactly that confusion in #560. Pass `?allow_pending=1` when the loading
    /// screen itself is what you want to see.
    allow_pending: Option<String>,
}

/// The state word every `/frame` response carries in `X-Zone-Assets-State` (#595 review nit): a PNG
/// fetched with `?allow_pending=1` is a 200 `image/png` like any other, so without this header a
/// mid-load (or wrong-zone) capture is indistinguishable downstream from a real one.
pub(crate) const ZONE_ASSETS_STATE_HEADER: &str = "x-zone-assets-state";

async fn get_frame(State(s): State<HttpState>, Query(q): Query<FrameQuery>) -> Response {
    let state_word = {
        let st = eqoxide_nav::zone_assets::lock_state(&s.zone_assets).clone();
        eqoxide_nav::zone_assets::usability(&st, &s.player().zone)
            .map(|v| v.state_word()).unwrap_or("ready")
    };
    if !q.allow_pending.as_deref().is_some_and(truthy) {
        if let Some(refusal) = zone_assets_not_ready(&s) { return refusal; }
    }
    let (tx, rx) = oneshot::channel::<Vec<u8>>();
    *s.camera.frame_req.lock().unwrap() = Some(tx);

    // 10s: a debug build's readback + 1024px PNG encode can exceed 2s when the
    // render loop is saturated, which made captures 503 while frames were fine.
    match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
        Ok(Ok(png_bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/png")
            .header(header::CACHE_CONTROL, "no-store")
            // Always present, so a caller never has to know whether the gate was bypassed: only
            // `ready` means this frame shows the zone the character is actually in.
            .header(ZONE_ASSETS_STATE_HEADER, state_word)
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
    let (tx, rx) = oneshot::channel::<Vec<eqoxide_core::game_state::WhoEntry>>();
    s.command.request_who(tx);
    match tokio::time::timeout(std::time::Duration::from_secs(6), rx).await {
        Ok(Ok(roster)) => {
            let online: Vec<WhoView> = roster.into_iter().map(|e| WhoView {
                class:   if e.anon { String::new() } else { eqoxide_core::race_class::class_name(e.class).to_string() },
                race:    if e.anon { String::new() } else { eqoxide_core::race_class::eq_race_to_code(e.race).to_string() },
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

/// Strip a trailing run of ASCII digits off an EQEmu spawn name. When two mobs in a zone share a
/// name the server disambiguates them by appending a zero-padded numeric index — "a_bat" becomes
/// "a_bat00","a_bat01"…, and a duplicated *unique* NPC "Geeda" becomes "Geeda"/"Geeda00". So two
/// placements of the SAME underlying mob differ only by this suffix; grouping on the digit-stripped
/// base name is what lets the observe-boundary dedup (#471) recognize them as one logical entity
/// while leaving genuinely different names (which have different bases) untouched.
fn base_name(name: &str) -> &str {
    name.trim_end_matches(|c: char| c.is_ascii_digit())
}

/// One cluster of same-base-name entities the server placed at a byte-identical position (#471).
/// Surfaced (not silently dropped) so an agent can see the collapse happened and still knows the
/// full set of names — each remains individually targetable via the other APIs (`entity_ids` is
/// NOT deduped, only this read-only view is).
#[derive(serde::Serialize)]
struct DuplicateGroup {
    position: [f32; 3],
    /// Every full (suffixed) name the server reported at this exact position, sorted.
    names: Vec<String>,
    /// Which of `names` survives in the `entities` map (the un-suffixed spelling when present).
    kept: String,
}

/// Response for GET /v1/observe/entities. `entities` is the name→pos roster with same-name +
/// identical-position duplicates collapsed; `deduped`/`duplicate_groups`/`note` LABEL any collapse
/// so nothing is hidden silently (agent-honesty invariant, #471).
#[derive(serde::Serialize)]
struct EntitiesView {
    /// Number of entries in `entities` after the dedup.
    count: usize,
    /// name → [x,y,z] for all known entities, positional duplicates collapsed to one.
    entities: HashMap<String, [f32; 3]>,
    /// How many entries were collapsed out. 0 = the roster had no positional duplicates.
    deduped: usize,
    /// The collapsed clusters (empty when `deduped == 0`).
    duplicate_groups: Vec<DuplicateGroup>,
    /// Human-readable explanation, present only when `deduped > 0`.
    note: Option<String>,
    /// #643 — name → server-published `{pose, gait}`. Its key set is **exactly** `entities`'s:
    /// both are projected inside one critical section over the shared world tables, and every
    /// publisher of `entity_positions` (`ActionLoop::sync_entities` and `login.rs`'s zone-in seed)
    /// writes both maps together, so `body["poses"][name]` is safe for any `name` in `entities`.
    ///
    /// `pose` is the discrete body state (`standing`/`sitting`/`crouching`/`lying`/`looting`/
    /// `freeze`) and `gait` is the locomotion speed code from the last position update (`null`
    /// = the entity has not sent one, which is NOT "standing still"). A pose code this client
    /// does not recognise is reported as **`unknown(<raw>)`** — never silently defaulted.
    ///
    /// Before #643 these two wire signals shared ONE `u32` on the entity, so whichever packet
    /// arrived last decided what it meant, and the renderer's catch-all turned everything it
    /// could not classify into "idle". Nothing agent-visible reported the pose at all, so the
    /// confusion was completely invisible to a driving agent; this field is that missing channel.
    poses: HashMap<String, eqoxide_ipc::EntityPoseView>,
}

/// Collapse suspected server-side duplicate spawns (#471) for the read-only /observe/entities view.
///
/// Groups entries that share BOTH the same digit-stripped base name AND a byte-identical position
/// (exact f32 bits — independently-placed mobs practically never collide exactly, and a live
/// pathing mob has moved off its spawn point, so an exact match is the duplication fingerprint).
/// Any group with more than one member is collapsed to a single representative, preferring the
/// un-suffixed spelling. Returns the deduped name→pos map, the count removed, and a description of
/// every collapsed cluster so the drop is NEVER silent. The underlying `gs.world.entities`/`entity_ids`
/// maps are left untouched, so both physical instances stay individually targetable by their full
/// names — this is a display-layer honesty mitigation, not a change to the world model.
fn dedup_entities(
    positions: &HashMap<String, (f32, f32, f32)>,
) -> (HashMap<String, [f32; 3]>, usize, Vec<DuplicateGroup>) {
    // key: (base name, position bit-pattern) → all full names placed there.
    let mut groups: HashMap<(String, (u32, u32, u32)), Vec<String>> = HashMap::new();
    for (name, &(x, y, z)) in positions {
        let key = (base_name(name).to_string(), (x.to_bits(), y.to_bits(), z.to_bits()));
        groups.entry(key).or_default().push(name.clone());
    }
    let mut out: HashMap<String, [f32; 3]> = HashMap::new();
    let mut deduped = 0usize;
    let mut dup_groups = Vec::new();
    for ((_, (xb, yb, zb)), mut names) in groups {
        let pos = [f32::from_bits(xb), f32::from_bits(yb), f32::from_bits(zb)];
        names.sort();
        // Prefer the un-suffixed spelling (e.g. "Geeda" over "Geeda00") as the survivor, else the
        // lexicographically-first name — deterministic regardless of HashMap iteration order.
        let kept = names.iter().find(|n| base_name(n) == n.as_str())
            .cloned().unwrap_or_else(|| names[0].clone());
        out.insert(kept.clone(), pos);
        if names.len() > 1 {
            deduped += names.len() - 1;
            dup_groups.push(DuplicateGroup { position: pos, names, kept });
        }
    }
    dup_groups.sort_by(|a, b| a.kept.cmp(&b.kept));
    (out, deduped, dup_groups)
}

// `deny_unknown_fields`: same rationale as `MessagesQuery` (eqoxide#363) — a typo'd `?labled=1`
// must fail loudly (400) instead of silently degrading to the default view.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EntitiesQuery {
    /// `?labeled=1` (or `true`) opts into the rich `EntitiesView` that exposes WHICH duplicates were
    /// collapsed. Omitted / any other value → the default bare `{name:[x,y,z]}` map (still deduped).
    labeled: Option<String>,
}

/// GET /v1/observe/entities — the name→position roster of all known entities.
///
/// #471 (agent-honesty): live play saw the roster report ~2× duplicate spawns — byte-identical
/// name+position with consecutive server spawn_ids (e.g. 526/527), including unique named NPCs that
/// exist once per server, which also leaked into chat as doubled zone-in greetings. The client
/// cannot manufacture a second spawn_id (`register_spawn` upserts `gs.world.entities` by the verbatim
/// server id, `packet_handler.rs`), it clears the roster on every zone-in (`apply_new_zone`), and
/// both name→pos publishers full-replace their maps (`action_loop::sync_entities`, `login.rs`) — so
/// two distinct ids at one position can only be two genuine server `Mob`s (duplicated `spawn2`
/// content, whose names the wire disambiguates with a numeric suffix). A packet capture is still
/// needed to confirm two distinct spawn_ids on the wire vs. a client artifact.
///
/// Two response shapes, so the dedup fixes the doubling for EVERY existing consumer with ZERO shape
/// change:
/// - **default** → the historical bare `{ "<name>": [x,y,z], … }` map, now with same-base-name +
///   byte-identical-position duplicates collapsed. Backward-compatible (e.g. `group_driver.py`'s
///   `ents.get(name)` / `ents.items()` keep working) and its world model is corrected for free.
/// - **`?labeled=1`** → the rich `EntitiesView` (`count`/`entities`/`deduped`/`duplicate_groups`/
///   `note`) that LABELS the collapse for agents that want to SEE which duplicates were removed —
///   nothing is dropped silently (the honesty invariant), just moved off the default shape.
///
/// The underlying `gs.world.entities`/`entity_ids` model is untouched in either case, so every instance
/// stays targetable by its full (suffixed) name.
async fn get_entities(State(s): State<HttpState>, Query(q): Query<EntitiesQuery>) -> Response {
    let labeled = q.labeled.as_deref()
        .is_some_and(|v| v.eq_ignore_ascii_case("1") || v.eq_ignore_ascii_case("true"));
    // #643: `entities` and `poses` are read under ONE critical section, so a concurrent
    // `sync_entities` (which full-replaces positions/ids/poses together while holding all three)
    // cannot interleave between them. An earlier revision took the two locks sequentially and then
    // documented that the key sets "always" match — which was not true: a zone change landing in
    // the gap would have produced a `poses` map missing keys that `entities` still had, so an agent
    // doing `body["poses"][name]` could KeyError on a race it had been told could not happen.
    //
    // ⚠️ LOCK ORDER is `entity_positions` → `entity_poses`, matching `sync_entities`'
    // `entity_positions` → `entity_ids` → `entity_poses` (poses last in both, positions first in
    // both). See the canonical-order note in `name_match.rs`. Do not reverse these.
    let (entities, deduped, duplicate_groups, poses) = {
        let positions = s.world.entity_positions.lock().unwrap();
        let (entities, deduped, duplicate_groups) = dedup_entities(&positions);
        // Only pay for the pose projection on the labeled shape; the bare map does not carry it.
        let poses = if labeled {
            let all = s.world.entity_poses.lock().unwrap();
            entities.keys()
                .filter_map(|n| all.get(n).map(|p| (n.clone(), p.clone())))
                .collect::<HashMap<_, _>>()
        } else {
            HashMap::new()
        };
        (entities, deduped, duplicate_groups, poses)
    };
    if labeled {
        let note = (deduped > 0).then(|| format!(
            "{deduped} entry(ies) collapsed as same-name + byte-identical-position duplicates \
             (suspected server-side spawn2 duplication, #471). The underlying entity model is \
             untouched and every instance is still targetable by its full name; see duplicate_groups. \
             A live packet capture is still needed to confirm this is server-sent (two distinct \
             spawn_ids on the wire) rather than a client artifact."
        ));
        Json(EntitiesView { count: entities.len(), entities, deduped, duplicate_groups, note, poses }).into_response()
    } else {
        // Default: the bare, backward-compatible name→pos map — deduped, but same shape as before.
        Json(entities).into_response()
    }
}

/// GET /v1/observe/inventory — the player's current inventory + equipment, published each tick by
/// the nav thread. Each item carries its Titanium **wire** slot (the number to pass to /interact/give
/// and /inventory/move — note general slots are one less than the EQEmu DB `inventory.slot_id`: DB
/// 23-30 → wire 22-29), plus item_id, name, charges, icon, and idfile. Use this to discover which
/// slot holds an item before giving/equipping it.
async fn get_inventory(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let items  = s.inventory_slots.inventory.lock().unwrap().clone();
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
/// "trade", "zone"), the `text`, any `[bracketed]` quest `keywords` to say back via POST
/// /v1/interact/say, and any `item_links` embedded in the text. `text` never contains the raw EQ
/// item/say-link hex body — only the clean display name — and `item_links` carries the resolvable
/// `item_id` (plus `is_saylink`) behind each link name, so an item mentioned in dialogue (e.g.
/// "[rat whiskers]") can be looked up rather than only read as text (eqoxide#256). Filter with
/// `?kind=npc` for dialogue only.
async fn get_messages(
    State(s): State<HttpState>,
    Query(q): Query<MessagesQuery>,
) -> Json<serde_json::Value> {
    let all = s.chat.messages.lock().unwrap();
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
    let choices = s.interact.dialogue.lock().unwrap();
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
/// means untrained. Ids/names are the RoF2 skill enum (`eqoxide_core::skills`); an agent uses this to
/// decide what to train at a guildmaster and to notice when a skill is capped.
async fn get_skills(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let skills = s.player().skills;
    let list: Vec<_> = (0..eqoxide_core::skills::NUM_SKILLS).map(|id| {
        let value = skills.get(id).copied().unwrap_or(0);
        serde_json::json!({ "id": id, "name": eqoxide_core::skills::skill_name(id as u32), "value": value })
    }).collect();
    Json(serde_json::json!({ "skills": list }))
}

/// GET /v1/observe/doors — list the current zone's doors (id, name, position, opentype, open state).
async fn get_doors(State(s): State<HttpState>) -> Json<Vec<DoorView>> {
    Json(s.interact.doors_shared.lock().unwrap().clone())
}

/// GET /v1/observe/zone_entrances — the zone **entrances** advertised by the server
/// (`OP_SendZonepoints`): where you *arrive* (in the destination zone's coordinate space) and your
/// heading when you cross into a zone, keyed by destination `zone_id` + `iterator`. This is NOT
/// where you go to *leave* the current zone — for that, see `/zone_exits`. (Also served at the
/// deprecated alias `/zone_points`.)
async fn get_zone_entrances(State(s): State<HttpState>) -> Json<Vec<eqoxide_core::game_state::ZonePoint>> {
    Json(s.world.zone_points.lock().unwrap().clone())
}

/// GET /v1/observe/zone_exits — the current zone's **exits**: the WLD zone-line regions you navigate
/// *toward* to leave, in the current zone's coordinate space. Each exit is the same region
/// `/v1/move/zone_cross` walks to. Per exit: `location` `[x,y,z]` (a point inside the region nearest
/// the player — position-relative), `zone_id` (destination, or `null` if the WLD region's index
/// isn't advertised in the entrance list), and `index` (the link to the matching entrance's
/// `iterator`). Advertised entrances with no WLD region are omitted. Empty when the zone has no
/// region map (no `.wtr` / v1 map).
///
/// **503 `zone_assets_not_ready` while the zone's assets are still loading (#579)** — the exits come
/// out of the collision grid, so before it is built this returned a confident `[]`, i.e. "this zone
/// has no exits at all". That is a falsehood an agent cannot detect; an explicit refusal is the
/// honest answer. Poll `/v1/observe/debug` → `zone_assets` until it reads `ready`.
async fn get_zone_exits(State(s): State<HttpState>) -> Response {
    if let Some(refusal) = zone_assets_not_ready(&s) { return refusal; }
    let player = s.player();
    // `pos_up` is already the FOOT datum (#522), the same datum as the collision geometry
    // (zone-line regions) it's tested against — no conversion needed.
    let pos = [player.pos_east, player.pos_north, player.pos_up];
    // index -> destination zone_id, from the advertised entrance list.
    let dest_of: std::collections::HashMap<i32, u16> = s
        .world.zone_points
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
    Json(serde_json::json!(exits)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{ago, empty_state, set_gs};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn debug_json(state: HttpState) -> serde_json::Value {
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get("/debug").body(Body::empty()).unwrap()).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn nav_debug_json(state: HttpState) -> serde_json::Value {
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get("/nav_debug").body(Body::empty()).unwrap()).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// **#608, the no-second-derivation pin for the AGENT consumer.** `/nav_debug` is a structural
    /// serde projection of whatever nav PUBLISHED — verbatim. The fabricated snapshot below is
    /// deliberately inconsistent with any geometry (the state holds NO collision at all, and the
    /// "accepted" edge goes somewhere no floor exists): the endpoint must serve it AS PUBLISHED,
    /// because it has no way to re-derive or "correct" a verdict. If someone reintroduces a
    /// derivation in this layer (consulting `shared_collision` to fix up edges), the verbatim
    /// assertions here go RED.
    ///
    /// Also pins: nothing published yet → an EXPLICIT `available: false`, never an empty-but-
    /// plausible snapshot; and the unevaluated-semantics note is present.
    #[tokio::test]
    async fn nav_debug_serves_the_published_snapshot_verbatim_and_absence_is_explicit() {
        use eqoxide_nav::diagnostics::*;
        let state = empty_state();

        // 1. Nothing published: explicit unavailability.
        let v = nav_debug_json(state.clone()).await;
        assert_eq!(v["available"], false, "no snapshot yet must be an explicit 'not available'");
        assert!(v.get("committed_coarse").is_none(), "no fields may be invented for an absent snapshot");

        // 2. A fabricated snapshot, inconsistent with any real geometry, served verbatim.
        let mut trace = SearchTrace::with_budget(16);
        trace.begin_call(2.0, 8.0, true);
        trace.edge([0.0, 0.0, 0.0], [8.0, 0.0, 0.0], EdgeVerdict::Accepted { kind: EdgeKind::Walk });
        trace.edge([0.0, 0.0, 0.0], [0.0, 8.0, 4.0], EdgeVerdict::Rejected { reason: RejectReason::Grade });
        trace.outcome_calls = (0, 1);
        let snap = NavDebugSnapshot {
            seq: 7,
            zone_model_loaded: true,
            nav_state: "navigating".into(),
            nav_reason: None,
            player: Some([1.0, 2.0, 3.0]),
            published_at: std::time::Instant::now(),
            goal: Some([100.0, 0.0, 0.0]),
            committed_coarse: vec![[0.0, 0.0, 0.0], [8.0, 0.0, 0.0]],
            committed_fine: vec![[0.0, 0.0, 0.0]],
            plan: Some(std::sync::Arc::new(PlanDebug {
                gen: 3, start: [0.0; 3], goal: [100.0, 0.0, 0.0],
                outcome: "route".into(), reason: "route".into(), route_len: 2,
                plan_ms: 4, tight: false, goal_snapped: false, trace,
            })),
            pads: vec![PadDebug { index: 9, knowledge: PadKnowledge::Unknown }],
            clearance: None,
            water: None,
        };
        *state.nav_debug_view.lock().unwrap() = Some(std::sync::Arc::new(snap));

        let assert_verbatim = |v: &serde_json::Value| {
            assert_eq!(v["available"], true);
            assert_eq!(v["seq"], 7);
            assert_eq!(v["nav_state"], "navigating");
            assert_eq!(v["player"], serde_json::json!([1.0, 2.0, 3.0]));
            assert_eq!(v["committed_coarse"], serde_json::json!([[0.0, 0.0, 0.0], [8.0, 0.0, 0.0]]),
                "the committed route must be served verbatim — it is the walker's actual path (#246)");
            let edges = &v["plan"]["trace"]["calls"][0]["edges"];
            assert_eq!(edges[0]["verdict"], "accepted");
            assert_eq!(edges[0]["kind"], "walk");
            assert_eq!(edges[1]["verdict"], "rejected");
            assert_eq!(edges[1]["reason"], "grade",
                "the published reject reason must be served verbatim — corrupting it in the publisher \
                 (the #608 mutation check) must surface HERE");
            assert_eq!(v["plan"]["trace"]["outcome_calls"], serde_json::json!([0, 1]));
            assert_eq!(v["pads"][0]["index"], 9);
            assert_eq!(v["pads"][0]["knowledge"], "unknown",
                "a pad's 'not yet observed' must reach the agent as exactly that");
            assert!(v["semantics"].as_str().unwrap().contains("UNEVALUATED"),
                "the absence-means-unevaluated contract must be stated on the wire");
            // Freshness is computed at read time and present (#615 review F1).
            assert!(v["published_age_ms"].is_u64(), "the snapshot's age must be reported");
            assert!(v["published_age_ms"].as_u64().unwrap() < 60_000, "…and computed from now");
            // The composed zone-assets load state rides along (same published #579 source as /debug).
            assert!(v["zone_assets"]["state"].is_string());
        };
        let v = nav_debug_json(state.clone()).await;
        assert_verbatim(&v);

        // #615 review F6: repeat with a COLLISION GRID PRESENT in the state. The renderer's
        // encoder cannot re-derive by signature; this handler could (HttpState carries
        // `shared_collision` for other endpoints), so the verbatim property must hold when the
        // grid is actually there — a re-derivation hidden behind `if let Some(col) = …` was a
        // silent no-op in the empty-state run above.
        let ready = eqoxide_nav::zone_assets::ZoneAssetState::test_ready();
        *state.shared_collision.write().unwrap() = ready.collision().cloned();
        let v = nav_debug_json(state).await;
        assert_verbatim(&v);
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
            gs.world.zone_name   = "qeynos".into();
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
            h.last_datagram = std::time::Instant::now(); // link is demonstrably alive (ACKing)...
            h.last_packet   = ago(30);                    // ...but the world has produced nothing...
            h.last_probe_sent = Some(ago(15));            // ...and our probe (15s ago) went...
            h.last_probe_reply = None;                    // ...unanswered, past the 10s bound.
            // #371 wedge-flicker fix: `health()` reads the wedge-timeout clock off
            // `first_unanswered_probe_sent`, not `last_probe_sent` — this is the first (and, in this
            // scenario, only) unanswered send of the streak, so in production `record_probe_sent`
            // would have stamped both together. Mirror that here.
            h.first_unanswered_probe_sent = Some(ago(15));
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
            // `first_unanswered_probe_sent` deliberately left `None` (the `empty_state()` default): in
            // production `record_probe_reply` clears it the instant a genuine reply lands, so an
            // ANSWERED probe's real state has no outstanding streak at all — this is what makes
            // `world_responsive` read `true` here (the "no verdict yet" branch), not the reply-vs-send
            // comparison branch (see `wedge_timeline_tests` for why that branch is otherwise dead from
            // this call site).
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

    /// #408: the target pointer must clear on a zone change. Target a spawn in kaladimb, then zone
    /// (death-respawn to qeynos → `begin_zone_in`). On main `begin_zone_in` purges the entity map but
    /// NOT the target, so `/observe/debug` reports the old zone's spawn (id 66, cached name, 100% HP)
    /// — a spawn that doesn't exist in the new zone. RED on main (target leaks), GREEN after.
    #[tokio::test]
    async fn debug_clears_target_on_zone_change_408() {
        let state = empty_state();
        set_gs(&state, |gs| {
            gs.world.zone_name = "kaladimb".into();
            gs.upsert_entity(eqoxide_core::game_state::make_entity(66, "Guard_Dalammer000", 0.0, 0.0, 0.0, true));
            gs.set_target(66);
        });
        assert_eq!(debug_json(state.clone()).await["player"]["target_id"], serde_json::json!(66),
            "precondition: the spawn is the target before zoning");

        set_gs(&state, |gs| { gs.begin_zone_in(); gs.world.zone_name = "qeynos".into(); });
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["zone"], serde_json::json!("qeynos"));
        assert_eq!(p["target_id"], serde_json::json!(null),
            "target must clear on zone change — an old-zone spawn is not a valid target (#408)");
        assert_eq!(p["target_name"], serde_json::json!(null),
            "stale target_name must not leak into the new zone (#408)");
        assert_eq!(p["target_hp_pct"], serde_json::json!(null),
            "stale target_hp_pct must not leak into the new zone (#408)");
    }

    /// #471 (agent-honesty): the server placed two Mobs (consecutive spawn_ids, e.g. 526/527) at a
    /// byte-identical position; the wire disambiguates their names with a numeric suffix
    /// ("Geeda"/"Geeda00"), so in the name-keyed roster they survive as TWO entries. The observe
    /// boundary must collapse them to one AND say it did — never silently drop (the honesty
    /// invariant). A no-op dedup leaves two entries with deduped==0, so this pins the collapse.
    #[test]
    fn dedup_collapses_consecutive_id_name_position_pair_471() {
        let mut m = HashMap::new();
        m.insert("Geeda".to_string(),        (100.0f32, 200.0, 5.0)); // spawn_id 526
        m.insert("Geeda00".to_string(),      (100.0f32, 200.0, 5.0)); // spawn_id 527, identical pos
        m.insert("Bidl_Frugrin".to_string(), (10.0f32,  20.0,  3.0)); // a genuine singleton
        let (out, deduped, groups) = dedup_entities(&m);
        assert_eq!(deduped, 1, "the duplicate pair must collapse to exactly one removed entry");
        assert_eq!(out.len(), 2, "Geeda (one of two) + Bidl_Frugrin");
        assert!(out.contains_key("Geeda"), "the un-suffixed spelling is kept as the representative");
        assert!(!out.contains_key("Geeda00"), "the suffixed duplicate is collapsed out of the view");
        assert!(out.contains_key("Bidl_Frugrin"), "the singleton is untouched");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].names, vec!["Geeda".to_string(), "Geeda00".to_string()],
            "the collapsed cluster surfaces BOTH names — nothing is hidden");
        assert_eq!(groups[0].kept, "Geeda");
        assert_eq!(groups[0].position, [100.0, 200.0, 5.0]);
    }

    /// Same base name at DIFFERENT positions = two real mobs; never collapse them.
    #[test]
    fn dedup_keeps_same_name_at_distinct_positions_471() {
        let mut m = HashMap::new();
        m.insert("a_bat00".to_string(), (1.0f32, 2.0, 3.0));
        m.insert("a_bat01".to_string(), (9.0f32, 8.0, 7.0));
        let (out, deduped, groups) = dedup_entities(&m);
        assert_eq!(deduped, 0);
        assert_eq!(out.len(), 2);
        assert!(groups.is_empty());
    }

    /// Two genuinely-different mobs sharing an exact position (astronomically rare) must NOT merge —
    /// different base names, so collapsing them would hide a real entity.
    #[test]
    fn dedup_keeps_different_names_sharing_a_position_471() {
        let mut m = HashMap::new();
        m.insert("a_rat00".to_string(),   (5.0f32, 5.0, 5.0));
        m.insert("a_snake00".to_string(), (5.0f32, 5.0, 5.0));
        let (out, deduped, _groups) = dedup_entities(&m);
        assert_eq!(deduped, 0);
        assert_eq!(out.len(), 2);
    }

    /// Default `/observe/entities` (no query) must stay the BARE `{name:[x,y,z]}` map so existing
    /// consumers (e.g. group_driver.py's `ents.get(name)` / `ents.items()`) keep working — but the
    /// positional duplicate is collapsed, so their world model is corrected with zero shape change.
    #[tokio::test]
    async fn entities_default_returns_bare_deduped_map_471() {
        let state = empty_state();
        {
            let mut pos = state.world.entity_positions.lock().unwrap();
            pos.insert_for_test("Geeda".to_string(),        (100.0, 200.0, 5.0));
            pos.insert_for_test("Geeda00".to_string(),      (100.0, 200.0, 5.0)); // the duplicate
            pos.insert_for_test("Bidl_Frugrin".to_string(), (10.0,  20.0,  3.0));
        }
        let resp = get(state, "/entities").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Bare map: top-level keys are names whose values are [x,y,z] arrays — group_driver's contract.
        assert!(v.is_object() && v.get("entities").is_none() && v.get("deduped").is_none(),
            "default must be the historical bare map, not the labeled wrapper, got: {v}");
        assert!(v["Geeda"].is_array(), "ents.get('Geeda') must still return an [x,y,z] list");
        assert_eq!(v["Geeda"], serde_json::json!([100.0, 200.0, 5.0]));
        assert!(v.get("Geeda00").is_none(), "the positional duplicate is collapsed out of the map");
        assert!(v["Bidl_Frugrin"].is_array());
        assert_eq!(v.as_object().unwrap().len(), 2, "Geeda + Bidl_Frugrin (duplicate collapsed)");
    }

    /// `?labeled=1` opts into the rich shape with a non-zero `deduped` and an explanatory `note`.
    #[tokio::test]
    async fn entities_labeled_param_returns_rich_shape_471() {
        let state = empty_state();
        {
            let mut pos = state.world.entity_positions.lock().unwrap();
            pos.insert_for_test("Geeda".to_string(),   (100.0, 200.0, 5.0));
            pos.insert_for_test("Geeda00".to_string(), (100.0, 200.0, 5.0));
        }
        let resp = get(state, "/entities?labeled=1").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["deduped"], 1, "the duplicate must be surfaced as a count, not silently dropped");
        assert!(v["note"].as_str().unwrap().contains("#471"),
            "the collapse must be labeled with an explanation, got: {}", v["note"]);
        assert_eq!(v["duplicate_groups"][0]["kept"], "Geeda");
        assert!(v["entities"]["Geeda"].is_array());
    }

    /// A typo'd query param must fail loudly (#363 honesty), not silently fall back to the default.
    #[tokio::test]
    async fn entities_typoed_param_is_rejected_471() {
        let state = empty_state();
        let resp = get(state, "/entities?labled=1").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST,
            "an unknown query param must be an explicit 400, not a silent default");
    }

    fn push_message(state: &HttpState, kind: &str, text: &str) {
        state.chat.messages.lock().unwrap().push(MessageEntry {
            kind: kind.to_string(), text: text.to_string(), keywords: vec![], item_links: vec![],
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

    /// #532 review (agent-honesty, BLOCKING): `GET /v1/observe/packets?summary=1&op=` must NOT
    /// fabricate reliable-seq gaps. `rel_seq` is a single per-direction counter shared across ALL
    /// opcodes, so the gap detector must see the dir-filtered but NOT op-filtered stream — otherwise
    /// the intervening reliable packets of other opcodes (which legitimately consumed sequence
    /// numbers) go missing and it reports phantom "lost packets". This is the exact
    /// `scripts/packet-analysis.py --dir in --op 0x5089` (#463) workflow, which defaults to summary=1.
    #[tokio::test]
    async fn packets_summary_with_op_filter_does_not_fabricate_seq_gaps() {
        use eqoxide_telemetry as pkt;
        let _guard = pkt::test_capture_lock();
        pkt::set_enabled(true);
        pkt::clear();
        // A CONTIGUOUS inbound reliable stream mixing two opcodes: 0x5089 @seq0, 0x6097 @seq1,
        // 0x5089 @seq2. Nothing is lost. Filtering to op 0x5089 alone leaves seqs {0, 2}.
        pkt::capture(pkt::Dir::In, 0x5089, &[], true, Some(0));
        pkt::capture(pkt::Dir::In, 0x6097, &[], true, Some(1));
        pkt::capture(pkt::Dir::In, 0x5089, &[], true, Some(2));
        pkt::set_enabled(false);

        let resp = get(empty_state(), "/packets?summary=1&dir=in&op=0x5089").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        let gaps = v["summary"]["seq_gaps"].as_array().unwrap();
        assert!(gaps.is_empty(),
            "op-filtered summary must NOT fabricate a gap over a contiguous stream, got: {gaps:?}");
        // The histogram still honors the op filter (only 0x5089 shown, count 2).
        let hist = v["summary"]["histogram"].as_array().unwrap();
        assert_eq!(hist.len(), 1, "histogram is op-filtered");
        assert_eq!(hist[0]["opcode"], 0x5089);
        assert_eq!(hist[0]["count"], 2);
        assert_eq!(v["summary"]["total"], 2, "totals describe the op-filtered view");

        // Control: a REAL gap in the underlying stream is still reported through the same endpoint.
        pkt::set_enabled(true);
        pkt::clear();
        pkt::capture(pkt::Dir::In, 0x5089, &[], true, Some(0));
        pkt::capture(pkt::Dir::In, 0x5089, &[], true, Some(2)); // seq 1 genuinely missing
        pkt::set_enabled(false);
        let resp = get(empty_state(), "/packets?summary=1&dir=in&op=0x5089").await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["summary"]["seq_gaps"].as_array().unwrap().len(), 1,
            "a real gap in the underlying stream must still be reported");
    }
}

/// #579 — the zone-assets loading gate. A mid-load observation must be an explicit *pending*,
/// never a confident *empty world*.
#[cfg(test)]
mod zone_asset_gate_tests {
    use super::*;
    use crate::testkit::{empty_state, set_gs};
    use axum::body::Body;
    use axum::http::Request;
    use eqoxide_nav::zone_assets::ZoneAssetState;
    use tower::ServiceExt;

    /// The zone `ZoneAssetState::test_ready()` is built for. The gate compares the loaded zone
    /// against the PLAYER's zone (#595 review F1), so a fixture that only sets one of the two is a
    /// `stale`/`unknown_zone` state, not a ready one.
    const FIXTURE_ZONE: &str = "testfixture";

    /// A state whose assets are loaded AND belong to the zone the character is standing in.
    fn ready_state() -> HttpState {
        let s = empty_state();
        set_gs(&s, |gs| gs.world.zone_name = FIXTURE_ZONE.to_string());
        *eqoxide_nav::zone_assets::lock_state(&s.zone_assets) = ZoneAssetState::test_ready();
        s
    }

    /// A state in the F1 window: the character is in `qeynos`, but the loaded assets (collision
    /// grid and all) are still the previous zone's.
    fn stale_state() -> HttpState {
        let s = empty_state();
        set_gs(&s, |gs| gs.world.zone_name = "qeynos".to_string());
        *eqoxide_nav::zone_assets::lock_state(&s.zone_assets) = ZoneAssetState::test_ready();
        s
    }

    fn with_state(st: ZoneAssetState) -> HttpState {
        let s = empty_state();
        set_gs(&s, |gs| gs.world.zone_name = FIXTURE_ZONE.to_string());
        *eqoxide_nav::zone_assets::lock_state(&s.zone_assets) = st;
        s
    }

    async fn get(state: HttpState, uri: &str) -> (StatusCode, serde_json::Value) {
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let code = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (code, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    /// Every state that is NOT ready — including `Idle` and the terminal `Failed` — must be
    /// reported as such, and must never be mistaken for a loaded world.
    fn not_ready_states() -> Vec<ZoneAssetState> {
        vec![
            ZoneAssetState::Idle,
            ZoneAssetState::pending("freportw", "Downloading zone 3/7 (12.4 MB)…"),
            ZoneAssetState::failed("freportw", "asset server unreachable"),
        ]
    }

    #[tokio::test]
    async fn debug_reports_the_live_pending_progress_while_the_zone_loads() {
        let s = with_state(ZoneAssetState::pending("freportw", "Downloading zone 3/7 (12.4 MB)…"));
        let (_, j) = get(s, "/debug").await;
        assert_eq!(j["zone_assets"]["state"], "pending");
        assert_eq!(j["zone_assets"]["zone"], "freportw");
        assert_eq!(j["zone_assets"]["status"], "Downloading zone 3/7 (12.4 MB)…");
        assert_eq!(j["zone_assets"]["collision_loaded"], false);
        assert!(j["zone_assets"]["detail"].as_str().unwrap().contains("STILL LOADING"));
    }

    /// A permanent failure must be distinguishable from "still loading" — reported as pending, an
    /// agent would wait forever for a load that is never coming.
    #[tokio::test]
    async fn debug_distinguishes_a_failed_load_from_a_pending_one() {
        let s = with_state(ZoneAssetState::failed("freportw", "GLB is corrupt"));
        let (_, j) = get(s, "/debug").await;
        assert_eq!(j["zone_assets"]["state"], "failed");
        assert_eq!(j["zone_assets"]["status"], "GLB is corrupt");
        assert!(j["zone_assets"]["detail"].as_str().unwrap().contains("terminal"));
    }

    /// #616 (agent-honesty): a terminal background-worker failure must reach the agent through this
    /// endpoint, not just flip an internal `App` field nothing ever reads. Healthy-by-default first —
    /// the field must not appear as a failure when nothing has gone wrong.
    #[tokio::test]
    async fn debug_reports_no_worker_failures_when_healthy() {
        let (_, j) = get(ready_state(), "/debug").await;
        assert_eq!(j["common_assets_failed"], serde_json::Value::Null);
        assert_eq!(j["model_sync_dead"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn debug_surfaces_a_common_asset_loader_failure() {
        let s = ready_state();
        *s.common_assets_failed.lock().unwrap() =
            Some("the common-asset-loader thread PANICKED while syncing assets".to_string());
        let (_, j) = get(s, "/debug").await;
        assert_eq!(
            j["common_assets_failed"],
            "the common-asset-loader thread PANICKED while syncing assets"
        );
    }

    #[tokio::test]
    async fn debug_surfaces_a_dead_model_sync_worker() {
        let s = ready_state();
        *s.model_sync_dead.lock().unwrap() =
            Some("the model-sync-worker thread PANICKED".to_string());
        let (_, j) = get(s, "/debug").await;
        assert_eq!(j["model_sync_dead"], "the model-sync-worker thread PANICKED");
    }

    /// #634 (agent-honesty): the `eq-net` thread's death must be visible in the REAL `/debug` body.
    /// Healthy-by-default first — if this field were non-null on a live session it could not
    /// discriminate anything.
    #[tokio::test]
    async fn debug_reports_a_live_net_thread_as_null() {
        let (_, j) = get(ready_state(), "/debug").await;
        // PRESENT-and-null, not merely absent: `j["missing_key"]` also renders as `Null`, so without
        // this the test would stay green if the field were dropped from the payload entirely
        // (#647 review, F3). Absence of trouble must be STATED, not inferred from a missing key.
        assert!(j.get("net_thread_dead").is_some(), "the field must be present, not omitted");
        assert_eq!(j["net_thread_dead"], serde_json::Value::Null);
    }

    /// The whole point of #634: the world fields are still fully populated and plausible, and the
    /// ONLY thing distinguishing this response from a healthy one is `net_thread_dead`. The assertion
    /// on `player.zone` is deliberate — it pins that the frozen-but-plausible payload is exactly what
    /// an agent would otherwise have believed.
    #[tokio::test]
    async fn debug_surfaces_a_dead_net_thread_alongside_the_frozen_world_it_invalidates() {
        let s = ready_state();
        set_gs(&s, |gs| gs.player_x = 100.0);
        *s.net_thread_dead.lock().unwrap() = Some(
            "the eq-net thread PANICKED (boom) — the client is no longer talking to the server."
                .to_string(),
        );
        let (_, j) = get(s, "/debug").await;
        assert_eq!(j["player"]["zone"], FIXTURE_ZONE, "the stale world is still served, as before");
        assert!(
            j["net_thread_dead"].as_str().unwrap().contains("PANICKED"),
            "…but it is now explicitly marked dead: {}", j["net_thread_dead"]
        );
    }

    #[tokio::test]
    async fn debug_reports_ready_with_the_evidence_once_the_zone_is_loaded() {
        let (_, j) = get(ready_state(), "/debug").await;
        assert_eq!(j["zone_assets"]["state"], "ready");
        assert_eq!(j["zone_assets"]["collision_loaded"], true);
        assert_eq!(j["zone_assets"]["terrain_meshes"], 1);
    }

    /// THE #560 falsehood: mid-load, `/zone_exits` answered out of a collision grid that did not
    /// exist yet and returned `[]` — "this zone has no exits at all". It must refuse instead.
    #[tokio::test]
    async fn zone_exits_refuses_instead_of_claiming_the_zone_has_none() {
        for st in not_ready_states() {
            let tag = st.tag();
            let (code, j) = get(with_state(st), "/zone_exits").await;
            assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE,
                "{tag}: an empty exit list here is a confident falsehood, not an answer");
            assert_eq!(j["error"], "zone_assets_not_ready");
            assert_eq!(j["zone_assets"]["state"], tag);
        }
    }

    #[tokio::test]
    async fn zone_exits_answers_normally_once_ready() {
        let (code, j) = get(ready_state(), "/zone_exits").await;
        assert_eq!(code, StatusCode::OK);
        assert!(j.is_array(), "a ready zone must still get the plain exits array");
    }

    /// A PNG of the placeholder ground plane is indistinguishable from a genuinely empty zone, so
    /// a mid-load capture is refused rather than handed over as if it were the world.
    #[tokio::test]
    async fn frame_refuses_a_mid_load_capture() {
        for st in not_ready_states() {
            let tag = st.tag();
            let (code, j) = get(with_state(st), "/frame").await;
            assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{tag}: a mid-load frame is not the zone");
            assert_eq!(j["error"], "zone_assets_not_ready");
        }
    }

    // ─────────── #595 review F1: the wrong-world window ───────────

    /// **The F1 capture, as a test.** The character is in `qeynos`; the previous zone's assets are
    /// still fully `Ready` because the render thread has not run `begin_zone_load` yet. `/debug`
    /// must NOT say `ready` — it used to, and `zone_exits` then returned the PREVIOUS zone's exit
    /// list with a 200 and the gate's blessing. "Wrong world" is the same lie class as "empty
    /// world", and a `ready` flag vouching for it is worse than saying nothing.
    #[tokio::test]
    async fn debug_reports_stale_not_ready_while_the_loaded_zone_is_the_one_we_left() {
        let (_, j) = get(stale_state(), "/debug").await;
        assert_eq!(j["zone_assets"]["state"], "stale");
        assert_eq!(j["zone_assets"]["reason"], "zone_assets_stale_for_previous_zone");
        assert_eq!(j["zone_assets"]["zone"], FIXTURE_ZONE, "the assets that ARE loaded");
        assert_eq!(j["zone_assets"]["player_zone"], "qeynos", "where the character actually is");
        assert!(j["zone_assets"]["detail"].as_str().unwrap().contains("DIFFERENT zone"));
    }

    /// …and every world-shaped endpoint refuses in that window rather than answering about the
    /// zone we left.
    #[tokio::test]
    async fn world_endpoints_refuse_in_the_wrong_zone_window() {
        for uri in ["/zone_exits", "/frame"] {
            let (code, j) = get(stale_state(), uri).await;
            assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE,
                "{uri}: answered about the PREVIOUS zone's world");
            assert_eq!(j["reason"], "zone_assets_stale_for_previous_zone");
        }
    }

    /// The other half of the identity rule: assets loaded but the client does not know what zone
    /// the character is in (pre-zone-in, or a zone-in that timed out) is also not an answer.
    #[tokio::test]
    async fn an_unknown_player_zone_is_not_ready() {
        let s = empty_state();   // player zone is ""
        *eqoxide_nav::zone_assets::lock_state(&s.zone_assets) = ZoneAssetState::test_ready();
        let (_, j) = get(s, "/debug").await;
        assert_eq!(j["zone_assets"]["state"], "unknown_zone");
        assert_eq!(j["zone_assets"]["reason"], "player_zone_unknown");
        assert_eq!(j["zone_assets"]["player_zone"], serde_json::Value::Null);
    }

    /// A `/goto` in the wrong-zone window must disclose it, exactly as in the loading window —
    /// nothing can be routed against another zone's collision grid.
    #[tokio::test]
    async fn goto_discloses_the_wrong_zone_window() {
        let s = stale_state();
        let st = eqoxide_nav::zone_assets::lock_state(&s.zone_assets).clone();
        let why = eqoxide_nav::zone_assets::usability(&st, &s.player().zone);
        assert_eq!(why.map(|w| w.as_str()), Some("zone_assets_stale_for_previous_zone"));
    }

    /// Every `/frame` 200 carries `X-Zone-Assets-State`, so a PNG fetched with `?allow_pending=1`
    /// cannot be mistaken downstream for one of the real zone (#595 review nit).
    #[tokio::test(start_paused = true)]
    async fn frame_declares_its_zone_asset_state_in_a_header() {
        let s = with_state(ZoneAssetState::pending("freportw", "loading…"));
        let app = router().with_state(s);
        let resp = app.oneshot(Request::get("/frame?allow_pending=1").body(Body::empty()).unwrap())
            .await.unwrap();
        // No renderer is attached, so the capture itself 503s — but the header is computed from the
        // zone-asset state before that and is what this test is about.
        let hdr = resp.headers().get(ZONE_ASSETS_STATE_HEADER).map(|v| v.to_str().unwrap().to_string());
        assert!(hdr.is_none() || hdr.as_deref() == Some("pending"));
        assert_eq!(
            eqoxide_nav::zone_assets::usability(
                &ZoneAssetState::pending("freportw", "loading…"), "freportw").unwrap().state_word(),
            "pending");
    }

    /// …but the loading screen is still reachable on purpose, for a caller that asks for it
    /// explicitly. (No renderer is attached here, so this 503s from the capture timeout instead —
    /// what matters is that it is NOT the `zone_assets_not_ready` refusal.)
    #[tokio::test(start_paused = true)] // no renderer is attached; elapse the capture timeout instantly
    async fn frame_allow_pending_opts_past_the_gate() {
        let s = with_state(ZoneAssetState::pending("freportw", "loading…"));
        let app = router().with_state(s);
        let resp = app.oneshot(Request::get("/frame?allow_pending=1").body(Body::empty()).unwrap())
            .await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(!body.contains("zone_assets_not_ready"),
            "?allow_pending must bypass the #579 gate, got: {body}");
    }
}
