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
use eqoxide_core::physics::RUN_SPEED;

/// The nav state published while this client has NO collision grid for the current zone — the
/// terrain assets are still loading, or their load failed (#579). It is NOT `blocked` (there is no
/// obstacle), NOT `no_path` (no search was ever run) and above all NOT `navigating`: the honest
/// answer is "I have no model of this world yet, so I cannot tell you anything about routes here."
/// Read `zone_assets` on GET /v1/observe/debug to tell *pending* from *failed*.
pub const NAV_STATE_ZONE_LOADING: &str = "zone_loading";

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
    ) -> Self {
        Walker {
            nav, world, collision, nav_intent, nav_debug,
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
    ///   idle | planning | navigating | navigating_partial | following | arrived
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
        let (state, reason) = {
            let s = self.nav.nav_state.lock().unwrap();
            (s.state.clone(), s.reason.clone())
        };
        let goal = self.nav.goto_target.lock().unwrap().map(|(x, y, z)| [x, y, z]);
        let snap = crate::diagnostics::NavDebugSnapshot {
            seq: self.debug_seq,
            zone_model_loaded: self.collision.read().unwrap().is_some(),
            nav_state: state,
            nav_reason: reason,
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
            let (outcome_str, route_len) = match &reply.outcome {
                PlanOutcome::Route(p) => ("route", p.len()),
                PlanOutcome::Unreachable { .. } => ("unreachable", 0),
                PlanOutcome::Exhausted { progress, .. } =>
                    ("exhausted", progress.as_ref().map_or(0, |p| p.len())),
            };
            self.last_plan = Some(std::sync::Arc::new(crate::diagnostics::PlanDebug {
                gen: reply.gen,
                start: reply.start,
                goal: reply.goal,
                outcome: outcome_str.to_string(),
                reason: reply.outcome.reason().to_string(),
                route_len,
                plan_ms: reply.plan_ms as u64,
                tight: reply.tight,
                goal_snapped: snapped.is_some(),
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
        self.set_nav_state("idle");
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
    pub fn find_in_zone_portal(&self, gs: &GameState) -> Option<(f32, f32, f32)> {
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
            } else if let Some(&pos) = self.world.entity_positions.lock().unwrap().get(&name) {
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
                if self.nav_state_is("navigating") || self.nav_state_is("navigating_partial")
                    || self.nav_state_is("planning") || self.nav_state_is(NAV_STATE_ZONE_LOADING)
                {
                    self.set_nav_state("idle");
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
            if let Some((wish_dir, heading)) =
                fast_steer_aim(&self.local_path, &mut self.local_i, [gs.player_x, gs.player_y, gs.player_z], 5.0)
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
        let edges = if advertised.is_empty() { Vec::new() } else { c.resolve_teleport_pads(&advertised) };
        // Publish what nav KNOWS about each pad (#608/#543): advertised-and-usable (an A* edge
        // exists), advertised-but-refused by the honesty gate, or unknown. The `Learned*` states
        // arrive with the #543 learning loop.
        self.last_pads = advertised.iter().map(|&(idx, _)| {
            match edges.iter().find(|e| e.index == idx) {
                Some(e) => PadDebug { index: idx, knowledge: PadKnowledge::AdvertisedUsable {
                    source: e.source, dest: e.dest } },
                None => PadDebug { index: idx, knowledge: PadKnowledge::AdvertisedUnusable },
            }
        }).chain(unknown_idxs.into_iter().map(|idx| PadDebug {
            index: idx, knowledge: PadKnowledge::Unknown,
        })).collect();
        edges
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
    fn halt_no_world(&mut self, player: Option<[f32; 3]>) {
        self.path.clear();
        self.path_i = 0;
        self.path_goal = None;      // force a REAL plan the moment collision appears
        self.clear_local_plan();
        self.planner.cancel();
        self.awaiting_first_plan = false;
        *self.nav_intent.lock().unwrap() = None;
        self.set_nav_state_because(NAV_STATE_ZONE_LOADING, Some("zone_assets_not_loaded"));
        // Publish honestly: `zone_model_loaded: false`, no routes — "I have no model of this
        // world", never a route through unloaded geometry (#579). `player` comes from the caller's
        // GameState (None until the server placed us — never a fabricated position, #615 F1).
        self.publish_debug(player, None);
    }

    pub fn drive_walk(&mut self, gs: &mut GameState, goal: (f32, f32, f32)) {
        // No collision grid → no world model. Never present a straight line through unloaded
        // geometry as a route (#579). Checked BEFORE any planning/steering so the walker cannot
        // move the character on a world it does not have.
        if self.collision.read().unwrap().is_none() {
            self.halt_no_world(Self::known_pos(gs));
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
                // zone change landing mid-tick). Same honest answer, never a bare "navigating".
                None => { self.halt_no_world(Self::known_pos(gs)); return; }
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

            let aim = steer_target(&self.path, self.path_i, &self.local_path, &mut self.local_i,
                [px, py, pz], LOOK_AHEAD, coarse);
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
                speed:       RUN_SPEED,
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
            speed:       RUN_SPEED,
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

    fn walker_with(collision: crate::collision::SharedCollision)
        -> (Walker, eqoxide_ipc::NavSlots, eqoxide_ipc::NavIntent, crate::diagnostics::NavDebugView)
    {
        let nav: eqoxide_ipc::NavSlots = Default::default();
        let world: eqoxide_ipc::WorldSlots = Default::default();
        let intent: eqoxide_ipc::NavIntent = Default::default();
        let view: crate::diagnostics::NavDebugView = Default::default();
        let w = Walker::new(nav.clone(), world, collision, intent.clone(), view.clone());
        (w, nav, intent, view)
    }

    /// **#579, the agent-honesty regression.** With no collision grid — the zone's terrain GLB is
    /// still downloading/decoding, which for freportw (~30 MB) is a multi-second window — the walker
    /// used to publish `nav_state: "navigating"` and steer in a dead-straight line at the goal. An
    /// agent polling in that window read a confident walkable route through geometry that had not
    /// been built: the "700u unobstructed" of the false #560 report.
    ///
    /// The honest answer is `zone_loading` / `zone_assets_not_loaded`, with NO movement intent and
    /// NO route overlay — "I have no model of this world", not "the way is clear".
    #[test]
    fn no_collision_reports_zone_loading_and_never_a_route() {
        let (mut w, nav, intent, view) = walker_with(Arc::new(std::sync::RwLock::new(None)));
        let mut gs = eqoxide_core::game_state::GameState::new();
        gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0;

        w.drive_walk(&mut gs, (700.0, 0.0, 0.0));

        let s = nav.nav_state.lock().unwrap().clone();
        assert_eq!(s.state, NAV_STATE_ZONE_LOADING,
            "with no collision the walker must NOT claim to be navigating — that is the #579 lie");
        assert_eq!(s.reason.as_deref(), Some("zone_assets_not_loaded"));
        assert!(intent.lock().unwrap().is_none(),
            "the walker must not drive the controller through a world it has not loaded");
        let snap = view.lock().unwrap().clone().expect("the honest no-world state must be published");
        assert!(!snap.zone_model_loaded, "the snapshot must say there is NO world model");
        assert!(snap.committed_coarse.is_empty() && snap.committed_fine.is_empty(),
            "no route may be published without collision");
        assert_eq!(snap.nav_state, NAV_STATE_ZONE_LOADING);
        assert!(w.path.is_empty());
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
}
