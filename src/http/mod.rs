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
//! publish — those `Arc<Mutex<…>>` IPC channel types themselves live in [`crate::ipc`] (re-exported
//! here for the handler submodules below) since they are shared with the network/render threads,
//! not genuine HTTP types. See `docs/http-api.md`.

use axum::Router;
use std::sync::{Arc, Mutex};
use crate::camera_state::{CameraCmd, CameraSnapshot};

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

mod observe;
mod quests;
mod group;
mod guild;
mod move_api;
mod trainer;
mod pet;
mod combat;
mod interact;
mod merchant;
mod inventory;
mod chat;
mod events;
mod social;
mod camera;
mod lifecycle;

// The `Arc<Mutex<…>>` request/snapshot ("IPC channel") types used to live here, but they are shared
// state between this thread, the network thread, and the render/app loop — not genuine HTTP types —
// so they were relocated to the neutral `crate::ipc` (cleanup; pure code motion). This glob re-export
// is a compatibility shim: every handler submodule below still just does `use super::*;` and every
// name it expects (the `*Req`/`*Shared` aliases, `HttpState`'s field types, `NetHealth`, …) keeps
// resolving unqualified. New cross-thread code should `use crate::ipc::…` directly instead of routing
// through here — see `crate::ipc` for the authoritative definitions.
pub use crate::ipc::*;

/// Seconds without any inbound server packet after which the session is reported disconnected
/// (`connected: false`). Generous enough to ride out normal quiet spells; short enough that a
/// dead/frozen server is caught within a few seconds (eqoxide#8).
pub const CONN_STALE_SECS: u64 = 15;

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
    /// mirrors `controller_view` into them every tick in `Navigator::stream_position` (and the
    /// controlled-fall branch writes `player_z` itself), so this is exactly the position the client
    /// streams to the server — no need to reach into the render thread's controller (#343).
    pub fn from_game_state(gs: &crate::game_state::GameState) -> Self {
        PlayerState {
            name:       gs.player_name.clone(),
            zone:       gs.zone_name.clone(),
            race:       gs.player_race.clone(),
            class:      gs.player_class.clone(),
            level:      gs.player_level as u32,
            pos_east:   gs.player_x,
            pos_north:  gs.player_y,
            pos_up:     gs.player_z,
            heading_ccw: gs.player_heading,
            heading_cw:  crate::eq_net::protocol::ccw_to_cw(gs.player_heading),
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
            target_name:   gs.target_id.and_then(|id| gs.entities.get(&id).map(|e| e.name.clone())
                               .or_else(|| gs.target_name.clone())),
            target_hp_pct: gs.target_id.and_then(|id| gs.entities.get(&id).map(|e| e.hp_pct)
                               .or(gs.target_hp_pct)),
            // #292: con difficulty tier + attitude enum (from the last consider) and the target's
            // level, only while something is targeted.
            target_con:      gs.target_id.and(gs.target_con_name.clone()),
            target_attitude: gs.target_id.and(gs.target_attitude.clone()),
            target_level:    gs.target_id.and_then(|id| gs.entities.get(&id)).map(|e| e.level),
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
                                 spell_name: crate::spells::name_of(c.spell_id),
                                 cast_ms:    c.cast_ms,
                                 started:    c.started,
                             }),
            last_cast:       gs.last_cast.as_ref().map(|o| LastCastView {
                                 spell_id:   o.spell_id,
                                 spell_name: crate::spells::name_of(o.spell_id),
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

/// Turn an entity key like "Guard_Phaeton000" into a display name "Guard Phaeton".
pub fn clean_entity_name(raw: &str) -> String {
    raw.trim_end_matches(|c: char| c.is_ascii_digit())
        .replace('_', " ")
        .trim()
        .to_string()
}

/// Render coin `[platinum, gold, silver, copper]` as a JSON object for the API.
pub(crate) fn currency_json(coin: [u32; 4]) -> serde_json::Value {
    serde_json::json!({
        "platinum": coin[0],
        "gold":     coin[1],
        "silver":   coin[2],
        "copper":   coin[3],
    })
}

#[derive(Clone)]
pub(crate) struct HttpState {
    pub(crate) cmd_tx:           Arc<Mutex<Option<CameraCmd>>>,
    pub(crate) snapshot:         Arc<Mutex<CameraSnapshot>>,
    pub(crate) frame_req:        FrameReq,
    pub(crate) goto_target:      GotoTarget,
    pub(crate) goto_entity:      GotoEntity,
    pub(crate) entity_positions: EntityPositions,
    pub(crate) entity_ids:       EntityIds,
    pub(crate) zone_points:      ZonePoints,
    /// Zone collision + region map (shared with the nav thread); read-only here, for zone_exits.
    pub(crate) shared_collision: crate::nav::collision::SharedCollision,
    pub(crate) zone_cross:       ZoneCrossReq,
    /// Aggro-avoidance knobs set by /v1/move/goto|zone_cross and read by the nav walker (#242).
    pub(crate) nav_avoid:        NavAvoidShared,
    /// Manual-move / jump escape hatch (#188), consumed by the render loop.
    pub(crate) manual_move:      ManualMoveReq,
    pub(crate) hail:             HailReq,
    pub(crate) say:              SayReq,
    pub(crate) target:           TargetReq,
    pub(crate) who_req:          WhoReq,
    pub(crate) friends_list:     FriendsListShared,
    pub(crate) friends_req:      FriendsReq,
    pub(crate) attack:           AttackReq,
    pub(crate) cast:             CastReq,
    pub(crate) mem_spell:        MemSpellReq,
    pub(crate) sit:              SitReq,
    pub(crate) consider:         ConsiderReq,
    pub(crate) buy:              BuyReq,
    pub(crate) sell:             SellReq,
    pub(crate) trade:            TradeReq,
    pub(crate) merchant:         MerchantShared,
    pub(crate) move_req:         MoveReq,
    pub(crate) give:             GiveReq,
    pub(crate) inventory:        InventoryShared,
    pub(crate) loot:             LootReq,
    pub(crate) messages:         MessagesShared,
    pub(crate) dialogue:         DialogueShared,
    pub(crate) nav_state:        NavStateShared,
    pub(crate) dialogue_click:   DialogueClickReq,
    pub(crate) chat_events:      ChatEventsShared,
    pub(crate) chat_send:        ChatSendShared,
    pub(crate) spells:           std::sync::Arc<crate::spells::SpellDb>,
    /// The network thread's authoritative `GameState`. Every agent-facing player field is projected
    /// from HERE at read time (`HttpState::player`) — the render loop is no longer in the path (#343).
    pub(crate) game_state:       GameStateSnapshot,
    /// The network thread's three liveness clocks — turned into `Health` on every read (#343).
    pub(crate) net_health:       NetHealthShared,
    /// Render-owned frame timings (the ONLY agent-visible value the render loop publishes).
    pub(crate) frame_profile:    FrameProfileShared,
    pub(crate) task_log:         TaskLog,
    pub(crate) task_offers_shared:    TaskOffersShared,
    pub(crate) completed_tasks_shared: CompletedTasksShared,
    pub(crate) accept_task:           AcceptTaskReq,
    pub(crate) cancel_task:           CancelTaskReq,
    pub(crate) group:             GroupShared,
    pub(crate) group_invite:      GroupInviteReq,
    pub(crate) trainer_open_req:  TrainerOpenReq,
    pub(crate) trainer_train_req: TrainerTrainReq,
    pub(crate) group_accept:      GroupAcceptReq,
    pub(crate) group_decline:     GroupDeclineReq,
    pub(crate) group_leave:       GroupLeaveReq,
    pub(crate) group_kick:        GroupKickReq,
    pub(crate) group_make_leader: GroupMakeLeaderReq,
    pub(crate) door_click:       DoorClickReq,
    pub(crate) doors_shared:     DoorsShared,
    pub(crate) camp:             CampReq,
    pub(crate) camp_until:       CampUntil,
    pub(crate) respawn:          RespawnReq,
    pub(crate) pet_cmd:          PetCmdReq,
    pub(crate) read_book:        ReadBookReq,
    pub(crate) guild:            GuildShared,
    pub(crate) guild_action:     GuildActionReq,
}

impl HttpState {
    /// The agent-facing player view, **derived at read time** from the network thread's `GameState`
    /// snapshot. There is no cached `PlayerState` anywhere, so there is nothing to go stale (#343).
    pub(crate) fn player(&self) -> PlayerState {
        PlayerState::from_game_state(&self.game_state.load())
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
        let world_responsive = world_responsive(
            probe_sent_ago, probe_reply_ago, last_packet_ago,
            std::time::Duration::from_secs(PROBE_TIMEOUT_SECS),
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
            connected:          link_age.as_secs() < CONN_STALE_SECS,
            world_responsive,
            last_world_response_ms,
        }
    }
}

pub fn spawn_camera_server(
    cmd_tx:           Arc<Mutex<Option<CameraCmd>>>,
    snapshot:         Arc<Mutex<CameraSnapshot>>,
    frame_req:        FrameReq,
    goto_target:      GotoTarget,
    goto_entity:      GotoEntity,
    entity_positions: EntityPositions,
    entity_ids:       EntityIds,
    zone_points:      ZonePoints,
    shared_collision: crate::nav::collision::SharedCollision,
    zone_cross:       ZoneCrossReq,
    manual_move:      ManualMoveReq,
    hail:             HailReq,
    say:              SayReq,
    target:           TargetReq,
    who_req:          WhoReq,
    friends_list:     FriendsListShared,
    friends_req:      FriendsReq,
    attack:           AttackReq,
    cast:             CastReq,
    mem_spell:        MemSpellReq,
    sit:              SitReq,
    consider:         ConsiderReq,
    buy:              BuyReq,
    sell:             SellReq,
    trade:            TradeReq,
    merchant:         MerchantShared,
    move_req:         MoveReq,
    give:             GiveReq,
    inventory:        InventoryShared,
    loot:             LootReq,
    messages:         MessagesShared,
    dialogue:         DialogueShared,
    nav_state:        NavStateShared,
    dialogue_click:   DialogueClickReq,
    chat_events:      ChatEventsShared,
    chat_send:        ChatSendShared,
    spells:           std::sync::Arc<crate::spells::SpellDb>,
    game_state:       GameStateSnapshot,
    net_health:       NetHealthShared,
    frame_profile:    FrameProfileShared,
    task_log:         TaskLog,
    task_offers_shared:    TaskOffersShared,
    completed_tasks_shared: CompletedTasksShared,
    accept_task:           AcceptTaskReq,
    cancel_task:           CancelTaskReq,
    group:             GroupShared,
    group_invite:      GroupInviteReq,
    trainer_open_req:  TrainerOpenReq,
    trainer_train_req: TrainerTrainReq,
    group_accept:      GroupAcceptReq,
    group_decline:     GroupDeclineReq,
    group_leave:       GroupLeaveReq,
    group_kick:        GroupKickReq,
    group_make_leader: GroupMakeLeaderReq,
    door_click:       DoorClickReq,
    doors_shared:     DoorsShared,
    camp:             CampReq,
    camp_until:       CampUntil,
    respawn:          RespawnReq,
    pet_cmd:          PetCmdReq,
    nav_avoid:        NavAvoidShared,
    read_book:        ReadBookReq,
    guild:            GuildShared,
    guild_action:     GuildActionReq,
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
            let state = HttpState { cmd_tx, snapshot, frame_req, goto_target, goto_entity, entity_positions, entity_ids, zone_points, shared_collision, zone_cross, manual_move, hail, say, target, who_req, friends_list, friends_req, attack, cast, mem_spell, sit, consider, buy, sell, trade, merchant, move_req, give, inventory, loot, messages, dialogue, nav_state, dialogue_click, chat_events, chat_send, spells, game_state, net_health, frame_profile, task_log, task_offers_shared, completed_tasks_shared, accept_task, cancel_task, group, group_invite, trainer_open_req, trainer_train_req, group_accept, group_decline, group_leave, group_kick, group_make_leader, door_click, doors_shared, camp, camp_until, respawn, pet_cmd, nav_avoid, read_book, guild, guild_action };
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
            crate::crash::log_instance(&format!("api_port={bound_port}"));
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

