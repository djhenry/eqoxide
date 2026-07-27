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
const STEER_LOS_CLEARANCE: f32 = eqoxide_core::physics::PLAYER_RADIUS;

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
/// still in flight? An unrecognised word is treated as IN-PROGRESS — the fail-safe direction: a
/// future state that someone forgets to classify retires honestly instead of sticking forever.
pub fn nav_state_is_terminal(state: &str) -> bool { TERMINAL_NAV_STATES.contains(&state) }

/// `nav_reason` on the `idle` a goal is retired to when it vanished without ever producing an
/// outcome (#725) — the accepted request was dropped, cancelled elsewhere, or its chase target
/// despawned. Distinguishes "idle because your goal quietly went nowhere" from "idle, ready for
/// work", which are the same word and were previously indistinguishable.
pub const NAV_REASON_GOAL_DROPPED: &str = "goal_dropped";

/// `nav_reason` on the `idle` that [`NAV_STATE_DEAD`] retires to once the character is alive again
/// (#644) — "idle because you respawned", not "idle, nothing happened".
pub const NAV_REASON_RESPAWNED: &str = "respawned";

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
        self.backoff_ticks = 0;
        self.local_stuck_ticks = 0;
        self.replan_coarse = false;
        self.replan_cooldown = 0;
        // A plan in flight was computed against the PREVIOUS zone's collision grid and its
        // coordinate space. Abandon it — applying it here would drive the character at a route
        // through a zone it is no longer in.
        self.planner.cancel();
        self.awaiting_first_plan = false;
        self.set_nav_state("idle");
        self.nav.nav_state.lock().unwrap().tier = None; // no route committed → no per-route tier
        // Publish the cleared snapshot so no consumer keeps drawing the previous zone's state.
        // Position: None — the old zone's coordinates would be a confident wrong answer in the
        // new zone's space (#615 review F1); the next tick republishes the real one.
        self.publish_debug(None, None);
    }

    /// Publish the current `/move/goto` navigation state for GET /v1/observe/debug (#166, #337).
    /// The value set is an AGENT-FACING CONTRACT — every value is documented in `docs/http-api.md`:
    ///
    ///   pending | idle | planning | navigating | navigating_partial | following | arrived
    ///   | no_path | search_exhausted | blocked | zone_loading
    ///
    /// `zone_loading` (#579) means the zone's collision grid is not built (assets still loading, or
    /// their load failed) — the client has no world model to route in, and no route claim of any
    /// kind should be read from it. See [`NAV_STATE_ZONE_LOADING`].
    ///
    /// `reason` is the machine-readable WHY behind a terminal state.
    pub fn set_nav_state(&self, state: &str) { self.set_nav_state_because(state, None); }

    /// Set the walker's state + reason. **Deliberately does NOT touch `local`** — the fine tier's
    /// last word is an independent fact about a different tier (#382).
    pub fn set_nav_state_because(&self, state: &str, reason: Option<&str>) {
        let mut s = self.nav.nav_state.lock().unwrap();
        let reason = reason.map(str::to_string);
        if s.state != state || s.reason != reason {
            s.state = state.to_string();
            s.reason = reason;
            // A state transition retires the previous route's per-instance facts (#378 Phase 2,
            // #343 discipline) — see the pre-extraction doc comment for the full rationale.
            s.blocked_goal = None;
            s.blocked_frontier = None;
            s.tier = None;
        }
    }

    /// Publish the FINE tier's last honest outcome (#382). Never touches `state`/`reason`.
    pub fn set_nav_local(&self, local: Option<eqoxide_ipc::NavLocal>) {
        let mut s = self.nav.nav_state.lock().unwrap();
        if s.local != local { s.local = local; }
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
                if self.goal_snapped {
                    self.set_nav_state_because("navigating", Some("goal_z_snapped"));
                } else {
                    self.set_nav_state("navigating");
                }
                self.nav.nav_state.lock().unwrap().tier =
                    Some(if reply.tight { "minimum" } else { "preferred" });
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
                self.set_nav_state_because("navigating_partial", Some(limit.as_str()));
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
        while self.path_i + 2 < self.path.len() {
            let (a, b) = (self.path[self.path_i], self.path[self.path_i + 1]);
            // 3D projection (water-nav Slice 3, §8.1): a near-vertical dive/ascent leg is not skipped
            // on frame one — path_i advances past it only once the char has actually changed depth.
            // Near-horizontal land: the z term vanishes, so this is the same advance as before.
            let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
            let t = if l2 < 1e-6 { 1.0 } else { ((px - a[0]) * ab[0] + (py - a[1]) * ab[1] + (pz - a[2]) * ab[2]) / l2 };
            if t >= 1.0 { self.path_i += 1; } else { break; }
        }
        let have_path = !self.path.is_empty();
        let target: (f32, f32, f32) = if have_path {
            const LOCAL_REACH: f32 = 24.0;   // how far ahead on the coarse route the fine plan aims
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
        //       the max seen on this route, while `nav_state == navigating`). A complete route's end
        //       IS the goal, so advancing it is guaranteed goal-ward progress *by construction* — it
        //       cannot be a lap (a lap would be a re-planned PARTIAL, `navigating_partial`, or would
        //       stop advancing `path_i` and trip `walker_stalled`). This is the fix for the reviewer's
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
            // (a) advancing a COMPLETE committed route — `path_i` past this route's max-so-far
            // (`stuck_i`, which the stall block below maintains and which is reset to 0 on every
            // re-plan, so a fresh route's advancement is always seen). `navigating` (not
            // `navigating_partial`) means the committed route actually reaches the goal.
            let advancing_complete_route = self.nav_state_is("navigating") && self.path_i > self.stuck_i;
            // (b) closest-approach improvement (side-effecting: lowers `nav_best_g3d`).
            let closer = crate::steering::progress_improved(&mut self.nav_best_g3d, g3d, NAV_PROGRESS_EPS);
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

    fn walker_with(collision: crate::collision::SharedCollision)
        -> (Walker, eqoxide_ipc::NavSlots, eqoxide_ipc::NavIntent, crate::diagnostics::NavDebugView)
    {
        let nav: eqoxide_ipc::NavSlots = Default::default();
        let world: eqoxide_ipc::WorldSlots = Default::default();
        let intent: eqoxide_ipc::NavIntent = Default::default();
        let view: crate::diagnostics::NavDebugView = Default::default();
        let za = zone_assets_for(&collision);
        let w = Walker::new(nav.clone(), world, collision, intent.clone(), view.clone(), za);
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
        let w = Walker::new(nav.clone(), world, collision, intent.clone(), view.clone(), zone_assets);
        (w, nav, intent, view)
    }

    // ───────────────────────────── #543: the unverifiable-pad scene ─────────────────────────────

    const PAD_ZONE: u16 = 2;
    const PAD_INDEX: i32 = 42;
    /// What the server ADVERTISES this pad's same-zone arrival to be (a real floor point on slab B).
    const PAD_ADVERTISED_DEST: [f32; 3] = [430.0, 40.0, 0.0];

    /// Two floor slabs 400u apart (no walk and no jump bridges the gap) with a DRNTP teleport-pad
    /// footprint on the near slab, advertised as landing on the far one. Mirrors `collision.rs`'s
    /// `pad_scene` (the #403 fixture) — duplicated here rather than shared so this test does not
    /// have to widen `collision.rs`'s private test module.
    fn pad_scene() -> crate::collision::Collision { pad_scene_leaves(false) }

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
        let w = Walker::new(nav, world.clone(), col, intent, view, za);
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
    /// one the character is in.** The sibling of `zone_assets::no_interleaving_of_the_two_writers_
    /// yields_a_usable_wrong_zone`, but exercising the CONSUMER (`drive_walk`) rather than the pure
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
    /// `render_lag >= 1` the walker routes on the previous zone's grid and the `state ==
    /// NAV_STATE_ZONE_LOADING` assertion in the stale window goes RED. A test that passes both ways
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
    /// real drivers in the eqoxide-net test `zone_cross_queued_during_load_is_cancellable_by_stop_
    /// and_never_leaks`. Here we only pin the guard's response: a present slot HOLDS `zone_loading`; an
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
    /// **Mutation check:** restore the old opt-in list (`navigating || navigating_partial ||
    /// planning || (zone_loading && !pending) || dead`) → `pending`, `following` and both unknown
    /// words stay put and this goes RED four times over. Alternatively add `"pending"` to
    /// [`TERMINAL_NAV_STATES`] → RED, since a terminal `pending` is precisely the lie.
    #[test]
    fn no_in_progress_nav_state_survives_a_tick_with_no_goal_725() {
        // The documented vocabulary, plus two words the codebase has never published — a future
        // in-progress state, and a typo'd one. Both must retire: unrecognised ⇒ in-progress.
        const IN_PROGRESS: [&str; 7] = [
            "pending", "planning", "navigating", "navigating_partial", "following",
            "crossing_a_state_invented_next_year", "navigatng",
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
        // A PARTIAL route (a moat lap), NOT a complete route to the goal.
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
        w.set_nav_state("navigating"); // a COMPLETE route (reaches the goal), not a partial
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
}
