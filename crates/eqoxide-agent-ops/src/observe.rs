//! `/v1/observe/*` — read-only world/player state for the agent.

use crate::wire::{Ops, Reply, Req, StatusCode};
use tokio::sync::oneshot;
use super::*;

/// The pure projection behind [`zone_assets_json`] — takes the two inputs explicitly so the
/// zone-identity rule can be property-tested over every combination.
pub(crate) fn zone_assets_json_of(
    st: &eqoxide_zone_geometry::zone_assets::ZoneAssetState,
    player_zone: &str,
) -> serde_json::Value {
    use eqoxide_zone_geometry::zone_assets::{usability, ZoneAssetState};
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

/// The body of that refusal, split out so a caller that obtained its verdict from
/// [`eqoxide_zone_geometry::zone_assets::usable_collision`] (which hands back the grid as well) serves the
/// byte-identical 503 rather than a second, drifting spelling of it (#821 review round 2, B4).
fn zone_assets_refusal(
    verdict: eqoxide_zone_geometry::zone_assets::NotUsable,
    st: &eqoxide_zone_geometry::zone_assets::ZoneAssetState,
    player_zone: &str,
) -> Reply {
    Reply::json(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::json!({
            "error":        "zone_assets_not_ready",
            "reason":       verdict.as_str(),
            "zone_assets":  zone_assets_json_of(st, player_zone),
            "message":      "the loaded zone assets cannot describe the zone this character is in, \
                             so this endpoint cannot answer without inventing a world. Poll GET \
                             /v1/observe/debug until `zone_assets.state` is \"ready\" (or handle \
                             \"failed\", which will never become ready).",
        }),
    )
}

/// **503 for a question that can only be answered from the zone's `.wtr` region map when that map
/// is not there (#803).**
///
/// The `zone_assets_not_ready` gate above covers the terrain GLB and the collision grid built from
/// it. It does **not** cover the `.wtr`: a zone whose GLB loaded fine and whose region map did not
/// is fully `ready`, and used to answer `/v1/observe/zone_exits` with a bare `[]` and 200 OK. That
/// is indistinguishable from the true, common reading "this zone has no zone lines" — and since
/// exits are the only way out of a zone, an agent reads it as *sealed in*, off a success response.
///
/// Deliberately NOT folded into `NotUsable`/`zone_assets_not_ready`. **`usability()` is reached
/// from FOUR non-test call sites in this file**, named below by their enclosing FUNCTION.
///
/// Not by `file:line` (#958): a name survives an edit above it; a line number does not, and it
/// fails silently, which is the worse property. `usability_consumers_are_exactly_the_ones_this_comment_names`
/// re-derives the list from source on every run.
///
/// Calling it directly:
///   * `zone_assets_json_of` and `zone_assets_not_ready` — every `/observe/*` endpoint;
///   * `get_frame` — the `X-Zone-Assets-State` word on a PNG capture.
///
/// Reaching it one call indirect, via [`eqoxide_zone_geometry::zone_assets::usable_collision`], whose first
/// statement calls `usability` and returns the verdict as `Err`:
///   * `get_zone_exits`.
///
/// So a new `NotUsable` variant would stop every `/observe/*` answer AND stop rendering
/// frames in any zone with a missing `.wtr` — far past what a
/// region-map failure actually invalidates. The refusal belongs to the questions whose answer really
/// does come out of that file. (Confirmed live in the PR's forced-failure run: with the `.wtr`
/// broken and the zone otherwise `ready`, `/frame`, `/zone_entrances` and `/debug` all still
/// answered `200`.)
///
/// `reason` is [`eqoxide_core::region_map::RegionDataAbsent::as_str`] — distinct per cause, so an
/// agent (or an operator reading its log) can tell "the asset pack never delivered this file" from
/// "the file is truncated" from "this build cannot read that version".
fn region_data_unavailable(absent: &eqoxide_core::region_map::RegionDataAbsent) -> Reply {
    Reply::json(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::json!({
            "error":   "zone_region_data_unavailable",
            "reason":  absent.as_str(),
            "detail":  absent.to_string(),
            "message": "this zone's region map (its `.wtr`) is not loaded, and the answer to this \
                        question comes out of that file. An empty list here would say \"this zone \
                        has no exits\" — a claim this client cannot make, and one you could not \
                        tell apart from the truth. This does NOT resolve itself by polling: it is a \
                        missing/unusable asset, not a load in progress. Re-sync the zone's asset \
                        pack, or use `/v1/observe/zone_entrances` (server-advertised, independent of \
                        the `.wtr`) to reason about where this zone connects.",
        }),
    )
}

/// #646 (split out of #634/#647): the read-time freshness age every `observe/*` op carries, as a
/// top-level `"snapshot_age_ms"` key in its reply.
///
/// **This is the SAME clock as `debug`'s `snapshot_age_ms`, not a second one** — both are
/// `OpState::health().snapshot_age_ms`, i.e. `NetHealth::last_tick`'s age against the health clock
/// (`Instant::now()` in every non-test build — see `HealthClock`, #760) measured fresh on
/// every request (#343: an age is only true at the instant it's read, so nothing here is cached).
/// `last_tick` is bumped, unconditionally, once per gameplay tick by the SAME `eq-net` thread loop
/// iteration that publishes `GameState` and drains `ActionLoop::tick` — the single writer of every
/// world table these ops read (`entity_positions`, inventory, chat messages, dialogue
/// choices, doors, zone points, `GameState`'s spells/skills/book_text) — see
/// `crates/eqoxide-net/src/gameplay.rs` around `action_loop.tick(..)` immediately followed by
/// `publish_snapshot(..)`. So a wedged/dead net thread freezes the world tables and stops bumping
/// this clock in the SAME instant: the age reported here always reflects whether the data next to
/// it can still change, never merely whether that data happens to have changed recently.
///
/// The roster ops (`doors`, `zone_entrances`, `zone_exits`) wrap their list under a named key
/// (`doors`, `zone_entrances`, `zone_exits`) so the age has somewhere to ride; before the socket
/// protocol they returned a bare array and carried the age in an HTTP header.

pub(super) fn register(ops: &mut Ops) {
    ops.add("GET /v1/observe/inventory", get_inventory);
    ops.add("GET /v1/observe/messages", get_messages);
    ops.add("GET /v1/observe/dialogue", get_dialogue);
    ops.add("GET /v1/observe/spells", get_spells);
    ops.add("GET /v1/observe/skills", get_skills);
    ops.add("GET /v1/observe/doors", get_doors);
    ops.add("GET /v1/observe/zone_entrances", get_zone_entrances);
    // Deprecated alias for zone_entrances (its content was always the entrance/arrival list).
    ops.add("GET /v1/observe/zone_points", get_zone_entrances);
    ops.add("GET /v1/observe/zone_exits", get_zone_exits);
    ops.add("GET /v1/observe/item_text", get_item_text);
    ops.add("GET /v1/observe/who", get_who);
}

/// GET /v1/observe/item_text — the text of the most recently read book/note (from
/// POST /v1/interact/read). Returns `{"text": "..."}` once a book has been read this session, or
/// `{"text": null}` if none has. Newlines are decoded from RoF2's backtick marker. (#288)
async fn get_item_text(s: OpState, _req: Req) -> serde_json::Value {
    let text = s.player().book_text;
    // #646: read-time freshness — see `the freshness doc above for the clock this reuses.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    serde_json::json!({ "text": text, "snapshot_age_ms": snapshot_age_ms })
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
async fn get_who(s: OpState, _req: Req) -> Reply {
    let (tx, rx) = oneshot::channel::<Vec<eqoxide_core::game_state::WhoEntry>>();
    s.command.request_who(tx);
    match tokio::time::timeout(std::time::Duration::from_secs(6), rx).await {
        Ok(Ok(roster)) => {
            let online: Vec<WhoView> = roster.into_iter().map(|e| WhoView {
                class:   if e.anon { String::new() } else { eqoxide_core::race_class::class_name(e.class).to_string() },
                race:    if e.anon { String::new() } else { eqoxide_core::race_class::eq_race_to_code(e.race).to_string() },
                name: e.name, level: e.level, zone_id: e.zone_id, guild: e.guild, anon: e.anon,
            }).collect();
            // #646: read-time freshness — see `the freshness doc above for the clock this
            // reuses. Note this only reflects whether the net thread is alive/ticking; the roster
            // itself is always a fresh request/reply, not a cached snapshot.
            let snapshot_age_ms = s.health().snapshot_age_ms;
            Reply::ok(serde_json::json!({ "online": online, "snapshot_age_ms": snapshot_age_ms }))
        }
        _ => Reply::text(
            StatusCode::SERVICE_UNAVAILABLE,
            "no /who response (not connected, or server did not reply in time)",
        ),
    }
}

// `deny_unknown_fields`: same rationale as `MessagesQuery` (eqoxide#363) — a typo'd `?labled=1`
// must fail loudly (400) instead of silently degrading to the default view.

/// GET /v1/observe/inventory — the player's current inventory + equipment, published each tick by
/// the net thread. Each item carries its Titanium **wire** slot (the number to pass to /interact/give
/// and /inventory/move — note general slots are one less than the EQEmu DB `inventory.slot_id`: DB
/// 23-30 → wire 22-29), plus item_id, name, charges, icon, and idfile. Use this to discover which
/// slot holds an item before giving/equipping it.
async fn get_inventory(s: OpState, _req: Req) -> serde_json::Value {
    let items  = s.inventory_slots.inventory.lock().unwrap().clone();
    let player = s.player();
    serde_json::json!({
        "count": items.len(),
        "items": items,
        "currency": currency_json(player.coin),
        // #361: see the /debug field of the same name — false means `currency` may not match the
        // server's real balance right now (a merchant buy in flight, or an unreconciled desync).
        "coin_verified": player.coin_verified,
        // #646: read-time freshness — see `the freshness doc above for the clock this reuses.
        "snapshot_age_ms": s.health().snapshot_age_ms,
    })
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
/// ~50 lines), published each tick by the net thread. This is how an agent reads **NPC dialogue**:
/// each line has a `kind` ("npc" = NPC say/emote, plus "chat", "combat", "system", "exp", "loot",
/// "trade", "zone"), the `text`, any `[bracketed]` quest `keywords` to say back via POST
/// /v1/interact/say, and any `item_links` embedded in the text. `text` never contains the raw EQ
/// item/say-link hex body — only the clean display name — and `item_links` carries the resolvable
/// `item_id` (plus `is_saylink`) behind each link name, so an item mentioned in dialogue (e.g.
/// "[rat whiskers]") can be looked up rather than only read as text (eqoxide#256). Filter with
/// `?kind=npc` for dialogue only.
async fn get_messages(s: OpState, req: Req) -> Reply {
    let q = match req.query::<MessagesQuery>() {
        Ok(q) => q,
        Err(e) => return Reply::text(StatusCode::BAD_REQUEST, e),
    };
    let all = s.chat.messages.lock().unwrap();
    let filtered: Vec<&MessageEntry> = match &q.kind {
        Some(k) => all.iter().filter(|m| m.kind == *k).collect(),
        None    => all.iter().collect(),
    };
    // #646: read-time freshness — see `the freshness doc above for the clock this reuses.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    Reply::ok(serde_json::json!({ "count": filtered.len(), "messages": filtered, "snapshot_age_ms": snapshot_age_ms }))
}

/// GET /v1/observe/dialogue — the current clickable NPC-dialogue choices (saylinks from the most
/// recent NPC message, e.g. a Soulbinder's "[bind your soul]"). `index` is the argument POSTed to
/// /v1/interact/dialogue to click that choice. Empty when no NPC has offered choices. (#120)
async fn get_dialogue(s: OpState, _req: Req) -> serde_json::Value {
    let choices = s.interact.dialogue.lock().unwrap();
    let list: Vec<_> = choices.iter().enumerate()
        .map(|(i, c)| serde_json::json!({ "index": i, "text": c.text }))
        .collect();
    // #646: read-time freshness — see `the freshness doc above for the clock this reuses.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    serde_json::json!({ "count": list.len(), "choices": list, "snapshot_age_ms": snapshot_age_ms })
}

/// GET /v1/observe/spells — the 9 memorized gems with names. Empty gem = spell id 0 or 0xFFFFFFFF.
///
/// #1127: also carries each known gem's `mana_cost`/`cast_time_ms`/`recast_time_ms` — the
/// per-ability resource cost/cooldown an agent needs to decide whether casting a gem is even
/// possible right now (enough mana, no timer running), straight from `spells_us.txt`
/// (`eqoxide_core::spells::SpellInfo`). `null` alongside `name: null` for an empty gem or an id our
/// spell table has no row for — never a fabricated `0`, since 0 is itself a real, meaningful cost.
async fn get_spells(s: OpState, _req: Req) -> serde_json::Value {
    let mem = s.player().mem_spells;
    let gems: Vec<_> = mem.iter().enumerate().map(|(i, &id)| {
        if id == 0 || id == 0xFFFF_FFFF {
            serde_json::json!({
                "gem": i, "spell_id": null, "name": null,
                "mana_cost": null, "cast_time_ms": null, "recast_time_ms": null,
            })
        } else {
            match s.spells.get(id) {
                Some(info) => serde_json::json!({
                    "gem": i, "spell_id": id, "name": info.name,
                    "mana_cost": info.mana_cost,
                    "cast_time_ms": info.cast_time_ms,
                    "recast_time_ms": info.recast_time_ms,
                }),
                None => serde_json::json!({
                    "gem": i, "spell_id": id, "name": null,
                    "mana_cost": null, "cast_time_ms": null, "recast_time_ms": null,
                }),
            }
        }
    }).collect();
    // #646: read-time freshness — see `the freshness doc above for the clock this reuses.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    serde_json::json!({ "gems": gems, "snapshot_age_ms": snapshot_age_ms })
}

/// GET /v1/observe/skills — the player's skills with current values (eqoxide#99). `value == 0`
/// means untrained. Ids/names are the RoF2 skill enum (`eqoxide_core::skills`); an agent uses this to
/// decide what to train at a guildmaster and to notice when a skill is capped.
async fn get_skills(s: OpState, _req: Req) -> serde_json::Value {
    let skills = s.player().skills;
    let list: Vec<_> = (0..eqoxide_core::skills::NUM_SKILLS).map(|id| {
        let value = skills.get(id).copied().unwrap_or(0);
        serde_json::json!({ "id": id, "name": eqoxide_core::skills::skill_name(id as u32), "value": value })
    }).collect();
    // #646: read-time freshness — see `the freshness doc above for the clock this reuses.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    serde_json::json!({ "skills": list, "snapshot_age_ms": snapshot_age_ms })
}

/// GET /v1/observe/doors — list the current zone's doors (id, name, position, opentype, open state).
///
/// **`[]` does NOT mean "this zone has no doors" (#939, residual case).** The roster starts empty
/// on either zone-entry path and refills from `OP_SpawnDoor` records as they arrive — but a record
/// the server genuinely has not sent yet arrives on no schedule this client controls, and nothing
/// served here tells that case apart from a real, empty zone. (#891 clears it at the top of the
/// re-zone handshake; on a session's FIRST zone-in it starts empty because the process just
/// started, and #1022's failure clear keeps it empty across `run_login_flow`'s retries.)
///
/// **What #937 used to add on top of that — a record already applied to game state but not yet
/// published — is now addressed on BOTH zone-entry paths (#1016, then #1022).**
/// `sync_doors`/`publish_doors` (`eqoxide_net::action_loop`) used to have exactly one call site, the
/// gameplay drain (`run_gameplay_phase`), which NEITHER zone-entry path's own drain ever reached; a
/// door parsed and applied during a zone-in sat unpublished until the first post-handshake drain.
/// `run_zone_entry_handshake` (`eqoxide_net::gameplay`, the re-zone path, #1016) and
/// `run_login_handshake` (`eqoxide_net::login`, the first zone-in of a session, #1022) now each call
/// `publish_doors` from their OWN drain, on the same pass that applies an `OP_SpawnDoor` record — so
/// an arrived-and-applied door is also a published one within the zone-in, not only after it. Both
/// also CLEAR the roster rather than leave a partial one standing when the attempt fails (#1016
/// review B4 for the re-zone handshake's timeout; #1022's `run_login_phase` wrapper for every login
/// error exit) — a partial roster for a zone the client was just told it failed to enter is not an
/// honest `[]` either.
///
/// **The residual publish window is one drain PASS, not one zone-in, and it is the same on both
/// zone-entry paths.** Each ZONE-ENTRY drain applies every packet it drained and then publishes
/// once, at the end of the pass — so a record applied earlier in the same pass is readable here only
/// after that pass's publish. (The gameplay drain does not share this: `sync_doors` runs after EVERY
/// applied packet there, not once per pass.) That is bounded by a single drain iteration instead of
/// by however long a zone-in takes, but it is not zero, which is why POST /v1/interact/click_door's
/// empty-roster `404` body still names "arrived but not yet published" as a live cause. This
/// endpoint states the window in drain passes rather than milliseconds because it has no
/// measurement of a pass's wall-clock duration.
///
/// There is no completeness observable to wait on for the remaining #939 case: `zone_assets.state`
/// reaching `"ready"` gates on terrain meshes and collision triangles, so it is about geometry, not
/// about which door packets the server has sent. **That limit is now disclosed IN BAND** rather than
/// only here and in `docs/http-api.md`: `GET /v1/observe/debug`'s `server_pushed_rosters` lists this
/// endpoint alongside `/v1/observe/entities` and `/v1/observe/zone_entrances` — the other two
/// rosters filled from server-pushed records — with the count held and `complete: null`. See
/// [`server_pushed_rosters`] for why `null` is the only value that field ever takes.
///
/// The same limit binds the other direction, and this is the reason `/v1/interact/click_door`
/// answers a miss with `404` *unknown* rather than *disproved*: a **populated** roster is not a
/// closed set either, so "not among the doors held right now" is the strongest claim available at
/// any roster size. Do not let a non-empty list here read as a complete one.
async fn get_doors(s: OpState, _req: Req) -> serde_json::Value {
    let snapshot_age_ms = s.health().snapshot_age_ms;
    let doors = s.interact.doors_shared.lock().unwrap().clone();
    serde_json::json!({ "doors": doors, "snapshot_age_ms": snapshot_age_ms })
}

/// GET /v1/observe/zone_entrances — the zone **entrances** advertised by the server
/// (`OP_SendZonepoints`): where you *arrive* (in the destination zone's coordinate space) and your
/// heading when you cross into a zone, keyed by destination `zone_id` + `iterator`. This is NOT
/// where you go to *leave* the current zone — for that, see `/zone_exits`. (Also served at the
/// deprecated alias `/zone_points`.)
///
/// The list can also carry a handful of client-synthesized entries read from this zone's map `.txt`
/// (labels starting `"to "`, currently only recognized for North/South Qeynos) — a fallback for
/// zones the server doesn't fully advertise. **#816:** if that `.txt` read fails, those fallback
/// entries are simply absent from this list rather than announced as missing — check
/// `zone_map_load` on `GET /v1/observe/debug` (`null` = this zone's map-label fallback loaded fine
/// or hasn't been needed yet; non-null names why it didn't, so an empty/short list here can be told
/// apart from "this zone's map genuinely has no fallback labels"). The SERVER-advertised entries in
/// this list are never affected — this only ever narrows the additive fallback.
///
/// **`[]` here carries the same ambiguity `/v1/observe/doors` does, and for the same reason
/// (#1073).** The zone-entry handshake clears `zone_points` (#1010/#1063) so that this list stops
/// describing the zone the character just LEFT. A session's FIRST zone-in reaches the same empty by
/// a different route: the roster starts empty, so there is nothing to clear and the same ambiguity
/// holds without any clear having run. Either way the records refill from `OP_SendZonepoints` on no
/// schedule this client controls. During that window an `[]` is the same bytes as the true answer
/// "this zone has no entrances". `zone_map_load` does not cover this — it reports the outcome of the
/// additive map-label `.txt` load and nothing else, so it says nothing at all about whether the
/// SERVER-advertised entries have arrived. The limit is disclosed in band on
/// `GET /v1/observe/debug`'s `server_pushed_rosters`,
/// which lists this endpoint with the other two rosters that share the shape; see
/// [`server_pushed_rosters`].
async fn get_zone_entrances(s: OpState, _req: Req) -> serde_json::Value {
    let snapshot_age_ms = s.health().snapshot_age_ms;
    let points = s.world.zone_points.lock().unwrap().clone();
    serde_json::json!({ "zone_entrances": points, "snapshot_age_ms": snapshot_age_ms })
}

/// GET /v1/observe/zone_exits — the current zone's **exits**: the WLD zone-line regions that lead
/// out of the zone, in the current zone's coordinate space. Per exit: `location` `[x,y,z]` (a point inside the region nearest
/// the player — position-relative), `zone_id` (destination, or `null` if the WLD region's index
/// isn't advertised in the entrance list), and `index` (the link to the matching entrance's
/// `iterator`). Advertised entrances with no WLD region are omitted.
///
/// **`[]` means exactly one thing: this zone's region map loaded and contains no zone-line regions
/// (#803).** It never means "the file that would list them did not load" — that is a
/// **503 `zone_region_data_unavailable`** with a machine-readable `reason`
/// (`region_data_missing` / `_unreadable` / `_not_region_data` / `_unsupported_version` /
/// `_truncated`). Before #803 both produced `[]` with 200 OK, and because exits
/// are the only way out of a zone, an agent read a failed read as "sealed in". Unlike the
/// `zone_assets_not_ready` 503 below, this one does **not** clear by polling — the asset is missing
/// or unusable, not loading.
///
/// **What makes that "never" true is the shape of the code, not this sentence** (#821 review round
/// 2, B4). This handler used to take permission from the zone-assets verdict and then read the grid
/// out of the separate `shared_collision` slot behind an `if let Some(col)` with **no `else`** — so
/// a `None` there returned `200 []` having consulted no region map at all, and nothing coupled the
/// two slots. It now gets verdict AND grid from one call
/// ([`eqoxide_zone_geometry::zone_assets::usable_collision`]), whose `Ok` arm carries the `Arc<Collision>` the
/// `Ready` state owns. There is no longer a branch that can reach the response builder without a
/// grid, so every `[]` this endpoint emits has been through `Collision::zone_line_indices()`.
///
/// `gated` (#713 item 3) is `true` when this exit's destination is unadvertised **and** this ZONE's
/// #679/#683 unresolved-cross gate refuses server-resolved crossings. It is a **zone-level** verdict:
/// **the player's position is not an input** (see the comment where it is computed), so it does not
/// mean "cannot be crossed from where you are standing" — an earlier revision of this sentence said
/// that, and it described a field that does not exist (#713 review round 2, B3).
///
/// `gated: false` is therefore **not a promise the auto-cross will fire** when you stand there. The
/// #713 attempt bound is stand-scoped and this field never consults it, so once the bound has been
/// reached, standing on a `gated: false` exit does not cross either. Cross-check `zone_cross_stopped`
/// on `/v1/observe/debug` before concluding that a non-crossing exit is a data problem.
///
/// **503 `zone_assets_not_ready` while the zone's assets are still loading (#579)** — the exits come
/// out of the collision grid, so before it is built this returned a confident `[]`, i.e. "this zone
/// has no exits at all". That is a falsehood an agent cannot detect; an explicit refusal is the
/// honest answer. Poll `/v1/observe/debug` → `zone_assets` until it reads `ready`.
async fn get_zone_exits(s: OpState, _req: Req) -> Reply {
    // The #579 gate AND the grid it vouches for, from ONE read of ONE slot (#821 review round 2,
    // B4). Deliberately not `zone_assets_not_ready(&s)` followed by `s.shared_collision`: those are
    // two slots, and the fall-through when the second was `None` answered `[]` — see this
    // function's doc comment.
    let st = eqoxide_zone_geometry::zone_assets::lock_state(&s.zone_assets).clone();
    let player_zone = s.player().zone;
    let col = match eqoxide_zone_geometry::zone_assets::usable_collision(&st, &player_zone) {
        Ok(col) => col.clone(),
        Err(verdict) => return zone_assets_refusal(verdict, &st, &player_zone),
    };
    let snapshot_age_ms = s.health().snapshot_age_ms;
    let player = s.player();
    // `pos_up` is already the FOOT datum (#522), the same datum as the collision geometry
    // (zone-line regions) it's tested against — no conversion needed.
    let pos = [player.pos_east, player.pos_north, player.pos_up];
    // index -> destination zone_id, from the advertised entrance list. `zone_id == 0` entries are
    // the "server resolves from position" SENTINEL, not a destination (the same filter
    // `resolve_cross_destination` applies) — dropping them makes such an exit report
    // `"zone_id": null` (destination honestly unknown, #683) instead of a nonexistent "zone 0".
    // #713 item 3 — the UNRESOLVED-EXIT GATE VERDICT, reported rather than re-derived.
    //
    // An exit with `zone_id: null` is one whose baked WLD index matches no advertised entrance. The
    // client can still cross it, by asking the server to resolve the destination from position
    // (the #683 fallback) — but ONLY when `classify_unresolved_cross` allows it in this zone. Where
    // it does not, standing on that exit does nothing, and before this the agent learned that from
    // a message-log line AFTER it had already walked there. `gated: true` says it beforehand.
    //
    // This calls the SAME function the net thread's `cross_unresolved` calls, from the crate both
    // sides depend on — deliberately not a second copy of the rule, because a `gated` flag that
    // drifted from the gate would be a confident falsehood the agent has no way to check (#713 moved
    // the function into `eqoxide-core` for exactly this). Nothing here changes what the gate
    // decides; the same-zone-pad question (#543/#266/#582) is untouched.
    //
    // NOT MEASURED (reasoned): the two inputs come from two places — `zone_points` from the IPC
    // slot the net thread syncs, `zone_id` from the `ArcSwap` GameState snapshot — so a read that
    // lands between those two publishes can pair one zone's points with the other's id, and the
    // rule is "any reported verdict computed from inputs sampled at two instants can straddle an
    // event that changes both". The instance here is a zone change. I did not observe it: the
    // window is one publish wide and this endpoint already refuses with 503 until the new zone's
    // assets are ready, which lags a zone change by seconds — but that is an argument about
    // likelihood, not an impossibility proof, so it is disclosed rather than claimed closed.
    let zone_points = s.world.zone_points.lock().unwrap().clone();
    // FIRST advertised entry wins per index, matching `ActionLoop::resolve_cross_destination`'s
    // `.find()` over the same filtered list (#713 review round 2, N1). A `.collect()` into a
    // `HashMap` takes the LAST, so where a zone advertises two points under one iterator the
    // reported `zone_id` named a destination the client would not actually take. `gated` never
    // depended on which one won (it only asks `is_none()`), but the `zone_id` printed beside it did.
    let mut dest_of: std::collections::HashMap<i32, u16> = std::collections::HashMap::new();
    for zp in zone_points.iter().filter(|zp| zp.zone_id != 0) {
        dest_of.entry(zp.iterator as i32).or_insert(zp.zone_id);
    }
    let unresolved_gated = eqoxide_core::zone_cross::classify_unresolved_cross(
        &zone_points, s.game_state.load().world.zone_id,
    ) == eqoxide_core::zone_cross::UnresolvedCross::Ignore;
    // #803: `[]` here must only ever mean "this zone's region map loaded and has no zone-line
    // regions". When the `.wtr` did NOT load, the exits are UNKNOWN, and publishing the empty
    // list would be a confident falsehood the agent cannot detect — and the falsehood is
    // specifically "there is no way out of this zone". Refuse, and name the cause.
    let indices = match col.zone_line_indices() {
        Ok(ix) => ix,
        Err(absent) => return region_data_unavailable(&absent),
    };
    let mut exits = Vec::new();
    for index in indices {
        let location = col
            .find_zone_line_near(Some(index), pos)
            .map(|(_, p)| serde_json::json!([p[0], p[1], p[2]]));
        // An exit with a known destination is crossed by `perform_cross`, which never consults
        // the unresolved gate — so the gate can only ever close a `zone_id: null` exit.
        let gated = !dest_of.contains_key(&index) && unresolved_gated;
        exits.push(serde_json::json!({
            "index": index,
            "zone_id": dest_of.get(&index),
            "location": location,
            "gated": gated,
        }));
    }
    Reply::ok(serde_json::json!({ "zone_exits": exits, "snapshot_age_ms": snapshot_age_ms }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{call, empty_state, empty_state_wall_clock, set_gs};

    async fn spells_json(state: OpState) -> serde_json::Value {
        call(&state, "GET /v1/observe/spells", Req::default()).await.body
    }

    // ── #726 review ROUND 2, finding 1: the primary is one sync, not the process ────────────────
    //
    // The registry made every field true of the sync it names. What was still false was the
    // GUIDANCE — carried in `semantics`, so an agent reads it — that the top-level copy of syncs[0]
    // answered "is the load I am waiting on alive?" and that a caller "need not iterate". Both of
    // the sequences below are ones the client really produces, and both are the reviewer's.

    // ── #731: the asset-server login is inside the observed window ──────────────────────────────

    /// #760/B1 — the rule about past-dated net-health stamps, as a TEST rather than as a doc comment.
    ///
    /// A stamp written `ago(N)` comes from the wall clock; `health()` reads it back against
    /// `NetHealth::clock`. On a frozen fixture those are different clocks, so the age is
    /// `N − (time since the fixture was built)` — it SHRINKS with machine load, silently, and any
    /// assertion needing it to stay above a bound has a margin that load eats. That is #760's own
    /// failure mode one level down, and it is what review found in
    /// `debug_reports_world_unresponsive_when_a_probe_goes_unanswered_while_the_link_acks`.
    ///
    /// A prose rule did not prevent it (the rule named only "an age that must move", not "an age
    /// that must exceed a bound"), so this scans the source instead.
    ///
    /// # What this matches
    ///
    /// The unit is a **statement**, not a physical line: comments and literals are removed by a
    /// character scanner, and what is left is cut at `;`, `{` and `}`. A statement offends when it
    /// mentions one of the eight [`STAMP_FIELDS`] *and* past-dates through a receiver that is not
    /// the clock reading it back (see `receiver_is_the_reading_clock`). Flagged forms: `ago(`, and
    /// `now()` followed by `-` or `.checked_sub`.
    ///
    /// # Why the `now()` spelling is matched, stated honestly
    ///
    /// It was added after a sweep for the CONCEPT rather than the field name found past-dated
    /// `NetHealth` stamps a bare `ago(` grep does not see. That sweep is worth keeping as a
    /// methodological result — **grep the concept, not the spelling** — but its sites do NOT
    /// justify the widening on their own terms, and an earlier version of this comment implied they
    /// did. Two corrections:
    ///
    /// 1. There are **seven**, not the three previously published — re-enumerated by sweeping
    ///    `eqoxide-net` for all eight [`STAMP_FIELDS`] against all three past-dating spellings:
    ///    `crates/eqoxide-net/src/gameplay.rs:1569, 1831, 1835, 1839` and
    ///    `crates/eqoxide-net/src/transport.rs:1885, 1914, 2679`.
    /// 2. Not one of them is reachable by this test — they are all in `eqoxide-net`, which is not in
    ///    the scanned set below and cannot be (`include_str!` reaches only this crate's own files).
    ///    And if the scan were extended to them they would all be **false positives**: each site's
    ///    `NetHealth` is built by `Default` (checked at every one of the seven), so its stamps are
    ///    past-dated from the wall clock and read back by the wall clock — the same clock — which is
    ///    exactly the property this test exists to require.
    ///
    /// What the widening actually buys is confined to the four files below: the spelling is now
    /// caught *there* if it ever appears, where today it does not.
    ///
    /// # How to probe this guard, and why it is written down
    ///
    /// A source scanner has TWO failure modes, and only one of them is obvious. It can fail to
    /// recognise a bad *shape* — and it can silently fail to *arrive* at the code in the first
    /// place. Round 3 of #760 had a twelve-cell shape table that was entirely void, because every
    /// probe was placed inside this test's own body, which happened to be the only region of
    /// `observe.rs` the scanner still reached: a `/*` inside a route glob in a doc comment on line 1
    /// latched the block-comment state and blinded **87% of the corpus**, and the probes were sitting
    /// in the window that this test's own `/* */`-containing doc had re-opened. The instrument was
    /// measured only where it worked.
    ///
    /// So: **any probe of this guard must be placed at several depths in EVERY scanned file** — near
    /// the top, mid-file, and near the end — and must be reported with two controls:
    /// 1. a **positive control**, proving the injected shape is detectable at all, and
    /// 2. a **reach control**, proving the scanner actually arrives at that location.
    ///
    /// Both controls are now permanent rather than ad-hoc: `scan_ended_clean` fails loudly if a file
    /// ends mid-comment or mid-string (a real source file never does), and every scanned file ends
    /// with a `GUARD_REACH_SENTINEL_*` const that this test asserts it can see.
    ///
    /// # What it still cannot see
    ///
    /// **Aliasing through a local.** `let s = ago(15); h.last_probe_sent = Some(s);` is two
    /// statements, neither carrying both halves, so it lands unflagged. Closing that needs dataflow,
    /// not text, and no amount of scanner care will do it.
    ///
    /// **A value produced inside a nested block or a `match` arm.** The cut at `{` and `}` puts
    /// `h.last_tick =` and the `ago(30)` in `h.last_tick = { ago(30) };` into different statements,
    /// so neither carries both halves. Same for a `match` that yields the stamp. Measured, with the
    /// controls this doc demands: both spellings were injected at three depths in all four scanned
    /// files (twelve locations) alongside a plain `now() -` positive control in the same function.
    /// All twelve controls were flagged — so the scanner reached every location and the shape is
    /// detectable there — and not one of the twenty-four brace/`match` forms was.
    ///
    /// Two lesser gaps: files outside the four named below, and a ninth `Instant` field added to
    /// `NetHealth` and to `health()` but not to [`STAMP_FIELDS`] — that list is hand-maintained with
    /// no cross-check against the struct.
    ///
    /// The rule is a SPELLING rule and errs toward flagging: it recognises the receivers `c.` and
    /// `….clock.`, so a *correct* stamp taken from a binding named anything else is flagged too, and
    /// so is a correct `self.now().checked_sub(…)` — which is the body of `HealthClock::ago` itself.
    /// That method lives in `eqoxide-ipc` and is not scanned, but the point stands: a flagged line is
    /// not automatically a wrong line.
    #[test]
    fn no_past_dated_net_health_stamp_is_taken_from_a_clock_other_than_the_one_that_reads_it() {
        /// Exactly the `NetHealth` fields `OpState::health()` turns into an age. Adding a stamp
        /// field to that projection without adding it here leaves the new field unguarded.
        const STAMP_FIELDS: [&str; 8] = [
            "last_datagram", "last_packet", "last_tick",
            "last_probe_sent", "last_probe_reply", "first_unanswered_probe_sent",
            "last_send_pressure_at", "last_send_error_at",
        ];

        /// Code text per source line, with comments and literal contents removed, plus whether the
        /// scan **ended clean** — i.e. not stranded inside a block comment or a string.
        ///
        /// The line-at-a-time version this replaces looked for `/*` before it truncated at `//`, so
        /// a `/*` occurring *inside* a line or doc comment latched the block state permanently. That
        /// is not a corner case: two of the four scanned files open with a `//!` doc line containing
        /// a route glob (`/v1/observe/*`), which blinded the scanner from line 1. Literals are
        /// skipped for the same family of reason — a `//` or `/*` inside a string is not a comment.
        fn strip_to_code(src: &str) -> (Vec<String>, bool) {
            #[derive(PartialEq, Clone, Copy)]
            enum S { Code, Line, Block(u32), Str, Raw(usize), Chr }
            let b: Vec<char> = src.chars().collect();
            let at = |k: usize| -> char { b.get(k).copied().unwrap_or('\0') };
            let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
            let mut out = vec![String::new()];
            let (mut st, mut i) = (S::Code, 0usize);
            while i < b.len() {
                let c = b[i];
                if c == '\n' {
                    if st == S::Line { st = S::Code; }
                    out.push(String::new());
                    i += 1;
                    continue;
                }
                match st {
                    S::Line => i += 1,
                    S::Block(d) => {
                        if c == '*' && at(i + 1) == '/' {
                            st = if d == 1 { S::Code } else { S::Block(d - 1) };
                            i += 2;
                        } else if c == '/' && at(i + 1) == '*' {
                            st = S::Block(d + 1); // Rust block comments nest.
                            i += 2;
                        } else { i += 1; }
                    }
                    S::Str => {
                        if c == '\\' { i += 2; } else { if c == '"' { st = S::Code; } i += 1; }
                    }
                    S::Raw(h) => {
                        if c == '"' && (1..=h).all(|k| at(i + k) == '#') { st = S::Code; i += h + 1; }
                        else { i += 1; }
                    }
                    S::Chr => {
                        if c == '\\' { i += 2; } else { if c == '\'' { st = S::Code; } i += 1; }
                    }
                    S::Code => {
                        let prev_is_ident = i > 0 && ident(b[i - 1]);
                        // NB the branch ORDER here is not what fixes C1 — `//` and `/*` differ in
                        // their second character, so they can never both match. The fix is that
                        // `S::Line` skips to end-of-line, so a `/*` sitting inside a line or doc
                        // comment is never read as an opener at all. The version this replaced
                        // stripped block comments in a whole-line pass BEFORE truncating at `//`,
                        // which is exactly how a route glob in a `//!` line latched it forever.
                        if c == '/' && at(i + 1) == '/' { st = S::Line; i += 2; }
                        else if c == '/' && at(i + 1) == '*' { st = S::Block(1); i += 2; }
                        else if c == '"' { st = S::Str; i += 1; }
                        else if (c == 'r' || (c == 'b' && at(i + 1) == 'r')) && !prev_is_ident && {
                            let j = i + if c == 'b' { 2 } else { 1 };
                            let mut h = 0;
                            while at(j + h) == '#' { h += 1; }
                            at(j + h) == '"'
                        } {
                            let j = i + if c == 'b' { 2 } else { 1 };
                            let mut h = 0;
                            while at(j + h) == '#' { h += 1; }
                            st = S::Raw(h);
                            i = j + h + 1;
                        }
                        // `'` is a char literal only if it closes; otherwise it is a lifetime.
                        else if c == '\'' && (at(i + 1) == '\\' || at(i + 2) == '\'') { st = S::Chr; i += 1; }
                        else { out.last_mut().unwrap().push(c); i += 1; }
                    }
                }
            }
            (out, matches!(st, S::Code | S::Line))
        }

        /// Statements, each kept as its (1-based line, text) fragments so an offender is still
        /// reported at the exact line that names the field.
        fn statements(code: &[String]) -> Vec<Vec<(usize, String)>> {
            let mut out: Vec<Vec<(usize, String)>> = Vec::new();
            let mut cur: Vec<(usize, String)> = Vec::new();
            for (i, line) in code.iter().enumerate() {
                // Cut at statement AND block boundaries, so unrelated code never merges into one
                // "statement" and invents a co-occurrence that fires falsely.
                for (n, piece) in line.split([';', '{', '}']).enumerate() {
                    if n > 0 && !cur.is_empty() { out.push(std::mem::take(&mut cur)); }
                    if !piece.trim().is_empty() { cur.push((i + 1, piece.to_string())); }
                }
            }
            if !cur.is_empty() { out.push(cur); }
            out
        }

        /// Is this call's RECEIVER the health clock that will read the stamp back?
        ///
        /// Exempt spellings are `c.` (the convention every call site uses, `let c = h.clock;`) and
        /// `….clock.`. Everything else is flagged — a free `ago(`, and equally `HealthClock::WALL`
        /// or any other receiver. Exempting *any* `.ago(` regardless of receiver was review's
        /// sharpest miss: `wall.ago(30)` stamped into a pinned fixture is #760 exactly.
        fn receiver_is_the_reading_clock(prefix: &str) -> bool {
            if prefix.ends_with(".clock.") { return true; }
            let Some(rest) = prefix.strip_suffix("c.") else { return false };
            !rest.chars().next_back().is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
        }

        // Naming the sentinels as VALUES, not just as strings to grep for, is what makes deleting
        // one a compile error rather than a silently weakened reach control.
        let _sentinels_must_exist = (
            super::GUARD_REACH_SENTINEL_OBSERVE,
            crate::GUARD_REACH_SENTINEL_LIB,
            crate::combat::GUARD_REACH_SENTINEL_COMBAT,
            crate::testkit::GUARD_REACH_SENTINEL_TESTKIT,
        );
        let sources = [
            ("observe.rs",  include_str!("observe.rs"),  "GUARD_REACH_SENTINEL_OBSERVE"),
            ("lib.rs",      include_str!("lib.rs"),      "GUARD_REACH_SENTINEL_LIB"),
            ("combat.rs",   include_str!("combat.rs"),   "GUARD_REACH_SENTINEL_COMBAT"),
            ("testkit.rs",  include_str!("testkit.rs"),  "GUARD_REACH_SENTINEL_TESTKIT"),
        ];
        let mut offenders = Vec::new();
        for (file, src, sentinel) in sources {
            let (code, ended_clean) = strip_to_code(src);
            // REACH CONTROL 1 — loud failure instead of a silent early stop. A real source file
            // never ends inside a block comment or a string, so this cannot false-positive.
            assert!(ended_clean,
                "{file}: the scanner ended stranded inside a comment or string literal, so it stopped \
                 seeing code partway through and every 'clean' result below is worthless (#760/C1)");
            let stmts = statements(&code);
            // REACH CONTROL 2 — the sentinel is the LAST item in each scanned file, so seeing it
            // proves the scan arrived at the end rather than merely ending without complaint.
            assert!(stmts.iter().flatten().any(|(_, t)| t.contains(sentinel)),
                "{file}: the scanner never reached `{sentinel}` at the end of the file, so it covered \
                 only a prefix of it — a clean scan of a corpus that was never read (#760/C1)");
            for stmt in stmts {
                let joined: String = stmt.iter().map(|(_, t)| t.as_str()).collect();
                let Some((line, _)) = stmt.iter().find(|(_, t)| STAMP_FIELDS.iter().any(|f| t.contains(f)))
                else { continue };
                let bad_ago = joined.match_indices("ago(")
                    .any(|(k, _)| !receiver_is_the_reading_clock(&joined[..k]));
                // `now()` is fine on its own — it dates a stamp to the present, which saturates to
                // age 0 against either clock. It is past-dating, and so a #760 shape, only when it
                // is followed by a subtraction. The receiver test applies here too: `c.now() - d` is
                // the correct clock and must NOT be flagged (review finding C2).
                let bad_now = joined.match_indices("now()").any(|(k, _)| {
                    if receiver_is_the_reading_clock(&joined[..k]) { return false; }
                    let tail = joined[k + "now()".len()..].trim_start();
                    tail.starts_with('-') || tail.starts_with(".checked_sub")
                });
                if !(bad_ago || bad_now) { continue; }
                offenders.push(format!("{file}:{line}: {}", joined.trim()));
            }
        }
        assert!(offenders.is_empty(),
            "these past-date a net-health stamp through a receiver that is not the clock `health()` \
             reads it back against — use `let c = h.clock; … = c.ago(N)` (#760). A flagged line is \
             not automatically wrong: the receiver test is a spelling rule, so a correct stamp under \
             a binding named other than `c` lands here too.\n{}",
            offenders.join("\n"));
    }

    /// #1127 — `GET /v1/observe/spells` carries each memorized gem's mana cost / cast time /
    /// recast time, straight from the loaded spell table, so an agent can tell whether casting a
    /// gem is even possible right now without guessing at EQ's own numbers.
    #[tokio::test]
    async fn spells_endpoint_reports_mana_cost_cast_time_and_recast_time_1127() {
        let db = eqoxide_core::spells::SpellDb::parse_str(&{
            let mut row = vec!["0"; 150];
            row[0] = "300"; row[1] = "Lightning Bolt";
            row[13] = "3000"; row[15] = "2500"; row[19] = "75";
            row.join("^")
        });
        let state = OpState { spells: std::sync::Arc::new(db), ..crate::testkit::empty_state() };
        set_gs(&state, |gs| {
            gs.mem_spells[0] = 300;
        });
        let v = spells_json(state).await;
        let gem0 = &v["gems"][0];
        assert_eq!(gem0["spell_id"], 300);
        assert_eq!(gem0["mana_cost"], 75);
        assert_eq!(gem0["cast_time_ms"], 3000);
        assert_eq!(gem0["recast_time_ms"], 2500);
    }

    /// An empty gem, and a memorized id our spell table has no row for, must both report `null`
    /// for the cost/cooldown fields — never a fabricated `0`, which is itself a meaningful cost.
    #[tokio::test]
    async fn spells_endpoint_nulls_cost_fields_for_empty_and_unknown_gems_1127() {
        let state = crate::testkit::empty_state();
        set_gs(&state, |gs| {
            gs.mem_spells[0] = 0;               // empty gem
            gs.mem_spells[1] = 40404;            // memorized, but not in the (empty) spell table
        });
        let v = spells_json(state).await;
        for gem in [&v["gems"][0], &v["gems"][1]] {
            assert!(gem["mana_cost"].is_null());
            assert!(gem["cast_time_ms"].is_null());
            assert!(gem["recast_time_ms"].is_null());
        }
        assert!(v["gems"][0]["spell_id"].is_null(), "empty gem's spell_id must stay null");
        assert_eq!(v["gems"][1]["spell_id"], 40404, "an unresolved but memorized id is still reported");
    }

    fn push_message(state: &OpState, kind: &str, text: &str) {
        state.chat.messages.lock().unwrap().push(MessageEntry {
            kind: kind.to_string(), text: text.to_string(), keywords: vec![], item_links: vec![],
        });
    }

    /// `GET /v1/observe<uri>`, with the query string split into the call's `query` arguments.
    async fn get(state: OpState, uri: &str) -> Reply {
        let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
        let query: serde_json::Map<String, serde_json::Value> = query.split('&').filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap();
                (k.to_string(), serde_json::Value::String(v.to_string()))
            })
            .collect();
        call(&state, &format!("GET /v1/observe{path}"), Req::new(query, None)).await
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
        assert_eq!(resp.status, StatusCode::BAD_REQUEST,
            "a typo'd/unknown query param must be an explicit failure, not a silent 200 over the whole log");
        let msg = resp.body.as_str().unwrap_or_default().to_string();
        assert!(msg.contains("kidn"), "the 400 body should name the offending field, got: {msg}");
    }

    /// The happy path must not regress: a correctly-spelled `kind` still filters normally.
    #[tokio::test]
    async fn valid_kind_param_still_works() {
        let state = empty_state();
        push_message(&state, "npc", "Well met, traveler.");
        push_message(&state, "chat", "someone: hi");
        let resp = get(state, "/messages?kind=npc").await;
        assert_eq!(resp.status, StatusCode::OK);
        let v = resp.body;
        assert_eq!(v["count"], 1);
        assert_eq!(v["messages"][0]["kind"], "npc");
    }

    /// `who` needs a simulated reply from the model layer (the who-roster oneshot) before it
    /// resolves, so it is pinned separately.
    #[tokio::test]
    async fn who_carries_snapshot_age_ms() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_tick = std::time::Instant::now();
        let command = state.command.clone();
        let mut handle = tokio::spawn({
            let state = state.clone();
            async move { get(state, "/who").await }
        });
        // Play the model layer's part: drain the request and reply, exactly like
        // `ActionLoop::drain_who_friends` would once the server's OP_WhoAllResponse lands.
        //
        // #717: race the poll against the handler's own JoinHandle. A naive unbounded poll loop
        // here would hang forever, not fail, if `get_who` ever returned early without registering
        // a who_req.
        let tx = tokio::select! {
            tx = async {
                loop {
                    if let Some(tx) = command.take_who_req() { return tx; }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            } => tx,
            res = &mut handle => {
                let resp = res.expect("handler task panicked");
                panic!(
                    "expected /who to reach the who-request hand-off, but the handler returned \
                     early with status {} instead", resp.status
                );
            }
        };
        tx.send(vec![]).unwrap();
        let resp = handle.await.unwrap();
        assert_eq!(resp.status, StatusCode::OK);
        let v = resp.body;
        assert!(v.get("snapshot_age_ms").is_some_and(|x| !x.is_null()),
            "GET /who must carry a PRESENT, non-null snapshot_age_ms key, got body: {v}");
        assert!(v["snapshot_age_ms"].as_u64().unwrap() < 1_000);
    }

    /// The age must reflect REALITY: it must keep CLIMBING for a source that has gone stale, driven
    /// by a read-time `now - last_tick`, never by anything the (in this test, deliberately silent)
    /// publisher itself updates. Mirrors `last_packet_age_advances_between_reads_with_no_publisher_
    /// running` above, over one of the newly-added JSON fields instead of the pre-existing `/debug`
    /// one.
    #[tokio::test]
    async fn snapshot_age_ms_climbs_across_reads_of_a_frozen_source() {
        let state = empty_state_wall_clock(); // #760: this test's subject IS the wall clock
        { let mut h = state.net_health.lock().unwrap(); let c = h.clock; h.last_tick = c.ago(5); }
        let first = get(state.clone(), "/messages").await.body["snapshot_age_ms"].as_u64().unwrap();
        // Nothing ticks, nothing republishes — just time passing, exactly like a wedged net thread.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let second = get(state, "/messages").await.body["snapshot_age_ms"].as_u64().unwrap();
        assert!(second > first,
            "snapshot_age_ms froze at {first} across two reads of a stale source — it must be \
             derived at READ time (#343/#646), not cached or driven by the dead publisher");
    }

    /// Same climb, over a roster op (`/doors`), whose list rides under its own key.
    #[tokio::test]
    async fn snapshot_age_climbs_across_reads_of_a_frozen_roster() {
        let state = empty_state_wall_clock(); // #760: this test's subject IS the wall clock
        { let mut h = state.net_health.lock().unwrap(); let c = h.clock; h.last_tick = c.ago(5); }
        let first = get(state.clone(), "/doors").await.body["snapshot_age_ms"].as_u64().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let second = get(state, "/doors").await.body["snapshot_age_ms"].as_u64().unwrap();
        assert!(second > first,
            "snapshot_age_ms froze at {first} across two reads of a stale source — it must be \
             derived at READ time (#343/#646), not cached or driven by the dead publisher");
    }

}

/// #579 — the zone-assets loading gate. A mid-load observation must be an explicit *pending*,
/// never a confident *empty world*.
#[cfg(test)]
mod zone_asset_gate_tests {
    use super::*;
    use crate::testkit::{call, empty_state, set_gs};
    use eqoxide_zone_geometry::zone_assets::ZoneAssetState;

    /// The zone `ZoneAssetState::test_ready()` is built for. The gate compares the loaded zone
    /// against the PLAYER's zone (#595 review F1), so a fixture that only sets one of the two is a
    /// `stale`/`unknown_zone` state, not a ready one.
    const FIXTURE_ZONE: &str = "testfixture";

    /// A state whose assets are loaded AND belong to the zone the character is standing in.
    ///
    /// The region map is `Ok` (a loaded map with no zone-line regions) rather than
    /// `test_ready()`'s never-attached grid, because a `ready` zone in production has ALWAYS had
    /// `set_region_data` called on it — with an `Ok` or with the loader's `Err`, never with nothing
    /// (#821 review round 2, B4). Before that round `/zone_exits` read the *other* slot,
    /// `shared_collision`, which `empty_state()` leaves `None`, so this fixture's grid was never
    /// consulted at all and the endpoint answered `[]` off a fall-through. It is consulted now.
    fn ready_state() -> OpState {
        let s = empty_state();
        set_gs(&s, |gs| gs.world.zone_name = FIXTURE_ZONE.to_string());
        *eqoxide_zone_geometry::zone_assets::lock_state(&s.zone_assets) =
            ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
                eqoxide_core::region_map::RegionMap::flat_below(-10.0))));
        s
    }

    /// A state in the F1 window: the character is in `qeynos`, but the loaded assets (collision
    /// grid and all) are still the previous zone's.
    fn stale_state() -> OpState {
        let s = empty_state();
        set_gs(&s, |gs| gs.world.zone_name = "qeynos".to_string());
        *eqoxide_zone_geometry::zone_assets::lock_state(&s.zone_assets) = ZoneAssetState::test_ready();
        s
    }

    fn with_state(st: ZoneAssetState) -> OpState {
        let s = empty_state();
        set_gs(&s, |gs| gs.world.zone_name = FIXTURE_ZONE.to_string());
        *eqoxide_zone_geometry::zone_assets::lock_state(&s.zone_assets) = st;
        s
    }

    async fn get(state: OpState, op: &str) -> (StatusCode, serde_json::Value) {
        let resp = call(&state, &format!("GET /v1/observe{op}"), Req::default()).await;
        (resp.status, resp.body)
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
        assert!(j["zone_exits"].is_array(), "a ready zone must still get the exits list: {j}");
    }

    // ─────────── #595 review F1: the wrong-world window ───────────

    /// The wrong-zone window must be disclosed, exactly as the loading window is —
    /// another zone's collision grid cannot answer queries.
    #[tokio::test]
    async fn the_wrong_zone_window_is_disclosed() {
        let s = stale_state();
        let st = eqoxide_zone_geometry::zone_assets::lock_state(&s.zone_assets).clone();
        let why = eqoxide_zone_geometry::zone_assets::usability(&st, &s.player().zone);
        assert_eq!(why.map(|w| w.as_str()), Some("zone_assets_stale_for_previous_zone"));
    }
}

/// **#713 — the zone-cross observables at the HTTP boundary.**
///
/// These are the agent-facing half of #713's three items. Each pins that a fact the net thread
/// decided actually REACHES something an agent can poll: the #409 failure mode is a value that is
/// computed correctly, stored correctly, and published nowhere.
#[cfg(test)]
mod zone_cross_observables_713 {
    use super::*;
    use crate::testkit::{call, empty_state, set_gs};
    use eqoxide_core::game_state::ZonePoint;
    use eqoxide_zone_geometry::zone_assets::ZoneAssetState;

    const FIXTURE_ZONE: &str = "testfixture";
    /// The baked region index the fixture's DRNTP box carries.
    const IDX: i32 = 7;
    const HERE: u16 = 54;
    const OTHER: u16 = 181;

    fn zp(iterator: u32, zone_id: u16) -> ZonePoint {
        ZonePoint { iterator, server_x: 1.0, server_y: 2.0, server_z: 3.0, heading: 0.0, zone_id }
    }

    async fn get(state: OpState, op: &str) -> (StatusCode, serde_json::Value) {
        let resp = call(&state, &format!("GET /v1/observe{op}"), Req::default()).await;
        (resp.status, resp.body)
    }

    /// A ready state whose collision grid carries ONE zone-line region (index [`IDX`]), plus the
    /// advertised entrance list the gate reads.
    fn exits_state(points: Vec<ZonePoint>) -> OpState {
        let s = empty_state();
        set_gs(&s, |gs| {
            gs.world.zone_name = FIXTURE_ZONE.to_string();
            gs.world.zone_id = HERE;
        });
        let ready = ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, IDX),
        )));
        *eqoxide_zone_geometry::zone_assets::lock_state(&s.zone_assets) = ready;
        *s.world.zone_points.lock().unwrap() = points;
        s
    }

    /// **#713 item 3: an exit that CANNOT be auto-crossed says so BEFORE the agent walks there.**
    ///
    /// A `zone_id: null` exit in a zone where the #679/#683 gate is closed is inert: standing on it
    /// does nothing. The only signal used to be a message-log line emitted once the character had
    /// already arrived — an answer to a question the agent could only ask by making the trip. The
    /// `gated` flag is that verdict, reported from the SAME function the net thread acts on.
    ///
    /// Note what is NOT claimed: `gated: false` is not a promise that crossing will succeed (the
    /// server still decides), and this changes nothing about what the gate decides.
    ///
    /// **Mutation checks:** hardcode `gated: false` → the first two cases go RED; hardcode `true` →
    /// the other two go RED; drop the `dest_of.get(&index).is_none()` conjunct → the
    /// `advertised_exit_in_a_gated_zone` case goes RED (an exit with a known destination is crossed
    /// by `perform_cross`, which never consults this gate).
    #[tokio::test]
    async fn a_zone_exit_that_cannot_be_auto_crossed_is_marked_gated_713() {
        // (label, advertised points, expected `gated` for the region-IDX exit)
        let cases: Vec<(&str, Vec<ZonePoint>, bool)> = vec![
            // No server adverts have arrived: every index is unresolved, so the fallback is shut.
            ("no_adverts_yet", vec![], true),
            // A same-zone pad is advertised here → the fallback refuses in this zone (#679).
            ("same_zone_pad_advertised", vec![zp(9, HERE)], true),
            // Cross-zone adverts only (the qrg shape) → the fallback is open, the exit IS crossable.
            ("cross_zone_adverts_only", vec![zp(99, OTHER)], false),
            // The exit's OWN index is advertised → it has a known destination and is crossed by the
            // resolved path, which never consults the unresolved gate — even though the gate is shut.
            ("advertised_exit_in_a_gated_zone", vec![zp(IDX as u32, OTHER), zp(5, HERE)], false),
        ];
        for (label, points, want_gated) in cases {
            let advertised_dest = points.iter().any(|p| p.iterator as i32 == IDX);
            let (code, j) = get(exits_state(points), "/zone_exits").await;
            assert_eq!(code, StatusCode::OK, "{label}");
            let exits = j["zone_exits"].as_array().unwrap_or_else(|| panic!("{label}: expected a list, got {j}"));
            let exit = exits.iter().find(|e| e["index"] == IDX)
                .unwrap_or_else(|| panic!("{label}: the fixture bakes exactly one exit: {j}"));
            assert_eq!(exit["gated"], want_gated,
                "{label}: `gated` must let an agent see BEFORE walking there whether this exit can \
                 be auto-crossed at all");
            assert_eq!(exit["zone_id"].is_null(), !advertised_dest,
                "{label}: the pre-existing honest-unknown destination is unchanged");
        }
    }

    /// **#713 review round 2, N1: when a zone advertises TWO points under one iterator, the
    /// `zone_id` reported beside `gated` must name the one the client would actually take.**
    ///
    /// `zone_exits` built its index→destination map with `.collect()` into a `HashMap`, which keeps
    /// the LAST duplicate; the net thread's `ActionLoop::resolve_cross_destination` uses `.find()`,
    /// which takes the FIRST. `gated` was never affected (it only asks `is_none()`), but the
    /// destination printed next to it could name a zone the client would not go to — a confident
    /// wrong answer with no way for the agent to check it. Both sides now take the first match.
    ///
    /// **Mutation check:** restore the `.collect()` (or make the loop overwrite instead of
    /// `or_insert`) → this goes RED.
    #[tokio::test]
    async fn duplicate_advertised_indices_report_the_destination_the_client_would_take_713() {
        // Two adverts under the SAME iterator as the baked region. `resolve_cross_destination`
        // takes the first (`OTHER`); the observable must not disagree with it.
        let s = exits_state(vec![zp(IDX as u32, OTHER), zp(IDX as u32, OTHER + 1)]);
        let (code, j) = get(s, "/zone_exits").await;
        assert_eq!(code, StatusCode::OK);
        let exit = j["zone_exits"].as_array().unwrap().iter().find(|e| e["index"] == IDX)
            .unwrap_or_else(|| panic!("the fixture bakes exactly one exit: {j}"));
        assert_eq!(exit["zone_id"], OTHER,
            "the reported destination must be the FIRST advertised match — the one \
             `resolve_cross_destination`'s `.find()` picks. Reporting the last one names a zone the \
             client would never take us to: {j}");
    }

}
/// **#803 — `/v1/observe/zone_exits` must not publish a failed file read as "this zone has no way
/// out".**
///
/// The endpoint answers out of the zone's region map (`maps/water/<zone>.wtr`), whose DRNTP
/// zone-line regions ARE the exits. When that file was missing, truncated, of an unsupported
/// version, or simply never attached, the loader's failure was discarded into a `None`,
/// `Collision::zone_line_indices()` `.unwrap_or_default()`-ed it to an empty vec, and this endpoint
/// served `[]` with **200 OK** — byte-identical to the true, common answer for a zone that
/// genuinely has no zone lines. Exits are the only way out of a zone, so the agent concluded it was
/// sealed in, from a success response it had no way to doubt.
///
/// The two halves below are deliberately separate tests with different names: one pins that the
/// honest `[]` is STILL served (so the fix cannot degenerate into "refuse whenever the list is
/// empty"), the other that a load failure is a 503 naming its cause. Neither alone is the property.
#[cfg(test)]
mod zone_exits_never_publishes_a_failed_read_as_empty_803 {
    use super::*;
    use crate::testkit::{call, empty_state, set_gs};
    use eqoxide_core::region_map::{RegionLoadError, RegionMap};
    use eqoxide_zone_geometry::zone_assets::ZoneAssetState;

    async fn get(state: OpState, op: &str) -> (StatusCode, serde_json::Value) {
        let resp = call(&state, &format!("GET /v1/observe{op}"), Req::default()).await;
        (resp.status, resp.body)
    }

    /// A `Ready` zone — terrain loaded, so the #579 gate is open and cannot be what refuses — whose
    /// grid carries the given region-data outcome. This is the combination the bug lives in: the
    /// zone is genuinely usable, and only the `.wtr` is not.
    fn state_with_region_data(
        data: Result<std::sync::Arc<RegionMap>, RegionLoadError>,
    ) -> OpState {
        let s = empty_state();
        set_gs(&s, |gs| gs.world.zone_name = "testfixture".to_string());
        let ready = ZoneAssetState::test_ready_with_region_data(data);
        *eqoxide_zone_geometry::zone_assets::lock_state(&s.zone_assets) = ready;
        s
    }

    /// **Half one: the honest empty stays green.** A zone whose region map LOADED and contains no
    /// zone-line regions still gets `[]` with 200 — that is a real reading of the world and the
    /// overwhelmingly common one. A "fix" that refused on an empty list would break every zone.
    #[tokio::test]
    async fn a_zone_that_genuinely_has_no_zone_lines_still_answers_the_empty_list() {
        let (code, j) = get(
            state_with_region_data(Ok(std::sync::Arc::new(RegionMap::flat_below(-10.0)))),
            "/zone_exits",
        ).await;
        assert_eq!(code, StatusCode::OK,
            "a loaded map with no zone-line regions is an ANSWER; refusing here would break the \
             common case the empty list legitimately describes");
        assert_eq!(j["zone_exits"], serde_json::json!([]));
    }

    /// …and a loaded map WITH an exit still lists it, so half one is not passing because the
    /// endpoint stopped reporting exits altogether.
    #[tokio::test]
    async fn a_zone_with_a_zone_line_still_lists_it() {
        let (code, j) = get(
            state_with_region_data(Ok(std::sync::Arc::new(
                RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, 7)))),
            "/zone_exits",
        ).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(j["zone_exits"].as_array().map(|a| a.len()), Some(1), "the fixture bakes exactly one exit: {j}");
        assert_eq!(j["zone_exits"][0]["index"], 7);
    }

    /// **Half two: THE #803 FALSEHOOD.** Every way the `.wtr` can fail to load, plus the
    /// never-attached case, must come back as an explicit refusal naming its cause — never `[]`,
    /// never 200. All five are looped because the old code collapsed them into one `None`; a fix
    /// that only handled the failure in the issue title would rebuild the same lie for the rest.
    #[tokio::test]
    async fn a_region_map_that_did_not_load_refuses_instead_of_reporting_no_exits() {
        let cases: Vec<(Result<std::sync::Arc<RegionMap>, RegionLoadError>, &str)> = vec![
            (Err(RegionLoadError::Missing), "region_data_missing"),
            (Err(RegionLoadError::Unreadable(std::io::ErrorKind::PermissionDenied)),
                "region_data_unreadable"),
            (Err(RegionLoadError::NotRegionData), "region_data_not_region_data"),
            (Err(RegionLoadError::UnsupportedVersion(99)), "region_data_unsupported_version"),
            (Err(RegionLoadError::Truncated { declared_nodes: 400, bytes: 12 }),
                "region_data_truncated"),
        ];
        for (data, want_reason) in cases {
            let (code, j) = get(state_with_region_data(data), "/zone_exits").await;
            assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE,
                "{want_reason}: a failed read served as `[]`/200 tells the agent this zone has no \
                 way out — a confident falsehood it cannot detect (#803). Got {code}: {j}");
            assert_eq!(j["error"], "zone_region_data_unavailable", "body: {j}");
            assert_eq!(j["reason"], want_reason,
                "the reason must name the ACTUAL failure — 'the file is absent' and 'the file is \
                 truncated' call for different operator action");
            assert!(!j["detail"].as_str().unwrap_or("").is_empty(), "body: {j}");
        }
    }

    /// The refusal is deliberately NOT the `zone_assets_not_ready` verdict: that one is also read by
    /// the net thread's zone-cross drain, and a missing `.wtr` does
    /// not invalidate the terrain those consume. Pinned so a later "simplification" that folds the
    /// two together has to change this line and say why.
    ///
    /// **Asserted positively** (#821 review round 2, minor M1). This used to be only
    /// `assert_ne!(j["error"], "zone_assets_not_ready")`, which is vacuously true whenever the body
    /// is an ARRAY — `j["error"]` is then `Value::Null` — so the endpoint restored to `200 []` (the
    /// exact regression this test is named after) passed it. Measured, not reasoned: the reviewer
    /// ran that mutation and this test survived it.
    #[tokio::test]
    async fn the_refusal_is_distinct_from_the_zone_assets_gate() {
        let (code, j) = get(state_with_region_data(Err(RegionLoadError::Missing)), "/zone_exits").await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "body: {j}");
        assert_eq!(j["error"], "zone_region_data_unavailable",
            "region data is a separate readiness question from the zone's terrain assets, and the \
             refusal has to SAY so — an array body would make a bare `assert_ne!` pass: {j}");
        assert_ne!(j["error"], "zone_assets_not_ready", "body: {j}");
    }
}

/// #939 / #1073 — `GET /v1/observe/debug`'s `server_pushed_rosters`, the in-band disclosure that an
/// empty body from `/v1/observe/doors`, `/v1/observe/entities` or `/v1/observe/zone_entrances` means
/// "no record held", not "none exist".
///
/// **What these three tests are for, and what they cannot do.** The defect is not that any of the
/// three endpoints computes the wrong array — each one faithfully serves the roster it holds. It is
/// that the roster is filled by records the SERVER pushes, so an empty one has two readings and
/// nothing published told them apart. No test here can produce the missing signal, because this
/// client holds none (see [`NO_COMPLETENESS_SIGNAL`]); what they hold is that the DISCLOSURE is
/// published, that it enumerates all three endpoints rather than crowning one, that it never
/// hardens into a completeness claim, and that its `held` counts are read off the same rosters the
/// endpoints serve rather than typed into a list that can drift.
#[cfg(test)]
mod server_pushed_roster_completeness_939_1073 {
    
    
    
    
    

}

/// Reach control for `observe::tests::no_past_dated_net_health_stamp_is_taken_from_a_clock_other_than_the_one_that_reads_it` (#760/C1).
///
/// That guard scans this file's source text. A scanner that silently stops early — as it did, when
/// a `/*` inside a route glob in a doc comment latched its block-comment state on line 1 — reports
/// a clean scan of a corpus it never read, which is a confident falsehood. The guard asserts it can
/// SEE this constant; because it is the last item in the file, seeing it proves the scan arrived at
/// the end. **Keep it last.**
#[cfg(test)]
pub(crate) const GUARD_REACH_SENTINEL_OBSERVE: u8 = 0;
