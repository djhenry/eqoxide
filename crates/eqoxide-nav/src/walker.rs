//! The nav PATH-WALKER (M1 extraction out of `eq_net::action_loop::ActionLoop`).
//!
//! `Walker` owns the `/goto` state machine: the coarse/fine route, the pure-pursuit steering
//! cursor, stall/back-off/oscillation recovery, controlled-fall edges, and the in-zone portal
//! escape. It is driven once per nav tick by `ActionLoop::tick`, in exactly the call order the
//! old inline `tick()` used (see that method's doc comment for the sequence).
//!
//! # The intent-only movement boundary
//!
//! **`Walker` cannot move the player.** It does not hold `ControllerSlots` (no
//! `controller_view`, no `pos_correction`) — only [`eqoxide_ipc::NavIntent`], the same per-frame
//! [`eqoxide_ipc::MoveIntent`] slot native WASD input writes in `app.rs`. The render-thread
//! `CharacterController` (`src/movement.rs`) is the ONLY thing that ever integrates a position from
//! that intent (collide-and-slide, step-up, gravity, buoyancy). `Walker` reads the player's
//! position from `GameState` (published by `ActionLoop::stream_position`, which mirrors the
//! controller's authoritative pose) and never writes it.
//!
//! There is no longer any position exception: §442 (#442) retired the controlled-fall handoff (the
//! old un-collided `gs.player_z` descent). A big drop is no longer special — `drive_walk` just keeps
//! walking toward the goal and the render controller's ONE collided gravity path descends off the
//! edge; the landing damage is applied driver-agnostically in `ActionLoop::stream_position` from the
//! controller's own tracked airborne height. `Walker` never touches `gs.player_*`, `EqStream`, or
//! the controller — it writes only the per-frame `nav_intent`.

use eqoxide_core::coord::eq_heading;
use eqoxide_core::physics::fall_damage;
use eqoxide_core::game_state::GameState;
use eqoxide_ipc::MoveIntent;
use crate::steering::*;

/// Native Titanium base run speed — see `eq_net::action_loop::RUN_SPEED` for the derivation. Kept
/// as one constant there (both `Walker` and `ActionLoop::drive_auto_engage_melee` need it) rather
/// than duplicated; `nav::steering` already reaches into it the same way (see its `advance_cursor`
/// test fixtures).
use eqoxide_core::physics::{RUN_SPEED, WALK_SPEED};

/// Radius the pure-pursuit carrot's line-of-sight clamp sweeps when deciding whether the straight
/// walker→carrot aim would cross geometry (#685). It is the character's OWN collision radius, so the
/// clamp asks exactly the controller's question — "would my body cross a wall going straight there" —
/// via the same `Collision::path_clear` volume-sweep the controller moves under and A* validates fine
/// edges with (#358). Kept at `PLAYER_RADIUS` (not padded wider) precisely so the clamp trips ONLY on
/// a real corner cut and never on merely hugging a straight wall — the over-tightening #685 must avoid.
pub(crate) const STEER_LOS_CLEARANCE: f32 = eqoxide_core::physics::PLAYER_RADIUS;


/// Buffer (beyond the body radius) the committed coarse route is inflated OFF convex wall corners by,
/// so the walker takes one smooth wider arc with clearance rather than hugging/wiggling the apex
/// (#685, owner-directed). Modest by design: `radius(1) + buffer(2) = 3u` desired wall clearance, well
/// under the clearance field's 4u spoke horizon, and bounded per-waypoint by the opposite wall so a
/// narrow corridor is centred, never widened into the far wall. See `Collision::inflate_route_off_corners`.
const CORNER_BUFFER: f32 = 2.0;

/// The nav state published while this client has NO collision grid for the current zone — the
/// terrain assets are still loading, or their load failed (#579). It is NOT `blocked` (there is no
/// obstacle), NOT `no_path` (no search was ever run) and above all NOT `navigating`: the honest
/// answer is "I have no model of this world yet, so I cannot tell you anything about routes here."
/// Read `zone_assets` on GET /v1/observe/debug to tell *pending* from *failed*.
pub const NAV_STATE_ZONE_LOADING: &str = "zone_loading";

/// Terminal `nav_state` published when navigation is halted because the player is DEAD (#644). A
/// slain character abandons its route (#238); before, that was reported as the ambiguous `idle`
/// (which also means "ready for work"), so an agent that issued a goto and then polled saw `idle`
/// and could not tell "arrived / ready" from "you died and went nowhere". `dead` names the
/// condition honestly and clears back to `idle` on respawn (see `Walker::resolve_goal`).
pub const NAV_STATE_DEAD: &str = "dead";

/// The CLOSED set of `nav_state` words that are a finished OUTCOME — an answer an agent may read
/// after the goal that produced it is gone (#725).
///
/// Everything else is an IN-PROGRESS word: it asserts that something is still happening, so it is
/// only true while a goal is actually in flight. [`Walker::resolve_goal`] therefore retires any
/// state NOT listed here the moment there is no goto goal and no queued zone-cross left to justify
/// it — see the argument there for why the rule is stated this way round.
///
/// `dead` is deliberately ABSENT: it is terminal *for the goal*, but it must clear on respawn
/// (#644), which is exactly a retirement. `zone_loading` is absent for the same reason — it is a
/// promise that a route is still coming.
pub const TERMINAL_NAV_STATES: [&str; 5] = ["idle", "arrived", "no_path", "search_exhausted", "blocked"];

/// Is `state` a finished outcome (see [`TERMINAL_NAV_STATES`]) rather than a claim that work is
/// still in flight? An unrecognised word is treated as IN-PROGRESS.
///
/// **That is the safe direction for the #725 defect class, and it is NOT safe in the other
/// direction — state both, because only one of them is obvious.** A future *in-progress* word
/// nobody adds here retires honestly instead of sticking forever, which is the whole point. But a
/// future *terminal* word nobody adds here is **retired to `idle`/`goal_dropped` one tick after it
/// is published**, replacing a true outcome with a false one — #725's defect class running
/// backwards, and strictly worse than what it replaced (an agent polling for its answer would see
/// the answer, then see it vanish). So: **adding a genuinely terminal `nav_state` word REQUIRES
/// adding it to [`TERMINAL_NAV_STATES`] in the same change.** The array is the contract, not a
/// convenience list, and `docs/http-api.md`'s state table must agree with it.
pub fn nav_state_is_terminal(state: &str) -> bool { TERMINAL_NAV_STATES.contains(&state) }

/// `nav_reason` on the `idle` a goal is retired to when it vanished without ever producing an
/// outcome (#725) — the accepted request was dropped, cancelled elsewhere, or its chase target
/// despawned. Distinguishes "idle because your goal quietly went nowhere" from "idle, ready for
/// work", which are the same word and were previously indistinguishable.
pub const NAV_REASON_GOAL_DROPPED: &str = "goal_dropped";

/// `nav_reason` on the `idle` that [`NAV_STATE_DEAD`] retires to once the character is alive again
/// (#644) — "idle because you respawned", not "idle, nothing happened".
pub const NAV_REASON_RESPAWNED: &str = "respawned";

/// `nav_reason` on the `idle` that [`Walker::reset_for_zone_change`] publishes when the character
/// changes zone — **including the SUCCESS path of `/v1/move/zone_cross`** (#725 review, B1).
///
/// Without it, a crossing that worked published bare `idle` + `nav_reason: null`, which is
/// byte-identical to "no request was ever outstanding": the agent that asked to cross, and whose
/// cross succeeded in about a second, could not tell success from "my request evaporated" — the
/// same indistinguishability #725 is about, at the other end of the same call. With it, `idle` +
/// `zoned` (read alongside `player.zone`) is a positive success signal.
///
/// It is deliberately about the ZONE CHANGE, not about the crossing request: `reset_for_zone_change`
/// also runs for GM `#zone`, gate/evac, portal doors and server-initiated moves, and "your
/// navigation was reset because you changed zone" is true of all of them.
pub const NAV_REASON_ZONED: &str = "zoned";

/// How many standable spots one pad offer carries in total (the one to try + its `alternates`).
/// Bounded: a pad's full leaf list is diagnostics, not an offer.
const OFFERED_SPOTS: usize = 8;

/// Minimum separation between offered spots (#660 review NB). Nearest-first ALONE is not enough: a
/// DRNTP region is a BSP, so one physical spot is split into many leaves, and the eight nearest
/// leaves of qeynos2's pad collapsed onto ~3 real places — one offered pair was **0.0005u** apart.
/// Live, five of six retry attempts landed in the same two spots, which is not six attempts. One
/// nav cell of separation makes the alternates genuinely different places to try.
const SPOT_SEPARATION: f32 = 8.0;

/// Thin `sorted` (nearest-first) down to at most `max` spots that are each at least `min_sep` from
/// every spot already kept. Order is preserved, so the first element stays the nearest.
fn spread_spots(sorted: Vec<[f32; 3]>, max: usize, min_sep: f32) -> Vec<[f32; 3]> {
    let mut out: Vec<[f32; 3]> = Vec::new();
    for p in sorted {
        if out.len() == max { break; }
        if out.iter().all(|q| (p[0] - q[0]).hypot(p[1] - q[1]).max((p[2] - q[2]).abs()) >= min_sep) {
            out.push(p);
        }
    }
    out
}

/// The local controller speed the nav walker drives its `MoveIntent`s at (#625): `RUN_SPEED` while
/// running (the default, and the only speed the walker used before #625), `WALK_SPEED` once the
/// player has toggled to walk. Purely a LOCAL speed choice — the wire message this toggle also
/// sends (`OP_SetRunMode`) does not itself change what the server permits (see `WALK_SPEED`'s doc).
fn nav_speed(gs: &GameState) -> f32 {
    if gs.run_mode { RUN_SPEED } else { WALK_SPEED }
}

/// **The #543 honesty gate.** Whether nav TRUSTS an advertised same-zone crossing enough to
/// AUTO-ROUTE the walker onto it — as a #403 teleport-pad planner edge, or as a #266 sealed-area
/// escape. It is `false`, and for a client that only has the wire to go on it must stay `false`.
///
/// An `OP_SendZonepoints` entry's `zone_id` is the honest `target_zone_id` of ONE zone-point row,
/// but it does not tell the client what physically entering the matching DRNTP region will do. The
/// server resolves an organic `OP_ZoneChange(zoneID = 0)` by an index-BLIND, nearest-XY match over
/// EVERY zone-point's **trigger** coordinates — and trigger coordinates are never on the wire. So a
/// pad advertised as same-zone (`zone_id == current`) can resolve server-side to a DIFFERENT zone,
/// and in qeynos2 provably does: its same-zone rows carry placeholder triggers that can never win
/// that nearest-XY race, so a real neighbouring zone's trigger wins instead.
///
/// Auto-routing the walker through such a pad therefore walks the character across whatever real
/// zone line the server picks, dumping it in a zone the `/goto` never targeted (qeynos2 → qcat,
/// #543) — a silent wrong-place result the agent has no way to detect. So a goal reachable only
/// across such a pad is honestly `no_path`.
///
/// **But `no_path` is not the whole answer.** Withholding the pad entirely would be its own quiet
/// falsehood ("there is nothing here"), so every declined pad is DISCLOSED to the agent —
/// [`crate::diagnostics::PadKnowledge::AdvertisedSameZoneDeclined`], published in the nav snapshot and surfaced on
/// `GET /v1/observe/debug` as `nav_declined_pads` — with what the client actually knows: the pad is
/// here, this is its footprint, this is what the server ADVERTISED, and the true destination is
/// unverifiable. The agent decides whether to take it or give up. **The client does NOT remember
/// where a pad landed** — that memory is the agent's, by owner decision; nothing here caches,
/// learns, or invalidates a pad destination.
///
/// **What this gate does NOT cover, deliberately.** There are three doors onto a same-zone line;
/// this gate closes the two nav opens *on its own initiative*, and leaves the one the AGENT opens:
/// 1. #403 planner pad edges (`same_zone_teleport_pads`) — GATED.
/// 2. #266 sealed-area escape (`find_in_zone_portal`) — GATED.
/// 3. `ActionLoop::drain_zone_cross` (`POST /v1/move/zone_cross`) — **NOT gated, by design.** That
///    door is the agent explicitly asking to cross, which is exactly the choice this PR exists to
///    hand back to it; and the auto-cross that fires when the character physically stands on a
///    footprint stays server-authoritative (#554). Closing it would take away the option the
///    disclosure offers.
pub(crate) const TRUST_ADVERTISED_SAME_ZONE_CROSSINGS: bool = false;

/// How many nav ticks between live clearance-probe refreshes for the diagnostics snapshot (#608).
/// The probe is ~48 short raycasts and the walker ticks on the net thread — sampling every Nth
/// tick keeps the diagnostic from perturbing what it observes.
const CLEARANCE_REFRESH_TICKS: u32 = 8;

/// Everything about the CURRENTLY COMMITTED route that the walker must remember in order to
/// re-derive its published `nav_state` row on any later tick (#851).
///
/// Before #851 all three of these were written once, at plan commit, straight into the shared
/// `NavStatus` and never reconstructible: `route` existed only as the choice of string literal,
/// and `tier`/`reason` were retired by the next state transition. That was fine while the driving
/// word was written exactly once per route. It is not fine now that the word is re-derived every
/// tick — a `navigating` → `navigating_stalled` → `navigating` cycle would otherwise silently drop
/// `nav_tier` and `nav_reason: goal_z_snapped` on the way through, replacing one honesty defect
/// with a smaller one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommittedFacts {
    /// Does this route reach the goal, or is it a partial toward a frontier?
    pub route:  crate::steering::CommittedRoute,
    /// The clearance tier that answered this route (`preferred` | `minimum`), or `None` for a
    /// partial (no tier is recorded for one).
    pub tier:   Option<&'static str>,
    /// The `nav_reason` that belongs with this route for its whole life — `goal_z_snapped` for a
    /// complete route whose z the planner moved, the search limit for a partial.
    pub reason: Option<&'static str>,
}

/// The path-walker: (re)plans the coarse/fine route toward the active `/goto` goal, steers
/// pure-pursuit along it, and drives arrival/stall/fall-edge/portal-escape handling.
///
/// Holds its own clones of the `NavSlots`/`WorldSlots`/`SharedCollision` bundles `ActionLoop` also
/// holds — cheap `Arc` clones of the SAME shared state, not a second copy of it (see
/// `ActionLoop::new`) — plus the two pathfinding worker handles, which `Walker` owns exclusively.
pub struct Walker {
    nav:       eqoxide_ipc::NavSlots,
    world:     eqoxide_ipc::WorldSlots,
    collision: crate::collision::SharedCollision,
    /// The zone terrain+collision LOAD STATE (#579), the SAME shared handle `main.rs` hands the
    /// HTTP surface. The walker consults it through [`crate::zone_assets::usability`] — the ONE
    /// decision function every consumer goes through (#600) — before routing, so that in the ~1-frame
    /// window where the net thread has published the new `player.zone` but the render thread has not
    /// yet started the new load, the walker REFUSES rather than routing on the previous zone's grid
    /// (`self.collision` still holds it). Gating on `collision.is_none()` alone could not see that
    /// window: the old grid is present and non-empty, so the walker would have routed on the WRONG
    /// world (the #560 shape). `usability` returns `None` only for a `Ready` grid whose zone equals
    /// the player's, so a `None` verdict guarantees `self.collision` is the RIGHT zone's grid.
    zone_assets: crate::zone_assets::ZoneAssetStateShared,
    /// The ONLY movement channel — see the module doc's "intent-only movement boundary".
    nav_intent: eqoxide_ipc::NavIntent,
    /// The published nav diagnostics snapshot (#608, replacing the old `NavPathView` pair): the
    /// walker is the ONLY writer, the renderer's 3D overlay and `/v1/observe/nav_debug` are the
    /// readers. It carries the walker's ACTUAL committed routes (`self.path`/`self.local_path`,
    /// verbatim — the #246 property), the last plan's per-edge trace, pad knowledge, and the live
    /// clearance sample. ONE published source: a second copy of any of these would be a channel
    /// that could drift. See `crate::diagnostics`.
    nav_debug: crate::diagnostics::NavDebugView,
    /// Monotonic snapshot publish counter (consumers key their caching on it).
    debug_seq: u64,
    /// The last coarse plan's debug record (kept across route clears — it is the diagnostic OF a
    /// failure; cleared on zone change, when it would describe the wrong zone's geometry).
    last_plan: Option<std::sync::Arc<crate::diagnostics::PlanDebug>>,
    /// Same-zone pad knowledge as of the last plan post (#543/#266/#403).
    last_pads: Vec<crate::diagnostics::PadDebug>,
    /// Throttled live clearance sample near the player (see `CLEARANCE_REFRESH_TICKS`).
    last_clearance: Option<crate::diagnostics::ClearanceProbe>,
    clearance_countdown: u32,

    /// Cached A* waypoints for the current goto goal (routes around walls). `path_i` is the
    /// current waypoint; `path_goal` is the goal these waypoints were computed for (recompute
    /// when the goal changes). Empty path = straight-line fallback.
    pub path:             Vec<[f32; 3]>,  // [east, north, floor_z] per waypoint
    pub path_i:           usize,
    pub path_goal:        Option<(f32, f32, f32)>,
    /// Fine LOCAL A* plan (2u grid, bounded) the walker actually steers along — see the field of
    /// the same name on the pre-extraction `ActionLoop` for the full #nav-multires/#382 rationale.
    pub local_path:       Vec<[f32; 3]>,
    pub local_from:       [f32; 3],
    pub local_i:          usize,
    /// No-progress detector for the path walker (see `nav_progress`). `stuck_best` is the
    /// closest distance reached toward the current aim, `stuck_ticks` the consecutive
    /// no-progress ticks, and `stuck_i` the `path_i` the detector is tracking.
    pub stuck_best:       f32,
    pub stuck_ticks:      u32,
    pub stuck_i:          usize,
    /// Stall-recovery re-paths WITHOUT forward progress; capped (#229 resets it on real progress).
    pub nav_repaths:      u32,
    /// Closest straight-line distance to the current goal reached so far.
    pub nav_best_gdist:   f32,
    /// ROUTE-LEVEL NO-PROGRESS DETECTION (#631 gap 3). `nav_best_g3d` is the closest 3-D approach to
    /// the current goal (`√(gdist² + gdz²)` to the goal's resolved floor) the walker has EVER made on
    /// this goal; `nav_progress_at` is when it last IMPROVED beyond [`NAV_PROGRESS_EPS`]. The existing
    /// stall detector (`stuck_ticks`) only catches a walker that STOPS advancing its `path_i` — it is
    /// blind to one that keeps moving productively-looking while making no headway toward the goal
    /// (the #309 Crushbone moat: swimming laps around the ring for 3+ minutes, `path_i` advancing the
    /// whole time, `navigating` forever, no terminal state). When closest-approach has not improved
    /// for [`NAV_NO_PROGRESS_WINDOW`] the walker terminates honestly (`blocked` / `no_progress`).
    /// 3-D (not horizontal) so a spiral ramp or a vertical climb toward a goal above counts as real
    /// progress and is never falsely killed. Scoped to a FIXED-destination goto (never a `/follow`
    /// chase, whose goal moves with the leader). `f32::MAX` = no approach measured yet on this goal.
    pub nav_best_g3d:     f32,
    pub nav_progress_at:  std::time::Instant,
    /// **THE PUBLISHED DRIVING STATE (#851).** `exec` is the walker's own verdict on whether the
    /// BODY is executing the committed route; `committed` is what route is committed and the
    /// per-route facts that go beside it. Together they are the ONLY input to the `nav_state` word
    /// the walker publishes while it has a route — see [`Walker::publish_drive_state`].
    ///
    /// `exec` is ticked once per drive tick from the SAME two-channel progress signal #631 already
    /// computes (cursor advanced by walking, or closest 3-D approach improved), and it latches: the
    /// stall/back-off/re-path recovery does not clear it, so it cannot read "fine" through a wedge
    /// the way `stuck_ticks` (reset the instant the stall block fires) and `nav_repaths` (reset only
    /// on a 200 u closest-approach improvement) both do.
    ///
    /// `exec_goal_id` is the `NavStatus::goal_id` the verdict is ABOUT (#349 identity). A verdict is
    /// a per-goal fact, and this is what resets it: a new goal id means a new journey, so the
    /// verdict starts fresh. Keying on identity rather than on remembering to reset at each of the
    /// several places a goal can change is deliberate — a forgotten reset here would report a fresh
    /// goto as already wedged.
    pub exec:             crate::steering::RouteExecution,
    pub exec_goal_id:     u64,
    /// **When the walker last made progress on this goal** — the origin `nav_stall.quiet_ms` is
    /// measured from. `None` only before the first drive tick of a journey.
    ///
    /// It is the origin of the SAME window [`Walker::exec`]`.quiet_ticks()` counts, and that is the
    /// fix for #851 review round 1, B2c. It used to be the moment the verdict FLIPPED, which made
    /// `quiet_ms` read `0` on the very tick a stall was first announced and left it a uniform
    /// [`crate::steering::NAV_STUCK_TICKS`]-tick (~3 s) understatement of how long the body had
    /// actually been going nowhere — while sitting in the payload next to a `quiet_ticks` that
    /// counted the whole window. Two fields with the same name-stem measuring two different windows
    /// is a trap an agent cannot detect from the payload, and erring towards "wait longer" does not
    /// make an understatement true. Both fields now measure one window: `quiet_ticks` is the
    /// evidence count, this is its wall clock.
    ///
    /// Measured, never derived from `quiet_ticks` × a nominal tick: the 150 ms nav tick is a floor,
    /// not a guarantee, so under load the real elapsed time runs longer than the arithmetic.
    pub last_progress_at: Option<std::time::Instant>,
    /// The currently committed route and the per-route facts published beside it. `None` when no
    /// route is committed. See [`CommittedFacts`].
    pub committed:        Option<CommittedFacts>,
    /// Downhill back-off (#212): drive the reverse direction for this many ticks before re-pathing.
    pub backoff_ticks:    u32,
    pub backoff_dir:      [f32; 2],
    /// Proactive coarse re-plan (#246) bookkeeping — see the pre-extraction field docs for
    /// `local_stuck_ticks`/`replan_coarse`/`replan_cooldown`/`proactive_replans` (#378 Phase 2).
    pub local_stuck_ticks: u32,
    pub replan_coarse:     bool,
    pub replan_cooldown:   u32,
    pub proactive_replans: u32,
    /// Auto-escape a SEALED interior via an in-zone teleport (#266) — see the pre-extraction
    /// field docs for `escape_return`/`last_walk_pos`/`portal_cooldown`.
    pub escape_return:     Option<(f32, f32, f32)>,
    pub last_walk_pos:     [f32; 3],
    pub portal_cooldown:   u32,
    /// The PATHFINDING WORKER (#340) — posted to; the net thread never blocks on a search.
    planner:          crate::planner::Planner,
    /// The FINE-TIER WORKER (#382) — posted every nav tick, never waited on.
    local_planner:    crate::planner::LocalPlanner,
    /// The planner SNAPPED the current goal's z to a floor the caller never named. Carried to
    /// ARRIVAL so the agent is not told `arrived` as though it got the goal it asked for.
    pub goal_snapped: bool,
    /// True while a plan is in flight for a goal we have NO route for yet.
    pub awaiting_first_plan: bool,
    /// The `NavStatus::goal_id` captured when the CURRENT plan was posted (#631 gap 1). Stamped into
    /// the published `PlanDebug` when the reply lands, so the plan record is attributable to the exact
    /// command it answers — a plan surviving on the snapshot after a `/stop`/fresh goto is then
    /// self-identifying (its `goal_id` differs from the snapshot's live one) rather than masquerading
    /// as the current command's outcome.
    pub plan_goal_id: u64,
}

impl Walker {
    /// `nav`/`world`/`collision` must be `.clone()`s of the SAME bundles `ActionLoop` keeps for its
    /// own (non-walker) uses, `nav_intent` must be `controller.nav_intent.clone()`, and `nav_debug`
    /// must be a clone of the SAME `NavDebugView` `main.rs` hands to the render/HTTP consumers —
    /// NOT fresh `Default`s, or the walker would drive an intent slot nothing reads / publish a
    /// snapshot nothing sees (see the module doc's intent-only boundary and `ActionLoop::new`).
    pub fn new(
        nav:        eqoxide_ipc::NavSlots,
        world:      eqoxide_ipc::WorldSlots,
        collision:  crate::collision::SharedCollision,
        nav_intent: eqoxide_ipc::NavIntent,
        nav_debug:  crate::diagnostics::NavDebugView,
        // The #579 load-state handle, SAME Arc as the HTTP surface's (see the field doc). Drives the
        // #600 zone-identity gate in `drive_walk`; must not be a fresh `Default` or the gate would
        // reason about a different state than the loader writes.
        zone_assets: crate::zone_assets::ZoneAssetStateShared,
    ) -> Self {
        // #766 review B9: a FRESH fine worker starts alive, so the row it will be published on must
        // say so. `local_planner_dead` is latched for the life of a worker (see its field doc), and
        // the row outlives any one `Walker` — it is a shared `Arc` the HTTP surface also holds. Today
        // exactly one `Walker` is built per process — watched, since #787, by
        // `tests::exactly_one_production_fine_worker_is_built_in_the_tree_787`, which fails and names
        // the four "session-scoped" sentences when a second construction SITE is written, and which
        // stays green if this one site is simply called twice (the relogin shape — measured; see its
        // rustdoc) — so this clear is a
        // no-op in production; it is
        // here so that the flag's lifetime is tied to the WORKER's, structurally, at the one place
        // that spawns one. Without it, a second `Walker` over the same row — the shape an in-process
        // relogin would create — would inherit `true` and report a planner it had just replaced as
        // dead forever — #343's shape, a value that outlives the thing it describes (there,
        // `connected: true` published by a loop that had stopped running; here, `dead` published for
        // a thread that had been replaced).
        nav.nav_state.lock().unwrap().local_planner_dead = false;
        Walker {
            nav, world, collision, nav_intent, nav_debug, zone_assets,
            debug_seq: 0,
            last_plan: None,
            last_pads: Vec::new(),
            last_clearance: None,
            clearance_countdown: 0,
            path: Vec::new(),
            path_i: 0,
            path_goal: None,
            local_path: Vec::new(),
            local_i: 0,
            local_from: [0.0, 0.0, 0.0],
            stuck_best: f32::MAX,
            stuck_ticks: 0,
            stuck_i: 0,
            nav_repaths: 0,
            nav_best_gdist: f32::MAX,
            nav_best_g3d: f32::MAX,
            nav_progress_at: std::time::Instant::now(),
            exec: crate::steering::RouteExecution::fresh(),
            exec_goal_id: 0,
            last_progress_at: None,
            committed: None,
            backoff_ticks: 0,
            backoff_dir: [0.0, 0.0],
            local_stuck_ticks: 0,
            replan_coarse: false,
            replan_cooldown: 0,
            proactive_replans: 0,
            escape_return: None,
            last_walk_pos: [0.0, 0.0, 0.0],
            portal_cooldown: 0,
            planner: crate::planner::Planner::spawn(),
            local_planner: crate::planner::LocalPlanner::spawn(),
            goal_snapped: false,
            awaiting_first_plan: false,
            plan_goal_id: 0,
        }
    }

    /// Drop the fine plan and forget the fine tier's last word. Called wherever the ground the plan
    /// describes stops being ground we are standing on — a new destination, a teleport, a stop.
    pub fn clear_local_plan(&mut self) {
        self.local_path.clear();
        self.local_i = 0;
        self.local_stuck_ticks = 0;
        self.local_planner.cancel();
        self.set_nav_local(None);
    }

    /// Did the FINE tier last say the corridor ahead is genuinely not threadable? Read from the
    /// published field rather than a shadow copy, so what steers the walker and what the agent is
    /// told cannot drift apart.
    pub fn local_says_no_way_through(&self) -> bool {
        self.nav.nav_state.lock().unwrap().local.as_ref().is_some_and(|l| l.state == "no_way_through")
    }

    /// Reset all walker state on a zone change (#248). Called by `ActionLoop::sync_zone_points`
    /// (which separately clears its own `falling` — see the module doc for why that field stays
    /// outside `Walker`). The old goal/path are in the PREVIOUS zone's coordinate space; kept
    /// across a crossing they aim the walker at an arbitrary spot and wedge it there.
    pub fn reset_for_zone_change(&mut self) {
        *self.nav.goto_target.lock().unwrap() = None;
        *self.nav.goto_entity.lock().unwrap() = None;
        // #600 review round 3: a one-shot `/zone_cross` that never resolved (re-queued through an
        // asset load for a zone we then LEFT — a Failed load, or a server-initiated move mid-load)
        // must NOT survive into the next zone and fire an unexpected crossing there. The queued id is
        // in the PREVIOUS zone's advertised-zonepoint space; the world has changed, so clearing it is
        // the safe/honest choice. A cross genuinely still mid-resolution for the CURRENT zone has
        // already been turned into a concrete `goto_target` (also cleared here), so nothing legitimate
        // is lost.
        *self.nav.zone_cross.lock().unwrap() = None;
        *self.nav_intent.lock().unwrap() = None; // stop driving the controller toward the stale aim
        // The debug snapshot's plan/pads/clearance describe the PREVIOUS zone's geometry — keeping
        // them would present the old zone's trace over the new zone's world (#608 honesty).
        self.last_plan = None;
        self.last_pads.clear();
        self.last_clearance = None;
        self.path.clear();
        self.local_path.clear();
        self.local_i = 0;
        self.path_goal = None;
        self.path_i = 0;
        self.stuck_i = 0;
        self.stuck_best = f32::MAX;
        self.stuck_ticks = 0;
        self.nav_repaths = 0;
        self.proactive_replans = 0;
        self.nav_best_gdist = f32::MAX;
        self.nav_best_g3d = f32::MAX; // #631 gap 3: closest-approach tracking is per-goal + per-zone
        self.nav_progress_at = std::time::Instant::now();
        // #851: the execution verdict and the committed route's facts are about a route in the
        // PREVIOUS zone's coordinate space. `exec_goal_id` is left alone on purpose — it is an
        // identity stamp, and the goal id does not restart at a zone change; `reset_drive_state`
        // re-stamps it from the live row so the next drive tick does not read this reset as a
        // goal change and reset again.
        self.reset_drive_state();
        self.backoff_ticks = 0;
        self.local_stuck_ticks = 0;
        self.replan_coarse = false;
        self.replan_cooldown = 0;
        // A plan in flight was computed against the PREVIOUS zone's collision grid and its
        // coordinate space. Abandon it — applying it here would drive the character at a route
        // through a zone it is no longer in.
        self.planner.cancel();
        // #766: the identical sentence is true of the FINE plan in flight, and this line was
        // MISSING. Not a considered asymmetry — archaeology: the coarse `cancel()` above predates
        // the fine worker (it is in the zone reset at f2dce47^, before #382 existed), and #382's own
        // diff dropped the fine plan — directly or through `clear_local_plan` — at the sites it
        // touched, while this reset, which already cleared `local_path`/`local_i`/
        // `local_stuck_ticks` three lines up, was never revisited. No comment anywhere records a
        // reason for the gap.
        //
        // SCOPE: this line is defence in depth restoring #382's own pattern. No production route
        // can reach a `post_if_idle` that the stale `pending` slot would refuse. Its only
        // production caller is in `drive_walk`, behind `!self.path.is_empty()`; `self.path` is made
        // non-empty at exactly two sites (`apply_plan`'s `Route` and `Exhausted { progress }`
        // arms — every other write to it is a `.clear()`), and each of those calls
        // `clear_local_plan()`, hence `local_planner.cancel()`, within three lines; `drive_walk`'s
        // empty-path `else` arm cancels as well. So a cancel always intervenes first, and this line
        // is not backed by any measured failure of the new zone's planning.
        self.local_planner.cancel();
        self.awaiting_first_plan = false;
        // SAY WHY (#725 review B1). A bare `idle` here is indistinguishable from "nothing was ever
        // requested" — and this is the line that runs on a SUCCESSFUL `/v1/move/zone_cross`, so the
        // endpoint's success looked exactly like its failure to a polling agent.
        // #732: this retires the published `nav_goal` too. The old goal's `[x, y, z]` is in the
        // PREVIOUS zone's coordinate space and carries no zone tag, so left standing beside `idle`
        // it is a well-formed answer about a world we have left — the defect #732 measured live
        // (`nav_goal: [2216.87, 579.17, -113.25]` read in lfaydark, from the zone before it).
        // `NavStatus::retire_to_idle` also clears `tier` (no route committed → no per-route tier),
        // which is why the explicit clear that used to sit on the next line is gone: one owner.
        // #766: and `local` — the fine tier's last verdict was about threading a corridor in the
        // zone we just left, against a collision grid that no longer exists. Before this it was left
        // standing until some LATER tick reached `resolve_goal` with no goal and called
        // `clear_local_plan`; in between, `nav_local: {"state":"no_way_through", ...}` published
        // beside `nav_state: idle` / `nav_reason: zoned`.
        self.set_nav_state_because("idle", Some(NAV_REASON_ZONED));
        // Publish the cleared snapshot so no consumer keeps drawing the previous zone's state.
        // Position: None — the old zone's coordinates would be a confident wrong answer in the
        // new zone's space (#615 review F1); the next tick republishes the real one.
        self.publish_debug(None, None);
    }

    /// Publish the current `/move/goto` navigation state for GET /v1/observe/debug (#166, #337).
    /// The value set is an AGENT-FACING CONTRACT — every value is documented in `docs/http-api.md`:
    ///
    ///   pending | idle | planning | navigating | navigating_partial | navigating_stalled
    ///   | following | arrived | no_path | search_exhausted | blocked | zone_loading
    ///
    /// **The three `navigating*` words do not belong to this writer (#851).** They are derived from
    /// a typed verdict by [`Walker::publish_drive_state`], which is the only site that may write
    /// one; passing one of them here directly would reintroduce exactly the defect #851 fixes (a
    /// progress word published without consulting whether the body is progressing).
    ///
    /// `zone_loading` (#579) means the zone's collision grid is not built (assets still loading, or
    /// their load failed) — the client has no world model to route in, and no route claim of any
    /// kind should be read from it. See [`NAV_STATE_ZONE_LOADING`].
    ///
    /// `reason` is the machine-readable WHY behind a terminal state.
    pub fn set_nav_state(&self, state: &str) { self.set_nav_state_because(state, None); }

    /// Set the walker's state + reason. **On any NON-`idle` state this deliberately does not touch
    /// `local`** — the fine tier's last word is an independent fact about a different tier (#382),
    /// and it is the evidence behind a terminal `blocked`/`no_path`.
    ///
    /// `idle` is the exception, and #766 made it one: an `idle` goes through
    /// `NavStatus::retire_to_idle`, which retires `local` along with the rest of the finished goal's
    /// facts. `idle` means the goal is over, so the fine tier's verdict about threading toward it is
    /// over too. Nothing here touches the fine PLANNER — see `Walker::clear_local_plan`.
    pub fn set_nav_state_because(&self, state: &str, reason: Option<&str>) {
        // #725 review round 3, B1: enforce the `idle` row's universal at the WRITER, not per call
        // site. `nav_reason: null` on `idle` means exactly one thing — no nav request has been made
        // since the client started — and the boot state is built directly by `NavStatus::default()`,
        // which does not route through here. Every other route to `idle` goes through this writer or
        // `CommandState::stamp_new_goal` (the only other production writer of `state`/`reason`
        // besides `ZoneCrossTicket::drop`, which always supplies a reason), so asserting here and
        // there covers the class. The doc-row pin cannot: it binds the documented reason list to a
        // hand-maintained array of constants, and a reasonless publish adds no constant.
        debug_assert!(!(state == "idle" && reason.is_none()),
            "#725 B1: `idle` must name how it got there; `nav_reason: null` is reserved for boot");
        let mut s = self.nav.nav_state.lock().unwrap();
        Self::write_nav_state_locked(&mut s, state, reason);
    }

    /// The body of [`Walker::set_nav_state_because`], with the lock passed in rather than taken.
    ///
    /// It is split out for exactly one reason (#851): [`Walker::publish_drive_state`] writes the
    /// state word AND the `stall` payload that must accompany it, and the row's own documented
    /// invariant is that `stall` is `Some` **exactly** while the state is `navigating_stalled`. Two
    /// separate `lock()`s would leave a window in which a reader on the HTTP thread — which clones
    /// the whole row under one lock — sees the transition into `navigating_stalled` with `stall`
    /// still cleared by the transition branch below. That window is short, and it is on the mild
    /// side (the WORD is honest, the calibration is momentarily missing), but "you will never see
    /// one without the other" is a universal the docs state, and a universal that is only usually
    /// true is the kind of claim this project treats as a defect. One lock, no window.
    fn write_nav_state_locked(s: &mut eqoxide_ipc::NavStatus, state: &str, reason: Option<&str>) {
        // #732: `idle` means the goal is over, so it goes through the ONE writer that retires the
        // goal's facts — including `goal` itself, which the transition branch below never touched.
        // Unconditional, not gated on `s.state != state`: defence in depth, so no caller can
        // reintroduce the leak by making a second retirement a no-op. (#732 review N1 measured that
        // re-gating it is currently fully GREEN — every route now clears `goal`, so an already-`idle`
        // row has nothing left to clear. This guards the shape, not a scenario I can exhibit.)
        if state == "idle" { s.retire_to_idle(reason); return; }
        if s.state != state || s.reason.as_deref() != reason {
            // A state transition retires the previous route's per-instance facts (#378 Phase 2,
            // #343 discipline) — see the pre-extraction doc comment for the full rationale, and
            // `NavStatus::transition_within_goal` for WHICH facts and why `goal`/`local` survive.
            //
            // #851 review round 1, B1: this used to be a flat assignment list, the third of three
            // and the last one with no exhaustiveness. `stall` was remembered here and forgotten in
            // `CommandState::stamp_new_goal`; the remedy is not to remember harder, it is that all
            // three routes out of a state now destructure `NavStatus` with no `..`, so the next
            // field added is force-decided on every one of them (E0027).
            s.transition_within_goal(state, reason);
        }
    }

    /// Forget the execution verdict and the committed route's facts (#851): a new goal, a new zone,
    /// or a terminated journey. Re-stamps [`Walker::exec_goal_id`] from the live row so the reset
    /// is not immediately re-triggered by the goal-identity check in [`Walker::tick_drive_state`].
    pub fn reset_drive_state(&mut self) {
        self.exec = crate::steering::RouteExecution::fresh();
        self.exec_goal_id = self.nav.nav_state.lock().unwrap().goal_id;
        // The quiet window starts NOW, not at `None`: a journey that never progresses at all still
        // owes an honest "how long has the body been going nowhere", and its origin is the moment
        // the journey began. (Leaving it `None` would publish `quiet_ms: 0` for exactly that walker
        // — the worst-wedged one — which is the understatement B2c is about, re-introduced.)
        self.last_progress_at = Some(std::time::Instant::now());
        self.committed = None;
    }

    /// Advance the execution verdict by one nav tick (#851) and return it.
    ///
    /// `progressed` is #631's TWO-CHANNEL progress signal, computed by the caller: the route cursor
    /// advanced by WALKING, or the closest 3-D approach to the goal improved by `NAV_PROGRESS_EPS`.
    /// The signal is not new; publishing it is. Until #851 it was consulted only to decide when to
    /// give up at 60 s, so a walker that had been going nowhere for 3 s and one walking cleanly were
    /// the same `navigating` to every reader.
    ///
    /// **The goal-identity reset lives here** rather than at each site a goal can change: a verdict
    /// is a fact about a journey, `NavStatus::goal_id` is that journey's identity (#349), and a
    /// forgotten reset would report a freshly-accepted goto as already wedged — a lie in the
    /// pessimistic direction, but still a lie.
    fn tick_drive_state(&mut self, progressed: bool) -> crate::steering::RouteExecution {
        let now = std::time::Instant::now();
        let goal_id = self.nav.nav_state.lock().unwrap().goal_id;
        if goal_id != self.exec_goal_id {
            self.exec = crate::steering::RouteExecution::fresh();
            self.exec_goal_id = goal_id;
            self.last_progress_at = Some(now); // a new journey's quiet window starts here
        }
        self.exec = self.exec.tick(progressed, self.nav_repaths);
        // [`Walker::last_progress_at`] is the ORIGIN of the window `quiet_ticks` counts — the last
        // tick that reported progress — not the moment the verdict flipped (#851 review round 1,
        // B2c). `tick` resets `quiet_ticks` to 0 on exactly the ticks that progressed, so this reads
        // the machine's own answer rather than re-deriving `progressed`. `is_none()` seeds a journey
        // whose very first drive tick is already quiet.
        if self.exec.quiet_ticks() == 0 || self.last_progress_at.is_none() {
            self.last_progress_at = Some(now);
        }
        self.exec
    }

    /// **Publish the driving `nav_state` row from the verdict (#851) — the one place the walker
    /// writes a `navigating*` word.**
    ///
    /// The word is not chosen here; it is `crate::steering::driving_nav_state`'s total function of
    /// (committed route, execution verdict), and the `nav_stall` payload beside it is built from
    /// the SAME verdict in the same call. That is what makes "reports progress while stalled"
    /// unrepresentable at this site: there is no argument to this function that produces
    /// `navigating` together with a `Stalled` verdict, and none that produces the word without the
    /// payload agreeing with it.
    ///
    /// `tier` and `reason` are re-asserted from [`Walker::committed`] because
    /// [`Walker::set_nav_state_because`] retires the previous route's per-instance facts on any
    /// transition, and this word now transitions mid-route.
    ///
    /// A no-op when no route is committed: with nothing committed there is no driving state to
    /// report, and inventing one (defaulting the route to `Complete`, say) is the fabrication class
    /// this whole issue is about. The caller only reaches it with `have_path`, and `self.path` is
    /// made non-empty only by the two `apply_plan` arms that set `committed` in the same breath.
    fn publish_drive_state(&self) {
        let Some(facts) = self.committed else { return };
        let word = crate::steering::driving_nav_state(facts.route, self.exec);
        // ONE lock for the word, the tier and the stall payload — see `write_nav_state_locked` for
        // why they cannot be three separate acquisitions.
        let mut s = self.nav.nav_state.lock().unwrap();
        Self::write_nav_state_locked(&mut s, word, facts.reason);
        s.tier  = facts.tier;
        s.stall = match self.exec {
            crate::steering::RouteExecution::Advancing { .. } => None,
            crate::steering::RouteExecution::Stalled { quiet_ticks, repaths } =>
                Some(eqoxide_ipc::NavStall {
                    quiet_ticks,
                    // Measured over the SAME window `quiet_ticks` counts — since the walker last
                    // made progress, not since the verdict flipped (#851 review round 1, B2c).
                    quiet_ms: self.last_progress_at.map_or(0, |t| t.elapsed().as_millis() as u64),
                    repaths,
                    route: facts.route.as_str(),
                }),
        };
    }

    /// Publish the FINE tier's last honest outcome (#382). Never touches `state`/`reason`.
    ///
    /// **A verdict is never stored on an `idle` row (#766).** `NavLocal` is the fine planner's
    /// verdict on threading toward *a goal*, and `idle` is the state that means there is no goal —
    /// so `Some` beside `idle` is precisely the stale-fact class this issue is about.
    /// `NavStatus::retire_to_idle` clears the field at the TRANSITION; this is the other half, the
    /// writer-level guard that keeps it clear for the row's whole lifetime. Without it
    /// `docs/http-api.md`'s "`nav_local` is `null` on every `idle`" is only true at the instant of
    /// retirement, which is not what a polling agent reads.
    ///
    /// **It is a coercion, not a `debug_assert!`, and that is deliberate.** The gap is a runtime
    /// race, not a programming error: `local` is published from the net thread, and `POST /v1/move/stop`
    /// can retire the row from the HTTP thread in between, so a verdict computed while the goal was
    /// live can arrive after it is gone — no call site is wrong. An assert cannot prevent that, and
    /// it is compiled out of `--release` besides, so it would leave the documented universal false
    /// in the shipped binary. Dropping the value instead makes it true in every profile.
    ///
    /// **What is lost by dropping it, precisely** (review B3). For `no_way_through` and `exhausted`,
    /// nothing: they are verdicts about threading toward a goal that no longer exists. The earlier
    /// draft of this doc said that of all of them, and it was false for the third publishable state.
    /// `planner_dead` is a latched fault about the *client's fine worker*, not about any goal, and
    /// dropping it here would have hidden a dead fine worker from an agent between goals — which is
    /// when an agent polls `/v1/observe/debug` to decide what to do next. That fact is no longer
    /// carried in this field alone: [`Walker::latch_local_planner_liveness`] mirrors it into
    /// `NavStatus::local_planner_dead`, which retirement keeps and this coercion does not touch. So
    /// the sentence is now true as scoped, because the code makes it true, not because it was
    /// narrowed until it stopped saying anything.
    pub fn set_nav_local(&self, local: Option<eqoxide_ipc::NavLocal>) {
        let mut s = self.nav.nav_state.lock().unwrap();
        let local = if s.state == "idle" { None } else { local };
        if s.local != local { s.local = local; }
    }

    /// Mirror the fine worker's death into `NavStatus::local_planner_dead` (#766 review B3). Latched
    /// for the life of the WORKER — `LocalPlanner::is_dead()` is itself latched, and this only ever
    /// sets; the single clear lives in [`Walker::new`], which runs when a REPLACEMENT worker is
    /// spawned (round-6 review B12: this line used to call the field SESSION-scoped, which is the
    /// agent-facing name for the same span on today's one-`Walker` process, not the internal rule).
    ///
    /// Called from the `is_dead()` branch in [`Walker::drive_walk`], beside the per-goal
    /// `nav_local` publication it backs up. **That is the DISCOVERY site, and discovery is the
    /// constraint.** The tempting alternative is to call this earlier in the walk tick,
    /// "unconditionally", so that it cannot be missed. (An uncommitted draft of mine did; that draft
    /// is in no commit, ref or reflog, so treat any account of it — including
    /// [`a_dead_fine_planner_stays_visible_after_the_goal_is_retired_766`]'s — as recollection rather
    /// than history, review B10. The argument below does not rest on it.) Two independent things kill
    /// that placement, and review B7 is right that the ORDERING one is decisive while the
    /// reachability one is merely additional:
    ///
    /// 1. **Ordering — structural, and by itself sufficient.** `LocalPlanner.dead` is written at
    ///    exactly two places, the failed send in `post_if_idle` and the disconnected receive in
    ///    `poll`. Both are called only from inside `drive_walk`'s `have_path` block, and both sit
    ///    ABOVE this check on the same tick. So at any point earlier in the tick `is_dead()` can only
    ///    report a death some EARLIER tick already discovered — an earlier latch is always one tick
    ///    late, and if the goal retires in that gap it never fires at all. That is the same
    ///    between-goals hole B3 exists to close, just moved one tick over.
    /// 2. **Reachability — additional, and the reason a "just put it higher" placement is not even
    ///    reliably one tick late.** `drive_walk` returns early at **five** points above
    ///    `let have_path`, not the three an earlier draft of this doc claimed (review B7): the
    ///    zone-usability halt; the mid-tick collision-grid-vanished halt; a coarse reply that
    ///    `apply_plan` says terminated the goal; the COARSE `planner.is_dead()` stop; and
    ///    `awaiting_first_plan`. Separately, `ActionLoop::tick` does not call `drive_walk` at all once
    ///    `resolve_goal` returns `None`, so there is no between-goals tick to hook either.
    ///
    /// Latching at the discovery site therefore records the fault on the very tick it becomes
    /// knowable, and because the record is on the shared row rather than in the per-goal verdict, the
    /// later retirement cannot take it away. What changes is not WHEN the fault is seen but how long
    /// it stays visible: for the rest of the worker's life — which, one `Walker` being built per
    /// process today, is the rest of the session — instead of until the next goal ends.
    ///
    /// The residual limit is stated on the field: a worker that dies and is never posted to again is
    /// not discoverable by any reader, this one included.
    pub fn latch_local_planner_liveness(&self) {
        if !self.local_planner.is_dead() { return; }
        let mut s = self.nav.nav_state.lock().unwrap();
        if !s.local_planner_dead { s.local_planner_dead = true; }
    }

    /// The player's position for the snapshot — **`None` until the server has told us where we
    /// are** (#615 review F1: a fresh login published a confident `[0,0,0]`, 985 units from the
    /// character; "unknown" must be representable, never a fabricated origin).
    fn known_pos(gs: &GameState) -> Option<[f32; 3]> {
        gs.player_pos_known.then(|| [gs.player_x, gs.player_y, gs.player_z])
    }

    /// Publish the nav diagnostics snapshot (#608). **This is the one place the snapshot is
    /// written**, and every field is copied from the walker's OWN state — `self.path` /
    /// `self.local_path` verbatim (the #246 committed-route property), the planner's own trace,
    /// the pad knowledge the last plan was given. Consumers (the 3D overlay, the HTTP endpoint)
    /// render this and nothing else; there is no second derivation for them to disagree with.
    fn publish_debug(&mut self, player: Option<[f32; 3]>, water: Option<crate::diagnostics::WaterDebug>) {
        self.debug_seq += 1;
        let (state, reason, goal_id) = {
            let s = self.nav.nav_state.lock().unwrap();
            (s.state.clone(), s.reason.clone(), s.goal_id)
        };
        let goal = self.nav.goto_target.lock().unwrap().map(|(x, y, z)| [x, y, z]);
        let snap = crate::diagnostics::NavDebugSnapshot {
            seq: self.debug_seq,
            zone_model_loaded: self.collision.read().unwrap().is_some(),
            nav_state: state,
            nav_reason: reason,
            goal_id,
            player,
            published_at: std::time::Instant::now(),
            goal,
            committed_coarse: self.path.clone(),
            committed_fine: self.local_path.clone(),
            plan: self.last_plan.clone(),
            pads: self.last_pads.clone(),
            clearance: self.last_clearance.clone(),
            water,
        };
        *self.nav_debug.lock().unwrap() = Some(std::sync::Arc::new(snap));
    }

    /// Read handle for consumers/tests. The walker remains the only WRITER.
    pub fn debug_view(&self) -> &crate::diagnostics::NavDebugView { &self.nav_debug }

    /// Is the published snapshot already the settled no-goto state? Used by `resolve_goal` so the
    /// no-goto tick republishes only when something drifted, not every idle tick.
    ///
    /// #615 review F1: this comparison MUST cover every published field that can change while no
    /// goto is active — `player` (WASD / server-pushed movement) and `zone_model_loaded` (assets
    /// finishing their load) drift on an idle walker, and comparing only routes/state left a
    /// fresh-login snapshot claiming `[0,0,0]` + "no world model" forever, 985 units from the
    /// character, while the `zone_assets` object beside it said "ready".
    fn debug_is_settled(&self, gs: &GameState) -> bool {
        let snap = self.nav_debug.lock().unwrap();
        match snap.as_ref() {
            None => false,
            Some(s) => {
                let live = self.nav.nav_state.lock().unwrap();
                let pos_settled = match (s.player, Self::known_pos(gs)) {
                    (None, None) => true,
                    (Some(a), Some(b)) =>
                        // A small tolerance so idle float jitter doesn't republish every tick;
                        // real movement (even one step) exceeds it and republishes.
                        (a[0] - b[0]).abs() < 0.5 && (a[1] - b[1]).abs() < 0.5 && (a[2] - b[2]).abs() < 0.5,
                    _ => false,
                };
                pos_settled
                    && s.zone_model_loaded == self.collision.read().unwrap().is_some()
                    && s.committed_coarse.is_empty() && s.committed_fine.is_empty() && s.goal.is_none()
                    && s.nav_state == live.state && s.nav_reason == live.reason
            }
        }
    }

    /// Refresh the live clearance sample at a throttled cadence: the probe is ~48 raycasts, and
    /// the walker ticks on the net thread, so it is sampled every [`CLEARANCE_REFRESH_TICKS`]th
    /// tick rather than every tick — a diagnostic must not perturb the behaviour it observes. The
    /// sample carries its own `at`, so a consumer always knows where it was taken.
    fn refresh_clearance(&mut self, player: [f32; 3]) {
        if self.clearance_countdown > 0 {
            self.clearance_countdown -= 1;
            return;
        }
        self.clearance_countdown = CLEARANCE_REFRESH_TICKS;
        self.last_clearance = self.collision.read().unwrap().as_ref()
            .map(|c| c.clearance_probe(player[0], player[1], player[2]));
    }

    /// Read the current nav state word (without the reason).
    pub fn nav_state_is(&self, state: &str) -> bool {
        self.nav.nav_state.lock().unwrap().state == state
    }

    /// Move the coarse-route cursor `path_i` to the segment the character is actually traversing.
    ///
    /// Two rules, in order:
    ///
    /// 1. The **monotone advance**: step past the current segment once the character's 3-D
    ///    projection parameter on it reaches 1.0. (3-D, water-nav Slice 3 §8.1: a near-vertical
    ///    dive/ascent leg is not skipped on frame one — the cursor advances past it only once the
    ///    character has actually changed depth. On near-horizontal land the z term vanishes, so this
    ///    is the same advance as before.) **This rule, and only this rule, means "walked".**
    ///
    /// 2. The **stale-cursor resync** (#673): rule 1 assumes the character travels ALONG the route.
    ///    Physics does not. A fall, or a slide down a ramp, can carry it past several waypoints at
    ///    once and leave it beside a segment whose projection parameter then saturates strictly below
    ///    1.0 — so rule 1 can never fire again. The cursor then names a segment the character is
    ///    nowhere near, and that lie reaches the steering aim by the route below.
    ///
    ///    ⚠️ **Correction (#727 round 3).** Earlier revisions of this comment said simply that "the
    ///    carrot lands ON the character". That is not what happens at the reach `drive_walk` actually
    ///    steers with, and the round-2 review was right to reject it: at `LOOK_AHEAD = 5.0` the
    ///    *coarse* carrot off a stale cursor leads by ~17 u on the captured #673 fixture. The chain
    ///    is one step longer:
    ///
    ///    * [`crate::steering::carrot_along`] measures arclength from a projection onto the stale
    ///      segment, so `local_goal` — the `LOCAL_REACH` (24 u) point handed to the FINE planner —
    ///      collapses to ~0.2 u from the body;
    ///    * `find_path_local` duly returns a degenerate two-waypoint stub;
    ///    * [`crate::steering::steer_target`] prefers the fine path at exactly `len() >= 2`, so the
    ///      stub is not discarded as too short — it is preferred over the healthy coarse aim;
    ///    * the 5 u carrot taken along that stub therefore lands ~0.2 u from the body, inside one
    ///      controller frame of travel (`RUN_SPEED * 0.01 = 0.44 u`), so it is overshot rather than
    ///      reached.
    ///
    ///    The aim then flips every frame and net displacement is zero: **the steering loop has no
    ///    trajectory that leaves the spot** while the cursor stays stale. Measured end to end by the
    ///    three `#673 step N of 3` tests in [`crate::steering`], the last of which drives the
    ///    production [`crate::steering::steer_target`] and [`crate::steering::fast_steer_aim`] at
    ///    `LOOK_AHEAD` on a featureless floor: 0.04 u of net displacement over 200 nav ticks
    ///    (30 s), and never more than 6.6 u from where it landed — less than one 8 u route leg.
    ///
    ///    ⚠️ **Correction (#727 round 5).** This line read "0.02 u" from round 2 until now. The
    ///    figure was not wrong when written, but the harness that produced it was: it dropped 14 of
    ///    every 15 controller frames on a tick with no fine plan, where the production controller
    ///    keeps integrating the last `MoveIntent`. The harness was fixed this round (round-4 review
    ///    finding B-C) and every figure it produces moved; the sibling number in
    ///    [`crate::steering`]'s test doc was restated to 0.04 u and this one was not swept with it.
    ///    That miss is the same defect the round-5 review named: correcting by memory instead of by
    ///    grepping the concept.
    ///
    ///    ⚠️ **Correction (#727 round 4).** This paragraph used to continue "…and the walker
    ///    exhausts its re-paths and stops with `blocked` / `walker_stalled`", citing that sim. The
    ///    sim does not contain the walker's stall detector, `NAV_STUCK_TICKS` backoff or re-plan, so
    ///    it cannot say that. Driven through the **production** `drive_walk` loop on that same
    ///    fixture, the walker sits in the cycle for ~22 nav ticks (~3.3 s), then backs off, re-plans
    ///    from the body, and **arrives** (#727 round-3 review, measured). What makes #673 terminal
    ///    rather than a hiccup is the re-plan reproducing the state, which is a property of the
    ///    terrain and not of this mechanism: live on qcat it did, and the walker stopped with
    ///    `blocked` / `walker_stalled` on 6 of 8 attempts. The cost of a stale cursor is therefore
    ///    *at least* a wasted backoff-and-re-plan lap per occurrence, and at worst a terminal stop on
    ///    a route the character could have walked.
    ///
    ///    ⚠️ **Correction (#727 round 5).** This used to add "and that reason code is only emitted
    ///    once `nav_repaths` has reached 8, so all eight re-plans failed to escape". The counter does
    ///    not support that: `nav_repaths` is reset to 0 whenever
    ///    `gdist < nav_best_gdist - REPATH_RESET_DIST` (200 u) and on `decision.reset_route`, so
    ///    reaching 8 means *at least
    ///    eight stall-triggered re-plans since the walker last closed 200 u on the goal* — not eight
    ///    attempts at one spot. The live record does not place all eight at `[-534.4, 144.4, -6.0]`.
    ///    The "terminal on real terrain" conclusion stands on the `blocked` outcome itself.
    ///
    /// ## A resync is NOT progress, and the walker says so (#727 round 2)
    ///
    /// `path_i` has two readers with different needs. STEERING needs it to name the segment the body
    /// is on — that is what rule 2 restores. The two HONEST-TERMINATION channels
    /// (`drive_walk`'s stall detector, and #631 channel (a) `advancing_complete_route`) instead need
    /// it to mean *"the walker got here by walking"*; channel (a)'s comment justifies its verdict
    /// "by construction" on exactly that premise, which held only while rule 1 was the sole way the
    /// cursor moved.
    ///
    /// Rule 2 breaks that premise, so rather than leave the premise false this raises `stuck_i` to
    /// the resynced cursor. A resync jump is then invisible to both channels: it can neither reset
    /// the stall clock nor refresh `nav_progress_at`, and `path_i > stuck_i` can still only arise
    /// from rule 1. This costs #673's fix nothing — the resync's value is that the walker starts
    /// MOVING again, and the movement then advances the cursor by rule 1, which does count. It is
    /// deliberately conservative in the honest direction: in a tick where both rules fire, the
    /// genuine rule-1 step is swallowed with the jump (under-reporting progress, never over-).
    ///
    /// **This is measured, not reasoned.** Delete the `stuck_i` raise below and
    /// `a_resync_jump_must_not_reset_the_no_progress_clock` goes RED: it drives the real
    /// [`Walker::drive_walk`] over a route where a resync jump happens, and without the raise the
    /// walker keeps reporting itself as making progress. The reviewer's question was whether a false
    /// cursor advance actually reaches a consumer in a way that misleads. It does, and that test is
    /// the execution-level proof.
    ///
    /// The reachability predicate is [`crate::steering::resync_reachable`] — a conjunction of a
    /// chest-height LOS ray (walls) and a floor-column probe (voids/drops) — because the LOS ray
    /// alone cannot answer the question rule 2 asks. It is named once and shared with both offline
    /// harnesses rather than spelled out here, so a change to it cannot leave them modelling a
    /// predicate this function no longer runs (#887 round 1 measured exactly that drift).
    ///
    /// It is still only a necessary condition; the `stuck_i` raise above is what makes a wrong
    /// answer harmless to the honesty machinery rather than merely unlikely. Two known limits, both
    /// on that function's rustdoc: #734 gap 1 — a hole narrower than `ground_continuous`'s probe
    /// spacing, in the direction of travel, crosses undetected (measured below by
    /// `a_narrow_hole_between_probes_still_crosses_the_resync_undetected`, driven through this exact
    /// function) — and #734 gap 2, the floor probe's width-blindness, which was measured to be
    /// AGREEMENT with the controller's own centre-column floor clamp rather than a defect, and whose
    /// attempted fix is held out by `a_resync_must_still_cross_ground_the_controller_can_stand_on`.
    fn advance_cursor(&mut self, p: [f32; 3]) {
        while self.path_i + 2 < self.path.len() {
            let (a, b) = (self.path[self.path_i], self.path[self.path_i + 1]);
            let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
            let t = if l2 < 1e-6 { 1.0 } else {
                ((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1] + (p[2] - a[2]) * ab[2]) / l2
            };
            if t >= 1.0 { self.path_i += 1; } else { break; }
        }
        let walked_to = self.path_i;
        let resynced = {
            let coll = self.collision.read().unwrap();
            let reachable = |a: [f32; 3], b: [f32; 3]| coll.as_ref()
                .map_or(true, |c| crate::steering::resync_reachable(c, a, b));
            crate::steering::resync_cursor(&self.path, walked_to, p, reachable)
        };
        self.path_i = resynced;
        if resynced > walked_to {
            // Not walked — not progress. See the doc above.
            self.stuck_i = self.stuck_i.max(resynced);
        }
    }

    /// Stop navigating and report WHY, loudly, in every channel an agent can see.
    pub fn stop_nav(&mut self, gs: &mut GameState, state: &str, reason: &str, msg: &str) {
        self.stop_nav_blocked(gs, state, reason, None, None, msg);
    }

    /// [`Walker::stop_nav`], additionally publishing the agent-honesty blockage payload (#378
    /// Phase 2).
    pub fn stop_nav_blocked(&mut self, gs: &mut GameState, state: &str, reason: &str,
        goal_blk: Option<crate::traversability::Blockage>,
        frontier_blk: Option<crate::traversability::Blockage>, msg: &str)
    {
        tracing::warn!("NAV: {msg}");
        gs.log_msg("zone", msg);
        self.set_nav_state_because(state, Some(reason));
        // Publish the blockage AFTER the state (set_nav_state_because clears it on transition).
        let to_nav = |b: crate::traversability::Blockage| eqoxide_ipc::NavBlockage {
            hazard: b.hazard.as_str(), at: b.at };
        {
            let mut s = self.nav.nav_state.lock().unwrap();
            s.blocked_goal = goal_blk.map(to_nav);
            s.blocked_frontier = frontier_blk.map(to_nav);
        }
        self.path.clear();
        // Drop the fine PLAN, but deliberately KEEP the fine tier's last word (`nav_local`).
        self.local_path.clear();
        self.local_i = 0;
        self.local_stuck_ticks = 0;
        self.local_planner.cancel();
        self.path_goal = None;
        self.planner.cancel();
        self.awaiting_first_plan = false;
        // #851: the journey is over and the route is gone. `set_nav_state_because` above already
        // cleared the published `nav_stall`; this drops the walker-side facts so nothing can
        // re-publish a driving row for a route that no longer exists.
        self.reset_drive_state();
        *self.nav.goto_target.lock().unwrap() = None;
        *self.nav_intent.lock().unwrap() = None;
        // Publish the terminal state. `last_plan` is deliberately KEPT: its trace is the
        // diagnostic OF this failure — exactly what a consumer needs to see now (#608).
        self.publish_debug(Self::known_pos(gs), None);
    }

    /// Apply a finished FINE plan from the local worker (#382). See the pre-extraction doc comment
    /// (three things happen: install the steer path, arm the proactive re-plan ONLY on a CLOSED
    /// window, and publish what the fine tier actually said).
    pub fn apply_local_plan(&mut self, reply: crate::planner::LocalReply) {
        let outcome = reply.outcome;
        self.local_path = outcome.steer().to_vec();
        self.local_from = reply.start;
        self.local_i = 0;

        let healthy = self.backoff_ticks == 0 && self.stuck_ticks < NAV_HOP_TICKS;
        if arms_coarse_replan(&outcome) && healthy && self.replan_cooldown == 0 {
            self.local_stuck_ticks += 1;
            if self.local_stuck_ticks >= NAV_LOCAL_STUCK_TICKS {
                self.replan_coarse = true;
                self.proactive_replans += 1;
                tracing::debug!("NAV: fine plan CLOSED its window short of the carrot near ({:.0},{:.0}) \
                    ({}) — re-planning coarse (#246, proactive #{})", reply.start[0], reply.start[1],
                    outcome.reason(), self.proactive_replans);
            }
        } else if outcome.threaded() {
            self.local_stuck_ticks = 0;
        }

        self.set_nav_local(Some(eqoxide_ipc::NavLocal {
            state:       outcome.state().to_string(),
            reason:      outcome.reason().to_string(),
            stuck_ticks: self.local_stuck_ticks,
            plan_us:     reply.plan_us as u64,
        }));
    }

    /// #685 (owner-directed): inflate a freshly-committed coarse route OFF convex wall corners, so the
    /// walker steers one smooth wider arc with clearance instead of hugging/wiggling the apex. Uses the
    /// zone clearance spokes via `Collision::inflate_route_off_corners`; a no-op when no grid is loaded.
    fn inflate_committed(&self, route: &mut [[f32; 3]]) {
        if let Some(c) = self.collision.read().unwrap().as_ref() {
            c.inflate_route_off_corners(route, eqoxide_core::physics::PLAYER_RADIUS, CORNER_BUFFER);
        }
    }

    /// Apply a finished plan from the worker thread. Returns `true` when the tick must STOP here —
    /// the plan was terminal (no route / gave up) or redirected the goto through a portal.
    pub fn apply_plan(
        &mut self,
        reply: crate::planner::PlanReply,
        gs: &mut GameState,
        goal: (f32, f32, f32),
    ) -> bool {
        use crate::collision::PlanOutcome;
        self.awaiting_first_plan = false;
        let snapped = reply.goal_snapped;
        self.goal_snapped = snapped.is_some();
        // Record the plan's debug record (#608) from the WORKER'S OWN reply — the outcome, the
        // reason, and the per-edge trace it recorded while searching. Published at the end of this
        // method, once the nav_state it belongs with has been set.
        {
            let (outcome_str, route_len, route_end) = match &reply.outcome {
                PlanOutcome::Route(p) => ("route", p.len(), p.last().copied()),
                PlanOutcome::Unreachable { .. } => ("unreachable", 0, None),
                PlanOutcome::Exhausted { progress, .. } =>
                    ("exhausted", progress.as_ref().map_or(0, |p| p.len()),
                     progress.as_ref().and_then(|p| p.last().copied())),
            };
            // #631 gap 2: how far, HORIZONTALLY, the committed route's ENDPOINT lands from the goal
            // the caller named. 0 for a complete route (it ends exactly at the requested XY), nonzero
            // for a partial that stops at its closest approach — the honest "your named coords are not
            // where the walker is headed, and by this much". Measured on the SAME route the walker
            // steers, so the disclosure cannot drift from the committed path (`goal_snapped` covers
            // only the vertical case; this is its horizontal companion).
            let goal_offset = crate::steering::route_goal_offset(route_end, reply.goal);
            self.last_plan = Some(std::sync::Arc::new(crate::diagnostics::PlanDebug {
                gen: reply.gen,
                // The goal_id captured when THIS plan was posted (#631 gap 1) — the command it answers,
                // never a later one. A superseded plan riding the snapshot is then self-identifying:
                // its goal_id differs from the snapshot's live goal_id.
                goal_id: self.plan_goal_id,
                start: reply.start,
                goal: reply.goal,
                outcome: outcome_str.to_string(),
                reason: reply.outcome.reason().to_string(),
                route_len,
                plan_ms: reply.plan_ms as u64,
                tight: reply.tight,
                goal_snapped: snapped.is_some(),
                goal_offset,
                trace: reply.trace.clone(),
            }));
        }
        match snapped {
            Some(crate::collision::GoalSnap::ToColumnFloor { z }) => gs.log_msg("zone", &format!(
                "Goal z={:.0} is not on any floor — routing to the floor at z={:.0} instead (the client \
                 CHANGED your goal; it is not the one you gave).", goal.2, z)),
            // The water qualifier (design §4d): "arrived" at a submerged goal without this line
            // would claim a depth the walker never reached — buoyancy only rises, so it floats at
            // the surface above the goal XY. Reported here AND carried to arrival via
            // `goal_snapped` (`nav_reason: goal_z_snapped`).
            Some(crate::collision::GoalSnap::ToWaterSurface { surface_z }) => gs.log_msg("zone", &format!(
                "Goal z={:.0} is submerged — the walker cannot dive and hold that depth; navigating to \
                 the WATER SURFACE at z={:.0} above it. Arrival will be at the surface, not the asked depth.",
                goal.2, surface_z)),
            None => {}
        }
        match reply.outcome {
            // A real, complete route to the goal. The only outcome the walker may treat as a plan.
            PlanOutcome::Route(path) => {
                tracing::info!("NAV: plan #{} → ROUTE to ({:.0},{:.0}) = {} waypoints ({}ms, off the net thread)",
                    reply.gen, goal.0, goal.1, path.len(), reply.plan_ms);
                let mut path = path;
                self.inflate_committed(&mut path); // #685: push the route off convex corners (owner)
                self.path = path;
                self.path_i = 0;
                self.stuck_i = 0;
                self.clear_local_plan();
                // #851: record the route's facts, then publish the row DERIVED from them and the
                // live execution verdict. A re-plan is not progress — the verdict is deliberately
                // NOT reset here, so a fresh route installed at the same wedge keeps reporting
                // `navigating_stalled` instead of laundering the stall into a clean `navigating`
                // eight times over. (`path_i`/`stuck_i` going back to 0 is exactly why the walker's
                // own `stuck_ticks` cannot carry this: a re-plan resets it.)
                self.committed = Some(CommittedFacts {
                    route:  crate::steering::CommittedRoute::Complete,
                    tier:   Some(if reply.tight { "minimum" } else { "preferred" }),
                    reason: self.goal_snapped.then_some("goal_z_snapped"),
                });
                self.publish_drive_state();
                self.publish_debug(Self::known_pos(gs), None);
                false
            }
            // The search was CUT SHORT — "I don't know", not "no route".
            PlanOutcome::Exhausted { limit, progress: Some(path) } => {
                tracing::warn!("NAV: plan #{} → EXHAUSTED ({}) after {}ms — walking a PARTIAL route ({} wp) toward \
                    ({:.0},{:.0}) and re-planning from its end. This is NOT a route to the goal.",
                    reply.gen, limit.as_str(), reply.plan_ms, path.len(), goal.0, goal.1);
                gs.log_msg("zone", "Planner gave up before finding a full route — walking as far as it can, then re-planning");
                let mut path = path;
                self.inflate_committed(&mut path); // #685: push the partial route off convex corners too
                self.path = path;
                self.path_i = 0;
                self.stuck_i = 0;
                self.clear_local_plan();
                // #851 — same as the `Route` arm above: facts recorded, row derived. `tier: None`
                // matches the old behaviour (no tier is recorded for a partial).
                self.committed = Some(CommittedFacts {
                    route:  crate::steering::CommittedRoute::Partial,
                    tier:   None,
                    reason: Some(limit.as_str()),
                });
                self.publish_drive_state();
                self.publish_debug(Self::known_pos(gs), None);
                false
            }
            // Gave up with nothing usable. Honest "I DON'T KNOW".
            PlanOutcome::Exhausted { limit, progress: None } => {
                self.stop_nav(gs, "search_exhausted", limit.as_str(), &format!(
                    "Path search to ({:.0},{:.0}) GAVE UP ({}) after {}ms with no usable route. This is not \
                     'no route exists' — the search never finished. Try a nearer waypoint.",
                    goal.0, goal.1, limit.as_str(), reply.plan_ms));
                true
            }
            // DEFINITIVE: no route exists.
            PlanOutcome::Unreachable { reason: why, goal_blocked_by, frontier_blocked_by } => {
                if portal_escape_applies(why) && self.escape_return.is_none() && self.portal_cooldown == 0 {
                    if let Some(portal) = self.find_in_zone_portal(gs) {
                        tracing::info!("NAV: goal ({:.0},{:.0}) is UNREACHABLE by walking ({}) — escaping the sealed area \
                            via the in-zone teleport at ({:.0},{:.0}) (#266)",
                            goal.0, goal.1, why.as_str(), portal.0, portal.1);
                        self.escape_return = Some(goal);
                        *self.nav.goto_target.lock().unwrap() = Some(portal);
                        self.portal_cooldown = PORTAL_COOLDOWN_TICKS;
                        self.path_goal = None; // re-plan to the portal next tick
                        *self.nav_intent.lock().unwrap() = None;
                        return true;
                    }
                }
                let blk = goal_blocked_by.or(frontier_blocked_by);
                let detail = blk.map(|b| format!(" — blocked by {} at ({:.0},{:.0},{:.0})",
                    b.hazard.as_str(), b.at[0], b.at[1], b.at[2])).unwrap_or_default();
                self.stop_nav_blocked(gs, "no_path", why.as_str(), goal_blocked_by, frontier_blocked_by,
                    &format!(
                    "No route to ({:.0},{:.0}): {} (searched to completion in {}ms — this is a definitive no, \
                     not a timeout){}.", goal.0, goal.1, why.as_str(), reply.plan_ms, detail));
                true
            }
        }
    }

    /// Stop all navigation the instant the player is slain (#238): abandon the destination + route +
    /// controller intent so a corpse doesn't keep walking toward the goal, and clear the overlay
    /// line. Returns true when the player is dead (the caller returns early from the tick).
    pub fn nav_halt_if_dead(&mut self, gs: &GameState) -> bool {
        if !gs.is_player_dead() {
            return false;
        }
        if self.nav.goto_target.lock().unwrap().take().is_some() {
            tracing::info!("NAV: player is dead — abandoning /goto");
        }
        *self.nav.goto_entity.lock().unwrap() = None;      // drop any entity chase
        *self.nav.zone_cross.lock().unwrap() = None;        // drop a queued zone-cross
        *self.nav_intent.lock().unwrap() = None;             // stop driving the controller
        self.path.clear();
        self.local_path.clear();
        self.local_i = 0;
        self.path_goal = None;
        self.path_i = 0;
        // A corpse must not act on a plan that lands after it died (#238 + #340).
        self.planner.cancel();
        self.awaiting_first_plan = false;
        // #644: publish an HONEST TERMINAL state, not the ambiguous `idle`. `idle` also means "ready
        // for work", so an agent that issued a goto (accepted while alive) and then polled after the
        // character died mid-route saw `idle` and could not distinguish "arrived / ready" from "you
        // died". `dead` names the condition; it clears back to `idle` on respawn (see `resolve_goal`).
        self.set_nav_state_because(NAV_STATE_DEAD, Some("player_dead"));
        self.publish_debug(Self::known_pos(gs), None);
        true
    }

    /// Live NPC-camp positions to route AROUND (aggro-avoidance, #67), excluding NPCs near the
    /// goal (you're walking TO the destination, often a target mob, so its own camp isn't avoided).
    pub fn aggro_avoid(gs: &GameState, goal: (f32, f32, f32), enabled: bool) -> Vec<[f32; 2]> {
        if !enabled { return Vec::new(); }
        const NEAR_GOAL_SQ: f32 = 55.0 * 55.0;
        gs.world.entities.values()
            .filter(|e| e.is_npc && !e.dead)
            .filter(|e| { let (dx, dy) = (e.x - goal.0, e.y - goal.1); dx * dx + dy * dy > NEAR_GOAL_SQ })
            .map(|e| [e.x, e.y])
            .collect()
    }

    /// The nearest FLOOR-REACHABLE in-zone translocator region (a zone-line region whose
    /// destination is THIS zone), as a goto target the char can walk INTO to teleport out (#266).
    ///
    /// **#543: always `None` while [`TRUST_ADVERTISED_SAME_ZONE_CROSSINGS`] is `false`.** This is
    /// the OTHER path that walks the character onto an advertised same-zone line on nav's own
    /// initiative, and it is unverifiable in exactly the same way: "sealed area, escape through the
    /// in-zone teleport" is only true if the teleport really is in-zone, which the wire cannot say.
    /// Auto-escaping through it can dump the character in another zone — the #543 drift, reached by
    /// a second door. The area is instead reported unreachable, with the pad DISCLOSED
    /// (`nav_declined_pads`) so the agent can choose to take it.
    pub fn find_in_zone_portal(&self, gs: &GameState) -> Option<(f32, f32, f32)> {
        if !TRUST_ADVERTISED_SAME_ZONE_CROSSINGS {
            return None;
        }
        let guard = self.collision.read().unwrap();
        let c = guard.as_ref()?;
        let pos = [gs.player_x, gs.player_y, gs.player_z];
        let in_zone_idxs: Vec<i32> = self.world.zone_points.lock().unwrap().iter()
            .filter(|zp| zp.zone_id == gs.world.zone_id)
            .map(|zp| zp.iterator as i32)
            .collect();
        let portal = c.find_reachable_in_zone_line(&in_zone_idxs, pos).map(|(_, l)| (l[0], l[1], l[2]));
        if tracing::enabled!(tracing::Level::DEBUG) {
            let cands: Vec<_> = in_zone_idxs.iter()
                .filter_map(|&idx| c.find_zone_line_near(Some(idx), pos)
                    .map(|(_, l)| (idx, [l[0].round(), l[1].round(), l[2].round()])))
                .collect();
            tracing::debug!("find_in_zone_portal: pos_z={:.0} in_zone_idxs={in_zone_idxs:?} nearest_per_idx={cands:?} chose_reachable={portal:?}", pos[2]);
        }
        portal
    }

    /// Chase (eqoxide#88): when /goto targets a named ENTITY, re-resolve its CURRENT position each
    /// tick and follow it, instead of pathing to a one-time snapshot.
    pub fn drive_chase(&mut self) {
        let chase = self.nav.goto_entity.lock().unwrap().clone();
        if let Some(name) = chase {
            if self.nav.goto_target.lock().unwrap().is_none() {
                *self.nav.goto_entity.lock().unwrap() = None; // cancelled elsewhere
            } else if let Some(&pos) = self.world.entity_positions().get(&name) {
                *self.nav.goto_target.lock().unwrap() = Some(pos); // follow the entity's latest position
            } else {
                *self.nav.goto_target.lock().unwrap() = None; // entity despawned / left view
                *self.nav.goto_entity.lock().unwrap() = None;
            }
        }
    }

    /// Teleport detection (#266): a position jump far bigger than one tick of walking means we
    /// were repositioned. If mid portal-escape, RESTORE the real goal and re-plan; any other jump
    /// just forces a re-plan off the stale path.
    pub fn drive_teleport_detect(&mut self, gs: &mut GameState) {
        let jumped = (gs.player_x - self.last_walk_pos[0]).hypot(gs.player_y - self.last_walk_pos[1]) > 40.0;
        self.last_walk_pos = [gs.player_x, gs.player_y, gs.player_z];
        if jumped {
            if let Some(ret) = self.escape_return.take() {
                *self.nav.goto_target.lock().unwrap() = Some(ret);
                tracing::info!("NAV: teleported via in-zone portal — resuming goto to ({:.0},{:.0}) (#266)", ret.0, ret.1);
            }
            self.path_goal = None; // force a re-plan from the new position
            self.clear_local_plan();
        }
        if self.portal_cooldown > 0 { self.portal_cooldown -= 1; }
    }

    /// Resolves the active `/goto` target for this tick, or performs the "no active goto"
    /// stop-and-reset and returns `None` when there is none (caller must stop the tick).
    pub fn resolve_goal(&mut self, gs: &GameState) -> Option<(f32, f32, f32)> {
        let goto = *self.nav.goto_target.lock().unwrap(); // copy out so the lock is released
        let goal = match goto {
            Some(t) => t,
            None    => {
                self.path.clear();
                self.path_goal = None;
                self.escape_return = None; // goto cancelled → abandon any in-progress portal escape (#266)
                self.planner.cancel();
                self.clear_local_plan();
                self.awaiting_first_plan = false;
                *self.nav_intent.lock().unwrap() = None;
                // #600: a queued `/zone_cross` is a PENDING intent even before it has a concrete goto
                // goal — `ActionLoop::drain_zone_cross` refuses to resolve it while the zone's assets
                // are not usable and publishes `zone_loading`, re-queueing the one-shot request so it
                // resolves once they land. Resetting that `zone_loading` back to `idle` here (no goto
                // goal is set yet) would flip the state to a misleading "ready/idle" every tick while
                // the load runs. So keep `zone_loading` while a cross is still queued. `/stop`
                // (`CommandState::request_stop`) DOES clear `nav.zone_cross` (round-3 fix), so after a
                // stop `zone_cross_pending` is false and this branch retires `zone_loading`→`idle` on
                // the very next tick — the cross is genuinely cancelled, not left to fire post-load.
                // (Only `zone_loading` is guarded — navigating/planning/dead never coexist with an
                // unresolved queued cross.)
                //
                // #725: the retirement rule is INVERTED — retire everything that is not in the
                // closed [`TERMINAL_NAV_STATES`] set, rather than everything on an opt-in list.
                //
                // The old form listed the states to retire (`navigating`, `navigating_partial`,
                // `planning`, `zone_loading`, `dead`), so any state NOT on the list stuck forever
                // once its goal vanished. Two did, and both were live agent-visible lies:
                //
                //   * `pending` — stamped by `request_zone_cross`, whose one-shot request
                //     `drain_zone_cross` then DRAINED. With the slot empty and no goto goal, nothing
                //     could ever retire it: measured `nav_state: "pending"` with `nav_reason: null`
                //     for 45 s / 48 s / 75 s while `docs/http-api.md` told the agent that meant "your
                //     goal is genuinely in flight" (#725).
                //   * `following` — published by the `FollowHold` arm while a chase holds near its
                //     leader. When that leader despawns, `drive_chase` clears BOTH nav slots, and
                //     `following` was left standing over a chase with nothing left to chase.
                //
                // Listing those two would fix those two. Inverting the rule fixes the CLASS: an
                // in-progress state invented tomorrow retires by default, and only a deliberate
                // addition to the terminal set can make a word survive with no goal behind it.
                // The one thing that legitimately outlives a goto goal: a `/zone_cross` request that
                // is STILL QUEUED. It has no concrete goal yet by construction — the concrete
                // zone-line goal only exists after `drain_zone_cross` resolves it — so "no goal ⇒
                // retire" would be wrong for exactly as long as the request sits in its slot, and
                // wrong in the OTHER direction: `idle` while a crossing the client fully intends to
                // perform is queued reads as "your request was dropped", and the character then
                // crosses anyway. That was #600's `zone_loading` case; writing this test found that
                // `pending` (what `request_zone_cross` itself stamps) has it too, in the window
                // between the accept on the HTTP thread and the drain on the net thread. Gate on the
                // REQUEST, not on the state word — the queued request is the thing that makes an
                // in-progress state true, and the word it happens to carry is not.
                //
                // This cannot resurrect #725: the slot is emptied by the drain, so from that instant
                // `held_for_cross` is false and the very next tick retires anything non-terminal.
                let zone_cross_pending = self.nav.zone_cross.lock().unwrap().is_some();
                let current = self.nav.nav_state.lock().unwrap().state.clone();
                if !zone_cross_pending && !nav_state_is_terminal(&current) {
                    // #644: once the player has RESPAWNED (no longer dead ⇒ this tick reaches
                    // `resolve_goal`), retire the terminal `dead` back to `idle` so the honest death
                    // state doesn't linger as a new never-clearing observable — and SAY WHY, so
                    // "idle because you came back" is distinguishable from "idle, ready for work".
                    let why = if current == NAV_STATE_DEAD { NAV_REASON_RESPAWNED } else { NAV_REASON_GOAL_DROPPED };
                    self.set_nav_state_because("idle", Some(why));
                    // #725 review N3's KNOWN GAP is CLOSED here (#732). It read: "`set_nav_state_
                    // because` never clears `s.goal`, so this retirement can leave the abandoned
                    // goal's coordinates standing beside `idle`". It does now — every `idle` goes
                    // through `NavStatus::retire_to_idle`, which owns `goal`. That covers this
                    // retirement (`goal_dropped`/`respawned`) as well as the zone-change one #732
                    // was filed against, because it is the same writer.
                }
                // Publish the cleared/terminal state so the snapshot does not keep saying
                // "arrived"/"navigating" with a route after the goto ended, and REPUBLISH whenever
                // an idle field drifts — the player moved (WASD / server push), the zone model
                // loaded — so a consumer can never read a stale confident position (#615 review
                // F1). `debug_is_settled` gates it to actual drift, not every idle tick.
                if !self.debug_is_settled(gs) {
                    self.publish_debug(Self::known_pos(gs), None);
                }
                return None;
            }
        };
        Some(goal)
    }

    /// FAST STEERING (#nav-multires). Re-projects the CURRENT position onto the stable fine path
    /// every ~10ms (far more often than the 150ms plan gate) and refreshes ONLY `nav_intent`'s
    /// `wish_dir` (+ facing) — the flags/speed the walker set stay.
    pub fn apply_fast_steering(&mut self, gs: &mut GameState) {
        if !self.local_path.is_empty() && self.nav.goto_target.lock().unwrap().is_some() {
            // NO LOS clamp in the ~10ms fast loop (#685, owner): with the coarse route now INFLATED off
            // convex corners (`inflate_route_off_corners`), the fine path the fast loop pursues no
            // longer grazes a corner, so a clamp here is redundant — and clamping the carrot every
            // ~10ms at an apex is exactly the "wiggle through each corner, sloppy and slow" the owner
            // saw. The route offset removes the CAUSE; the light backstop clamp stays only on the 150ms
            // coarse tick (`steer_target`), never in this hot loop. So the fast aim is plain pursuit.
            if let Some((wish_dir, heading)) =
                fast_steer_aim(&self.local_path, &mut self.local_i, [gs.player_x, gs.player_y, gs.player_z], 5.0, |_, _| true)
            {
                if let Some(intent) = self.nav_intent.lock().unwrap().as_mut() {
                    intent.wish_dir = wish_dir;
                }
                gs.player_heading = heading;
            }
        }
    }

    /// The walker: (re)plans the coarse/fine route toward `goal`, steers pure-pursuit along it,
    /// and drives arrival/stall/fall-edge handling. This is the tail of the old `tick()` — every
    /// early return here is a return from the tick, exactly as before the split.
    ///
    /// Writes ONLY the per-frame `nav_intent` (the intent-only movement boundary — see the module
    /// doc). A big single-step drop is no longer special-cased: §442 (#442) retired the controlled-
    /// fall handoff, so the walker just keeps walking toward the goal and the render controller's ONE
    /// collided gravity path descends off the edge; the landing damage is applied driver-agnostically
    /// in `ActionLoop::stream_position`. The only thing this method still does about big drops is the
    /// pre-emptive lethal-fall SAFETY guard (don't walk off a ledge a fall from which would kill us).
    /// Resolve this zone's intra-zone teleport pads (#403) for the planner. Same-zone DRNTP
    /// translocators from the `OP_SendZonepoints` list — filtered to `zp.zone_id == gs.world.zone_id` (so a
    /// CROSS-zone line is never turned into an intra-zone teleport) and with the keep-position
    /// sentinel (`999999`, relocates nobody) dropped — then honesty-gated by `resolve_teleport_pads`
    /// (only pads whose footprint AND advertised destination land on walkable floor become edges).
    /// Empty in the common case (a zone with no same-zone pads), so ordinary plans pay nothing.
    ///
    /// **#543: the returned edge list is EMPTY while [`TRUST_ADVERTISED_SAME_ZONE_CROSSINGS`] is
    /// `false`** — nav will not steer the character onto a crossing it cannot verify. The pads are
    /// still resolved, because resolving them is how the client learns the footprint and the
    /// advertised arrival it must DISCLOSE: each one is recorded as
    /// [`crate::diagnostics::PadKnowledge::AdvertisedSameZoneDeclined`] and published for the agent
    /// to act on (or not). Declining and staying silent would swap one lie for another.
    fn same_zone_teleport_pads(&mut self, gs: &GameState, c: &crate::collision::Collision)
        -> Vec<crate::collision::PadEdge> {
        use crate::diagnostics::{PadDebug, PadKnowledge};
        let mut advertised: Vec<(i32, [f32; 3])> = Vec::new();
        // Same-zone pads with NO usable advertised destination (the keep-position sentinel): their
        // true behaviour has never been observed — `Unknown`, first-class, in the debug record.
        let mut unknown_idxs: Vec<i32> = Vec::new();
        for zp in self.world.zone_points.lock().unwrap().iter() {
            if zp.zone_id != gs.world.zone_id { continue; }
            if zp.server_x.abs() < 900_000.0 && zp.server_y.abs() < 900_000.0 && zp.server_z.abs() < 900_000.0 {
                advertised.push((zp.iterator as i32, [zp.server_x, zp.server_y, zp.server_z]));
            } else {
                unknown_idxs.push(zp.iterator as i32);
            }
        }

        // The DISCLOSURE's question is a DIFFERENT one — "can the AGENT take this pad?" — and it
        // must not be answered from the advertised destination (#660 review B1). The first revision
        // classified from `resolved`, which needs BOTH ends, so a pad with a perfectly standable
        // footprint whose ADVERTISED arrival had no floor collapsed into `AdvertisedUnusable` and
        // was withheld — a pad the agent can walk onto, hidden on the strength of the one datum this
        // whole gate exists because the client cannot trust. That is the #266 pad class exactly:
        // `find_in_zone_portal` never required a resolvable destination.
        //
        // So the ONLY thing that silences a pad now is having no DRNTP region in the loaded map at
        // all — nothing to point at. Everything else is reported as a fact, including "I found no
        // standable point inside it" (`footprint: None`), which is a warning to the agent, not a
        // reason to go quiet. `Unknown` keeps its #607 meaning: nothing advertised AND nothing to
        // point at. The `Learned*` states stay unused — the agent owns pad memory (owner, #543).
        //
        // ONE ENTRY PER INDEX, not per leaf. A DRNTP region is a BSP and one index routinely has
        // dozens of leaves (qeynos2 index 2: 58, measured live) — an offer each is noise, not
        // disclosure. `footprint` is the leaf NEAREST the character (the actionable "walk here") and
        // `footprint_count` carries what the multiplicity actually means to a caller (#660 NB2).
        let here = [gs.player_x, gs.player_y, gs.player_z];

        let by_distance = |mut ps: Vec<[f32; 3]>| {
            let d2 = |p: &[f32; 3]| (p[0] - here[0]).powi(2) + (p[1] - here[1]).powi(2) + (p[2] - here[2]).powi(2);
            ps.sort_by(|a, b| d2(a).total_cmp(&d2(b)));
            ps
        };
        let mut pads: Vec<PadDebug> = Vec::new();
        let mut classify = |idx: i32, wire_dest: Option<[f32; 3]>| {
            // Is there a region for this index in the map AT ALL? (`find_zone_line_near` does not
            // require standability — it answers "where is it", not "can you use it".)
            let Some((_, region_at)) = c.find_zone_line_near(Some(idx), here) else {
                pads.push(PadDebug { index: idx, knowledge: match wire_dest {
                    Some(_) => PadKnowledge::AdvertisedUnusable, // advertised, but absent from our map
                    None    => PadKnowledge::Unknown,            // nothing advertised, nothing to point at
                }});
                return;
            };
            let footprints = c.teleport_pad_footprints(idx);
            // Where the ADVERTISEMENT lands on our floor model, if anywhere. Reported separately from
            // the verbatim wire value so neither is passed off as the other (#660 review NB3): the
            // wire datum is the server's claim, the snap is our derivation from it. Derived through
            // `resolve_teleport_pads` so the number disclosed here and the number the planner would
            // have used cannot drift; `.map(dest)` because we want only its destination half.
            let dest_floor = wire_dest
                .and_then(|d| c.resolve_teleport_pads(&[(idx, d)]).first().map(|e| e.dest));
            // Nearest-first, then thinned so each offered spot is a genuinely DIFFERENT place.
            let spread = spread_spots(by_distance(footprints.clone()), OFFERED_SPOTS, SPOT_SEPARATION);
            let footprint = spread.first().copied();
            // The rest of the offer: the next few spots to TRY if the first fires nothing. Verified
            // live (#660) that leaves of one pad genuinely differ in whether they trigger — one spot
            // fired nothing while another on the same pad crossed — so a bare count without the
            // alternates would be a number the agent cannot act on.
            let alternates: Vec<[f32; 3]> = spread.iter().skip(1).copied().collect();
            // Only computed when the gate is ON — under the gate no edge can ever be produced, so
            // running the batch resolve for a value nothing reads was pure waste (#660 review NB).
            let usable = match (TRUST_ADVERTISED_SAME_ZONE_CROSSINGS, footprint, wire_dest) {
                (true, Some(fp), Some(d)) => c.resolve_teleport_pads(&[(idx, d)]).into_iter()
                    .find(|e| e.source == fp),
                _ => None,
            };
            pads.push(PadDebug { index: idx, knowledge: match usable {
                Some(ref e) => PadKnowledge::AdvertisedUsable { source: e.source, dest: e.dest },
                None => PadKnowledge::AdvertisedSameZoneDeclined {
                    footprint,
                    footprint_count: footprints.len(),
                    alternates,
                    region_at,
                    advertised_dest: wire_dest,
                    advertised_dest_floor: dest_floor,
                },
            }});
        };
        for &(idx, dest) in &advertised { classify(idx, Some(dest)); }
        for idx in unknown_idxs { classify(idx, None); }
        self.last_pads = pads;

        // THE GATE. Nothing reaches A*: a goal reachable only across an unverifiable pad is an
        // honest `no_path` plus the disclosure above, never a silent drift into another zone (#543).
        if !TRUST_ADVERTISED_SAME_ZONE_CROSSINGS {
            return Vec::new();
        }
        // Only reached with the gate ON. The batch resolve lives HERE, not above, so the gated-off
        // path does not compute a value nothing reads (#660 review NB).
        if advertised.is_empty() { Vec::new() } else { c.resolve_teleport_pads(&advertised) }
    }

    /// #579 (agent-honesty): there is no collision grid, so this client has NO model of the world —
    /// the zone's terrain GLB is still loading, or its load failed. Abandon any route, stop driving
    /// the controller, and say so. The `/goto` target is deliberately KEPT: once the assets land,
    /// `replan_decision` posts a real plan and navigation resumes on its own.
    ///
    /// This replaces the old behaviour, which is the bug: with no collision the walker published
    /// `nav_state: "navigating"` and steered in a dead-straight line at the goal, so an agent
    /// observing mid-load saw a confident walkable route through geometry that had not been built
    /// (the "700u unobstructed" of the false #560 report).
    fn halt_no_world(&mut self, player: Option<[f32; 3]>, reason: &str) {
        self.path.clear();
        self.path_i = 0;
        self.path_goal = None;      // force a REAL plan the moment collision appears
        self.clear_local_plan();
        self.planner.cancel();
        self.awaiting_first_plan = false;
        *self.nav_intent.lock().unwrap() = None;
        // `reason` is the machine-readable WHY from `zone_assets::usability` (#600) — the SAME
        // vocabulary the HTTP surface publishes: `zone_assets_pending` / `_failed` / `_idle` /
        // `_stale_for_previous_zone` / `player_zone_unknown`. Each is verifiably the variant the
        // one decision function returned, not a coarser reworded claim.
        self.set_nav_state_because(NAV_STATE_ZONE_LOADING, Some(reason));
        // Publish honestly: `zone_model_loaded: false`, no routes — "I have no model of this
        // world", never a route through unloaded geometry (#579). `player` comes from the caller's
        // GameState (None until the server placed us — never a fabricated position, #615 F1).
        self.publish_debug(player, None);
    }

    pub fn drive_walk(&mut self, gs: &mut GameState, goal: (f32, f32, f32)) {
        // THE ONE DECISION FUNCTION (#600). May nav route on the loaded world at all? This is the
        // SAME predicate the HTTP surface goes through (`zone_assets::usability`), so the walker is
        // no longer a consumer that opts out. It supersedes the old `collision.is_none()` check:
        //   * no grid at all (Idle/Pending/Failed) — as before, refuse (#579), AND
        //   * a grid that is present but belongs to the zone we just LEFT — the ~1-frame stale
        //     window after the net thread published the new `player.zone` but before the render
        //     thread ran `begin_zone_load` (StaleForPreviousZone). The old check could not see this
        //     (the previous zone's grid is present + non-empty), so the walker would have routed on
        //     the WRONG world — the #560 shape this fix closes.
        // A `None` verdict is returned ONLY for a `Ready` grid whose zone == `gs.world.zone_name`
        // (the SAME string the HTTP `player().zone` reads), so passing this gate guarantees
        // `self.collision` below is the RIGHT zone's grid. Checked BEFORE any planning/steering.
        if let Some(why) = {
            let st = crate::zone_assets::lock_state(&self.zone_assets);
            crate::zone_assets::usability(&st, &gs.world.zone_name)
        } {
            self.halt_no_world(Self::known_pos(gs), why.as_str());
            return;
        }
        if self.replan_cooldown > 0 { self.replan_cooldown -= 1; }
        // Throttled live clearance sample for the diagnostics snapshot (#608).
        self.refresh_clearance([gs.player_x, gs.player_y, gs.player_z]);
        let is_chase = self.nav.goto_entity.lock().unwrap().is_some();
        let in_flight = self.planner.in_flight_goal().map(|g| (g[0], g[1], g[2]));
        let decision = replan_decision(self.path_goal, goal, in_flight, self.replan_coarse, is_chase);
        if decision.reset_route {
            self.path.clear();
            self.clear_local_plan();
            self.path_i = 0;
            self.stuck_i = 0;
            self.backoff_ticks = 0;
            self.stuck_best = f32::MAX;
            self.stuck_ticks = 0;
            self.nav_repaths = 0;
            self.proactive_replans = 0;
            self.nav_best_gdist = f32::MAX;
            // #631 gap 3: a genuinely NEW destination — restart closest-approach tracking so the
            // no-progress window measures progress toward THIS goal, not a stale one. (The `f32::MAX`
            // sentinel self-initialises `nav_progress_at` on the first drive tick, but resetting the
            // clock here too keeps the window honest across a re-aim.)
            self.nav_best_g3d = f32::MAX;
            self.nav_progress_at = std::time::Instant::now();
            // #851: a genuinely NEW destination — the route facts describe a route to somewhere
            // else, and the execution verdict is about a journey that is over. (A new `/goto` also
            // bumps `goal_id`, which `tick_drive_state` keys off; this covers the routes that
            // re-aim WITHOUT a new goal id — a `/follow` chase whose leader moved a cell.)
            self.reset_drive_state();
            self.replan_cooldown = 0;
            self.replan_coarse = false;
            self.goal_snapped = false;
        }
        if decision.post {
            if !decision.reset_route {
                self.replan_coarse = false;
                self.local_stuck_ticks = 0;
                self.replan_cooldown = REPLAN_COOLDOWN_TICKS;
            }
            let av = *self.nav.nav_avoid.lock().unwrap();
            let avoid = Self::aggro_avoid(gs, goal, av.enabled);
            let col = self.collision.read().unwrap().as_ref().cloned(); // Arc clone, not the grid
            match col {
                Some(c) => {
                    let goal_region = c.zone_line_at([goal.0, goal.1, goal.2 + 1.0]);
                    let teleport_pads = self.same_zone_teleport_pads(gs, &c);
                    let t0 = std::time::Instant::now();
                    let gen = self.planner.request(crate::planner::PlanRequest {
                        gen: 0, // assigned by the planner
                        start: [gs.player_x, gs.player_y, gs.player_z],
                        goal:  [goal.0, goal.1, goal.2],
                        avoid,
                        aggro_buffer: av.buffer,
                        goal_region,
                        teleport_pads,
                        collision: c,
                    });
                    self.path_goal = Some(goal); // the goal the committed/incoming route is FOR
                    // #631 gap 1: remember WHICH command (goal_id) this plan answers, to stamp onto
                    // its published PlanDebug when the reply lands.
                    self.plan_goal_id = self.nav.nav_state.lock().unwrap().goal_id;
                    let post_us = t0.elapsed().as_micros();
                    tracing::info!("NAV: posted plan #{gen} to ({:.0},{:.0}) — {post_us}us on the net thread (was: the whole A*)",
                        goal.0, goal.1);
                    if self.path.is_empty() {
                        self.awaiting_first_plan = true;
                        self.set_nav_state("planning");
                        *self.nav_intent.lock().unwrap() = None;
                    }
                }
                // The collision grid vanished between the gate at the top of this fn and here (a
                // zone change landing mid-tick: the render thread ran `begin_zone_load`, which
                // clears the grid AND sets the state to `Pending` for the new zone in one call).
                // Same honest answer, never a bare "navigating".
                None => { self.halt_no_world(Self::known_pos(gs), "zone_assets_pending"); return; }
            }
        }

        if let Some(reply) = self.planner.poll() {
            if self.apply_plan(reply, gs, goal) { return; }
        }

        if self.planner.is_dead() {
            self.stop_nav(gs, "no_path", "planner_dead", &format!(
                "The pathfinding worker thread has DIED — no route to ({:.0},{:.0}) or anywhere else can be \
                 planned for the rest of this session. This is a client fault, not an unreachable goal; \
                 movement must be driven manually or the client restarted.", goal.0, goal.1));
            return;
        }

        if self.awaiting_first_plan {
            *self.nav_intent.lock().unwrap() = None;
            self.publish_debug(Self::known_pos(gs), None); // "planning", no route yet
            return;
        }

        // PURE-PURSUIT path following.
        const LOOK_AHEAD: f32 = 5.0;
        let px = gs.player_x;
        let py = gs.player_y;
        let pz = gs.player_z;
        self.advance_cursor([px, py, pz]);
        let have_path = !self.path.is_empty();
        let target: (f32, f32, f32) = if have_path {
            // How far ahead on the coarse route the fine plan aims. Read from `steering` rather than
            // restated here so `resync_cursor`'s carrot-collapse check (#733) and this call site can
            // never end up judging two different carrots.
            use crate::steering::LOCAL_REACH;
            const LOCAL_BOUND: f32 = 40.0;   // the fine search window (keeps it bounded → it terminates)
            let coarse = carrot_along(&self.path, self.path_i, [px, py, pz], LOOK_AHEAD)
                .unwrap_or([goal.0, goal.1, gs.player_z]);
            if let Some(reply) = self.local_planner.poll() {
                self.apply_local_plan(reply);
            }

            if !self.local_path.is_empty()
                && (px - self.local_from[0]).hypot(py - self.local_from[1]) > LOCAL_BOUND
            {
                self.clear_local_plan();
            }

            let local_goal = carrot_along(&self.path, self.path_i, [px, py, pz], LOCAL_REACH).unwrap_or(coarse);
            if let Some(c) = self.collision.read().unwrap().as_ref().cloned() {
                self.local_planner.post_if_idle(crate::planner::LocalRequest {
                    gen: 0, // assigned by the planner
                    start: [px, py, gs.player_z],
                    goal:  local_goal,
                    cell:  LOCAL_CELL,
                    bound: LOCAL_BOUND,
                    carrot_tol: LOCAL_CELL * 2.0,
                    collision: c,
                });
            }
            if self.local_planner.is_dead() {
                self.set_nav_local(Some(eqoxide_ipc::NavLocal {
                    state: "planner_dead".into(), reason: "local_planner_dead".into(),
                    stuck_ticks: 0, plan_us: 0,
                }));
                // #766 B3: and mirror it into the shared row, which retirement keeps (the latch is
                // scoped to the WORKER, not to the session — round-6 review B12). The
                // `set_nav_local` above is a PER-GOAL channel that #766 now retires on every route
                // to `idle` — correct for the two verdict states, wrong for this one. See the
                // method's doc for why this call sits here and not somewhere more general.
                self.latch_local_planner_liveness();
            }

            // LOS clamp (#685): shorten the carrot at a convex corner so the walker rounds it instead
            // of chording across the wall. Same `path_clear` volume-sweep the controller/A* use; clear
            // when no grid is loaded. Held for the single synchronous `steer_target` call only.
            let coll = self.collision.read().unwrap();
            let los = |a: [f32; 3], b: [f32; 3]|
                coll.as_ref().map_or(true, |c| c.carrot_los_clear(a, b, STEER_LOS_CLEARANCE));
            let aim = steer_target(&self.path, self.path_i, &self.local_path, &mut self.local_i,
                [px, py, pz], LOOK_AHEAD, coarse, los);
            drop(coll);
            (aim[0], aim[1], aim[2])
        } else {
            self.clear_local_plan();
            (goal.0, goal.1, gs.player_z)
        };
        // (The committed coarse/fine routes are published in the snapshot at the end of this tick —
        // the old separate `nav_path_view` pair is gone: ONE published source, #608.)

        let dx   = target.0 - gs.player_x; // east  delta (server_x)
        let dy   = target.1 - gs.player_y; // north delta (server_y)
        let dist = (dx * dx + dy * dy).sqrt();

        // Big single-step drop ahead: no longer a controlled-fall handoff (§442, #442 retired that —
        // the render controller falls off the edge under its ONE collided gravity path). We keep only
        // the pre-emptive lethal-fall SAFETY guard: don't walk off a ledge a fall from which would
        // kill us. (`drop_to_target` is the waypoint-based drop, used ONLY for this stop decision —
        // the actual fall damage is computed from the controller's own tracked airborne height.)
        const FALL_TRIGGER: f32 = 18.0; // bigger than a stair/ledge step (the walk STEP_H is 20)
        let drop_to_target = gs.player_z - target.2;
        let water_landing = self.collision.read().unwrap().as_ref()
            .is_some_and(|c| c.in_water([target.0, target.1, target.2 + 3.0]));
        if drop_to_target > FALL_TRIGGER && dist <= STOP_DIST + 8.0 && !water_landing {
            let (_, max_dmg) = fall_damage(drop_to_target);
            if gs.cur_hp > 0 && max_dmg >= gs.cur_hp as u32 {
                tracing::info!("NAV: fall of {:.0}u (up to {} dmg) would exceed {} hp — stopping at ledge",
                    drop_to_target, max_dmg, gs.cur_hp);
                gs.log_msg("zone", "Fall too dangerous (HP too low) — stopped at the ledge");
                self.set_nav_state_because("blocked", Some("fall_would_be_lethal"));
                *self.nav.goto_target.lock().unwrap() = None;
                *self.nav_intent.lock().unwrap() = None; // else the controller keeps walking the last
                // wish_dir forever — drifting 1000s of units with no nav activity (eqoxide#71).
                self.publish_debug(Self::known_pos(gs), None);
                return;
            }
            // Non-lethal: fall through to normal walking — the controller descends off the edge.
        }

        // Arrival: measure distance to the FINAL goal, not the look-ahead carrot.
        let gdx = goal.0 - gs.player_x;
        let gdy = goal.1 - gs.player_y;
        let gdist = (gdx * gdx + gdy * gdy).sqrt();
        // ...and the VERTICAL gap to the goal's FLOOR (#344). Correct x/y at the wrong z — the NPC a
        // storey up, A* having routed to the floor below it — is NOT arrival. Anchor to the goal's
        // RESOLVED floor (the tier `astar` plans to), not the caller's raw z: a sloppy z the planner
        // projected onto a real floor must still count as arrived when the walker reaches that floor.
        let goal_floor_z = self.collision.read().unwrap().as_ref()
            .and_then(|c| c.resolve_goal_floor([goal.0, goal.1, goal.2]))
            .unwrap_or(goal.2);
        let gdz = goal_floor_z - gs.player_z;
        let following = self.nav.goto_entity.lock().unwrap().is_some();
        match arrival_action(gdist, gdz, following) {
            ArrivalAction::FollowHold => {
                self.set_nav_state("following");
                self.path.clear();
                self.path_goal = None;
                *self.nav_intent.lock().unwrap() = None; // stand still until the leader moves
                gs.player_heading = eq_heading(gdx, gdy);
                self.publish_debug(Self::known_pos(gs), None);
                return;
            }
            ArrivalAction::Arrived => {
                if let Some(ret) = self.escape_return.take() {
                    tracing::info!("NAV: reached the in-zone portal without teleporting — resuming goto to ({:.0},{:.0})", ret.0, ret.1);
                    *self.nav.goto_target.lock().unwrap() = Some(ret);
                    self.path_goal = None;
                    *self.nav_intent.lock().unwrap() = None;
                    return;
                }
                tracing::info!("NAV: arrived at ({:.1},{:.1},z~{:.1}) (goal floor z={:.1}, |dz|={:.1})",
                    goal.0, goal.1, gs.player_z, goal_floor_z, gdz.abs());
                if self.goal_snapped {
                    self.set_nav_state_because("arrived", Some("goal_z_snapped"));
                } else {
                    self.set_nav_state("arrived");
                }
                *self.nav.goto_target.lock().unwrap() = None;
                *self.nav_intent.lock().unwrap() = None; // stop driving the controller
                // #851 review round 1, N3 — `reset_drive_state`'s own doc names "a terminated
                // journey", and ARRIVAL is the one terminal route that was not calling it. The
                // published row was already honest (the word change above clears `nav_stall`), so
                // this was latent rather than live; what it left standing was the WALKER-side
                // `exec`/`last_progress_at`/`committed`, i.e. a stalled verdict and a route for a
                // journey that ended in success. `stop_nav` (the terminal failure route) has always
                // done this; the two terminal routes now agree.
                self.reset_drive_state();
                gs.player_heading = eq_heading(gdx, gdy);
                self.publish_debug(Self::known_pos(gs), None);
                return;
            }
            ArrivalAction::Drive => {} // not there yet — keep walking / re-plan below
        }

        const REPATH_RESET_DIST: f32 = 200.0;
        if gdist < self.nav_best_gdist - REPATH_RESET_DIST {
            self.nav_best_gdist = gdist;
            self.nav_repaths = 0;
            self.proactive_replans = 0;
        }

        // ROUTE-LEVEL NO-PROGRESS DETECTION (#631 gap 3 — the Crushbone-moat honesty fix).
        //
        // The stall detector below catches a walker whose `path_i` STOPS advancing. It is blind to
        // one that keeps re-planning PARTIAL routes in laps while making no headway toward the GOAL —
        // the moat: the walker swam partial routes around the castle ring for 3+ minutes, no terminal
        // state ever. We terminate honestly when the journey is genuinely getting nowhere.
        //
        // THE PROGRESS SIGNAL IS TWO-CHANNEL, and the walker is progressing if EITHER fires:
        //   (a) COMMITTED-ROUTE progress — the walker advanced along a COMPLETE route (`path_i` past
        //       the max seen on this route, while `self.committed` records a COMPLETE route; #851
        //       moved that qualifier off the published string — see the note at the term itself,
        //       and `driving_nav_state` for why the string stopped being usable for it). A complete route's end
        //       IS the goal, so advancing it is guaranteed goal-ward progress *by construction* — it
        //       cannot be a lap (a lap would be a re-planned PARTIAL, `navigating_partial`, or would
        //       stop advancing `path_i` and trip `walker_stalled`).
        //
        //       THE PREMISE UNDER "by construction" IS THAT `path_i` ONLY ADVANCES BY WALKING, and
        //       #673's stale-cursor resync would have broken it — a resync can move the cursor
        //       several segments in one tick without the character walking any of them, and a
        //       reachability predicate that got it wrong would then reset this very clock with
        //       progress the walker never made. So `Walker::advance_cursor` raises `stuck_i` with
        //       any resync jump, which keeps `path_i > stuck_i` reachable ONLY through the monotone
        //       (walked) advance and leaves this comment's premise true. Read it there before
        //       changing either side. (#727)
        //
        //       This is the fix for the reviewer's
        //       false-fire: a legitimate long go-around across a barrier (river/wall/moat) whose START
        //       is the closest straight-line point to the goal makes NO closest-approach improvement
        //       for most of the trip, yet is plainly getting there — killing it was a confident
        //       falsehood on a legit route. Straight-line distance to the goal is irrelevant while a
        //       complete route is being traversed.
        //   (b) CLOSEST APPROACH — `√(gdist² + gdz²)` to the goal's resolved floor improved by
        //       [`NAV_PROGRESS_EPS`]. This is the honest signal for PARTIAL navigation (`navigating_
        //       partial`), where advancing `path_i` can be a lap: a legit partial makes genuine
        //       goal-ward progress (`PARTIAL_MIN_UNITS`) so its closest approach keeps improving,
        //       while the moat's laps never close on the goal → no improvement → terminate.
        //
        // Only when NEITHER channel has fired for [`NAV_NO_PROGRESS_WINDOW`] do we stop. Further
        // guards against over-firing: 3-D distance (a spiral/vertical climb toward a goal above
        // counts as approach); FIXED-destination gotos only (a `/follow` chase's goal moves with the
        // leader); and only while walking a committed route (`have_path`).
        if have_path && !following {
            let g3d = (gdist * gdist + gdz * gdz).sqrt();
            let now = std::time::Instant::now();
            // The cursor advanced by WALKING this tick — `path_i` past this route's max-so-far
            // (`stuck_i`, which the stall block below maintains and which is reset to 0 on every
            // re-plan, so a fresh route's advancement is always seen; #727 raises it on a resync so
            // that a jump the character did not walk cannot reach this).
            let cursor_advanced = self.path_i > self.stuck_i;
            // (a) advancing a COMPLETE committed route. A complete route's end IS the goal, so
            // advancing it is guaranteed goal-ward progress by construction; on a PARTIAL it could
            // be a lap, which is why the completeness qualifier is here.
            //
            // #851 reads that qualifier off `self.committed` — the walker's own record of what it
            // installed — instead of off the published `nav_state` string it wrote earlier. The old
            // `nav_state_is("navigating")` read was correct only while `navigating` was the ONLY
            // word a complete route could be published under; adding `navigating_stalled` would have
            // silently turned it into "…and not currently stalled", which is a different predicate
            // and would have kept this clock running through a stall the walker then escaped.
            let advancing_complete_route =
                self.committed.map(|c| c.route) == Some(crate::steering::CommittedRoute::Complete)
                && cursor_advanced;
            // (b) closest-approach improvement (side-effecting: lowers `nav_best_g3d`).
            let closer = crate::steering::progress_improved(&mut self.nav_best_g3d, g3d, NAV_PROGRESS_EPS);
            // #851 — THE HONESTY PUBLICATION. The two-channel signal has always been computed here;
            // until now it was only ever *consulted* at the 60 s give-up below, so a walker that had
            // made no progress for 3 s and one walking cleanly published the identical
            // `navigating`. `progressed` is deliberately the ROUTE-EXECUTION reading of the same two
            // channels — cursor advance counts on a partial too, because executing a partial is
            // real execution and `navigating_partial` already says the route is not to the goal.
            //
            // Ordering: this runs BEFORE the give-up below, before the oscillation guard and before
            // the stall/back-off block, all of which may `return` from the tick. Publishing last
            // would mean the ticks that matter most never publish at all.
            let progressed = cursor_advanced || closer;
            let exec = self.tick_drive_state(progressed);
            self.publish_drive_state();
            // The universal, re-checked against the ROW an observer actually reads, on every tick of
            // every test that drives a walker. The words come from the constants, not from literals,
            // so `the_driving_nav_state_word_is_only_ever_written_through_the_verdict_851` still
            // reads zero driving-word literals in this file's production region.
            debug_assert!(!(exec.is_stalled() && {
                    let published = self.nav.nav_state.lock().unwrap().state.clone();
                    published == crate::steering::NAV_STATE_NAVIGATING
                        || published == crate::steering::NAV_STATE_NAVIGATING_PARTIAL
                }),
                "#851: a stalled verdict must never be published as unqualified progress");
            if advancing_complete_route || closer {
                self.nav_progress_at = now;
            } else if now.duration_since(self.nav_progress_at) >= NAV_NO_PROGRESS_WINDOW {
                self.stop_nav(gs, "blocked", "no_progress", &format!(
                    "No progress toward ({:.1},{:.1}) for {}s: the walker keeps moving but its closest \
                     approach to the goal has not improved (held at ~{:.0}u). This is the moving-but-\
                     going-nowhere case the stall detector misses — a lap/eddy the route cannot escape \
                     (e.g. swimming a moat ring), not a physical wedge. A coarse route keeps being \
                     followed, but it does not get closer. Approach from another direction or pick a \
                     reachable waypoint.",
                    goal.0, goal.1, NAV_NO_PROGRESS_WINDOW.as_secs(), self.nav_best_g3d));
                return;
            }
        }

        // OSCILLATION GUARD (#378 Phase 2 — the live qcat L-corner honesty fix).
        if self.proactive_replans >= PROACTIVE_REPLAN_CAP {
            self.stop_nav(gs, "blocked", "local_no_way_through", &format!(
                "Wedged near ({:.1},{:.1}) after {} proactive coarse re-plans that did not get the \
                 journey past this spot: the fine 2u planner cannot thread the committed route here, \
                 and re-routing keeps returning to the same impasse. The corridor is not traversable at \
                 the character's collision radius from this approach — a coarse route to the goal exists, \
                 but the walker cannot follow it around this corner. Approach from another direction.",
                gs.player_x, gs.player_y, self.proactive_replans));
            return;
        }

        // Active downhill back-off (eqoxide#212).
        if self.backoff_ticks > 0 {
            self.backoff_ticks -= 1;
            *self.nav_intent.lock().unwrap() = Some(MoveIntent {
                wish_dir:    self.backoff_dir,
                wish_vspeed: 0.0,
                jump:        false,
                want_swim:   false,
                speed:       nav_speed(gs),
                climb:       0.0,
                hop:         false,
            });
            if self.backoff_ticks == 0 {
                let av = *self.nav.nav_avoid.lock().unwrap();
                let avoid = Self::aggro_avoid(gs, goal, av.enabled);
                let col = self.collision.read().unwrap().as_ref().cloned();
                if let Some(c) = col {
                    let goal_region = c.zone_line_at([goal.0, goal.1, goal.2 + 1.0]);
                    let teleport_pads = self.same_zone_teleport_pads(gs, &c);
                    let gen = self.planner.request(crate::planner::PlanRequest {
                        gen: 0,
                        start: [gs.player_x, gs.player_y, gs.player_z],
                        goal:  [goal.0, goal.1, goal.2],
                        avoid,
                        aggro_buffer: av.buffer,
                        goal_region,
                        teleport_pads,
                        collision: c,
                    });
                    self.stuck_ticks = 0;
                    self.plan_goal_id = self.nav.nav_state.lock().unwrap().goal_id; // #631 gap 1
                    tracing::warn!("NAV: backed off downhill — posted re-plan #{gen} (attempt {})", self.nav_repaths);
                }
            }
            self.publish_debug(Self::known_pos(gs), None);
            return;
        }

        // Progress-based stall detection.
        if have_path {
            if self.path_i > self.stuck_i {
                self.stuck_i = self.path_i;
                self.stuck_ticks = 0;
            } else {
                self.stuck_ticks += 1;
                if self.stuck_ticks >= NAV_STUCK_TICKS {
                    self.stuck_ticks = 0;
                    if self.nav_repaths < 8 {
                        self.nav_repaths += 1;
                        self.backoff_ticks = NAV_BACKOFF_TICKS;
                        self.backoff_dir = if dist > 1e-3 { [-dx / dist, -dy / dist] } else { [0.0, 0.0] };
                        tracing::warn!("NAV: no progress near ({:.1},{:.1}) — backing off downhill (attempt {})",
                            gs.player_x, gs.player_y, self.nav_repaths);
                        return;
                    }
                    if self.local_says_no_way_through() {
                        self.stop_nav(gs, "blocked", "local_no_way_through", &format!(
                            "Wedged at ({:.1},{:.1}) after {} re-path attempts — and the FINE 2u planner has \
                             CLOSED its whole 40u window without finding a way along the committed route. The \
                             corridor here is not threadable at the character's own collision radius: this is \
                             not a slide/collision wedge, and nudging will not fix it. Approach the goal from \
                             another direction.",
                            gs.player_x, gs.player_y, self.nav_repaths));
                    } else {
                        self.stop_nav(gs, "blocked", "walker_stalled", &format!(
                            "Wedged at ({:.1},{:.1}) after {} re-path attempts — the route is planned, the fine \
                             planner can thread it, but the walker cannot physically follow it. (The goal itself \
                             IS reachable; this is a collision/steering wedge, not a routing failure.)",
                            gs.player_x, gs.player_y, self.nav_repaths));
                    }
                    return;
                }
            }
        }

        // Planner (design §3.5): the walker no longer slides or writes positions. It emits a
        // MoveIntent toward the current waypoint; the render-thread CharacterController owns
        // collide-and-slide, step-up, gravity and the authoritative position.
        let heading = eq_heading(dx, dy);
        gs.player_heading = heading;
        let swim = self.collision.read().unwrap().as_ref().is_some_and(|c| {
            c.in_water([gs.player_x, gs.player_y, gs.player_z])
                || c.in_water([gs.player_x, gs.player_y, gs.player_z + 3.0])
        });
        let jump = match (self.path.get(self.path_i), self.path.get(self.path_i + 1)) {
            (Some(a), Some(b)) if self.path_i >= 1 => {
                let seg = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
                let to_takeoff = ((gs.player_x - a[0]).powi(2) + (gs.player_y - a[1]).powi(2)).sqrt();
                seg > JUMP_SEG_MIN && to_takeoff < JUMP_TAKEOFF_DIST
            }
            _ => false,
        };
        // Vertical swim wish — the water-nav Slice 3 depth controller (design §8.2), replacing the
        // old up-only rule that could not express a mid-water hold. `swim_vspeed` drives the wish from
        // the carrot's DEPTH so the swimmer follows the planned route z (dive, hold, tunnel transit)
        // instead of floating to the surface (#547 live qcat: descended, then surfaced/wedged). It
        // returns 0 ONLY when the carrot is at/above the swim plane, letting the controller's buoyancy
        // do the lift — which preserves the #359 haul-out approach (the last water waypoint before an
        // exit IS the swim-plane node, so the carrot rises there and buoyancy mounts the lip). Below
        // the plane the wish is always nonzero, which suppresses buoyancy so the hold is not a fight.
        let swim_plane = if swim {
            self.collision.read().unwrap().as_ref()
                .and_then(|c| c.water_surface([gs.player_x, gs.player_y, gs.player_z]))
                .map(|surf| surf - crate::traversability::PLAYER_BODY.float_depth)
        } else {
            None
        };
        let wish_vspeed = if swim { swim_vspeed(target.2, gs.player_z, swim_plane) } else { 0.0 };
        *self.nav_intent.lock().unwrap() = Some(MoveIntent {
            wish_dir:    [dx / dist, dy / dist],
            wish_vspeed,
            jump,
            want_swim:   swim,
            speed:       nav_speed(gs),
            climb:       0.0, // nav uses the native step-up now (#239); fences handled by hop
            hop:         self.stuck_ticks >= NAV_HOP_TICKS,
        });
        // Publish this tick's snapshot: the committed routes the walker is ACTUALLY following and
        // the swim state it just acted on — the same `swim`/`swim_plane` that went into the intent
        // above, not a recompute (#608).
        self.publish_debug(
            Self::known_pos(gs),
            Some(crate::diagnostics::WaterDebug { swimming: swim, swim_plane }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The canonical zone the walker fixtures live in. Navigating tests set
    /// `gs.world.zone_name = TEST_ZONE` so the #600 identity gate (`usability`) passes.
    const TEST_ZONE: &str = "testfixture";

    /// Build a `zone_assets` handle CONSISTENT with the collision handed in — the same coupling
    /// production keeps (`finish_zone_load` writes both from one verdict): a present grid ⇒
    /// `Ready(TEST_ZONE)` carrying that very grid; no grid ⇒ `Pending(TEST_ZONE)` (assets still
    /// loading — the #579 window). So `usability` sees a real state, not a fabricated one.
    fn zone_assets_for(collision: &crate::collision::SharedCollision)
        -> crate::zone_assets::ZoneAssetStateShared {
        let st = match collision.read().unwrap().as_ref() {
            Some(c) => crate::zone_assets::ZoneAssetState::ready(TEST_ZONE, 1, c.clone()),
            None    => crate::zone_assets::ZoneAssetState::pending(TEST_ZONE, "loading…"),
        };
        Arc::new(std::sync::Mutex::new(st))
    }

    /// The route facts `apply_plan`'s `Route` arm installs for a COMPLETE, untight, unsnapped route
    /// (#851). Fixtures that used to say "this is a complete route" by publishing the string
    /// `navigating` say it here instead — the string is derived from this now, not the reverse.
    fn committed_complete() -> CommittedFacts {
        CommittedFacts { route: crate::steering::CommittedRoute::Complete,
                         tier: Some("preferred"), reason: None }
    }

    /// The route facts `apply_plan`'s `Exhausted { progress: Some(_) }` arm installs (#851).
    fn committed_partial() -> CommittedFacts {
        CommittedFacts { route: crate::steering::CommittedRoute::Partial,
                         tier: None, reason: Some("search_node_cap") }
    }

    /// Every `set_nav_state…` call in `src` whose argument list contains a driving-word LITERAL,
    /// as `(byte offset, the call text scanned)`. Split out from the test below so the test can run
    /// it against synthetic sources whose answer is known — a matcher that always returns `vec![]`
    /// is the obvious way to fake this guard, and the positive controls are what stop it.
    ///
    /// Deliberately NOT line-based: it walks from the call's `(` to the matching `)` (depth-counted,
    /// bounded), so an argument split across lines is still one scanned unit.
    fn nav_state_calls_writing_a_driving_word_literal(src: &str) -> Vec<(usize, String)> {
        let mut hits = Vec::new();
        for (at, _) in src.match_indices("set_nav_state") {
            let Some(open) = src[at..].find('(') else { continue };
            let mut depth = 0i32;
            let mut end = at + open;
            for (i, c) in src[at + open..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => { depth -= 1; if depth == 0 { end = at + open + i; break; } }
                    _ => {}
                }
                if i > 4096 { break; }
            }
            let call = &src[at..=end.min(src.len() - 1)];
            if call.contains("\"navigating") { hits.push((at, call.to_string())); }
        }
        hits
    }

    /// **#851 — the three driving words are written in exactly ONE place, and that place is the
    /// verdict.** The type work upstream (a [`crate::steering::RouteExecution`] that is `Advancing`
    /// or `Stalled` and a total [`crate::steering::driving_nav_state`] mapping it to a word) makes
    /// it impossible to compute `navigating` from a stalled verdict. It does not, on its own, stop
    /// somebody writing the string past the verdict entirely — which is exactly how the bug being
    /// fixed here was written in the first place: `apply_plan` published the literal `"navigating"`
    /// and nothing downstream ever revised it.
    ///
    /// So: no `set_nav_state`/`set_nav_state_because` call in this file's PRODUCTION region may name
    /// a driving word literally. The only producer is `driving_nav_state`, reached through
    /// `publish_drive_state`.
    ///
    /// **What this is and is not.** It is a lexical scan, and this repo has eight measured cases of
    /// source text written but never reached (#799), so the direction of its unsoundness matters
    /// more than its strength. Comments and unrelated string literals can only ADD hits, never hide
    /// one — a driving word commented out, or quoted in prose next to a `set_nav_state`, makes this
    /// test FAIL loudly rather than pass falsely. That is why it does not strip comments and does
    /// not claim to.
    ///
    /// **What it genuinely cannot see, stated so nobody reads it as more:** (a) an INDIRECT write —
    /// `let w = "navigating"; self.set_nav_state(w);` is invisible to it; (b) anything outside
    /// `walker.rs` — `NavStatus` is behind a shared `Arc` and `eqoxide-http`, `eqoxide-net` and
    /// `eqoxide-renderer` can all lock and write `state` (they name the field; being unable to name
    /// a *type* is not being unable to reach a *state*). At the time of writing, every such write in
    /// those crates is in `#[cfg(test)]` code — checked by grep, not by this test, and nothing keeps
    /// it that way. The backstop for both gaps is the `debug_assert!` in `drive_walk`, which re-reads
    /// the PUBLISHED row each tick and fires if a stalled verdict ever coexists with an unqualified
    /// word, whatever wrote it — **a TEST-TIME instrument, not a runtime one** (#851 review round 1,
    /// N1). `debug_assert!` compiles out under `--release` and this workspace has no
    /// `[profile.release]` overriding `debug-assertions = false`; the reviewer confirmed by string-
    /// searching the shipped binary (ABSENT) against a debug test binary (PRESENT). So outside tests
    /// these two gaps have no enforcement at all — not weaker enforcement — and this convention is
    /// what stands between them and a regression. The same reading `NavStatus::retire_to_idle`'s doc
    /// and `a_reasonless_idle_is_refused_by_the_writer_not_just_by_a_per_call_site_test_725` already
    /// record for the other `debug_assert!`s on this row.
    #[test]
    fn the_driving_nav_state_word_is_only_ever_written_through_the_verdict_851() {
        const SRC: &str = include_str!("walker.rs");
        // Slice off the test module: fixtures below legitimately publish these words to build a
        // state to test against. `#[cfg(test)]` occurs once in this file — asserted, because a
        // second one would silently move the boundary and shrink the scanned region.
        let marks: Vec<usize> = SRC.match_indices("\n#[cfg(test)]").map(|(i, _)| i).collect();
        assert_eq!(marks.len(), 1,
            "expected exactly one top-level `#[cfg(test)]` in walker.rs, found {}; the production \
             region this guard scans is defined by it", marks.len());
        let production = &SRC[..marks[0]];

        // REACH CONTROLS — a corpus that shrank, or a slice that missed, must fail rather than pass
        // over nothing. #778's scanner silently covered ~12% of its corpus with every probe inside
        // the visible window.
        let call_sites = production.matches("set_nav_state").count();
        assert!(call_sites >= 8,
            "reach control: only {call_sites} `set_nav_state` mentions in the {} bytes of scanned \
             production region — this guard is scanning the wrong slice of the file",
            production.len());
        assert!(production.contains("fn publish_drive_state"),
            "reach control: the scanned region does not contain `publish_drive_state`, so it is not \
             the region that publishes the driving word");
        // NON-DEGENERACY — the one legitimate producer really is in the scanned region. Without
        // this, deleting `publish_drive_state`'s body would leave this test green.
        assert!(production.contains("driving_nav_state(facts.route, self.exec)"),
            "the verdict→word call is gone from the scanned region: the words are now produced \
             somewhere this guard is not looking");

        // POSITIVE CONTROLS on the matcher itself, including a multi-line call and a nested-paren
        // one, so `vec![]` and a line-based matcher are both excluded.
        for probe in [
            "self.set_nav_state(\"navigating\");",
            "self.set_nav_state_because(\n    \"navigating_stalled\",\n    Some(\"x\"),\n);",
            "self.set_nav_state_because(if a { \"navigating_partial\" } else { \"idle\" }, None);",
        ] {
            assert_eq!(nav_state_calls_writing_a_driving_word_literal(probe).len(), 1,
                "positive control: the matcher failed to see a driving-word write in {probe:?}");
        }
        // NEGATIVE CONTROL — it must not fire on the non-driving words, or the check below is just
        // "no `set_nav_state` calls exist".
        assert!(nav_state_calls_writing_a_driving_word_literal(
            "self.set_nav_state(\"arrived\"); self.set_nav_state_because(\"blocked\", None);").is_empty(),
            "negative control: the matcher fires on words that are not driving words");

        let hits = nav_state_calls_writing_a_driving_word_literal(production);
        assert!(hits.is_empty(),
            "#851: {} production call(s) in walker.rs write a driving word as a literal instead of \
             letting `driving_nav_state` decide it. A literal cannot know whether the walker is \
             still executing its route, which is the whole bug:\n{}",
            hits.len(),
            hits.iter().map(|(at, c)| format!("  @{at}: {c}")).collect::<Vec<_>>().join("\n"));
    }

    fn walker_with(collision: crate::collision::SharedCollision)
        -> (Walker, eqoxide_ipc::NavSlots, eqoxide_ipc::NavIntent, crate::diagnostics::NavDebugView)
    {
        let nav: eqoxide_ipc::NavSlots = Default::default();
        let world: eqoxide_ipc::WorldSlots = Default::default();
        let intent: eqoxide_ipc::NavIntent = Default::default();
        let view: crate::diagnostics::NavDebugView = Default::default();
        let za = zone_assets_for(&collision);
        let w = Walker::new(nav.clone(), world, collision, intent.clone(), view.clone(), za); // #787-NOT-PRODUCTION
        (w, nav, intent, view)
    }

    /// As [`walker_with`], but the caller supplies the SHARED `collision` + `zone_assets` handles so a
    /// test can mutate them mid-run through `begin_zone_load`/`finish_zone_load` (the true two-writer
    /// coupling). Returns the walker plus the nav/intent handles.
    fn walker_with_shared(
        collision: crate::collision::SharedCollision,
        zone_assets: crate::zone_assets::ZoneAssetStateShared,
    ) -> (Walker, eqoxide_ipc::NavSlots, eqoxide_ipc::NavIntent, crate::diagnostics::NavDebugView) {
        let nav: eqoxide_ipc::NavSlots = Default::default();
        let world: eqoxide_ipc::WorldSlots = Default::default();
        let intent: eqoxide_ipc::NavIntent = Default::default();
        let view: crate::diagnostics::NavDebugView = Default::default();
        let w = Walker::new(nav.clone(), world, collision, intent.clone(), view.clone(), zone_assets); // #787-NOT-PRODUCTION
        (w, nav, intent, view)
    }

    /// **#673 wiring guard.** The stale-cursor resync must be part of the walker's cursor advance,
    /// not just a library function nobody calls. Fixture: the coarse route the live client committed
    /// on a FAILING South Qeynos → qcat run (waypoints 44..52 of it), and the position the character
    /// physically reaches after dropping off the street into the aqueduct trench. The monotone
    /// advance alone leaves `path_i` three segments behind — where the fine planner's goal collapses
    /// onto the character and the steering loop has no trajectory that leaves the spot (see
    /// [`Walker::advance_cursor`]'s root-cause note for the four-step chain, and for why "deadlocks
    /// at `walker_stalled`" — this comment's wording through round 3 — overstated what the offline
    /// sim measures).
    #[test]
    fn a_cursor_the_character_has_overtaken_is_resynced_by_the_walkers_advance() {
        let (mut w, _nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        w.path = vec![
            [-542.718_75, 160.375, -0.000_007_629_394_5],
            [-534.718_75, 160.375, -0.000_007_629_394_5],
            [-526.718_75, 160.375, -0.000_007_629_394_5],
            [-518.718_75, 152.375, -2.226_699_8],
            [-526.718_75, 144.375, -4.161_232],
            [-534.718_75, 144.375, -6.095_749],
            [-542.718_75, 144.375, -8.030_266],
            [-550.718_75, 144.375, -9.964_805_6],
            [-558.718_75, 144.375, -11.899_315],
        ];
        w.path_i = 2;
        w.advance_cursor([-534.285_6, 144.375, -5.991_005]);
        assert!(w.path_i >= 4,
            "the walker must resync a cursor the character has physically overtaken; path_i = {}",
            w.path_i);
    }

    /// The ordinary case still advances exactly one segment at a time, and only when the character
    /// has actually passed the current waypoint — the resync must not turn the cursor into a
    /// nearest-segment snap.
    #[test]
    fn the_cursor_still_advances_monotonically_along_a_route_being_walked() {
        let (mut w, _nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        w.path = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [20.0, 0.0, 0.0], [30.0, 0.0, 0.0]];
        w.path_i = 0;
        w.advance_cursor([5.0, 0.0, 0.0]);
        assert_eq!(w.path_i, 0, "mid-segment must not advance");
        w.advance_cursor([12.0, 0.0, 0.0]);
        assert_eq!(w.path_i, 1, "past the waypoint advances exactly one");
    }

    // ─────────────── #727 round 2: the resync under REAL geometry, and its honesty ───────────────

    /// A world with two ledges at z = 0 split by a `gap`-wide chasm whose next floor is 200 u down,
    /// crossed only by a bridge at the far north (n ∈ [90, 100]). Ledges span n ∈ [-100, 100], the
    /// west one e ∈ [-60, -gap/2], the east one e ∈ [gap/2, 60].
    ///
    /// This is the round-1 reviewer's counterexample to guard 2, rebuilt with the production
    /// `Collision` so the predicate under test is the production one.
    fn chasm_zone(gap: f32) -> crate::collision::SharedCollision {
        // `Collision::build` maps a mesh vertex [x, y, z] to world [east, north, height] = [z, x, y],
        // so a world slab is written [north, height, east] — wound like `open_plane`'s quad.
        let slab = |e0: f32, e1: f32, n0: f32, n1: f32, h: f32| {
            quad(vec![[n0, h, e0], [n1, h, e0], [n1, h, e1], [n0, h, e1]])
        };
        let half = gap / 2.0;
        let terrain = vec![
            slab(-60.0, -half, -100.0, 100.0, 0.0),   // west ledge
            slab(half, 60.0, -100.0, 100.0, 0.0),     // east ledge
            slab(-half, half, 90.0, 100.0, 0.0),      // the only crossing
            slab(-half, half, -100.0, 90.0, -200.0),  // the chasm floor, 200 u down
        ];
        let col = crate::collision::Collision::build(
            &eqoxide_assets::ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0);
        Arc::new(std::sync::RwLock::new(Some(Arc::new(col))))
    }

    /// A complete route that runs north up the west ledge, over the far-north bridge, and back south
    /// down the east ledge. Cursor 2 names the segment `[-40, 0] → [-40, 60]`.
    const CHASM_ROUTE: [[f32; 3]; 9] = [
        [-40.0, -80.0, 0.0], [-40.0, -40.0, 0.0], [-40.0, 0.0, 0.0], [-40.0, 60.0, 0.0],
        [-40.0, 95.0, 0.0], [10.0, 95.0, 0.0], [10.0, 60.0, 0.0], [10.0, 0.0, 0.0],
        [10.0, -80.0, 0.0],
    ];
    /// On the west lip: 34 u off its own segment (stale) but only 16 u from the east-ledge segment
    /// on the other side of the chasm — which a chest-height ray sees clean through.
    const CHASM_BODY: [f32; 3] = [-6.0, 0.0, 0.0];

    /// **THE ROUND-1 COUNTEREXAMPLE, PINNED: a hole is not a wall (#727).** `carrot_los_clear` is
    /// documented in its own rustdoc as a chest-height centre ray chosen to ride ABOVE ground
    /// undulation and to catch WALLS. Asked "has the character reached that segment" it flies
    /// straight over a 200 u drop, and on this fixture it moved the cursor **2 → 6** — three
    /// waypoints and the whole bridge detour, declared walked, over a chasm the character cannot
    /// cross.
    ///
    /// The premise assert below keeps this from passing vacuously: it re-runs the LOS-only predicate
    /// and requires that it STILL jumps, so if the fixture ever stops reproducing the counterexample
    /// the test says so instead of going quietly green.
    ///
    /// Mutation check (run at authoring time): drop `ground_continuous` from the walker's
    /// predicate and this goes RED.
    #[test]
    fn a_resync_must_not_cross_a_chasm_the_character_cannot_walk() {
        let col = chasm_zone(10.0);
        let path: Vec<[f32; 3]> = CHASM_ROUTE.to_vec();
        // PREMISE: the LOS ray ALONE still declares the far side reachable — otherwise this fixture
        // is no longer testing anything.
        let los_only = crate::steering::resync_cursor(&path, 2, CHASM_BODY, |a, b| {
            col.read().unwrap().as_ref().unwrap().carrot_los_clear(a, b, STEER_LOS_CLEARANCE)
        });
        assert!(los_only > 2,
            "fixture no longer reproduces the round-1 counterexample (LOS-only cursor stayed at {los_only})");

        let (mut w, _nav, _intent, _view) = walker_with(col);
        w.path = path;
        w.path_i = 2;
        w.advance_cursor(CHASM_BODY);
        assert_eq!(w.path_i, 2,
            "the cursor crossed a 10 u chasm with the next floor 200 u down and declared the bridge \
             detour walked — a hole is not a wall; path_i = {}", w.path_i);
    }

    /// The same guard where the LOS ray genuinely is the load-bearing half: a WALL, not a hole. The
    /// ground under the hop is continuous (both ledges are one slab), so only `carrot_los_clear`
    /// can refuse it — and it must, at the real `STEER_LOS_CLEARANCE`.
    ///
    /// **This is the test the round-1 review found missing:** every other walker test runs
    /// `collision = None`, which `carrot_los_clear` documents as vacuously "clear", so the
    /// `STEER_LOS_CLEARANCE → 0.0` mutant survived. Here the wall sits just PAST the candidate
    /// point, inside the clearance the ray is extended by, so zeroing the clearance turns this RED.
    #[test]
    fn a_resync_must_not_cross_a_wall_and_it_uses_the_real_clearance() {
        let slab = |e0: f32, e1: f32, n0: f32, n1: f32, h: f32| {
            quad(vec![[n0, h, e0], [n1, h, e0], [n1, h, e1], [n0, h, e1]])
        };
        // A wall is a vertical quad: constant east, spanning north and height.
        let wall = |e: f32, n0: f32, n1: f32, h0: f32, h1: f32| {
            quad(vec![[n0, h0, e], [n1, h0, e], [n1, h1, e], [n0, h1, e]])
        };
        let terrain = vec![
            slab(-60.0, 60.0, -100.0, 100.0, 0.0),      // one continuous floor — no hole anywhere
            wall(0.5, -100.0, 90.0, -1.0, 20.0),        // a wall just PAST the candidate segment
        ];
        let col = Arc::new(std::sync::RwLock::new(Some(Arc::new(
            crate::collision::Collision::build(
                &eqoxide_assets::ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0)))));
        // The east-side route line sits at e = 0.0, i.e. `STEER_LOS_CLEARANCE` (1.0) PAST the wall:
        // the ray reaches the candidate cleanly and only its clearance extension crosses the wall.
        let path: Vec<[f32; 3]> = vec![
            [-40.0, -80.0, 0.0], [-40.0, -40.0, 0.0], [-40.0, 0.0, 0.0], [-40.0, 60.0, 0.0],
            [-40.0, 95.0, 0.0], [0.0, 95.0, 0.0], [0.0, 60.0, 0.0], [0.0, 0.0, 0.0],
            [0.0, -80.0, 0.0],
        ];
        let body = [-12.0, 0.0, 0.0]; // 28 u off segment 2, 12 u from segment 7 — through the wall
        // PREMISE: the ground under the hop really is continuous, so the LOS ray is the only guard
        // that can refuse this — otherwise the assert below would pass for the wrong reason.
        assert!(col.read().unwrap().as_ref().unwrap().ground_continuous(body, [0.0, 0.0, 0.0]),
            "fixture broken: the floor under the hop must be continuous so only the wall can refuse it");

        let (mut w, _nav, _intent, _view) = walker_with(col);
        w.path = path;
        w.path_i = 2;
        w.advance_cursor(body);
        assert_eq!(w.path_i, 2,
            "the cursor jumped through a wall; path_i = {}", w.path_i);
    }

    // ───────────── #734: the line-sampled gap, and the width gap that was not one ─────────────

    /// A single flat slab whose NORTH edge sits `margin` from the hop line (n = 0) the resync
    /// tests: east ∈ [-60, 60], north ∈ [-100, `margin`], all at h = 0. The body's centre column is
    /// over floor the whole way; a probe offset `+PLAYER_RADIUS` north is over void whenever
    /// `margin < 1.0`. This is the ordinary "route runs along a ledge lip" shape, and the
    /// controller walks it — its floor clamp only ever asks about the centre.
    fn lip_zone(margin: f32) -> crate::collision::SharedCollision {
        let slab = |e0: f32, e1: f32, n0: f32, n1: f32, h: f32| {
            quad(vec![[n0, h, e0], [n1, h, e0], [n1, h, e1], [n0, h, e1]])
        };
        let terrain = vec![slab(-60.0, 60.0, -100.0, margin, 0.0)];
        let col = crate::collision::Collision::build(
            &eqoxide_assets::ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0);
        Arc::new(std::sync::RwLock::new(Some(Arc::new(col))))
    }

    /// Twin of `chasm_zone`, but the "hole" is FILLED with a floor STRIP narrower than the body
    /// instead of left open. Same two ledges, same `gap`-wide split, but the crossing at n = 0 —
    /// where the #727 counterexample's direct hop actually runs — is a `ridge_width`-wide ridge
    /// instead of nothing. `carrot_los_clear` (no walls anywhere in this fixture) and the
    /// centre-line `ground_continuous` production runs (floor sits under n = 0 the whole way across,
    /// flat) both read this as clear — and so does the controller's own floor clamp, which is why
    /// that agreement is the thing being guarded rather than the thing being fixed.
    fn ridge_zone(gap: f32, ridge_width: f32) -> crate::collision::SharedCollision {
        let slab = |e0: f32, e1: f32, n0: f32, n1: f32, h: f32| {
            quad(vec![[n0, h, e0], [n1, h, e0], [n1, h, e1], [n0, h, e1]])
        };
        let half = gap / 2.0;
        let rh = ridge_width / 2.0;
        let terrain = vec![
            slab(-60.0, -half, -100.0, 100.0, 0.0), // west ledge
            slab(half, 60.0, -100.0, 100.0, 0.0),   // east ledge
            slab(-half, half, -rh, rh, 0.0),        // the ridge: the ONLY crossing, at n = 0
        ];
        let col = crate::collision::Collision::build(
            &eqoxide_assets::ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0);
        Arc::new(std::sync::RwLock::new(Some(Arc::new(col))))
    }

    /// **#734 gap 2 is not a defect, and this is the guard that keeps the "fix" out (#887).**
    ///
    /// The resync's floor probe samples the body's CENTRE line only, so it cannot tell a
    /// body-width crossing from a knife-edge ridge. #734 called that "width-blind" and #887 round 1
    /// implemented the obvious fix — `ground_continuous` on three lines at `-r / 0 / +r`
    /// perpendicular to travel. Measured, that fix refuses hops the **controller** walks:
    /// `CharacterController`'s floor clamp is a single column under the body centre — in
    /// `src/movement.rs`, `ground_below(self.pos[0], self.pos[1], foot + GROUND_ORIGIN, GROUND_DEPTH)`
    /// with no `±radius` term in either arm — so a shoulder over void is supported and walks
    /// normally. A predicate stricter than the model it exists to approximate produces FALSE
    /// REFUSALS — reporting an ordinary reachable state as unreachable — which is a wrong answer in
    /// the same sense a false acceptance is, and it withholds the #673 resync exactly on ledge-lip
    /// and trench geometry, where #673 was observed live.
    ///
    /// Each arm below is a shape where the controller's own floor probe is satisfied at every
    /// sample along the hop. Each is asserted twice: **premise** — the controller really can stand
    /// there, so a failure of the second assert is over-tightening and not a broken fixture — and
    /// **claim** — the production resync still crosses it.
    ///
    /// Mutation check, both directions (#887 round 2). Reinstating the three-line sweep in
    /// `steering::resync_reachable`'s floor half turns this test RED — `255 passed; 1 failed`,
    /// exactly this test and nothing else in the 272 — and removing it again turns it GREEN
    /// (`256 passed; 0 failed`). Note what the mutant run does **not** show: an `assert!` aborts the
    /// loop, so only the FIRST arm (the 0.5 u lip) was observed failing. That the two ridge arms are
    /// refused by the same mutant is measured separately, in the readout table on
    /// `steering::resync_reachable`'s rustdoc, not by this run. The refusal band there is exactly
    /// "lip margin below one `STEER_LOS_CLEARANCE`" and "ridge narrower than
    /// `2 * STEER_LOS_CLEARANCE`", which is why 1.9 u is an arm here and 2.1 u is not.
    #[test]
    fn a_resync_must_still_cross_ground_the_controller_can_stand_on() {
        // The controller's own ground-probe window, `src/movement.rs`:35 and :37. Restated because
        // it lives in the app crate and this one sits below it. Nothing enforces agreement between
        // this copy and the app crate's constants — this test compares only against the fixture.
        const GROUND_ORIGIN: f32 = 1.0;
        const GROUND_DEPTH: f32 = 200.0;

        // `CharacterController`'s floor clamp, sampled every 0.5 u along the hop the resync tests
        // (`CHASM_BODY` → `[10, 0, 0]`, 16 u of run at n = 0).
        let controller_stands_the_whole_way = |sh: &crate::collision::SharedCollision| -> bool {
            let guard = sh.read().unwrap();
            let c = guard.as_ref().unwrap();
            (0..=32).all(|i| {
                let e = CHASM_BODY[0] + i as f32 * 0.5;
                c.ground_below(e, 0.0, CHASM_BODY[2] + GROUND_ORIGIN, GROUND_DEPTH).is_some()
            })
        };
        let cursor_after_resync = |sh: crate::collision::SharedCollision| -> usize {
            let (mut w, _nav, _intent, _view) = walker_with(sh);
            w.path = CHASM_ROUTE.to_vec();
            w.path_i = 2;
            w.advance_cursor(CHASM_BODY);
            w.path_i
        };

        for (what, sh) in [
            ("a ledge whose lip is 0.5 u north of the hop line", lip_zone(0.5)),
            ("a 0.8 u ridge with void either side", ridge_zone(10.0, 0.8)),
            ("a 1.9 u ridge — just under the body's 2 u diameter", ridge_zone(10.0, 1.9)),
        ] {
            assert!(controller_stands_the_whole_way(&sh),
                "PREMISE broken for {what}: the controller's own centre-column floor probe must \
                 find standable ground at every 0.5 u sample along the hop, or this arm is not \
                 about over-tightening at all");
            let got = cursor_after_resync(sh);
            assert!(got > 2,
                "OVER-TIGHTENING (#887): the resync refused {what} — ground the controller's own \
                 floor clamp stands on at every sample along the hop. That is a FALSE REFUSAL. The \
                 floor half of `steering::resync_reachable` must stay a CENTRE-line probe: the \
                 controller has no shoulder term, so a ±STEER_LOS_CLEARANCE sweep models a body \
                 production does not have. See that function's rustdoc for the measurement, and \
                 `Collision::edge_clear`'s for what this direction cost the planner (876 → 813 \
                 routable pairs). path_i = {got}");
        }

        // CONTROL. This driver is not simply "accepts everything": a genuine void is still refused,
        // by the same predicate, on the same route, from the same cursor.
        assert_eq!(cursor_after_resync(chasm_zone(10.0)), 2,
            "control: a 10 u chasm with the next floor 200 u down must still be REFUSED, or the \
             three acceptances above prove nothing about over-tightening");
    }

    /// **⚠️ THIS TEST PINS A BUG. It asserts the resync DOES step over a hole it should not. If it
    /// goes RED, the bug is FIXED — DELETE this test, do not weaken the assertion.** The deletion
    /// list is in the failure message below and repeated here: this test, its entry in
    /// `every_walker_test_name_cited_in_a_doc_comment_still_exists`'s array at the bottom of this
    /// module, and the "#734 gap 1" bullet in `steering::resync_cursor`'s rustdoc plus the gap-1
    /// clause in [`Walker::advance_cursor`]'s.
    ///
    /// **#734 gap 1, measured: a hole narrower than `Collision::ground_continuous`'s probe spacing,
    /// in the direction of TRAVEL, is invisible to it.**
    ///
    /// One floor, one 1.5 u hole (`< PROBE_SPACING` = 2.0 u) positioned in the middle of one probe
    /// interval on the `CHASM_BODY -> [10, 0, 0]` hop (`run` = 16 u; `PROBE_SPACING` = 2 u divides
    /// it evenly, so the probes land at whole-metre offsets e ∈ {-4,-2,0,2,4,6,8,10}). The hole sits
    /// at e ∈ [2.25, 3.75], strictly between the e = 2 and e = 4 probes and spanning the whole north
    /// extent this fixture models, so no probe ever lands inside it. `collision.rs`'s own
    /// `ground_continuous_probe_spacing_catches_every_hole_wider_than_the_spacing` is the
    /// complementary case — the same 2 u spacing, but a WIDER hole, on a different hop
    /// (`[-18,0,0] → [18,0,0]`) and sweeping the hole's start position rather than pinning one
    /// offset set; the shared property is the spacing, not the geometry.
    ///
    /// **No fix is attempted for this gap in this change.** Closing it means changing
    /// `PROBE_SPACING` or the sampling strategy inside `Collision::ground_continuous` itself, in
    /// `collision.rs` — see `steering::resync_cursor`'s rustdoc for why that file is out of scope
    /// here.
    ///
    /// **The delete-me path is measured, not asserted (#887 round 2).** `PROBE_SPACING` was
    /// temporarily set to 1.0 in `collision.rs` and the walker tests re-run: this test alone went
    /// RED (`43 passed; 1 failed` of the 44 walker tests) with the instruction above, and the
    /// mutation was hand-restored. So a future `collision.rs` change that shrinks the spacing turns
    /// `main` RED here, with **no git conflict** to warn anyone first — that is the whole reason
    /// the instruction is in the assertion message and not only in this doc.
    ///
    /// Sensitivity control, same shape: widening the hole to 2.5 u (still starting at e = 2.25, now
    /// spanning the e = 4 probe) is refused by the same predicate — so the acceptance above is not
    /// simply "this predicate accepts everything on this fixture", it is specifically the
    /// narrower-than-spacing case. **What that control does and does not establish:** it proves the
    /// fixture responds to hole width ALONG travel, i.e. to `PROBE_SPACING`, which is exactly what
    /// this test is about. It says nothing about any across-travel property, and no assertion here
    /// should be read as covering one.
    #[test]
    fn a_narrow_hole_between_probes_still_crosses_the_resync_undetected() {
        let make = |hole_width: f32| {
            let slab = |e0: f32, e1: f32| quad(vec![
                [-50.0, 0.0, e0], [50.0, 0.0, e0], [50.0, 0.0, e1], [-50.0, 0.0, e1],
            ]);
            let terrain = vec![slab(-60.0, 2.25), slab(2.25 + hole_width, 60.0)];
            let col = crate::collision::Collision::build(
                &eqoxide_assets::ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0);
            Arc::new(std::sync::RwLock::new(Some(Arc::new(col))))
        };

        // NARROW: the gap this test is about.
        let narrow_col: crate::collision::SharedCollision = make(1.5);
        // PREMISE: the hole is really there — a direct probe of its own column finds no floor.
        assert!(narrow_col.read().unwrap().as_ref().unwrap()
                .ground_below(3.0, 0.0, 10.0, 20.0).is_none(),
            "fixture broken: the hole at e = 3.0 must have no floor for this test to mean anything");
        let (mut w, _nav, _intent, _view) = walker_with(narrow_col);
        w.path = CHASM_ROUTE.to_vec();
        w.path_i = 2;
        w.advance_cursor(CHASM_BODY);
        assert!(w.path_i > 2,
            "THIS TEST PINS A BUG AND IT HAS JUST BEEN FIXED — DELETE THIS TEST, do not weaken \
             this assertion. It asserted (#734 gap 1) that the production resync predicate STILL \
             crosses a hole narrower than PROBE_SPACING because the hole falls between two probes; \
             path_i = {} (expected > 2 while the gap is live). Deleting it also requires removing \
             `a_narrow_hole_between_probes_still_crosses_the_resync_undetected` from the array in \
             `every_walker_test_name_cited_in_a_doc_comment_still_exists` at the bottom of this \
             module — that guard fails on a dangling citation, and it is a second, \
             unrelated-looking red if you miss it — and dropping the '#734 gap 1' bullet from \
             `steering::resync_cursor`'s rustdoc and the gap-1 clause from `advance_cursor`'s",
            w.path_i);

        // SENSITIVITY CONTROL: a wider hole, same start, spans a real probe and is refused.
        let wide_col: crate::collision::SharedCollision = make(2.5);
        let (mut w2, _nav2, _intent2, _view2) = walker_with(wide_col);
        w2.path = CHASM_ROUTE.to_vec();
        w2.path_i = 2;
        w2.advance_cursor(CHASM_BODY);
        assert_eq!(w2.path_i, 2,
            "control: a 2.5 u hole spans a probe and must be refused, or this test's acceptance \
             above proves nothing about hole WIDTH; path_i = {}", w2.path_i);
    }

    /// **A RESYNC IS NOT PROGRESS (#727, agent honesty).** #631 channel (a) justifies calling an
    /// advancing complete route "progress *by construction*" on the premise that `path_i` only moves
    /// by WALKING. A resync moves it without walking, so a resync must not reach that channel — or
    /// the no-progress killer's clock is reset by a step the character never took, which is a silent
    /// wrong answer in the machinery whose whole job is to answer this honestly.
    ///
    /// This drives the real `drive_walk` with the no-progress clock fully expired and closest
    /// approach not improving. The route is COMPLETE and `navigating`, so channel (a) is live; the
    /// only thing that moves `path_i` this tick is the resync. It must still terminate.
    ///
    /// Mutation check (run at authoring time): delete the `stuck_i` raise in `advance_cursor` and
    /// this goes RED — the walker keeps navigating on progress it did not make.
    #[test]
    fn a_resync_jump_must_not_reset_the_no_progress_clock() {
        let (mut w, nav, _intent, _view) = walker_with(open_plane(600.0));
        let mut gs = eqoxide_core::game_state::GameState::new();
        gs.world.zone_name = TEST_ZONE.into();
        // 30 u off its own segment (stale) and 10 u from the next one — a flat open plane, so the
        // reachability predicate accepts and the resync WILL fire.
        gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0; gs.player_pos_known = true;
        let goal = (500.0, 0.0, 0.0);
        *nav.goto_target.lock().unwrap() = Some(goal);
        // #851: "a COMPLETE route" is `committed`, not the published word. The `set_nav_state` line
        // stays because the walker's own publication is what the assertions read.
        w.committed = Some(committed_complete());
        w.set_nav_state("navigating");
        w.path = vec![
            [0.0, -30.0, 0.0], [40.0, -30.0, 0.0], [80.0, -30.0, 0.0],
            [80.0, -10.0, 0.0], [0.0, -10.0, 0.0], [500.0, 0.0, 0.0],
        ];
        w.path_i = 0;
        w.stuck_i = 0;
        w.path_goal = Some(goal);
        w.nav_best_g3d = 10.0; // closest approach has not improved…
        w.nav_progress_at = std::time::Instant::now() - (NAV_NO_PROGRESS_WINDOW + std::time::Duration::from_secs(1));

        w.drive_walk(&mut gs, goal);

        // PREMISE: the resync really did move the cursor this tick — otherwise nothing is tested.
        assert!(w.path_i > 0, "fixture no longer resyncs; path_i = {}", w.path_i);
        assert!(w.nav_state_is("blocked"),
            "a resync jump reset the no-progress clock: the walker reported progress it did not \
             make (path_i = {}, stuck_i = {})", w.path_i, w.stuck_i);
        assert_eq!(nav.nav_state.lock().unwrap().reason.as_deref(), Some("no_progress"));
    }

    /// The other half of the same rule: a resync must not reset the STALL detector's clock either.
    /// `stuck_ticks` is reset by `path_i > stuck_i`, so a resync that raised `path_i` without
    /// raising `stuck_i` would hand the stall detector a fresh clock every time the body drifted.
    #[test]
    fn a_resync_jump_leaves_the_stall_detectors_high_water_mark_at_the_new_cursor() {
        let (mut w, _nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        w.path = CHASM_ROUTE.to_vec();
        w.path_i = 2;
        w.stuck_i = 2;
        w.advance_cursor(CHASM_BODY); // no collision ⇒ predicate vacuously true ⇒ the resync fires
        assert!(w.path_i > 2, "fixture no longer resyncs; path_i = {}", w.path_i);
        assert_eq!(w.stuck_i, w.path_i,
            "the stall detector's high-water mark must move WITH a resync jump, so the jump reads \
             as zero progress (path_i = {}, stuck_i = {})", w.path_i, w.stuck_i);
    }

    /// **Every walker test name cited in a doc comment still resolves** (#727 round 5).
    ///
    /// Twin of `collision::tests::every_ground_continuous_test_name_cited_in_a_doc_comment_still_exists`
    /// and `steering::cursor_resync_tests::every_test_name_cited_in_a_doc_comment_still_exists`, and
    /// it exists for the same reason: round 5 found `resync_cursor`'s rustdoc citing a module that
    /// never existed and `CURSOR_STALE_DIST`'s citing a test renamed two rounds earlier. Rustdoc
    /// cannot intra-doc-link a `#[cfg(test)]` item, so a citation to one rots silently. Naming the
    /// cited tests as `fn()` values makes a rename a COMPILE error.
    ///
    /// Add a name here whenever a doc comment — in this module OR in another that cites `walker` by
    /// name, as `collision::ground_continuous` and `steering::resync_cursor` both do — starts
    /// citing a test that lives here.
    #[test]
    fn every_walker_test_name_cited_in_a_doc_comment_still_exists() {
        let _cited: &[fn()] = &[
            // cited by `Walker::advance_cursor`'s rustdoc
            a_resync_jump_must_not_reset_the_no_progress_clock,
            // cited by `collision::Collision::ground_continuous` and `steering::resync_cursor`
            a_resync_must_not_cross_a_chasm_the_character_cannot_walk,
            // added in round 6 by `steering`'s mechanical citation scan: cited in a doc comment in
            // this file and named in no guard list.
            cancelling_the_goto_while_loading_returns_to_idle,
            // #766 round 5: cited by `Walker::latch_local_planner_liveness`'s rustdoc, which points
            // at this test for the B10 hedge on the uncommitted draft. Caught by `steering`'s scan,
            // not by me — the citation was added and the guard was not.
            a_dead_fine_planner_stays_visible_after_the_goal_is_retired_766,
            // #787: cited by `NOT_PRODUCTION`'s rustdoc, which points at the guard that decides what
            // the marker means. Caught by `steering`'s scan when the citation was written.
            exactly_one_production_fine_worker_is_built_in_the_tree_787,
            // #734: cited by `Walker::advance_cursor`'s rustdoc and by `steering::resync_reachable`'s
            // and `steering::resync_cursor`'s rustdoc (cross-file, no guard entry required there —
            // see that scan's own rule 1 vs rule 2).
            a_resync_must_still_cross_ground_the_controller_can_stand_on,
            // ⚠️ This one pins a bug. When #734 gap 1 is closed the test goes RED and must be
            // DELETED — and this line deleted with it, or the deletion produces a second red here
            // that looks unrelated. Its own failure message says so too.
            a_narrow_hole_between_probes_still_crosses_the_resync_undetected,
            // This guard names ITSELF because that delete-me instruction cites it by name in a doc
            // comment, and this scan's rule is that a cited test in this file must be listed here —
            // including when the cited test is the guard. A rename is now a compile error, which is
            // the whole point: the instruction must not be able to point at a fn that moved.
            every_walker_test_name_cited_in_a_doc_comment_still_exists,
        ];
    }

    // ───────────────────────────── #543: the unverifiable-pad scene ─────────────────────────────

    const PAD_ZONE: u16 = 2;
    const PAD_INDEX: i32 = 42;
    /// What the server ADVERTISES this pad's same-zone arrival to be (a real floor point on slab B).
    const PAD_ADVERTISED_DEST: [f32; 3] = [430.0, 40.0, 0.0];

    /// `two_leaves` bakes the SAME DRNTP index as two horizontally-separated footprint boxes — the
    /// real shape a pad can have, and the case where naming only one leaf sends the agent to a
    /// footprint it may not be able to reach (#660 review NB2).
    fn pad_scene_leaves(two_leaves: bool) -> crate::collision::Collision {
        use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};
        let quad = |v: Vec<[f32; 3]>| MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // Slab A: east[-120,0] × north[0,80] @ z=0.  Slab B: east[400,480] × north[0,80] @ z=0.
        let slab_a = quad(vec![[0.0, 0.0, -120.0], [80.0, 0.0, -120.0], [80.0, 0.0, 0.0], [0.0, 0.0, 0.0]]);
        let slab_b = quad(vec![[0.0, 0.0, 400.0], [80.0, 0.0, 400.0], [80.0, 0.0, 480.0], [0.0, 0.0, 480.0]]);
        let mut col = crate::collision::Collision::build(
            &ZoneAssets { terrain: vec![slab_a, slab_b], objects: vec![], textures: vec![] }, 8.0);
        // Pad footprint: a DRNTP box on slab A straddling the z=0 floor, so a character standing on
        // it is inside the region and the crossing would fire.
        col.set_water(Some(std::sync::Arc::new(if two_leaves {
            eqoxide_core::region_map::RegionMap::zone_line_two_boxes(
                10.0, 25.0, 45.0, 60.0, -40.0, -16.0, -5.0, 5.0, PAD_INDEX)
        } else {
            eqoxide_core::region_map::RegionMap::zone_line_box(30.0, 50.0, -40.0, -16.0, -5.0, 5.0, PAD_INDEX)
        })));
        col
    }

    /// A walker standing on slab A, with the pad advertised as a SAME-ZONE teleport.
    fn pad_walker() -> (Walker, eqoxide_ipc::WorldSlots, eqoxide_core::game_state::GameState) {
        pad_walker_with(PAD_ADVERTISED_DEST, false)
    }

    /// As `pad_walker`, but the pad advertises `dest` (use a column with no floor to model a pad
    /// whose ADVERTISEMENT cannot be resolved) and optionally has two footprint leaves.
    fn pad_walker_with(dest: [f32; 3], two_leaves: bool)
        -> (Walker, eqoxide_ipc::WorldSlots, eqoxide_core::game_state::GameState) {
        let nav: eqoxide_ipc::NavSlots = Default::default();
        let world: eqoxide_ipc::WorldSlots = Default::default();
        let intent: eqoxide_ipc::NavIntent = Default::default();
        let view: crate::diagnostics::NavDebugView = Default::default();
        let col = Arc::new(std::sync::RwLock::new(Some(Arc::new(pad_scene_leaves(two_leaves)))));
        let za = zone_assets_for(&col); // pad tests don't drive `drive_walk`; a consistent handle regardless
        let w = Walker::new(nav, world.clone(), col, intent, view, za); // #787-NOT-PRODUCTION
        *world.zone_points.lock().unwrap() = vec![eqoxide_core::game_state::ZonePoint {
            iterator:  PAD_INDEX as u32,
            server_x:  dest[0], server_y: dest[1], server_z: dest[2],
            heading:   0.0,
            zone_id:   PAD_ZONE, // "same zone" — as ADVERTISED, which is all the client ever gets
        }];
        let mut gs = eqoxide_core::game_state::GameState::new();
        gs.world.zone_id = PAD_ZONE;
        gs.player_x = -112.0; gs.player_y = 40.0; gs.player_z = 0.0; // slab A, clear of the footprint
        (w, world, gs)
    }

    /// **#543, the honesty gate + the disclosure that must come with it.**
    ///
    /// An advertised same-zone pad is unverifiable: the server picks a crossing's destination from
    /// trigger coordinates the wire never carries, so `zone_id == current` does not mean "stays in
    /// this zone" (qeynos2 index=2 advertises same-zone and really lands in qcat). Nav must NOT
    /// auto-route the walker through one — that is the silent wrong-zone drift.
    ///
    /// But it must not go silent either: the pad IS there, and the owner's decision is that nav
    /// offers it back and the agent chooses. So the same call that refuses the edge must record the
    /// pad, its measured footprint, and the server's ADVERTISED destination, labelled as advertised.
    ///
    /// Mutation check: flip `TRUST_ADVERTISED_SAME_ZONE_CROSSINGS` to `true` → an edge is handed to
    /// A* and the knowledge state becomes `AdvertisedUsable` → both halves go RED.
    #[test]
    fn an_unverifiable_same_zone_pad_is_never_routed_through_but_is_always_disclosed_543() {
        let (mut w, _world, gs) = pad_walker();
        let c = w.collision.read().unwrap().clone().unwrap();

        // PRECONDITION — the mechanism genuinely would route through this pad. Without it the test
        // could pass on a scene where no pad exists at all, proving nothing.
        let resolved = c.resolve_teleport_pads(&[(PAD_INDEX, PAD_ADVERTISED_DEST)]);
        assert_eq!(resolved.len(), 1,
            "fixture: the advertised pad must resolve to exactly one usable edge, got {resolved:?}");

        let edges = w.same_zone_teleport_pads(&gs, &c);

        // 1. THE GATE: nothing reaches the planner, so a goal beyond the pad is an honest no_path.
        assert!(edges.is_empty(),
            "#543: nav must not hand A* an edge through a pad it cannot verify — that is the drift");

        // 2. THE DISCLOSURE: the pad is reported, with what the client actually knows.
        assert_eq!(w.last_pads.len(), 1, "the declined pad must still be reported, got {:?}", w.last_pads);
        let pad = &w.last_pads[0];
        assert_eq!(pad.index, PAD_INDEX);
        match pad.knowledge {
            crate::diagnostics::PadKnowledge::AdvertisedSameZoneDeclined {
                footprint, advertised_dest, advertised_dest_floor, ..
            } => {
                assert_eq!(footprint, Some(resolved[0].source),
                    "the footprint is measured geometry — the agent needs it to walk onto the pad");
                assert_eq!(advertised_dest, Some(PAD_ADVERTISED_DEST),
                    "the ADVERTISED destination must be the VERBATIM wire value — the client's floor \
                     snap of it is a DERIVATION and must not stand in for the server's claim");
                assert_eq!(advertised_dest_floor, Some(resolved[0].dest),
                    "…and the client's own snap is reported alongside it, as its own field");
            }
            ref other => panic!(
                "a policy-declined pad must be disclosed as AdvertisedSameZoneDeclined — not \
                 withheld, and not mislabelled as a geometry verdict or as usable. Got {other:?}"),
        }
    }

    /// **#660 review NB — nearest-first is not "different places".**
    ///
    /// Live, the eight nearest leaves of qeynos2's pad collapsed onto about three real spots,
    /// including a pair **0.0005u** apart, and five of six retry attempts landed in the same two
    /// places. Offering eight near-duplicates is one option wearing eight hats — the same
    /// over-claim as the `footprint` wording, in list form.
    ///
    /// Mutation: drop the separation filter (take the nearest N) → RED.
    #[test]
    fn offered_spots_are_spread_not_eight_names_for_one_place_543() {
        // Nearest-first, and deliberately degenerate: a near-exact duplicate pair, a cluster, and
        // two genuinely distant spots.
        let sorted = vec![
            [0.0, 0.0, 0.0],
            [0.0005, 0.0, 0.0],   // the observed duplicate
            [1.0, 1.0, 0.0],      // same place, really
            [40.0, 0.0, 0.0],     // a different place
            [40.2, 0.3, 0.0],     // …and its duplicate
            [90.0, 0.0, 0.0],     // another different place
        ];
        let got = spread_spots(sorted, OFFERED_SPOTS, SPOT_SEPARATION);
        assert_eq!(got, vec![[0.0, 0.0, 0.0], [40.0, 0.0, 0.0], [90.0, 0.0, 0.0]],
            "six leaves are three PLACES — offer the three, nearest first, not six names for three");
        for (i, a) in got.iter().enumerate() {
            for b in got.iter().skip(i + 1) {
                assert!((a[0] - b[0]).hypot(a[1] - b[1]).max((a[2] - b[2]).abs()) >= SPOT_SEPARATION,
                    "every offered spot must be somewhere else: {a:?} vs {b:?}");
            }
        }
        // The cap still binds, and the nearest is still first.
        let many: Vec<[f32; 3]> = (0..40).map(|i| [i as f32 * 20.0, 0.0, 0.0]).collect();
        let capped = spread_spots(many, OFFERED_SPOTS, SPOT_SEPARATION);
        assert_eq!(capped.len(), OFFERED_SPOTS, "an offer is bounded — the full leaf list is diagnostics");
        assert_eq!(capped[0], [0.0, 0.0, 0.0], "…and the nearest spot stays the one to try first");
    }

    /// **#660 review B1 — the disclosure had a hole in exactly the #266 pad class.**
    ///
    /// The first revision classified a pad by whether `resolve_teleport_pads` produced an EDGE, which
    /// requires the footprint AND the advertised destination to resolve. So a pad with a perfectly
    /// standable footprint whose ADVERTISED arrival has no floor collapsed into `AdvertisedUnusable`
    /// and was withheld entirely — a pad the agent can walk onto and take, hidden on the strength of
    /// the one datum this entire PR argues the client cannot trust. `find_in_zone_portal` (the #266
    /// door) never required a resolvable destination, so pads only that door could reach were newly
    /// refused AND undisclosed. Live: qeynos2 index 1 has a real DRNTP region and was silent.
    ///
    /// The question the DISCLOSURE asks is "can the agent take this pad?" — footprint only. The
    /// question the PLANNER asks is "may A* route through it?" — both ends. They are different
    /// questions and must not share an answer.
    ///
    /// Mutation check: classify from `resolved` instead of `teleport_pad_footprints` (i.e. restore
    /// the first revision) → the pad becomes `AdvertisedUnusable` and vanishes from the offer → RED.
    #[test]
    fn a_pad_whose_advertised_destination_does_not_resolve_is_still_disclosed_543() {
        // Advertise an arrival out over the 400u gap between the slabs: no floor anywhere in that
        // column, so the ADVERTISEMENT cannot be resolved — but the footprint is untouched.
        const VOID_DEST: [f32; 3] = [200.0, 40.0, 0.0];
        let (mut w, _world, gs) = pad_walker_with(VOID_DEST, false);
        let c = w.collision.read().unwrap().clone().unwrap();

        // PRECONDITIONS, both halves — this is exactly the case the two questions disagree about.
        assert!(c.resolve_teleport_pads(&[(PAD_INDEX, VOID_DEST)]).is_empty(),
            "fixture: the ADVERTISED destination must NOT resolve (that is the whole point)");
        assert_eq!(c.teleport_pad_footprints(PAD_INDEX).len(), 1,
            "fixture: …while the FOOTPRINT is standable, so the agent genuinely can take this pad");

        assert!(w.same_zone_teleport_pads(&gs, &c).is_empty(), "still no A* edge, of course");

        assert_eq!(w.last_pads.len(), 1, "got {:?}", w.last_pads);
        match w.last_pads[0].knowledge {
            crate::diagnostics::PadKnowledge::AdvertisedSameZoneDeclined {
                footprint, advertised_dest, advertised_dest_floor, ..
            } => {
                assert_eq!(footprint, Some(c.teleport_pad_footprints(PAD_INDEX)[0]),
                    "the agent is told WHERE to stand — the part the client actually measured");
                assert_eq!(advertised_dest, Some(VOID_DEST),
                    "the server's claim is still reported verbatim, unresolvable or not");
                assert_eq!(advertised_dest_floor, None,
                    "and the client says plainly that it found no floor there — never invents one");
            }
            ref other => panic!(
                "#660 B1: a pad the agent CAN take must be OFFERED. Withholding it because its \
                 ADVERTISED destination did not resolve decides the agent's options from the very \
                 datum this gate exists because the client cannot trust. Got {other:?}"),
        }
    }

    /// A pad whose region has **no standable point** (the #266 "floating leaf": the DRNTP box sits
    /// above the floor, so walking to its XY never fires the crossing) is STILL offered — with
    /// `footprint: None` and `footprint_count: 0`, plus where the region actually is. That is the
    /// honest shape: "this pad is here, and I could not find anywhere in it you can stand" is a
    /// warning the agent can act on; silence is not, and the client's standability probe is its own
    /// model, not ground truth. Only a pad ABSENT from the loaded map is silenced.
    ///
    /// Mutation: withhold a pad with no standable footprint (the previous revision's behaviour) → RED.
    #[test]
    fn a_pad_with_no_standable_footprint_is_still_offered_with_an_explicit_null_543() {
        use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};
        let (mut w, _world, gs) = pad_walker_with(PAD_ADVERTISED_DEST, false);
        // A DRNTP box FLOATING 100u above the floor: the region exists, nothing in it is standable.
        // (The zone needs real vertical extent for the region precompute to reach that height, so
        // this scene has a high roof quad as well as the ground slab.)
        let quad = |v: Vec<[f32; 3]>| MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let ground = quad(vec![[0.0, 0.0, -120.0], [80.0, 0.0, -120.0], [80.0, 0.0, 0.0], [0.0, 0.0, 0.0]]);
        let roof   = quad(vec![[0.0, 300.0, -120.0], [80.0, 300.0, -120.0], [80.0, 300.0, 0.0], [0.0, 300.0, 0.0]]);
        let mut col = crate::collision::Collision::build(
            &ZoneAssets { terrain: vec![ground, roof], objects: vec![], textures: vec![] }, 8.0);
        col.set_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_box(30.0, 50.0, -40.0, -16.0, 100.0, 120.0, PAD_INDEX))));
        let col = Arc::new(col);
        *w.collision.write().unwrap() = Some(col.clone());
        assert!(col.teleport_pad_footprints(PAD_INDEX).is_empty(), "fixture: nothing standable");
        assert!(col.find_zone_line_near(Some(PAD_INDEX), [0.0; 3]).is_some(),
            "fixture: …but the region is genuinely in the map");

        assert!(w.same_zone_teleport_pads(&gs, &col).is_empty());
        assert_eq!(w.last_pads.len(), 1);
        match w.last_pads[0].knowledge {
            crate::diagnostics::PadKnowledge::AdvertisedSameZoneDeclined {
                footprint, footprint_count, .. } => {
                assert_eq!(footprint, None,
                    "no standable point was found — say so explicitly, never invent one");
                assert_eq!(footprint_count, 0);
            }
            ref other => panic!(
                "a pad that IS in the map must still be disclosed, with the standability failure as \
                 a FACT rather than as a reason to go silent. Got {other:?}"),
        }
    }

    /// The one case that is genuinely silent: the server advertises an index this client's loaded map
    /// has no DRNTP region for (a `.wtr` data gap). There is nothing to point the agent at, so
    /// `advertised_unusable` — and it must NOT be dressed up as an offer with a fabricated position.
    #[test]
    fn a_pad_absent_from_the_loaded_map_is_not_offered_543() {
        let (mut w, world, gs) = pad_walker_with(PAD_ADVERTISED_DEST, false);
        let c = w.collision.read().unwrap().clone().unwrap();
        // Advertise an index the map has no region for.
        world.zone_points.lock().unwrap()[0].iterator = 4242;
        assert!(c.find_zone_line_near(Some(4242), [0.0; 3]).is_none(), "fixture: no such region");

        assert!(w.same_zone_teleport_pads(&gs, &c).is_empty());
        assert_eq!(w.last_pads[0].knowledge, crate::diagnostics::PadKnowledge::AdvertisedUnusable,
            "nothing in the map to walk to — do not manufacture an offer");
    }

    /// Multi-leaf pads (#660 review NB2). ONE offer per pad index — a real DRNTP index has dozens of
    /// BSP leaves and an offer each is noise — but the offer must name the leaf NEAREST the
    /// character (the actionable one) and say how many exist, so a failed goto does not read as
    /// "this pad is out of options".
    ///
    /// Mutation: report `footprints[0]` instead of the nearest, or hard-code `footprint_count: 1` → RED.
    #[test]
    fn a_multi_leaf_pad_offers_the_nearest_leaf_and_says_how_many_543() {
        let (mut w, _world, mut gs) = pad_walker_with(PAD_ADVERTISED_DEST, true);
        let c = w.collision.read().unwrap().clone().unwrap();
        let leaves = c.teleport_pad_footprints(PAD_INDEX);
        assert_eq!(leaves.len(), 2, "fixture: this scene must really have two standable leaves");

        // Stand next to each leaf in turn: the offer must FOLLOW the character, not name a fixed one.
        for want in [0usize, 1] {
            gs.player_x = leaves[want][0]; gs.player_y = leaves[want][1] - 6.0; gs.player_z = leaves[want][2];
            let _ = w.same_zone_teleport_pads(&gs, &c);
            assert_eq!(w.last_pads.len(), 1,
                "one offer per pad INDEX, not per leaf — 58 near-identical points is noise: {:?}", w.last_pads);
            match w.last_pads[0].knowledge {
                crate::diagnostics::PadKnowledge::AdvertisedSameZoneDeclined {
                    footprint, footprint_count, ref alternates, .. } => {
                    assert_eq!(footprint, Some(leaves[want]),
                        "the offer must name the leaf NEAREST the character — the one it can act on");
                    assert_eq!(footprint_count, 2,
                        "…and say that another exists, so a failed goto is not read as 'no options'");
                    // Verified live (#660): one leaf of a pad can fire nothing while another leaf of
                    // the SAME pad crosses. A count the agent cannot act on is not a disclosure, so
                    // the other spots must be handed over, not just tallied.
                    assert_eq!(alternates.as_slice(), &[leaves[1 - want]],
                        "the OTHER spot must be offered too, or `footprint_count` is unactionable");
                }
                ref other => panic!("expected an offer, got {other:?}"),
            }
        }
    }

    /// The OTHER door onto the same unverifiable line (#266): when a goal is unreachable, nav used
    /// to auto-escape the "sealed" area by walking into an advertised in-zone teleport. Same
    /// unverifiability, same drift — so it is off, and the pad is disclosed instead.
    ///
    /// Mutation check: this is a SEPARATE call site from the pad edges above. Flip the gate to
    /// `true` and this goes RED on its own (the fixture's footprint is reachable in-zone line), so
    /// neither half of the fix can be unpinned without a test noticing.
    #[test]
    fn the_266_in_zone_portal_escape_is_off_for_an_unverifiable_line_543() {
        let (w, _world, gs) = pad_walker();
        assert_eq!(w.find_in_zone_portal(&gs), None,
            "#543/#266: nav must not walk the character into an advertised in-zone teleport on its \
             own initiative — it cannot verify the line stays in this zone");
    }

    /// **#579, the agent-honesty regression.** With no collision grid — the zone's terrain GLB is
    /// still downloading/decoding, which for freportw (~30 MB) is a multi-second window — the walker
    /// used to publish `nav_state: "navigating"` and steer in a dead-straight line at the goal. An
    /// agent polling in that window read a confident walkable route through geometry that had not
    /// been built: the "700u unobstructed" of the false #560 report.
    ///
    /// The honest answer is `zone_loading` / (here) `zone_assets_pending`, with NO movement intent and
    /// NO route overlay — "I have no model of this world", not "the way is clear". Since #600 the
    /// walker refuses through the SAME `zone_assets::usability` predicate the HTTP surface uses, so
    /// the reason is that predicate's own verdict (a still-loading zone ⇒ `zone_assets_pending`).
    #[test]
    fn no_collision_reports_zone_loading_and_never_a_route() {
        // `walker_with` with no grid ⇒ zone_assets = Pending(TEST_ZONE) (assets still loading).
        let (mut w, nav, intent, view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        let mut gs = eqoxide_core::game_state::GameState::new();
        gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0;

        w.drive_walk(&mut gs, (700.0, 0.0, 0.0));

        let s = nav.nav_state.lock().unwrap().clone();
        assert_eq!(s.state, NAV_STATE_ZONE_LOADING,
            "with no collision the walker must NOT claim to be navigating — that is the #579 lie");
        assert_eq!(s.reason.as_deref(), Some("zone_assets_pending"),
            "#600: the refusal reason is `usability`'s own verdict — a still-loading zone is pending");
        assert!(intent.lock().unwrap().is_none(),
            "the walker must not drive the controller through a world it has not loaded");
        let snap = view.lock().unwrap().clone().expect("the honest no-world state must be published");
        assert!(!snap.zone_model_loaded, "the snapshot must say there is NO world model");
        assert!(snap.committed_coarse.is_empty() && snap.committed_fine.is_empty(),
            "no route may be published without collision");
        assert_eq!(snap.nav_state, NAV_STATE_ZONE_LOADING);
        assert!(w.path.is_empty());
    }

    /// **#600 — THE UNIVERSAL: the walker can NEVER route on a collision grid whose zone is not the
    /// one the character is in.** The sibling of
    /// `zone_assets::no_interleaving_of_the_two_writers_yields_a_usable_wrong_zone`, but exercising
    /// the CONSUMER (`drive_walk`) rather than the pure
    /// predicate — because before this fix the walker consulted `collision.is_none()`, not
    /// `usability`, and so opted out of the guarantee #595 built.
    ///
    /// It drives the REAL `drive_walk` across EVERY interleaving of the two independent writers around
    /// a zone change — the net thread publishing `player.zone` (`apply_net`) and the render thread
    /// running `begin_zone_load` (`apply_render`) — through SHARED `collision`+`zone_assets` handles
    /// the loader mutates for real. In the stale window (net published the new zone, render has not
    /// yet started the load) the grid is STILL PRESENT and non-empty (the exact case
    /// `collision.is_none()` cannot see), and the walker must REFUSE, naming
    /// `zone_assets_stale_for_previous_zone` — never commit a route or a movement intent through the
    /// zone it just left.
    ///
    /// **Mutation check (do this to trust the test):** revert the `drive_walk` gate to
    /// `if self.collision.read().unwrap().is_none()` → in every `net_first` iteration with
    /// `render_lag >= 1` the walker routes on the previous zone's grid and the
    /// `state == NAV_STATE_ZONE_LOADING` assertion in the stale window goes RED. A test that passes
    /// both ways
    /// pins nothing; this one does not.
    #[test]
    fn walker_never_routes_on_a_collision_grid_whose_zone_is_not_the_players() {
        use crate::zone_assets::{begin_zone_load, finish_zone_load, ZoneAssetState, ZoneAssetStateShared};
        // A real floor grid per zone, as the bare `Arc<Collision>` `finish_zone_load` commits.
        let grid = || open_plane(600.0).read().unwrap().clone().unwrap();
        let goal = (400.0, 0.0, 0.0);

        for net_first in [true, false] {
            for render_lag in 0..4u32 {
                // SHARED handles: the walker reads them; `begin`/`finish_zone_load` (the render
                // thread's writes) mutate them — the true two-writer coupling, not a fabricated state.
                let col: crate::collision::SharedCollision = Arc::new(std::sync::RwLock::new(None));
                let za: ZoneAssetStateShared = Arc::new(std::sync::Mutex::new(ZoneAssetState::Idle));
                finish_zone_load(&col, &za, "freporte", Some(grid()), 9, None); // fully loaded, OLD zone
                let (mut w, nav, intent, _view) = walker_with_shared(col.clone(), za.clone());
                let mut gs = eqoxide_core::game_state::GameState::new();
                gs.world.zone_name = "freporte".into();
                gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0; gs.player_pos_known = true;
                *nav.goto_target.lock().unwrap() = Some(goal);

                // CONTROL: a Ready grid for the player's OWN zone must let nav route (no regression —
                // the gate must pass normally once the correct zone's collision is loaded).
                w.drive_walk(&mut gs, goal);
                assert_ne!(nav.nav_state.lock().unwrap().state, NAV_STATE_ZONE_LOADING,
                    "control: routing must be PERMITTED for the player's own loaded zone");

                let apply_net = |gs: &mut GameState| gs.world.zone_name = "qeynos".into();
                let apply_render = |col: &crate::collision::SharedCollision, za: &ZoneAssetStateShared|
                    begin_zone_load(col, za, "qeynos", "loading…");

                if net_first {
                    apply_net(&mut gs);
                    // THE STALE WINDOW. player.zone = qeynos, but `col` still holds freporte's grid
                    // and `za` is still Ready(freporte). The walker MUST refuse on the wrong world.
                    for _ in 0..render_lag {
                        assert!(col.read().unwrap().is_some(),
                            "precondition: the stale window HAS a present grid — the case collision.is_none() misses");
                        w.drive_walk(&mut gs, goal);
                        let s = nav.nav_state.lock().unwrap().clone();
                        assert_eq!(s.state, NAV_STATE_ZONE_LOADING,
                            "net-first lag {render_lag}: routed on the PREVIOUS zone's grid in the stale window (#600)");
                        assert_eq!(s.reason.as_deref(), Some("zone_assets_stale_for_previous_zone"),
                            "the refusal must name the wrong-world reason, not a generic one");
                        assert!(w.path.is_empty(), "no route may be committed for a zone we are not in");
                        assert!(intent.lock().unwrap().is_none(), "no movement intent through the wrong world");
                    }
                    apply_render(&col, &za);
                } else {
                    apply_render(&col, &za); // render clears the grid + goes Pending BEFORE net flips the zone
                    for _ in 0..render_lag {
                        w.drive_walk(&mut gs, goal);
                        assert_eq!(nav.nav_state.lock().unwrap().state, NAV_STATE_ZONE_LOADING,
                            "render-first lag {render_lag}: must refuse mid-change");
                    }
                    apply_net(&mut gs);
                }

                // Still loading the new zone (grid None, state Pending): refuse.
                w.drive_walk(&mut gs, goal);
                assert_eq!(nav.nav_state.lock().unwrap().state, NAV_STATE_ZONE_LOADING,
                    "the new zone's grid is not built yet — refuse");

                // The new zone's grid lands and the player IS in it: routing resumes (no regression).
                finish_zone_load(&col, &za, "qeynos", Some(grid()), 5, None);
                w.drive_walk(&mut gs, goal);
                assert_ne!(nav.nav_state.lock().unwrap().state, NAV_STATE_ZONE_LOADING,
                    "once the correct zone's grid is loaded, in-zone navigation must resume");
            }
        }
    }

    /// The state must not be terminal-sticky: it is a fact about right now, and the goal is KEPT so
    /// navigation resumes by itself once the assets land. Repeated ticks keep saying the same thing.
    #[test]
    fn zone_loading_is_stable_across_ticks_and_keeps_the_goal() {
        let (mut w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        let mut gs = eqoxide_core::game_state::GameState::new();
        *nav.goto_target.lock().unwrap() = Some((700.0, 0.0, 0.0));
        for _ in 0..5 { w.drive_walk(&mut gs, (700.0, 0.0, 0.0)); }
        assert_eq!(nav.nav_state.lock().unwrap().state, NAV_STATE_ZONE_LOADING);
        assert!(nav.goto_target.lock().unwrap().is_some(),
            "the goal must survive the load window so the walker can plan it for real afterwards");
        assert!(w.path_goal.is_none(), "no goal may be recorded as routed while there is no world");
    }

    /// Cancelling the `/goto` during the load window must return to plain `idle`, not leave
    /// `zone_loading` stuck on a walker that is no longer trying to go anywhere.
    #[test]
    fn cancelling_the_goto_while_loading_returns_to_idle() {
        let (mut w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        let mut gs = eqoxide_core::game_state::GameState::new();
        w.drive_walk(&mut gs, (700.0, 0.0, 0.0));
        assert_eq!(nav.nav_state.lock().unwrap().state, NAV_STATE_ZONE_LOADING);
        *nav.goto_target.lock().unwrap() = None;
        assert!(w.resolve_goal(&gs).is_none());
        assert_eq!(nav.nav_state.lock().unwrap().state, "idle");
    }

    /// **#600 (review round 2): `resolve_goal`'s guard responds to the `nav.zone_cross` slot state.**
    /// This is a PURE MECHANISM test of `resolve_goal` in isolation — it drives the slot directly
    /// (`nav.zone_cross`) because `CommandState`/`request_stop` live in a crate ABOVE this one and are
    /// not reachable here. What actually WRITES that slot in production — `drain_zone_cross` re-queuing
    /// during a load, and `request_stop`/`reset_for_zone_change` CLEARING it — is exercised through the
    /// real drivers in the eqoxide-net test
    /// `zone_cross_queued_during_load_is_cancellable_by_stop_and_never_leaks`
    /// (round 6: the name used to be hand-wrapped inside its backticks, which both broke the
    /// `cargo doc` rendering and hid it from every grep for it).
    /// Here we only pin the guard's response: a present slot HOLDS `zone_loading`; an
    /// absent slot retires it to `idle`.
    ///
    /// Mutation check: drop `&& !zone_cross_pending` from the reset guard in `resolve_goal` → this
    /// goes RED (the state resets to `idle` with the slot still set). Complements
    /// `cancelling_the_goto_while_loading_returns_to_idle`, which pins the empty-slot case still resets.
    #[test]
    fn resolve_goal_holds_zone_loading_while_the_zone_cross_slot_is_set() {
        let (mut w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        let gs = eqoxide_core::game_state::GameState::new();
        // The drain published zone_loading and re-queued the one-shot cross; no goto goal is set.
        w.set_nav_state_because(NAV_STATE_ZONE_LOADING, Some("zone_assets_pending"));
        *nav.zone_cross.lock().unwrap() = Some(30);
        assert!(nav.goto_target.lock().unwrap().is_none(), "no concrete goto goal yet — that is the point");

        assert!(w.resolve_goal(&gs).is_none());
        assert_eq!(nav.nav_state.lock().unwrap().state, NAV_STATE_ZONE_LOADING,
            "#600: zone_loading must persist while the zone_cross slot is set, not flip to a misleading idle");

        // Slot cleared (what the real `request_stop`/`reset_for_zone_change` do): the tick retires it.
        *nav.zone_cross.lock().unwrap() = None;
        assert!(w.resolve_goal(&gs).is_none());
        assert_eq!(nav.nav_state.lock().unwrap().state, "idle",
            "with the slot cleared, zone_loading retires to idle exactly as before");
    }

    /// **#600 (review round 3): a never-resolved one-shot `/zone_cross` must NOT leak across a zone
    /// change.** If a cross was re-queued through an asset load for a zone we then leave (a Failed load,
    /// or a server-initiated move mid-load), `reset_for_zone_change` must clear `nav.zone_cross` — else
    /// it survives into the NEXT zone and fires an unexpected crossing there.
    ///
    /// Mutation check: delete the `*self.nav.zone_cross.lock().unwrap() = None;` line from
    /// `reset_for_zone_change` → this goes RED (the stale cross leaks into the new zone).
    #[test]
    fn reset_for_zone_change_clears_a_never_resolved_zone_cross() {
        let (mut w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        *nav.zone_cross.lock().unwrap() = Some(30); // queued but never resolved (loaded through a change)
        w.reset_for_zone_change();
        assert!(nav.zone_cross.lock().unwrap().is_none(),
            "#600: a one-shot cross that never resolved must not survive into the next zone");
    }

    /// **#732: the goal-DROPPED retirement clears `goal` too — not just the zone-change one.**
    ///
    /// #732 was filed against the zone change, but the defect was one line up the call chain:
    /// `set_nav_state_because` was the walker's only route to `idle` and never touched `s.goal`, so
    /// EVERY retirement leaked it. This pins the other production route through that writer — the
    /// per-tick retirement in `resolve_goal` that #725 inverted to cover the whole non-terminal
    /// class (`pending`, `following`, `planning`, …). Its own KNOWN-GAP comment named this as
    /// #732's job; that comment is now the claim under test.
    ///
    /// The goal is planted through the same slots production uses and then the goal slot is
    /// emptied — which is exactly what `drive_chase` does when a followed leader despawns.
    ///
    /// Mutation check: delete `*goal = None;` from `NavStatus::retire_to_idle` → RED here as well
    /// as in the zone-change test, which is the point: one writer, so one mutation kills both.
    #[test]
    fn the_goal_dropped_retirement_clears_the_abandoned_nav_goal_732() {
        let (mut w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        let gs = eqoxide_core::game_state::GameState::new();
        // A chase in flight, with the goal published exactly as `request_follow` publishes it.
        *nav.goto_target.lock().unwrap() = Some((10.0, 20.0, 3.0));
        nav.nav_state.lock().unwrap().goal = Some([10.0, 20.0, 3.0]);
        w.set_nav_state_because("following", None);
        assert_eq!(nav.nav_state.lock().unwrap().goal, Some([10.0, 20.0, 3.0]),
            "PREMISE: the observable field is loaded, and `following` is not terminal — so the tick \
             below genuinely reaches the retirement branch rather than short-circuiting");

        // The leader despawned: the goal slot is emptied, and no zone_cross is queued.
        *nav.goto_target.lock().unwrap() = None;
        assert!(w.resolve_goal(&gs).is_none());

        let s = nav.nav_state.lock().unwrap().clone();
        assert_eq!(s.state, "idle");
        assert_eq!(s.reason.as_deref(), Some(NAV_REASON_GOAL_DROPPED));
        assert_eq!(s.goal, None,
            "#732: a goal that vanished must not keep publishing its coordinates beside `idle`");
    }

    /// **#732 review round 1, N1: the `idle` branch is deliberately NOT gated on the transition
    /// check, and this is what pins that.**
    ///
    /// The rest of `set_nav_state_because` only retires the previous route's facts when `state` or
    /// `reason` actually CHANGES. If the `idle` branch inherited that gate, a retirement into a row
    /// that is already `idle` with the same reason would skip the clear entirely.
    ///
    /// **Honest scope.** The reviewer measured that re-gating the branch is currently fully green,
    /// and that is right: under the fixed code every route to `idle` clears `goal`, so an
    /// already-`idle` row reached through production has nothing left to clear. This test therefore
    /// plants the `idle` + non-null `goal` pair DIRECTLY — the state a hypothetical future writer
    /// (or a partially-reverted one) would leave behind. It pins the SHAPE of the guard, not a
    /// scenario reachable through today's production paths, and it should be read that way.
    ///
    /// Mutation check: re-gate the branch as `if state == "idle" && s.state != state` → RED here,
    /// and green everywhere else in the workspace, which is the whole reason this test exists.
    #[test]
    fn retiring_into_an_already_idle_row_still_clears_the_goal_732() {
        let (w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        {
            // NOT reachable through production today (see the doc comment) — planted to model the
            // row a writer that set `idle` without retiring would leave.
            let mut s = nav.nav_state.lock().unwrap();
            s.state = "idle".to_string();
            s.reason = Some(NAV_REASON_ZONED.to_string());
            s.goal = Some([10.0, 20.0, 3.0]);
            assert_eq!(s.goal, Some([10.0, 20.0, 3.0]),
                "PREMISE: the row is already `idle` with the SAME reason the call below passes, so \
                 a transition-gated branch would take the no-op path");
        }
        w.set_nav_state_because("idle", Some(NAV_REASON_ZONED));
        assert_eq!(nav.nav_state.lock().unwrap().goal, None,
            "#732: retiring to `idle` must clear the goal even when the row is already `idle` — a \
             second retirement is not permitted to be a no-op that leaves the goal standing");
    }

    /// **#725 review, B1: a successful crossing must not look like a dropped one.** This is the line
    /// that runs when the character actually arrives in the new zone — including on the SUCCESS path
    /// of `/v1/move/zone_cross`. It published a bare `idle` with `nav_reason: null`, which is
    /// exactly what an agent sees when nothing was ever requested, so the endpoint's success and its
    /// failure were indistinguishable on the channel the docs tell agents to poll.
    ///
    /// **Mutation check:** change `set_nav_state_because("idle", Some(NAV_REASON_ZONED))` back to
    /// `set_nav_state("idle")` → RED on the reason assertion. Publish `zoned` from somewhere that is
    /// not a zone change and the meaning is gone, but nothing here can catch that — which is why the
    /// constant's doc comment states what the word is *about* (the zone change, not the request).
    #[test]
    fn a_zone_change_publishes_idle_with_a_reason_not_a_bare_idle_725() {
        let (mut w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        // Mid-crossing: a cross was queued and a goal was in flight, as on the real success path.
        *nav.zone_cross.lock().unwrap() = Some(30);
        *nav.goto_target.lock().unwrap() = Some((10.0, 20.0, 3.0));
        w.set_nav_state("pending");

        w.reset_for_zone_change();

        let s = nav.nav_state.lock().unwrap().clone();
        assert_eq!(s.state, "idle", "a zone change ends navigation");
        assert_eq!(s.reason.as_deref(), Some(NAV_REASON_ZONED),
            "#725 B1: `idle` + `nav_reason: null` is the boot state — a SUCCESSFUL crossing must not \
             be reported with it, or success and 'your request was thrown away' are the same read");
    }

    /// **#766: a zone change must retire the FINE tier too — the published verdict AND the plan in
    /// flight.** `reset_for_zone_change` cleared the coarse tier's every trace and the fine tier's
    /// *walker-side* state (`local_path`, `local_i`, `local_stuck_ticks`) but left two things
    /// standing: the published `NavStatus.local` (the field `/v1/observe/debug` serves as
    /// `nav_local`) and `LocalPlanner`'s `pending` slot.
    ///
    /// **What this test measures, precisely.** It reads the row IMMEDIATELY after
    /// `reset_for_zone_change` returns, with no intervening tick — which is the whole point. The
    /// pre-fix code did eventually clear `local`, on some LATER tick that reached `resolve_goal`
    /// with no goal and called `clear_local_plan`; the defect was the window in between, during
    /// which a reader got `nav_local: {"state":"no_way_through"}` beside `nav_state: idle` /
    /// `nav_reason: zoned`. So "the field is already retired at the instant the reset returns" is
    /// the property, and an immediate read is what states it. This test does **not** measure how
    /// WIDE that window was in wall-clock terms on a live client — see the PR body.
    ///
    /// Mutation check: delete `*local = None;` from `NavStatus::retire_to_idle` → the `local`
    /// assertion goes RED. Delete `self.local_planner.cancel();` from `reset_for_zone_change` → the
    /// `is_planning` assertion goes RED. They are separate lines in separate crates and each has its
    /// own assertion here, so neither can ride on the other.
    #[test]
    fn a_zone_change_retires_the_fine_tiers_verdict_and_its_in_flight_plan_766() {
        let col = open_plane(400.0);
        let (mut w, nav, _intent, _view) = walker_with(col.clone());

        // We are mid-goal in the previous zone. This is not scene-setting: `set_nav_local` refuses
        // to store a verdict on an `idle` row (#766, see its doc comment), and the fixture row
        // starts `idle`, so a plant without this line would be swallowed and the PREMISE below
        // would catch it.
        w.set_nav_state_because("navigating", None);

        // The previous zone's fine tier reached a verdict, and it is the UNHEALTHY kind — the only
        // kind `observe.rs` publishes at all (it filters `threaded` out), so this is the shape a
        // reader actually sees.
        w.set_nav_local(Some(eqoxide_ipc::NavLocal {
            state: "no_way_through".into(), reason: "search_closed".into(),
            stuck_ticks: 7, plan_us: 1234,
        }));
        assert_eq!(nav.nav_state.lock().unwrap().local.as_ref().map(|l| l.state.clone()),
            Some("no_way_through".to_string()),
            "PREMISE: the observable field is genuinely loaded before the reset — otherwise the \
             post-condition below would be satisfied by the default row and prove nothing");

        // …and a fine plan is genuinely in flight, posted the way `drive_walk` posts one.
        let c = col.read().unwrap().as_ref().cloned().expect("PREMISE: the fixture has collision");
        assert!(w.local_planner.post_if_idle(crate::planner::LocalRequest {
            gen: 0, // assigned by the planner
            start: [0.0, 0.0, 0.0], goal: [20.0, 0.0, 0.0],
            cell: 2.0, bound: 40.0, carrot_tol: 4.0, collision: c,
        }), "PREMISE: the post succeeded, so there IS a plan to abandon");
        assert!(w.local_planner.is_planning(),
            "PREMISE: the fine planner's `pending` slot is armed — without this the cancel \
             assertion below would pass on a planner that was never busy");

        w.reset_for_zone_change();

        // Read the row RIGHT HERE — no tick between the reset and this read.
        let after_reset = nav.nav_state.lock().unwrap().clone();
        assert_eq!(after_reset.state, "idle");
        assert_eq!(after_reset.reason.as_deref(), Some(NAV_REASON_ZONED));
        assert_eq!(after_reset.local, None,
            "#766: the fine tier's verdict is about threading a corridor in the zone we just LEFT, \
             computed against a collision grid that no longer exists — publishing it beside \
             `idle`/`zoned` tells the agent something false about the zone it is standing in");
        assert!(!w.local_planner.is_planning(),
            "#766: the fine plan in flight is abandoned like the coarse one — it was computed \
             against the previous zone's collision grid. Defence in depth: no production route \
             reaches a `post_if_idle` this stale `pending` would refuse (see the SCOPE note in \
             `reset_for_zone_change`), so this pins the line, not a measured failure");
    }

    /// **#766 review B2: the documented universal holds for the whole `idle` row, not just its
    /// first instant.** `docs/http-api.md` says "`nav_local` is `null` on every `idle`". Retiring
    /// the field in `retire_to_idle` only makes that true at the TRANSITION — and the two writers
    /// race, because `set_nav_local` is called from the net thread while `POST /v1/move/stop` retires the
    /// row from the HTTP thread, each taking the `nav_state` lock separately. A verdict computed
    /// while the goal was live can therefore land after it is gone, with no call site at fault.
    /// `set_nav_local` coerces it away; this measures three directions of that coercion, because a
    /// guard that swallowed everything would satisfy the negative half on its own — and, per review
    /// B5, because a guard keyed on the WRONG thing would satisfy the first two.
    ///
    /// **Why three and not two.** The first two directions are `navigating`+`reason: None` and
    /// `idle`+`reason: Some`, which vary two things at once, so any predicate separating those rows
    /// passes: the reviewer replaced the guard with `if s.reason.is_some()` and the whole workspace
    /// stayed green. The third direction is `blocked`+`reason: Some` — non-`idle` *and* carrying a
    /// reason — which is exactly the row that tells a state-keyed guard apart from a reason-keyed one.
    ///
    /// Mutation checks, both RUN on this branch, not reasoned, and reported by ASSERTION rather than
    /// by line number (review B8: a line locator drifts on the next edit above it, and a freshly
    /// re-measured one is trusted more than it deserves). (a) Delete
    /// `let local = if s.state == "idle" { None } else { local };` from `Walker::set_nav_local` →
    /// the post-retirement assertion goes RED, the other directions stay GREEN. (b) Replace it with
    /// `if s.reason.is_some() { None } else { local }` → the `blocked` assertion below is the ONLY
    /// thing that goes RED anywhere (`eqoxide-nav` `FAILED. 214 passed; 1 failed; 16 ignored`; before
    /// that assertion existed the same mutation left the whole workspace green). So the line can
    /// fire, fires only where it should, and keys on the right field.
    #[test]
    fn a_verdict_arriving_after_the_goal_is_retired_is_not_published_766() {
        let (w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        let verdict = || Some(eqoxide_ipc::NavLocal {
            state: "no_way_through".into(), reason: "search_closed".into(),
            stuck_ticks: 7, plan_us: 1234,
        });

        // Direction 1 — mid-goal, the verdict publishes exactly as #382 intended. Without this the
        // test would pass on a `set_nav_local` that had been gutted to a no-op.
        w.set_nav_state_because("navigating", None);
        w.set_nav_local(verdict());
        assert_eq!(nav.nav_state.lock().unwrap().local, verdict(),
            "PREMISE: the guard does not disturb the tier's normal publication — a verdict on a \
             live goal is still what a reader gets");

        // Direction 2 — the goal is retired, and only THEN does the fine tier's reply come back.
        // This is the interleaving, not a hypothetical: the reply was in flight across the
        // retirement. Which of the six reasons retired the row is immaterial — they all land in
        // `retire_to_idle` — so this uses the one whose constant lives in this crate.
        w.set_nav_state_because("idle", Some(NAV_REASON_ZONED));
        assert_eq!(nav.nav_state.lock().unwrap().local, None,
            "PREMISE: retirement cleared the field, so the assertion below is about the LATE \
             write and cannot be satisfied by leftover state");

        w.set_nav_local(verdict());
        assert_eq!(nav.nav_state.lock().unwrap().local, None,
            "#766 B2: `idle` means there is no goal, and `NavLocal` is a verdict about threading \
             toward one — publishing it here would tell the agent the fine planner is stuck on a \
             goal it no longer has. `docs/http-api.md` states this as a universal over the row, \
             so the row must hold it for its whole lifetime, not just at the transition");

        // Direction 3 — a TERMINAL `blocked` row, which carries a reason. This is the direction that
        // makes the test able to see a WRONG predicate (review B5). Directions 1 and 2 vary two
        // things at once (`navigating`+no reason vs `idle`+reason), so any predicate separating those
        // two rows passes them both — the reviewer drove `if s.reason.is_some()` through the entire
        // workspace green. `blocked` is non-`idle` AND carries a reason, so it separates the two.
        //
        // It also pins #382's keep-the-verdict-as-EVIDENCE design, which had no test anywhere in the
        // tree despite being this field's most load-bearing documented behaviour on a non-`idle` row:
        // on a terminal failure the fine tier's verdict is the evidence BEHIND the failure the agent
        // is being told about, so suppressing it would delete the explanation and keep the complaint.
        w.set_nav_state_because("blocked", Some("local_no_way_through"));
        w.set_nav_local(verdict());
        assert_eq!(nav.nav_state.lock().unwrap().local, verdict(),
            "#766 B5 / #382: a terminal `blocked` row MUST keep the fine tier's verdict — it is the \
             evidence behind the failure being reported, and `Walker::local_says_no_way_through` \
             reads it back as a steering input. The guard keys on `state == \"idle\"` and nothing \
             else; a predicate that keyed on `reason` instead would pass every other assertion in \
             this test and silently break this design");
    }

    /// **#766 review B3 — a dead fine worker is a WORKER fault and must outlive the goal.**
    ///
    /// `planner_dead` is one of the three publishable `nav_local.state` values, but unlike
    /// `no_way_through` / `exhausted` it is not a verdict about a goal: it is a latched client fault
    /// meaning steering has degraded to the coarse 8 u route with nothing on any nav route to
    /// recover it. #766 retires `nav_local` on
    /// every route to `idle`, and the review found the consequence — an agent BETWEEN goals, which is
    /// when it polls to decide what to do next, could no longer see that its fine planner was dead.
    /// `nav_local` is its only publication surface in the tree (the `no_path`/`planner_dead` pair on
    /// `nav_state` comes from the COARSE planner, a different object).
    ///
    /// The fix is a separate field outliving the goal, not a carve-out in `retire_to_idle` — that
    /// would have re-opened the clear-on-every-`idle` uniformity #766 exists to create. (Its
    /// lifetime is the fine WORKER's; round-6 review B12 corrected an earlier "session-scoped" here.
    /// Nothing this test does turns on the difference — it retires a goal, and retiring a goal does
    /// not replace a worker.)
    ///
    /// **This test is driven end-to-end by production `drive_walk`, and an earlier draft that was not
    /// is why.** My recollection of that draft — pre-forcing `is_dead()` in a loop, calling
    /// `drive_walk` with an EMPTY path, latching from above `let have_path` — is **recollection, not
    /// history**: the review established that the draft survives in no commit, ref or reflog entry,
    /// so nobody can reproduce it and no account of its internals, mine included, is checkable
    /// (round-5 review B10). Treat it the way this branch treats any un-run claim.
    ///
    /// What IS checkable is on the tree in front of you, and it is the part that matters. An empty
    /// path returns at `awaiting_first_plan`, one of the FIVE early returns above `let have_path`, so
    /// no latch placed after `advance_cursor` can fire in that fixture at all — reachability. And
    /// independently of any draft, `dead` is only ever written inside `have_path`, so an earlier latch
    /// is a tick late by construction even where it IS reached — ordering, which is the decisive
    /// defect and is argued in full on [`Walker::latch_local_planner_liveness`]. The two are separate
    /// failures with separate evidence; earlier rounds on both sides collapsed them into one story
    /// and got the attribution wrong in both directions. Below, the test avoids the whole question:
    /// no forcing loop, no direct latch call, and a committed route so production discovers the death
    /// itself.
    ///
    /// Mutation checks, both RUN, reported by ASSERTION rather than by line number — a re-measured
    /// line number is correct only until the next edit above it, and reads more trustworthy than it
    /// is (review B8). Delete the `latch_local_planner_liveness()` call from `drive_walk` → the
    /// discovery assertion here goes RED, `eqoxide-nav` `214 passed; 1 failed; 16 ignored`. Clear
    /// `local_planner_dead` in `retire_to_idle` instead of keeping it → the post-retirement assertion
    /// here goes RED (`eqoxide-nav` `214 passed; 1 failed`) **and** so does the endpoint test in
    /// `eqoxide-http` (`246 passed; 1 failed`). Two lines in two crates, one assertion each, and the
    /// second is visible from the published API as well as from the row.
    #[test]
    fn a_dead_fine_planner_stays_visible_after_the_goal_is_retired_766() {
        let (mut w, nav, _intent, _view) = walker_with(open_plane(400.0));
        let mut gs = eqoxide_core::game_state::GameState::new();
        gs.world.zone_name = TEST_ZONE.into(); // #600: loaded-zone match, or drive_walk halts early
        gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0; gs.player_pos_known = true;

        // A committed route the walker is following — the ONLY situation in which the fine tier is
        // touched at all, and therefore the only one in which its death is discoverable.
        let goal = (60.0, 0.0, 0.0);
        *nav.goto_target.lock().unwrap() = Some(goal);
        w.committed = Some(committed_complete()); // #851
        w.set_nav_state("navigating");
        w.path = vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [40.0, 0.0, 0.0], [60.0, 0.0, 0.0]];
        w.path_i = 0;
        w.path_goal = Some(goal); // same goal → no replan, no `awaiting_first_plan`

        // Kill the fine worker the way a panic does: its reply `Sender` drops. Nothing has NOTICED
        // yet — noticing is a failed send or a disconnected receive, and both live in `drive_walk`.
        w.local_planner.kill_worker_for_test();
        assert!(!nav.nav_state.lock().unwrap().local_planner_dead,
            "PREMISE: nothing has published the fault yet, so the assertion below measures the \
             latch and not a value that was already there");

        w.drive_walk(&mut gs, goal);

        assert!(nav.nav_state.lock().unwrap().local_planner_dead,
            "#766 B3: production `drive_walk` discovered the fine worker's death on this tick and \
             must record it on the shared row that outlives the goal, not only in the per-goal \
             verdict — steering has degraded to the coarse 8u route and THIS thread does not come \
             back");
        assert_eq!(nav.nav_state.lock().unwrap().local.as_ref().map(|l| l.state.clone()),
            Some("planner_dead".into()),
            "PREMISE: the pre-existing per-goal publication still happens — the liveness field is \
             an addition beside it, not a replacement for it");

        // …and now the retirement that #766 made uniform. This is the exact interleaving the review
        // found: `nav_local` goes `null` and the agent is between goals, which is when it polls.
        w.reset_for_zone_change();
        let s = nav.nav_state.lock().unwrap();
        assert_eq!(s.state, "idle");
        assert_eq!(s.local, None,
            "PREMISE: the per-goal verdict is still retired — the liveness field is an addition, \
             not a hole in #766's guarantee");
        assert!(s.local_planner_dead,
            "#766 B3: a zone change does not replace the fine worker, so the dead one is still the \
             live one and the fault has not healed. Clearing it here would tell an agent its \
             degraded steering had recovered when nothing recovered it");
    }

    /// **#766 review B9 — the latch is scoped to the WORKER, and construction is where that is
    /// enforced.** Once set, `local_planner_dead` is cleared by nothing on any nav route — no goal,
    /// no zone change, no retirement — which is right for a thread that does not come back. But
    /// "no nav route clears it" and "it outlives the thread it describes" are different claims, and
    /// only the first one is wanted. The row is a shared `Arc` the HTTP surface holds; a `Walker` is
    /// not. So a second `Walker` over the same row would publish a *fresh, healthy* worker as
    /// permanently dead: #343's shape, and a lie in the honesty-critical direction (the client
    /// asserting a fault it has just fixed). `Walker::new` is therefore the one writer that clears
    /// it, and this test is what pins that — so do not read the opening clause as "never clears".
    ///
    /// Production cannot reach that today — `Walker::new` runs once per process, through
    /// `ActionLoop::new` from `run_login_flow`, which returns when the gameplay phase ends. Round 4
    /// declined the clear on that ground and the round-5 review was right to block it: what the
    /// missing route makes untestable is the end-to-end relogin *scenario*, not the *clear*, and the
    /// clear is what carries the guarantee. `Walker::new` takes caller-owned slots and this suite
    /// already calls it directly, so the property is testable at construction with no relogin route
    /// in sight — which is what this does. Tier: the flag's lifetime is now pinned to the
    /// constructor that spawns the worker, so the bad state is created-and-cleared in one place
    /// rather than argued about in a comment.
    ///
    /// Mutation check, RUN, named by assertion rather than by line (review B8): delete the
    /// `local_planner_dead = false` clear from [`Walker::new`] → this test's B9 assertion goes RED,
    /// alone. Measured across `eqoxide-nav` / `-http` / `-ipc` / `-net` (this branch's blast radius,
    /// the last of them because `action_loop.rs` `include_str!`s `docs/http-api.md`): `eqoxide-nav`
    /// `215 passed; 1 failed; 16 ignored`, and 247 / 37 / 380 unmoved in the other three. Not a
    /// workspace run — say "these four crates", not "nothing anywhere". That the blast radius is one
    /// assertion is the honest measure of the fix: on today's single-`Walker` process it is a
    /// structural guarantee, not a behaviour change to any live path.
    #[test]
    fn a_new_walker_does_not_inherit_a_previous_workers_death_766() {
        // A row that already carries a dead worker's latch — the state an in-process relogin would
        // hand the next `Walker`, and the only way to reach it from here (nothing in the process
        // retires a `Walker` today, which is exactly why the scenario is not the thing under test).
        let nav: eqoxide_ipc::NavSlots = Default::default();
        nav.nav_state.lock().unwrap().local_planner_dead = true;
        assert!(nav.nav_state.lock().unwrap().local_planner_dead,
            "PREMISE: the row starts dirty, so the assertion below measures the constructor and not \
             a field that was `false` all along");

        let world: eqoxide_ipc::WorldSlots = Default::default();
        let intent: eqoxide_ipc::NavIntent = Default::default();
        let view: crate::diagnostics::NavDebugView = Default::default();
        let collision = open_plane(400.0);
        let za = zone_assets_for(&collision);
        let _w = Walker::new(nav.clone(), world, collision, intent, view, za); // #787-NOT-PRODUCTION

        assert!(!nav.nav_state.lock().unwrap().local_planner_dead,
            "#766 B9: `Walker::new` has just spawned a NEW `LocalPlanner`, which is alive. Leaving \
             the previous worker's latch standing would publish `nav_local_planner_dead: true` for a \
             thread that is running fine, and it would never clear — the client asserting a \
             permanent fault it had itself just repaired");
    }

    /// The literal token a fine-worker construction site must carry to declare itself **not a
    /// production fine worker** — see [`exactly_one_production_fine_worker_is_built_in_the_tree_787`],
    /// which treats every unmarked, non-prose site as production. Grep it to enumerate the opt-outs.
    const NOT_PRODUCTION: &str = "#787-NOT-PRODUCTION";

    /// The message the #787 guard fails with. Its whole job: tell whoever tripped it *which four
    /// sentences have just become false*, so the decay is not silent.
    fn four_sentences(what: &str) -> String {
        format!(
"{what}

`NavStatus::local_planner_dead` is LATCHED and scoped to the fine WORKER. The agent-facing name for
it — `nav_local_planner_dead`, documented as SESSION-scoped — is accurate only because worker span
and process span coincide, and they coincide only while exactly one fine steering worker is built per
client process. A second production fine worker makes the session-scoped name WRONG, and makes these
four sentences false. Revisit all four before you land this:

  1. crates/eqoxide-ipc/src/lib.rs, `NavStatus::local_planner_dead` field doc —
     \"…exactly one fine worker exists per process and 'latched forever' and 'latched for this
      worker' coincide.\"
  2. crates/eqoxide-ipc/src/lib.rs, same field doc —
     \"…which is why the agent-facing docs call the field session-scoped.\"
  3. crates/eqoxide-http/src/observe.rs, the `nav_local_planner_dead` publication comment —
     \"…it reads as session-scoped from outside only because exactly one fine worker is built per
      process.\"
  4. docs/http-api.md — the section headed (backticks included, so this is grep-able verbatim):
     ### `nav_local_planner_dead` — fine-planner liveness, session-scoped
     \"…exactly one fine worker is built per client process, so from out here the two are the same
      span.\"

Weaker dependents to re-read while you are there: the \"this clear is a no-op in production\" comment
in `Walker::new`, and `a_new_walker_does_not_inherit_a_previous_workers_death_766` below (which
constructs a second `Walker` deliberately and therefore can never be the thing that catches this).

If the new site is genuinely NOT a production worker (a test, a bench, a fixture), append a trailing
`// {NOT_PRODUCTION}` comment to its line and this guard will accept it. NOTE: nothing checks WHERE a
marker may appear, so a marker on a production line disarms this guard for that site silently. It is
an honour-system opt-out; `grep -rn '{NOT_PRODUCTION}'` enumerates every use.")
    }

    /// Repo root, from this crate's manifest dir. Anchored below, so a workspace re-layout fails the
    /// guard loudly instead of quietly scanning an empty tree.
    fn repo_root_787() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
    }

    /// Every `.rs` file under `root`, recursively.
    ///
    /// `target/` and every DOT-directory are skipped. `.claude/worktrees/` in particular holds other
    /// agents' complete checkouts of this same tree; walking into one would count their construction
    /// sites as this working tree's and fail the guard for reasons that have nothing to do with the
    /// code under test.
    fn rs_files_787(root: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(dir) = std::fs::read_dir(root) else { return };
        for entry in dir {
            let p = entry.expect("readable dir entry").path();
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if p.is_dir() {
                if name == "target" || name.starts_with('.') {
                    continue;
                }
                rs_files_787(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }

    /// The tree's TRACKED `.rs` files, straight out of the git index.
    ///
    /// The corpus is taken from the index rather than measured against a tolerance because round-1
    /// review measured the tolerance failing: with `crates/eqoxide-http/` skipped the walk returned
    /// 152 files, which cleared the old `>= 150` floor with all five named anchors still present,
    /// leaving 20 files (11.6% of the tree) dark and a planted production site inside them invisible
    /// — the #778 failure reproduced inside this guard. Four crates fit under that band; the worst
    /// case was 22 files / 12.8% with every control green. A floor 13% below the true value is a
    /// tolerance band, not a reach control, so there is no longer a floor: the index IS the corpus,
    /// and the filesystem walk is checked against it for equality below.
    ///
    /// Returns `None` when the tree is not a git checkout — **which is a real environment here, not a
    /// hypothetical**: this workspace's tests are compiled and run on a remote builder that receives
    /// an rsync'd copy of the worktree with no `.git`, and the first version of this helper turned
    /// that into a hard failure of the whole suite. A missing index is not evidence of a second
    /// worker, so it must not be reported as one. The caller falls back to the git-free per-member
    /// reach control and **says out loud in its failure text which control was in force**; see
    /// [`workspace_members_787`].
    fn git_tracked_rs_787(root: &std::path::Path) -> Option<Vec<String>> {
        let out = std::process::Command::new("git")
            .arg("-C").arg(root)
            .args(["ls-files", "-z", "*.rs"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None; // not a git checkout (the remote builder), or git is absent
        }
        let files: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        // An index that exists but lists nothing is NOT the no-git case — it is a broken corpus, and
        // silently accepting it would be the tolerance band all over again.
        assert!(!files.is_empty(),
            "#787 guard reach: `git ls-files '*.rs'` succeeded but listed nothing — the corpus is \
             empty, so a pass here would assert nothing at all");
        Some(files)
    }

    /// The workspace's member crate directories, read out of the root `Cargo.toml`.
    ///
    /// This is the corpus check that survives where git does not. The defect round-1 review measured
    /// was a whole crate directory vanishing from the walk under a tolerance floor; a member list read
    /// from a file that must exist in *any* copy of this tree catches exactly that, with no index and
    /// no hard-coded count to drift. It is strictly weaker than the index equality check — it cannot
    /// see a single missing FILE, only a missing member — and the failure text below says so rather
    /// than letting a builder-only run look as strong as a developer one.
    /// It has now been wrong twice in opposite directions, and both were measured:
    ///
    ///   * `p.contains('/')` dropped `tools`, a member with no slash in its path — 12 of 13, round-2
    ///     review;
    ///   * arming on `buf.ends_with("members")` armed on **`default-members`** as well, so adding
    ///     `default-members = ["tools"]` to the manifest collapsed the check to a single member while
    ///     the guard reported success. Round-3 review measured 29 `.rs` files going dark that way,
    ///     with an unmarked production construction planted among them, and the guard GREEN.
    ///
    /// The key is therefore ANCHORED (a line whose own key is exactly `members`), the manifest has its
    /// comments STRIPPED first (a `#` comment containing a quoted string was swept in as a fourteenth
    /// member and turned an intact tree red — the opposite failure, same root: reasoning about raw
    /// text while claiming a property of effective text), and — the part that actually makes this a
    /// control rather than a number nobody checks — the result is ASSERTED against the directories
    /// that contain a `Cargo.toml`. A count that is printed and never compared is not a control.
    fn workspace_members_787(root: &std::path::Path) -> Vec<String> {
        let raw = std::fs::read_to_string(root.join("Cargo.toml"))
            .expect("#787 guard reach: the workspace manifest must be readable — it is the corpus \
                     definition when there is no git index");

        // REACH CONTROL ON THE STRIPPER ITSELF. It is a scanner, so it needs evidence that it ran and
        // did something, not just that it returned. A silent no-op would restore the exact behaviour
        // this fix exists to remove, with more confident prose sitting on top of it.
        {
            let probe = "a = \"x # not-a-comment\" # yes-a-comment\nb = 'y # also-not'\n";
            let got = strip_toml_comments_787(probe);
            assert!(got.contains("x # not-a-comment"),
                "#787 guard: the TOML comment stripper corrupted a quoted value — a stripper that \
                 eats string contents swaps one silent evasion for another. Got: {got:?}");
            assert!(got.contains("y # also-not"),
                "#787 guard: the TOML comment stripper corrupted a literal-string value. Got: {got:?}");
            assert!(!got.contains("yes-a-comment"),
                "#787 guard: the TOML comment stripper removed NOTHING from a probe that contains a \
                 real comment — it is a no-op, and every claim below about comment-free text is \
                 false. Got: {got:?}");
            assert_eq!(got.lines().count(), probe.lines().count(),
                "#787 guard: the TOML comment stripper changed the line structure of its input");
        }

        let manifest = strip_toml_comments_787(&raw);

        // ANCHORED key: the line's own key must be exactly `members`, so `default-members`,
        // `exclude-members` and anything else ending in those seven letters cannot arm it.
        let mut members = Vec::new();
        let mut collecting = false;
        for line in manifest.lines() {
            if !collecting {
                let t = line.trim_start();
                let Some(rest) = t.strip_prefix("members") else { continue };
                if !rest.trim_start().starts_with('=') { continue }
                collecting = true;
            }
            let mut in_str = false;
            let mut delim = '"';
            let mut buf = String::new();
            for ch in line.chars() {
                match ch {
                    c if in_str && c == delim => {
                        in_str = false;
                        let p = buf.trim().trim_end_matches('/');
                        if !p.is_empty() { members.push(p.to_string()); }
                        buf.clear();
                    }
                    '"' | '\'' if !in_str => { in_str = true; delim = ch; buf.clear(); }
                    ']' if !in_str => { collecting = false; break }
                    _ if in_str => buf.push(ch),
                    _ => {}
                }
            }
            if !collecting { break }
        }
        assert!(!members.is_empty(),
            "#787 guard reach: parsed zero workspace members out of the root `Cargo.toml` — the \
             corpus definition is unreadable, so this guard cannot claim to have covered the tree");
        members
    }

    /// Directories under `root` that contain a `Cargo.toml` — the filesystem's own answer to "what
    /// crates are in this workspace", independent of how the manifest spells its member list.
    ///
    /// This exists so [`workspace_members_787`] has something to be checked AGAINST. A crate here
    /// that the manifest does not list is a false RED; that is the safe direction and it forces a
    /// decision rather than silently shrinking the corpus.
    fn crate_dirs_787(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            if p.is_dir() {
                if name == "target" || name.starts_with('.') { continue }
                if p.join("Cargo.toml").is_file() {
                    out.push(p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/"));
                }
                crate_dirs_787(&p, root, out);
            }
        }
    }

    /// `text` with every TOML `#` comment removed, newlines preserved.
    ///
    /// String-aware on purpose: `#` is legal inside a quoted value, and a naive strip that cut at the
    /// first `#` would corrupt member paths — replacing one silent evasion with another. Basic (`"`)
    /// and literal (`'`) strings are both tracked, and a backslash escape inside a basic string is
    /// honoured. It is exercised by a probe in [`workspace_members_787`] before it is trusted.
    fn strip_toml_comments_787(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for line in text.split_inclusive('\n') {
            let (mut in_str, mut delim, mut esc) = (false, '"', false);
            for ch in line.chars() {
                if in_str {
                    out.push(ch);
                    if esc { esc = false; continue }
                    match ch {
                        '\\' if delim == '"' => esc = true,
                        c if c == delim => in_str = false,
                        _ => {}
                    }
                    continue;
                }
                match ch {
                    '"' | '\'' => { in_str = true; delim = ch; out.push(ch); }
                    '#' => { if line.ends_with('\n') { out.push('\n') } break }
                    _ => out.push(ch),
                }
            }
        }
        out
    }

    /// `text` with every comment blanked to spaces, newlines preserved so line numbers still map.
    ///
    /// This exists to close A5 (round-1 review): `Walker /* … */ ::new(` is valid Rust and, since
    /// [`flatten_787`] strips whitespace but not comments, the needle never formed — which falsified
    /// the very reflow-resistance this guard advertised. The scan now runs over BOTH the raw text and
    /// this blanked copy and unions the findings, so an interposed comment cannot hide a call.
    ///
    /// The blanking is deliberately naive (it tracks `"` strings and nested `/* */`, and will be
    /// confused by a `'"'` char literal). That is acceptable *in this direction*: a confused blanker
    /// stops blanking, which can only make this pass find FEWER candidates than the raw pass, and the
    /// raw pass is unioned in. It cannot silently remove a finding the raw scan already has.
    fn blank_comments_787(text: &str) -> String {
        let b: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let (mut i, mut state, mut depth) = (0usize, 0u8, 0usize);
        while i < b.len() {
            let c = b[i];
            let n = if i + 1 < b.len() { b[i + 1] } else { '\0' };
            match state {
                // 0 = code
                0 => {
                    if c == '/' && n == '/' { state = 1; out.push_str("  "); i += 2; continue; }
                    if c == '/' && n == '*' { state = 2; depth = 1; out.push_str("  "); i += 2; continue; }
                    if c == '"' { state = 3; }
                    out.push(c);
                    i += 1;
                }
                // 1 = line comment, to end of line
                1 => {
                    if c == '\n' { state = 0; out.push(c); } else { out.push(' '); }
                    i += 1;
                }
                // 2 = block comment (Rust nests them)
                2 => {
                    if c == '/' && n == '*' { depth += 1; out.push_str("  "); i += 2; continue; }
                    if c == '*' && n == '/' {
                        depth -= 1; out.push_str("  "); i += 2;
                        if depth == 0 { state = 0; }
                        continue;
                    }
                    out.push(if c == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                // 3 = inside a string literal
                _ => {
                    if c == '\\' {
                        out.push(c);
                        if i + 1 < b.len() { out.push(n); }
                        i += 2;
                        continue;
                    }
                    if c == '"' { state = 0; }
                    out.push(c);
                    i += 1;
                }
            }
        }
        out
    }

    /// Whitespace-stripped file text, plus the source line each retained BYTE came from.
    ///
    /// Stripping first is what makes the scan survive a WHITESPACE reflow: `eqoxide-http`'s sibling
    /// guard was rewritten for exactly this reason (see `slot.rs`) after a call site wrapped across
    /// lines and silently left a line-based guard's coverage. Here the needle is found in the
    /// flattened text and the byte offset is mapped back to the line the match STARTS on, which is
    /// the line whose raw text is then classified. Whitespace alone was NOT enough — see
    /// [`blank_comments_787`], added after review measured `Walker /* x */ ::new(` slipping through.
    fn flatten_787(text: &str) -> (String, Vec<usize>) {
        let mut flat = String::with_capacity(text.len());
        let mut line_of = Vec::with_capacity(text.len());
        let mut line = 1usize;
        for c in text.chars() {
            if c == '\n' {
                line += 1;
            }
            if c.is_whitespace() {
                continue;
            }
            flat.push(c);
            for _ in 0..c.len_utf8() {
                line_of.push(line);
            }
        }
        (flat, line_of)
    }

    /// **#787 — a TRIPWIRE under the one-fine-worker premise. Read the limits before you trust it.**
    ///
    /// `local_planner_dead` is latched for the life of a fine WORKER; the agent is told it is
    /// SESSION-scoped. Those are the same span only while exactly one fine worker is constructed per
    /// client process, and that premise used to be prose with nothing under it at all.
    ///
    /// **What it grades — and the subject mismatch that is its deepest limit.** The premise is a
    /// RUNTIME count: *exactly one fine worker is CONSTRUCTED per process*. This test is a TEXT
    /// count: *exactly one construction SITE is written*. Those are different claims and they come
    /// apart on exactly the scenario #787 was filed about.
    ///
    /// > **An in-process relogin re-enters an existing construction path. It adds ZERO new textual
    /// > sites.** Round-1 review of #836 measured it: move the production construction into a helper
    /// > in `action_loop.rs` and call that helper twice — one unmarked site in the whole tree, in the
    /// > expected file, both asserts below satisfied, **two fine workers per process, guard green.**
    ///
    /// So do not read a pass here as "one worker exists". Read it as "nobody has written a second,
    /// plainly-spelled construction site". That is a real and useful thing to know — it is the shape
    /// most second workers would actually arrive in — but it is a proxy, and it is the weaker of the
    /// two claims. [`eqoxide_ipc::NavStatus::local_planner_dead`]'s field doc says the same thing.
    ///
    /// Two links of the premise chain are checked, over the WHOLE repository, not this crate:
    ///
    ///   * exactly one production `Walker::new(` call site — in `crates/eqoxide-net/src/action_loop.rs`;
    ///   * exactly one production `LocalPlanner::spawn(` call site — in this file, inside `Walker::new`.
    ///
    /// The second link matters on its own: the field doc's claim is "`LocalPlanner::spawn` is reached
    /// only through `Walker::new`", so a fine worker spawned OUTSIDE a `Walker` would break the
    /// premise without adding a `Walker`.
    ///
    /// **Why a source scan and not a runtime counter.** A process-global `AtomicUsize` in
    /// `Walker::new` grades the property rather than the text and would be the stronger instrument —
    /// except that there is no `cfg` that separates "the production process" from "a downstream
    /// crate's test binary". `cfg(test)` is per-compilation: when `eqoxide-nav` is built as a
    /// dependency of `eqoxide-net`, `cfg(test)` is FALSE for this crate, so a `#[cfg(not(test))]`
    /// trip fires inside `eqoxide-net`'s own suite, which builds two `ActionLoop`s (and therefore two
    /// `Walker`s) in one process. That was measured, not reasoned — see the PR body. A release-only
    /// trip avoids the false fire but is never exercised in CI and turns a prose-decay problem into a
    /// new field failure mode. So the instrument that can fail at AUTHORING time wins here.
    ///
    /// **MEASURED evasions. This list is the honest description of the instrument** — round-1 review
    /// of #836 planted eight production fine-worker constructions simultaneously and this test
    /// reported `ok`. Two of the eight are now closed; the rest are not closable by a text scan
    /// without turning it into a Rust parser (#799 catalogues the family).
    ///
    /// A round-3 draft of this comment said "**every** row below was RUN … not reasoned about".
    /// Round-3 review found that false for two rows and it was: the `LocalPlanner::spawn` fn-pointer
    /// row and the `Default`/`Clone` row were written by analogy with rows next to them. Both have
    /// since been run (the spawn fn-pointer row survived exactly as claimed; the `Default` row did
    /// **not** and has been rewritten to what the run showed). Every row now carries the plant that
    /// produced its verdict, so the claim is checkable per row rather than as a blanket assurance —
    /// which is the form the false one took.
    ///
    /// A round-2 draft of this comment claimed the marker hole was *narrower* than review recorded —
    /// that the opt-out could only relocate the production site within its own file. **That was
    /// wrong and round-2 review refuted it by construction**: a marked second construction in a
    /// different file, original untouched, leaves the guard green. Two rows in this very table (the
    /// marker row and the macro row) are instances of the form that claim said could not exist. The
    /// narrowing is withdrawn; what follows is what was measured, nothing inferred from it.
    ///
    /// | form | status |
    /// |---|---|
    /// | plainly-written second site, any file, any whitespace reflow | **caught** |
    /// | `use …::Walker as W;` — plain `as`-rename | **caught** (alias check below) |
    /// | `pub use …::Walker as W;` — re-export rename | **caught since round 2**; the round-1 check tested `starts_with("use ")`, which no re-export can satisfy — every one of them leads with a visibility (`pub`, `pub(crate)`, `pub(super)`, `pub(in …)`) — so `pub use … as W;` was GREEN. The check now strips the visibility, parenthesised form included, before matching |
    /// | `Walker /* x */ ::new(` — comment interposed in the path | **caught since round 2**, via [`blank_comments_787`]; whitespace-only flattening never formed the needle |
    /// | `let ctor = …::Walker::new;` then `ctor(..)` — fn-pointer binding | **NOT caught.** The name is followed by `;`, never `(`, and it is not an `as`-rename. The cheapest invisible second worker there is; a measured survivor against a different pin in this repo already (#828 round 3) |
    /// | same, for `LocalPlanner::spawn` | **NOT caught** — run, not reasoned, since round 4: an unmarked production `fn second_fine_worker_787() { let ctor = …::LocalPlanner::spawn; ctor() }` added to this very file left the guard GREEN (`walked 172`, one site found) |
    /// | `<…::Walker>::new(..)` — qualified path | **NOT caught** |
    /// | a `macro_rules!` body carrying the marker, invoked from production | **NOT caught** |
    /// | a `// #787-NOT-PRODUCTION` marker on a second PRODUCTION construction, original untouched, ANY file | **NOT caught.** The marker is an unpinned opt-out: nothing asserts where a marker may appear, so a marked line is invisible wherever it is. Measured in `observe.rs` — guard green with two production constructions in two files; removing the marker went RED naming the file |
    /// | one textual site executed twice (the relogin shape) | **NOT caught** — see the subject mismatch above |
    /// | a `Default`/`Clone` impl for `Walker` | **CAUGHT in the direct form — the round-3 "NOT caught" here was reasoned and it was wrong.** Run in round 4: an `impl Default for Walker` whose body is a struct literal went **RED** (`found 2 site(s)`), because to be a fine worker at all the literal has to fill `local_planner`, and the only plain spelling of that is `LocalPlanner::spawn(` — which is the second needle. The same impl reaching the worker through a fn-pointer binding went GREEN, so this evades only *via* the fn-pointer row above, not on its own. Both plants were text-level: the guard is a text scan, and a full `Walker` literal does not compile outside this module |
    /// | the needle inside a `/* … */` block comment, or inside a string literal | **false RED** (measured). The two passes are combined by MAX per line so that two constructions on one line still count as two; the cost is that the raw pass's hit survives even where the blanked pass correctly sees none. Harmless direction, disclosed rather than left to be tripped over |
    ///
    /// **The balance, corrected.** An earlier draft of this comment argued that the residual holes
    /// push into a false RED — the safe direction — because #799's `if false` / `#[cfg(any())]` /
    /// shadowing evasions make a *written* call *unreached*. That argument was **falsified by
    /// measurement**: the residual is dominated by invisible-to-the-scan misses (the table above),
    /// not by false REDs. Those false-RED forms are real and they are the harmless direction, but
    /// they are not the balance. **This is a tripwire for the common case, not a pin**, and the
    /// argument that a source scan was the right mechanism here is correspondingly weaker than it
    /// was written to be. What survives of it is narrower and still true: the instrument fails at
    /// AUTHORING time, which is when this premise decays, and a runtime counter cannot be built here
    /// at all (next paragraph).
    ///
    /// **Reach.** #778 found an existing source-scanning guard silently covering a fraction of its
    /// corpus, which is indistinguishable from a passing one — and round-1 review reproduced exactly
    /// that inside THIS guard's first draft (see [`git_tracked_rs_787`] for the measurement). The
    /// corpus is therefore the git index, not a tolerance band, and the filesystem walk is asserted
    /// EQUAL to it rather than merely large. Untracked `.rs` files are scanned too (the corpus is the
    /// union), so a stray scratch file is a false RED — the safe direction, disclosed here rather
    /// than left to be discovered.
    ///
    /// **The reach control is not the same strength everywhere, and this guard says which one ran.**
    /// The index equality check needs a git checkout. The remote builder that compiles and runs this
    /// workspace receives an rsync'd copy with no `.git`, and the first version of this fix turned
    /// that into a hard failure of the entire suite — a missing index reported as if it were evidence
    /// about workers, which is the same class of dishonesty #787 is about. So:
    ///
    ///   * git present (any developer or agent worktree, and CI — the workflow uses
    ///     `actions/checkout@v4`, so **the merge gate runs index equality**) — per FILE;
    ///   * git absent (the remote builder) — every workspace member read from the root `Cargo.toml`
    ///     must contribute at least one walked file, per MEMBER. It catches the failure that was
    ///     actually measured, because that failure was a whole crate directory dropping out of the
    ///     walk; it cannot catch one missing file.
    ///
    /// **And the member LIST is itself asserted, because round-3 review broke it.** That list used to
    /// be parsed and printed and never compared to anything. A `default-members` key in the manifest
    /// armed the old suffix match, the list collapsed to ONE member, 29 `.rs` files went dark with a
    /// planted production construction among them, and the guard passed while printing
    /// `workspace members checked = 1`. **A number that is printed and never checked is not a
    /// control** — it is the #778 shape one level up, in the reach control's own input. So the parsed
    /// list is now asserted EQUAL to the set of directories that actually contain a `Cargo.toml`
    /// ([`crate_dirs_787`]), which needs no git and does not depend on this parser being right. Both
    /// halves were measured in round 4: with the anchor deliberately removed the parser still
    /// collapses to `{"tools"}`, and the tree is **RED anyway** — `Parsed 1 … on disk 13`.
    ///
    /// **How weak the degraded control is, in numbers.** A corpus satisfying every git-absent check
    /// needs 13 member representatives, plus a 14th file because `eqoxide-nav` carries TWO named
    /// anchors (`walker.rs` and `planner.rs`) and one representative cannot be both, plus the two
    /// anchors that live outside any member (`src/app.rs`, `tests/walker_sim.rs`): **16 of 172
    /// files. Up to 156 — 90.7% — could be dark and every control would still pass.** (A round-3
    /// draft said 15 / 157 / 91%; it counted `eqoxide-nav` once.) That is far worse than the 11.6%
    /// tolerance band round-1 review made me delete, and it is the honest ceiling on what a
    /// builder-only run proves.
    ///
    /// Two things bound it in practice, and neither is a fix. The merge gate has git, so nothing
    /// reaches `main` on the weak path — it is every pre-merge run a developer or agent sees that
    /// takes it. And the guard now prints its mode and its file count **unconditionally, before any
    /// finding**, to the process's real stderr handle — so it appears in a plain `cargo test` log of
    /// a PASSING run, with no `--nocapture` (that flag would be needed only for the `println!` form,
    /// which is exactly the form this deliberately does not use; see the comment at the write site).
    /// A degraded run therefore states its own coverage rather than looking identical to a strong
    /// one. Round-2 review found that disclosure on the failure path
    /// only, which is the #778 property reproduced inside this guard's own reporting: it told the
    /// truth exactly when it was already failing.
    ///
    /// **Why the corpus is still cross-checked against git rather than being the walk alone.**
    /// Measured on two trees that have both: `git ls-files '*.rs'` and the walk produce the **same
    /// 172 files, identical by name**, not merely the same count. So the corpus is already the walk
    /// either way — the union adds nothing — and dropping the index would not simplify the corpus,
    /// it would only delete the one per-FILE reach control the merge gate actually runs. The two
    /// modes are a difference in CHECKING, not in what gets scanned.
    #[test]
    fn exactly_one_production_fine_worker_is_built_in_the_tree_787() {
        let root = repo_root_787();
        assert!(root.join("Cargo.toml").is_file(),
            "layout anchor: expected the workspace manifest at the repo root this guard walks from");
        assert!(root.join("crates/eqoxide-net/src/action_loop.rs").is_file(),
            "layout anchor: the known production construction site has moved — re-point this guard");

        // The corpus is the GIT INDEX unioned with a filesystem walk. Neither alone is right: the
        // index cannot see an untracked file, and a hand-rolled walk can silently skip a subtree —
        // which is precisely what round-1 review measured against this guard's first draft.
        let tracked: Option<std::collections::BTreeSet<String>> =
            git_tracked_rs_787(&root).map(|v| v.into_iter().collect());

        let mut walked_paths = Vec::new();
        rs_files_787(&root, &mut walked_paths);
        let walked: std::collections::BTreeSet<String> = walked_paths.iter()
            .map(|p| p.strip_prefix(&root).unwrap_or(p).to_string_lossy().replace('\\', "/"))
            .collect();

        let members = workspace_members_787(&root);

        // THE MEMBER LIST IS ASSERTED, NOT JUST PRINTED. Round-3 review: the count was reported and
        // never compared, so `default-members = ["tools"]` in the manifest collapsed the per-member
        // control to ONE member — 29 `.rs` files dark with a planted production construction among
        // them — and the guard still passed while printing `members checked = 1`. A number nothing
        // checks is not a control. The filesystem's own answer is the independent oracle here, and it
        // needs no git, which is the whole point of this branch.
        let mut crate_dirs = Vec::new();
        crate_dirs_787(&root, &root, &mut crate_dirs);
        let dirs: std::collections::BTreeSet<String> = crate_dirs.into_iter().collect();
        let listed: std::collections::BTreeSet<String> = members.iter().cloned().collect();
        assert_eq!(listed, dirs,
            "#787 guard reach: the workspace members parsed out of `Cargo.toml` do not match the \
             directories that actually contain a `Cargo.toml`. Whichever side is short, the \
             per-member reach control below is covering less of the tree than it reports. Parsed {} \
             ({:?}); on disk {} ({:?})",
            listed.len(), listed, dirs.len(), dirs);

        // MODE DISCLOSURE, UNCONDITIONAL AND BEFORE ANY FINDING. Round-2 review: the mode was printed
        // only when the guard was already failing, so a PASSING degraded run was byte-identical to a
        // passing strong one — the #778 property reproduced inside this guard's own reporting. It is
        // printed here, ahead of every assert, so it survives a failure too.
        //
        // It goes to the process's REAL stderr handle rather than through `println!`, because libtest
        // captures the macros and shows their output only for tests that FAIL — which would have left
        // the disclosure on the failure path again, in a different disguise. A direct handle write is
        // not captured, so this line is in the default suite log of a PASSING run, which is the run
        // whose strength was previously unknowable. Measured, not assumed.
        {
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(),
            "#787 guard: reach control = {}; walked {} .rs file(s); git index available = {} \
             ({} tracked); workspace members checked = {}",
            if tracked.is_some() { "INDEX EQUALITY, per FILE" } else { "PER WORKSPACE MEMBER only (weaker)" },
            walked.len(),
            tracked.is_some(),
            tracked.as_ref().map(|t| t.len()).unwrap_or(0),
            members.len(),
            );
        }

        // REACH CONTROL 1 — EQUALITY against the git index, not a tolerance. Every tracked `.rs` file
        // must have been reached by the walk. The old form was `files.len() >= 150`, and review
        // measured a 20-file crate (11.6% of the tree, worst case 22 / 12.8%) vanishing under that
        // band with every other control green and a planted production site inside it invisible. A
        // floor 13% below the true value is not a reach control.
        //
        // This control is UNAVAILABLE where there is no git checkout — the remote builder runs the
        // suite from an rsync'd copy. Control 1b below is what runs there, and it is weaker.
        if let Some(tracked) = tracked.as_ref() {
            let unwalked: Vec<&String> = tracked.difference(&walked).collect();
            assert!(unwalked.is_empty(),
                "#787 guard reach: the filesystem walk missed {} tracked file(s) the git index lists \
                 — it skipped a subtree, so any 'exactly one' finding covers less of the tree than \
                 it claims. Walked {} of {} tracked:\n  {:?}",
                unwalked.len(), walked.len(), tracked.len(), unwalked);
        }

        // REACH CONTROL 1b — git-free, and the only reach control in force on the remote builder.
        // Every workspace member must contribute at least one walked `.rs` file. It cannot see a
        // single missing file the way control 1 can; it CAN see the failure that was actually
        // measured (a whole crate directory dropping out of the walk), and it needs no index.
        for member in &members {
            let prefix = format!("{member}/");
            let n = walked.iter().filter(|f| f.starts_with(&prefix)).count();
            assert!(n > 0,
                "#787 guard reach: workspace member `{member}` contributed ZERO files to the walk — \
                 a whole crate is dark, so 'exactly one production construction' is a claim about \
                 less of the tree than it sounds like. git index available: {}. Walked {} file(s). \
                 Members checked: {}.",
                tracked.is_some(), walked.len(), members.len());
        }

        // Scan the UNION where the index is available. Untracked strays are included deliberately: an
        // unmarked construction in one is a false RED, which is the safe direction and is disclosed
        // in this test's rustdoc.
        let files: Vec<String> = match tracked.as_ref() {
            Some(t) => t.union(&walked).cloned().collect(), // BTreeSet → sorted
            None => walked.iter().cloned().collect(),
        };

        // REACH CONTROL 2 — named anchors. Redundant against control 1 for the walk, but it is what
        // catches "this guard is running against the wrong tree entirely" (a re-layout, a vendored
        // copy). All four files carrying the four dependent sentences are in the list.
        for anchor in [
            "crates/eqoxide-net/src/action_loop.rs",
            "crates/eqoxide-nav/src/walker.rs",
            "crates/eqoxide-nav/src/planner.rs",
            "crates/eqoxide-ipc/src/lib.rs",
            "crates/eqoxide-http/src/observe.rs",
            "src/app.rs",
            "tests/walker_sim.rs",
        ] {
            assert!(files.iter().any(|f| f == anchor),
                "#787 guard reach: `{anchor}` is not in the corpus — this guard is looking at the \
                 wrong tree. Corpus: {} file(s).", files.len());
        }

        // Needles assembled at run time so this guard's own source does not contain the text it
        // searches for (`slot.rs` names the same self-match hazard).
        let walker_new = format!("Walker::{}(", "new");
        let fine_spawn = format!("LocalPlanner::{}(", "spawn");

        let mut prod_walker: Vec<String> = Vec::new(); // "path:line: code"
        let mut prod_spawn:  Vec<String> = Vec::new();
        let mut marked = 0usize;
        let mut prose = 0usize;
        let mut aliases: Vec<String> = Vec::new();

        for rel in &files {
            let path = root.join(rel);
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let raw: Vec<&str> = text.lines().collect();

            // The `as`-rename hole. A renamed import defeats the needle below, so it is a failure in
            // its own right. The leading visibility MUST be stripped first: round-1 review measured
            // `use … as X;` going RED and `pub use … as X;` going GREEN off the same tree. A
            // re-export — the form that actually matters — always leads with a visibility, and `pub`
            // is only the shortest of them, which is why the parenthesised forms (`pub(crate)`,
            // `pub(super)`, `pub(in path)`) are stripped below rather than assumed away.
            for (n, line) in raw.iter().enumerate() {
                let mut code = line.trim_start();
                if let Some(rest) = code.strip_prefix("pub") {
                    let rest = rest.trim_start();
                    // `pub(crate)` / `pub(super)` / `pub(in path)` — drop the parenthesised part.
                    code = if rest.starts_with('(') {
                        rest.split_once(')').map(|(_, r)| r.trim_start()).unwrap_or(rest)
                    } else {
                        rest
                    };
                }
                if code.starts_with("use ")
                    && (code.contains("Walker as ") || code.contains("LocalPlanner as "))
                {
                    aliases.push(format!("{rel}:{}: {}", n + 1, line.trim()));
                }
            }

            // TWO passes over every file: the raw text, and the same text with comments blanked.
            // The second is what sees `Walker /* x */ ::new(` — valid Rust that whitespace-only
            // flattening never forms the needle from (A5, measured). Findings are de-duplicated by
            // `path:line`, so a call visible to both passes is reported once. Classification always
            // reads the RAW line, so a `// #787-NOT-PRODUCTION` marker still marks its site even in
            // the pass where comments are gone.
            let blanked = blank_comments_787(&text);
            // (is_spawn, line) → how many matches that line carries. The two passes are combined by
            // MAX per line, not by set-union, so two constructions on ONE line still count as two.
            let mut hits: std::collections::BTreeMap<(bool, usize), usize> = Default::default();
            for source in [&text, &blanked] {
                let (flat, line_of) = flatten_787(source);
                let mut pass: std::collections::BTreeMap<(bool, usize), usize> = Default::default();
                for (is_spawn, needle) in [(false, &walker_new), (true, &fine_spawn)] {
                    for (off, _) in flat.match_indices(needle.as_str()) {
                        *pass.entry((is_spawn, line_of[off])).or_default() += 1;
                    }
                }
                for (k, v) in pass {
                    let e = hits.entry(k).or_default();
                    *e = (*e).max(v);
                }
            }
            for ((is_spawn, ln), n) in hits {
                let line = raw[ln - 1];
                let code = line.trim_start();
                if code.starts_with("//") {
                    prose += n; // doc/comment mentions, not constructions
                } else if line.contains(NOT_PRODUCTION) {
                    marked += n;
                } else {
                    let entry = format!("{rel}:{ln}: {}", code.trim_end());
                    for _ in 0..n {
                        if is_spawn { prod_spawn.push(entry.clone()) } else { prod_walker.push(entry.clone()) }
                    }
                }
            }
        }

        assert!(aliases.is_empty(),
            "{}",
            four_sentences(&format!(
                "#787: a `use` statement RENAMES `Walker` or `LocalPlanner`, which hides fine-worker \
                 construction from the guard that pins the one-worker premise:\n  {}\n\nImport the \
                 type under its own name, or teach this guard the alias.",
                aliases.join("\n  "))));

        // POSITIVE CONTROL — if the marker path ever counts zero, the needle has drifted from the
        // source and the "exactly one" finding below is vacuous rather than true.
        assert!(marked >= 7,
            "#787 guard: matched only {marked} marked construction site(s); at least 7 are expected \
             (the 4 `Walker` fixtures in this file and the 3 `LocalPlanner` fixtures in planner.rs). \
             The search pattern has drifted from the source and this guard has stopped measuring \
             anything. (prose mentions seen: {prose})");

        for (what, where_, found) in [
            ("`Walker` construction, expected only in `ActionLoop::new`",
             "crates/eqoxide-net/src/action_loop.rs:", &prod_walker),
            ("fine-worker `LocalPlanner::spawn`, expected only inside `Walker::new`",
             "crates/eqoxide-nav/src/walker.rs:", &prod_spawn),
        ] {
            assert_eq!(found.len(), 1,
                "{}",
                four_sentences(&format!(
                    "#787: expected exactly ONE production {what}; found {} site(s):\n  {}",
                    found.len(), found.join("\n  "))));
            assert!(found[0].starts_with(where_),
                "{}",
                four_sentences(&format!(
                    "#787: the sole production {what} has MOVED — it is now `{}`, not in `{}`. That \
                     is not by itself a second worker, but the premise chain the four sentences \
                     below rest on is written in terms of the old location.",
                    found[0], where_.trim_end_matches(':'))));
        }
    }

    /// **#766, the OTHER walker route to `idle` — and it is here to MEASURE a claim, not to guard a
    /// line.** The fix's rationale (in `NavStatus::retire_to_idle`'s doc comment and this PR's body)
    /// says four of the six routes to `idle` already cleared `local` before #766, two of them —
    /// `goal_dropped` and `respawned` — because `resolve_goal`'s no-goto branch calls
    /// `clear_local_plan()` on the same tick, *before* it retires. That was read off the source, and
    /// a mechanism claim read off the source is exactly the kind this project keeps getting wrong,
    /// so it is run here instead.
    ///
    /// **Deliberately NOT mutation-pinned.** Deleting `*local = None;` from `retire_to_idle` leaves
    /// this GREEN, because `clear_local_plan()` gets there first — that is the whole finding. Read
    /// it as a measurement of the pre-existing behaviour and a regression guard on this route, not
    /// as evidence for the fix.
    #[test]
    fn the_goal_dropped_route_already_cleared_the_fine_verdict_before_766() {
        let (mut w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        let gs = eqoxide_core::game_state::GameState::new();
        *nav.goto_target.lock().unwrap() = Some((10.0, 20.0, 3.0));
        w.set_nav_state_because("following", None);
        w.set_nav_local(Some(eqoxide_ipc::NavLocal {
            state: "no_way_through".into(), reason: "search_closed".into(),
            stuck_ticks: 7, plan_us: 1234,
        }));
        assert!(nav.nav_state.lock().unwrap().local.is_some(),
            "PREMISE: the verdict is loaded, and `following` is non-terminal so the tick below \
             genuinely reaches the retirement branch");

        *nav.goto_target.lock().unwrap() = None; // the leader despawned
        assert!(w.resolve_goal(&gs).is_none());

        let s = nav.nav_state.lock().unwrap();
        assert_eq!(s.reason.as_deref(), Some(NAV_REASON_GOAL_DROPPED));
        assert_eq!(s.local, None,
            "#766: this route was never the leaky one — `clear_local_plan()` runs on the same tick. \
             Measured here so the PR's four-of-six claim is not just read off the source.");
    }

    /// **#725 review round 3: the writer-level guard itself is pinned.** The test above is a
    /// PER-CALL-SITE assertion, and the round-3 review measured what that class of test cannot do:
    /// a brand-new reasonless `idle` on a path nobody wrote an assertion for (`perform_cross`'s
    /// cross-zone branch, i.e. the `/v1/move/zone_cross` SUCCESS path) left the whole suite green,
    /// and the docs↔constants pin in `eqoxide-net` cannot see it either — a reasonless publish adds
    /// no constant to the list it compares. The `debug_assert!` in `set_nav_state_because` is what
    /// closes that, so it needs its own test: without this, deleting the assert is silent.
    ///
    /// Debug-only by construction — `debug_assert!` compiles out under `--release`, so the guard is
    /// a TEST-TIME instrument, not a runtime one. That is the honest scope of the claim: it fails
    /// the suite for anyone who adds a reasonless `idle`; it does not police a shipped binary.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "#725 B1")]
    fn a_reasonless_idle_is_refused_by_the_writer_not_just_by_a_per_call_site_test_725() {
        let (w, _nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        w.set_nav_state("idle"); // the exact shape of B1's original defect, at the chokepoint
    }

    /// The other side of it: the guard must be about `idle` SPECIFICALLY, not about a missing reason
    /// in general. Every in-progress state is legitimately reasonless — `pending` needs no
    /// explanation, the request itself is the explanation — so an over-broad assert would fire on
    /// the normal path. Mutation check: widen the guard to `reason.is_none()` and this goes RED.
    #[test]
    fn a_reasonless_in_progress_state_is_still_allowed_725() {
        let (w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        w.set_nav_state("pending");
        assert_eq!(nav.nav_state.lock().unwrap().reason, None);
    }

    /// #644: the honest terminal `dead` state must NOT become a new never-clearing observable — once
    /// the character has RESPAWNED (so the tick reaches `resolve_goal` again) and there is no active
    /// goto, it must retire back to plain `idle`. Mutation check: drop `NAV_STATE_DEAD` from the
    /// reset list in `resolve_goal` and this goes RED (the state stays stuck at `dead` after respawn).
    #[test]
    fn dead_nav_state_clears_to_idle_on_respawn() {
        let (mut w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        let gs = eqoxide_core::game_state::GameState::new(); // alive (cur_hp/max_hp both 0 = unknown, not dead)
        // Simulate the post-death published state: terminal `dead`, no active goto.
        w.set_nav_state_because(NAV_STATE_DEAD, Some("player_dead"));
        *nav.goto_target.lock().unwrap() = None;
        assert_eq!(nav.nav_state.lock().unwrap().state, "dead");
        // A respawned (live) player's tick reaches resolve_goal; with no goto it retires `dead`→`idle`.
        assert!(w.resolve_goal(&gs).is_none());
        assert_eq!(nav.nav_state.lock().unwrap().state, "idle",
            "#644: `dead` must clear to `idle` on respawn, not linger forever");
        assert_eq!(nav.nav_state.lock().unwrap().reason.as_deref(), Some(NAV_REASON_RESPAWNED),
            "#725: and it says WHY — 'idle because you came back', not the ambiguous bare idle");
    }

    // ───────────── #725: no in-progress nav_state may outlive the goal behind it ─────────────

    /// **THE UNIVERSAL CLAIM, argued over the whole input space rather than by run count.**
    ///
    /// The claim `resolve_goal` has to support is *"no `nav_state` that asserts work is in flight
    /// can survive a tick with no goto goal and no queued zone-cross"*. That is a statement about
    /// every state word, so testing the two that were observed to stick (`pending`, `following`)
    /// would restate the bug, not the invariant. The input space here is the state vocabulary
    /// itself: every word `docs/http-api.md` documents, plus words no one has written yet — which
    /// is the case that matters, because the defect was a state nobody remembered to list.
    ///
    /// The exhaustive half is real: `set_nav_state_because` accepts any `&str`, so the space of
    /// state words is unbounded, and the property is checked against representatives of both
    /// classes AND against arbitrary unrecognised words, which must retire (fail-safe by default).
    ///
    /// **Mutation check:** restore the old opt-in list
    /// (`navigating || navigating_partial || planning || (zone_loading && !pending) || dead`)
    /// → `pending`, `following` and both unknown
    /// words stay put and this goes RED four times over. Alternatively add `"pending"` to
    /// [`TERMINAL_NAV_STATES`] → RED, since a terminal `pending` is precisely the lie.
    #[test]
    fn no_in_progress_nav_state_survives_a_tick_with_no_goal_725() {
        // The documented vocabulary, plus two words the codebase has never published — a future
        // in-progress state, and a typo'd one. Both must retire: unrecognised ⇒ in-progress.
        const IN_PROGRESS: [&str; 8] = [
            "pending", "planning", "navigating", "navigating_partial", "navigating_stalled",
            "following", "crossing_a_state_invented_next_year", "navigatng",
        ];
        for state in IN_PROGRESS {
            let (mut w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
            let gs = eqoxide_core::game_state::GameState::new();
            w.set_nav_state(state);
            assert!(nav.goto_target.lock().unwrap().is_none() && nav.zone_cross.lock().unwrap().is_none(),
                "{state}: the premise is a tick with NOTHING in flight");

            assert!(w.resolve_goal(&gs).is_none());

            let s = nav.nav_state.lock().unwrap();
            assert_eq!(s.state, "idle",
                "#725: `{state}` claims work is in flight; with no goal and no queued cross there is \
                 none, so it must retire — an unlisted word sticking forever is the whole defect");
            assert_eq!(s.reason.as_deref(), Some(NAV_REASON_GOAL_DROPPED),
                "{state}: and the retirement must be explained — a bare `idle` cannot be told apart \
                 from 'ready for work', which is what an agent polling for its goal needs to know");
        }

        // The other half of the property: a TERMINAL word is an answer about a goal that is already
        // over, so the same tick must leave it exactly alone — retiring it would destroy the outcome
        // the agent is polling for (#349's stale-answer bug, pointing the other way).
        for state in TERMINAL_NAV_STATES {
            let (mut w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
            let gs = eqoxide_core::game_state::GameState::new();
            w.set_nav_state_because(state, Some("some_prior_reason"));

            assert!(w.resolve_goal(&gs).is_none());

            let s = nav.nav_state.lock().unwrap();
            assert_eq!((s.state.as_str(), s.reason.as_deref()), (state, Some("some_prior_reason")),
                "`{state}` is a finished outcome — an idle tick must not overwrite the answer the \
                 caller is waiting to read");
        }
    }

    /// **The #725 primary defect, in the walker's own terms.** `request_zone_cross` stamps
    /// `pending` and queues a one-shot request; `drain_zone_cross` DRAINS that request. If the drain
    /// then resolves to nothing, the slot is empty and no goto goal exists, so — before this fix —
    /// no code path could ever move `pending` again. Live, it read `pending` / `nav_reason: null` /
    /// `nav_goal: null` for 75 s while the docs said that meant "in flight".
    ///
    /// The queued-cross case is the deliberate exception and is pinned here too: while the request
    /// is STILL in its slot, `pending` is a true statement and must hold. **This half caught a real
    /// bug in the first draft of the fix**, which held only `zone_loading` while a cross was queued
    /// (the #600 shape) and therefore retired `pending`→`idle` in the window between the HTTP
    /// thread's accept and the net thread's drain — reporting "your request was dropped" about a
    /// crossing the client was about to perform. Hence the guard keys on the QUEUED REQUEST, not on
    /// the state word.
    ///
    /// **Mutation check:** revert `resolve_goal`'s retirement rule to the old opt-in list → the
    /// post-drain assertion goes RED (`pending` stands forever). Narrow `zone_cross_pending` back to
    /// `current == NAV_STATE_ZONE_LOADING && zone_cross_pending` → the still-queued assertion goes
    /// RED (`idle` at the accept). Drop the guard entirely → the same assertion goes RED.
    #[test]
    fn pending_retires_once_the_zone_cross_request_has_been_drained_725() {
        let (mut w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        let gs = eqoxide_core::game_state::GameState::new();

        // The accept: `pending` + the one-shot request in its slot (what `request_zone_cross` does).
        w.set_nav_state("pending");
        *nav.zone_cross.lock().unwrap() = Some(30);
        assert!(w.resolve_goal(&gs).is_none());
        assert_eq!(nav.nav_state.lock().unwrap().state, "pending",
            "while the request is still queued, `pending` is TRUE — it must not be retired early");

        // The drain: the request leaves the slot. Whatever the drainer decided, `pending` is now a
        // claim about something that no longer exists anywhere.
        *nav.zone_cross.lock().unwrap() = None;
        assert!(w.resolve_goal(&gs).is_none());
        let s = nav.nav_state.lock().unwrap();
        assert_eq!(s.state, "idle",
            "#725: once the request is out of its slot nothing downstream can retire `pending` — \
             the tick that observes no goal and no queued cross is the last chance to be honest");
        assert_eq!(s.reason.as_deref(), Some(NAV_REASON_GOAL_DROPPED));
    }

    /// **The second instance of the same class, found while fixing the first (#725).** `following`
    /// is published by the `FollowHold` arm while a `/v1/move/follow` chase holds near its leader.
    /// When that leader despawns, `drive_chase` clears BOTH nav slots — and `following` was not on
    /// the old retirement list, so it stood forever over a chase with nothing left to chase. An
    /// agent polling a follow it had issued read `following` indefinitely for an entity that no
    /// longer exists.
    ///
    /// Driven through the REAL `drive_chase` (not a hand-cleared slot), so the despawn path is the
    /// one production takes.
    ///
    /// **Mutation check:** revert `resolve_goal` to the opt-in list → `following` persists and this
    /// goes RED. (On unmodified `main` it is RED, which is the point: this is a shipped bug, and it
    /// was found by inverting the rule rather than by observing it.)
    #[test]
    fn following_retires_when_the_chased_entity_despawns_725() {
        let (mut w, nav, _intent, _view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        let gs = eqoxide_core::game_state::GameState::new();

        // An established chase, holding near its leader.
        *nav.goto_entity.lock().unwrap() = Some("a_large_rat".to_string());
        *nav.goto_target.lock().unwrap() = Some((10.0, 10.0, 0.0));
        w.set_nav_state("following");

        // The leader despawns: it is absent from the (empty) entity roster, so the real `drive_chase`
        // drops the chase.
        w.drive_chase();
        assert!(nav.goto_target.lock().unwrap().is_none() && nav.goto_entity.lock().unwrap().is_none(),
            "premise: drive_chase abandons a chase whose entity left view");

        assert!(w.resolve_goal(&gs).is_none());
        let s = nav.nav_state.lock().unwrap();
        assert_eq!(s.state, "idle",
            "#725 (second instance): `following` must not outlive the entity being followed");
        assert_eq!(s.reason.as_deref(), Some(NAV_REASON_GOAL_DROPPED));
    }

    /// **#615 review F1 — the idle snapshot must TRACK reality, never fabricate it.** The live
    /// finding: a fresh login published `player: [0,0,0]` (985 units from the character) with
    /// `zone_model_loaded: false`, and the idle walker never republished — a confident wrong
    /// position with no age and no hedge, forever. Pins all four halves of the fix:
    /// unknown position publishes `None` (never an invented origin); a known position republishes
    /// on movement; `zone_model_loaded` republishes when assets land; and a genuinely idle walker
    /// does NOT churn (seq stable).
    #[test]
    fn idle_snapshot_tracks_player_and_world_and_never_fabricates_a_position() {
        let (mut w, _nav, _intent, view) = walker_with(open_plane(200.0));
        let mut gs = eqoxide_core::game_state::GameState::new();
        assert!(!gs.player_pos_known, "fixture premise: a fresh GameState has no known position");

        // 1. No goto, position UNKNOWN: the settled publish must say None — not [0,0,0].
        assert!(w.resolve_goal(&gs).is_none());
        let snap = view.lock().unwrap().clone().expect("the idle state must be published");
        assert_eq!(snap.player, None,
            "an unknown position must publish as None — [0,0,0] was the #615-F1 confident lie");
        assert!(snap.zone_model_loaded, "the collision grid is loaded and must be reported so");

        // 2. The server places us: the next idle tick must republish the REAL position.
        gs.player_pos_known = true;
        gs.player_x = 398.9; gs.player_y = 899.1; gs.player_z = 12.0;
        assert!(w.resolve_goal(&gs).is_none());
        let snap = view.lock().unwrap().clone().unwrap();
        assert_eq!(snap.player, Some([398.9, 899.1, 12.0]),
            "the idle snapshot must track where the character actually is");

        // 3. Genuinely idle: no republish churn.
        let seq_before = snap.seq;
        for _ in 0..3 { assert!(w.resolve_goal(&gs).is_none()); }
        assert_eq!(view.lock().unwrap().clone().unwrap().seq, seq_before,
            "an unchanged idle state must not republish every tick");

        // 4. The character moves (WASD / server push — no goto involved): republish.
        gs.player_x = 350.0;
        assert!(w.resolve_goal(&gs).is_none());
        let snap = view.lock().unwrap().clone().unwrap();
        assert!(snap.seq > seq_before, "movement must republish");
        assert_eq!(snap.player, Some([350.0, 899.1, 12.0]));
    }

    /// GLB-space quad (`positions` are `[north, up, east]`) — the synthetic-fixture pattern the
    /// planner/traversability tests use: hand-built geometry with known-correct answers, no baked
    /// assets, CI-safe.
    fn quad(v: Vec<[f32; 3]>) -> eqoxide_assets::MeshData {
        eqoxide_assets::MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: eqoxide_assets::RenderMode::Opaque, anim: None,
        }
    }

    fn open_plane(half: f32) -> crate::collision::SharedCollision {
        let terrain = vec![quad(vec![
            [-half, 0.0, -half], [half, 0.0, -half], [half, 0.0, half], [-half, 0.0, half],
        ])];
        let col = crate::collision::Collision::build(
            &eqoxide_assets::ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0);
        Arc::new(std::sync::RwLock::new(Some(Arc::new(col))))
    }

    /// **THE #246/#608 PUBLISH PROPERTY.** Once a real plan lands, the published snapshot's
    /// `committed_coarse` IS the walker's own `path` — the route it actually follows — and the
    /// snapshot carries the planner's own record of the plan (outcome + a trace whose accepted
    /// edges exist). No consumer input goes anywhere near this: the walker is the only writer.
    ///
    /// Mutation-checked at authoring time: publishing an empty/fabricated route in
    /// `publish_debug` instead of `self.path` turns this RED.
    #[test]
    fn published_snapshot_carries_the_walkers_actual_committed_route_and_the_plan_trace() {
        use crate::diagnostics::{EdgeKind, EdgeVerdict};
        let (mut w, nav, _intent, view) = walker_with(open_plane(400.0));
        let mut gs = eqoxide_core::game_state::GameState::new();
        gs.world.zone_name = TEST_ZONE.into(); // #600: match the loaded zone so `usability` permits routing
        gs.player_x = -300.0; gs.player_y = 0.0; gs.player_z = 0.0;
        gs.player_pos_known = true;
        *nav.goto_target.lock().unwrap() = Some((300.0, 0.0, 0.0));

        // Tick until the worker's plan lands and the walker commits a route.
        let mut committed = false;
        for _ in 0..2000 {
            w.drive_walk(&mut gs, (300.0, 0.0, 0.0));
            if !w.path.is_empty() { committed = true; break; }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(committed, "an open-plane goal must plan and commit a route");

        let snap = view.lock().unwrap().clone().expect("a snapshot must be published");
        assert!(snap.zone_model_loaded);
        assert_eq!(snap.committed_coarse, w.path,
            "the published committed route must BE the walker's own path — byte-for-byte (#246)");
        assert_eq!(snap.goal, Some([300.0, 0.0, 0.0]));

        let plan = snap.plan.as_ref().expect("the plan record must be published");
        assert_eq!(plan.outcome, "route");
        assert_eq!(plan.reason, "route");
        assert!(plan.route_len >= 2);
        assert_eq!(plan.goal, [300.0, 0.0, 0.0]);
        // The planner's own trace: at least one call, accepted Walk edges present, and the
        // outcome-call range points into `calls`.
        assert!(!plan.trace.calls.is_empty(), "the coarse worker must arm the edge trace");
        let (o0, o1) = plan.trace.outcome_calls;
        assert!(o0 < o1 && o1 <= plan.trace.calls.len(),
            "outcome_calls {:?} must be a valid range into {} calls", plan.trace.outcome_calls, plan.trace.calls.len());
        let accepted_walks = plan.trace.calls[o0..o1].iter()
            .flat_map(|c| &c.edges)
            .filter(|e| matches!(e.verdict, EdgeVerdict::Accepted { kind: EdgeKind::Walk }))
            .count();
        assert!(accepted_walks > 0, "an open-plane route must have accepted walk edges in its trace");
    }

    // ─────────────────────────── #631 gap 1: the plan is attributable ───────────────────────────

    /// **#631 gap 1: a failed goto's diagnostic must never masquerade as the CURRENT command's
    /// outcome.** A `PlanDebug` survives route clears (it is the diagnostic OF a failure), so after a
    /// `/stop` (or a fresh goto) the previous goal's plan keeps riding the snapshot. Live-reproduced
    /// on the current binary: after a failed goto then `/stop`, `nav_debug.plan.gen`/`outcome`
    /// described the SUPERSEDED goal with no goal_id to tell it apart — the plan read as this
    /// command's result. The fix stamps the plan with the goal_id it was FOR and the snapshot with the
    /// LIVE goal_id, so a stale plan is self-identifying (`plan.goal_id != snapshot.goal_id`).
    ///
    /// Mutation check: in `apply_plan` stamp `goal_id: 0` (or the live goal_id) instead of
    /// `self.plan_goal_id`, or in `publish_debug` publish a constant goal_id — either collapses the
    /// two identities and the `assert_ne!` below goes RED (the exact reproduced masquerade returns).
    #[test]
    fn a_superseded_failed_plan_is_attributable_to_its_own_goal_id_not_the_current_command() {
        use crate::collision::{NoRoute, PlanOutcome};
        let (mut w, nav, _intent, view) = walker_with(open_plane(200.0));
        let mut gs = eqoxide_core::game_state::GameState::new();
        gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0; gs.player_pos_known = true;

        // Goal A, accepted as goal_id 1, posted under that id; the worker returns a DEFINITIVE no.
        nav.nav_state.lock().unwrap().goal_id = 1;
        *nav.goto_target.lock().unwrap() = Some((999.0, 999.0, 0.0));
        w.plan_goal_id = 1;
        let reply = crate::planner::PlanReply {
            gen: 5, start: [0.0; 3], goal: [999.0, 999.0, 0.0],
            outcome: PlanOutcome::Unreachable {
                reason: NoRoute::SearchClosed, goal_blocked_by: None, frontier_blocked_by: None },
            plan_ms: 3, goal_snapped: None, tight: false,
            trace: crate::diagnostics::SearchTrace::default(),
        };
        assert!(w.apply_plan(reply, &mut gs, (999.0, 999.0, 0.0)),
            "a definitive no must stop the tick");
        assert!(w.nav_state_is("no_path"), "goal A fails honestly under its own id");

        // The agent SUPERSEDES: a fresh command bumps the goal_id (a /stop → idle here). The failure
        // diagnostic is retained, as designed — but it is now the PREVIOUS goal's.
        {
            let mut s = nav.nav_state.lock().unwrap();
            s.goal_id = 2;
            s.state = "idle".into();
        }
        *nav.goto_target.lock().unwrap() = None;
        w.publish_debug(Walker::known_pos(&gs), None);

        let snap = view.lock().unwrap().clone().expect("a snapshot must be published");
        assert_eq!(snap.goal_id, 2, "the snapshot carries the CURRENT command's identity");
        let plan = snap.plan.as_ref().expect("the failure diagnostic is retained");
        assert_eq!(plan.goal_id, 1, "the retained plan names the goal it was actually FOR");
        assert_ne!(plan.goal_id, snap.goal_id,
            "#631 gap 1: a superseded plan MUST be distinguishable from the current command's outcome \
             — its goal_id differs from the live one, so `plan.gen`/`plan.outcome` can never be read \
             as this command's result (the reproduced idle+stale-gen masquerade)");
    }

    /// **#631 gap 2, wiring: `apply_plan` records the horizontal shortfall on the published plan.**
    /// A partial route (`Exhausted`) that stops short of the goal must publish a nonzero `goal_offset`
    /// equal to its committed endpoint's horizontal distance from the requested goal — the honest
    /// "your named coords are not where I'm headed, by this much", which `goal_snapped: false` alone
    /// hid. (The pure math is pinned in `steering::route_goal_offset_reports_horizontal_shortfall_only`.)
    ///
    /// Mutation check: hard-code `goal_offset: 0.0` in `apply_plan` and the `> 40.0` assertion goes RED.
    #[test]
    fn apply_plan_publishes_the_horizontal_goal_offset_for_a_partial() {
        use crate::collision::{PlanLimit, PlanOutcome};
        let (mut w, nav, _intent, view) = walker_with(open_plane(400.0));
        let mut gs = eqoxide_core::game_state::GameState::new();
        gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0; gs.player_pos_known = true;
        nav.nav_state.lock().unwrap().goal_id = 1;
        *nav.goto_target.lock().unwrap() = Some((300.0, 0.0, 0.0));
        w.plan_goal_id = 1;

        // A partial route whose far end is 55u (horizontally) short of the goal (the #482 shape).
        let partial = vec![[0.0, 0.0, 0.0], [40.0, -30.0, 0.0], [245.0, 0.0, 0.0]];
        let reply = crate::planner::PlanReply {
            gen: 9, start: [0.0; 3], goal: [300.0, 0.0, 0.0],
            outcome: PlanOutcome::Exhausted { limit: PlanLimit::NodeCap, progress: Some(partial) },
            plan_ms: 4, goal_snapped: None, tight: false,
            trace: crate::diagnostics::SearchTrace::default(),
        };
        w.apply_plan(reply, &mut gs, (300.0, 0.0, 0.0));
        w.publish_debug(Walker::known_pos(&gs), None);
        let snap = view.lock().unwrap().clone().unwrap();
        let plan = snap.plan.as_ref().expect("the partial plan is published");
        assert!(!plan.goal_snapped, "no vertical snap here — the old channel says nothing");
        assert!((plan.goal_offset - 55.0).abs() < 1.0,
            "#631 gap 2: the ~55u horizontal shortfall to the committed endpoint must be disclosed, \
             got {}", plan.goal_offset);
    }

    // ────────────────────────── #631 gap 3: route-level no-progress ──────────────────────────

    /// A walker with a committed route whose closest approach has not improved for the whole window
    /// TERMINATES honestly (`blocked` / `no_progress`) instead of reporting `navigating` forever —
    /// the moving-but-going-nowhere case the `stuck_ticks` detector (which only watches `path_i`)
    /// misses. Drives the real `drive_walk` tick with the no-progress clock already expired.
    ///
    /// Represents the real moat: a re-planned PARTIAL route (`navigating_partial`) whose `path_i` is
    /// ADVANCING (a lap) but whose closest approach never improves. `path_i` advancement must NOT
    /// count as progress here — that is the whole reason channel (a) is gated on a COMPLETE route —
    /// so the walker still terminates honestly (`blocked` / `no_progress`).
    ///
    /// Mutation check: delete the no-progress block in `drive_walk` and this goes RED. Also: drop the
    /// `nav_state_is("navigating")` gate on channel (a) (so a partial's `path_i` advance counts as
    /// progress) and this ALSO goes RED — the lap would reset the clock and never terminate, exactly
    /// the moat regression the gate exists to prevent.
    #[test]
    fn drive_walk_terminates_no_progress_on_an_advancing_partial_lap() {
        let (mut w, nav, _intent, _view) = walker_with(open_plane(600.0));
        let mut gs = eqoxide_core::game_state::GameState::new();
        gs.world.zone_name = TEST_ZONE.into(); // #600: loaded-zone match
        gs.player_x = 10.0; gs.player_y = 0.0; gs.player_z = 0.0; gs.player_pos_known = true;
        let goal = (500.0, 0.0, 0.0); // far → ArrivalAction::Drive
        *nav.goto_target.lock().unwrap() = Some(goal);
        // A PARTIAL route (a moat lap), NOT a complete route to the goal. (#851: the fact lives on
        // `committed` now — the published word is derived from it, not the other way round.)
        w.committed = Some(committed_partial());
        w.set_nav_state("navigating_partial");
        w.path = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [20.0, 0.0, 0.0], [30.0, 0.0, 0.0]];
        w.path_i = 1;   // ADVANCING along the lap (path_i 1 > stuck_i 0) — looks like motion...
        w.stuck_i = 0;
        w.path_goal = Some(goal);
        // ...but closest approach has been stuck at ~10u for a full window: the lap never closes.
        w.nav_best_g3d = 10.0;
        w.nav_progress_at = std::time::Instant::now() - (NAV_NO_PROGRESS_WINDOW + std::time::Duration::from_secs(1));

        w.drive_walk(&mut gs, goal);

        assert!(w.nav_state_is("blocked"),
            "an advancing PARTIAL lap that never closes on the goal must still terminate — path_i \
             advancement on a partial is not goal-ward progress");
        assert_eq!(nav.nav_state.lock().unwrap().reason.as_deref(), Some("no_progress"),
            "#631 gap 3: the terminal reason must be the distinct `no_progress`");
        assert!(nav.goto_target.lock().unwrap().is_none(), "a terminal stop clears the goto");
    }

    /// **THE REVIEWER'S FALSE-FIRE REPRO (#631 gap 3, changes-requested): a legitimate long go-around
    /// whose START is the closest straight-line point to the goal must NEVER be killed while the
    /// walker is advancing its COMPLETE committed route** — even though closest-approach makes no
    /// improvement for the whole away-leg and the walker is currently far (straight-line) from the
    /// goal. The prior code used only the all-time-closest signal, so a >60s go-around (start-is-
    /// closest) terminated `no_progress` mid-journey, ~12s before arrival — a confident falsehood on a
    /// route that was getting there.
    ///
    /// FAILS ON THE PRIOR CODE (only the closest-approach channel → the expired clock + no improvement
    /// → `blocked`); PASSES AFTER the committed-route channel is added. Mutation check: delete the
    /// `advancing_complete_route` term and this goes RED (the go-around is killed again).
    #[test]
    fn drive_walk_never_terminates_a_complete_route_the_walker_is_advancing_even_when_far_from_goal() {
        let (mut w, nav, _intent, _view) = walker_with(open_plane(2000.0));
        let mut gs = eqoxide_core::game_state::GameState::new();
        // The walker is on its go-around, currently FAR (straight-line) from the goal — the peak of
        // the away-leg — and standing on waypoint 3 of a long COMPLETE route.
        gs.world.zone_name = TEST_ZONE.into(); // #600: loaded-zone match
        gs.player_x = 500.0; gs.player_y = 800.0; gs.player_z = 0.0; gs.player_pos_known = true;
        let goal = (1000.0, 0.0, 0.0);
        *nav.goto_target.lock().unwrap() = Some(goal);
        w.committed = Some(committed_complete()); // #851: a COMPLETE route, not a partial
        w.set_nav_state("navigating");
        w.path = vec![
            [0.0, 0.0, 0.0], [200.0, 400.0, 0.0], [400.0, 700.0, 0.0], [500.0, 800.0, 0.0],
            [700.0, 600.0, 0.0], [900.0, 200.0, 0.0], [1000.0, 0.0, 0.0],
        ];
        w.path_i = 3;    // advanced along the route...
        w.stuck_i = 2;   // ...past this route's previous max → advancing_complete_route
        w.path_goal = Some(goal);
        // The START was the closest straight-line point: best was set low (~50u) and has NOT improved
        // since — the away-leg makes closest-approach worse, and the window has fully elapsed.
        w.nav_best_g3d = 50.0;
        w.nav_progress_at = std::time::Instant::now() - (NAV_NO_PROGRESS_WINDOW + std::time::Duration::from_secs(30));

        w.drive_walk(&mut gs, goal);

        assert!(!w.nav_state_is("blocked"),
            "#631 gap 3 (reviewer repro): a COMPLETE route the walker is actively advancing must NEVER \
             be terminated no_progress, however far the current straight-line distance to the goal — a \
             complete route's end IS the goal, so advancing it is progress by construction");
        assert!(nav.goto_target.lock().unwrap().is_some(), "the go-around continues to the goal");
    }

    /// **The over-firing guard (the #631 high-risk half): a route still making progress is NEVER
    /// killed — even at the window boundary.** Same expired clock as above, but this tick's closest
    /// approach IMPROVES on the best, which must reset the clock and let navigation continue.
    ///
    /// Mutation check: make `progress_improved` always return `false` (or drop the `if improved`
    /// branch) and this route gets killed → the assert that it is STILL navigating goes RED. This is
    /// the test that would catch a no-progress detector that fires on legitimate progress.
    #[test]
    fn drive_walk_never_terminates_a_route_that_is_still_getting_closer() {
        let (mut w, nav, _intent, _view) = walker_with(open_plane(600.0));
        let mut gs = eqoxide_core::game_state::GameState::new();
        gs.world.zone_name = TEST_ZONE.into(); // #600: loaded-zone match
        gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0; gs.player_pos_known = true;
        let goal = (500.0, 0.0, 0.0);
        *nav.goto_target.lock().unwrap() = Some(goal);
        w.committed = Some(committed_complete()); // #851
        w.set_nav_state("navigating");
        w.path = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [20.0, 0.0, 0.0]];
        w.path_i = 0;
        w.path_goal = Some(goal);
        // The clock is expired, BUT the best approach so far (600) is worse than this tick's ~500, so
        // this observation is real progress: the clock resets and no termination may occur.
        w.nav_best_g3d = 600.0;
        w.nav_progress_at = std::time::Instant::now() - (NAV_NO_PROGRESS_WINDOW + std::time::Duration::from_secs(1));

        w.drive_walk(&mut gs, goal);

        assert!(!w.nav_state_is("blocked"),
            "#631 gap 3 (over-firing guard): a route whose closest approach IMPROVED this tick must \
             NOT be terminated — that would kill legitimate slow/detouring progress");
        assert!(nav.goto_target.lock().unwrap().is_some(), "navigation continues");
        assert!(w.nav_best_g3d <= 500.5, "the improved closest approach must be recorded");
    }

    // ───────────────────────── #851: the stall is PUBLISHED, not just detected ─────────────────

    /// A walker with a committed COMPLETE route, a body that never moves, and a closest approach
    /// that never improves. `drive_walk` is the real production tick.
    ///
    /// `path` deliberately starts 100u AWAY from the body so `advance_cursor` cannot move `path_i`
    /// (a cursor advance is channel (a) progress and would legitimately reset the verdict), and
    /// `nav_best_g3d` is pinned low so channel (b) cannot fire either. That is a genuinely stalled
    /// walker, not a slow one.
    fn stalled_walker_fixture() -> (Walker, eqoxide_ipc::NavSlots, eqoxide_core::game_state::GameState, (f32, f32, f32)) {
        let (mut w, nav, _intent, _view) = walker_with(open_plane(2000.0));
        let mut gs = eqoxide_core::game_state::GameState::new();
        gs.world.zone_name = TEST_ZONE.into(); // #600: loaded-zone match
        gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0; gs.player_pos_known = true;
        let goal = (500.0, 0.0, 0.0);
        *nav.goto_target.lock().unwrap() = Some(goal);
        nav.nav_state.lock().unwrap().goal_id = 1;
        w.reset_drive_state();                       // as `apply_plan` does on a fresh route…
        w.committed = Some(committed_complete());    // …then commits the facts…
        w.publish_drive_state();                     // …and publishes the first word from them.
        w.path = vec![[100.0, 0.0, 0.0], [200.0, 0.0, 0.0], [300.0, 0.0, 0.0],
                      [400.0, 0.0, 0.0], [500.0, 0.0, 0.0]];
        w.path_i = 0;
        w.stuck_i = 0;
        w.path_goal = Some(goal);
        w.nav_best_g3d = 1.0;      // channel (b) can never improve on this
        w.nav_best_gdist = 0.0;    // and the 200u repath-reset cannot fire either
        (w, nav, gs, goal)
    }

    /// **#851 — THE BUG. A walker whose body has stopped making progress must stop publishing
    /// `navigating`.** Drives the REAL `drive_walk` against a static body and reads the row an agent
    /// reads (`NavStatus`), not an internal counter.
    ///
    /// The stall was already DETECTED before this change — `stuck_ticks` reaches
    /// [`NAV_STUCK_TICKS`] in ~3 s and triggers a back-off and a re-path — it was simply never
    /// published, so `nav_state` read `navigating` for the whole ~32 s the walker spent circling
    /// under a ledge. That is what this test pins: the transition, and the calibration data beside
    /// it.
    ///
    /// PRE-CONDITION (asserted, not assumed): the walker publishes plain `navigating` before the
    /// threshold. Without it, a `driving_nav_state` hard-wired to `navigating_stalled` would pass.
    ///
    /// Mutation checks (outputs in the PR): WRAP the `Stalled` arm of `driving_nav_state` in
    /// `if false { … }` → RED here; delete the `self.publish_drive_state();` call from `drive_walk`
    /// → RED here.
    #[test]
    fn drive_walk_publishes_navigating_stalled_once_the_body_stops_progressing_851() {
        let (mut w, nav, mut gs, goal) = stalled_walker_fixture();

        // Before the threshold: unqualified progress is the HONEST answer — the walker has a route
        // and has only just started.
        for tick in 0..(NAV_STUCK_TICKS - 1) {
            w.drive_walk(&mut gs, goal);
            let s = nav.nav_state.lock().unwrap();
            assert_eq!(s.state, "navigating",
                "tick {tick}: a walker inside its own stall threshold must read as navigating");
            assert!(s.stall.is_none(), "tick {tick}: no stall data before the verdict flips");
        }
        // The threshold tick.
        w.drive_walk(&mut gs, goal);
        let s = nav.nav_state.lock().unwrap().clone();
        assert_eq!(s.state, "navigating_stalled",
            "#851: after {NAV_STUCK_TICKS} ticks with no cursor advance and no closest-approach \
             improvement, `nav_state` must NOT still read as unqualified progress");
        let stall = s.stall.expect("#851: the stalled state must carry its calibration data");
        assert!(stall.quiet_ticks >= NAV_STUCK_TICKS,
            "the published quiet-tick count must be the real one, got {}", stall.quiet_ticks);
        assert_eq!(stall.route, "complete",
            "the committed route is complete — the stall is about EXECUTING it, not about routing");
        // …and the honesty fix must not have changed WHEN navigation gives up: the goal is still
        // live, because a stall is recoverable and the walker is about to back off and re-path.
        assert!(nav.goto_target.lock().unwrap().is_some(),
            "publishing the stall must not terminate the goto — that is a different decision");
    }

    /// **The stall verdict cannot be laundered by the walker's own recovery.** The pre-#851 signals
    /// were `stuck_ticks` (reset to 0 at every threshold, before the back-off) and `nav_repaths` —
    /// so anything published from `stuck_ticks` would have flickered back to a clean reading every
    /// [`NAV_STUCK_TICKS`] ticks while the body sat exactly where it was. That flicker is precisely
    /// the shape that makes an agent's read a lie, so the verdict LATCHES: only real progress clears
    /// it ([`crate::steering::RouteExecution::tick`]).
    ///
    /// Drives long enough to cross the threshold several times and collects EVERY published word.
    ///
    /// REACH CONTROL: the walker must actually have re-pathed and backed off during the run —
    /// otherwise this is just the previous test with a longer loop and proves nothing about
    /// laundering.
    ///
    /// Mutation check: break `quiet_ticks` monotonicity in `RouteExecution::tick` — reset it on a
    /// re-path (`let quiet_ticks = if repaths > 0 { 1 } else { … };`), which is the shape a "the
    /// re-path fixed it" bug takes → RED here (the word returns to `navigating` after each
    /// back-off). Output in the PR.
    #[test]
    fn a_repath_and_backoff_cannot_launder_the_851_stall() {
        let (mut w, nav, mut gs, goal) = stalled_walker_fixture();
        let mut words: Vec<String> = Vec::new();
        for _ in 0..(NAV_STUCK_TICKS * 4) {
            w.drive_walk(&mut gs, goal);
            words.push(nav.nav_state.lock().unwrap().state.clone());
            if nav.goto_target.lock().unwrap().is_none() { break; } // terminated honestly — stop
        }
        assert!(w.nav_repaths > 0 || w.backoff_ticks > 0 || words.iter().any(|s| s == "blocked"),
            "reach control: the walker never re-pathed, backed off or gave up in {} ticks, so this \
             run never exercised the recovery path the latch exists to survive", words.len());
        let first_stall = words.iter().position(|s| s == "navigating_stalled")
            .expect("the walker must reach the stalled verdict at all");
        for (i, w) in words.iter().enumerate().skip(first_stall) {
            assert_ne!(w, "navigating",
                "#851: tick {i} republished unqualified `navigating` after the stall at tick \
                 {first_stall}, with the body in the same place the whole time. Sequence: {words:?}");
            assert_ne!(w, "navigating_partial",
                "#851: tick {i} republished `navigating_partial` after the stall at tick {first_stall}");
        }
    }

    /// **Real progress clears the verdict, and a healthy walk never trips it.** The over-firing
    /// control for the two tests above: a body that keeps closing on the goal must read `navigating`
    /// for the whole run, well past [`NAV_STUCK_TICKS`]. A stall report on a walker that is walking
    /// is the same class of lie in the other direction.
    ///
    /// Mutation check: make `RouteExecution::tick` ignore its `progressed` argument (always take the
    /// quiet branch) → RED here, GREEN on the two tests above — which is why this control exists.
    #[test]
    fn a_walker_that_keeps_closing_on_the_goal_never_reads_as_stalled_851() {
        let (mut w, nav, mut gs, goal) = stalled_walker_fixture();
        w.nav_best_g3d = 1000.0; // the honest starting best for a body 500u out
        let mut checked = 0u32;
        for tick in 0..(NAV_STUCK_TICKS * 3) {
            gs.player_x += 10.0; // …and it really is closing, 10u per tick
            w.drive_walk(&mut gs, goal);
            if nav.goto_target.lock().unwrap().is_none() { break; } // arrived
            let s = nav.nav_state.lock().unwrap();
            assert_eq!(s.state, "navigating",
                "tick {tick}: a walker whose closest approach improves every tick must never be \
                 reported as stalled");
            assert!(s.stall.is_none(), "tick {tick}: no stall data on a healthy walk");
            checked += 1;
        }
        // REACH CONTROL: the run must have gone WELL past the stall threshold, or an early arrival
        // would leave this control green without ever testing the window it is about.
        assert!(checked > NAV_STUCK_TICKS,
            "reach control: only {checked} ticks were checked, which is inside the {NAV_STUCK_TICKS}\
             -tick threshold — this control never reached the region it claims to cover");
    }

    /// **`quiet_ms` measures the window `quiet_ticks` counts — not the window since the verdict
    /// flipped (#851 review round 1, B2c).**
    ///
    /// The old origin was the flip, so the first `navigating_stalled` an agent ever saw carried
    /// `quiet_ms: 0` beside `quiet_ticks: 20` — a uniform [`NAV_STUCK_TICKS`]-tick (~3 s)
    /// understatement of how long the body had been going nowhere, in the direction that makes an
    /// agent wait longer. Erring safely does not make a number true, and the payload gives an agent
    /// no way to detect the offset.
    ///
    /// Unit ticks run microseconds apart, so a real ~3 s figure is not observable here. What IS
    /// observable is the ORIGIN: park 60 ms of wall clock inside the quiet window, before the tick
    /// that flips the verdict, and the honest reading must include it.
    ///
    /// Mutation check: restore the old origin (`Some(Instant::now())` when the verdict flips, `None`
    /// otherwise) → RED here, `quiet_ms` reads ~0. Delete the `last_progress_at` seed from
    /// `reset_drive_state` → also RED (the `is_none()` seed in `tick_drive_state` then stamps the
    /// origin one tick INTO the quiet window instead of at the journey's start… and this test's
    /// sleep is before that tick, so the 60 ms is lost).
    #[test]
    fn the_stall_clock_measures_the_whole_quiet_window_not_just_since_the_verdict_851() {
        let (mut w, nav, mut gs, goal) = stalled_walker_fixture();
        // Every tick below is quiet, so the whole loop is inside the window `quiet_ticks` counts.
        for _ in 0..(NAV_STUCK_TICKS - 1) { w.drive_walk(&mut gs, goal); }
        assert_eq!(nav.nav_state.lock().unwrap().state, "navigating",
            "PREMISE: the verdict has NOT flipped yet, so the sleep below lands strictly inside the \
             detection window the old origin excluded");
        const PARKED: u64 = 60;
        std::thread::sleep(std::time::Duration::from_millis(PARKED));
        w.drive_walk(&mut gs, goal); // …the tick that flips it

        let s = nav.nav_state.lock().unwrap();
        assert_eq!(s.state, "navigating_stalled", "PREMISE: the verdict flipped on this tick");
        let stall = s.stall.expect("the stalled state carries its calibration");
        assert!(stall.quiet_ms >= PARKED,
            "#851 B2c: `quiet_ms` reported {} ms for a body that had already been going nowhere for \
             at least {PARKED} ms when the verdict flipped. It must measure the same window \
             `quiet_ticks` ({}) counts — since the walker last made progress — not since the flip.",
            stall.quiet_ms, stall.quiet_ticks);
    }

    /// **A NEW goal starts clean.** The verdict is keyed on `NavStatus::goal_id`, so a stall latched
    /// against the previous goto cannot be reported against the next one — the failure mode a latch
    /// invites is telling an agent its brand-new `/move/goto` is already wedged.
    ///
    /// Mutation check: delete the `goal_id != self.exec_goal_id` reset from `tick_drive_state` and
    /// this goes RED.
    #[test]
    fn a_fresh_goal_id_clears_a_latched_851_stall() {
        let (mut w, nav, mut gs, goal) = stalled_walker_fixture();
        for _ in 0..NAV_STUCK_TICKS { w.drive_walk(&mut gs, goal); }
        assert_eq!(nav.nav_state.lock().unwrap().state, "navigating_stalled",
            "PREMISE: the walker is latched stalled before the new goal arrives");

        // A new goto: `request_goto` bumps `goal_id`. Everything else about the walker is left
        // exactly as the stalled run left it, which is the point — the reset must not depend on the
        // caller having tidied up.
        nav.nav_state.lock().unwrap().goal_id = 2;
        w.drive_walk(&mut gs, goal);
        let s = nav.nav_state.lock().unwrap();
        assert_eq!(s.state, "navigating",
            "#851: a stall latched against goal #1 must not be reported against goal #2");
        assert!(s.stall.is_none(), "and its calibration data must go with it");
    }
}
