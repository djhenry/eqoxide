//! The agent ops: every operation the agent socket serves beyond the verbs the harness owns.
//! An op is named `"<METHOD> /v1/<group>/<action>"` (groups mirror the MCP tool grouping —
//! `observe`, `combat`, `interact`, `merchant`, `inventory`, `chat`, `lifecycle`, …) and is run by
//! [`ApiDispatcher`] from an agent-socket `Call`. Nothing here listens on a socket or speaks HTTP (the harness does);
//! the numeric statuses are kept only because every refusal is already expressed in them.
//!
//! Each group lives in its own submodule (e.g. `combat.rs`) exposing `register(&mut Ops)`, which
//! `v1_ops()` collects. This module holds the cross-cutting pieces: request/response body types
//! (`Health`, …) and `OpState`. Most handlers just write a shared request slot that
//! the navigation thread drains each tick; reads come from snapshots the render/network threads
//! publish — those `Arc<Mutex<…>>` IPC channel types themselves live in [`eqoxide_ipc`]
//! (re-exported here for the handler submodules below) since they are shared with the
//! network/render threads. See `docs/agent-ops.md` for the route reference.

// Every `eqoxide_ipc` type the handlers reference comes in via the `pub use eqoxide_ipc::*;` glob
// below — a separate private `use` here would only shadow it.

mod actions;
mod dispatch;
pub mod wire;
pub use dispatch::ApiDispatcher;
mod observe;
mod quests;
mod group;
mod guild;
mod trainer;
mod combat;
pub mod interact;
mod merchant;
mod events;
mod social;
// #347/B1: the single polarity site for "queued or refused" — see `refusal.rs`.
mod refusal;
// #952/#956: the single site for "you sent two mutually exclusive forms of one argument" — plus the
// guard that makes every Deserialize request struct in this crate declare which kind it is.
mod req_form;

// Shared test fixtures (`OpState` builder + snapshot-seeding helpers). Available to this crate's
// own unit tests and, via the `test-fixtures` feature, to the app crate's integration tests — see
// the module docs. Never compiled into a release build.
#[cfg(any(test, feature = "test-fixtures"))]
pub mod testkit;

// The `Arc<Mutex<…>>` request/snapshot ("IPC channel") types used to live here, but they are shared
// state between this thread, the network thread, and the render/app loop — not genuine op types —
// so they were relocated to the neutral `eqoxide_ipc` (cleanup; pure code motion). This glob re-export
// is a compatibility shim: every handler submodule below still just does `use super::*;` and every
// name it expects (the `*Req`/`*Shared` aliases, `OpState`'s field types, `NetHealth`, …) keeps
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
/// that killed the gameplay thread while the agent-ops layer kept serving — the #470/#477 zombie) lets
/// `snapshot_age_ms` climb this high. Kept comfortably below `CONN_STALE_SECS * 1000` so a thread
/// that died WITHOUT the link having gone stale yet (datagrams may briefly keep arriving after the
/// thread exits) is still caught here, faster than `connected` would flip.
pub const SESSION_STALE_TICK_MS: u64 = 5_000;

/// The slice of the network thread's `GameState` that the observe and trainer ops read.
///
/// **This is a pure projection of `GameState`** — derived on demand by [`OpState::player`], never
/// cached, so there is nothing to go stale (#343). It holds no connection-health or freshness
/// fields: those are time-derived and are computed at READ time from [`NetHealth`]'s clocks.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlayerState {
    pub zone:         String,
    pub pos_east:     f32,
    pub pos_north:    f32,
    pub pos_up:       f32,
    pub mem_spells:   [u32; 9],
    /// Player skill values by skill id (0..77), for GET /v1/observe/skills (eqoxide#99).
    pub skills:       Vec<u32>,
    /// Whether a guildmaster training window is open, and the caps it offers per skill id (#99).
    pub trainer_open:   bool,
    pub trainer_skills: Vec<u32>,
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
    /// Text of the most recently read book/note (OP_ReadBook reply), newline-decoded. None until a
    /// book is read. Surfaced via GET /v1/observe/item_text so an agent can read a quest note (#288).
    pub book_text:          Option<String>,
}

impl PlayerState {
    /// Project the network thread's authoritative `GameState` into the agent-facing view.
    ///
    /// `gs.player_x/y/z` are the *controller's* position: the net thread mirrors `controller_view`
    /// into them every tick in `ActionLoop::stream_position` (and the controlled-fall branch writes
    /// `player_z` itself), so this is exactly the position the client streams to the server — no
    /// need to reach into the render thread's controller (#343).
    pub fn from_game_state(gs: &eqoxide_core::game_state::GameState) -> Self {
        PlayerState {
            zone:       gs.world.zone_name.clone(),
            pos_east:   gs.player_x,
            pos_north:  gs.player_y,
            // FOOT datum (#522, see coord::WIRE_Z_OFFSET): every agent-facing position reports the
            // collision-floor/foot height, the SAME datum used internally (controller, gs.player_z,
            // collision) and by the observed entities (entities are converted wire→foot at ingest).
            // One datum end to end means a position the agent READS here can be fed straight back
            // into coords without a 3u skew, and self reads at the same height as another player
            // standing on the same plank. The wire↔foot conversion happens ONLY at the packet edge
            // (outbound adds the offset in OP_ClientUpdate; inbound subtracts it), never here.
            pos_up:     gs.player_z,
            mem_spells: gs.mem_spells,
            skills:     gs.player_skills.clone(),
            trainer_open:   gs.trainer_open.is_some(),
            trainer_skills: gs.trainer_skills.clone(),
            coin:       gs.coin,
            coin_verified: gs.coin_verified(),
            book_text:       gs.last_book_text.clone(),
        }
    }
}

/// Connection/freshness health, computed fresh on every op read — never stored (#343).
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
    /// #612: how many outbound datagrams the client BUILT but could not put on the wire (`try_send`
    /// returned an error). Cumulative since process start. Before #612 this was unobservable — every
    /// send error was discarded, so a packet that never left the machine looked exactly like one the
    /// server received.
    ///
    /// **0 IS the expected healthy reading since #641.** The 283-on-a-healthy-`qeynos`-login this
    /// doc used to warn about are now recovered by two paths (an immediate direct `send(2)` retry,
    /// and a deferral queue) and counted in `send_wouldblock_rescued` / `send_deferred`. A nonzero
    /// value here means a datagram never reached the wire and nothing will re-send it.
    /// See [`eqoxide_ipc::NetHealth::send_failures`].
    pub send_failures:          u64,
    /// #641: datagrams whose `try_send` returned `WouldBlock` and which an immediate direct
    /// `send(2)` then accepted. **Not failures** — they reached the wire. An UPPER BOUND on tokio's
    /// synthetic-`WouldBlock` case, not a measurement of it (a kernel refusal whose buffer drained
    /// in between is indistinguishable). Read as a load signal.
    /// See [`eqoxide_ipc::NetHealth::send_wouldblock_rescued`].
    pub send_wouldblock_rescued: u64,
    /// #641: how many **datagrams** a transient refusal caused to be queued for retry on a later
    /// tick instead of dropped (control datagrams only). Counted once per datagram, not per refusal.
    /// **Not a loss counter** — but **not disjoint from `send_failures`** either: a queued datagram
    /// that is later abandoned (queue overflow, or the session ending) is counted in both.
    /// See [`eqoxide_ipc::NetHealth::send_deferred`].
    pub send_deferred: u64,
    /// #656: is the io driver's send path starved RIGHT NOW — a `send_wouldblock_rescued`/
    /// `send_deferred` burst happening currently, as opposed to having happened at some point in
    /// the past? `send_wouldblock_rescued`/`send_deferred` above are cumulative since process start
    /// and can only grow, so before this field nothing could tell "starved once, an hour ago" from
    /// "starved right now" — this is the previously-missing ALERT #656 asked for.
    ///
    /// Computed at READ time (#343) from [`eqoxide_ipc::send_starved`], fed the CURRENT streak of
    /// consecutive pressure events and the age of the most recent one. It is a RATE/recency signal,
    /// not another lifetime count: it fires once a burst crosses
    /// [`eqoxide_ipc::SEND_PRESSURE_FIRE_THRESHOLD`] consecutive events, and CLEARS on its own once
    /// no new pressure event has arrived within [`eqoxide_ipc::SEND_PRESSURE_BURST_GAP_SECS`] —
    /// regardless of how large the historical streak or the lifetime counters are. See
    /// [`eqoxide_ipc::send_starved`] for the full fire/clear contract.
    pub send_starved: bool,
    /// #612: the subset of `send_failures` that the client does not retransmit itself (unreliable
    /// position updates, ACKs, keepalives, session control). The complement is the reliable stream,
    /// which recovers structurally via the resend window. See [`eqoxide_ipc::NetHealth`] for the
    /// precise contract — in particular, this is NOT "a command was lost".
    pub send_failures_unretried: u64,
    /// #612: `ErrorKind` of the most recent send failure. `None` if no send has ever failed.
    /// Kept as the `ErrorKind` (not a `String`) so `Health` stays `Copy` like the rest of this
    /// struct; it serializes as its `Debug` name (`"WouldBlock"`, `"Uncategorized"`, …).
    #[serde(serialize_with = "ser_error_kind")]
    pub last_send_error:        Option<std::io::ErrorKind>,
    /// #612: ms since the most recent send failure, measured at READ time (#343). `None` if none.
    /// Lets an agent tell "one WouldBlock at login an hour ago" from "failing right now".
    pub last_send_error_age_ms: Option<u64>,
    /// #612 (review F1): un-ACKed RELIABLE datagrams abandoned when a session ended — the case the
    /// resend window does NOT cover, because the next session's window starts empty. Measured 0
    /// across three clean zone handoffs, so treat a nonzero value DURING PLAY as signal (clean
    /// shutdown is the measured exception — 4 and 8 on two live exits, cause not established).
    /// Since #642 it ALSO covers a server-side `resend_timeout` drop WHEN the client observes it
    /// (OP_SessionDisconnect/OP_OutOfSession or a closed socket → `session_drop` → teardown → stream
    /// drop); a server drop into total silence still counts nothing there. See
    /// [`eqoxide_ipc::NetHealth::reliable_abandoned`] for the full contract and coverage list (it is
    /// an upper bound on abandoned reliable payload, not a proven count of lost commands).
    pub reliable_abandoned:     u64,
    /// #642: why the session is over, when the client has POSITIVELY OBSERVED its end (inbound
    /// `OP_SessionDisconnect`/`OP_OutOfSession`, or a closed socket) — `null` while the session is
    /// live. Distinct from `connected`, which only goes false after `CONN_STALE_SECS` of SILENCE:
    /// this is immediate and explicit, and when it is set `connected` is forced `false` (see
    /// `OpState::health`), so a server-side drop can never be reported as connected. Serialized as a
    /// stable snake_case string (`server_disconnect` / `out_of_session` / `socket_closed`).
    /// See [`eqoxide_ipc::SessionDropCause`].
    #[serde(serialize_with = "ser_session_drop")]
    pub session_drop:           Option<eqoxide_ipc::SessionDropCause>,
}

/// Serializes [`SessionDropCause`](eqoxide_ipc::SessionDropCause) as its stable snake_case name (or
/// `null`) so `Health::session_drop` reaches the agent as a machine-readable string while `Health`
/// stays `Copy` (#642).
fn ser_session_drop<S: serde::Serializer>(
    c: &Option<eqoxide_ipc::SessionDropCause>, s: S,
) -> Result<S::Ok, S::Error> {
    match c {
        Some(c) => s.serialize_some(c.as_str()),
        None => s.serialize_none(),
    }
}

/// Serializes an `io::ErrorKind` as its `Debug` name so `Health::last_send_error` can stay `Copy`
/// while still reaching the agent as a readable string (#612).
fn ser_error_kind<S: serde::Serializer>(
    k: &Option<std::io::ErrorKind>, s: S,
) -> Result<S::Ok, S::Error> {
    match k {
        Some(k) => s.serialize_some(&format!("{k:?}")),
        None => s.serialize_none(),
    }
}

/// Relocated to `eqoxide-core::game_state` (#544 Step 2o) so `eqoxide-ui` can use it without an
/// up-reference into this crate. Re-exported so every existing `crate::clean_entity_name` /
/// `eqoxide_agent_ops::clean_entity_name` call site keeps resolving unchanged.
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
/// cross-thread channel the net thread drains. See `ipc.rs` and `main.rs` wiring.
///
/// A few bundle fields are unused on THIS side of a channel (e.g. `camera.manual_move` is written by
/// a handler and read by the render thread, never drained here) — that's expected: the bundle
/// boundary is the DOMAIN, not "exactly the fields this struct touches".
// `pub` (not `pub(crate)`) only so downstream integration tests can NAME the type that
// `testkit::empty_state`/`debug_json` hand back (they can't otherwise hold a value of it across the
// crate boundary). Its fields stay `pub(crate)`, so it remains un-constructible outside this crate —
// only `start_agent_api` / `testkit::empty_state` build one. (#544 Step 2l)
#[derive(Clone)]
pub struct OpState {
    /// The live entity registry + zone exit points (#M4).
    pub(crate) world:           eqoxide_ipc::WorldSlots,
    /// The zone terrain+collision LOAD STATE (#579, agent-honesty). Written only by the app thread
    /// that owns the zone loader. Every endpoint that describes the WORLD (as opposed to the
    /// character or the session) must consult this first: while it is `Pending`, the client is
    /// standing on a placeholder ground plane with no collision, and reporting that as the zone is
    /// the false-empty that produced the bogus #560 report.
    pub(crate) zone_assets: eqoxide_zone_geometry::zone_assets::ZoneAssetStateShared,
    /// The `eq-net` thread's terminal death (#634, agent-honesty). `None` while the Model — the sole
    /// writer of `GameState`, sole drainer of the command slots, sole stamper of `NetHealth` — is
    /// running; `Some(reason)` once it has ended for ANY reason (panic, fatal error, clean shutdown)
    /// and therefore will never publish again. This is the field that tells an agent the frozen world
    /// it is reading is frozen FOREVER, which no age can say. Written only by
    /// `eqoxide::model::run_net_thread`; same shared-`Arc`-identity discipline as the two above.
    pub(crate) net_thread_dead: eqoxide_ipc::NetThreadDeadShared,
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
    /// from HERE at read time (`OpState::player`) — the render loop is no longer in the path (#343).
    pub(crate) game_state:       GameStateSnapshot,
    /// The network thread's three liveness clocks — turned into `Health` on every read (#343).
    pub(crate) net_health:       NetHealthShared,
    /// `/v1/quests/*` slots (#M4).
    pub(crate) quest:           eqoxide_ipc::QuestSlots,
    /// `/v1/group/*` slots (#M4).
    pub(crate) group_slots:     eqoxide_ipc::GroupSlots,
    /// `/v1/guild/*` slots (#M4).
    pub(crate) guild_slots:     eqoxide_ipc::GuildSlots,
}

impl OpState {
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
        // #760: EVERY age below is measured back from `h.clock`, never from a bare `Instant::now()`.
        // In production that clock IS `Instant::now()` (`HealthClock::WALL`, the only clock a
        // non-test build can construct), so this is the same read-time measurement #343 requires.
        // In a TEST it is pinned, which is what makes a fixture's liveness verdict a pure function
        // of the fixture instead of a function of how long the test took to reach this line. If you
        // add a field here, derive its age from `h.clock` too — a stray `.elapsed()` silently opts
        // that one field back into wall time and re-arms #760's flake for it.
        let link_age        = h.clock.age_of(h.last_datagram);
        let last_packet_ago = h.clock.age_of(h.last_packet);
        // #371: the active-probe verdict, all measured at read time (like every other health field,
        // per #343 — never cached, so no live publisher has to run for the answer to stay honest).
        // NOTE: the timeout check is measured against `first_unanswered_probe_sent`, NOT
        // `last_probe_sent` — the latter is bumped by every 30s resend and would let a permanently
        // wedged zone re-earn the 10s in-flight grace window forever (the #371-followup bug).
        let probe_sent_ago  = h.first_unanswered_probe_sent.map(|t| h.clock.age_of(t));
        let probe_reply_ago = h.last_probe_reply.map(|t| h.clock.age_of(t));
        // #470: `world_responsive` must know the LINK is dead. When a failed world-reconnect kills the
        // net thread the prober dies too, so no probe is ever outstanding — without this, the "no
        // probe" branch reported a zombie session alive forever. `connected` here is the SAME value
        // published in `Health` below (both derived from `last_datagram` vs CONN_STALE_SECS).
        // #642: a POSITIVELY-OBSERVED server-side drop (OP_SessionDisconnect/OP_OutOfSession or a
        // closed socket) forces `connected` false IMMEDIATELY, rather than waiting out CONN_STALE_SECS
        // of link silence. This is the type-level guarantee the honesty invariant asks for: once
        // `session_drop` is `Some`, there is no derivation of `connected` that reads `true` for the
        // dead session — the only thing that clears `session_drop` is a fresh OP_SessionResponse.
        let connected = link_age.as_secs() < CONN_STALE_SECS && h.session_drop.is_none();
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
        // #656: the io-starvation alert. Measured HERE, at read time (#343's rule for every age in
        // this payload) — `last_send_pressure_at` is a timestamp, never a pre-computed bool, so the
        // alert cannot go stale between the net thread's last write and this op read.
        let send_pressure_ago = h.last_send_pressure_at.map(|t| h.clock.age_of(t));
        let send_starved = eqoxide_ipc::send_starved(h.send_pressure_streak, send_pressure_ago);
        Health {
            link_age_ms:        link_age.as_millis() as u64,
            last_packet_age_ms: last_packet_ago.as_millis() as u64,
            snapshot_age_ms:    h.clock.age_of(h.last_tick).as_millis() as u64,
            // Link liveness, NOT world activity — see `NetHealth`. An idle session goes 40+s with
            // no application packet while the session layer keeps ACKing; calling that "disconnected"
            // would be just as much a lie as #343's frozen `connected: true`.
            connected,
            world_responsive,
            last_world_response_ms,
            // #612: outbound send failures. Counters are read straight off `NetHealth` (the net
            // thread's `EqStream::transmit` is their sole writer); the AGE is measured here, at read
            // time, like every other duration in this projection (#343).
            send_failures:           h.send_failures,
            send_wouldblock_rescued: h.send_wouldblock_rescued,
            send_deferred:           h.send_deferred,
            send_starved,
            send_failures_unretried: h.send_failures_unretried,
            last_send_error:         h.last_send_error_kind,
            last_send_error_age_ms:  h.last_send_error_at.map(|t| h.clock.age_of(t).as_millis() as u64),
            reliable_abandoned:      h.reliable_abandoned,
            session_drop:            h.session_drop,
        }
    }
}

/// #477: reject a WRITE command when the game SESSION is not live, so an agent never gets a false
/// `200 OK` for an action that goes nowhere. This is the other half of the #470 zombie: when a failed
/// world-reconnect exits the gameplay NET THREAD, the agent-ops layer keeps running and every WRITE
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
/// The "dead" verdict is keyed on three INDEPENDENT net-thread-liveness signals, never on transient
/// quiet, so a healthy idle session (connected + ticking, but no application traffic for tens of
/// seconds) always passes (#343):
///   - `net_thread_dead != None` (#634) — the net thread PUBLISHED its own death on the way out
///     (panic / fatal error / clean exit). Unlike the two clocks below this is immediate and
///     unambiguous: it cannot fire on a merely slow loop, and it can never clear.
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
pub(crate) fn require_live_session(s: &OpState) -> Result<(), (crate::wire::StatusCode, String)> {
    // #634: the net thread has published its own death. This is checked FIRST because it is both
    // IMMEDIATE (no 5s tick-staleness or 15s link-staleness wait) and TERMINAL (the two clock-based
    // verdicts below are ambiguous between "wedged, may recover" and "gone"; this one is not).
    if let Some(death) = s.net_thread_dead.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        // "is not running" rather than "is dead": the same slot also carries `NeverStarted`
        // (`--testzone`, where no thread was ever spawned), and the agent-ops layer runs in that mode
        // (#890). The relayed reason says which state it is; this prefix must be true of all of them.
        let reason = death.reason();
        return Err((
            crate::wire::StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "the network thread is not running — {reason} This command was NOT sent and will \
                 not take effect. Do not retry: nothing will drain it. See GET /v1/observe/debug \
                 (`net_thread_dead`)."
            ),
        ));
    }
    let h = s.health();
    if !h.connected {
        return Err((
            crate::wire::StatusCode::SERVICE_UNAVAILABLE,
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
            crate::wire::StatusCode::SERVICE_UNAVAILABLE,
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

/// Every op the agent socket serves, by name (`"<METHOD> /v1/<group>/<action>"`).
pub(crate) fn v1_ops() -> wire::Ops {
    let mut ops = wire::Ops::default();
    trainer::register(&mut ops);
    social::register(&mut ops);
    quests::register(&mut ops);
    group::register(&mut ops);
    merchant::register(&mut ops);
    guild::register(&mut ops);
    events::register(&mut ops);
    combat::register(&mut ops);
    observe::register(&mut ops);
    interact::register(&mut ops);
    ops
}

#[allow(clippy::too_many_arguments)]
pub fn start_agent_api(
    world:           eqoxide_ipc::WorldSlots,
    zone_assets:      eqoxide_zone_geometry::zone_assets::ZoneAssetStateShared,
    net_thread_dead:      eqoxide_ipc::NetThreadDeadShared,
    command:         eqoxide_command::CommandState,
    social:          eqoxide_ipc::SocialSlots,
    merchant_slots:  eqoxide_ipc::MerchantSlots,
    inventory_slots: eqoxide_ipc::InventorySlots,
    interact:        eqoxide_ipc::InteractSlots,
    chat:            eqoxide_ipc::ChatSlots,
    spells:           std::sync::Arc<eqoxide_core::spells::SpellDb>,
    game_state:       GameStateSnapshot,
    net_health:       NetHealthShared,
    quest:           eqoxide_ipc::QuestSlots,
    group_slots:     eqoxide_ipc::GroupSlots,
    guild_slots:     eqoxide_ipc::GuildSlots,
) -> dispatch::ApiDispatcher {
    let state = OpState {
        world, zone_assets, net_thread_dead, command, social, merchant_slots,
        inventory_slots, interact, chat, spells, game_state, net_health,
        quest, group_slots, guild_slots,
    };
    let dispatcher = dispatch::ApiDispatcher::new(&state);
    // Named (see #380 — a panic hook can only say WHICH thread died if the thread has a name;
    // the default anonymous name would just show up as '<unnamed>' in the crash log). The thread
    // only runs the action watchdog; calls themselves run on the Agent Plugin host's runtime.
    std::thread::Builder::new()
        .name("agent-api-watchdog".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("agent api tokio runtime");
            rt.block_on(async move {
                let _watchdog = actions::watchdog(state);
                std::future::pending::<()>().await;
            });
        })
        .expect("spawn agent-api-watchdog thread");
    dispatcher
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
    use crate::wire::StatusCode;
    use crate::testkit::empty_state;
    /// Overwrite the three net-health clocks (ages in seconds) to model a given liveness state; the
    /// probe clocks are left at their `Default` (`None`), which is what the guard reads through
    /// `health()` but never keys its verdict on.
    ///
    /// #760: the clock is PINNED at the same instant the stamps are taken from, so the ages
    /// `health()` reads back are exactly the numbers passed in. Before, the stamps were wall-clock
    /// and the read happened later, so the real age was `arg + (however long the test then took)`.
    /// By construction that put `tick_just_under_bound_passes` (1s, bound 5s) on a 4s margin that
    /// machine load could eat — the same mechanism as #760, one test over. Not observed failing;
    /// stated as a derivation from the old code, not as a measurement.
    fn set_clocks(s: &OpState, datagram_ago: u64, tick_ago: u64, packet_ago: u64) {
        *s.net_health.lock().unwrap() = NetHealth::frozen_at(
            std::time::Instant::now(), datagram_ago, tick_ago, packet_ago,
        );
    }

    /// A freshly-built state (all clocks = now) is LIVE — the guard must pass. This is why every
    /// pre-existing handler test (which uses `empty_state`) keeps its status unchanged.
    #[test]
    fn fresh_state_is_live() {
        assert!(require_live_session(&empty_state()).is_ok());
    }

    /// #760: `health()` measures every age against `NetHealth::clock`, not against a fresh
    /// `Instant::now()`. Proved by pinning the clock 7s AHEAD of the stamps and reading the ages
    /// back exactly — no sleeping, no tolerance window. Swap any `h.clock.age_of(x)` in `health()`
    /// back to `x.elapsed()` and the corresponding assertion here reads ~0 instead of 7000.
    ///
    /// This is the half that makes the frozen fixture mean anything: a fixture can pin its clock all
    /// it likes, but only if the projection actually reads that clock does the wall clock stop
    /// deciding a test's status code.
    #[test]
    fn health_ages_are_measured_against_the_injected_clock() {
        let s = empty_state();
        let stamped = std::time::Instant::now();
        *s.net_health.lock().unwrap() = NetHealth {
            // Clock pinned 7s after the stamps → every age must read exactly 7000ms.
            clock: HealthClock::frozen_at(stamped + std::time::Duration::from_secs(7)),
            last_datagram: stamped,
            last_packet:   stamped,
            last_tick:     stamped,
            ..NetHealth::default()
        };
        let h = s.health();
        assert_eq!(h.snapshot_age_ms, 7_000, "snapshot_age_ms must come from the injected clock");
        assert_eq!(h.link_age_ms, 7_000, "link_age_ms must come from the injected clock");
        assert_eq!(h.last_packet_age_ms, 7_000, "last_packet_age_ms must come from the injected clock");
        // And the guard's verdict follows that injected reading, not the wall: 7s > 5s bound.
        let (code, msg) = require_live_session(&s).unwrap_err();
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(msg.contains("has not ticked in 7000ms"), "message: {msg}");
    }

    /// #760: the shared handler fixture pins its clock, so no handler test's status can depend on
    /// how loaded the machine was. Delete the `frozen_at` in `testkit::empty_state` (back to
    /// `NetHealth::default()`) and this goes RED immediately — `is_frozen()` is false — which is the
    /// point: the property is structural, not a margin.
    #[test]
    fn handler_fixture_pins_its_health_clock() {
        let s = empty_state();
        assert!(s.net_health.lock().unwrap().clock.is_frozen(),
            "empty_state() must pin the health clock so gated handler tests cannot age into a 503");
        let h = s.health();
        assert_eq!((h.snapshot_age_ms, h.link_age_ms, h.last_packet_age_ms), (0, 0, 0),
            "a pinned fixture's ages are exactly zero, not merely small");
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

    /// #634: the net thread published its own death. Every clock is FRESH — this is the window
    /// immediately after the thread dies, before `snapshot_age_ms` has had 5s to climb and before
    /// `connected` has had 15s to flip — so both pre-existing signals still say "live". Only the
    /// explicit terminal state can reject here, and it must, because nothing will ever drain the
    /// command. Delete the `net_thread_dead` check in `require_live_session` and this goes RED.
    #[test]
    fn dead_net_thread_is_503_immediately_even_with_perfectly_fresh_clocks() {
        let s = empty_state();
        assert!(require_live_session(&s).is_ok(), "precondition: fresh clocks are otherwise live");
        *s.net_thread_dead.lock().unwrap() = Some(eqoxide_ipc::NetThreadDeath::new(
            eqoxide_ipc::NetThreadEnd::Panicked, "the eq-net thread PANICKED (boom)."));
        let (code, msg) = require_live_session(&s).unwrap_err();
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(msg.contains("network thread is not running"), "message: {msg}");
        assert!(msg.contains("PANICKED"), "the published reason must be relayed verbatim: {msg}");
        assert!(msg.contains("Do not retry"), "a terminal failure must say so: {msg}");
    }

    /// The SAME guard on `--testzone`'s `NeverStarted`, which is not a death at all (#890 review
    /// B2c). The refusal must still fire — nothing will drain the command either way — but the
    /// sentence must not tell an agent a thread died when none was ever spawned. Restore the old
    /// "the network thread is dead" prefix and this goes RED while the test above stays green.
    #[test]
    fn a_net_thread_that_was_never_started_is_refused_without_claiming_it_died() {
        let s = empty_state();
        *s.net_thread_dead.lock().unwrap() = Some(eqoxide_ipc::NetThreadDeath::new(
            eqoxide_ipc::NetThreadEnd::NeverStarted,
            "--testzone: the eq-net thread was never started (offline renderer mode)."));
        let (code, msg) = require_live_session(&s).unwrap_err();
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!msg.contains("is dead"),
            "nothing died here — the thread was never spawned: {msg}");
        assert!(msg.contains("never started"),
            "the published reason must be relayed verbatim: {msg}");
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

}


#[cfg(test)]
mod health_serde_tests {
    /// #612 review (F6): `Health` derives `Serialize` and its `last_send_error` field needs
    /// `ser_error_kind` to do so (an `io::ErrorKind` is not `Serialize`, and the field is kept as an
    /// `ErrorKind` so `Health` stays `Copy`). No production path serializes `Health` directly today
    /// — the agent observation projects the fields individually — so without this test the helper is
    /// dead code that would rot silently and break the derive the day something does serialize it.
    /// Kept rather than deleted because the alternative is dropping `Serialize` from a public type.
    #[test]
    fn health_serializes_its_send_error_kind_as_a_name_and_null_when_absent() {
        let h = super::Health {
            link_age_ms: 0, last_packet_age_ms: 0, snapshot_age_ms: 0,
            connected: true, world_responsive: true, last_world_response_ms: 0,
            send_failures: 3, send_wouldblock_rescued: 5, send_deferred: 9,
            send_starved: false,
            send_failures_unretried: 1,
            last_send_error: Some(std::io::ErrorKind::WouldBlock),
            last_send_error_age_ms: Some(42), reliable_abandoned: 7,
            session_drop: Some(eqoxide_ipc::SessionDropCause::ServerDisconnect),
        };
        let v: serde_json::Value = serde_json::to_value(h).unwrap();
        assert_eq!(v["last_send_error"], serde_json::json!("WouldBlock"),
            "an ErrorKind must serialize as its readable name, not as a struct or an integer");
        assert_eq!(v["send_failures"], serde_json::json!(3));
        assert_eq!(v["reliable_abandoned"], serde_json::json!(7));
        // #656: the alert field must be PRESENT and boolean, never omitted (an absent JSON key
        // reads as `null` to a naive caller, not as "healthy") and never a lie in either direction.
        assert_eq!(v["send_starved"], serde_json::json!(false),
            "send_starved must serialize as an explicit false when the fixture is not starved (#656)");

        let starved = super::Health { send_starved: true, ..h };
        let v: serde_json::Value = serde_json::to_value(starved).unwrap();
        assert_eq!(v["send_starved"], serde_json::json!(true),
            "send_starved must serialize as an explicit true when the fixture IS starved (#656) — \
             both the fired and the cleared shape must be honest, not just one of them");
        // #642: SessionDropCause must serialize as its stable snake_case name, not the PascalCase
        // variant a derived Serialize would emit.
        assert_eq!(v["session_drop"], serde_json::json!("server_disconnect"),
            "a session-drop cause must serialize as its snake_case wire name (#642)");

        let none = super::Health { last_send_error: None, session_drop: None, ..h };
        let v: serde_json::Value = serde_json::to_value(none).unwrap();
        assert!(v["last_send_error"].is_null(),
            "absence must serialize as an explicit null, not be omitted (#612)");
        assert!(v["session_drop"].is_null(),
            "a live session must serialize session_drop as an explicit null (#642)");
    }
}

/// Reach control for `observe::tests::no_past_dated_net_health_stamp_is_taken_from_a_clock_other_than_the_one_that_reads_it` (#760/C1).
///
/// That guard scans this file's source text. A scanner that silently stops early — as it did, when
/// a `/*` inside a route glob in a doc comment latched its block-comment state — reports
/// a clean scan of a corpus it never read, which is a confident falsehood. The guard asserts it can
/// SEE this constant; because it is the last item in the file, seeing it proves the scan arrived at
/// the end. **Keep it last.**
#[cfg(test)]
pub(crate) const GUARD_REACH_SENTINEL_LIB: u8 = 0;
