//! The agent-facing HTTP/REST API (axum). Routes are versioned + grouped: `/v1/<group>/<action>`,
//! where `<group>` mirrors the MCP tool grouping — `observe`, `move`, `combat`, `interact`,
//! `merchant`, `inventory`, `chat`, `camera`, `lifecycle`. The `/v1` prefix lets a future breaking
//! revision ship as `/v2` while old clients keep working.
//!
//! Each group lives in its own submodule (e.g. `combat.rs`) exposing a `router()` of relative
//! paths; `spawn_camera_server` nests them under `/v1/<group>`. This module holds the cross-cutting
//! HTTP-only pieces: request/response body types (`PlayerState`, `Health`, …), `HttpState`, and the
//! server task. Most handlers just write a shared request slot (the `*Req` aliases) that the
//! navigation thread drains each tick; reads come from snapshots the render/network threads
//! publish — those `Arc<Mutex<…>>` IPC channel types themselves live in [`eqoxide_ipc`] (re-exported
//! here for the handler submodules below) since they are shared with the network/render threads,
//! not genuine HTTP types. See `docs/http-api.md`.

use axum::Router;
// `CameraCmd`/`CameraSnapshot` (and every other `eqoxide_ipc` type the handlers reference) come in
// via the `pub use eqoxide_ipc::*;` glob below — a separate private `use` here would only shadow it.

/// Extracts an optional JSON body, distinguishing "no body was sent" (→ `.0 == None`, so the
/// handler applies its own defaults) from "a body was sent but didn't parse" (→ a 400 naming
/// exactly what failed). `Option<axum::Json<T>>` collapses BOTH cases into `None` — a malformed or
/// out-of-range field (e.g. a `u16` field given `99999`) silently looks identical to an omitted
/// body, so the handler's default kicks in and the caller gets a misleading 200 (eqoxide#328).
///
/// Whether a body was "sent" is judged from the raw bytes (empty/whitespace-only ⇒ absent), not
/// from the `Content-Type` header — so a caller that forgets `Content-Type: application/json` still
/// gets its body parsed (or a clear 400) instead of a silent no-op.
pub(crate) struct OptionalJson<T>(pub(crate) Option<T>);

#[axum::async_trait]
impl<T, S> axum::extract::FromRequest<S> for OptionalJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = (axum::http::StatusCode, String);

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        // Preserve the underlying rejection's own status (e.g. an over-limit body is 413, not 400).
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(|e| (e.status(), format!("failed to read request body: {e}")))?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(OptionalJson(None));
        }
        let de = &mut serde_json::Deserializer::from_slice(&bytes);
        let value = serde_path_to_error::deserialize::<_, T>(&mut *de)
            .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("malformed JSON body: {e}")))?;
        // Reject trailing garbage after the JSON value (`{"zone_id":45} lolwut`, two concatenated
        // objects, …). serde_json's streaming Deserializer stops at the end of the FIRST value and
        // would otherwise silently ignore whatever follows — `axum::Json` rejects these, so we must
        // too, or #328's silent-acceptance bug survives in a smaller form.
        de.end()
            .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("malformed JSON body: trailing data after the JSON value: {e}")))?;
        Ok(OptionalJson(Some(value)))
    }
}

mod name_match;
mod observe;
mod quests;
mod group;
mod guild;
mod move_api;
mod trainer;
mod pet;
mod combat;
pub mod interact;
mod merchant;
mod inventory;
mod chat;
mod events;
mod social;
mod camera;
mod lifecycle;

// Shared test fixtures (`HttpState` builder + snapshot-seeding helpers). Available to this crate's
// own unit tests and, via the `test-fixtures` feature, to the app crate's integration tests — see
// the module docs. Never compiled into a release build.
#[cfg(any(test, feature = "test-fixtures"))]
pub mod testkit;

// The `Arc<Mutex<…>>` request/snapshot ("IPC channel") types used to live here, but they are shared
// state between this thread, the network thread, and the render/app loop — not genuine HTTP types —
// so they were relocated to the neutral `eqoxide_ipc` (cleanup; pure code motion). This glob re-export
// is a compatibility shim: every handler submodule below still just does `use super::*;` and every
// name it expects (the `*Req`/`*Shared` aliases, `HttpState`'s field types, `NetHealth`, …) keeps
// resolving unqualified. New cross-thread code should `use eqoxide_ipc::…` directly instead of routing
// through here — see `eqoxide_ipc` for the authoritative definitions.
pub use eqoxide_ipc::*;

/// Seconds without any inbound server packet after which the session is reported disconnected
/// (`connected: false`). Generous enough to ride out normal quiet spells; short enough that a
/// dead/frozen server is caught within a few seconds (eqoxide#8).
pub const CONN_STALE_SECS: u64 = 15;

/// #477: milliseconds without a network-thread tick after which a WRITE command treats the session
/// as dead. The gameplay net thread republishes the snapshot every ~10ms (see
/// `gameplay::publish_snapshot`, which bumps `NetHealth::last_tick` UNCONDITIONALLY every loop even
/// on an idle world), so a healthy session — including a fully IDLE one — keeps `snapshot_age_ms`
/// near zero and never approaches this. Only a wedged or EXITED net thread (a failed world-reconnect
/// that killed the gameplay thread while the HTTP server kept serving — the #470/#477 zombie) lets
/// `snapshot_age_ms` climb this high. Kept comfortably below `CONN_STALE_SECS * 1000` so a thread
/// that died WITHOUT the link having gone stale yet (datagrams may briefly keep arriving after the
/// thread exits) is still caught here, faster than `connected` would flip.
pub const SESSION_STALE_TICK_MS: u64 = 5_000;

/// How long after a death `killed_by` / `died_ago_secs` keep being reported (through a respawn), so
/// an infrequently-polling agent still learns that it died and what killed it (#284).
pub const DEATH_STICKY_SECS: u64 = 300;

/// Live player state for the /v1/observe/debug endpoint.
///
/// **This is a pure projection of the network thread's `GameState`** — derived on demand by
/// [`HttpState::player`], never cached. It deliberately contains NO connection-health or freshness
/// fields: those are time-derived, and a time-derived value baked into a stored struct is a lie the
/// moment its publisher stops running. `/debug` computes `connected` / `last_packet_age_ms` /
/// `snapshot_age_ms` at READ time from [`NetHealth`]'s clocks (#343).
///
/// It also contains no render-owned fields: `frame_profile` lives in [`FrameProfileShared`], written
/// by the render loop. Observation must not be coupled to rendering.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PlayerState {
    /// The player's own character name — so `/v1/observe/debug` identifies which char it drives (#109).
    pub name:         String,
    pub zone:         String,
    /// #335/agent-honesty: the last in-game zone change FAILED — we connected to the new zone server
    /// and sent OP_ZoneEntry but the handshake timed out (`zone-entry-handshake-race.md`). When true,
    /// `zone` is EMPTY on purpose (we are not confidently in any zone) rather than reporting the zone
    /// we came from, and the net thread is tearing down. Default false on every healthy reading.
    pub zone_in_failed: bool,
    pub race:         String, // 3-letter race code, e.g. "ELF" (Wood Elf)
    pub class:        String, // class name, e.g. "Cleric"
    pub level:        u32,
    pub pos_east:     f32,
    pub pos_north:    f32,
    pub pos_up:       f32,
    pub heading_ccw:  f32, // 0=north CCW
    pub heading_cw:   f32, // 0=north CW (wire format)
    pub server_corrections: u32,
    pub mem_spells:   [u32; 9],
    /// Player skill values by skill id (0..77), for GET /v1/observe/skills (eqoxide#99).
    pub skills:       Vec<u32>,
    /// Whether a guildmaster training window is open, and the caps it offers per skill id (#99).
    pub trainer_open:   bool,
    pub trainer_skills: Vec<u32>,
    /// The player's own spawn id (for scripting a self-target). (eqoxide#95)
    pub player_id:    u32,
    pub target_id:    Option<u32>,
    /// Coin on hand: [platinum, gold, silver, copper], from the player profile.
    pub coin:         [u32; 4],
    /// False when `coin` may not match the server's real balance right now (#361): any merchant buy
    /// has been sent since the last authoritative OP_PlayerProfile, or `coin` has never been seeded
    /// from a real reading. A silent buy refusal (inventory-full/LORE conflict) sends no echo at
    /// all, so an agent reading `currency` alone has no other way to know the figure is provisional.
    /// `true` only once the balance is server-confirmed accurate (the most recent OP_PlayerProfile
    /// reconciliation, with no buy sent since). A per-buy echo deliberately does not restore this —
    /// it confirms a relative delta, not that the absolute balance escaped an earlier silent refusal
    /// (#361 review — FIX 1).
    pub coin_verified: bool,
    /// Vitals — same values the HUD renders. Percentages are 0–100. Lets an API consumer make
    /// flee/heal/leveling decisions instead of scraping the message log. (eqoxide#9)
    pub hp_pct:        f32,
    pub cur_hp:        i32,
    pub max_hp:        i32,
    /// Death state for headless agents (#284). `dead` = currently slain (held until POST
    /// /v1/lifecycle/respawn). `killed_by` + `died_ago_secs` also persist for a window AFTER a
    /// respawn, so an infrequently-polling agent can still tell it died and what killed it.
    pub dead:          bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub killed_by:     Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub died_ago_secs: Option<u64>,
    pub mana_pct:      f32,
    pub cur_mana:      i32,
    pub max_mana:      i32,
    pub xp_pct:        f32,
    /// Current target's display name and HP percent (0–100), or None when nothing is targeted.
    pub target_name:   Option<String>,
    pub target_hp_pct: Option<f32>,
    /// #292: consider result for the current target (from POST /v1/combat/consider). `target_con`
    /// is the difficulty tier (gray/green/light_blue/blue/white/yellow/red), `target_attitude` the
    /// faction attitude enum (ally … scowls), `target_level` the target's actual level — so agents
    /// can gauge "how tough + how hostile" programmatically. Populated after a consider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_con:      Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_attitude: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_level:    Option<u32>,
    /// #336: the result of the most recent consider of ANY spawn — target or not. `target_con`/
    /// `target_attitude`/`target_level` above are target-scoped (only populated while that spawn IS
    /// the current target); `last_consider` is spawn-scoped, so a standalone
    /// `POST /v1/combat/consider {"id":N}` on a non-target spawn is readable here without first
    /// targeting it. `None` until a consider reply has been received this session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_consider:   Option<LastConsiderView>,
    /// Text of the most recently read book/note (OP_ReadBook reply), newline-decoded. None until a
    /// book is read. Surfaced via GET /v1/observe/item_text so an agent can read a quest note (#288).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book_text:          Option<String>,
    /// Spellcasting (#348). `casting` is Some ONLY while the player's own cast bar is running (the
    /// server accepted the cast and sent OP_BeginCast for our spawn); `last_cast` is how the most
    /// recent cast ENDED and persists afterwards, so an agent that polls rather than long-polls
    /// `/v1/events/combat` can still tell *casting* / *landed* / *fizzled* / *interrupted* /
    /// *never started* apart. Both were previously tracked internally and published nowhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub casting:            Option<CastingView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_cast:          Option<LastCastView>,
}

impl PlayerState {
    /// Project the network thread's authoritative `GameState` into the agent-facing view.
    ///
    /// `gs.player_x/y/z` and `gs.player_heading` are the *controller's* position: the nav thread
    /// mirrors `controller_view` into them every tick in `ActionLoop::stream_position` (and the
    /// controlled-fall branch writes `player_z` itself), so this is exactly the position the client
    /// streams to the server — no need to reach into the render thread's controller (#343).
    pub fn from_game_state(gs: &eqoxide_core::game_state::GameState) -> Self {
        PlayerState {
            name:       gs.player_name.clone(),
            zone:       gs.world.zone_name.clone(),
            zone_in_failed: gs.world.zone_in_failed,
            race:       gs.player_race.clone(),
            class:      gs.player_class.clone(),
            level:      gs.player_level as u32,
            pos_east:   gs.player_x,
            pos_north:  gs.player_y,
            // FOOT datum (#522, see coord::WIRE_Z_OFFSET): every agent-facing position reports the
            // collision-floor/foot height, the SAME datum used internally (controller, gs.player_z,
            // nav, collision) and by /observe/entities (entities are converted wire→foot at ingest).
            // One datum end to end means a position the agent READS here can be fed straight back
            // into goto/coords without a 3u skew, and self reads at the same height as another player
            // standing on the same plank. The wire↔foot conversion happens ONLY at the packet edge
            // (outbound adds the offset in OP_ClientUpdate; inbound subtracts it), never here.
            pos_up:     gs.player_z,
            heading_ccw: gs.player_heading,
            heading_cw:  eqoxide_protocol::protocol::ccw_to_cw(gs.player_heading),
            server_corrections: gs.server_corrections,
            mem_spells: gs.mem_spells,
            skills:     gs.player_skills.clone(),
            trainer_open:   gs.trainer_open.is_some(),
            trainer_skills: gs.trainer_skills.clone(),
            player_id:  gs.player_id,
            target_id:  gs.target_id,
            coin:       gs.coin,
            coin_verified: gs.coin_verified(),
            hp_pct:     gs.hp_pct,
            cur_hp:     gs.cur_hp,
            max_hp:     gs.max_hp,
            // Death state (#284). `dead` is live (held slain until /lifecycle/respawn);
            // killed_by/died_ago_secs stay reported for DEATH_STICKY_SECS after death (through a
            // respawn too) so an infrequent poller still sees it. Both are time-derived — being
            // computed here, at read time, they can no longer freeze mid-window (#343).
            dead:          gs.player_dead,
            killed_by:     gs.died_at
                               .filter(|t| t.elapsed().as_secs() < DEATH_STICKY_SECS)
                               .map(|_| gs.killed_by.clone()),
            died_ago_secs: gs.died_at
                               .filter(|t| t.elapsed().as_secs() < DEATH_STICKY_SECS)
                               .map(|t| t.elapsed().as_secs()),
            mana_pct:   gs.mana_pct,
            cur_mana:   gs.cur_mana,
            max_mana:   gs.max_mana,
            xp_pct:     gs.xp_pct,
            // Prefer the live entity (its hp_pct tracks combat via OP_HP_UPDATE); fall back to the
            // target snapshot stored at target time if the entity is gone. Both gated on target_id
            // being Some (#331 defence in depth) so a missed GameState::clear_target can never leak
            // a stale name/HP into the API alongside a null id.
            target_name:   gs.target_id.and_then(|id| gs.world.entities.get(&id).map(|e| e.name.clone())
                               .or_else(|| gs.target_name.clone())),
            target_hp_pct: gs.target_id.and_then(|id| gs.world.entities.get(&id).map(|e| e.hp_pct)
                               .or(gs.target_hp_pct)),
            // #292: con difficulty tier + attitude enum (from the last consider) and the target's
            // level, only while something is targeted.
            target_con:      gs.target_id.and(gs.target_con_name.clone()),
            target_attitude: gs.target_id.and(gs.target_attitude.clone()),
            target_level:    gs.target_id.and_then(|id| gs.world.entities.get(&id)).map(|e| e.level),
            // #336: spawn-scoped, unlike target_con*/target_level above — populated for the LAST
            // consider of any spawn, not gated on that spawn being the current target.
            last_consider: gs.last_consider.as_ref().map(|c| LastConsiderView {
                spawn_id: c.spawn_id,
                name:     c.name.clone(),
                con_name: c.con_name.clone(),
                attitude: c.attitude.clone(),
                level:    c.level,
                at:       c.at,
            }),
            book_text:       gs.last_book_text.clone(),
            // Spellcasting (#348). The live cast bar and how the last cast ended, straight from the
            // server's own packets (OP_BeginCast / OP_InterruptCast / OP_MemorizeSpell scribing=3 /
            // the spell-failure eqstr messages) — see packet_handler.rs. Publishing these is what
            // makes casting observable at all.
            //
            // The `Instant`s are carried through UNMEASURED on purpose: `elapsed_ms` / `ago_secs`
            // are computed in the HTTP handler at read time (see `observe::get_debug`). Measuring an
            // age at projection time would be the #343 bug in miniature — an age is only true at the
            // moment it is read.
            casting:         gs.casting.as_ref().map(|c| CastingView {
                                 spell_id:   c.spell_id,
                                 spell_name: eqoxide_core::spells::name_of(c.spell_id),
                                 cast_ms:    c.cast_ms,
                                 started:    c.started,
                             }),
            last_cast:       gs.last_cast.as_ref().map(|o| LastCastView {
                                 spell_id:   o.spell_id,
                                 spell_name: eqoxide_core::spells::name_of(o.spell_id),
                                 outcome:    o.kind.to_string(),
                                 text:       o.text.clone(),
                                 at:         o.at,
                             }),
        }
    }
}

/// Connection/freshness health, computed fresh on every HTTP read — never stored (#343).
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Health {
    /// Milliseconds since the last inbound UDP datagram. This is what `connected` is derived from.
    pub link_age_ms:        u64,
    /// Milliseconds since the last inbound *application* packet — i.e. how long the WORLD has been
    /// quiet. On an idle session this legitimately reaches tens of seconds while the link is fine,
    /// so an agent must not treat it as a disconnect signal; use `connected` for that.
    pub last_packet_age_ms: u64,
    /// Milliseconds since the network thread last ticked (client-liveness). A large value means OUR
    /// publisher stopped, so every other field in the payload is stale.
    pub snapshot_age_ms:    u64,
    /// False when NO datagram — not even a session-layer ACK — has arrived for [`CONN_STALE_SECS`].
    /// That is a dead link, as distinct from a quiet world.
    pub connected:          bool,
    /// #371: the honest "is the WORLD alive?" signal. `connected` only means the socket ACKs, which
    /// a WEDGED zone keeps doing perfectly. This is `false` only when an active liveness probe went
    /// unanswered past [`PROBE_TIMEOUT_SECS`] while nothing spontaneous arrived either — i.e. the
    /// zone main loop is not processing. An idle-but-alive zone stays `true` (the probe is answered).
    /// `true` before the first probe fires (no verdict yet — read `connected`/`last_packet_age_ms`).
    pub world_responsive:      bool,
    /// #371: ms since the world last demonstrably processed something for us — the most recent of a
    /// probe reply or a spontaneous application packet. This is the "how long since real proof of
    /// life" companion to `world_responsive`, and unlike `last_packet_age_ms` it is NOT reset by
    /// probe traffic being suppressed, because probe replies legitimately count as proof of life here.
    pub last_world_response_ms: u64,
}

/// A cast in flight, for `/v1/observe/debug` → `casting` (#348).
///
/// NOTE the missing `elapsed_ms`: it is derived at **HTTP read time** from `started`, never stored.
/// An age baked in when the value is *projected* is only true at that instant; every moment after,
/// it is wrong, and nothing in the payload says so. That is #343 in miniature — and it is why the
/// whole player view is now derived on read rather than published by a loop that sleeps. A duration
/// must be measured when it is read, or it is just another lie with a timestamp on it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CastingView {
    pub spell_id:   u32,
    pub spell_name: String,
    /// Total cast time the server announced in OP_BeginCast.
    pub cast_ms:    u32,
    /// When the cast started. Not serialized — `elapsed_ms` is computed from it on read.
    #[serde(skip)]
    pub started:    std::time::Instant,
}

/// How the player's most recent cast ended, for `/v1/observe/debug` → `last_cast` (#348).
/// `ago_secs` is likewise derived at read time from `at` — see [`CastingView`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct LastCastView {
    /// 0 when the server never named the spell (an honest unknown, not a guess).
    pub spell_id:   u32,
    pub spell_name: String,
    /// `cast_completed` | `cast_interrupted` | `cast_fizzled` | `cast_failed` |
    /// `cast_ended_unexplained` — the same value the matching `/v1/events/combat` event carries as
    /// its `kind`.
    ///
    /// The first four are verdicts the SERVER gave us. `cast_ended_unexplained` is the client's own
    /// INFERENCE: the server sent its cast-end signal (OP_ManaChange keepcasting=0) and never said
    /// why — the usual cause being `Mob::SpellFinished` returning false. An agent must be able to
    /// branch on "the server said it failed" vs "we don't know why it ended", so these are
    /// deliberately not the same kind.
    pub outcome:    String,
    /// The line shown to the agent. For a server verdict this is the SERVER's own string ("Your
    /// spell fizzles!", "Insufficient Mana to cast this spell!"). For `cast_ended_unexplained` it
    /// is written in the CLIENT's voice and says plainly that the server reported nothing — never
    /// server-sounding prose we invented, which an agent could not tell from a real server line.
    pub text:       String,
    /// When the outcome landed. Not serialized — `ago_secs` is computed from it on read.
    #[serde(skip)]
    pub at:         std::time::Instant,
}

/// The most recent consider result for ANY spawn (target or not), for `/v1/observe/debug` →
/// `last_consider` (#336). `ago_secs` is derived at read time from `at` — see [`CastingView`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct LastConsiderView {
    pub spawn_id:   u32,
    pub name:       String,
    /// Difficulty tier: gray (trivial/no exp) | green | light_blue | blue | white (even) | yellow |
    /// red (dangerous). See `con_level_name`.
    pub con_name:   String,
    /// Attitude enum: ally | warmly | kindly | amiable | indifferent | apprehensive | dubious |
    /// threatening | scowls (KOS). See `attitude_name`.
    pub attitude:   String,
    /// The spawn's actual character level, when known. `None` is an honest "unknown" (the spawn had
    /// already left `entities` by the time the reply arrived) — never a fabricated number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level:      Option<u32>,
    /// When the consider reply landed. Not serialized — `ago_secs` is computed from it on read.
    #[serde(skip)]
    pub at:         std::time::Instant,
}

/// Relocated to `eqoxide-core::game_state` (#544 Step 2o) so `eqoxide-ui` can use it without an
/// up-reference into this crate. Re-exported so every existing `crate::clean_entity_name` /
/// `eqoxide_http::clean_entity_name` call site keeps resolving unchanged.
pub use eqoxide_core::game_state::clean_entity_name;

/// Render coin `[platinum, gold, silver, copper]` as a JSON object for the API.
pub(crate) fn currency_json(coin: [u32; 4]) -> serde_json::Value {
    serde_json::json!({
        "platinum": coin[0],
        "gold":     coin[1],
        "silver":   coin[2],
        "copper":   coin[3],
    })
}

/// **M4 domain bundles** (see `eqoxide_ipc` module docs): the ~62 flat request/snapshot slots this
/// struct used to hold individually are now grouped by domain, mirroring the `/v1/<group>/*` router
/// nesting below — prefiguring a future shared controller-verb API (Phase 2). Each bundle here MUST
/// be a `.clone()` of the SAME bundle instance handed to `ActionLoop::new` in `main.rs`; that shared
/// Arc identity (not two independently-`Default`-constructed bundles) is what keeps this the same
/// cross-thread channel the nav thread drains. See `ipc.rs` and `main.rs` wiring.
///
/// A few bundle fields are unused on THIS side of a channel (e.g. `camera.frame_req` is written by an
/// HTTP handler and read by the render thread, never drained here) — that's expected: the bundle
/// boundary is the DOMAIN, not "exactly the fields this struct touches". (The walker's draw-only
/// `nav_path_view` overlay moved off `NavSlots` to `ControllerSlots` in #452 — a render↔nav channel
/// `HttpState` never held.)
// `pub` (not `pub(crate)`) only so downstream integration tests can NAME the type that
// `testkit::empty_state`/`debug_json` hand back (they can't otherwise hold a value of it across the
// crate boundary). Its fields stay `pub(crate)`, so it remains un-constructible outside this crate —
// only `spawn_camera_server` / `testkit::empty_state` build one. (#544 Step 2l)
#[derive(Clone)]
pub struct HttpState {
    /// `/v1/camera/*` slots (#M4).
    pub(crate) camera:          eqoxide_ipc::CameraSlots,
    /// `/v1/move/*` slots (#M4).
    pub(crate) nav:             eqoxide_ipc::NavSlots,
    /// The live entity registry + zone exit points (#M4).
    pub(crate) world:           eqoxide_ipc::WorldSlots,
    /// Zone collision + region map (shared with the nav thread); read-only here, for zone_exits.
    pub(crate) shared_collision: eqoxide_nav::collision::SharedCollision,
    /// The typed write-path facade (#446). Combat is fully migrated onto it — combat/pet handlers
    /// write via `s.command.request_*` (no direct `ipc::CombatSlots` field any more); other domains
    /// still use their own bundle fields until Wave-2 migrates them. See `eqoxide_command`.
    pub(crate) command:         eqoxide_command::CommandState,
    /// `/v1/social/*` (who/friends) slots (#M4).
    pub(crate) social:          eqoxide_ipc::SocialSlots,
    /// `/v1/merchant/*` slots (#M4).
    pub(crate) merchant_slots:  eqoxide_ipc::MerchantSlots,
    /// `/v1/inventory/*` slots (#M4).
    pub(crate) inventory_slots: eqoxide_ipc::InventorySlots,
    /// `/v1/interact/*` slots (#M4).
    pub(crate) interact:        eqoxide_ipc::InteractSlots,
    /// Outgoing chat + async events + the message log (#M4).
    pub(crate) chat:            eqoxide_ipc::ChatSlots,
    pub(crate) spells:           std::sync::Arc<eqoxide_core::spells::SpellDb>,
    /// The network thread's authoritative `GameState`. Every agent-facing player field is projected
    /// from HERE at read time (`HttpState::player`) — the render loop is no longer in the path (#343).
    pub(crate) game_state:       GameStateSnapshot,
    /// The network thread's three liveness clocks — turned into `Health` on every read (#343).
    pub(crate) net_health:       NetHealthShared,
    /// Render-owned frame timings (the ONLY agent-visible value the render loop publishes).
    pub(crate) frame_profile:    FrameProfileShared,
    /// `/v1/quests/*` slots (#M4).
    pub(crate) quest:           eqoxide_ipc::QuestSlots,
    /// `/v1/group/*` slots (#M4).
    pub(crate) group_slots:     eqoxide_ipc::GroupSlots,
    /// `/v1/lifecycle/*` slots (#M4).
    pub(crate) lifecycle:       eqoxide_ipc::LifecycleSlots,
    /// `/v1/guild/*` slots (#M4).
    pub(crate) guild_slots:     eqoxide_ipc::GuildSlots,
}

impl HttpState {
    /// The agent-facing player view, **derived at read time** from the network thread's `GameState`
    /// snapshot. There is no cached `PlayerState` anywhere, so there is nothing to go stale (#343).
    pub(crate) fn player(&self) -> PlayerState {
        PlayerState::from_game_state(&self.game_state.load())
    }

    /// The player's world position, or `None` if the SERVER has not told us where we are yet
    /// (#513 review, F4).
    ///
    /// `PlayerState::pos_*` are plain `f32` that read `0.0` from construction until the first
    /// server position packet, so anything derived from them before that is measured from the zone
    /// ORIGIN. That is fine for a raw position readout (which is honestly "what we last knew") but
    /// NOT for a derived figure like the `distance` a name-resolution endpoint publishes: a
    /// confident wrong distance is precisely the falsehood #513 exists to remove, and the
    /// just-zoned-in window is where its original wrong-target near-miss happened. Callers that
    /// would publish such a figure must use THIS, not `player()`, so an unknown position becomes an
    /// omitted field rather than a number measured from nowhere.
    pub(crate) fn player_pos(&self) -> Option<(f32, f32, f32)> {
        let gs = self.game_state.load();
        gs.player_pos_known.then(|| (gs.player_x, gs.player_y, gs.player_z))
    }

    /// Connection + snapshot freshness, computed from the two shared clocks **on every read**.
    ///
    /// This is the whole point of #343: before, `connected` was computed inside `render_frame`, and
    /// the render loop deliberately sleeps when no packets arrive — so a dead connection (no packets
    /// → no render) meant `connected` was never recomputed and stayed `true` forever. Elapsed time
    /// is now measured when the agent asks, so silence *is* the signal, and no publisher — not the
    /// renderer, not the network thread — has to be alive for the answer to be honest.
    pub(crate) fn health(&self) -> Health {
        let h = *self.net_health.lock().unwrap();
        let link_age       = h.last_datagram.elapsed();
        let last_packet_ago = h.last_packet.elapsed();
        // #371: the active-probe verdict, all measured at read time (like every other health field,
        // per #343 — never cached, so no live publisher has to run for the answer to stay honest).
        // NOTE: the timeout check is measured against `first_unanswered_probe_sent`, NOT
        // `last_probe_sent` — the latter is bumped by every 30s resend and would let a permanently
        // wedged zone re-earn the 10s in-flight grace window forever (the #371-followup bug).
        let probe_sent_ago  = h.first_unanswered_probe_sent.map(|t| t.elapsed());
        let probe_reply_ago = h.last_probe_reply.map(|t| t.elapsed());
        // #470: `world_responsive` must know the LINK is dead. When a failed world-reconnect kills the
        // net thread the prober dies too, so no probe is ever outstanding — without this, the "no
        // probe" branch reported a zombie session alive forever. `connected` here is the SAME value
        // published in `Health` below (both derived from `last_datagram` vs CONN_STALE_SECS).
        let connected = link_age.as_secs() < CONN_STALE_SECS;
        let world_responsive = world_responsive(
            connected, probe_sent_ago, probe_reply_ago, last_packet_ago,
            std::time::Duration::from_secs(PROBE_TIMEOUT_SECS),
            std::time::Duration::from_secs(PASSIVE_LIVENESS_STALE_SECS),
        );
        // Most recent proof the world processed something for us: a probe reply OR spontaneous
        // traffic, whichever is fresher. Falls back to the app clock before any probe has replied.
        let last_world_response_ms = probe_reply_ago
            .map_or(last_packet_ago, |r| r.min(last_packet_ago))
            .as_millis() as u64;
        Health {
            link_age_ms:        link_age.as_millis() as u64,
            last_packet_age_ms: last_packet_ago.as_millis() as u64,
            snapshot_age_ms:    h.last_tick.elapsed().as_millis() as u64,
            // Link liveness, NOT world activity — see `NetHealth`. An idle session goes 40+s with
            // no application packet while the session layer keeps ACKing; calling that "disconnected"
            // would be just as much a lie as #343's frozen `connected: true`.
            connected,
            world_responsive,
            last_world_response_ms,
        }
    }
}

/// #477: reject a WRITE command when the game SESSION is not live, so an agent never gets a false
/// `200 OK` for an action that goes nowhere. This is the other half of the #470 zombie: when a failed
/// world-reconnect exits the gameplay NET THREAD, the HTTP server keeps running and every WRITE
/// handler still writes its request slot and returns 200 — but the net thread that DRAINS those slots
/// is gone, so the action silently never happens. This turns that into an immediate, explicit failure.
///
/// A WRITE handler is one that only enqueues a request the gameplay net thread must later drain
/// (`move/*`, `combat/*`, `interact/*`, `merchant/*`, `inventory/*`, `quests/*`, `trainer/*`,
/// `group/*`, `guild/*`, `chat/*`, `pet/*`, `lifecycle/{respawn,camp}`). READ/observe handlers are
/// deliberately NOT gated: they honestly report last-known state and already carry `connected` /
/// `world_responsive`, so an agent can see the session is dead without being lied to. `lifecycle/exit`
/// is also NOT gated — it must always work, and its own watchdog force-exits the process even if the
/// loop is wedged.
///
/// The "dead" verdict is keyed on two INDEPENDENT net-thread-liveness signals, never on transient
/// quiet, so a healthy idle session (connected + ticking, but no application traffic for tens of
/// seconds) always passes (#343):
///   - `connected == false` — no server datagram (not even a session-layer ACK) for `CONN_STALE_SECS`.
///     The clearest "the link/thread is gone" signal; an idle-but-alive session keeps ACKing so stays
///     `true`.
///   - `snapshot_age_ms >= SESSION_STALE_TICK_MS` — the net thread stopped ticking. Catches the window
///     where the thread died but the link has not gone stale yet, and does so faster than `connected`.
///     An idle-but-alive session ticks every ~10ms, so this never fires on it.
///
/// Deliberately NOT keyed on `world_responsive`: that reads `false` on a WEDGED-but-ALIVE zone (#371)
/// whose net thread is fine and where a queued command WILL eventually drain once the world unwedges —
/// rejecting there would be dishonest in the other direction. This guard is strictly about "is the net
/// THREAD that drains commands still running", which `connected` + tick-staleness answer without any
/// false positive on a healthy idle session.
pub(crate) fn require_live_session(s: &HttpState) -> Result<(), (axum::http::StatusCode, String)> {
    let h = s.health();
    if !h.connected {
        return Err((
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "not connected — the game session is not live (no server datagram for over \
                 {CONN_STALE_SECS}s; the network thread has disconnected or exited). This command \
                 was NOT sent and will not take effect. See GET /v1/observe/debug \
                 (`connected`/`world_responsive`)."
            ),
        ));
    }
    if h.snapshot_age_ms >= SESSION_STALE_TICK_MS {
        return Err((
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "the game session is not live — the network thread has not ticked in {}ms (it has \
                 exited or wedged). This command was NOT sent and will not take effect. See GET \
                 /v1/observe/debug (`snapshot_age_ms`).",
                h.snapshot_age_ms
            ),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_camera_server(
    camera:          eqoxide_ipc::CameraSlots,
    nav:             eqoxide_ipc::NavSlots,
    world:           eqoxide_ipc::WorldSlots,
    shared_collision: eqoxide_nav::collision::SharedCollision,
    command:         eqoxide_command::CommandState,
    social:          eqoxide_ipc::SocialSlots,
    merchant_slots:  eqoxide_ipc::MerchantSlots,
    inventory_slots: eqoxide_ipc::InventorySlots,
    interact:        eqoxide_ipc::InteractSlots,
    chat:            eqoxide_ipc::ChatSlots,
    spells:           std::sync::Arc<eqoxide_core::spells::SpellDb>,
    game_state:       GameStateSnapshot,
    net_health:       NetHealthShared,
    frame_profile:    FrameProfileShared,
    quest:           eqoxide_ipc::QuestSlots,
    group_slots:     eqoxide_ipc::GroupSlots,
    lifecycle:       eqoxide_ipc::LifecycleSlots,
    guild_slots:     eqoxide_ipc::GuildSlots,
    port:             u16,
    // When `Some`, an already-bound listener from `--api-port` (exact port, no scan).
    // When `None`, scan upward from `port` for the first free port.
    exact_listener:   Option<std::net::TcpListener>,
) {
    // Named (see #380 — a panic hook can only say WHICH thread died if the thread has a name;
    // the default anonymous name would just show up as '<unnamed>' in the crash log).
    std::thread::Builder::new()
        .name("http-server".into())
        .spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("http-tokio-worker")
            .build()
            .expect("http tokio runtime");
        rt.block_on(async move {
            let state = HttpState {
                camera, nav, world, shared_collision, command, social, merchant_slots,
                inventory_slots, interact, chat, spells, game_state, net_health, frame_profile,
                quest, group_slots, lifecycle, guild_slots,
            };
            // Versioned + grouped routes: /v1/<group>/<action>. Each group's `router()` defines
            // relative paths; nesting prefixes them. Shared state is applied once at the end.
            let app = Router::new()
                .nest("/v1/observe",   observe::router())
                .nest("/v1/quests",    quests::router())
                .nest("/v1/group",     group::router())
                .nest("/v1/guild",     guild::router())
                .nest("/v1/move",      move_api::router())
                .nest("/v1/trainer",   trainer::router())
                .nest("/v1/pet",       pet::router())
                .nest("/v1/combat",    combat::router())
                .nest("/v1/interact",  interact::router())
                .nest("/v1/merchant",  merchant::router())
                .nest("/v1/inventory", inventory::router())
                .nest("/v1/chat",      chat::router())
                .nest("/v1/events",    events::router())
                .nest("/v1/social",    social::router())
                .nest("/v1/camera",    camera::router())
                .nest("/v1/lifecycle", lifecycle::router())
                .with_state(state);
            let (listener, bound_port) = if let Some(std_l) = exact_listener {
                // --api-port: use the listener main already bound to the exact requested port.
                std_l.set_nonblocking(true).expect("set api-port listener non-blocking");
                let l = tokio::net::TcpListener::from_std(std_l).expect("adopt api-port listener");
                let p = l.local_addr().map(|a| a.port()).unwrap_or(port);
                (l, p)
            } else {
                // Scan upward from the configured base port so multiple client instances
                // (e.g. one per worktree) each grab the next free port instead of colliding.
                const MAX_TRIES: u16 = 50;
                let mut bound = None;
                for p in port..port.saturating_add(MAX_TRIES) {
                    if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", p)).await {
                        bound = Some((l, p));
                        break;
                    }
                }
                match bound {
                    Some(found) => found,
                    None => {
                        tracing::info!(
                            "camera HTTP: no free port in {}..{} — camera API disabled",
                            port,
                            port.saturating_add(MAX_TRIES)
                        );
                        return;
                    }
                }
            };
            // Machine-parseable line on stdout so a launching agent can discover the port.
            // Flush explicitly: the render loop may never return, leaving stdout buffered.
            use std::io::Write;
            tracing::info!("API_PORT={bound_port}");
            let _ = std::io::stdout().flush();
            // Stamp the per-pid crash log with what this instance IS (#380). Several clients run at
            // once on distinct ports; without this, a directory of crash-<pid>.log files is a pile
            // of anonymous pids and a post-mortem can't tell which one was the client it cares about.
            //
            // This is deliberately the SECOND `INSTANCE` line, not the only one: `crash::install()`
            // already stamped a fallback identity (argv + cwd) at process start, before argument
            // parsing, config load, asset sync, or GPU init got a chance to kill the process before
            // the listener ever got here (#392). If none of that happens — the port never binds, or
            // this task panics first — the fallback stamp is still on record; this call just adds
            // the more specific `api_port=` line on top of it once the listener is actually up.
            eqoxide_crash::log_instance(&format!("api_port={bound_port}"));
            tracing::info!("camera HTTP: http://127.0.0.1:{bound_port}");
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("camera HTTP: server error: {e}");
            }
        });
    })
    .expect("spawn http-server thread");
}

#[cfg(test)]
mod currency_tests {
    use super::currency_json;

    #[test]
    fn currency_json_maps_coin_slots_to_named_fields() {
        let v = currency_json([12, 3, 45, 6]);
        assert_eq!(v["platinum"], 12);
        assert_eq!(v["gold"], 3);
        assert_eq!(v["silver"], 45);
        assert_eq!(v["copper"], 6);
    }

    #[test]
    fn currency_json_all_zero() {
        let v = currency_json([0, 0, 0, 0]);
        assert_eq!(v["platinum"], 0);
        assert_eq!(v["copper"], 0);
    }
}

/// #522: the agent-facing player view reports the FOOT datum, not the wire (model-origin) datum.
/// `gs.player_z` is FOOT internally; `pos_up` must echo it verbatim so the agent's world model is
/// one datum end to end. MUTATION CHECK: reintroduce `gs.player_z + WIRE_Z_OFFSET` in
/// `from_game_state` → this goes RED.
#[cfg(test)]
mod player_view_datum_tests {
    use super::PlayerState;

    #[test]
    fn pos_up_reports_foot_not_wire() {
        let mut gs = eqoxide_core::game_state::GameState::new();
        gs.player_z = 73.875; // FOOT
        let view = PlayerState::from_game_state(&gs);
        assert_eq!(view.pos_up, 73.875,
            "pos_up must report the internal FOOT datum, not foot + WIRE_Z_OFFSET");
    }
}

/// #477: the dead-session guard for WRITE-command handlers. These prove the guard turns a dead net
/// thread into an honest 503 (never a false 200), that a live/idle session passes, and — via a
/// route-level test — that a gated handler returns 503 AND does NOT enqueue the action.
#[cfg(test)]
mod live_session_guard_tests {
    use super::*;
    use crate::testkit::{ago, empty_state};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// Overwrite the three net-health clocks (ages in seconds) to model a given liveness state; the
    /// probe clocks are left at their `Default` (`None`), which is what the guard reads through
    /// `health()` but never keys its verdict on.
    fn set_clocks(s: &HttpState, datagram_ago: u64, tick_ago: u64, packet_ago: u64) {
        *s.net_health.lock().unwrap() = NetHealth {
            last_datagram: ago(datagram_ago),
            last_packet:   ago(packet_ago),
            last_tick:     ago(tick_ago),
            ..NetHealth::default()
        };
    }

    /// A freshly-built state (all clocks = now) is LIVE — the guard must pass. This is why every
    /// pre-existing handler test (which uses `empty_state`) keeps its status unchanged.
    #[test]
    fn fresh_state_is_live() {
        assert!(require_live_session(&empty_state()).is_ok());
    }

    /// A healthy but IDLE session — connected + ticking, yet the WORLD has been quiet for a full
    /// minute (`last_packet` 60s stale) — must NOT be rejected. This is the #343 over-rejection guard:
    /// keying the verdict on `last_packet` (world quiet) instead of `connected`/`last_tick` (thread
    /// alive) would fail here.
    #[test]
    fn healthy_idle_session_is_not_rejected() {
        let s = empty_state();
        set_clocks(&s, 0, 0, 60);
        assert!(require_live_session(&s).is_ok(),
            "a connected, ticking session must pass even after 60s of world silence");
    }

    /// A dead LINK (no datagram for > CONN_STALE_SECS) → 503, regardless of the tick clock.
    #[test]
    fn disconnected_link_is_503() {
        let s = empty_state();
        set_clocks(&s, CONN_STALE_SECS + 5, 0, CONN_STALE_SECS + 5);
        let (code, msg) = require_live_session(&s).unwrap_err();
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(msg.contains("not connected"), "message: {msg}");
    }

    /// The net thread stopped TICKING while the link is still fresh (a thread that died before the
    /// datagram clock went stale) → 503 via the snapshot-staleness bound, faster than `connected`.
    #[test]
    fn stale_tick_with_live_link_is_503() {
        let s = empty_state();
        // Datagram fresh (connected == true), but no tick for > SESSION_STALE_TICK_MS.
        set_clocks(&s, 0, SESSION_STALE_TICK_MS / 1000 + 1, 0);
        let (code, msg) = require_live_session(&s).unwrap_err();
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(msg.contains("has not ticked"), "message: {msg}");
    }

    /// Mutation guard on the tick bound: a tick just UNDER the staleness bound must still pass, so the
    /// `>=` comparison can't be silently loosened/tightened without a test failing.
    #[test]
    fn tick_just_under_bound_passes() {
        let s = empty_state();
        // ~1s of no tick on a connected link is well within a healthy loop's tolerance.
        set_clocks(&s, 0, 1, 0);
        assert!(require_live_session(&s).is_ok());
    }

    /// Route-level: a WRITE handler (`/v1/move/goto`) on a DEAD session returns 503 and does NOT
    /// enqueue the nav action — the whole point of #477 (no false 200, and nothing left in the slot
    /// for a net thread that will never drain it).
    #[tokio::test]
    async fn write_handler_on_dead_session_is_503_and_does_not_enqueue() {
        let state = empty_state();
        set_clocks(&state, CONN_STALE_SECS + 5, CONN_STALE_SECS + 5, CONN_STALE_SECS + 5);
        let goto_target = state.nav.goto_target.clone();
        let app = super::move_api::router().with_state(state);
        let req = Request::post("/goto")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"x":1.0,"y":2.0,"z":3.0}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE,
            "a WRITE command on a dead session must be an honest 503, never a false 200");
        assert!(goto_target.lock().unwrap().is_none(),
            "a dead-session command must NOT be enqueued — nothing will ever drain it");
    }

    /// Route-level companion: the SAME request on a live session proceeds normally (200 + enqueued),
    /// proving the guard doesn't block the happy path.
    #[tokio::test]
    async fn write_handler_on_live_session_proceeds() {
        let state = empty_state(); // fresh clocks = live
        let goto_target = state.nav.goto_target.clone();
        let app = super::move_api::router().with_state(state);
        let req = Request::post("/goto")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"x":1.0,"y":2.0,"z":3.0}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*goto_target.lock().unwrap(), Some((1.0, 2.0, 3.0)));
    }
}

