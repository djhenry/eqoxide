//! Pure navigation/steering math — pursuit carrots, replan/arrival decisions, the fast-steering
//! cursor. Net-independent: this takes positions and paths, and depends only on `assets` types
//! (no `EqStream`, no packets). Extracted out of `eq_net::action_loop` (cleanup step 2 — nav must
//! not live inside net). The `ActionLoop` god-struct and its `tick()`/`sync_*`/`apply_*plan`
//! methods (the net action loop) are a later step and still live in `eq_net::action_loop`.

use eqoxide_core::coord::eq_heading;

// NOTE: `slide_move` — a second, divergent collision-slide implementation (chest ray at z+3, its
// own axis-drop logic) — was DELETED in Phase 2 (#378). It had ZERO production callers: the walker
// steers via `CharacterController` (movement.rs `slide`), the ONE collision model, which derives
// its probe heights from `traversability::PLAYER_BODY`. A second slide model that nothing calls is
// exactly the drift this refactor exists to make impossible (its z+3 chest ray never matched the
// controller's `Body::chest` = 4.0). Gone; there is now a single collide-and-slide in the client.

/// Fine grid resolution of the LOCAL plan — the tier the walker actually steers along.
///
/// This is the tier whose edges A* validates against the character's whole collision volume rather
/// than a ray (`nav::collision::SWEPT_EDGE_MAX_CELL`, which a test pins to be >= this). The coupling is why
/// the value lives here rather than inside `tick`: a silent change to either number un-arms the
/// #358 fix on the only tier that enforces it.
pub const LOCAL_CELL: f32 = 2.0;

/// Consecutive no-progress nav ticks (~150 ms each) before the pure-pursuit walker is declared
/// stuck and re-paths. ~3 s — long enough to ride out a brief wall-slide, short enough to recover.
pub const NAV_STUCK_TICKS: u32 = 20;
/// After this many consecutive no-progress ticks (well before the `NAV_STUCK_TICKS` give-up), the
/// walker commands the controller to hop — net progress has stalled, which is the real "wedged
/// against a fence/cart" signal (sliding along it still looks like motion frame-to-frame). (#41)
pub const NAV_HOP_TICKS: u32 = 6;
/// On a hard stall (NAV_STUCK_TICKS), drive the reverse (downhill) direction for this many ticks
/// before re-pathing — long enough to clear a wedged slope-face start (~150 ms/tick). (eqoxide#212)
pub const NAV_BACKOFF_TICKS: u32 = 3;
/// Proactive re-plan (#246): after this many consecutive ticks where the fine 2u plan can't REACH its
/// carrot on the committed coarse route, the route is treated as blocked ahead and re-planned from the
/// current position — long before the ~3 s NAV_STUCK_TICKS give-up, so the walker detours instead of
/// pressing into the obstacle. Small so the reaction is quick (~0.5 s) but > 1 to ride out a carrot
/// that momentarily lands on a fine-impassable lip.
pub const NAV_LOCAL_STUCK_TICKS: u32 = 3;
/// Minimum ticks between two proactive coarse re-plans, so a persistently-awkward carrot can't thrash
/// the coarse planner every tick (~1 s). The existing stall/back-off recovery still handles a genuine
/// wedge the fresh coarse plan can't route around.
pub const REPLAN_COOLDOWN_TICKS: u32 = 6;
/// How many PROACTIVE coarse re-plans (#246) may fire at ONE spot — without the journey getting
/// meaningfully closer to the goal — before the walker stops honestly (#378 Phase 2). Each proactive
/// re-plan reinstalls a fresh coarse route and so resets the stall clock, which is why the ~3 s
/// `NAV_STUCK_TICKS` give-up never trips at a fine-impassable spot and the walker oscillated
/// `navigating` forever (the live qcat L-corner). At ~(NAV_LOCAL_STUCK_TICKS + REPLAN_COOLDOWN_TICKS)
/// ≈ 9 ticks per proactive re-plan, 8 of them is ~11 s of trying to detour before the honest
/// `blocked / local_no_way_through`. Resets on real goal-ward progress (like `nav_repaths`), so a
/// long multi-corner journey that keeps progressing never trips it.
pub const PROACTIVE_REPLAN_CAP: u32 = 8;
/// After auto-escaping a sealed interior through an in-zone teleport (#266), block another escape for
/// this long (~10 s at 150 ms/tick) so a goal that's STILL unreachable after the teleport can't
/// ping-pong the char back and forth through the portal. One escape attempt, then it walks/stalls.
pub const PORTAL_COOLDOWN_TICKS: u32 = 66;
/// ROUTE-LEVEL NO-PROGRESS DETECTION (#631 gap 3). How much the closest 3-D approach to the goal
/// must improve to count as PROGRESS. Deliberately a whole coarse cell (8u): smaller and ordinary
/// server-position jitter around a lap would masquerade as forward progress and keep resetting the
/// window forever (the moat would never terminate); larger and a genuinely slow approach could be
/// mistaken for none. 8u over the [`NAV_NO_PROGRESS_WINDOW`] is a ~0.13 u/s closing rate — a walker
/// that cannot beat that toward its goal is, by any honest reading, not getting there.
pub const NAV_PROGRESS_EPS: f32 = 8.0;
/// How long BOTH progress channels (committed-route advancement of a complete route, AND
/// closest-approach improvement — see `Walker::drive_walk`) may go quiet before navigation
/// terminates honestly (`blocked` / `no_progress`). This window governs ONLY the case where the
/// walker is *not* advancing a complete route (a re-planned partial / lap) AND is not getting any
/// closer — the moat, which swam laps for 3+ minutes. A legitimate route the walker is traversing
/// keeps channel (a) firing every tick regardless of how long or how far-from-goal the go-around is,
/// so the length of a legit route is NOT bounded by this constant (the earlier claim that 60s
/// "tolerates ~2.6 km of away-travel" was wrong: with a single all-time-closest signal the return
/// leg kept the clock running, so the real tolerance was total-route < 60s — which is exactly the
/// false-fire the committed-route channel removes). 60s is simply how long a genuine no-forward-
/// progress lap may persist before it is called. (#631)
pub const NAV_NO_PROGRESS_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// Record a closest-approach observation and report whether it was genuine PROGRESS (#631 gap 3).
///
/// `best` is the smallest 3-D distance-to-goal seen so far on this goal (`f32::MAX` before any); a
/// new observation `d` counts as progress only when it beats `best` by more than `eps`, at which
/// point `best` is lowered to `d`. The first observation (from `f32::MAX`) always counts, so the
/// caller's no-progress clock self-initialises on the first drive tick. Pure and total, so the
/// no-progress policy is unit-testable off the tick against circling / approaching / detouring
/// trajectories — the exact place an over-firing bug would hide.
pub fn progress_improved(best: &mut f32, d: f32, eps: f32) -> bool {
    if d < *best - eps {
        *best = d;
        true
    } else {
        false
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// #851 — the published `nav_state` word for a walker that HAS a route, as a function of a type.
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Which committed route the walker is executing. Until #851 this fact lived ONLY in the published
/// `nav_state` string (`navigating` vs `navigating_partial`), which is why the #631 no-progress
/// detector had to read its own published state back (`nav_state_is("navigating")`) to find out
/// whether the route it is walking reaches the goal. A published string is the wrong home for a
/// fact the publication itself depends on: the moment a third driving word exists, that read
/// silently answers a different question. It is a walker field now, and the string is derived
/// from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommittedRoute {
    /// The committed route ENDS at the goal (`PlanOutcome::Route`).
    Complete,
    /// The committed route is a partial toward a frontier (`PlanOutcome::Exhausted { progress }`) —
    /// it is not a route to the goal and will be re-planned from its far end.
    Partial,
}

impl CommittedRoute {
    /// The agent-facing word for this route's completeness, used in the `nav_stall` payload.
    pub fn as_str(self) -> &'static str {
        match self { CommittedRoute::Complete => "complete", CommittedRoute::Partial => "partial" }
    }
}

/// **The walker's verdict on whether the BODY is executing the committed route (#851).**
///
/// This is the type that makes "reports progress while going nowhere" unrepresentable at the
/// publication site — **for values that came out of [`RouteExecution::tick`]**, which is the
/// precise claim and worth stating as such (#851 review round 1, N2). The variants carry public
/// fields, so `RouteExecution::Stalled { quiet_ticks: 0, repaths: 0 }` and
/// `Advancing { quiet_ticks: 5_000 }` are both *values* — nonsense ones, contradicting the field
/// docs below, that the machine cannot reach but the syntax can write. `#[non_exhaustive]` on each
/// variant confines that to THIS crate (the idiom `zone_assets::ZoneAssetState` uses). Inside the
/// crate, a hand-written value still cannot reach `Walker::exec`, for a reason the compiler
/// enforces rather than a convention: that field is a [`GoalVerdict`], whose fields are private to
/// THIS module, so `walker.rs` cannot build one out of a `RouteExecution` it wrote itself — the
/// only `GoalVerdict`s it can obtain come from [`GoalVerdict::fresh_for`] and [`GoalVerdict::tick`].
/// So: unreachable in production, unconstructible downstream, constructible in this module's own
/// tests — which is where the fixtures that drive the machine to a named state have to live anyway.
///
/// Before #851 the walker's stall knowledge lived in three loose `u32`s
/// (`stuck_ticks`, `nav_repaths`, `backoff_ticks`), none of which was published anywhere, and the
/// published `nav_state` was a `&str` literal written once at plan commit. So the whole
/// stall/back-off/re-path recovery window — ~32 s of it, measured live at the qcat pocket — read as
/// an unqualified `navigating`. The detection was not missing; it fired at ~3 s and was simply not
/// observable. See [`driving_nav_state`], which is the total function from this verdict to the word.
///
/// **The latch is the point.** `stuck_ticks` is reset to 0 by the stall block the instant it fires,
/// and a proactive coarse re-plan (#246) reinstalls a fresh route and resets it again — which is
/// exactly why neither counter can be published as-is: they read "fine" for most of a wedge. This
/// machine's `quiet_ticks` is its own, and once it reaches [`NAV_STUCK_TICKS`] the verdict stays
/// [`RouteExecution::Stalled`] until the walker makes REAL progress. Nothing else clears it — not a
/// back-off, not a re-path, not a fresh route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RouteExecution {
    /// The body is executing the route: it has made progress within the last `quiet_ticks` ticks,
    /// and `quiet_ticks < NAV_STUCK_TICKS`.
    #[non_exhaustive]
    Advancing { quiet_ticks: u32 },
    /// The body has STOPPED executing the route: no progress for `quiet_ticks` consecutive ticks
    /// (`>= NAV_STUCK_TICKS`), with `repaths` stall-recovery re-paths run in the meantime.
    #[non_exhaustive]
    Stalled { quiet_ticks: u32, repaths: u32 },
}

impl RouteExecution {
    /// The verdict a freshly-aimed goal starts from. Not `Default`, deliberately: this is the state
    /// of a goal that has just been accepted, and it should be named at the sites that mean that.
    pub const fn fresh() -> Self { RouteExecution::Advancing { quiet_ticks: 0 } }

    /// Consecutive ticks since the walker last made progress (either channel).
    pub fn quiet_ticks(self) -> u32 {
        match self {
            RouteExecution::Advancing { quiet_ticks } => quiet_ticks,
            RouteExecution::Stalled { quiet_ticks, .. } => quiet_ticks,
        }
    }

    /// Has the body stopped executing the committed route?
    pub fn is_stalled(self) -> bool { matches!(self, RouteExecution::Stalled { .. }) }

    /// One nav tick (~150 ms). `progressed` is the walker's TWO-CHANNEL progress signal — the same
    /// one #631 already computes every tick and, until #851, only ever consulted at the 60 s
    /// give-up: the route cursor advanced by WALKING, or the closest 3-D approach to the goal
    /// improved by [`NAV_PROGRESS_EPS`]. `repaths` is the live stall-recovery re-path count, carried
    /// into the verdict so the published payload can say how hard the walker has already tried.
    ///
    /// Total, pure and `#[must_use]`: the only way to move this machine is to take its answer.
    ///
    /// **The `Stalled` verdict is a LATCH — nothing but real progress clears it — and that is a
    /// consequence of the line below, not a separate flag.** `quiet_ticks` is MONOTONE: the early
    /// return is the only place it ever decreases, so once it reaches [`NAV_STUCK_TICKS`] it stays
    /// there, and the comparison alone re-derives `Stalled` on every subsequent quiet tick.
    ///
    /// That matters because the walker's own recovery machinery resets the signals this replaces:
    /// `stuck_ticks` is set to 0 at every threshold *before* the back-off, so a state published
    /// from it would have flickered back to a clean reading every ~3 s with the body in exactly the
    /// same place — a flicker an agent cannot distinguish from a recovery that worked.
    ///
    /// This was written the other way first, as `if self.is_stalled() || quiet_ticks >= …`, and the
    /// mutation check is what removed it: dropping `self.is_stalled() ||` left every test GREEN,
    /// because it is redundant given monotonicity. Redundant text that no test can distinguish is a
    /// liability — it reads like the thing carrying the property when the comparison is. The real
    /// mutation for the latch is one that breaks monotonicity (reset `quiet_ticks` when
    /// `repaths > 0`, the shape a "the re-path fixed it" bug would take), and it is RED — see
    /// `a_repath_and_backoff_cannot_launder_the_851_stall`.
    #[must_use]
    pub fn tick(self, progressed: bool, repaths: u32) -> Self {
        if progressed { return RouteExecution::fresh(); }
        let quiet_ticks = self.quiet_ticks().saturating_add(1);
        if quiet_ticks >= NAV_STUCK_TICKS {
            RouteExecution::Stalled { quiet_ticks, repaths }
        } else {
            RouteExecution::Advancing { quiet_ticks }
        }
    }
}

/// **A [`RouteExecution`] and the journey it is about, as one value (#851 review round 2, B1).**
///
/// A verdict is a fact about ONE goal. Round 2 measured what happens when it is kept in a field and
/// read by a publisher that never asks which goal it belongs to: a `/follow` chase that was
/// genuinely walking — 80 u covered in 8 ticks, leader still 220 u off — published
/// `navigating_stalled` on every tick, carrying the *previous* `/goto`'s `nav_stall` numbers. A
/// moving walker reading as stalled is the same lie #851 exists to remove, pointing the other way.
///
/// The remedy is not another check at the publisher — the walker already had an `exec_goal_id`
/// field and the publisher simply did not consult it. Here verdict and goal id are ONE value with
/// PRIVATE
/// fields, and the only way to get a `RouteExecution` back out is [`GoalVerdict::as_of`], which has
/// to be told which goal the caller is publishing for. Reading a verdict without naming a goal does
/// not compile.
///
/// A verdict about a different goal answers [`RouteExecution::fresh`] — "nothing is known about
/// THIS journey yet". That is not a default standing in for a missing answer: a machine that never
/// observed this goal has, correctly, no evidence that this goal is stalled, and `fresh` is the
/// value that says so. It is what a walker publishes for its first tick on any goal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GoalVerdict {
    goal_id: u64,
    exec:    RouteExecution,
}

impl GoalVerdict {
    /// The verdict a freshly-aimed `goal_id` starts from. Not `Default`: a verdict is always about
    /// some goal, and the goal has to be named.
    pub const fn fresh_for(goal_id: u64) -> Self {
        GoalVerdict { goal_id, exec: RouteExecution::fresh() }
    }

    /// Is this verdict about `goal_id`? For callers that must distinguish "a new journey started"
    /// from "this journey progressed" — [`crate::walker::Walker`]'s stall clock is one.
    pub fn is_about(self, goal_id: u64) -> bool { self.goal_id == goal_id }

    /// **The only way to read the verdict.** `goal_id` is the goal the caller is about to publish
    /// for; a verdict about any other goal reads [`RouteExecution::fresh`] (see the type doc).
    #[must_use]
    pub fn as_of(self, goal_id: u64) -> RouteExecution {
        if self.goal_id == goal_id { self.exec } else { RouteExecution::fresh() }
    }

    /// One nav tick FOR `goal_id`. Ticking a verdict that belongs to another goal starts that
    /// goal's own journey rather than continuing the old one's count — the identity rule is
    /// [`GoalVerdict::as_of`]'s, applied here too, so there is exactly one implementation of it.
    #[must_use]
    pub fn tick(self, goal_id: u64, progressed: bool, repaths: u32) -> Self {
        GoalVerdict { goal_id, exec: self.as_of(goal_id).tick(progressed, repaths) }
    }
}

/// `nav_state` for a walker on a complete route that is executing it.
pub const NAV_STATE_NAVIGATING: &str = "navigating";
/// `nav_state` for a walker on a PARTIAL route that is executing it.
pub const NAV_STATE_NAVIGATING_PARTIAL: &str = "navigating_partial";
/// `nav_state` for a walker that HAS a route and is NOT executing it (#851) — "route in hand,
/// execution not progressing, still retrying". IN-PROGRESS, not terminal: the walker is still in
/// its stall/back-off/re-path recovery and may well escape (and if it does not, it terminates at
/// `blocked` as before). It is deliberately NOT on [`crate::walker::TERMINAL_NAV_STATES`] — see the
/// argument on `nav_state_is_terminal` for why an unlisted in-progress word retires honestly while
/// an unlisted terminal one would be destroyed.
pub const NAV_STATE_NAVIGATING_STALLED: &str = "navigating_stalled";

/// **THE mapping from (what route is committed, is the body executing it) to the published
/// `nav_state` word (#851).** Total, and that totality is the guarantee: there is no
/// (`CommittedRoute`, [`RouteExecution`]) pair that yields `navigating` or `navigating_partial`
/// while the verdict is [`RouteExecution::Stalled`], because the `Stalled` arm is matched FIRST and
/// does not look at the route at all. A caller cannot "forget" to check the stall, because there is
/// no route through this function that does not.
///
/// **What that proves, and what it does not.** It proves that every word produced HERE agrees with
/// the verdict. It does NOT prove that `navigating` is unreachable by other means:
/// `Walker::set_nav_state_because` takes a `&str`, so any writer in `eqoxide-nav` can still publish
/// the literal. What #851 changes is that the walker's own driving publication goes through this
/// function and nothing else — checked by
/// `walker::tests::the_driving_nav_state_word_is_only_ever_written_through_the_verdict_851`, which
/// is a source scan (a grep-checkable convention, an alarm on the likely edit) and not a type. The
/// structural remedy is the workspace-wide typed `state` `NavStatus::retire_to_idle`'s doc already
/// records as out of scope; this is the strongest version available without it, and the difference
/// is stated rather than implied.
pub fn driving_nav_state(route: CommittedRoute, exec: RouteExecution) -> &'static str {
    match exec {
        RouteExecution::Stalled { .. } => NAV_STATE_NAVIGATING_STALLED,
        RouteExecution::Advancing { .. } => match route {
            CommittedRoute::Complete => NAV_STATE_NAVIGATING,
            CommittedRoute::Partial  => NAV_STATE_NAVIGATING_PARTIAL,
        },
    }
}

/// The HORIZONTAL distance from the committed route's ENDPOINT to the goal the caller named
/// (#631 gap 2). `route_end` is the last waypoint of the committed route (`None` for a definitive
/// no-route). A COMPLETE route ends exactly at the requested XY, so this is `0.0`; a partial route
/// that stops at its closest approach returns how far, horizontally, that endpoint falls short —
/// the honest companion to the vertical-only `goal_snapped`, so `goal_snapped: false` can no longer
/// hide that the destination differs from the named coordinates. Pure/total for unit testing.
pub fn route_goal_offset(route_end: Option<[f32; 3]>, goal: [f32; 3]) -> f32 {
    match route_end {
        Some(end) => ((end[0] - goal[0]).powi(2) + (end[1] - goal[1]).powi(2)).sqrt(),
        None => 0.0,
    }
}

/// A path segment longer than this (horizontal) is a find_path JUMP-EDGE, not a walk — normal
/// adjacent nav cells are ≤ 8·√2 ≈ 11.3u apart, jump-edges span ≥ 16u across a real gap. The walker
/// asks the controller to jump when traversing such a segment. (eqoxide#190)
pub const JUMP_SEG_MIN: f32 = 12.0;
/// Only fire the jump while within this of the takeoff waypoint — so the leap starts grounded at
/// the near edge and does NOT re-trigger after landing (just under the 8u nav cell). (eqoxide#190)
pub const JUMP_TAKEOFF_DIST: f32 = 7.0;
// The planner itself now lives on its own thread — see `crate::planner`. `plan_path`
// moved there wholesale: it used to run SYNCHRONOUSLY here, on the network thread, which is the
// single root cause of #340 (up to ~2 s of net-thread stall → linkdead) and #337 (the 150 ms budget
// forced A* to give up, and a give-up was indistinguishable from "no route", so the walker silently
// drove a partial route into a wall and froze). `ActionLoop::tick` now POSTS a request and returns.

/// A chase goal must move at least this far (one nav cell) before it counts as a different goal
/// worth re-planning for. `/follow` and `/goto <entity>` rewrite the goal with the leader's LIVE
/// position EVERY TICK, so an exact compare called it "changed" ~every tick (#377 review, B1).
pub const GOAL_REPLAN_DIST: f32 = 8.0;
/// A goal that moves further than this is a different DESTINATION, not a drifting one: the committed
/// route is thrown away, the journey counters reset, and any in-flight plan is superseded.
pub const GOAL_RESET_DIST: f32 = 40.0;

/// What a tick should do about (re)planning. Pure, so the `/follow` freeze below is unit-testable
/// without a live `EqStream`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Replan {
    /// Post a fresh plan request to the worker.
    pub post: bool,
    /// The goal is somewhere else entirely — drop the committed route and the recovery budget.
    pub reset_route: bool,
}

/// Decide whether to post a new coarse plan this tick.
///
/// **This exists because of the `/follow` freeze.** A chase goal is rewritten with the leader's live
/// position every tick, so `path_goal != Some(goal)` (an exact f32 compare) was true ~every tick
/// while the leader moved. Each tick therefore posted a fresh plan, which superseded the previous
/// generation's reply *before it could land*, cleared the route, and stopped the walker — so a
/// `/follow` of a MOVING leader never got a route at all and simply stood there. When the plan ran
/// inline this was invisible: the walker always had a route the same tick it asked.
///
/// Two thresholds fix it:
/// * a goal that drifts less than `GOAL_REPLAN_DIST` is the SAME goal — don't re-plan at all;
/// * while a plan is IN FLIGHT, don't supersede it unless the goal has moved further than
///   `GOAL_RESET_DIST` — otherwise a leader walking at run speed re-posts faster than the planner
///   can ever answer, and no reply ever survives to be applied.
///
/// `planned_goal` is the goal the committed/incoming route is for; `in_flight` is the goal of the
/// plan currently computing, if any.
/// `is_chase` = the goal is an ENTITY we are following (`/follow`, `/goto <name>`), not a fixed
/// point. That distinction is what makes this sound: a leader who runs 500u away is still the SAME
/// goal, so its route must never be thrown away for "moving too far" — whereas a fresh `/goto` to a
/// point 500u away IS a different destination and the old route must go.
pub fn replan_decision(
    planned_goal: Option<(f32, f32, f32)>,
    goal: (f32, f32, f32),
    in_flight: Option<(f32, f32, f32)>,
    replan_coarse: bool,
    is_chase: bool,
) -> Replan {
    let moved = |a: (f32, f32, f32)| -> f32 {
        ((a.0 - goal.0).powi(2) + (a.1 - goal.1).powi(2) + (a.2 - goal.2).powi(2)).sqrt()
    };
    let drift = planned_goal.map_or(f32::MAX, moved);
    // A chase goal is never a "new destination", however far the leader runs — dropping the route
    // and freezing the walker every time a fleeing leader crosses the threshold is the same #377/B1
    // freeze wearing a different hat.
    let reset_route = !is_chase && drift > GOAL_RESET_DIST;
    let want = drift > GOAL_REPLAN_DIST || replan_coarse;
    let may_post = match in_flight {
        None => true,
        // NEVER supersede an in-flight plan for a chase. The leader moves every single tick, so a
        // plan that is always superseded never lands and the walker never gets a route at all. Let
        // it finish; the next tick re-plans from the leader's newer position.
        Some(_) if is_chase => false,
        // For a fixed goal, only supersede when the goal really has moved on (its answer would be
        // worthless anyway); otherwise let it land.
        Some(f) => moved(f) > GOAL_RESET_DIST,
    };
    Replan { post: want && may_post, reset_route }
}

/// May an UNREACHABLE goal be escaped to via an in-zone translocator (#266)? Only when a teleport
/// could conceivably help: we are WALLED OFF from a goal that does exist (`SearchClosed`), or the
/// character itself is boxed in (`StartIsolated`). A goal with no walkable floor under it is not
/// somewhere any portal leads — redirecting there is nonsense, and worse, it replaces the agent's
/// real reason (`goal_not_walkable` — *fix your coordinates*) with the portal's.
pub fn portal_escape_applies(why: crate::collision::NoRoute) -> bool {
    use crate::collision::NoRoute;
    matches!(why, NoRoute::SearchClosed | NoRoute::StartIsolated)
}

/// What the walker should do on reaching (near) its goal, kept pure so the follow-vs-goto distinction
/// is unit-tested off the tick. `Arrived` = a one-shot /goto is done → stop for good. `FollowHold` = a
/// /follow chase has caught up → stand near the leader but STAY latched so it re-engages when the
/// leader moves (#268). `Drive` = not there yet → keep walking.
#[derive(Debug, PartialEq, Eq)]
pub enum ArrivalAction { Drive, Arrived, FollowHold }

/// Arrival radius for a one-shot /goto (melee range is ~14u, so 2u keeps us well inside it).
pub const STOP_DIST: f32 = 2.0;
/// A /follow settles up to this far behind the leader (a bit behind, still in group range).
pub const FOLLOW_DIST: f32 = 10.0;

/// Vertical arrival tolerance — the walker must be on the goal's FLOOR, not a floor above/below it.
/// This is the SAME tolerance `astar` uses to accept a searched cell as the goal tier, so the
/// arrival predicate and the pathfinder agree on "the right floor" by construction (#344). See the
/// const's own doc for why 8u distinguishes floors without rejecting standing height / step-ups.
pub const Z_ARRIVAL_TOL: f32 = crate::collision::GOAL_TIER_TOL;

/// Stop within 2u for a one-shot /goto; a /follow settles up to FOLLOW_DIST behind the leader.
///
/// `gdz` is the SIGNED vertical gap to the goal's resolved floor (`goal_floor_z − player_z`).
/// Arrival/hold requires being on that floor: `|gdz| ≤ Z_ARRIVAL_TOL`. Correct x/y at the wrong z
/// (the NPC one storey up, #344) is NOT arrival — it stays `Drive`, so the client keeps navigating
/// (climbing toward a reachable floor) or, when the floor is unreachable, runs on into the walker's
/// existing honest `blocked`/`no_path` terminal states — never a false `arrived`/`following`.
pub fn arrival_action(gdist: f32, gdz: f32, following: bool) -> ArrivalAction {
    // Wrong FLOOR: correct horizontally but a storey off. Never report arrival/hold here — the
    // agent must not be told it reached a goal it is a floor away from (#344, agent-honesty).
    if gdz.abs() > Z_ARRIVAL_TOL {
        return ArrivalAction::Drive;
    }
    if following {
        if gdist <= FOLLOW_DIST { ArrivalAction::FollowHold } else { ArrivalAction::Drive }
    } else if gdist <= STOP_DIST {
        ArrivalAction::Arrived
    } else {
        ArrivalAction::Drive
    }
}

/// How far off its CURRENT segment the character may drift before the coarse-route cursor is
/// treated as STALE and resynced (see [`resync_cursor`]). One coarse cell (8 u) is the natural
/// scale: the coarse route is planned on an 8 u grid, so a character genuinely traversing segment
/// `path_i` is always well inside this, while a character that has been carried onto a *different*
/// part of the route (a fall, a slide down a ramp) is far outside it. Deliberately generous —
/// ordinary corner-cutting and server position jitter must never trip a resync.
///
/// **The comparison is `<`, not `<=`, and that is measured, not cosmetic (#727 round 2).** The
/// cycle #673 describes has an ATTRACTING FIXED POINT sitting exactly ON this boundary: on a
/// hairpin whose legs are one coarse cell apart the cursor/carrot loop converges to a body offset of
/// exactly 8.0 u and parks there. With `<=` that state is *inside* the guard, so the DISTANCE
/// trigger never fires there — measured in round 2 as 1 of 8 swept starts still CARROT-PINNED after
/// 400 ticks, and 33 of 288 on the wider 8 u-separation sweep. With `<` it is outside, and both went
/// to zero.
///
/// **Since #733 that flip is no longer observable, and no test enforces the token** — see
/// `the_deadlock_fixed_point_exactly_on_the_guard_boundary_is_resynced`'s rustdoc, which records
/// the re-run. The second trigger catches the same fixed point, so `<` survives on the
/// round-2 reasoning alone. Read the counts above as a measurement of this constant's trigger in
/// isolation, not as a live mutation check.
///
/// **What this constant does NOT do — the residual class, stated so nobody re-derives it as a
/// surprise.** Below the guard this trigger is inert by construction, so a route whose legs are
/// closer together than `CURSOR_STALE_DIST` was still able to form the #673 cycle. Measured on the
/// hairpin sweep with this as the SOLE trigger (carrot-pinned starts / total): 8 u and above →
/// 0 pinned; 7 u → 252/252; 6 u → 216/216; 4 u → 144/144, i.e. below the guard this trigger is not
/// partial, it is absent. The real deadlock invariant is CARROT COLLAPSE, and a distance guard is
/// only a proxy for it — wrong on exactly the routes whose legs are closer together than the guard.
/// Do not read this constant as pinning a value, it is unpinned over at least [2, 16].
///
/// **#733 did not widen this constant; it added a second, threshold-free trigger beside it.**
/// [`resync_cursor`] now also fires when [`carrot_leads`] is false — the collapse measured directly
/// as an arclength comparison rather than inferred from a distance. Widening `CURSOR_STALE_DIST`
/// could not have worked: the sub-guard cycle is a body 4–7 u from its own segment, so any constant
/// small enough to catch it also snaps a walker legitimately cutting a tight switchback
/// (`a_walker_cutting_a_tight_switchback_keeps_its_cursor`, a body 1.5 u from its segment and 0.5 u
/// from a later one, whose carrot still leads and whose cursor must not move). The two triggers
/// answer different questions and the sweep counts above are what this one, alone, is worth.
///
/// **The LIMIT on every count above.** They all come from `hairpin_carrot_stops_leading`, a loop
/// that steps the body straight at `local_goal` (24 u). That is not how `drive_walk` moves, so the
/// loop measures CARROT PINNING soundly — it is pure function composition — but it cannot measure
/// whether a walker WEDGES. Read them as pinning counts only. (This does not weaken the `<`
/// finding, which is about the fixed point sitting on the boundary, not about what the loop's
/// outcome is called.) A round-1 review figure — **133/1649 at 8 u** — was retracted by the round-2
/// reviewer on exactly this ground, being the same 24 u-step model on a denser grid. It is recorded
/// here because this is the only surviving copy of that retraction; an earlier revision said the
/// pair was preserved elsewhere, and that pointer was false when written.
pub const CURSOR_STALE_DIST: f32 = 8.0;

/// The furthest a resync may reach: a candidate segment whose closest point is further than this
/// from the character is never adopted, however clear the line to it (#727 round 2).
///
/// Two reasons, one honest and one practical.
///
/// * **Honest.** The invariant [`resync_cursor`] restores is *"`path_i` names the segment the
///   character is actually on"*. A segment 60 u away is not a segment the character is on, whatever
///   a straight-line predicate says about it. 24 u is the walker's own `LOCAL_REACH` — the horizon
///   its fine planner is re-planned over every tick — so "inside the walker's local horizon" is the
///   widest reading of "on" that the rest of the walker already commits to.
/// * **Practical.** It bounds the cost of the geometry predicate, which the round-1 review flagged
///   as unprofiled: the reachability test now includes column probes at ~2 u spacing, and this caps
///   them at ~12 per candidate instead of running the length of a 141-waypoint route.
pub const CURSOR_RESYNC_MAX_HOP: f32 = 24.0;

/// The reach `drive_walk` builds its `local_goal` with — the point on the coarse route it hands the
/// FINE planner as a destination — and therefore the reach whose collapse [`carrot_leads`] measures.
///
/// It lives here, `pub`, for one reason: `drive_walk` and [`resync_cursor`] must be talking about
/// **the same carrot**. A private copy in each would agree today by coincidence and drift silently,
/// and a drifted copy makes the collapse check answer a question about a carrot production never
/// builds — the guard would still pass, on the wrong carrot. `walker::Walker::drive_walk` and the
/// offline `fixture_run` harness both read this constant rather than restating 24.0.
pub const LOCAL_REACH: f32 = 24.0;

/// Squared 3-D distance from `p` to segment `a`→`b`, plus the closest point on it.
fn seg_closest(a: [f32; 3], b: [f32; 3], p: [f32; 3]) -> ([f32; 3], f32) {
    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
    let t = if l2 < 1e-6 {
        0.0
    } else {
        (((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1] + (p[2] - a[2]) * ab[2]) / l2).clamp(0.0, 1.0)
    };
    let c = [a[0] + ab[0] * t, a[1] + ab[1] * t, a[2] + ab[2] * t];
    let d = [p[0] - c[0], p[1] - c[1], p[2] - c[2]];
    (c, d[0] * d[0] + d[1] * d[1] + d[2] * d[2])
}

/// The three arclengths carrot collapse is a comparison between, all measured along `path` from
/// `path[start_i]` (#733). Returns `None` when `start_i` names no segment, so there is nothing to
/// measure.
///
/// * `.0` — **the carrot's ORIGIN**: the arclength of the projection of `from` onto segment
///   `start_i`. This is exactly where [`carrot_along`] starts spending its `reach` budget, and the
///   whole defect is that it is a point on the segment the cursor NAMES, not a point near the body.
/// * `.1` — **the BODY**: the arclength of the point on `path[start_i..]` genuinely nearest `from`.
///   Ties resolve to the EARLIER segment (`<`, not `<=`), which is the conservative half of the tie:
///   an earlier reading of where the body is makes a collapse HARDER to declare, never easier.
/// * `.2` — the total length of `path[start_i..]`, which is the cap [`carrot_along`] clamps the
///   carrot at once the route runs out before the budget does.
///
/// The forward-only scan (`start_i..`) is deliberate and matches [`resync_cursor`]'s guard 1: a body
/// nearest to a part of the route BEHIND the cursor has not collapsed anything — its carrot still
/// leads — and rewinding the cursor is a thing this module never does.
fn cursor_arclengths(path: &[[f32; 3]], start_i: usize, from: [f32; 3]) -> Option<(f32, f32, f32)> {
    if start_i + 1 >= path.len() {
        return None;
    }
    let (mut s, mut s_proj, mut s_near, mut near_sq) = (0.0f32, 0.0f32, 0.0f32, f32::INFINITY);
    for i in start_i..(path.len() - 1) {
        let (a, b) = (path[i], path[i + 1]);
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let seg_len = (ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2]).sqrt();
        let (c, d_sq) = seg_closest(a, b, from);
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let along = (ac[0] * ac[0] + ac[1] * ac[1] + ac[2] * ac[2]).sqrt();
        if i == start_i {
            s_proj = along;
        }
        if d_sq < near_sq {
            near_sq = d_sq;
            s_near = s + along;
        }
        s += seg_len;
    }
    Some((s_proj, s_near, s))
}

/// **Does the pure-pursuit carrot off `start_i` actually LEAD the body? (#733)**
///
/// This is the deadlock invariant of #673 measured DIRECTLY, rather than through the distance proxy
/// [`CURSOR_STALE_DIST`]. [`carrot_along`] spends its `reach` budget from the projection of the body
/// onto segment `start_i`. When the cursor is honest that projection IS the body's own place on the
/// route, so the carrot lands `reach` further along and leads by construction. When the cursor names
/// a segment the body is merely BESIDE, the budget is spent on a phantom leg the body has already
/// left — and if that phantom leg is longer than the whole budget, the carrot lands **at or behind
/// the body's own point on the route**. There is then no aim that leaves the spot.
///
/// So the predicate is one comparison of arclengths, with **no threshold of its own**:
///
/// ```text
/// min(s_projection + reach, total) > s_body
/// ```
///
/// `reach` is not a tuning knob — it is the budget of the very carrot being judged, and passing
/// anything other than the reach the caller will really use measures a different carrot. Production
/// passes [`LOCAL_REACH`], the reach `drive_walk` builds `local_goal` with, because `local_goal` is
/// the carrot #727 measured collapsing (0.21 u ahead on the captured #673 fixture).
///
/// **What it does NOT measure**, so nobody over-quotes it:
///
/// * It judges the UNCLAMPED carrot. [`carrot_along_los`] can only ever SHORTEN the carrot, so a
///   carrot that leads here may still fail to lead once a corner clamps it. This predicate therefore
///   under-detects in the presence of geometry; it never over-detects.
/// * It says nothing about walkability. A body 0.1 u from a later leg with a wall between is
///   "nearest" to that leg here. Acting on the answer is [`resync_cursor`]'s job, and it is that
///   function's `reachable` predicate — not this one — that refuses to cross geometry.
/// * A body whose nearest point is the path's FINAL vertex reports "does not lead" (the carrot is
///   clamped to that same vertex). That is a body at the goal, which is arrival's business, not
///   steering's. That edge, and the rule that an unmeasurable cursor is not evidence of a collapse,
///   are pinned by `carrot_leads_is_honest_at_the_route_end_and_where_it_can_measure_nothing`.
///
/// The three arclengths are checked against their definitions by
/// `the_three_arclengths_are_the_points_they_claim_to_be`, and the claim that
/// `min(s_projection + reach, total)` really is where [`carrot_along`] lands — the drift that would
/// leave this guard passing on a carrot production never builds — by
/// `carrot_leads_judges_the_carrot_the_production_code_actually_builds`.
pub fn carrot_leads(path: &[[f32; 3]], start_i: usize, from: [f32; 3], reach: f32) -> bool {
    match cursor_arclengths(path, start_i, from) {
        // Nothing to measure is not evidence of a collapse. Never claim one we cannot see.
        None => true,
        Some((s_proj, s_near, total)) => (s_proj + reach).min(total) > s_near,
    }
}

/// **The reachability predicate production drives [`resync_cursor`] with — ONE definition, every
/// caller (#734).**
///
/// A conjunction of two `Collision` primitives, both deliberately CENTRE-line:
///
/// * `Collision::carrot_los_clear` at the walker's `STEER_LOS_CLEARANCE` — a chest-height centre
///   ray, so a WALL between the body and the candidate refuses the hop.
/// * `Collision::ground_continuous` — a floor-column probe along the hop, so a VOID, or a drop
///   outside the controller's slope+step envelope, refuses it. **A hole is not a wall**: #727's
///   round-1 review broke the LOS ray alone with a 200 u chasm the chest ray flew straight over.
///
/// It is a named function rather than a closure spelled out per call site because it *was* spelled
/// out at three of them — `Walker::advance_cursor`, `cursor_resync_tests::fixture_run` below, and
/// `tests/walker_sim.rs` — and #887's round-1 review measured what that costs: changing the
/// walker's copy left the other two modelling a predicate production no longer ran, while both of
/// their doc comments went on saying they ran "the walker's own predicate". `pub`, not
/// `pub(crate)`, specifically so the integration harness can call it: `tests/walker_sim.rs` also
/// hand-copies the clearance today (its own ⚠️ Correction discloses that the copy agrees with
/// `STEER_LOS_CLEARANCE` by coincidence rather than by construction), and this function closes both
/// holes for whoever edits that file next.
///
/// ## Why BOTH halves are centre-line — and why the floor half must not be widened to the body
///
/// #734 names the floor probe "width-blind": a floor strip narrower than the body reads exactly
/// like a body-width crossing, because only the centre is ever sampled. Widening it to three
/// parallel lines at `-radius / 0 / +radius` was implemented, measured, and **withdrawn**. The
/// reason is that the width-blindness is *faithful*, not a defect:
///
/// **The consumer has no shoulders.** `CharacterController`'s floor clamp is a single column under
/// the body's CENTRE — in `src/movement.rs` it is
/// `ground_below(self.pos[0], self.pos[1], foot + GROUND_ORIGIN, GROUND_DEPTH)`, with no
/// `±radius` term in either the grounded or the levitating arm. A body whose shoulder overhangs a
/// ledge lip is supported and walks normally. A floor test at `±PLAYER_RADIUS` therefore models a
/// body production does not have, and every hop it newly refuses is one the controller would in
/// fact have walked.
///
/// Measured — the real `Walker::advance_cursor` on the `CHASM_ROUTE`/`CHASM_BODY` fixture, cursor
/// starting at 2, alongside the controller's own `ground_below` sampled every 0.5 u along the same
/// hop. Reproduced by `crate::walker`'s
/// `a_resync_must_still_cross_ground_the_controller_can_stand_on`:
///
/// ```text
/// fixture                            controller floor    centre-line    ±radius swept
/// slab edge 0.5 u from the hop line   standable, all      2 -> 6         2  (REFUSED)
/// slab edge 1.0 u from the hop line   standable, all      2 -> 6         2 -> 6
/// 0.8 u ridge, void either side       standable, all      2 -> 6         2  (REFUSED)
/// 1.9 u ridge, void either side       standable, all      2 -> 6         2  (REFUSED)
/// 2.1 u ridge, void either side       standable, all      2 -> 6         2 -> 6
/// ```
///
/// The refusal band is exactly "floor narrower than the body's diameter", and in every row of it
/// the controller's floor model is satisfied at every sample. A predicate stricter than the thing
/// it models does not fail safe: it reports an ordinary reachable state as UNREACHABLE, which under
/// this project's agent-honesty invariant is a wrong answer in the same way a false acceptance is,
/// and it withholds the #673 resync precisely on the geometry #673 was observed live on.
///
/// The repo has measured this direction severe once already, on the planner side:
/// `Collision::edge_clear`'s rustdoc records that sweeping the body volume along a coarse edge
/// "does not reject *unwalkable corridors*, it rejects *corridors*" — routable pairs 876 → 813,
/// Ak'Anon 90/120 → 55/120. The resync's candidates come off that same coarse route.
///
/// The wall half is centre-line for its own separately measured reason, which points the same way:
/// `carrot_los_clear`'s rustdoc records that a more aggressive variant "slowed the walker 5-8x and
/// newly FAILED dozens of routes (measured)".
///
/// **So this is ACCEPTED on purpose and is not a defect of this predicate:** a knife-edge ridge
/// with void either side reads as a clean crossing, because to the controller's floor model it *is*
/// one. What is genuinely not modelled is whether the walker can STEER accurately enough to stay on
/// such a ridge — a different question from floor continuity, unmeasured here, and the reason
/// `Walker::advance_cursor` raises `stuck_i` on every resync jump instead of trusting this answer.
///
/// Not-a-regression guard: `a_resync_must_still_cross_ground_the_controller_can_stand_on` goes RED
/// if the floor half is ever widened to a `±STEER_LOS_CLEARANCE` sweep (mutation-checked in both
/// directions).
pub fn resync_reachable(col: &crate::collision::Collision, from: [f32; 3], to: [f32; 3]) -> bool {
    let clearance = crate::walker::STEER_LOS_CLEARANCE;
    col.carrot_los_clear(from, to, clearance) && col.ground_continuous(from, to)
}

/// **Resync a stale coarse-route cursor (#673).**
///
/// The walker advances `path_i` monotonically, one segment at a time, and only when the character's
/// projection parameter on the CURRENT segment reaches 1.0. That rule silently assumes the character
/// travels along the route. Physics does not honour that assumption: a fall, or a slide down a ramp,
/// can carry the character *past* several waypoints in one step, landing it beside a segment whose
/// projection parameter then saturates strictly below 1.0 — forever. The cursor is now pointing at a
/// segment the character is nowhere near, and every consumer of it is computing against a false
/// premise:
///
/// * [`carrot_along`] measures the carrot's arclength budget from the projection onto that stale
///   segment, so the budget is eaten by a phantom leg. What that collapses is **`local_goal`** — the
///   `LOCAL_REACH` (24 u) point `drive_walk` hands the FINE planner — to ~0.2 u from the body. The
///   *steering* carrot at `LOOK_AHEAD` (5 u) does **not** collapse; off the same stale cursor it
///   still leads by ~17 u. The collapse reaches the steering aim one step later: `find_path_local`
///   returns a degenerate two-waypoint stub, [`steer_target`] prefers the fine path at exactly
///   `len() >= 2` rather than discarding it as too short, and the 5 u carrot taken *along that stub*
///   is inside one controller frame of travel (`RUN_SPEED * 0.01 = 0.44 u`), so it is overshot and
///   the aim flips **each frame** — 15 times per 150 ms nav tick.
/// * The stall detector watches `path_i`, which can now never advance.
///
/// The result is a stable limit cycle with zero net progress on a route the character is physically
/// standing on. (Observed live in South Qeynos on the qcat aqueduct ramp — `blocked` /
/// `walker_stalled` at `[-534.4, 144.4, -6.0]` on 6 of 8 attempts; reproduced offline from the
/// captured live route by the three `#673 step N of 3` tests in `steering::cursor_resync_tests`.)
///
/// > ## ⚠️ The LIMITS of the figures above
/// >
/// > * **The offline fixture is not production.** `fixture_run`'s settled band is **1.32 u**
/// >   per frame (0.88 u sampled once per nav tick); the production `drive_walk` log oscillates
/// >   over **~2.6 u** on the same fixture. Use these as evidence for the MECHANISM, not for
/// >   production amplitudes.
/// > * **The cycle is not terminal here.** No offline instrument on this branch contains the stall
/// >   detector, `NAV_STUCK_TICKS` backoff or re-plan. Driven through the production `drive_walk`
/// >   the walker sits in the cycle ~22 nav ticks, re-plans, and **arrives**. #673 is terminal on
/// >   real terrain, which is a property of the terrain and not of this mechanism.
///
/// This moves the cursor toward its invariant — *`path_i` names the segment the character is
/// actually on* — under three hard guards that keep it strictly conservative:
///
/// 1. **Forward only.** The scan starts at `start_i`; the cursor can never move backwards, so a lap
///    can never be un-counted and the #631/#309 progress channels keep their meaning.
/// 2. **Inside the local horizon.** A candidate whose closest point is further than
///    [`CURSOR_RESYNC_MAX_HOP`] from the character is never adopted (see that constant).
/// 3. **Only onto a segment `reachable(from, closest)` accepts.**
///
/// ## What guard 3 does and does not establish — read this before trusting it
///
/// `reachable` is a caller-supplied predicate and this function makes **no** claim about
/// walkability on its own. Production passes [`resync_reachable`] — a conjunction of a chest-height
/// line-of-sight ray (excludes WALLS) and a floor-column probe along the hop (excludes VOIDS and
/// drops steeper than the controller's own slope+step envelope). That pairing exists because the
/// round-1 review broke the LOS ray alone with a counterexample and it is worth stating plainly:
/// **a hole is not a wall.** `Collision::carrot_los_clear` is documented in its own rustdoc as a
/// chest-height centre ray, chosen deliberately to ride ABOVE ground undulation; asked "has the
/// character reached this segment" it flies straight over a chasm. Measured: two ledges split by a
/// 10 u gap with the next floor 200 u down, and the LOS ray alone moved the cursor 2 → 6, declaring
/// an entire bridge detour walked (`crate::walker`'s
/// `a_resync_must_not_cross_a_chasm_the_character_cannot_walk`).
///
/// Even with the floor probe this is a **necessary, not a sufficient** condition — read
/// `true` as *"not proven unreachable"*, never as *"reachable"*. #734 gaps 1 and 2 bear directly
/// on this predicate; #734 also files further gaps against `Collision::ground_continuous` itself
/// (see `collision.rs`) that this rustdoc does not enumerate. Of gaps 1 and 2, one is live and one
/// has been withdrawn on measurement:
///
/// * **Line-sampled (#734 gap 1) — MEASURED, NOT FIXED, still live.** The floor probe samples the
///   column at `PROBE_SPACING` (2 u) intervals along the hop, so a hole narrower than that can fall
///   between two probes and is invisible. `crate::walker`'s
///   `a_narrow_hole_between_probes_still_crosses_the_resync_undetected` measures it directly: a
///   1.5 u hole placed inside one probe interval is stepped over by the production predicate.
///   **No fix is attempted here** — closing it means changing `PROBE_SPACING` or replacing the line
///   sample with an exact analytic test inside `Collision::ground_continuous` itself, in
///   `collision.rs`, which this change does not touch.
/// * **Width-blind (#734 gap 2) — WITHDRAWN as a defect, on measurement (#887).** The floor probe
///   samples the centre only and so cannot tell a body-width crossing from a knife-edge ridge. That
///   is not a gap between this predicate and production, it is *agreement* with it: the controller's
///   floor clamp is also a single centre column. A three-line `±PLAYER_RADIUS` sweep was built and
///   measured to refuse hops whose floor the controller stands on at every sample — a false refusal,
///   which under the agent-honesty invariant is as wrong as a false acceptance. The numbers, the
///   `edge_clear` precedent (876 → 813 routable pairs), and the regression guard that keeps the
///   sweep out are on [`resync_reachable`]'s rustdoc. **#734's gap-2 framing is superseded by that
///   measurement**; a comment recording the retraction is on the issue, so a reader of the issue and
///   a reader of this file are not told different things.
///
/// So the honest statement of what a resync means is *"the character is within
/// [`CURSOR_RESYNC_MAX_HOP`] of this segment, with no wall and no sampled void between"* — **not**
/// "the character walked this leg". Which is why the walker deliberately does not report a resync
/// jump as PROGRESS: see `Walker::advance_cursor`.
///
/// ## When it fires — two triggers, not one (#733)
///
/// The cursor is left alone only when **neither** trigger fires:
///
/// * **DISTANCE** (#727) — the body is at least [`CURSOR_STALE_DIST`] from the segment `path_i`
///   names. This is a *proxy*: "far from its own segment" correlates with the deadlock and is not
///   it, and the correlation breaks down entirely below one coarse cell.
/// * **CARROT COLLAPSE** (#733) — [`carrot_leads`] is false, i.e. the carrot built off `path_i` at
///   [`LOCAL_REACH`] lands at or behind the body's own point on the route. This is the invariant
///   itself. It is what catches the class the distance trigger is structurally blind to, a cycle
///   whose whole geometry fits inside 8 u; measured, the hairpin sweep's 4/6/7 u columns go from
///   144/144, 216/216, 252/252 carrot-pinned to 0
///   (`the_resync_clears_the_carrot_pinning_at_every_leg_separation_measured`).
///
/// Neither trigger has any say in *where* the cursor goes — that is the candidate loop and its three
/// guards above, unchanged. A trigger only decides whether the loop is allowed to run at all, so a
/// spurious trigger costs a scan and cannot move the cursor anywhere the guards would refuse.
///
/// **The normal case is still untouched and still pays no geometry.** [`carrot_leads`] is pure
/// arithmetic over the route; an on-route walker fails both triggers and returns before `reachable`
/// is ever consulted (`an_on_route_walker_is_left_alone_without_consulting_geometry`).
///
/// **What the second trigger buys, as a universal rather than an example.** With a clear predicate,
/// a body whose nearest point is inside [`CURSOR_RESYNC_MAX_HOP`] and has route left beyond it comes
/// out of this function with a leading carrot — swept over seven route shapes, every cursor and a
/// body grid by `after_a_resync_with_clear_geometry_the_carrot_always_leads`, which also asserts the
/// sweep actually contains collapsed inputs. The #673 cycle itself is pinned as one arithmetic
/// example by `the_sub_guard_hairpin_fixed_point_resyncs_though_the_distance_trigger_cannot_see_it`.
pub fn resync_cursor(
    path: &[[f32; 3]],
    start_i: usize,
    from: [f32; 3],
    reachable: impl Fn([f32; 3], [f32; 3]) -> bool,
) -> usize {
    // A cursor needs at least one segment ahead of it to be resyncable; `path_i + 2 <= len` mirrors
    // the walker's own advance bound so a resync can never park the cursor past the last segment.
    if path.len() < 3 || start_i + 2 >= path.len() {
        return start_i;
    }
    let (_, d0_sq) = seg_closest(path[start_i], path[start_i + 1], from);
    // TWO independent triggers; the cursor is left alone only when NEITHER fires.
    //
    //  * DISTANCE (#727) — the body is further than one coarse cell from the segment `path_i` names.
    //    `<`, not `<=`: the cycle's fixed point sits exactly ON the boundary, see CURSOR_STALE_DIST.
    //  * CARROT COLLAPSE (#733) — the carrot measured off `path_i` lands at or behind the body's own
    //    point on the route. This is the deadlock invariant itself rather than a correlate of it,
    //    and it is what catches the class the distance trigger is structurally blind to: a cycle
    //    whose whole geometry fits INSIDE the guard. It costs the normal case nothing extra in
    //    geometry queries — `carrot_leads` is pure arithmetic and `reachable` is still consulted
    //    only from the candidate loop below.
    if d0_sq < CURSOR_STALE_DIST * CURSOR_STALE_DIST
        && carrot_leads(path, start_i, from, LOCAL_REACH)
    {
        return start_i;
    }
    let hop_sq = CURSOR_RESYNC_MAX_HOP * CURSOR_RESYNC_MAX_HOP;
    let (mut best_i, mut best_sq) = (start_i, d0_sq);
    for i in (start_i + 1)..(path.len() - 1) {
        let (c, d_sq) = seg_closest(path[i], path[i + 1], from);
        // Cheap tests first — the geometry predicate is only consulted for a candidate that both
        // improves on the current best AND is inside the reach band.
        if d_sq < best_sq && d_sq <= hop_sq && reachable(from, c) {
            best_i = i;
            best_sq = d_sq;
        }
    }
    best_i
}

/// A pure-pursuit carrot: the point `reach` units (3D arclength) along `path` (starting from segment
/// `start_i`), measured from the 3D projection of `from` = `[east, north, z]` onto that segment.
/// Returns `[east, north, z]` INTERPOLATED at the carrot point. Used at two scales: a far carrot
/// (~LOCAL_REACH) as the fine plan's goal, and a near carrot (LOOK_AHEAD) along the fine plan as the
/// steering aim; its z is also the depth target the water-nav depth controller (`swim_vspeed`) holds.
///
/// **3D (water-nav Slice 3, design §8.1).** The projection, the arclength, and the returned z are all
/// 3D. This is load-bearing for a diving/ascending water leg: such a segment is (near-)vertical in XY,
/// so the OLD XY-only math gave it zero length, jumped the carrot straight to the next waypoint's z,
/// and (with the cursor) consumed the descent on frame one — the walker would then drive HORIZONTALLY
/// into the shaft wall instead of swimming DOWN it. On near-horizontal LAND segments 3D ≡ 2D (the z
/// contribution vanishes) and the interpolated z equals the segment z, so land steering is unchanged.
pub fn carrot_along(path: &[[f32; 3]], start_i: usize, from: [f32; 3], reach: f32) -> Option<[f32; 3]> {
    // The unclamped carrot is exactly the LOS-clamped one with an always-clear predicate — byte for
    // byte, by construction — so callers with no collision (and every existing test) are unaffected.
    carrot_along_los(path, start_i, from, reach, |_, _| true)
}

/// A pure-pursuit carrot, **LINE-OF-SIGHT CLAMPED**: the furthest point up to `reach` arclength along
/// `path` (from segment `start_i`) whose STRAIGHT segment from `from` stays clear per `los`.
///
/// This is [`carrot_along`] with a corner guard, and it exists because the plain carrot **cuts convex
/// corners** (#685). The plain carrot advances a fixed arclength ahead regardless of geometry, so
/// where the committed path bends around a convex corner (walking around the outside of a wall) the
/// straight walker→carrot aim is the **chord across the corner** — it crosses the wall on the inside
/// of the turn. The walker drives into the wall, makes no forward progress, trips the stall detector,
/// and wedges (the live qcat L-corner, which the `PROACTIVE_REPLAN_CAP` machinery above only ever
/// worked *around* via replan cooldowns, never fixed). Here the carrot only advances while
/// `los(from, candidate)` holds; at a corner it stops at the last clear point, so the walker steers at
/// the corner and **rounds** it (following the path's own waypoints) instead of cutting through.
///
/// `los(a, b)` answers *"can the character travel straight from a to b without crossing geometry"* —
/// production callers pass `Collision::path_clear` (the SAME volume-sweep the controller moves under
/// and A* validates fine edges with, #358). It is a closure so this stays pure/testable and so the
/// no-geometry case (`los` always-true) reduces to the old `carrot_along` exactly.
///
/// **Not over-tightening — the dominant risk (#685).** `path_clear` is blind to a wall the segment
/// runs ALONGSIDE (a ray parallel to a plane never intersects it — `collision.rs`'s documented LIMIT),
/// so merely hugging a straight corridor wall does NOT trip the clamp; only a segment that actually
/// CROSSES geometry does, which is exactly the corner cut. A clear straight shot therefore keeps the
/// full `reach`, and gentle bends whose chord stays in open space are unchanged.
pub fn carrot_along_los(
    path: &[[f32; 3]], start_i: usize, from: [f32; 3], reach: f32,
    los: impl Fn([f32; 3], [f32; 3]) -> bool,
) -> Option<[f32; 3]> {
    let a = *path.get(start_i)?;
    let b = path.get(start_i + 1).copied().unwrap_or(a);
    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
    let t = if l2 < 1e-6 { 0.0 }
        else { (((from[0] - a[0]) * ab[0] + (from[1] - a[1]) * ab[1] + (from[2] - a[2]) * ab[2]) / l2).clamp(0.0, 1.0) };
    let mut cur = [a[0] + ab[0] * t, a[1] + ab[1] * t, a[2] + ab[2] * t];
    // The nearest path point is the LOS FLOOR: the walker is essentially on it, so it is always a
    // valid aim. Seeding `best` here UNCHECKED is what stops the clamp from ever STALLING the walker —
    // even with everything ahead blocked it aims forward onto the path, never at a point behind itself
    // (pinned by the never-stall property test with an all-blocking `los`). It also makes the aim
    // MONOTONE: the clamp can only ever shorten the reach, never retreat behind where we already are.
    let mut best = cur;
    let (mut rem, mut i) = (reach, start_i);
    loop {
        match path.get(i + 1).copied() {
            Some(bp) => {
                let d = [bp[0] - cur[0], bp[1] - cur[1], bp[2] - cur[2]];
                let dl = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
                let last = dl >= rem || i + 2 >= path.len();
                let cand = if last {
                    if dl < 1e-6 { cur }
                    else { let s = (rem / dl).min(1.0); [cur[0] + d[0] * s, cur[1] + d[1] * s, cur[2] + d[2] * s] }
                } else { bp };
                // Advance the accepted carrot only while the straight shot to it stays clear. The
                // instant it would cross geometry (a corner), stop at the last clear point — the walker
                // aims at the corner and rounds it. `best` may already be the LOS floor `cur`.
                if los(from, cand) { best = cand; } else { break; }
                if last { break; }
                rem -= dl; cur = bp; i += 1;
            }
            None => break,
        }
    }
    Some(best)
}

/// Max commanded vertical swim speed (u/s), kept UNDER the controller's `BUOY_RATE` (30) so a carrot
/// at the swim plane lets buoyancy do the faster lift (see [`swim_vspeed`]). Was the old inline
/// `SWIM_UP_RATE`.
pub const SWIM_VRATE: f32 = 20.0;

/// Signed vertical swim wish (u/s) that makes the walker **HOLD the planned depth** instead of
/// floating to the surface — the crux of water-nav Slice 3 (design §8.2).
///
/// It replaces the old up-only rule (`swim && carrot > z+1 → +20 else 0`), which could only RISE: a
/// mid-water waypoint was inexpressible, so the instant the wish was 0 the controller's buoyancy —
/// which fires ONLY on `wish_vspeed == 0` (`movement`) — lifted the swimmer back to the swim
/// plane and the deep route waypoints could never be followed. That is the planner-z-vs-controller-z
/// fight of design §1, live-proven in qcat (#547: the char descended, then surfaced/wedged).
///
/// * `carrot_z` — the z the pursuit carrot wants NOW (from [`carrot_along`], now depth-interpolated).
/// * `player_z` — the character's feet z (all nav z's are feet-frame).
/// * `swim_plane` — `surface − float_depth` at the character's column: the depth buoyancy settles at.
///   `None` for a column with no bounded surface (open / unbounded deep water), where buoyancy also
///   cannot act.
///
/// Rule — **proportional toward the carrot's depth** (`err/τ`, clamped to ±`SWIM_VRATE`): a carrot
/// above the feet drives a collided rise, one below drives a collided sink, one at the feet drives ~0.
/// This is what makes a mid-water waypoint followable at all — the retired up-only rule returned 0 for
/// any waypoint at/below the feet, and 0 hands the swimmer to buoyancy. Note buoyancy alone reaches
/// only the swim plane, *not* the surface, so a proportional rise (capped at `SWIM_VRATE`, then
/// collided/surface-clamped by the controller's `swim_rise`) is what actually reaches an above-plane
/// entry/haul-out waypoint; buoyancy still assists any rise for free.
///
/// The one place the wish must be forced nonzero: **at the target while BELOW the swim plane.** There
/// `err ≈ 0` would give a 0 wish, and a 0 wish lets buoyancy (which fires only on `wish_vspeed == 0`,
/// `movement`, at 30 u/s) reclaim the swimmer and float it to the plane — the surfacing that
/// broke the deep route in #547. So below the plane a zero proportional term is nudged to a tiny
/// `MIN_HOLD` sink: nonzero enough to suppress buoyancy, tiny enough that the controller's `SKIN`
/// clamp on `swim_sink` turns it into zero net motion — a true hold. At/above the plane a 0 wish is
/// safe: buoyancy simply rests the swimmer AT the plane, which is where the route wants it anyway.
pub fn swim_vspeed(carrot_z: f32, player_z: f32, swim_plane: Option<f32>) -> f32 {
    const DEPTH_TAU: f32 = 0.25; // s — proportional time-constant of the depth hold
    // The tiny nonzero kept below the plane so buoyancy (wish==0) can never reclaim a mid-water hold.
    // |MIN_HOLD·dt| < movement::SKIN (0.05) at any real frame dt, so it suppresses buoyancy without
    // itself drifting the depth. `err` is essentially never exactly 0, but pin the invariant anyway.
    const MIN_HOLD: f32 = 0.1;
    let err = carrot_z - player_z; // + → carrot above the feet (rise); − → carrot below (sink)
    let w = (err / DEPTH_TAU).clamp(-SWIM_VRATE, SWIM_VRATE);
    if w == 0.0 && matches!(swim_plane, Some(plane) if player_z < plane - 0.001) {
        -MIN_HOLD // holding below the swim plane: keep buoyancy suppressed so we do not surface
    } else {
        w
    }
}

/// Fast-steering aim (#nav-multires / #311). Advances `local_i` — the cursor into `local_path` —
/// as far as the projection of `from` onto the active segment has passed its end (mirrors the
/// coarse `path_i` advance in `tick()`), then returns the unit `wish_dir` + EQ heading toward a
/// carrot `reach` units further along `local_path` from there. Pulled out of the fast-steering
/// block in `tick()` so the cursor mechanics are directly unit-testable without a live `EqStream`:
/// before this existed, that block called `carrot_along(&self.local_path, 0, ...)` with the
/// segment index PINNED at 0. `local_path` waypoints are only ~LOCAL_CELL(2u) apart and the plan is
/// only rebuilt on the 150ms gate, but this steering loop runs every ~10ms — so within ~45ms at
/// RUN_SPEED the projection onto segment 0 saturates at t=1, and for the rest of the gate the aim
/// is measured from `local_path[1]`, which is now BEHIND the walker. The look-ahead collapses and
/// can invert on a bend, which is the drawn-path-vs-actual-movement divergence in #311.
pub fn fast_steer_aim(
    path: &[[f32; 3]], local_i: &mut usize, from: [f32; 3], reach: f32,
    los: impl Fn([f32; 3], [f32; 3]) -> bool,
) -> Option<([f32; 2], f32)> {
    advance_cursor(path, local_i, from);
    // LOS-CLAMPED (#685): the fast-steer aim runs every ~10ms and is what the controller actually
    // heads at between plan gates, so the corner-cut guard MUST be here or the walker still chords
    // across the corner in the fast loop even after the tick's carrot is clamped.
    let aim = carrot_along_los(path, *local_i, from, reach, los)?;
    let (dx, dy) = (aim[0] - from[0], aim[1] - from[1]);
    let d = (dx * dx + dy * dy).sqrt();
    (d > 1e-3).then(|| ([dx / d, dy / d], eq_heading(dx, dy)))
}

/// Advance a pure-pursuit cursor into `path` while the projection of `from` onto the active segment
/// has passed its end. Monotone and idempotent: calling it twice from the same position is a no-op.
///
/// Both cursors need this and for the same reason (#311): a path is only rebuilt every so often, but
/// the walker keeps moving along it, so a cursor pinned to segment 0 saturates at t=1 and the carrot
/// starts being measured from a point BEHIND the walker — the look-ahead collapses and inverts on a
/// bend. Since #382 the fine path arrives from a worker a tick or two after it was requested and so
/// STARTS a few units behind the walker by construction, which makes this advance load-bearing on the
/// very first use of a fresh plan, not just partway through its life.
pub fn advance_cursor(path: &[[f32; 3]], i: &mut usize, from: [f32; 3]) {
    // A cursor can only ever index the path it was advanced along, whatever it held before. The fine
    // path is now REPLACED asynchronously, by a worker, with one that may be SHORTER than the one the
    // cursor was walking — so "the cursor outran the path" is a state this code must simply not have.
    // Clamping here makes it unrepresentable everywhere downstream, rather than leaving each caller to
    // remember a bounds check. (Found by the `the_walker_never_stalls_waiting_on_the_fine_plan`
    // property test, which fuzzes exactly this.)
    *i = (*i).min(path.len().saturating_sub(1));
    while *i + 2 < path.len() {
        let (a, b) = (path[*i], path[*i + 1]);
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
        // 3D projection (water-nav Slice 3, design §8.1): a purely VERTICAL water leg (l2_xy≈0) is no
        // longer mistaken for a zero-length segment and skipped on frame one — the cursor advances
        // past it only once the character has actually descended/ascended it. Near-horizontal land
        // segments have a vanishing z term, so 3D ≡ 2D and land steering is unchanged. A genuinely
        // zero-length segment (a==b) still has l2<1e-6 and is skipped, as before.
        let t = if l2 < 1e-6 { 1.0 }
            else { ((from[0] - a[0]) * ab[0] + (from[1] - a[1]) * ab[1] + (from[2] - a[2]) * ab[2]) / l2 };
        if t >= 1.0 { *i += 1; } else { break; }
    }
}

/// **THE NO-STALL INVARIANT, as a total function (#382).**
///
/// The fine 2 u tier is ADVISORY. It runs on a worker thread now, so on any given tick it may be:
/// never asked, still computing, dead, or back with nothing usable. In every one of those cases this
/// returns an aim and the walker drives. There is no input — none — for which the walker must wait.
///
/// That is the whole safety argument for moving the fine plan off the net thread, and it is
/// deliberately expressed as a **total pure function** rather than as an "is a plan in flight?" guard
/// somewhere in `tick`. A guard is a claim you can only test by racing it; totality is a property you
/// can prove. This distinction is not academic here: a `/follow` deadlock in this codebase once passed
/// LIVE verification **by luck** (the reply happened to land in a window where the leader had not
/// moved) and was caught only by a pure-function test. "The walker cannot stall" is a universal claim,
/// and no number of live runs discharges a universal.
///
/// `local` is whatever the fine tier last produced (empty = nothing to steer on). `fallback` is the
/// aim of last resort when even the coarse route yields nothing (the straight line to the goal).
pub fn steer_target(
    coarse: &[[f32; 3]], path_i: usize,
    local:  &[[f32; 3]], local_i: &mut usize,
    from: [f32; 3], look_ahead: f32,
    fallback: [f32; 3],
    los: impl Fn([f32; 3], [f32; 3]) -> bool,
) -> [f32; 3] {
    // Both carrots are LOS-CLAMPED (#685): whichever tier we steer along, the straight aim must not
    // chord across a convex corner. `los` is passed BY REFERENCE to both so one predicate serves both
    // tiers. With an always-clear `los` (no collision) this is byte-for-byte the old behaviour.
    // The coarse carrot: the aim we ALWAYS have while a route is committed.
    let coarse_aim = carrot_along_los(coarse, path_i, from, look_ahead, &los).unwrap_or(fallback);
    // The fine carrot, when the fine tier has given us a path worth steering along. A 1-waypoint
    // "path" is just the character's own position and steers nowhere.
    if local.len() >= 2 {
        // The fine plan was computed a tick or two ago, FROM a point the walker has since driven past
        // (#382) — so advance the cursor onto the segment it is actually on before measuring the
        // carrot, or the aim is taken from behind it (#311).
        advance_cursor(local, local_i, from);
        carrot_along_los(local, *local_i, from, look_ahead, &los).unwrap_or(coarse_aim)
    } else {
        coarse_aim
    }
}

/// Should the fine tier's outcome arm a proactive COARSE re-plan (#246)?
///
/// **Only a CLOSED window may.** `NoWayThrough` means the fine search explored its entire 40 u window
/// and proved there is no way along the committed coarse corridor from here — that is real evidence
/// the coarse route skims something the 8 u grid missed, and re-planning around it is the right move.
///
/// `Exhausted` means the search **did not look**. Arming on it is a limit laundered into "the route
/// ahead is blocked" — and that is exactly what the deleted 150 ms wall-clock budget did every time it
/// fired: under CPU load, a perfectly threadable corridor got re-planned as though it were walled,
/// which both wasted a coarse plan and (per #379) fed the coarse tier no information it could act on,
/// so it re-proposed the same corridor forever.
///
/// `Threaded` obviously does not: the walker is threading it right now.
pub fn arms_coarse_replan(outcome: &crate::collision::LocalOutcome) -> bool {
    matches!(outcome, crate::collision::LocalOutcome::NoWayThrough { .. })
}

#[cfg(test)]
mod cursor_resync_tests {
    use super::*;

    /// The real #673 fixture, verbatim from the coarse route the live walker committed on a FAILING
    /// South Qeynos → qcat run (captured off `/v1/observe/nav_debug`, so these are the client's own
    /// bytes, not a recomputation). Index 0 here is route waypoint 44.
    ///
    /// Shape: the route runs EAST along the street at y≈160 (idx 0..2), turns into the aqueduct
    /// trench mouth (idx 3), then doubles back WEST down the ramp at y≈144 (idx 4..8).
    const HAIRPIN: [[f32; 3]; 9] = [
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
    /// Where the character physically ENDS UP, measured in the offline replay of that live route:
    /// it drops off the street into the trench between idx 2 and idx 3 and lands on the ramp at
    /// idx 5 — three waypoints further along than the cursor thinks.
    const LANDED: [f32; 3] = [-534.285_6, 144.375, -5.991_005];
    /// The cursor the walker's monotone advance is left holding when that happens.
    const STALE_I: usize = 2;

    fn always_clear(_: [f32; 3], _: [f32; 3]) -> bool { true }

    /// **#673 — a cursor the character has physically overtaken must resync.**
    ///
    /// The projection of `LANDED` onto segment `STALE_I` is t≈0.61, so the walker's `t >= 1.0`
    /// advance rule can never move past it; the character is 17 u from that segment while standing
    /// exactly on segment 5.
    #[test]
    fn stale_cursor_after_a_fall_resyncs_to_the_segment_the_character_is_on() {
        // Premise: the monotone advance really is stuck here (this is why the bug exists).
        let (a, b) = (HAIRPIN[STALE_I], HAIRPIN[STALE_I + 1]);
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
        let t = ((LANDED[0] - a[0]) * ab[0] + (LANDED[1] - a[1]) * ab[1] + (LANDED[2] - a[2]) * ab[2]) / l2;
        assert!(t < 1.0, "fixture no longer pins the advance rule (t = {t})");

        let i = resync_cursor(&HAIRPIN, STALE_I, LANDED, always_clear);
        // The character is standing on the idx4→idx5 / idx5→idx6 joint; either segment names where
        // it actually is. What must NOT survive is the stale cursor three segments behind.
        assert!((4..=5).contains(&i),
            "cursor must resync to the segment the character is standing on, got {i}");
    }

    /// **#673 step 1 of 3 — the stale cursor collapses the FINE PLANNER'S GOAL onto the character.**
    ///
    /// **Read the reach literally: 24 u is NOT a steering target.** `drive_walk` steers with
    /// `LOOK_AHEAD = 5.0`; `LOCAL_REACH = 24.0` is only the *goal it hands the fine planner*. At 5 u
    /// off the coarse route this carrot does **not** collapse — 17.06 u ahead on this very fixture.
    /// So what collapses here is `local_goal`, and it reaches the steering aim by a different route:
    /// the two tests that follow carry that chain the rest of the way. What they measure is the
    /// STEERING LOOP having no escaping trajectory, **not** a wedge — whether it is a wedge is
    /// decided by the stall detector, backoff and re-plan, none of which any instrument on this
    /// branch contains.
    #[test]
    fn a_stale_cursor_collapses_the_fine_planners_goal_onto_the_character() {
        // Not a steering carrot: this is `drive_walk`'s `local_goal`, the point handed to
        // `find_path_local` as the fine plan's destination — so it reads the shared
        // [`LOCAL_REACH`] rather than restating 24.0. It was a private `const REACH: f32 = 24.0;`
        // until the #733 review found it: a copy of the reach in the very file that now defines
        // the shared one, and spelled differently enough that an identifier grep missed it.
        let stale = carrot_along(&HAIRPIN, STALE_I, LANDED, LOCAL_REACH).unwrap();
        let d_stale = (stale[0] - LANDED[0]).hypot(stale[1] - LANDED[1]);
        assert!(d_stale < 1.0,
            "fixture no longer reproduces the collapse (carrot was {d_stale:.2} u ahead)");

        let i = resync_cursor(&HAIRPIN, STALE_I, LANDED, always_clear);
        let fixed = carrot_along(&HAIRPIN, i, LANDED, LOCAL_REACH).unwrap();
        let d_fixed = (fixed[0] - LANDED[0]).hypot(fixed[1] - LANDED[1]);
        assert!(d_fixed > 8.0,
            "after resync the carrot must actually lead the character; it was {d_fixed:.2} u ahead");
        // …and it must lead DOWN the ramp (west), i.e. forward along the route.
        assert!(fixed[0] < LANDED[0], "carrot must lead forward along the route, got {fixed:?}");
    }

    // ───────── #727 round 3: the sim at the walker's REAL steering reach (review finding A) ─────────

    /// A flat floor under the whole #673 fixture. Deliberately FEATURELESS: no trench wall, no ramp,
    /// nothing that could trap the character. If the walker still fails to make headway on this, the
    /// failure is in the steering loop, not in terrain the harness invented to produce it.
    fn fixture_floor() -> crate::collision::Collision {
        let quad = |v: Vec<[f32; 3]>| eqoxide_assets::MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: eqoxide_assets::RenderMode::Opaque, anim: None,
        };
        // `Collision::build` maps mesh [x, y, z] -> world [east, north, height], so a vertex is
        // written [north, height, east].
        let floor = quad(vec![[100.0, -6.0, -600.0], [200.0, -6.0, -600.0],
                              [200.0, -6.0, -480.0], [100.0, -6.0, -480.0]]);
        crate::collision::Collision::build(
            &eqoxide_assets::ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }, 32.0)
    }

    /// What one `fixture_run` observed.
    ///
    /// The run has two phases and they must not be averaged together. Ticks 0–2 are a **transient**:
    /// the fine planner has not answered yet, so the walker steers the healthy ~17 u coarse carrot
    /// off the stale cursor and lunges several units back up the route. Only after the degenerate
    /// 2-point stub arrives does the limit cycle close. So the whole-run extent (`x_min` / `x_max`)
    /// and the settled extent (`late_x_min` / `late_x_max`) measure different things, and both are
    /// recorded rather than one standing in for the other.
    ///
    /// # Sampling rate — one rule, no exceptions
    ///
    /// **Every positional extent in this struct is sampled ONCE PER CONTROLLER FRAME** (~100 Hz),
    /// inside [`fixture_run`]'s `step`. Not per nav tick. The single exception is `head`, which is
    /// defined as a tick-boundary quantity and says so on its own line.
    ///
    /// **Why the one rule is enforced by construction rather than by care (#727 round 5, B-1).** The
    /// two bands were once sampled at different rates and printed in one table as "the same
    /// measurement at two times". The settled cycle flips the aim **every frame** — the root-cause
    /// mechanism itself — so a once-per-15-frames sampler is structurally incapable of seeing its
    /// width: it understated the settled span by **50%** (0.880 u tick-sampled vs 1.320 u
    /// frame-sampled over the identical `tick >= 100` window). Hand-matching the rates is how they
    /// drifted apart in the first place; there is now exactly ONE place a position is recorded.
    struct Run {
        arrived: bool,
        ticks: u32,
        /// Straight-line distance from the start position to wherever the run ended.
        net: f32,
        /// Extent of the east coordinate over the WHOLE run, transient included. Per frame.
        x_min: f32,
        x_max: f32,
        /// Extent of the east coordinate over ticks ≥ 100 — long past any transient, so this is the
        /// settled limit cycle and nothing else. Per frame.
        late_x_min: f32,
        late_x_max: f32,
        /// The furthest the body ever got from where it landed. Per frame.
        max_from_landed: f32,
        /// Position at the end of each of the first three nav ticks. **The one tick-boundary
        /// quantity here**, and deliberately so: it exists to be compared against the production
        /// `drive_walk` loop's log, which is written once per nav tick. Sampling it per frame would
        /// compare two different things.
        head: Vec<[f32; 2]>,
    }

    /// Everything [`fixture_run`]'s `step` records, in one place so there is one sampling rate.
    ///
    /// `late` is set once at the top of each nav tick and read by `step`, so the `tick >= 100`
    /// window is a property of the tick while the *sampling* inside it stays per frame.
    struct Bands {
        x_min: f32,
        x_max: f32,
        late_x_min: f32,
        late_x_max: f32,
        max_from_landed: f32,
        late: bool,
    }

    /// Drive the #673 fixture through the walker's REAL steering rule and report how far the
    /// character actually gets along the route.
    ///
    /// Structure mirrors `navigation.rs`'s two-rate loop: a 150 ms NAV TICK (cursor + `steer_target`
    /// + a fine-plan request) and 14 fast-steer frames at ~100 Hz in between, each calling the
    /// production [`fast_steer_aim`] exactly as `Walker::apply_fast_steering` does. The steering
    /// itself is **not mirrored** — it is the production functions, called with the production
    /// arguments and the production `STEER_LOS_CLEARANCE`. The only copied numbers are the loop
    /// rates, `LOOK_AHEAD` / `LOCAL_REACH` / `LOCAL_BOUND` and `RUN_SPEED`.
    ///
    /// **`wish_dir` PERSISTS, which is the one thing a per-frame harness gets wrong by default.**
    /// `Walker::apply_fast_steering` runs only when `!self.local_path.is_empty()`, and all it does
    /// is *overwrite* `MoveIntent.wish_dir`. It never clears the intent and never stops the body.
    /// So a tick with no fine plan is not a tick where the character stands still: the controller
    /// integrates the tick's own `steer_target` direction for all 15 frames. This harness holds
    /// `wish` across frames for that reason (round-4 review, B-C — the earlier version `break`ed out
    /// of the fast loop instead, moving the body 1 frame in 15 on such a tick).
    ///
    /// # What this fixture does NOT model
    ///
    /// Stated bluntly and in one place, because scattered caveats are how a sim's result gets read
    /// as the walker's behaviour (#727 round-3 review, blocking 1). This is a model of the
    /// **steering loop**, not of the walker:
    ///
    /// 1. **No stall detector, no `NAV_STUCK_TICKS` backoff, and no re-plan.** That is precisely the
    ///    machinery that decides whether a limit cycle is a *wedge* or a three-second hiccup. A
    ///    `net = 0` result here means "the steering loop has no trajectory that leaves the spot",
    ///    **not** "the walker never leaves the spot" — measured, the walker escapes this featureless
    ///    fixture via its own stall/backoff/re-plan. See the doc on
    ///    `the_stale_cursor_leaves_the_steering_loop_no_escaping_trajectory_and_the_resync_clears_it`.
    /// 2. **No controller.** Position is `wish_dir * RUN_SPEED * dt` with a floor snap: no gravity,
    ///    no collision response, no acceleration, no slope handling. On this floor there is nothing
    ///    for those to do, which is the point of the floor being featureless — but it is a model.
    /// 3. **The fine plan is synchronous with one tick of latency**, where production posts it with
    ///    `post_if_idle` and applies whatever the worker has finished. The one-tick delay reproduces
    ///    production's "tick 0 has no fine plan and steers the healthy coarse carrot"; a *variable*
    ///    planner latency is not modelled, and a synchronous zero-latency plan (what this harness did
    ///    through round 3) makes the fixed point artificially perfect.
    /// 4. **One flat slab of floor**, not qcat. Nothing here says how often the live client enters
    ///    this state, and the featureless floor is why the re-plan in (1) can rescue it.
    ///
    /// **On not building the answer into the instrument.** The floor is featureless, so nothing here
    /// can trap the character except the steering rule itself; the harness is never told what a
    /// stall is, it only integrates position and reports it; and
    /// `a_walker_whose_cursor_is_honest_walks_this_same_fixture_out` runs it with a correct cursor
    /// and no fix, so a harness that reported a stall unconditionally would fail its own control.
    ///
    /// `resync` selects the cursor rule: `false` = the monotone advance alone (pre-#727), `true` =
    /// the advance plus [`resync_cursor`] with the walker's own predicate — [`resync_reachable`],
    /// the same function `Walker::advance_cursor` passes, not a restatement of it. It used to be a
    /// restatement, and #887 round 1 caught the restatement still claiming to be "the walker's own
    /// predicate" after production's had been changed out from under it.
    fn fixture_run(col: &crate::collision::Collision, start_i: usize, resync: bool, verbose: bool)
        -> Run
    {
        const DT: f32 = 0.01;          // ~100 Hz controller frame
        const FRAMES: u32 = 14;        // 150 ms nav tick = 1 steer_target + 14 fast_steer_aim frames
        const TICKS: u32 = 200;
        const LOOK_AHEAD: f32 = 5.0;   // walker.rs `drive_walk`
        // `drive_walk`'s `local_goal` reach is NOT restated here: [`LOCAL_REACH`] is in scope from
        // `use super::*`, so this harness cannot drift onto a different carrot than production and
        // the collapse check. (A `const LOCAL_REACH: f32 = super::LOCAL_REACH;` alias stood here
        // briefly; it could not drift either, but it reads exactly like the copies the #733 review
        // was hunting, so it is gone.)
        const LOCAL_BOUND: f32 = 40.0; // walker.rs `drive_walk`
        // The walker's own clearance, referenced rather than re-derived: it is defined as
        // `PLAYER_RADIUS` today, so a copy would agree by coincidence and drift silently.
        let clearance = crate::walker::STEER_LOS_CLEARANCE;
        let los = |a: [f32; 3], b: [f32; 3]| col.carrot_los_clear(a, b, clearance);
        let goal = *HAIRPIN.last().unwrap();
        let mut p = LANDED;
        let mut path_i = start_i;
        // Production applies the fine plan a tick after it is requested (`post_if_idle` + `poll`), so
        // tick 0 has none — which is why tick 0 steers the healthy coarse carrot.
        let mut local: Vec<[f32; 3]> = Vec::new();
        let mut pending: Vec<[f32; 3]> = Vec::new();
        let mut local_i = 0usize;
        let mut local_from = p;
        // The controller's live `MoveIntent.wish_dir`. It persists across frames exactly as
        // production's does — see the fast loop below.
        let mut wish = [0.0f32; 2];
        let mut b = Bands {
            x_min: p[0], x_max: p[0],
            late_x_min: f32::MAX, late_x_max: f32::MIN,
            max_from_landed: 0.0,
            late: false,
        };
        let mut head: Vec<[f32; 2]> = Vec::new();
        for tick in 0..TICKS {
            let before = p;
            // The settled-cycle window is chosen per TICK; the sampling inside it is per FRAME.
            b.late = tick >= 100;
            while path_i + 2 < HAIRPIN.len() {
                let (a, b) = (HAIRPIN[path_i], HAIRPIN[path_i + 1]);
                let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
                let t = if l2 < 1e-6 { 1.0 } else {
                    ((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1] + (p[2] - a[2]) * ab[2]) / l2
                };
                if t >= 1.0 { path_i += 1; } else { break; }
            }
            if resync {
                path_i = resync_cursor(&HAIRPIN, path_i, p, |a, b| resync_reachable(col, a, b));
            }
            let coarse = carrot_along(&HAIRPIN, path_i, p, LOOK_AHEAD).unwrap_or(goal);
            // `drive_walk`: apply whatever the fine planner finished, then drop it if the body has
            // wandered outside the window it was planned from.
            if !pending.is_empty() { local = std::mem::take(&mut pending); local_i = 0; }
            if !local.is_empty() && (p[0] - local_from[0]).hypot(p[1] - local_from[1]) > LOCAL_BOUND {
                local.clear();
                local_i = 0;
            }
            // The fine plan, exactly as `drive_walk` requests it: goal = the LOCAL_REACH carrot off
            // the CURRENT cursor. This is where a stale cursor is consumed.
            let local_goal = carrot_along(&HAIRPIN, path_i, p, LOCAL_REACH).unwrap_or(coarse);
            local_from = p;
            pending = match col.find_path_local(p, local_goal, LOCAL_CELL, LOCAL_BOUND, LOCAL_CELL * 2.0) {
                crate::collision::LocalOutcome::Threaded(s) => s,
                crate::collision::LocalOutcome::NoWayThrough { steer, .. } => steer,
                crate::collision::LocalOutcome::Exhausted { steer, .. } => steer,
            };
            // One frame of the controller integrating a MoveIntent: a UNIT `wish_dir` driven at
            // RUN_SPEED for the whole frame. The controller does NOT slow down for a near carrot,
            // which is why an aim inside one frame's travel (44 * 0.01 = 0.44 u) is overshot rather
            // than reached.
            //
            // **This is the ONLY place a position is recorded** (round-5 review, B-1). Every extent
            // on `Run` is therefore sampled at the same rate — this one, per frame — and a new
            // extent cannot quietly be added at the tick boundary instead.
            let step = |p: &mut [f32; 3], wish: [f32; 2], b: &mut Bands| {
                p[0] += wish[0] * eqoxide_core::physics::RUN_SPEED * DT;
                p[1] += wish[1] * eqoxide_core::physics::RUN_SPEED * DT;
                if let Some(fz) = col.ground_below(p[0], p[1], p[2] + 4.0, 40.0) { p[2] = fz; }
                b.x_min = b.x_min.min(p[0]);
                b.x_max = b.x_max.max(p[0]);
                if b.late {
                    b.late_x_min = b.late_x_min.min(p[0]);
                    b.late_x_max = b.late_x_max.max(p[0]);
                }
                b.max_from_landed =
                    b.max_from_landed.max((p[0] - LANDED[0]).hypot(p[1] - LANDED[1]));
            };
            // ONE `steer_target` per nav tick (the 150 ms coarse tick, with the LOS clamp). Its aim
            // is what `drive_walk` turns into `MoveIntent.wish_dir` — and `wish_dir` then PERSISTS:
            // the controller keeps integrating that same vector every frame until something writes
            // a new one.
            let aim = steer_target(&HAIRPIN, path_i, &local, &mut local_i, p, LOOK_AHEAD, coarse, &los);
            let (adx, ady) = (aim[0] - p[0], aim[1] - p[1]);
            let ad = (adx * adx + ady * ady).sqrt();
            // `drive_walk` publishes no intent for a degenerate aim; the previous tick's intent is
            // what the controller would still be holding, so carry `wish` over unchanged.
            if ad > 1e-3 { wish = [adx / ad, ady / ad]; }
            step(&mut p, wish, &mut b);
            // …then the ~10 ms fast loop. `Walker::apply_fast_steering` OVERRIDES `wish_dir` — and
            // only when `!self.local_path.is_empty()`. It never clears it and never stops the body.
            // So on a tick with NO fine plan the controller integrates the tick's own MoveIntent for
            // all 15 frames.
            //
            // DO NOT `break` here on an empty fine plan (#727 round 4, B-C). Doing so advances the
            // body 1 frame of the 15 — not a physics simplification but the harness declining to
            // move the body — and it manufactures a lateral bound the character does not have: the
            // whole run's eastern extreme was once set on tick 0 purely because 14 frames were
            // dropped, while production `drive_walk` reached `x = -528.39` at t1, 5.9 u east, on
            // this same fixture.
            for _ in 0..FRAMES {
                if !local.is_empty() {
                    if let Some((w, _)) = fast_steer_aim(&local, &mut local_i, p, LOOK_AHEAD, |_, _| true) {
                        wish = w;
                    }
                }
                step(&mut p, wish, &mut b);
            }
            if verbose {
                println!("  t{tick:<3} cursor {path_i} local.len {:<3} moved {:.3} u  pos ({:.3},{:.3})",
                    local.len(), (p[0] - before[0]).hypot(p[1] - before[1]), p[0], p[1]);
            }
            let net = (p[0] - LANDED[0]).hypot(p[1] - LANDED[1]);
            // `head` is the ONE tick-boundary quantity: it is compared against a production log that
            // is itself written once per nav tick. Everything else was recorded inside `step`.
            if head.len() < 3 { head.push([p[0], p[1]]); }
            if (p[0] - goal[0]).hypot(p[1] - goal[1]) <= 3.0 {
                return Run { arrived: true, ticks: tick + 1, net,
                    x_min: b.x_min, x_max: b.x_max,
                    late_x_min: b.late_x_min, late_x_max: b.late_x_max,
                    max_from_landed: b.max_from_landed, head };
            }
        }
        Run { arrived: false, ticks: TICKS, net: (p[0] - LANDED[0]).hypot(p[1] - LANDED[1]),
            x_min: b.x_min, x_max: b.x_max,
            late_x_min: b.late_x_min, late_x_max: b.late_x_max,
            max_from_landed: b.max_from_landed, head }
    }


    /// **#673 step 2 of 3 — how the collapse reaches the steering aim.** The round-2 review's
    /// finding A was half right and this test records both halves, because getting this wrong in
    /// either direction is what cost two review rounds.
    ///
    /// * **Right:** the walker does not steer with a 24 u carrot, and at its real `LOOK_AHEAD = 5.0`
    ///   the *coarse* carrot off the stale cursor is not collapsed at all — it leads by ~17 u.
    /// * **Wrong:** that carrot is not what the walker steers with either. [`steer_target`] prefers
    ///   the fine path whenever it has two or more points, and the fine path is planned to the
    ///   collapsed `local_goal`, so it is a degenerate 2-point stub 0.2 u long. The 5 u carrot
    ///   measured *along that stub* is the aim, and it is collapsed.
    ///
    /// So the stale cursor reaches the steering aim through the fine tier, not the coarse one.
    #[test]
    fn the_stale_cursor_reaches_the_steering_aim_through_the_fine_plan_not_the_coarse_carrot() {
        let col = fixture_floor();
        let radius = eqoxide_core::physics::PLAYER_RADIUS;
        let los = |a: [f32; 3], b: [f32; 3]| col.carrot_los_clear(a, b, radius);
        let d = |q: [f32; 3]| (q[0] - LANDED[0]).hypot(q[1] - LANDED[1]);

        let coarse = carrot_along_los(&HAIRPIN, STALE_I, LANDED, 5.0, &los).unwrap();
        assert!(d(coarse) > 10.0,
            "the COARSE carrot at LOOK_AHEAD is not the collapsed one — if this ever fails, the \
             round-3 account of #673 is wrong and the round-2 one may be right (was {:.2} u)", d(coarse));

        // `LOCAL_REACH`, not a literal 24.0: this IS `drive_walk`'s `local_goal`, and a bare literal
        // here was a fourth copy of the reach that the #733 review's identifier grep could not see.
        let local_goal = carrot_along(&HAIRPIN, STALE_I, LANDED, LOCAL_REACH).unwrap();
        assert!(d(local_goal) < 1.0, "local_goal must be the collapsed one (was {:.2} u)", d(local_goal));

        let steer = match col.find_path_local(LANDED, local_goal, LOCAL_CELL, 40.0, LOCAL_CELL * 2.0) {
            crate::collision::LocalOutcome::Threaded(s) => s,
            crate::collision::LocalOutcome::NoWayThrough { steer, .. } => steer,
            crate::collision::LocalOutcome::Exhausted { steer, .. } => steer,
        };
        // Two points is the threshold at which `steer_target` starts preferring the fine tier. A
        // degenerate stub is therefore not ignored as too short — it is preferred.
        assert_eq!(steer.len(), 2, "expected a degenerate fine plan, got {steer:?}");

        let mut local_i = 0usize;
        let aim = steer_target(&HAIRPIN, STALE_I, &steer, &mut local_i, LANDED, 5.0, coarse, &los);
        assert!(d(aim) < 1.0,
            "the steering aim must be collapsed even though the coarse carrot is not (was {:.2} u)", d(aim));
        // One frame's travel is RUN_SPEED * 0.01 = 0.44 u, so an aim this near is overshot, not
        // reached. That is the limit cycle.
        assert!(d(aim) < eqoxide_core::physics::RUN_SPEED * 0.01,
            "aim {:.2} u is further than one frame of travel; the overshoot argument would not hold", d(aim));
    }

    /// **#673 step 3 of 3 — the STEERING LOOP has no escaping trajectory, and the resync clears it.**
    ///
    /// The whole chain, driven at `LOOK_AHEAD` through the production [`steer_target`] and
    /// [`fast_steer_aim`], on a floor with nothing in it to blame. With the stale cursor the loop
    /// enters a limit cycle and stays in it for the whole run: **0.04 u net displacement over 200 nav
    /// ticks** (30 s of simulated time), never getting further than **6.6 u** from where it landed —
    /// less than one 8 u leg of the route it is standing on. With the resync it walks the fixture out
    /// in **4** nav ticks.
    ///
    /// **THE LIMIT: this instrument does not contain the walker**, so it cannot say the walker
    /// WEDGES. [`fixture_run`] has no stall detector, no `NAV_STUCK_TICKS` backoff and no re-plan —
    /// the exact machinery that decides whether a limit cycle is a wedge or a hiccup. Driven through
    /// the **production** `drive_walk` + `apply_fast_steering` loop on this same fixture with the
    /// resync mutated out, the walker sits in the cycle ~22 nav ticks (~3.3 s), escapes via its own
    /// backoff + re-plan and **arrives** at t27. On this featureless floor the pre-#727 cost is a
    /// wasted re-plan lap, not a permanent stop.
    ///
    /// **What the defect is, then.** The steering loop having no escaping trajectory is the
    /// mechanism; whether that is terminal is decided outside this sim, by whether the re-plan
    /// reproduces the state. Live on qcat it did: #673 records `blocked` / `walker_stalled` at
    /// `[-534.4, 144.4, -6.0]` on **6 of 8** attempts. The terminal state itself is what carries that
    /// — the walker stopped, on a route it was standing on. Do **not** additionally read
    /// `nav_repaths == 8` at the emission site as eight failed attempts AT THAT SPOT: `drive_walk`
    /// resets the counter whenever `gdist < nav_best_gdist - REPATH_RESET_DIST` (200 u) and on
    /// `decision.reset_route`, so it establishes only *at least eight stall-triggered re-plans since
    /// the walker last closed 200 u on the goal*. The conclusion rests on the `blocked` outcome, not
    /// on the count.
    ///
    /// The residual #673 defect therefore ranges from ~22 wasted ticks plus a re-plan lap (measured,
    /// featureless floor) to a terminal stop (observed, real terrain). *Reasoned, not measured:* the
    /// flat slab is probably why the re-plan rescues it here — a re-plan starts at the body, and this
    /// floor offers no second fall to carry the character off the new route, where the aqueduct
    /// trench does.
    #[test]
    fn the_stale_cursor_leaves_the_steering_loop_no_escaping_trajectory_and_the_resync_clears_it() {
        let col = fixture_floor();

        let stale = fixture_run(&col, STALE_I, false, false);
        assert!(!stale.arrived, "pre-#727 the fixture must reproduce the stall");
        assert!(stale.net < 0.5,
            "the stall is ZERO net displacement, not slow progress; moved {:.2} u in {} ticks",
            stale.net, stale.ticks);
        // Zero NET is not the same as never moving — the transient below is real motion. Bound the
        // excursion against the route's own scale: one 8 u leg. In 200 nav ticks (30 s) a walker
        // that were merely slow covers 1320 u; this one never advances a single leg.
        assert!(stale.max_from_landed < 8.0,
            "the body wandered {:.2} u from where it landed — that is more than one route leg, so \
             this is not the no-escaping-trajectory state the test is named for", stale.max_from_landed);

        let fixed = fixture_run(&col, STALE_I, true, false);
        assert!(fixed.arrived, "the resync must get the character to the goal; it moved {:.2} u", fixed.net);
        assert!(fixed.ticks <= 20, "arrival took {} nav ticks, expected a handful", fixed.ticks);
    }


    /// The control for the test above: same harness, same floor, same route, cursor NOT stale and the
    /// fix NOT applied. It arrives. A harness that reports a stall whatever you feed it proves
    /// nothing, so
    /// this is the assertion that makes the one above mean something.
    #[test]
    fn a_walker_whose_cursor_is_honest_walks_this_same_fixture_out() {
        let col = fixture_floor();
        let run = fixture_run(&col, 4, false, false);
        assert!(run.arrived,
            "control failed: the harness cannot complete the route even with an honest cursor, so it \
             cannot attribute a stall to a stale one (moved {:.2} u in {} ticks)", run.net, run.ticks);
    }

    /// **The simulated stall is a BOUNDED overshoot cycle a few frames wide** — it does not drift,
    /// and it does not creep along the route. That is the property worth pinning: an aim nearer than
    /// one frame of travel (`RUN_SPEED * 0.01 = 0.44 u`) is overshot, the direction flips, and the
    /// body orbits the stub instead of following the route.
    ///
    /// The run is **two-phase**, which is why no single number describes it:
    ///
    /// ```text
    /// whole run    x ∈ [-536.524, -528.391]   span 8.13 u   max 6.60 u from LANDED
    /// ticks ≥ 100  x ∈ [-535.204, -533.884]   span 1.32 u = 3 frames of travel
    ///                                         0.4011 u east of LANDED, 0.9189 u west
    /// ```
    ///
    /// Ticks 0–2 are a transient: no fine plan has arrived, so the walker steers the *healthy* ~17 u
    /// coarse carrot and lunges back up the route — real motion. From tick 3 on the degenerate stub
    /// is in hand and the cycle closes. **The settled cycle is what "a few frames wide" is about**,
    /// and that is what the assertions below measure. The test prints all three figures
    /// (`cargo test … -- --nocapture`) so the next person to quote them reads them off a run rather
    /// than doing arithmetic on this table's rounded endpoints.
    ///
    /// ## What this test does NOT establish — three measured traps (#727 rounds 3–7)
    ///
    /// 1. **The live capture cannot corroborate this sim.** An earlier version asserted
    ///    `|x_min − (−534.73)| < 0.05` against the live capture and called the agreement evidence.
    ///    It is an **identity**: `LANDED[0] = -534.285_583` is a harness INPUT and
    ///    `RUN_SPEED * 0.01 = 0.44` is a code constant, so any harness seeded at `LANDED` that aims
    ///    west and steps one frame produces `-534.725_586`, whatever the mechanism. The capture's
    ///    −534.73 is *consistent* with an overshoot cycle; it is not independent evidence of one.
    /// 2. **The sim's WIDTH is a property of the harness, not of the defect.** Production
    ///    `drive_walk` oscillates over roughly `[-536.5, -533.0]` (~2.6 u) against this sim's
    ///    1.32 u. Do not quote the sim's band as production's.
    /// 3. **Sample per FRAME, not per nav tick.** The settled cycle flips the aim every frame — the
    ///    root-cause mechanism itself — so a once-per-15-frames sampler cannot see its width. Over
    ///    the identical `tick >= 100` window: **0.880 u tick-sampled, 1.320 u frame-sampled**, a 50%
    ///    understatement that read as a real result for two rounds. Every extent on `Run` is now
    ///    recorded in one place, inside `step`; see `Run`'s own "one sampling rate" note.
    ///
    /// **A separately measured non-result:** whether [`fixture_run`] carries `wish_dir` across
    /// frames (it now does; it used to `break` out of the fast loop with no fine plan) leaves the
    /// settled width **identical to three decimal places** — 1.320 either way, band shifted 0.019 u.
    /// What the frame carry does change is the whole-run band (1.320 → 8.134 u, by no longer
    /// suppressing the transient), `net` (0.0197 → 0.0389 u) and arrival (5 → 4 ticks). So a
    /// "1.32 u" quoted from before the fix is a correct SETTLED width mislabelled as a whole-run
    /// band, not a dropped-frame artifact.
    ///
    /// **On the production agreement below.** The harness reproduces the production `drive_walk`
    /// log tick for tick — but tick 0's *magnitude* is another identity
    /// (`|head[0] − LANDED| = 6.5997` against `15 × RUN_SPEED × 0.01 = 6.6000`), so position 0
    /// contributes only its direction. The agreements that discriminate are `head[1]` (0.091 u from
    /// `LANDED`) and `head[2]` (2.239 u), which depend on the stub geometry and the aim flip. Honest
    /// count: **two independent agreements plus one direction**, not three. And both instruments run
    /// the *same* production `steer_target` / `carrot_along` / `find_path_local` on the *same*
    /// fixture, so even that is evidence about the HARNESS'S FIDELITY, not independent evidence
    /// about the live defect. The three positions are quoted from the round-3 review and were not
    /// re-measured here.
    #[test]
    fn the_simulated_stall_is_a_bounded_overshoot_cycle_a_few_frames_wide() {
        const FRAME: f32 = eqoxide_core::physics::RUN_SPEED * 0.01;
        let run = fixture_run(&fixture_floor(), STALE_I, false, false);

        // The SETTLED cycle (ticks >= 100), which is the thing "a few frames wide" describes.
        let span = run.late_x_max - run.late_x_min;
        // Round 7 (non-blocking 4): the doc above quotes these three. Print them, so the quoted
        // figures come from THIS run and not from arithmetic on the band's printed 3 decimals.
        //
        // #742: the WHOLE-run extent is printed alongside. `Run`'s doc says the whole-run and
        // settled extents "measure different things, and both are recorded rather than one standing
        // in for the other" — but nothing read `x_min`/`x_max`, which is what the `dead_code` warning
        // on those two fields was reporting: they were computed and then dropped, so the transient
        // `Run`'s doc exists to keep separate was invisible in the output. Printing them is
        // deliberately NOT an assertion: the transient's width is a diagnostic, and the only
        // claim this test makes is about the settled band.
        eprintln!(
            "settled span {span:.4}   east of LANDED {:.4}   west of LANDED {:.4}   \
             whole-run extent [{:.4}, {:.4}] (transient included)",
            run.late_x_max - LANDED[0], LANDED[0] - run.late_x_min,
            run.x_min, run.x_max);
        assert!(span < 5.0 * FRAME,
            "the settled cycle must stay within a few frames of travel — a wider band would be \
             drift, not a limit cycle; it was {span:.3} u");
        // …and it must sit ON the landing spot, neither creeping west down the route nor east back
        // up it. This is the character claim the withdrawn assertion was reaching for, made about
        // the phase where the harness is actually integrating every frame.
        assert!(run.late_x_max - LANDED[0] < 5.0 * FRAME && LANDED[0] - run.late_x_min < 5.0 * FRAME,
            "the settled cycle drifted off the landing spot to [{:.3}, {:.3}] against a landing of \
             {:.3} — a cycle that makes ground is slow progress, not a stall",
            run.late_x_min, run.late_x_max, LANDED[0]);

        // Fidelity: the transient must keep matching the production loop, or the frame-carry fix
        // that earns the assertions above has been undone. (Round-3 review's log, quoted.)
        const PRODUCTION_HEAD: [[f32; 2]; 3] =
            [[-528.39, 147.34], [-534.33, 144.46], [-536.52, 144.37]];
        for (i, (got, want)) in run.head.iter().zip(PRODUCTION_HEAD.iter()).enumerate() {
            assert!((got[0] - want[0]).abs() < 0.02 && (got[1] - want[1]).abs() < 0.02,
                "tick {i} ended at ({:.3}, {:.3}); the production drive_walk loop ended it at \
                 ({:.2}, {:.2}). The harness has stopped modelling the controller's persistent \
                 wish_dir, so any bound it reports is about the harness, not the character.",
                got[0], got[1], want[0], want[1]);
        }
    }

    /// **The normal case is untouched, and pays nothing.** A walker within `CURSOR_STALE_DIST` of
    /// its own segment keeps its cursor and the `clear` predicate is never even consulted.
    #[test]
    fn an_on_route_walker_is_left_alone_without_consulting_geometry() {
        let called = std::cell::Cell::new(false);
        for (i, on_seg) in [(0usize, [-538.0f32, 160.375, 0.0]), (4, [-530.0, 144.375, -5.0])] {
            let got = resync_cursor(&HAIRPIN, i, on_seg, |_, _| { called.set(true); true });
            assert_eq!(got, i, "an on-route cursor must not move");
        }
        assert!(!called.get(), "the LOS predicate must not be called for an on-route walker");
    }

    /// **The staleness guard EXISTS — this does not pin its value.** On a tight switchback a walker
    /// that is genuinely mid-leg can be geometrically CLOSER to a later segment than to its own —
    /// here 1.5 u from segment 0 but 0.5 u from segment 2, an out-and-back only 2 u wide. A
    /// nearest-segment snap would skip the entire outbound leg and quietly cut the route; some
    /// staleness guard means a walker still on its segment is never touched.
    ///
    /// Measured: `CURSOR_STALE_DIST` mutated to both **2.0** and **16.0** leaves this test green
    /// (1.5 u is inside either guard). What it kills is REMOVING the guard entirely. The constant is
    /// unpinned over at least [2, 16] and is a judgement call, not a measurement — see the
    /// constant's own doc for the one thing about it that *is* measured (the `<` boundary).
    #[test]
    fn a_walker_cutting_a_tight_switchback_keeps_its_cursor() {
        let switchback = [[0.0f32, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 2.0, 0.0], [0.0, 2.0, 0.0]];
        assert_eq!(resync_cursor(&switchback, 0, [5.0, 1.5, 0.0], always_clear), 0,
            "a walker mid-leg must not be snapped onto a nearer later segment");
    }

    /// **Every test name this crate's rustdoc cites must still exist (#727 round 5).**
    ///
    /// Round-4's blocking findings were both dangling citations: `walker_cursor_resync` (a module
    /// that never existed) and `the_resync_clears_the_deadlock_above_the_guard_and_is_inert_below_it`
    /// (renamed in round 3 *because* its name asserted a retracted claim). Nothing caught either,
    /// because both were plain backticks — and they cannot be intra-doc links, since a `#[cfg(test)]`
    /// item is invisible to the rustdoc pass that would check the link.
    ///
    /// So this stands in for the link: naming each cited item as a value makes a rename a **compile
    /// error** in the same commit, not a silent rot found four review rounds later.
    ///
    /// **You do not have to remember to add a line here — for every citation shape that scan can
    /// see.** The list below is the *enforcement*; its *completeness* is checked mechanically by
    /// `every_test_citation_in_the_five_citation_files_resolves_and_is_listed_in_a_guard`,
    /// which reads the source and fails if a doc comment cites a test this array does not name.
    ///
    /// Read the qualifier literally: the completeness claim is about a **stated rule**, not about
    /// "any citation". Written once without it, it was false for two live shapes at once —
    /// `::`-qualified paths failed the charset filter, and a hand-wrapped citation never appears
    /// whole on any line, both hit by
    /// `zone_assets::no_interleaving_of_the_two_writers_yields_a_usable_wrong_zone` in `walker.rs`,
    /// inside the scan's own corpus. Both are covered now; the rule's remaining blind spots are
    /// written down in "What it does NOT do" below, because a guard that claims coverage it does
    /// not have is worse than no guard.
    #[test]
    fn every_test_name_cited_in_a_doc_comment_still_exists() {
        let _cited: &[fn()] = &[
            // cited by `CURSOR_STALE_DIST`
            the_deadlock_fixed_point_exactly_on_the_guard_boundary_is_resynced,
            the_resync_clears_the_carrot_pinning_at_every_leg_separation_measured,
            // cited by `resync_cursor` and by `Walker::advance_cursor`
            the_stale_cursor_reaches_the_steering_aim_through_the_fine_plan_not_the_coarse_carrot,
            the_stale_cursor_leaves_the_steering_loop_no_escaping_trajectory_and_the_resync_clears_it,
            a_stale_cursor_collapses_the_fine_planners_goal_onto_the_character,
            // cited by `fixture_run`'s "not building the answer into the instrument" note
            a_walker_whose_cursor_is_honest_walks_this_same_fixture_out,
            // added in round 6 by the mechanical scan below — all three were cited in doc comments
            // in this file and named in no guard list, which is the defect the scan exists to find.
            resync_never_jumps_across_blocked_geometry,
            every_test_name_cited_in_a_doc_comment_still_exists,
            // #788 split the doc-span half out of the citation test and renamed the citation test
            // so neither name reads as workspace-wide. All three are cited in doc comments in this
            // file, so all three are pinned here.
            every_test_citation_in_the_five_citation_files_resolves_and_is_listed_in_a_guard,
            unbalanced_doc_spans_are_rejected_in_the_five_citation_files_only_not_the_workspace,
            the_doc_span_scan_reaches_all_five_citation_files_at_three_depths_each,
            // #789/#874: two tests caught by this file's own scan on the run that added them.
            unbalanced_doc_code_spans_in_the_whole_workspace_are_a_named_shrinking_backlog,
            the_doc_span_scan_reaches_the_full_resolution_corpus_at_three_depths_each,
            // …and this one the scan caught on its first run, on a citation added in round 6
            // itself — which is the whole argument for having it.
            the_simulated_stall_is_a_bounded_overshoot_cycle_a_few_frames_wide,
            // The identity-mutant survivor list in `the_resync_moves_the_cursor_...`'s doc. All four
            // were invisible to any grep until round 6: three were elided to `..._` and two were
            // hand-wrapped across a line break INSIDE their backticks. Un-eliding them is what
            // exposed them to the scan.
            resync_never_moves_the_cursor_backwards,
            resync_is_inert_on_degenerate_paths,
            an_on_route_walker_is_left_alone_without_consulting_geometry,
            a_walker_cutting_a_tight_switchback_keeps_its_cursor,
            // #733: cited by `carrot_leads` and by `resync_cursor`'s two-trigger section.
            the_three_arclengths_are_the_points_they_claim_to_be,
            carrot_leads_judges_the_carrot_the_production_code_actually_builds,
            after_a_resync_with_clear_geometry_the_carrot_always_leads,
            the_sub_guard_hairpin_fixed_point_resyncs_though_the_distance_trigger_cannot_see_it,
            carrot_leads_is_honest_at_the_route_end_and_where_it_can_measure_nothing,
        ];
        // Helpers cited by name in the same docs.
        let _helpers: (fn(&crate::collision::Collision, usize, bool, bool) -> Run,
                       fn(f32, [f32; 3], usize) -> HairpinRun) = (fixture_run, hairpin_carrot_stops_leading);
    }

    /// **The citation guard's ALPHABET is now mechanical, not remembered (#727 round 6 review,
    /// non-blocking 1).**
    ///
    /// The `fn()` guard above bites — rename a cited test and you get a compile error — but its list
    /// is hand-maintained, i.e. the same memory-scoped act one level down. A twenty-line scan found
    /// two misses in seconds: a real test cited in a doc comment in the guard's own file and named in
    /// no list, and a citation in `collision.rs` that resolved to nothing anywhere in the workspace.
    /// This test is what stops the third.
    ///
    /// **The two corpora, named — because a sweep is (terms × corpus) and the recurring defect here
    /// has been running the right terms over the wrong corpus.**
    ///
    /// * **Citations are READ from** whatever [`citation_corpus`] returns — today five files. Files
    ///   outside it are not scanned and this test claims nothing about them.
    /// * **Names are RESOLVED against** every `.rs` in the whole workspace (`crates/`, `tests/`,
    ///   `src/`, minus any `target/`) — deliberately wider than the citation corpus, so a citation
    ///   that points at another crate's test resolves instead of being reported as rot. `walker.rs`
    ///   cites one in `eqoxide-net`, so this width is load-bearing, not decorative.
    ///
    /// **The rule.** For every backticked, lower-snake identifier in a doc comment with at least
    /// three underscores (four or more words — the shape test names take in this crate, and a stated
    /// heuristic rather than a proof: a cited two-word test name would slip through):
    ///
    /// 1. if it is a `#[test] fn` **in the same file as the citation** → it must appear in that
    ///    file's `_cited` / `_helpers` guard, so a rename is a compile error;
    /// 2. else if it resolves to any `fn` in the resolution corpus → fine (rustdoc's own link check
    ///    covers the public ones, and cross-module test items cannot be named from here anyway);
    /// 3. else it must be listed in `NOT_A_FN` below **with a reason**. That inversion is the point:
    ///    the default is "must resolve", and every exception is written down and argued.
    ///
    /// **Verified to bite, by execution, on both halves** (round 6, each mutation applied → run →
    /// reverted): deleting one name from the `_cited` array above reports *"is a #[test] in this file
    /// … but no `_cited`/`_helpers` guard in this file names it"*; adding `a_test_that_does_not_exist_anywhere`
    /// to `resync_cursor`'s rustdoc reports *"resolves to NO fn in the resolution corpus"*. It has
    /// also bitten twice unprompted: on its very first run, on a citation this same round had just
    /// added and not guarded; and on the paragraph you are reading, whose quoted mutation name it
    /// flagged as unresolvable — correctly — forcing the `NOT_A_FN` entry below.
    ///
    /// **The two harder shapes were verified the same way** (mutation applied → run → reverted,
    /// `md5sum -c` clean). Re-wrapping `walker.rs`'s `zone_assets::…` citation back across two lines
    /// reports *"a code span opens on this line and closes on another"* on **both** lines, and —
    /// because `:` is in the charset — the truncated leading fragment additionally reports *"resolves
    /// to NO fn"*; retyping a `::`-qualified citation to a name that does not exist
    /// (`steering::route_goal_offset_reports_vertical_shortfall_only`) reports the same. Two of those
    /// four reports now arrive on the span test's result line rather than this one's, since #788
    /// split the halves.
    ///
    /// **What it does NOT do**, so nobody reads it as more:
    ///
    /// * it does not check that a citation is *apt* — only that the name exists and is pinned
    ///   against renaming;
    /// * its `>= 3 underscores` filter is a heuristic about this crate's naming, not a proof;
    ///   `walker_cursor_resync`, the round-4 dangling citation, has two and would still slip past it;
    /// * for a `::`-qualified path it resolves the **tail only** (`resolution_name`). A citation
    ///   whose module prefix is wrong but whose final identifier exists elsewhere in the workspace
    ///   resolves and is not reported. Prefixes are prose here, not links;
    /// * lines inside a triple-backtick fence are exempt from the **unbalanced-span check only**.
    ///   `doc_citations` has no fence check at all, so a citation written inside a fenced example is
    ///   still resolved (rule 2/3) and, if it is a same-file `#[test]`, still required to be in the
    ///   `_cited`/`_helpers` guard (rule 1) — verified by execution, both halves;
    /// * **backtick parity is not span-crossing.** Escaping a literal backtick in prose (the standard
    ///   CommonMark double-backtick escape) both false-fails the balance check on its own line, and —
    ///   applied the same way on both sides of a genuine wrap — can make both lines land on an even
    ///   count and hide it from `unbalanced_doc_spans` entirely. The same padding evades
    ///   `doc_citations` too, for a distinct reason: that scan selects citation text by its position
    ///   in the line's backtick-split (`skip(1).step_by(2)`, only odd indices read), and the padding
    ///   shifts the citation's fragment off an odd index onto an even one, where nothing ever reads
    ///   it. Demonstrated by execution against this exact corpus.
    ///
    /// **Why the name is this long.** It used to be
    /// `every_doc_comment_test_citation_resolves_and_is_listed_in_a_guard`; "every doc comment" reads
    /// as the workspace, and the name is what a `cargo test` result line shows. #788 also split the
    /// unbalanced-code-span half out into
    /// `unbalanced_doc_spans_are_rejected_in_the_five_citation_files_only_not_the_workspace` for the
    /// same reason: it ran inside this test's loop and its green was read as workspace coverage by
    /// three independent readers in two days.
    ///
    /// **The workspace outside these five files is NOT clean by the span check.** #789 named that
    /// backlog rather than fixing it: `unbalanced_doc_code_spans_in_the_whole_workspace_are_a_named_shrinking_backlog`
    /// runs the same `scan_doc_spans` over the full resolution corpus and holds it to zero NEW
    /// offenders — pre-existing ones are listed individually in that test's `KNOWN_VIOLATIONS`, which
    /// must shrink as they are fixed (a stale entry fails the build) and cannot silently grow (an
    /// unlisted offender fails the build). The five citation files are held to exactly zero,
    /// unconditionally. Deliberately no COUNT is quoted anywhere here: a count in a comment is a
    /// measurement with an expiry date, and this doc has already had to delete one that went stale
    /// inside a single review round.
    #[test]
    fn every_test_citation_in_the_five_citation_files_resolves_and_is_listed_in_a_guard() {
        use std::collections::{HashMap, HashSet};
        use std::path::PathBuf;

        /// Citations that deliberately name something that is not a `fn` in this workspace. Each
        /// entry is a claim in its own right; if one stops being true, delete it and the scan will
        /// say so.
        const NOT_A_FN: &[(&str, &str)] = &[
            ("the_resync_clears_the_deadlock_above_the_guard_and_is_inert_below_it",
             "a test renamed in round 3, quoted verbatim in the SIBLING guard \
              `every_test_name_cited_in_a_doc_comment_still_exists`'s rustdoc — its round-4 \
              dangling-citation paragraph — so the retracted name is preserved rather than \
              deleted. NOT in this fn's own rustdoc: an earlier locator said the blockquote on \
              `CURSOR_STALE_DIST` and a later one said this fn, and both were wrong. Inherently \
              unguardable."),
            ("open_air_ceiling_is_never_returned_as_floor",
             "a fixture RETRACTED at PR-D/D-2 and deleted; the doc that names it IS its retraction \
              note."),
            ("baked_zone_has_collision_mesh_with_invisible_faces",
             "a test in the asset-server repository, not this workspace; the doc says so."),
            ("zone_assets_stale_for_previous_zone",
             "a `nav_reason` string (`NotUsable::StaleForPreviousZone::as_str`), not a fn."),
            ("local_no_way_through",
             "a `stop_nav` reason string, not a fn."),
            ("arrived_at_goal_tier",
             "a local binding in the walker sim's arrival check, not a fn."),
            ("route_goal_offset_reports_vertical_shortfall_only",
             "the deliberately-wrong name round 7's `::`-resolution mutation retyped \
              `steering::route_goal_offset_reports_horizontal_shortfall_only` to, quoted in this \
              fn's rustdoc as the evidence that half bites. It resolves to nothing by design — and \
              the scan caught it here too, on the run that added the paragraph, which is the second \
              time this doc has had to buy its own exception."),
            ("a_test_that_does_not_exist_anywhere",
             "the deliberately-nonexistent name this scan's own mutation check injected, quoted in \
              this fn's rustdoc as the evidence that the check bites. Its whole point is that it \
              resolves to nothing — and the scan caught it here, unprompted, on the run that added \
              that paragraph."),
            ("every_doc_comment_test_citation_resolves_and_is_listed_in_a_guard",
             "this test's own name before #788 renamed it, quoted in its rustdoc as the retracted \
              name rather than deleted. Inherently unresolvable — that is what renamed means."),
            ("the_resync_clears_the_carrot_pinning_above_the_guard_and_is_inert_below_it",
             "the hairpin sweep's name between round 3 of #727 and #733, quoted verbatim under \
              'Grepping for the retired name' in \
              `the_resync_clears_the_carrot_pinning_at_every_leg_separation_measured`'s rustdoc, \
              because the name asserted 'inert below the guard' — which #733 made false. Retired, \
              so inherently unresolvable."),
        ];

        // ── corpus 1: where citations are read from ──────────────────────────────────────────────
        let cited_in: Vec<PathBuf> = citation_corpus().to_vec();
        // ── corpus 2: where names are resolved against ───────────────────────────────────────────
        let resolve_in: Vec<PathBuf> = workspace_rs_files();
        // A silently empty corpus would make this test vacuously green — the exact failure mode it
        // exists to prevent. Pin both ends.
        for p in &cited_in {
            assert!(p.is_file(), "citation corpus file is missing: {}", p.display());
        }
        assert!(resolve_in.len() >= cited_in.len(),
            "resolution corpus is smaller than the citation corpus ({} files) — the source tree is \
             not where this test thinks it is, and every check below would pass vacuously",
            resolve_in.len());

        let read = |p: &PathBuf| std::fs::read_to_string(p)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));

        // `fn NAME` declarations, and which of them carry a `#[test]` above them.
        let scan_fns = |src: &str, tests_out: &mut HashSet<String>, all_out: &mut HashSet<String>| {
            let mut pending_test = false;
            for line in src.lines() {
                let t = line.trim_start();
                if t.starts_with("#[test]") { pending_test = true; continue; }
                let Some(name) = fn_name_on(line) else { continue };
                if pending_test { tests_out.insert(name.clone()); pending_test = false; }
                all_out.insert(name);
            }
        };
        let mut all_fns: HashSet<String> = HashSet::new();
        let mut tests_by_file: HashMap<PathBuf, HashSet<String>> = HashMap::new();
        for p in &resolve_in {
            let src = read(p);
            let mut tests = HashSet::new();
            scan_fns(&src, &mut tests, &mut all_fns);
            tests_by_file.insert(p.clone(), tests);
        }

        let mut problems: Vec<String> = Vec::new();
        for p in &cited_in {
            let src = read(p);
            let guard = guard_entries(&src);
            let own_tests = tests_by_file.get(p).cloned().unwrap_or_default();
            for (name, line) in doc_citations(&src) {
                let where_ = format!("{}:{line}", p.file_name().unwrap().to_string_lossy());
                let resolved = resolution_name(&name).to_string();
                if own_tests.contains(&resolved) {
                    if !guard.contains(&resolved) {
                        problems.push(format!(
                            "{where_}: `{name}` is a #[test] in this file cited in a doc comment, \
                             but no `_cited`/`_helpers` guard in this file names it — a rename would \
                             rot the citation silently. Add it to the guard array."));
                    }
                } else if !all_fns.contains(&resolved) {
                    if !NOT_A_FN.iter().any(|(n, _)| *n == resolved) {
                        problems.push(format!(
                            "{where_}: `{name}` is cited in a doc comment and resolves to NO fn in \
                             the resolution corpus. Either the citation is stale, or it names \
                             something that is not a fn — in which case add it to NOT_A_FN with a \
                             reason."));
                    }
                }
            }
            // The structural check for a code span wrapped across two lines used to run here, over
            // this same loop's four-file corpus. #788 moved it to its own test — same four files,
            // its own result line — because a green line naming *this* test was read as workspace
            // coverage. It remains independent of the citation loop above, which reads each line's
            // chunks on its own and so can ALSO fire on a wrapped citation's truncated fragment
            // (round 9 review: verified by execution, both halves firing in the same run — after
            // #788, in the same `cargo test` run but on two result lines). Neither subsumes the
            // other.
        }
        // Dead exceptions are their own kind of stale claim.
        // Under the RESOLUTION name, not the written one — `NOT_A_FN` is keyed on what a citation
        // resolves to, so a `::`-qualified exception must match here too or it reads as dead.
        let all_cited: HashSet<String> = cited_in.iter()
            .flat_map(|p| doc_citations(&read(p)).into_iter()
                .map(|(n, _)| resolution_name(&n).to_string()))
            .collect();
        for (n, _) in NOT_A_FN {
            if !all_cited.contains(*n) {
                problems.push(format!(
                    "NOT_A_FN lists `{n}`, which no doc comment in the citation corpus cites any \
                     more. Delete the exception."));
            }
        }
        assert!(problems.is_empty(), "doc-comment citation scan found {} problem(s):\n  {}",
            problems.len(), problems.join("\n  "));
    }

    /// The workspace root: two levels above this crate's manifest directory.
    fn workspace_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().and_then(|p| p.parent())
            .expect("crate is two levels under the workspace root")
            .to_path_buf()
    }

    /// **The citation corpus: the five files, as a value.**
    ///
    /// Every scan in this module that READS doc comments reads exactly these five — the citation
    /// scan, the code-span scan, and the code-span scan's reach control. One definition, so a reach
    /// control cannot end up measuring a corpus the guard does not use.
    ///
    /// The return type is `[PathBuf; 5]`, not a `Vec`, on purpose: three test names in this module
    /// say "five_citation_files", and the length in this signature is one place a reader is forced
    /// to edit, and to see the number, when the corpus grows.
    ///
    /// **THE LIMIT of that, measured (#882 round 2).** The length does *not* make growth "a compile
    /// error at every call site" — grown to `[PathBuf; 6]` with a sixth path,
    /// `cargo test -p eqoxide-nav --lib --no-run` builds the test executable with **zero errors**.
    /// No call site binds the length: `citation_corpus().to_vec()`, `let files = citation_corpus()`,
    /// `&citation_corpus()` and `.len()` are all length-agnostic. The only edit the compiler forces
    /// is the one in this signature, and nothing mechanical catches the three
    /// `…_five_citation_files…` names drifting — they are renamed by hand, as #874 had to for 4 → 5.
    ///
    /// **Why `src/movement.rs` is in the corpus (#874).** `CharacterController` lives there and its
    /// `MUTATION-CHECK` blocks cite test names by name, previously read by nothing mechanical: #866
    /// found two doc defects there by hand (a `MUTATION-CHECK` quoting a message its own assertion
    /// cannot produce, and an "either of two" claim where there are three) that a mechanical check
    /// would have caught if it read the file at all. Its pre-existing unbalanced doc spans were
    /// fixed before it was added, so the corpus started clean rather than landing the guard red.
    fn citation_corpus() -> [std::path::PathBuf; 5] {
        let crate_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let ws = workspace_root();
        [
            crate_root.join("src/steering.rs"),
            crate_root.join("src/walker.rs"),
            crate_root.join("src/collision.rs"),
            ws.join("tests/walker_sim.rs"),
            ws.join("src/movement.rs"),
        ]
    }

    /// Every `.rs` file under the workspace's `crates/`, `tests/`, `src/` and `tools/`, minus
    /// anything under a `target/` directory — generated sources, where a stale build artifact could
    /// vouch for a citation. Sorted, so a failure message is stable between runs.
    ///
    /// **`tools/` is in the walk on purpose (#882 round 2).** It is a first-class
    /// `[workspace] members` entry in the root `Cargo.toml`, and leaving it out once made this fn
    /// cover strictly fewer files than the workspace while every doc built on top of it said "the
    /// full resolution corpus" — the #788 defect class, a corpus narrower than its name, inside the
    /// PR whose whole subject was corpus reach. `tools/src/main.rs` carried two genuine unbalanced
    /// doc spans nothing was holding.
    ///
    /// This is the resolution corpus for the citation scan AND, since #789, the corpus the
    /// doc-span backlog test scans for defects.
    fn workspace_rs_files() -> Vec<std::path::PathBuf> {
        let ws = workspace_root();
        let mut out: Vec<std::path::PathBuf> = Vec::new();
        let mut stack = vec![
            ws.join("crates"), ws.join("tests"), ws.join("src"), ws.join("tools"),
        ];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n == "target") { continue; }
                    stack.push(p);
                } else if p.extension().is_some_and(|s| s == "rs") { out.push(p); }
            }
        }
        out.sort();
        out
    }

    /// Read a corpus once, so a guard and its reach control scan byte-identical input rather than
    /// two reads that could disagree.
    fn read_corpus(files: &[std::path::PathBuf]) -> Vec<(std::path::PathBuf, String)> {
        files.iter().map(|p| {
            let src = std::fs::read_to_string(p)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
            (p.clone(), src)
        }).collect()
    }

    /// What `scan_doc_spans` returns. `files_scanned` and `lines_scanned` are here so that a scan
    /// which stops part-way through its corpus is a LOUD failure and not a quieter green: #760's
    /// round-4 finding was a source scanner that silently stopped at roughly an eighth of its
    /// corpus, and every one of the twelve mutation probes aimed at it happened to sit inside the
    /// window it could still see, so a twelve-cell mutation table proved nothing.
    struct DocSpanScan {
        offenders: Vec<(std::path::PathBuf, usize)>,
        files_scanned: usize,
        lines_scanned: usize,
    }

    /// Run `unbalanced_doc_spans` over a whole corpus, reporting what it found AND how far it got.
    fn scan_doc_spans(corpus: &[(std::path::PathBuf, String)]) -> DocSpanScan {
        let mut offenders = Vec::new();
        let mut files_scanned = 0usize;
        let mut lines_scanned = 0usize;
        for (p, src) in corpus {
            files_scanned += 1;
            lines_scanned += src.lines().count();
            for line in unbalanced_doc_spans(src) { offenders.push((p.clone(), line)); }
        }
        DocSpanScan { offenders, files_scanned, lines_scanned }
    }

    /// **A code span must not open on one line and close on the next — IN THE FIVE CITATION FILES.
    /// Held to a separate, WIDER standard elsewhere. (#788, corpus grown by #874)**
    ///
    /// `cargo doc` renders the line break inside the span, so the rendered docs show something the
    /// source does not say. This is the check; `unbalanced_doc_spans` is the mechanism, and its own
    /// doc records what backtick parity does and does not imply.
    ///
    /// **Why this is a test of its own.** Until #788 this ran inside
    /// `every_test_citation_in_the_five_citation_files_resolves_and_is_listed_in_a_guard`'s loop —
    /// the same files, and honestly described in that test's body. But the corpus was legible
    /// only to a reader of the *code*. A reader of the *result* saw one passing line whose name
    /// began "every doc comment", and read it as the workspace. That happened to three independent
    /// readers within two days, one of them twice, including on two other PRs whose doc edits landed
    /// outside the corpus and were therefore covered by nothing. The corpus is now in the
    /// name, which is the only part of a green run anybody reads.
    ///
    /// **The corpus, stated exactly**, and identical to the citation corpus by construction (both
    /// call `citation_corpus`): this crate's `src/steering.rs`, `src/walker.rs` and
    /// `src/collision.rs`, the workspace's `tests/walker_sim.rs`, and — since #874 —
    /// `src/movement.rs`. Five files. This test holds them to **zero** offenders, unconditionally.
    ///
    /// **The rest of the workspace is not unclaimed any more (#789).**
    /// `unbalanced_doc_code_spans_in_the_whole_workspace_are_a_named_shrinking_backlog` runs
    /// the identical `scan_doc_spans` mechanism over every `.rs` file `workspace_rs_files()` walks
    /// and holds it to zero *new* offenders, with pre-existing ones named individually and required
    /// to shrink. That corpus is a superset of these five, which it also scans (and finds clean);
    /// the split exists so a citation-corpus regression fails with a five-file, un-allowlistable
    /// message rather than a backlog-list diff.
    ///
    /// **Reach, not just shape.** That this check *can* detect a wrapped span says nothing about
    /// whether it *arrives* at file 5, or at line 2000 of file 1.
    /// `the_doc_span_scan_reaches_all_five_citation_files_at_three_depths_each` is the control for
    /// that, and it injects its probes into every file of this exact corpus, at three depths each.
    #[test]
    fn unbalanced_doc_spans_are_rejected_in_the_five_citation_files_only_not_the_workspace() {
        let files = citation_corpus();
        for p in &files {
            assert!(p.is_file(), "citation corpus file is missing: {}", p.display());
        }
        let corpus = read_corpus(&files);
        let scan = scan_doc_spans(&corpus);
        assert_eq!(scan.files_scanned, files.len(),
            "the span scan visited {} of {} corpus files — it stopped early and its result is not \
             about the corpus it names",
            scan.files_scanned, files.len());
        // Visible with `-- --nocapture`; the corpus is in the test NAME for everyone else.
        println!("doc-span corpus: {} files, {} lines — {}",
            scan.files_scanned, scan.lines_scanned,
            files.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>().join(", "));
        let report: Vec<String> = scan.offenders.iter()
            .map(|(p, line)| format!(
                "{}:{line}: a code span opens on this line and closes on another. `cargo doc` \
                 renders the break inside the span. Keep the span on one line.",
                p.file_name().unwrap().to_string_lossy()))
            .collect();
        assert!(report.is_empty(),
            "doc-span scan found {} problem(s) in the {} citation files:\n  {}",
            report.len(), files.len(), report.join("\n  "));
    }

    /// **#789: the doc-span scan, widened to the WHOLE workspace — a named, shrinking backlog
    /// instead of an unclaimed pile.**
    ///
    /// The guard above holds the five citation files to zero. #789 measured that the workspace
    /// outside them is not clean by the same check. `KNOWN_VIOLATIONS` below is the list as
    /// re-measured for that change, and the live totals are **printed by this test at run time**
    /// (see its `println!`) rather than restated here — any count written here drifts with every
    /// merge.
    ///
    /// **Do not write a corpus-size figure into this doc.** One was, and it matched no tree —
    /// neither `main` nor the branch — while the test printed the true one twelve lines below it. A
    /// corpus-size figure inside the corpus it measures is self-invalidating: editing the sentence
    /// changes it.
    ///
    /// **What this test does NOT do: bulk-fix or silently bless the backlog.** Every entry is named
    /// individually, by file and by the text of the offending line. Two things keep the list honest
    /// instead of decorative:
    ///
    /// * an offender the scan finds that is **not** in this list fails the build — the backlog
    ///   cannot silently grow, and a doc edit anywhere in the workspace that introduces a new wrap
    ///   is caught the moment it lands, not the next time someone widens a corpus by hand;
    /// * an entry in this list the scan **no longer finds** also fails the build — fixing one and
    ///   not deleting its entry here is itself a build break, so the list can only shrink by an
    ///   explicit deletion next to the fix that earned it, and can never go stale in the safe
    ///   direction silently.
    ///
    /// Both halves are reported by **one** assertion, deliberately: keyed separately, a fixer sees
    /// "add it" on the first run and "now delete the stale one" only on the next, which is two
    /// round-trips for one edit.
    ///
    /// **Why the key is the line's TEXT and not its line number (#882 round 2, blocking).** The
    /// list was keyed on `(file, absolute line)`. Three measurements, all re-derived rather than
    /// argued, all over `crates/`, `tests/`, `src/` and `tools/`:
    ///
    /// * **Backwards, over merged history.** Of the 159 offenders at `origin/main` (`749c932`),
    ///   121 sat at the same `file:line` five commits earlier, **28 carried byte-identical
    ///   normalized text at a *different* line**, and only 10 were genuinely new or edited text.
    ///   Line keys would have needed 28 hand-edits in five commits; text keys, zero.
    /// * **Forwards, against a PR in flight.** `fix-797-848-cap-reporting` (`e89985a`) changes no
    ///   doc span at all, yet compared with its own merge-base it moves **30** offenders to a
    ///   different line, across 7 files. Line keys: 30 entries to re-derive by hand, or a red
    ///   `main` for whichever of the two PRs merges second — with **no git conflict** to warn
    ///   anyone, since the two touch unrelated concerns. Text keys: **0**.
    /// * **Observed, on this branch's own merge — not a constructed example.** Merging
    ///   `origin/main` at `749c932` into this branch moved four offenders in
    ///   `crates/eqoxide-core/src/game_state.rs` down ten lines (880/881/1331/1332 to
    ///   890/891/1341/1342) with byte-identical text. Under line keys that routine merge is
    ///   4 NEW and 4 DEAD — a red build, from a merge that changed no doc comment this list
    ///   names. Under text keys the offender multiset is unchanged: **0 NEW, 0 DEAD**.
    ///
    /// (A later merge of `origin/main` at `6ca5ab7` moved nothing, but is **not** a fourth
    /// data point: every file it touched is a citation-corpus file held to zero offenders, so no
    /// entry could have shifted under either keying.)
    ///
    /// Every one of those edits would be forced on an author who did nothing but insert a line
    /// somewhere above, and would arrive as a red `main` naming a file whose author did nothing
    /// wrong. Keyed on `(file, normalized line text)` both directions above still bite (proved by
    /// mutation, both ways) and the only thing that reopens an entry is somebody actually touching
    /// that doc line. The line number is not stored at all — not even as a trailing comment, since a
    /// comment nobody is forced to update is the stale-figure defect this module keeps re-learning;
    /// the failure message prints the live line number instead.
    ///
    /// The mechanism for the backlog going to zero is: fix a wrap, delete its entry from
    /// `KNOWN_VIOLATIONS` in the same change (the dead-entry check requires it). Nothing here
    /// schedules that work; it only makes the backlog visible, bounded, and unable to grow without
    /// being caught.
    ///
    /// Reach is proven separately by
    /// `the_doc_span_scan_reaches_the_full_resolution_corpus_at_three_depths_each` — this test
    /// pins the reach of the RUN it actually did (`files_scanned` below) but does not itself prove
    /// the scanner cannot silently shrink its own corpus over time; the sibling reach control does.
    #[test]
    fn unbalanced_doc_code_spans_in_the_whole_workspace_are_a_named_shrinking_backlog() {
        use std::collections::HashMap;

        // (workspace-relative path, the offending doc lines' NORMALIZED text — see
        // `normalized_doc_line`). Grouped by file, in the order the lines appear in it. Re-measure
        // with `cargo test -p eqoxide-nav --lib unbalanced_doc_code_spans -- --nocapture` after any
        // fix and delete the corresponding entry — a stale entry fails this test on its own.
        const KNOWN_VIOLATIONS: &[(&str, &[&str])] = &[
        ("crates/eqoxide-command/src/nav.rs", &[
            r#"cancel and the *success* path of a zone crossing all published `idle` with `nav_reason:"#,
            r#"null`, which is byte-identical to "no request has ever been made" — so the endpoint's"#,
        ]),
        ("crates/eqoxide-core/src/config.rs", &[
            r#"than `std::env::set_current_dir` — so `load_wires_the_fallback_dir_through_"#,
            r#"to_load_with_fallback_dir` below can drive the real `load()` call site"#,
        ]),
        ("crates/eqoxide-core/src/eqstr.rs", &[
            r#"arrives — the server sends `string_id=554` with `args=[npc_name, "1148", player_name,"#,
            r#"probably find a %4 handy."`) is itself resolved from arg slot 2, whose own `%3`/`%4`"#,
        ]),
        ("crates/eqoxide-core/src/game_state.rs", &[
            r#"(`eq_constants.h` `Animation`: `Standing=100, Freeze=102, Looting=105, Sitting=110,"#,
            r#"which every `underworld_no_recovery` hold guarantees (its arm runs only inside `if"#,
            r#"!self.on_ground`) — so on that shape the republish cannot wake a loop that was not already"#,
            r#"and `Mob::SendSpellBarEnable` (zone/spells.cpp:5752) send with `spell_id = the cast that"#,
            r#"ended`. It is the ONLY way to name the spell in a *fizzle*: EQEmu decides a fizzle in"#,
        ]),
        ("crates/eqoxide-core/src/physics.rs", &[
            r#"`anim = speed_u_per_s * (0.7 * 40 / RUN_SPEED)` — EQEmu computes `base_runspeed = runspeed_float *"#,
            r#"40` with the player special-case run `0.7 → 28` and walk `0.3 → 12` (`EQEmu/zone/mob.cpp:190-196`,"#,
        ]),
        ("crates/eqoxide-core/src/region_map.rs", &[
            r#"- **v1** record = 36 bytes: `i32 node_number; f32 normal[3]; f32 split; i32 region; i32 special;"#,
            r#"i32 left; i32 right`."#,
        ]),
        ("crates/eqoxide-core/src/zone_map.rs", &[
            r#"`#[ignore]`d (CI has no client cache) — run explicitly with `cargo test -p eqoxide-core --lib"#,
            r#"zone_map::tests::diagnostic_measure_contributing_zones_869 -- --ignored --nocapture`."#,
        ]),
        ("crates/eqoxide-crash/src/lib.rs", &[
            r#"thing that kills the process. `src/http/mod.rs` adds a second, more specific `INSTANCE"#,
            r#"api_port=<N>` line later if (and only if) the listener actually binds; this fallback line is"#,
        ]),
        ("crates/eqoxide-http/src/combat.rs", &[
            r#"#513 (agent-honesty): the response now DISCLOSES the matched entity — `matched:{id, name,"#,
            r#"quality, distance?}` — so the caller can confirm the resolution picked the intended spawn."#,
        ]),
        ("crates/eqoxide-http/src/guild.rs", &[
            r#"conjuncts 2 or 3 — deleting either of them left the round-2 suite green at `240 passed;"#,
            r#"0 failed` (the round-3 reviewer's measurement, mutations M-R3a and M-R3c; not re-run here,"#,
        ]),
        ("crates/eqoxide-http/src/lib.rs", &[
            r#"softening. And covered by tests, and STILL absent from `GET"#,
            r#"/v1/observe/debug`, because nothing serialises `PlayerState` whole: `observe::get_debug`"#,
        ]),
        ("crates/eqoxide-http/src/merchant.rs", &[
            r#"• 200 — the server CONFIRMED the open (OP_ShopRequest echo, command=1). Body: `{status:"open","#,
            r#"merchant_id}`. Watch GET /v1/merchant/list for the item list arriving."#,
            r#"window (RoF2's server collapses all of these into the same echo). Body: `{status:"refused","#,
            r#"reason}`."#,
            r#"GET /v1/merchant/list — the open merchant's offered items (for buying). Returns `{open,"#,
            r#"merchant_id, count, items:[{merchant_slot,item_id,name,icon,price,quantity}]}`. `open:false`"#,
            r#"• 200 — the server CONFIRMED the buy (OP_ShopPlayerBuy echo). Body: `{status:"bought", item,"#,
            r#"price, coin_after}` read back from the applied receipt."#,
        ]),
        ("crates/eqoxide-http/src/name_match.rs", &[
            r#"This is not the only place in the HTTP layer that holds both at once — `move_api::"#,
            r#"current_target_match` also does, in the same canonical order. The invariant that actually"#,
        ]),
        ("crates/eqoxide-http/src/observe.rs", &[
            r#"are online before coordinating). Returns `{online: [{name, level, class, race, zone_id, guild,"#,
            r#"anon}]}`. 503 if no response arrives in time. (#300)"#,
            r#"appeared in `last_ended` **0 times** — buried by `startup game data`, then `model-sync"#,
            r#"worker`, then `zone load: neriakc`. The documented recipe ("had a login fail →"#,
            r#"publisher itself updates. Mirrors `last_packet_age_advances_between_reads_with_no_publisher_"#,
            r#"running` above, over one of the newly-added JSON fields instead of the pre-existing `/debug`"#,
        ]),
        ("crates/eqoxide-http/src/refusal.rs", &[
            r#"on top: with the mailbox **free** it queues the command *and* answers `409 … (it was NOT"#,
            r#"queued)` (so an agent that trusts "409 is definitive" retries and double-fires), and with the"#,
        ]),
        ("crates/eqoxide-http/src/testkit.rs", &[
            r#"guarded by `observe::tests::no_past_dated_net_health_stamp_is_taken_from_a_clock_other_than_the_"#,
            r#"one_that_reads_it` — a source scan over four files, one statement at a time. It catches the"#,
            r#"[`empty_state`] and derive the stamp from the fixture's own clock (`let c = h.clock;"#,
            r#"h.last_probe_sent = Some(c.ago(15));`). A wall-clock `ago(15)` read back against a pinned"#,
        ]),
        ("crates/eqoxide-ipc/src/asset_sync.rs", &[
            r#"**The adjudication, all four cells measured at round 9** (`-p eqoxide-ipc --locked"#,
            r#"--no-fail-fast`; the round-7 head is reproduced exactly, since `5df7099..55ecbff` changes no"#,
        ]),
        ("crates/eqoxide-ipc/src/lib.rs", &[
            r#"`eqoxide-core` and below everything else — the layering is `core ← ipc ← {net, render, http,"#,
            r#"command, …}` — and depends ONLY on `eqoxide-core` plus the low-level channel/serde primitives"#,
            r#"review, B1: `after begin_zone_in: hold=None` → `after ONE net tick:"#,
            r#"hold=Some(EmbeddedNoRecovery, 7.5)`) — the mirror faithfully re-manufacturing a stale claim"#,
            r#"level down — it happened, in review, to `debug_reports_world_unresponsive_when_a_probe_goes_"#,
            r#"unanswered_while_the_link_acks` (a 15s stamp that had to clear a 10s bound: a 5s margin)."#,
            r#"publisher in the most idiomatic Rust form — `world.entity_positions.lock().unwrap()"#,
            r#".insert(..)`, mutation through a temporary guard with no binding at all — and the suite"#,
            r#"- A production publisher written the idiomatic way — `world.entity_positions.lock().unwrap()"#,
            r#".insert(..)` — fails to compile under **both** `cargo test --workspace` and"#,
            r#"Command-with-result buy request (A3 Migration 1, #448) — `(merchant spawn id, merchant slot,"#,
            r#"oneshot Sender)`. POST /v1/merchant/buy writes this and AWAITS the `Sender`; the nav thread"#,
            r#"Command-with-result merchant-open request (A3 migration, eqoxide#479) — `(merchant spawn id,"#,
            r#"oneshot Sender)`. POST /v1/merchant/open writes this and AWAITS the `Sender`; the nav thread's"#,
            r#"Command-with-result give request (A3 Migration 2, #448) — `(npc spawn id, item from_slot,"#,
            r#"oneshot Sender)`. POST /v1/interact/give writes this and AWAITS the `Sender`; the nav thread's"#,
            r#"3. **half-neuter it** — `let keep = self.disclosures().1; self.publish_disclosures((None,"#,
            r#"keep));` — so the hold is invalidated and the stall is not → RED at the stall assertion."#,
        ]),
        ("crates/eqoxide-ipc/src/result.rs", &[
            r#"detail read back from the applied receipt (e.g. `BuyOk { item_name,"#,
            r#"price, coin_after }`) — never an optimistic guess made at send time."#,
        ]),
        ("crates/eqoxide-nav/src/water_grid.rs", &[
            r#"MUTATION CHECK: make `ZoneWater::measure` return `WaterMeasurement { value: Some(f(&default)),"#,
            r#"reason: None }` in the `Unmeasured` arm (i.e. re-fabricate the old dry answer) and the"#,
            r#"MUTATION CHECK: make `WaterRollup::add` treat `(None, Some(_))` as `self.total += 0;"#,
            r#"self.measured_zones += 1` (the pre-fix behaviour) and `is_complete`/`unmeasured_zones`/the"#,
        ]),
        ("crates/eqoxide-nav/src/zone_assets.rs", &[
            r#"Measured, not reasoned: with a wildcard here and a fifth `ProbeRefreshing { zone,"#,
            r#"collision }` variant added (the enum has FOUR — `Idle`, `Pending`, `Ready`, `Failed`), the"#,
        ]),
        ("crates/eqoxide-net/src/action_loop.rs", &[
            r#"Refused before any packet (empty gem, or a stale/non-clicky item slot). `finish_cast(0,"#,
            r#""cast_failed", …)` was already recorded; the `String` is the human reason for the 409."#,
            r#"unconditionally on every ~10 ms net tick, so the departed zone's `Some(EmbeddedNoRecovery,"#,
            r#"7.5)` was back one tick after the clear —"#,
            r#"bug this test was added to catch) → the received `anim` collapses to ~1 (fails the `anim >="#,
            r#"20` assertion below), even though every pure-function unit test above still passes untouched."#,
            r#"* re-guard the off-region reset with the cooldown (`if index.is_none() &&"#,
            r#"self.last_zone_cross.elapsed() > ZONE_CROSS_COOLDOWN_MS`) — i.e. restore the pre-fix"#,
            r#"A STALE prior outcome (recorded BEFORE this cast parked) must never resolve it — the `at >"#,
            r#"sent_at` correlation is what keeps a previous cast's verdict from fabricating a result here."#,
            r#"MUTATION CHECK: revert the verdict to the all-slot name-scan (`any(slot < TRADE_BEGIN && name =="#,
            r#"item_name)`) and this goes RED — the duplicate in the general slot forces a bogus `Unconfirmed`."#,
        ]),
        ("crates/eqoxide-net/src/gameplay.rs", &[
            r#"tick, which is precisely the case (`the render loop publishes nothing at all across the"#,
            r#"load`) that `GameState::begin_zone_in`'s own doc claims it covers. The behaviour that follows"#,
        ]),
        ("crates/eqoxide-net/src/packet_handler.rs", &[
            r#"display text and decode the body's hex fields. Only real saylinks (body `item_id =="#,
            r#"SAYLINK_ITEM_ID`) become [`eqoxide_core::game_state::DialogueChoice`]s (click-to-say); every"#,
        ]),
        ("crates/eqoxide-net/src/transport.rs", &[
            r#"scheduler luck, not the transport. `connect`'s retry decision is `session_request_due(last_send,"#,
            r#"now)` (see its doc), which needs no real waiting to exercise — `Instant + Duration` builds a"#,
            r#"pass unchanged even if `connect()` is WIRED to it incorrectly, e.g. `let _ ="#,
            r#"session_request_due(...) { send(); /* forgot */ last_send = Instant::now(); }` (never resets"#,
            r#"encoded-body assertion is the mutation check: revert `send_out_of_order` to `send_raw(.., &seq"#,
            r#".to_be_bytes())` and the decoded-seq assertion fails (the raw bytes XOR-decode to garbage)."#,
        ]),
        ("crates/eqoxide-protocol/src/protocol/group.rs", &[
            r#"`Wrong size on incoming [OP_GroupDisband] (structs::GroupGeneric_Struct): Got [128], expected"#,
            r#"[148]` and silently dropped the packet (no roster change, no disband on either side). The"#,
        ]),
        ("crates/eqoxide-renderer/src/models.rs", &[
            r#"transcribed literally (it also still passes a `center_xz` argument), `error[E0308]:"#,
            r#"mismatched types` on a minimal transcription onto the 3-argument signature. Both measured;"#,
        ]),
        ("crates/eqoxide-renderer/src/pipeline.rs", &[
            r#"to a joint shader produced `character_skinned.wgsl: WGSL failed to parse: expected identifier,"#,
            r#"found '128'`. It also rewrote the shaders' own **comments**, so the text naga received described"#,
        ]),
        ("crates/eqoxide-renderer/src/renderer.rs", &[
            r#"Resolve a mesh's animated-texture spec `(ms, frame names)` into `(ms, frame texture"#,
            r#"indices)` against the loaded texture list. Returns `None` if fewer than 2 frames resolve."#,
        ]),
        ("crates/eqoxide-renderer/src/skin_observation.rs", &[
            r#"i.e. the reviewer's own mutation applied one round later: `error[E0308]: mismatched types"#,
            r#"… expected `ModelAsset`, found `Option<_>``, **twice** (once in `(lib)`, once in"#,
            r#"- **R1b** — `crate::models::ModelAsset::default()` in its place: `error[E0599]: no associated"#,
            r#"function or constant named `default` found for struct `ModelAsset``, twice. `ModelAsset`"#,
        ]),
        ("crates/eqoxide-renderer/tests/floating_placement.rs", &[
            r#"edit no longer compiles (measured: `error[E0061]: this function takes 4 arguments but 5"#,
            r#"arguments were supplied`). What is left open at the type level is calling"#,
            r#"separate `center_xz` argument, so it is `error[E0061]: this function takes 3 arguments but 4"#,
            r#"arguments were supplied`, with `expected &ModelBounds, found f32` as a sub-note rather than a"#,
            r#"standalone error; transcribed minimally onto the 3-argument signature it is `error[E0308]:"#,
            r#"mismatched types`, expected `&ModelBounds`, found `f32`. Neither compiles."#,
        ]),
        ("crates/eqoxide-renderer/tests/joint_cap_single_source.rs", &[
            r#"`discovered_corpus_is_not_silently_truncated` fail with `the shader walk never returned"#,
            r#"["ghost.wgsl"]` — the honest direction, but an alarm about a file that never existed."#,
            r#"This is the check that makes a hardcoded-but-currently-correct length impossible. `array<JMat,"#,
            r#"128>` written literally compiles to the same IR as the placeholder does today, so"#,
            r#"`const JOINT_CAP_SCALE: f32 = 1.0;` to a joint shader produced `character_skinned.wgsl: WGSL"#,
            r#"failed to parse: expected identifier, found '128'`. Exhaustive over the shapes an identifier can"#,
            r#"Every comment in `src`, in source order, as `(1-based line of its opener, text including the"#,
            r#"opener)`."#,
        ]),
        ("crates/eqoxide-renderer/tests/shadow_caster_selection.rs", &[
            r#"`ENTITY_DRAW_DIST` of the player, but one projects to NDC x = 1.6, outside `1.0 +"#,
            r#"ENTITY_CULL_MARGIN`. A mutant that drops the frustum test (keeping only the distance test)"#,
            r#"**Fixed (eqoxide#751): each scene now draws from its own `Rng(splitmix64(0x5EED_740 ^ scene as"#,
            r#"u64))`**, built by `build_scene(scene)` and never threaded across scenes (see that function's"#,
            r#"**not** catch the same coupling reached through an intermediate helper function (e.g. `fn"#,
            r#"prior_signal(scene: usize) -> u64 { let (c, ..) = build_scene(scene - 1); c.len() as u64 }` called"#,
        ]),
        ("crates/eqoxide-renderer/tests/shadow_shader.rs", &[
            r#"separate literal that merely reads the same. `masked_shadow_pipeline_binds_the_entry_point_"#,
            r#"this_file_grades` below couples them."#,
        ]),
        ("crates/eqoxide-renderer/tests/skin_cap_selection.rs", &[
            r#"The cap is INCLUSIVE — a skin with exactly `JOINT_CAP` joints fits, matching the deleted `<="#,
            r#"128`. Getting this off by one either rejects the widest rig that currently ships"#,
        ]),
        ("crates/eqoxide-renderer/tests/weather_shader.rs", &[
            r#"2. `pipeline.rs` wires the weather pipeline correctly: bind-group layouts `[camera_bgl,"#,
            r#"weather_bgl]`, two vertex buffers (the static quad + the per-instance particle buffer),"#,
        ]),
        ("crates/eqoxide-telemetry/src/lib.rs", &[
            r#"`Cargo.toml`). A bare `#[cfg(test)]` item is invisible outside `eqoxide-telemetry`'s own `cargo"#,
            r#"test` — `cfg(test)` is per-crate, so it would NOT exist in the rlib the app crate links against"#,
        ]),
        ("crates/eqoxide-ui/src/lib.rs", &[
            r#"SESSION. A body that sizes its canvas from `available - <hardcoded"#,
            r#"footer>` and then draws a taller footer overflows its allotment; the"#,
        ]),
        ("src/app.rs", &[
            r#"previous zone's collision grid, which is precisely the stale-ready lie `NotUsable::"#,
            r#"StaleForPreviousZone` exists to report."#,
            r#"`i32 index, 3×f32 normal, f32 split, i32 region, i32 special, i32 left, i32 right,"#,
            r#"i32 zone_line_index`."#,
        ]),
        ("src/camera_state.rs", &[
            r#"those types. Re-exported so every existing `crate::camera_state::{CameraMode,CameraCmd,"#,
            r#"CameraSnapshot}` path across the tree keeps resolving unchanged."#,
        ]),
        ("src/zone_in.rs", &[
            r#"**Complement, not duplicate.** `movement::tests::"#,
            r#"the_zone_change_reload_block_still_forgets_the_recovery_ring` asserts the line is *written*"#,
        ]),
        ("tests/synthetic_water_capability.rs", &[
            r#"puts a character on the lid (see `a_swimmer_at_the_pocket_swim_plane_holds_its_own_depth_not_"#,
            r#"the_lid`), so the position here is manually authored rather than reached by driving the"#,
        ]),
        ("tools/src/main.rs", &[
            r#"which is a valid **180° rotation**, NOT identity. The old `if denominator != 0 { … } else"#,
            r#"{ IDENTITY }` guard silently dropped those flips: the wolf's rear hind-leg-top bones store"#,
        ]),
        ];

        let resolve_in = workspace_rs_files();
        let ws = workspace_root();
        // A silently empty/shrunk corpus would make this test vacuously green.
        assert!(resolve_in.len() >= citation_corpus().len(),
            "resolution corpus ({} files) is smaller than the citation corpus — the source tree \
             is not where this test thinks it is",
            resolve_in.len());

        let corpus = read_corpus(&resolve_in);
        let scan = scan_doc_spans(&corpus);
        assert_eq!(scan.files_scanned, resolve_in.len(),
            "the span scan visited {} of {} workspace files — it stopped early and every count \
             below is about a corpus it did not actually cover",
            scan.files_scanned, resolve_in.len());

        // Keyed on TEXT, not line number — see this test's rustdoc for the measurement that
        // decided it. A multiset, not a set: two offenders in one file may normalize to the same
        // text, and losing one of them to set collapse would be a silent hole.
        let mut allowed: HashMap<(&str, &str), usize> = HashMap::new();
        let mut listed = 0usize;
        for (f, texts) in KNOWN_VIOLATIONS {
            for t in *texts { *allowed.entry((*f, *t)).or_default() += 1; listed += 1; }
        }

        // The offending line's text, looked up in the same bytes the scan read.
        let src_of: HashMap<&std::path::PathBuf, &String> =
            corpus.iter().map(|(p, s)| (p, s)).collect();
        let mut found: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (p, line) in &scan.offenders {
            let rel = p.strip_prefix(&ws).unwrap_or(p).display().to_string();
            let src = src_of[p];
            let text = normalized_doc_line(src.lines().nth(line - 1).unwrap_or_default());
            found.entry((rel, text)).or_default().push(*line);
        }

        // Both directions in ONE assertion: keyed separately, a fixer clears the "new" half, re-runs,
        // and only then learns about the "dead" half — two round-trips for one edit.
        let mut problems: Vec<String> = Vec::new();
        for ((rel, text), lines) in &found {
            let listed_here = allowed.get(&(rel.as_str(), text.as_str())).copied().unwrap_or(0);
            for line in lines.iter().skip(listed_here) {
                problems.push(format!(
                    "{rel}:{line}: NEW — a code span opens on this line and closes on another, and \
                     this line's text is not in KNOWN_VIOLATIONS. Either keep the span on one line, \
                     or — if this is a pre-existing offender just now coming into scope — add it to \
                     KNOWN_VIOLATIONS under {rel} as:\n      r#\"{text}\"#,"));
            }
        }
        for ((f, t), n) in &allowed {
            let found_here = found.get(&((*f).to_string(), (*t).to_string()))
                .map_or(0, |v| v.len());
            for _ in found_here..*n {
                problems.push(format!(
                    "{f}: DEAD — KNOWN_VIOLATIONS lists a line the scan no longer finds unbalanced. \
                     The backlog shrank; delete this entry:\n      r#\"{t}\"#,"));
            }
        }
        problems.sort();
        assert!(problems.is_empty(),
            "doc-span backlog: {} problem(s) across the workspace ({} entry/entries listed, {} \
             offender(s) found over {} files):\n  {}",
            problems.len(), listed, scan.offenders.len(), scan.files_scanned,
            problems.join("\n  "));

        println!("#789 backlog: {} allowlisted offender(s) tracked, {} files / {} lines scanned \
                  in the whole-workspace corpus",
            listed, scan.files_scanned, scan.lines_scanned);
    }

    /// The key an entry in `KNOWN_VIOLATIONS` is matched on: the offending source line with its
    /// leading/trailing whitespace and its `///` or `//!` marker stripped, and every internal
    /// whitespace run collapsed to a single space.
    ///
    /// Collapsing runs is not cosmetic. `check-wrapped-literals.py` fails the build on a run of 12+
    /// spaces inside a string literal (it is how a lost `\` line-continuation is detected), and two
    /// of the workspace's offending lines are inside an ASCII diagram with exactly that shape — so
    /// storing them verbatim would trip a different guard. Collapsing also means re-indenting a doc
    /// block, which moves no words, does not reopen an entry.
    fn normalized_doc_line(line: &str) -> String {
        let t = line.trim();
        let body = t.strip_prefix("///").or_else(|| t.strip_prefix("//!")).unwrap_or(t);
        body.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Which of a file's fence-safe insertion points a reach probe goes at.
    ///
    /// Bound **by value** in `PROBE_DEPTHS` and consumed by an exhaustive `match`: deleting a depth
    /// is a compile error in two places, not a silent loss of a third of the reach evidence. That is
    /// the remedy shape #760 landed after its round-4 finding, where a mutation table that *looked*
    /// twelve-wide was in fact one window wide.
    #[derive(Clone, Copy, Debug)]
    enum ProbeDepth { Top, Mid, End }

    /// The three depths every scanned file carries a probe at. Top, middle and end — because a
    /// scanner that stops early stops *inside* a file as readily as *between* files.
    const PROBE_DEPTHS: [ProbeDepth; 3] = [ProbeDepth::Top, ProbeDepth::Mid, ProbeDepth::End];

    /// 0-based line indices at which an inserted doc-comment line would NOT be inside a
    /// triple-backtick fence — i.e. positions where `unbalanced_doc_spans` will actually look at it.
    ///
    /// This mirrors that fn's own fence state machine, and it has to: a probe dropped inside a fence
    /// is exempt by design, would go unreported, and would read as "the scan did not reach here"
    /// when the scan was working correctly. Index 0 is always safe, so this never returns empty for
    /// a non-empty file.
    fn fence_safe_insertion_points(src: &str) -> Vec<usize> {
        let mut out = Vec::new();
        let mut in_fence = false;
        for (i, line) in src.lines().enumerate() {
            if !in_fence { out.push(i); }
            let t = line.trim_start();
            let Some(body) = t.strip_prefix("///").or_else(|| t.strip_prefix("//!")) else {
                in_fence = false;
                continue;
            };
            if strip_blockquote_markers(body).starts_with("```") { in_fence = !in_fence; }
        }
        out
    }

    /// Strip leading Markdown blockquote markers (one or more `>` characters, each optionally
    /// followed by whitespace, possibly repeated for a nested quote) so the fence check below can
    /// see a quoted triple-backtick fence line for what it is: a fence. #789 found this gap live in
    /// `movement.rs`, where a `⚠️ Correction` block quotes an old fenced example, its two fence
    /// lines each starting with a `>` marker before the triple backticks. rustdoc renders a quoted
    /// fence as a real fence — CommonMark nests fences inside blockquotes — but the fence check
    /// used to test the raw line, never saw a fence, so it never toggled `in_fence`, and the two
    /// fence-delimiter lines were scored as ordinary text with an odd backtick count each: two
    /// false positives from one real example, zero actual span breaks.
    fn strip_blockquote_markers(body: &str) -> &str {
        let mut rest = body.trim_start();
        while let Some(r) = rest.strip_prefix('>') { rest = r.trim_start(); }
        rest
    }

    /// **The reach control for the doc-span scan: it must ARRIVE, not merely match (#788).**
    ///
    /// A positive control proves the scanner recognises the shape it hunts. It says nothing about
    /// how far the scanner gets. #760's round-4 review found exactly that gap: a source-scanning
    /// guard had silently stopped near the start of its corpus, and all twelve cells of the
    /// mutation table built to prove it worked sat inside the one window still visible — nine of
    /// the twelve injected violations were invisible and the table still came out clean.
    ///
    /// So this control does not put a probe next to the guard. It takes the **real corpus**, reads
    /// it, and builds a mutated copy in which **every file carries three unbalanced-span probes**,
    /// one at each `PROBE_DEPTHS` position — the first fence-safe line, the middle one, and the last
    /// one. `fence_safe_insertion_points` picks positions the scan is not entitled to skip. Then it
    /// runs the **same** `scan_doc_spans` the guard runs, and requires:
    ///
    /// * every one of the 5 × 3 probes is reported, named by file and depth if not; and
    /// * the total offender count is exactly the unmutated baseline plus fifteen, so a scan that
    ///   reported the probes but dropped real findings on the way cannot pass either; and
    /// * the scan reports having visited all five files.
    ///
    /// A scan that stops after file 1, or after the first 200 lines of a 2600-line file, fails this
    /// with a list of the probes it never reached. Deleting a depth from `PROBE_DEPTHS` does not
    /// quietly shrink the evidence — it fails to compile.
    ///
    /// ⚠️ **Correction (#789).** A sibling control,
    /// `the_doc_span_scan_reaches_the_full_resolution_corpus_at_three_depths_each`, runs this same
    /// probe-and-count method over `workspace_rs_files()` instead of `citation_corpus()` — every
    /// file in the workspace, not just these five — so #789's wider backlog test has its own reach
    /// proof rather than borrowing this one's on the strength of sharing a helper.
    #[test]
    fn the_doc_span_scan_reaches_all_five_citation_files_at_three_depths_each() {
        assert_doc_span_scan_reaches_corpus(&citation_corpus(), "citation");
    }

    /// **#789's own reach control: the same proof, over `workspace_rs_files()` instead of the five
    /// citation files.**
    ///
    /// `unbalanced_doc_code_spans_in_the_whole_workspace_are_a_named_shrinking_backlog` claims
    /// to scan the whole resolution corpus, not just the five files the guard above already covers.
    /// That claim gets its own proof rather than resting on "it calls the same function" — #760's
    /// round-4 finding was exactly a scanner whose *positive* control passed while the scanner
    /// silently stopped short, and every probe used to validate it sat inside the window it could
    /// still see. Running the identical probe-and-count method over every file in the workspace
    /// instead of 5 is the difference between "a scanner that can find a wrapped span" and "a
    /// scanner that finds one wherever it is."
    ///
    /// Verified at this head by execution, at the two hardest truncation points rather than an easy
    /// one: a break after the second-to-last file (a tail truncation, which a probe set clustered
    /// near the start would miss) and a break after the first 200 lines of *every* file (the #778
    /// shape — full file reach, no line reach). Both fail this control; both are recorded in #882's
    /// PR body.
    #[test]
    fn the_doc_span_scan_reaches_the_full_resolution_corpus_at_three_depths_each() {
        assert_doc_span_scan_reaches_corpus(&workspace_rs_files(), "resolution");
    }

    /// Shared body for the two reach-control tests above: mutate a copy of `files` with three
    /// unbalanced-span probes per file (`PROBE_DEPTHS`), scan it, and require every probe found,
    /// none dropped, and every file visited. One implementation so the citation-corpus proof and
    /// the #789 full-workspace proof cannot silently diverge in what they actually check.
    fn assert_doc_span_scan_reaches_corpus(files: &[std::path::PathBuf], label: &str) {
        use std::collections::HashSet;
        use std::path::PathBuf;

        // One backtick: odd parity on its own line, which is what the scan reports.
        const PROBE: &str = "    /// #788 reach probe: this code span opens `and never closes";

        let corpus = read_corpus(files);
        let baseline = scan_doc_spans(&corpus);

        let mut mutated: Vec<(PathBuf, String)> = Vec::new();
        let mut expected: Vec<(PathBuf, ProbeDepth, usize)> = Vec::new();
        for (p, src) in &corpus {
            let safe = fence_safe_insertion_points(src);
            assert!(safe.len() >= 3,
                "{}: only {} fence-safe insertion point(s) — this file cannot carry a three-depth \
                 probe and the control would be weaker than it claims",
                p.display(), safe.len());
            let at = |d: ProbeDepth| match d {
                ProbeDepth::Top => safe[0],
                ProbeDepth::Mid => safe[safe.len() / 2],
                ProbeDepth::End => safe[safe.len() - 1],
            };
            let mut idx: Vec<(ProbeDepth, usize)> =
                PROBE_DEPTHS.iter().map(|d| (*d, at(*d))).collect();
            idx.sort_by_key(|(_, i)| *i);
            assert!(idx[0].1 < idx[1].1 && idx[1].1 < idx[2].1,
                "{}: two probe depths landed on the same line ({:?}) — the depths would not be \
                 distinct and the control would prove less than it claims",
                p.display(), idx);
            let mut lines: Vec<&str> = src.lines().collect();
            // Insert from the back so the earlier indices stay valid while inserting.
            for (_, i) in idx.iter().rev() { lines.insert(*i, PROBE); }
            mutated.push((p.clone(), lines.join("\n")));
            // After all three insertions the probe of rank `n` sits at 0-based `i + n`, so its
            // 1-based line number is `i + 1 + n`.
            for (n, (d, i)) in idx.iter().enumerate() {
                expected.push((p.clone(), *d, i + 1 + n));
            }
        }

        let scan = scan_doc_spans(&mutated);
        assert_eq!(scan.files_scanned, corpus.len(),
            "[{label}] the scan visited {} of {} files — it stopped between files",
            scan.files_scanned, corpus.len());

        let found: HashSet<(PathBuf, usize)> = scan.offenders.iter().cloned().collect();
        let missed: Vec<String> = expected.iter()
            .filter(|(p, _, line)| !found.contains(&(p.clone(), *line)))
            .map(|(p, d, line)| format!("{}:{line} ({d:?})", p.display()))
            .collect();
        assert!(missed.is_empty(),
            "[{label}] the doc-span scan never arrived at {} of {} probes — it matches the shape \
             but does not reach the whole corpus:\n  {}",
            missed.len(), expected.len(), missed.join("\n  "));

        assert_eq!(scan.offenders.len(), baseline.offenders.len() + expected.len(),
            "[{label}] mutated scan reported {} offender(s); expected the {} baseline offender(s) \
             plus {} probes. A count that is short means the scan dropped findings it had already \
             made; a count that is over means the probe insertion perturbed something it should not \
             have",
            scan.offenders.len(), baseline.offenders.len(), expected.len());

        println!("[{label}] reach control: {} probes ({} files x {} depths) all reported; \
                  baseline {} offender(s) over {} files / {} lines",
            expected.len(), corpus.len(), PROBE_DEPTHS.len(),
            baseline.offenders.len(), baseline.files_scanned, baseline.lines_scanned);
    }

    /// The `NAME` in a `fn NAME` declaration on this line, if any. Deliberately dumb — it is a
    /// lexical scan, not a parser, and both callers only need it to be right about ordinary
    /// declarations. `&[fn()]` and `fn(f32) -> bool` do not match (no space after `fn`).
    fn fn_name_on(line: &str) -> Option<String> {
        let mut rest = line;
        loop {
            let i = rest.find("fn ")?;
            let before_ok = i == 0 || !rest[..i].ends_with(|c: char| c.is_alphanumeric() || c == '_');
            let tail = &rest[i + 3..];
            let name: String = tail.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            if before_ok && !name.is_empty() && name.starts_with(|c: char| c.is_ascii_lowercase()) {
                return Some(name);
            }
            rest = &rest[i + 3..];
        }
    }

    /// Every backticked lower-snake identifier of four or more words appearing in a doc comment,
    /// with its 1-based line number.
    ///
    /// `::`-qualified paths are included (round 7). They were excluded until the round-6 review
    /// found `zone_assets::no_interleaving_of_the_two_writers_yields_a_usable_wrong_zone` cited in
    /// `walker.rs` and unseen by this scan, because `:` failed the charset filter. There are eight
    /// such citations in the corpus and every one of them now goes through the same resolution as an
    /// unqualified name — see `resolution_name`.
    fn doc_citations(src: &str) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        for (i, line) in src.lines().enumerate() {
            let t = line.trim_start();
            if !(t.starts_with("///") || t.starts_with("//!")) { continue; }
            for chunk in t.split('`').skip(1).step_by(2) {
                if chunk.len() > 2
                    && chunk.starts_with(|c: char| c.is_ascii_lowercase())
                    && chunk.chars().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == ':'
                    })
                    && chunk.matches('_').count() >= 3
                    && !chunk.ends_with(':')
                {
                    out.push((chunk.to_string(), i + 1));
                }
            }
        }
        out
    }

    /// The name a citation is RESOLVED under: the tail after the last `::`. `walker.rs` cites tests
    /// in `collision`, `steering`, `zone_assets` and `eqoxide-net`; the module prefix is not
    /// checked, only that the final identifier exists — see this scan's "What it does NOT do".
    fn resolution_name(citation: &str) -> &str {
        citation.rsplit("::").next().unwrap_or(citation)
    }

    /// A backtick-PARITY heuristic: every doc-comment line outside a triple-backtick fence whose
    /// backtick count is odd. Round 7: this is what caught the round-6 review's surviving
    /// citation, once the hand-wrap made the opening line's count odd.
    ///
    /// **PARITY IS NOT "a code span that opens on one line and closes on another".** That
    /// equivalence is false in BOTH directions, demonstrated by execution, and three review rounds
    /// running wrote an absolute here that turned out false — so this doc states what the check does
    /// and stops:
    ///
    /// * **FALSE FAILURE.** Escaping a literal backtick in prose (the standard CommonMark
    ///   double-backtick escape) puts an odd count on a line with nothing crossing anything. This
    ///   check hard-fails it, reporting a defect that is not there.
    /// * **FALSE PASS.** A citation genuinely wrapped across two lines, padded with the same escape
    ///   so each line's count comes out even, passes silently — the exact shape it exists to catch.
    ///
    /// Parity implies a crossing span only when nothing else on the line contributes an odd count of
    /// its own. No claim is made here about how this check's catch set relates to `doc_citations`':
    /// one such comparison was written and falsified. (#764, disclosed rather than rewritten:
    /// #727's PR body and two of its review comments carry an earlier version of that
    /// falsified-claims list which omits one item and double-counts another. Merged history is left
    /// as merged, so an external artefact still carries the superseded list.) What is measured: a
    /// wrapped citation's truncated leading fragment IS visible to `doc_citations` — it reads each
    /// line's chunks independently and `:` is in the charset — **when the opening line's own count
    /// is odd**, which is exactly the case the padding above defeats.
    ///
    /// The fence test runs on the line with blockquote markers stripped; see
    /// `strip_blockquote_markers` for the live case that forced it.
    fn unbalanced_doc_spans(src: &str) -> Vec<usize> {
        let mut out = Vec::new();
        let mut in_fence = false;
        for (i, line) in src.lines().enumerate() {
            let t = line.trim_start();
            let Some(body) = t.strip_prefix("///").or_else(|| t.strip_prefix("//!")) else {
                // A fence cannot span a gap in the doc comment; a stray unterminated one must not
                // silence every later line in the file.
                in_fence = false;
                continue;
            };
            if strip_blockquote_markers(body).starts_with("```") { in_fence = !in_fence; continue; }
            if in_fence { continue; }
            if body.matches('`').count() % 2 == 1 { out.push(i + 1); }
        }
        out
    }

    /// Every identifier named inside a `let _cited: &[fn()] = &[ … ];` or `let _helpers … = ( … );`
    /// block in this source — i.e. the set a rename would break the build on.
    ///
    /// **A `//` comment tail does not count (#911).** Every line inside the block is stripped to
    /// its code prefix before tokenising, so a commented-out entry — `// some_test_name,` — reads
    /// as blank, not as a live guard. Without this, commenting out an array line while leaving the
    /// same name quoted in the comment satisfied the guard exactly as well as a real entry: the
    /// citation test stayed green while nothing in the file any longer forced a rename of that test
    /// to be a compile error. A naive `line.split("//").next()` is exact here — unlike the
    /// `strip_comments` in `zone_assets.rs` / `transport.rs`, which must additionally protect a
    /// `//` living inside a string or char literal — because this block's content is restricted by
    /// construction to bare identifiers, commas and whitespace; no citation guard entry is ever a
    /// string or char literal, so there is nothing here for a naive split to cut through by mistake.
    fn guard_entries(src: &str) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        let mut depth: Option<&str> = None;
        for line in src.lines() {
            let t = line.trim();
            if depth.is_none() {
                if t.starts_with("let _cited") { depth = Some("];"); continue; }
                if t.starts_with("let _helpers") { depth = Some(");"); }
                else { continue; }
            }
            let end = depth.unwrap();
            let code = line.split("//").next().unwrap_or(line);
            let mut cur = String::new();
            for c in code.chars() {
                if c.is_ascii_alphanumeric() || c == '_' { cur.push(c); }
                else { if cur.len() > 2 { out.insert(std::mem::take(&mut cur)); } else { cur.clear(); } }
            }
            if cur.len() > 2 { out.insert(cur); }
            if t.ends_with(end) { depth = None; }
        }
        out
    }

    /// **A distance TIE resolves to the EARLIER segment, and that is the conservative half of the
    /// tie (#727 round-4 review, non-blocking 1).**
    ///
    /// The candidate loop's test is `d_sq < best_sq`, strictly. Mutate it to `<=` and the *latest*
    /// equidistant admissible segment wins instead, so the cursor jumps further forward on a tie —
    /// and the round-4 review measured that mutation surviving the whole suite (196 passed).
    /// Unpinned is the wrong state for this token in particular: round-2's finding B turned entirely
    /// on the strictness of this comparison (a candidate 30 u from the body was refused by the TIE,
    /// not by the reach band, which is what made a test look like it pinned
    /// `CURSOR_RESYNC_MAX_HOP` when it did not).
    ///
    /// Fixture: two parallel bars at y = ±6 either side of the body, both **exactly** 6.0 u away
    /// (the closest points are `[0, 6]` and `[0, -6]`, computed at t = 0.5 with no rounding, so the
    /// tie is exact in f32 and not a near-miss). Segment 0 is 10 u away, so the body is stale and
    /// both bars improve on it; both are inside `CURSOR_RESYNC_MAX_HOP`. The earlier bar is
    /// segment 2, the later is segment 4.
    ///
    /// Forward-only means the cursor may only ADVANCE, so on a tie the smallest advance is the one
    /// that claims the least. `resync_cursor` makes no walkability claim (see its rustdoc), so when
    /// two segments are equally good evidence, taking the nearer-to-current one is the reading that
    /// asserts less about where the character has been.
    #[test]
    fn a_distance_tie_between_two_admissible_segments_resolves_to_the_earlier_one() {
        let bars = [
            [-20.0f32, 10.0, 0.0], [20.0, 10.0, 0.0],   // seg 0: 10 u from the body
            [20.0, 6.0, 0.0],                            // seg 1: the east connector
            [-20.0, 6.0, 0.0],                           // seg 2: the +6 bar  ← must win
            [-20.0, -6.0, 0.0],                          // seg 3: the west connector
            [20.0, -6.0, 0.0],                           // seg 4: the -6 bar  (ties with seg 2)
            [20.0, -10.0, 0.0], [-20.0, -10.0, 0.0],
        ];
        let body = [0.0f32, 0.0, 0.0];
        // Premise: the two candidates really are EXACTLY equidistant, or this pins nothing.
        let (_, d2) = seg_closest(bars[2], bars[3], body);
        let (_, d4) = seg_closest(bars[4], bars[5], body);
        assert_eq!(d2, d4, "fixture no longer produces an exact tie ({d2} vs {d4})");
        // Premise: both are admissible — stale (seg 0 is 10 u away) and inside the reach band.
        let (_, d0) = seg_closest(bars[0], bars[1], body);
        assert!(d0 > CURSOR_STALE_DIST * CURSOR_STALE_DIST, "fixture is not stale (d0² = {d0})");
        assert!(d2 <= CURSOR_RESYNC_MAX_HOP * CURSOR_RESYNC_MAX_HOP,
            "fixture's tied candidates are outside the reach band and would be refused by it");

        assert_eq!(resync_cursor(&bars, 0, body, always_clear), 2,
            "a tie must resolve to the EARLIER segment — the smallest forward jump the evidence \
             supports. Flip `d_sq < best_sq` to `<=` and this goes to 4.");
    }

    /// **Guard 1 — forward only.** A character that has fallen BACKWARDS onto an earlier part of the
    /// route must not have its cursor rewound: rewinding would un-count progress the walker has
    /// genuinely made and hand the lap detectors (#631/#309) a false premise.
    #[test]
    fn resync_never_moves_the_cursor_backwards() {
        // Standing on segment 0 while the cursor has advanced to 6 — 30 u away from segment 6.
        let back = [-538.0f32, 160.375, 0.0];
        assert_eq!(resync_cursor(&HAIRPIN, 6, back, always_clear), 6);
    }

    /// **Guard 2 — never across geometry.** With everything blocked the cursor must stay put: a
    /// resync must never declare a leg of the route walked that the character could not reach.
    #[test]
    fn resync_never_jumps_across_blocked_geometry() {
        assert_eq!(resync_cursor(&HAIRPIN, STALE_I, LANDED, |_, _| false), STALE_I);
    }

    /// **THE property test.** For a dense sweep of positions and every starting cursor, the returned
    /// index must be (i) a valid forward segment start, (ii) never FURTHER from the body than the
    /// cursor it was handed, and (iii) once the cursor is stale, no admissible forward segment
    /// (inside [`CURSOR_RESYNC_MAX_HOP`], predicate clear) may be strictly nearer than the one
    /// chosen. Properties (ii) and (iii) are stated as OUTCOMES and checked against an independent
    /// scan — not by re-implementing the function.
    ///
    /// Assertions (i) alone (`i >= start_i`, `i + 1 < len`) are **true of the identity function**,
    /// so they pin nothing about the fix; they are kept because they are correct, with (ii)/(iii)
    /// added and a premise counter so the sweep cannot go green by never exercising a resync.
    ///
    /// **Mutation-checked by execution (#727 round 2; count re-measured round 7):** with
    /// `resync_cursor` replaced by the identity function this test now FAILS (it panics on the
    /// premise counter at `moved = 0`), together with **11 others — 12 in total**. The tests that
    /// still pass under that mutant are exactly
    /// the ones whose assertion IS "the cursor does not move":
    /// `resync_never_moves_the_cursor_backwards`,
    /// `resync_never_jumps_across_blocked_geometry`,
    /// `resync_is_inert_on_degenerate_paths`,
    /// `an_on_route_walker_is_left_alone_without_consulting_geometry`,
    /// `a_walker_cutting_a_tight_switchback_keeps_its_cursor`,
    /// `walker`'s `a_resync_must_not_cross_a_wall_and_it_uses_the_real_clearance`.
    /// An identity mutant satisfying a "must not move" test is not a gap in the test; the movement
    /// claims are the ones that had to be pinned, and are. (One identifier, one line: hand-wrapping
    /// a name inside its backticks makes `cargo doc` render a space in the middle and elide it to
    /// `...` — a citation that can neither be grepped nor checked.)
    ///
    /// **The 12 is measured at `4cc217f`** — `test result: FAILED. 191 passed; 12 failed` — and was
    /// written as 9, then 11, before anyone re-ran it. **An enumerated count is a measurement with
    /// an expiry date:** every test added anywhere near the mutated function invalidates it, which
    /// is precisely the kind of change nobody thinks of as touching a doc. If you add a test here,
    /// re-run the mutant or delete the count; do not update it by reasoning.
    #[test]
    fn resync_always_returns_the_nearest_admissible_forward_segment() {
        let d = |p: [f32; 3], i: usize| seg_closest(HAIRPIN[i], HAIRPIN[i + 1], p).1.sqrt();
        let (mut swept, mut moved) = (0usize, 0usize);
        for start_i in 0..HAIRPIN.len() - 1 {
            let mut x = -570.0f32;
            while x <= -510.0 {
                let mut y = 130.0f32;
                while y <= 175.0 {
                    for z in [-20.0f32, -6.0, 0.0, 12.0] {
                        let p = [x, y, z];
                        let i = resync_cursor(&HAIRPIN, start_i, p, always_clear);
                        swept += 1;
                        // (i) — retained from round 1.
                        assert!(i >= start_i, "moved backwards: {start_i} -> {i}");
                        assert!(i + 1 < HAIRPIN.len(), "cursor {i} is not a segment start");
                        if start_i + 2 >= HAIRPIN.len() { continue; }
                        let d_start = d(p, start_i);
                        let d_got = d(p, i);
                        // (ii) a resync may never leave the body further from its own segment.
                        assert!(d_got <= d_start + 1e-3,
                            "resync made the cursor WORSE at {p:?}: {start_i}({d_start:.2}u) -> {i}({d_got:.2}u)");
                        if i != start_i { moved += 1; }
                        // (iii) with a stale cursor, nothing admissible is nearer than what we took.
                        if d_start >= CURSOR_STALE_DIST {
                            for j in (start_i + 1)..(HAIRPIN.len() - 1) {
                                let dj = d(p, j);
                                if dj <= CURSOR_RESYNC_MAX_HOP {
                                    assert!(d_got <= dj + 1e-3,
                                        "left a nearer admissible segment on the table at {p:?}: took \
                                         {i} ({d_got:.2}u), segment {j} was {dj:.2}u");
                                }
                            }
                        }
                    }
                    y += 3.0;
                }
                x += 3.0;
            }
        }
        // Premise: the sweep must actually exercise the resync, or (ii)/(iii) are vacuous.
        assert!(moved > 200, "premise: the sweep resynced only {moved} of {swept} positions");
    }

    /// **Guard 2 — the reach band.** A segment the predicate says is perfectly clear is still refused
    /// when it is further than [`CURSOR_RESYNC_MAX_HOP`] away, because "the segment the character is
    /// actually on" cannot mean one 30 u distant. Here the body is 30 u off segment 0 and 30 u from
    /// the parallel return leg, with an always-clear predicate: the cursor must not move.
    #[test]
    fn resync_refuses_a_segment_beyond_the_reach_band() {
        // n = 59, not 60, and that is load-bearing. At 60 the body sits 30 u from segment 0 *and*
        // 30 u from segment 2, so segment 2 is refused by the strict `d_sq < best_sq` tie rather
        // than by the band — measured: deleting `&& d_sq <= hop_sq` SURVIVED this test. At 59
        // segment 2 is strictly nearer and would be adopted on proximity alone, so the band is the
        // only thing that can refuse it.
        let far = [[0.0f32, 0.0, 0.0], [80.0, 0.0, 0.0], [80.0, 59.0, 0.0], [0.0, 59.0, 0.0]];
        let body = [40.0f32, 30.0, 0.0];
        let d = |i: usize, path: &[[f32; 3]]| seg_closest(path[i], path[i + 1], body).1.sqrt();
        assert!(d(2, &far) < d(0, &far),
            "premise: the far segment must WIN on proximity ({:.1} u vs {:.1} u), or this test passes \
             for the wrong reason", d(2, &far), d(0, &far));
        assert!(CURSOR_RESYNC_MAX_HOP < d(2, &far),
            "premise: the far segment must sit outside the band ({:.1} u)", d(2, &far));
        assert_eq!(resync_cursor(&far, 0, body, always_clear), 0,
            "a segment beyond the reach band must never be adopted, however clear the line to it");
        // …and the same fixture DOES resync once the far leg is brought inside the band.
        let near = [[0.0f32, 0.0, 0.0], [80.0, 0.0, 0.0], [80.0, 40.0, 0.0], [0.0, 40.0, 0.0]];
        assert_eq!(resync_cursor(&near, 0, body, always_clear), 2,
            "premise: with the leg inside the band the same geometry must resync");
    }

    /// Degenerate inputs must be inert, not panic: an empty path, a single waypoint, and a cursor
    /// already parked on the last segment.
    #[test]
    fn resync_is_inert_on_degenerate_paths() {
        assert_eq!(resync_cursor(&[], 0, LANDED, always_clear), 0);
        assert_eq!(resync_cursor(&[[0.0, 0.0, 0.0]], 0, LANDED, always_clear), 0);
        assert_eq!(resync_cursor(&HAIRPIN, HAIRPIN.len() - 2, LANDED, always_clear), HAIRPIN.len() - 2);
        assert_eq!(resync_cursor(&HAIRPIN, 99, LANDED, always_clear), 99);
    }

    // ──────────────── #727 round 2: the deadlock sim, and where the fix stops ────────────────

    /// An 8 u-spaced hairpin: out along `y = 0` from `x = 0` to `x = 80`, back along `y = sep`.
    /// 8 u is the coarse planner's own cell, so this is the shape a real switchback route takes.
    fn hairpin_route(sep: f32) -> Vec<[f32; 3]> {
        let mut p: Vec<[f32; 3]> = (0..=10).map(|k| [k as f32 * 8.0, 0.0, 0.0]).collect();
        p.extend((0..=10).rev().map(|k| [k as f32 * 8.0, sep, 0.0]));
        p
    }

    /// Drive the PRODUCTION cursor arithmetic for 400 nav ticks and report whether the cursor/carrot
    /// loop reaches a fixed point instead of running the route out. Each tick: the walker's monotone
    /// advance (verbatim), then [`resync_cursor`], then [`carrot_along`] at `LOCAL_REACH`, then one
    /// 150 ms step at `RUN_SPEED` straight at that point.
    ///
    /// **Read the name literally: this is NOT the walker's motion.** `drive_walk` steers with
    /// `LOOK_AHEAD = 5.0`; 24 u is only the goal it hands the fine planner, so a body that steps
    /// straight at `local_goal` moves like nothing in production and NO ARRIVAL CLAIM may be
    /// measured this way. What this loop does measure, soundly, is whether the CURSOR/CARROT
    /// arithmetic can reach a fixed point — pure function composition, independent of how the body
    /// is propelled. The tables below are kept for that and only that. Arrival is measured instead by
    /// `the_stale_cursor_leaves_the_steering_loop_no_escaping_trajectory_and_the_resync_clears_it`,
    /// which drives the production [`steer_target`] and [`fast_steer_aim`] at `LOOK_AHEAD` (and which
    /// carries its own disclosure that it models the steering loop, not the whole walker).
    ///
    /// Deliberately NOT a physics sim: no collision, no controller, so the reachability predicate is
    /// vacuously clear. That isolates the one thing under test.
    ///
    /// **#733: it also reports a PREMISE counter.** `collapse_only_ticks` counts nav ticks on which
    /// the carrot was collapsed *while the body was inside* [`CURSOR_STALE_DIST`] of the segment the
    /// cursor names — i.e. ticks in the region the #727 distance trigger is structurally incapable of
    /// catching. A zero there means the sweep never reached the defect and its green rows are
    /// vacuous, so the sub-guard rows assert it is non-zero.
    ///
    /// **What this counter does NOT prove, measured (#733 review).** It is NOT a reach control for
    /// the new trigger: it calls [`carrot_leads`] itself, so it observes the PREDICATE, never
    /// whether [`resync_cursor`]'s gate consults it. Wrapping the gate's *call site* — leaving
    /// `carrot_leads` intact, so the guard is genuinely dead while the counter still reads a live
    /// predicate — does not zero this counter. **It raises it, on every row, 7 of 7.** On the three
    /// sub-guard rows the body stays parked in the collapsed state for all 400 ticks, so the count
    /// goes up ~286×/351×/395× (4 u: 144 → 41141; 6 u: 216 → 75818; 7 u: 252 → 99469); on the four
    /// rows at or above the guard the rise is small — 1.8×/1.7×/1.5×/1.2×. (So "two to three orders
    /// of magnitude" is true of 3 rows and false of 4.) A reader who took a non-zero value as
    /// evidence the guard is live would be reading a confident falsehood; the run tables are in
    /// #818. Wrapping the *body* of `carrot_leads` does zero it — but tautologically, since
    /// predicate and counter are then the same call.
    ///
    /// **The generalizable rule, written down because this cost a round: mutate at the CALL SITE,
    /// not inside the function body.** A body-wrap cannot tell "the guard is dead" from "the
    /// predicate is false" — it forces both at once, so whatever it shows is unattributable. And
    /// more generally: *any instrument that shares a code path with the thing it certifies is
    /// measuring itself.* Backing the retracted sentence would need a counter incremented from
    /// inside `resync_cursor` on the branch actually taken, not recomputed out here.
    ///
    /// What DOES catch a dead guard is the three tests that go red under the call-site wrap:
    /// [`after_a_resync_with_clear_geometry_the_carrot_always_leads`],
    /// [`the_resync_clears_the_carrot_pinning_at_every_leg_separation_measured`] and
    /// [`the_sub_guard_hairpin_fixed_point_resyncs_though_the_distance_trigger_cannot_see_it`].
    struct HairpinRun {
        /// The carrot never led the body out: 400 nav ticks without reaching the goal.
        pinned: bool,
        /// Ticks where `carrot_leads` was false AND the body was within `CURSOR_STALE_DIST` of the
        /// segment `cursor` names — the region only the #733 trigger can act in. A NON-VACUITY
        /// measure of the fixture, not a liveness measure of the guard: a nonzero count says the
        /// fixture reached the region the guard acts in, and says nothing about whether it acted.
        collapse_only_ticks: u32,
    }

    fn hairpin_carrot_stops_leading(sep: f32, start: [f32; 3], mut cursor: usize) -> HairpinRun {
        const DT: f32 = 0.15;          // the nav tick
        // `LOCAL_REACH` below is the module constant via `use super::*` — `drive_walk`'s
        // `local_goal` reach, the fine planner's destination, NOT a steering carrot. Not restated,
        // not aliased, so this sweep and the collapse check judge the same carrot by construction.
        let route = hairpin_route(sep);
        let goal = *route.last().unwrap();
        let mut p = start;
        let mut collapse_only_ticks = 0u32;
        for _ in 0..400 {
            while cursor + 2 < route.len() {
                let (a, b) = (route[cursor], route[cursor + 1]);
                let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
                let t = if l2 < 1e-6 { 1.0 } else {
                    ((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1] + (p[2] - a[2]) * ab[2]) / l2
                };
                if t >= 1.0 { cursor += 1; } else { break; }
            }
            // Observe, before the resync, whether this tick is one only #733's trigger can see.
            if cursor + 1 < route.len() {
                let (_, d0_sq) = seg_closest(route[cursor], route[cursor + 1], p);
                if d0_sq < CURSOR_STALE_DIST * CURSOR_STALE_DIST
                    && !carrot_leads(&route, cursor, p, LOCAL_REACH)
                {
                    collapse_only_ticks += 1;
                }
            }
            cursor = resync_cursor(&route, cursor, p, always_clear);
            let carrot = carrot_along(&route, cursor, p, LOCAL_REACH).unwrap_or(goal);
            let d = [carrot[0] - p[0], carrot[1] - p[1]];
            let len = (d[0] * d[0] + d[1] * d[1]).sqrt();
            if len > 1e-4 {
                let step = (eqoxide_core::physics::RUN_SPEED * DT).min(len);
                p[0] += d[0] / len * step;
                p[1] += d[1] / len * step;
            }
            if (p[0] - goal[0]).hypot(p[1] - goal[1]) <= 4.0 {
                return HairpinRun { pinned: false, collapse_only_ticks };
            }
        }
        HairpinRun { pinned: true, collapse_only_ticks }
    }

    /// **MEASURED (#727 round 2): the deadlock's attracting fixed point sits exactly ON the
    /// [`CURSOR_STALE_DIST`] boundary, and `<=` leaves it inside the guard.**
    ///
    /// On the 8 u hairpin the loop converges to a body offset of exactly 8.0 u with the cursor
    /// pinned, and stays there for the whole run. Swept starts 6.25 … 8.00 u off the outbound leg,
    /// cursor pinned at the waypoint the body is abreast of:
    ///
    /// * with `d0_sq <= CURSOR_STALE_DIST²` (the round-1 code): **1 of 8 pinned** (the 7.00 u start,
    ///   which converges onto the boundary and is then held there by the `<=`)
    /// * with `d0_sq <  CURSOR_STALE_DIST²` (this branch): **0 of 8**
    ///
    /// Both numbers were measured in round 2 by flipping that single token and re-running; the same
    /// flip took the 8 u column of
    /// [`the_resync_clears_the_carrot_pinning_at_every_leg_separation_measured`] from 0/288 to
    /// 33/288.
    ///
    /// These counts are of *carrot pinning*, not of a walker failing to arrive — see
    /// [`hairpin_carrot_stops_leading`].
    ///
    /// **The `<` token has NO live mutation check, measured (#733).** *"Flip it back to `<=` and
    /// this test goes RED"* was true while the distance trigger was the only trigger. It is now
    /// false, and the mutation was run rather than reasoned about: with
    /// `d0_sq <= CURSOR_STALE_DIST²` on this branch the whole crate is still green
    /// (`223 passed; 0 failed`) and every row of the sweep still reads 0 pinned, 8 u included. The
    /// reason is not that the `<` finding was wrong — it was a real round-2 measurement, and the
    /// counts above stand as a measurement OF THE DISTANCE TRIGGER ALONE. It is that #733's second
    /// trigger catches that same fixed point: the carrot at the 8 u attractor is collapsed, so the
    /// resync now fires on the collapse whichever way the distance comparison rounds.
    ///
    /// **Consequence, stated rather than hidden: `<` is now a surviving mutant.** No test in this
    /// crate distinguishes `<` from `<=` any more. The token is kept because the round-2 reasoning
    /// for it is unchanged — a fixed point exactly ON the boundary should be outside a guard that
    /// means "close enough to leave alone" — but this doc no longer claims a test enforces it, and
    /// a future reader must not infer one from the counts above.
    #[test]
    fn the_deadlock_fixed_point_exactly_on_the_guard_boundary_is_resynced() {
        let mut pinned = Vec::new();
        for k in 0..8 {
            let y = 6.25 + 0.25 * k as f32;
            if hairpin_carrot_stops_leading(8.0, [40.0, y, 0.0], 5).pinned { pinned.push(y); }
        }
        assert!(pinned.is_empty(),
            "the guard-boundary band still deadlocks at offsets {pinned:?} — the fixed point at \
             exactly CURSOR_STALE_DIST must be OUTSIDE the guard");
    }

    /// **MEASURED (#733): carrot pinning is cleared at EVERY leg separation this sweep measures,
    /// including the ones below [`CURSOR_STALE_DIST`] where the distance trigger is inert.**
    ///
    /// Same sweep, same fixture, same counts as #727's — only the code under it changed. The `before`
    /// column is this branch with the `carrot_leads` conjunct deleted from [`resync_cursor`]'s gate
    /// (executed, not reasoned: that is mutation M1 in the PR body):
    ///
    /// ```text
    /// sep    before (#727: distance trigger only)   after (#733: + carrot collapse)
    ///  4 u   144/144 carrot-pinned                  0/144
    ///  6 u   216/216 carrot-pinned                  0/216
    ///  7 u   252/252 carrot-pinned                  0/252
    ///  8 u     0/288                                0/288
    ///  9 u     0/324                                0/324
    /// 10 u     0/360                                0/360
    /// 12 u     0/432                                0/432
    /// ```
    ///
    /// Grepping for the retired name: this was
    /// `the_resync_clears_the_carrot_pinning_above_the_guard_and_is_inert_below_it`, retired because
    /// "inert below the guard" became false of the function when #733 added the collapse trigger.
    ///
    /// **THE LIMIT: these are counts of CARROT PINNING, not walker outcomes.**
    /// [`hairpin_carrot_stops_leading`] steps the body straight at `local_goal`, which is not how
    /// `drive_walk` moves, so the loop measures whether the cursor/carrot arithmetic reaches a fixed
    /// point (pure function composition) and nothing about stall detection, backoff or re-planning.
    /// #733's statement of the cost stands: at least a wasted backoff-and-re-plan lap, at worst
    /// terminal — not "always wedges".
    ///
    /// **The premise counter is a PREMISE control, not a reach control.** Each sub-guard separation
    /// must record at least one tick on which the carrot was collapsed *while the body was inside*
    /// `CURSOR_STALE_DIST`, so a green row cannot be vacuous. It does **not** establish that the
    /// gate consults `carrot_leads` — see the measured falsification on
    /// [`hairpin_carrot_stops_leading`], where wrapping the gate's call site RAISES this counter on
    /// 7 of 7 rows. The `before` column above is what carries that, and it is a call-site mutation.
    #[test]
    fn the_resync_clears_the_carrot_pinning_at_every_leg_separation_measured() {
        let count = |sep: f32| {
            let (mut pinned, mut total, mut collapse_only) = (0usize, 0usize, 0u32);
            let mut yi = 1;
            while yi as f32 * 0.25 <= sep {
                let y = yi as f32 * 0.25;
                for xi in 1..10 {
                    let x = xi as f32 * 8.0;
                    total += 1;
                    let run = hairpin_carrot_stops_leading(sep, [x, y, 0.0], xi);
                    if run.pinned { pinned += 1; }
                    collapse_only += run.collapse_only_ticks;
                }
                yi += 1;
            }
            (pinned, total, collapse_only)
        };
        // EVERY row is measured and printed BEFORE anything is asserted. A per-row `assert` aborts
        // the sweep at the first failure and prints a one-row table, which is useless as the
        // `before` column of a comparison — the mutation run has to publish the same seven rows the
        // fixed run does or the table is not a comparison at all.
        let rows: Vec<(f32, usize, usize, u32)> =
            [4.0f32, 6.0, 7.0, 8.0, 9.0, 10.0, 12.0].into_iter()
                .map(|sep| { let (w, t, c) = count(sep); (sep, w, t, c) }).collect();
        for (sep, w, t, c) in &rows {
            println!(
                "hairpin leg separation {sep:>4} u: carrot pinned {w}/{t}  \
                 (collapse-only trigger ticks: {c})");
        }
        for (sep, w, t, c) in &rows {
            // "carrot-pinned", NOT "wedged" — see this test's THE LIMIT paragraph.
            assert_eq!(*w, 0,
                "carrot pinning must be cleared at {sep} u separation, got {w}/{t} carrot-pinned");
            if *sep < CURSOR_STALE_DIST {
                assert!(*c > 0,
                    "premise: at {sep} u separation the #733 trigger never fired inside the distance \
                     guard, so this row says nothing about it — the sweep is not reaching the defect");
            }
        }
    }

    // ──────────────── #733: the collapse measurement itself ────────────────

    /// A test-local, deliberately independent walk of the polyline: the point `s` units of 3-D
    /// arclength along `path[start_i..]`, clamped to the final vertex. Written out longhand here
    /// rather than shared with the code under test — a helper both sides call cannot disagree with
    /// itself, and disagreement is the entire point of the three tests below.
    fn point_at_arclength(path: &[[f32; 3]], start_i: usize, s: f32) -> [f32; 3] {
        let mut left = s.max(0.0);
        for i in start_i..path.len().saturating_sub(1) {
            let (a, b) = (path[i], path[i + 1]);
            let d = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
            if left <= l || i + 2 >= path.len() {
                let t = if l < 1e-9 { 0.0 } else { (left / l).clamp(0.0, 1.0) };
                return [a[0] + d[0] * t, a[1] + d[1] * t, a[2] + d[2] * t];
            }
            left -= l;
        }
        *path.last().unwrap()
    }

    /// The corpus the three #733 tests below sweep: the #673 hairpin at a sub-guard and an
    /// on-guard separation, the deliberate-conservatism switchback, a straight run, a 3-D ramp
    /// (the water-nav shape `carrot_along` is 3-D for), a zigzag, and a path with a **repeated
    /// vertex** so the zero-length-segment branch is exercised rather than argued about.
    fn arclength_fixtures() -> Vec<(&'static str, Vec<[f32; 3]>)> {
        vec![
            ("hairpin 4u", hairpin_route(4.0)),
            ("hairpin 8u", hairpin_route(8.0)),
            ("switchback", vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 2.0, 0.0], [0.0, 2.0, 0.0]]),
            ("straight",   vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [40.0, 0.0, 0.0], [60.0, 0.0, 0.0]]),
            ("3-D ramp",   vec![[0.0, 0.0, 0.0], [10.0, 0.0, 10.0], [20.0, 0.0, 0.0],
                                [30.0, 0.0, -10.0], [40.0, 0.0, 0.0]]),
            ("zigzag",     vec![[0.0, 0.0, 0.0], [10.0, 10.0, 0.0], [20.0, 0.0, 0.0],
                                [30.0, 10.0, 0.0], [40.0, 0.0, 0.0]]),
            ("dup vertex", vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 0.0, 0.0],
                                [10.0, 10.0, 0.0], [0.0, 10.0, 0.0]]),
        ]
    }

    /// A coarse body grid over and around each fixture, plus two z levels so the 3-D ramp is not
    /// swept only in its own plane.
    fn arclength_bodies() -> Vec<[f32; 3]> {
        let mut v = Vec::new();
        let mut x = -6.0f32;
        while x <= 88.0 {
            let mut y = -6.0f32;
            while y <= 12.0 {
                for z in [0.0f32, 6.0] { v.push([x, y, z]); }
                y += 3.0;
            }
            x += 6.0;
        }
        v
    }

    /// **The three arclengths [`cursor_arclengths`] returns are the points it says they are (#733).**
    ///
    /// The collapse check is one comparison between three numbers, so the numbers themselves are
    /// where a silent error would live. This checks each against its *definition*, using a test-local
    /// [`point_at_arclength`] and a resampling of the route, never against a restatement of the
    /// implementation:
    ///
    /// * `s_projection` — the point that far along must be the point [`seg_closest`] projects onto
    ///   segment `start_i`. That is exactly where [`carrot_along`] starts spending its budget.
    /// * `s_body` — the point that far along must be **at least as close** to the body as every
    ///   point sampled along the whole of `path[start_i..]` at 0.1 u spacing. Note the direction:
    ///   a sparse resampling can only ever fail to find something closer, so a wrong `s_body` is
    ///   caught while a coarse sample is never a false failure. Density buys strength, not validity.
    /// * `total` — the point that far along must be the route's final vertex.
    ///
    /// Plus the ordering the comparison relies on: `0 <= s_body <= total`.
    #[test]
    fn the_three_arclengths_are_the_points_they_claim_to_be() {
        let mut checked = 0usize;
        for (name, path) in arclength_fixtures() {
            for start_i in 0..path.len().saturating_sub(1) {
                for from in arclength_bodies() {
                    let Some((s_proj, s_near, total)) = cursor_arclengths(&path, start_i, from) else {
                        continue;
                    };
                    checked += 1;
                    let d = |a: [f32; 3], b: [f32; 3]| {
                        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
                    };
                    // s_projection is the projection onto segment `start_i`.
                    let (proj, _) = seg_closest(path[start_i], path[start_i + 1], from);
                    assert!(d(point_at_arclength(&path, start_i, s_proj), proj) < 1e-2,
                        "{name} start {start_i} body {from:?}: s_projection {s_proj} is not the \
                         projection onto the cursor's own segment");
                    // total ends at the last vertex.
                    assert!(d(point_at_arclength(&path, start_i, total), *path.last().unwrap()) < 1e-2,
                        "{name} start {start_i}: total {total} does not reach the final vertex");
                    assert!(s_near >= -1e-4 && s_near <= total + 1e-3,
                        "{name} start {start_i} body {from:?}: s_body {s_near} outside [0, {total}]");
                    // s_body beats every resampled point on the route ahead.
                    let claimed = d(point_at_arclength(&path, start_i, s_near), from);
                    let mut s = 0.0f32;
                    while s <= total {
                        let sampled = d(point_at_arclength(&path, start_i, s), from);
                        assert!(claimed <= sampled + 1e-3,
                            "{name} start {start_i} body {from:?}: s_body {s_near} is {claimed} away \
                             but arclength {s} is nearer at {sampled} — s_body is not the body's \
                             point on the route");
                        s += 0.1;
                    }
                }
            }
        }
        assert!(checked > 5_000, "premise: the sweep only measured {checked} cases");
    }

    /// **[`carrot_leads`] judges the carrot production actually builds (#733).**
    ///
    /// The collapse check is only meaningful if `min(s_projection + reach, total)` is *the arclength
    /// [`carrot_along`] lands at*. If the two ever drift the guard still passes — on the wrong
    /// carrot — which is the failure mode the shared [`LOCAL_REACH`] constant exists to prevent on
    /// the value side and this test pins on the arithmetic side. Swept at three reaches, including
    /// production's, and at reaches longer than some fixtures so the clamp-to-`total` branch is run.
    ///
    /// This is a **model-agreement** test, not a claim about [`carrot_along_los`]: the LOS clamp can
    /// only shorten the carrot, so under geometry the real carrot may sit *behind* the point checked
    /// here. `carrot_leads` therefore under-detects collapse in the presence of walls and never
    /// over-detects — stated on `carrot_leads` itself and not measured anywhere.
    #[test]
    fn carrot_leads_judges_the_carrot_the_production_code_actually_builds() {
        let mut checked = 0usize;
        for (name, path) in arclength_fixtures() {
            for start_i in 0..path.len().saturating_sub(1) {
                for from in arclength_bodies() {
                    let Some((s_proj, _, total)) = cursor_arclengths(&path, start_i, from) else {
                        continue;
                    };
                    for reach in [5.0f32, LOCAL_REACH, 400.0] {
                        let built = carrot_along(&path, start_i, from, reach)
                            .expect("carrot_along must produce a carrot wherever start_i names a segment");
                        let modelled = point_at_arclength(&path, start_i, (s_proj + reach).min(total));
                        let sep = ((built[0] - modelled[0]).powi(2) + (built[1] - modelled[1]).powi(2)
                                 + (built[2] - modelled[2]).powi(2)).sqrt();
                        assert!(sep < 1e-2,
                            "{name} start {start_i} body {from:?} reach {reach}: the carrot \
                             production builds is {built:?} but the collapse check models it at \
                             {modelled:?} ({sep} u apart) — the two have drifted");
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 5_000, "premise: the sweep only measured {checked} cases");
    }

    /// **PROPERTY (#733): after a resync with clear geometry the carrot leads the body — always,
    /// not on the hairpin.**
    ///
    /// "The carrot never lands on or behind the body" is a universal, so examples cannot discharge
    /// it. This sweeps every fixture × every cursor × a body grid on and around each route and
    /// asserts the property of the cursor [`resync_cursor`] RETURNS, not of the one it was given.
    ///
    /// **The three honest preconditions**, which are capability boundaries and not conveniences.
    /// (It said "two" until the #733 review counted them; the last one was in the code and not in
    /// the prose.)
    ///
    /// * there are at least two segments left from `start_i` — `resync_cursor`'s own first line
    ///   returns `start_i` untouched when `path.len() < 3 || start_i + 2 >= path.len()`, so on those
    ///   inputs the property would be asserting something about a function that declined to run.
    ///   [`carrot_leads_is_honest_at_the_route_end_and_where_it_can_measure_nothing`] covers that
    ///   edge directly instead;
    /// * the body's nearest point on `path[start_i..]` is inside [`CURSOR_RESYNC_MAX_HOP`] — a
    ///   resync refuses to adopt a segment further than that however clear the line, so a body 60 u
    ///   off its route is a case this function deliberately declines to fix (see that constant's own
    ///   doc for both reasons);
    /// * there is more than 1 u of route left beyond that nearest point — a body at the goal has no
    ///   carrot ahead of it by construction, which is arrival's business, not steering's.
    ///
    /// The predicate is always-clear, which is the honest scope: with real geometry a candidate can
    /// be refused as unreachable and the cursor left stale on purpose. That is `reachable` doing its
    /// job, and this property makes no claim there.
    ///
    /// **Non-vacuity is asserted, not assumed.** The sweep counts inputs that were genuinely
    /// collapsed before the resync, and separately those collapsed *while inside* the distance guard
    /// — the class #727's trigger structurally cannot see. Both must be non-zero or the property
    /// proves nothing about the fix.
    #[test]
    fn after_a_resync_with_clear_geometry_the_carrot_always_leads() {
        let (mut checked, mut collapsed_in, mut collapsed_inside_guard) = (0usize, 0usize, 0usize);
        for (name, path) in arclength_fixtures() {
            for start_i in 0..path.len() {
                for from in arclength_bodies() {
                    if path.len() < 3 || start_i + 2 >= path.len() { continue; }
                    // Nearest forward segment, by the same primitive `resync_cursor` scans with.
                    let mut near_sq = f32::INFINITY;
                    for i in start_i..(path.len() - 1) {
                        let (_, d_sq) = seg_closest(path[i], path[i + 1], from);
                        if d_sq < near_sq { near_sq = d_sq; }
                    }
                    if near_sq.sqrt() >= CURSOR_RESYNC_MAX_HOP - 0.01 { continue; }
                    let (_, s_near, total) = cursor_arclengths(&path, start_i, from).unwrap();
                    if total - s_near <= 1.0 { continue; }

                    checked += 1;
                    if !carrot_leads(&path, start_i, from, LOCAL_REACH) {
                        collapsed_in += 1;
                        let (_, d0_sq) = seg_closest(path[start_i], path[start_i + 1], from);
                        if d0_sq < CURSOR_STALE_DIST * CURSOR_STALE_DIST { collapsed_inside_guard += 1; }
                    }
                    let out = resync_cursor(&path, start_i, from, always_clear);
                    assert!(carrot_leads(&path, out, from, LOCAL_REACH),
                        "{name}: cursor {start_i} → {out} for a body at {from:?} still leaves the \
                         carrot at or behind the body");
                }
            }
        }
        assert!(checked > 2_000, "premise: the sweep only measured {checked} cases");
        assert!(collapsed_in > 100,
            "premise: only {collapsed_in} swept inputs were collapsed at all — the property is \
             passing on cases that were never broken");
        assert!(collapsed_inside_guard > 20,
            "premise: only {collapsed_inside_guard} swept inputs were collapsed while INSIDE the \
             distance guard, so this sweep is not reaching the class #733 adds");
    }

    /// **The two edges [`carrot_leads`]'s own doc claims, pinned so the claims are not free (#733).**
    ///
    /// Both were surviving mutants before this test existed, found by running the mutations rather
    /// than by reading the function:
    ///
    /// * **`>` is not `>=`.** A body sitting on the route's final vertex has `s_body == total`, and
    ///   the carrot is clamped to that same vertex, so the two arclengths are EQUAL. `>` calls that
    ///   "does not lead", which is the honest answer — there is no route ahead to lead onto — and it
    ///   is what the doc says. Relaxing to `>=` would have it report a carrot that leads a body it is
    ///   standing on, and nothing else in the suite noticed.
    /// * **an unmeasurable cursor is not a collapse.** `start_i` naming no segment yields `None`, and
    ///   `None` must read as "leads": declaring a collapse we cannot see would let the resync fire on
    ///   the strength of missing evidence. (`resync_cursor` never reaches that branch — it rejects
    ///   short paths first — so this is a claim about the predicate, for its other callers.)
    #[test]
    fn carrot_leads_is_honest_at_the_route_end_and_where_it_can_measure_nothing() {
        let route = hairpin_route(8.0);
        let end = *route.last().unwrap();
        assert!(!carrot_leads(&route, route.len() - 2, end, LOCAL_REACH),
            "a body ON the final vertex has no route ahead of it, so no carrot can lead it");
        // …and one step back from the end it does lead again, so the assert above is about the end
        // and not about the last segment being special.
        let stepped_back = [end[0] + 3.0, end[1], end[2]];
        assert!(carrot_leads(&route, route.len() - 2, stepped_back, LOCAL_REACH),
            "premise: a body short of the final vertex must still have a leading carrot");

        assert!(carrot_leads(&[], 0, end, LOCAL_REACH),
            "an empty path measures nothing; that is not evidence of a collapse");
        assert!(carrot_leads(&route, route.len() - 1, end, LOCAL_REACH),
            "a cursor past the last segment measures nothing; that is not evidence of a collapse");
        assert!(carrot_leads(&route, 999, end, LOCAL_REACH),
            "an out-of-range cursor measures nothing; that is not evidence of a collapse");
    }

    /// **REGRESSION (#733): the sub-guard hairpin's fixed point, as one arithmetic example.**
    ///
    /// The 4 u hairpin's limit cycle parks the body at `(56 + sep, sep)` with the cursor still on the
    /// outbound leg. Every number below is checked, so the example documents *why* the distance
    /// trigger is blind here rather than just asserting an index:
    ///
    /// * the body is 4 u from the segment the cursor names — half the [`CURSOR_STALE_DIST`] guard, so
    ///   #727's trigger is silent and cannot be made to fire by any constant that also leaves
    ///   `a_walker_cutting_a_tight_switchback_keeps_its_cursor` alone;
    /// * the carrot is spent from arclength 4 on the outbound leg and lands at 28, while the body's
    ///   own point on the route — on the return leg — is at 48. The carrot is **20 u behind the
    ///   body**, and no aim at it leaves the spot;
    /// * the resync moves the cursor onto the return-leg segment the body is actually on, after which
    ///   the carrot leads again.
    #[test]
    fn the_sub_guard_hairpin_fixed_point_resyncs_though_the_distance_trigger_cannot_see_it() {
        let route = hairpin_route(4.0);
        let body = [60.0f32, 4.0, 0.0];
        let (cursor, on_return_leg) = (7usize, 13usize);
        assert_eq!(route[cursor], [56.0, 0.0, 0.0], "premise: the cursor names the outbound leg");
        assert_eq!(route[on_return_leg], [64.0, 4.0, 0.0], "premise: segment 13 is the return leg");

        let (_, d0_sq) = seg_closest(route[cursor], route[cursor + 1], body);
        assert!(d0_sq < CURSOR_STALE_DIST * CURSOR_STALE_DIST,
            "premise: the whole cycle fits INSIDE the distance guard ({} u), which is why #727's \
             trigger is structurally blind to it", d0_sq.sqrt());

        let (s_proj, s_near, total) = cursor_arclengths(&route, cursor, body).unwrap();
        assert!((s_proj - 4.0).abs() < 1e-3 && (s_near - 48.0).abs() < 1e-3 && (total - 108.0).abs() < 1e-3,
            "premise: expected the fixed point's arclengths (4, 48, 108), got ({s_proj}, {s_near}, {total})");
        assert!(!carrot_leads(&route, cursor, body, LOCAL_REACH),
            "the carrot lands at {} and the body is at {s_near} — that is the collapse",
            (s_proj + LOCAL_REACH).min(total));

        assert_eq!(resync_cursor(&route, cursor, body, always_clear), on_return_leg,
            "the resync must move the cursor onto the leg the body is really on");
        assert!(carrot_leads(&route, on_return_leg, body, LOCAL_REACH),
            "and the carrot must lead again once it does");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **`/follow` a MOVING leader must actually get a route.** (#377 review, B1.)
    ///
    /// The chase block rewrites the goal with the leader's LIVE position every tick, so the old
    /// exact-compare `path_goal != Some(goal)` said "the goal changed" ~every tick. Each tick then
    /// posted a fresh plan — which SUPERSEDED the previous generation's reply before it could land —
    /// cleared the route, and stopped the walker. A `/follow` of a moving leader therefore never got
    /// a route at all and just stood there. Inline planning hid this completely: the walker always
    /// had a route the same tick it asked for one.
    ///
    /// Simulate the leader walking away at run speed while a plan is in flight, and assert the
    /// planner is allowed to FINISH: the tick must not re-post on every jitter.
    #[test]
    fn following_a_moving_leader_lets_the_plan_land() {
        // Leader at (100,0,0), a plan for it is in flight. It walks ~6.6u per tick (RUN_SPEED·0.15).
        let mut in_flight = Some((100.0, 0.0, 0.0));
        let mut planned   = Some((100.0, 0.0, 0.0));
        let mut leader    = (100.0f32, 0.0f32, 0.0f32);
        let mut posts = 0;
        for tick in 0..10 {
            leader.0 += 6.6; // the leader keeps walking
            let d = replan_decision(planned, leader, in_flight, false, true);
            assert!(!d.reset_route,
                "tick {tick}: a leader drifting a few units is the SAME goal — the committed route \
                 must not be thrown away, and the walker must not be stopped");
            if d.post {
                posts += 1;
                in_flight = Some(leader);
                planned = Some(leader);
            }
        }
        assert_eq!(posts, 0,
            "while a plan is IN FLIGHT for essentially this goal, the tick must NOT keep superseding \
             it — that is the /follow freeze: post, discard, post, discard, and no route ever lands");

        // Once the plan LANDS (nothing in flight), a leader that has since drifted past one nav cell
        // must be re-planned for — otherwise the chase would never update at all.
        let d = replan_decision(planned, (leader.0 + 20.0, 0.0, 0.0), None, false, true);
        assert!(d.post, "with no plan in flight, a leader that moved a cell+ must trigger a re-plan");
        assert!(!d.reset_route, "but 20u is a drift, not a new destination — keep walking the route");

        // A leader who RUNS AWAY is still the same goal: never throw the route away and freeze.
        let d = replan_decision(planned, (leader.0 + 500.0, 0.0, 0.0), None, false, true);
        assert!(d.post && !d.reset_route,
            "a fleeing leader is still the SAME goal — re-plan for it, but never drop the route and stop");

        // A genuinely NEW destination (a fresh one-shot /goto far away) DOES reset the route.
        let d = replan_decision(planned, (leader.0 + 500.0, 0.0, 0.0), in_flight, false, false);
        assert!(d.post && d.reset_route, "a far-away new goto supersedes the in-flight plan and resets the route");
    }

    /// **An in-zone portal escape (#266) may only be attempted for a goal a portal could actually
    /// help with.** Caught live: a `/goto` whose z put it off any floor came back
    /// `Unreachable(GoalNotWalkable)` — correctly — but the escape logic fired anyway, silently
    /// re-aimed the character at a translocator, and then reported `no_path: search_closed`, which
    /// was the PORTAL's verdict. The agent asked about goal X and was handed the reason for goal Y;
    /// the true reason (`goal_not_walkable`, the one that tells them to fix their coordinates) never
    /// reached them. Same family of lie as everything else this PR exists to kill.
    #[test]
    fn only_a_walled_off_goal_may_be_escaped_via_a_portal() {
        use crate::collision::NoRoute;
        // Walled off from a perfectly good goal, or boxed in ourselves → a teleport might genuinely
        // be the way out. That is what #266 is for.
        assert!(portal_escape_applies(NoRoute::SearchClosed), "a walled-off goal may be escaped to");
        assert!(portal_escape_applies(NoRoute::StartIsolated), "a boxed-in start may be escaped from");
        // No floor under the goal / no geometry at all: no teleport anywhere reaches a place that
        // does not exist. Redirecting is nonsense AND it buries the agent's real reason.
        assert!(!portal_escape_applies(NoRoute::GoalNotWalkable),
            "a goal with no walkable floor must NOT be redirected through a portal — the agent needs \
             `goal_not_walkable` (fix your coordinates), not the portal's `search_closed`");
        assert!(!portal_escape_applies(NoRoute::NoGeometry), "no collision loaded is not a portal problem");
    }

    /// **THE LIVENESS INVARIANT: no sequence of goals may leave the planner wedged.**
    ///
    /// This is the property, pinned directly rather than by example — because the bug it guards
    /// against was found by reasoning about the state machine, NOT by live play (live `/follow`
    /// passed by sheer luck: NPC position updates are sparse relative to the 150ms tick, so the
    /// reply happened to land while the leader was still).
    ///
    /// The deadlock: `poll()` consumed a reply and cleared `pending`, but a `plan_goal == goal`
    /// exact-compare in the tick DROPPED it — and `apply_plan` is the only thing that clears
    /// `plan_goal`. So `plan_goal` stayed `Some(stale)` forever, `replan_decision` refused to post
    /// while a plan was "in flight", and the character sat at `nav_state: planning` PERMANENTLY,
    /// with a live, idle worker that `is_dead()` could never flag.
    ///
    /// Models the real tick loop — including the ONE rule that fixes it: consuming a reply always
    /// clears `plan_goal` — and drives it with adversarial goal motion (jitter, cell-sized drift,
    /// mid-flight re-aims inside the reset threshold, teleports, standing still). Over the whole
    /// run the walker must never go blind for long: it must keep getting routes.
    #[test]
    fn no_goal_sequence_can_wedge_the_planner() {
        // Adversarial goal motion, including the exact sequence that deadlocked: re-aim 20u away
        // (> GOAL_REPLAN_DIST 8, < GOAL_RESET_DIST 40) BEFORE the in-flight plan lands.
        let moves: [f32; 12] = [0.0, 0.3, 20.0, 9.0, 1320.0, 0.0, 12.0, 39.0, 41.0, 0.5, 20.0, 200.0];
        for &is_chase in &[true, false] {
            for &replan_coarse in &[true, false] {
                let mut planned: Option<(f32, f32, f32)> = None;
                let mut in_flight: Option<(f32, f32, f32)> = None;
                let mut in_flight_age = 0;
                let mut goal = (0.0f32, 0.0, 0.0);
                let mut ticks_since_route = 0;

                for tick in 0..600 {
                    // The goal wanders adversarially.
                    goal.0 += moves[tick % moves.len()];

                    let d = replan_decision(planned, goal, in_flight, replan_coarse, is_chase);
                    if d.post {
                        in_flight = Some(goal);
                        planned = Some(goal);
                        in_flight_age = 0;
                    }
                    // The worker answers after a couple of ticks. Consuming the reply ALWAYS clears
                    // the in-flight goal — that is the invariant the deadlock violated.
                    if in_flight.is_some() {
                        in_flight_age += 1;
                        if in_flight_age >= 2 {
                            in_flight = None;      // reply consumed -> plan_goal cleared, ALWAYS
                            ticks_since_route = 0; // and the walker got a route
                        }
                    }
                    ticks_since_route += 1;

                    assert!(ticks_since_route < 60, // ~9 s at 150ms/tick: far beyond any real plan
                        "DEADLOCK at tick {tick} (chase={is_chase}, replan_coarse={replan_coarse}): the \
                         walker has gone {ticks_since_route} ticks with no route while the goal keeps \
                         moving. A plan must always eventually be posted AND consumed — a planner that \
                         silently stops posting leaves the character at `nav_state: planning` forever, \
                         which is a lie no `is_dead()` check can ever catch.");
                }
            }
        }
    }

    /// The exact ordinary sequence the reviewer used to prove the deadlock: `/goto A`, then re-aim to
    /// `/goto B` 20u away (inside the reset threshold) BEFORE A's plan lands. Once A's reply is
    /// consumed, B must be planned for — not frozen forever.
    #[test]
    fn re_aiming_before_the_first_plan_lands_does_not_freeze() {
        let a = (100.0f32, 0.0, 0.0);
        // /goto A: nothing planned, nothing in flight -> post.
        let d = replan_decision(None, a, None, false, false);
        assert!(d.post, "the first goal must be planned for");
        let (planned, in_flight) = (Some(a), Some(a));

        // Re-aim 20u away while A's plan is still computing. > GOAL_REPLAN_DIST, < GOAL_RESET_DIST:
        // we correctly do NOT supersede the in-flight plan...
        let b = (120.0f32, 0.0, 0.0);
        let d = replan_decision(planned, b, in_flight, false, false);
        assert!(!d.post, "an in-flight plan for essentially this goal is left to land");

        // ...and when it lands, `plan_goal` is CLEARED (apply_plan always runs now). B must then be
        // planned for. If the reply had been dropped instead, in_flight would still be Some(a) here
        // and this would be `false` forever — the deadlock.
        let d = replan_decision(planned, b, None, false, false);
        assert!(d.post,
            "once the in-flight plan is consumed, a goal that has moved must be re-planned for — \
             otherwise the character is frozen at `planning` permanently");
    }

    /// A goal that has not meaningfully moved must not re-plan at all (the cheap half of B1).
    #[test]
    fn a_jittering_goal_does_not_replan() {
        let planned = Some((100.0, 0.0, 0.0));
        // Sub-cell jitter (server position noise, a stationary leader breathing).
        let d = replan_decision(planned, (100.5, 0.3, 0.0), None, false, true);
        assert!(!d.post && !d.reset_route, "sub-cell jitter is the SAME goal — do not re-plan on it");
        // But a proactive re-plan (#246) still gets through.
        let d = replan_decision(planned, (100.5, 0.3, 0.0), None, true, false);
        assert!(d.post, "an armed proactive re-plan must still post");
    }

    #[test]
    fn arrival_action_follow_stays_latched_goto_stops() {
        use super::{arrival_action, ArrivalAction};
        // On the goal's floor (gdz=0). One-shot /goto (following=false): stops for good only within STOP_DIST(2u).
        assert_eq!(arrival_action(1.0, 0.0, false), ArrivalAction::Arrived);
        assert_eq!(arrival_action(3.0, 0.0, false), ArrivalAction::Drive);
        // /follow (following=true): HOLDS within FOLLOW_DIST(10u) — keeps the chase, never "arrives" —
        // and drives again once the leader moves past it (#268). A one-shot goto never HoldFollows.
        assert_eq!(arrival_action(1.0, 0.0, true),  ArrivalAction::FollowHold);
        assert_eq!(arrival_action(9.0, 0.0, true),  ArrivalAction::FollowHold);
        assert_eq!(arrival_action(12.0, 0.0, true), ArrivalAction::Drive); // leader walked off → re-engage
        // Crucially, a follower within melee range does NOT get the terminal `Arrived` a goto would.
        assert_ne!(arrival_action(1.0, 0.0, true), ArrivalAction::Arrived);
    }

    /// #344 (agent-honesty): correct x/y at the WRONG floor is NOT arrival — the walker is a storey
    /// off the goal, and telling the agent `arrived`/`following` there is a confident falsehood.
    #[test]
    fn arrival_action_rejects_wrong_floor_z() {
        use super::{arrival_action, ArrivalAction, Z_ARRIVAL_TOL};
        // Perfect horizontally (well inside STOP_DIST) but a whole floor (50u) below the goal.
        assert_eq!(arrival_action(0.5, 50.0, false), ArrivalAction::Drive,
            "goto: dead-on x/y but a floor below the goal must NOT report Arrived");
        assert_eq!(arrival_action(0.5, -50.0, false), ArrivalAction::Drive,
            "goto: a floor ABOVE the goal must NOT report Arrived either (sign-agnostic)");
        // Same for /follow: a leader one storey up is not "caught up" — keep driving, don't Hold.
        assert_eq!(arrival_action(0.5, 50.0, true), ArrivalAction::Drive,
            "follow: leader a floor up must NOT report FollowHold");
        // Companion: dead-on x/y AND on the goal's floor (within tolerance) DOES arrive / hold.
        assert_eq!(arrival_action(0.5, 0.0, false), ArrivalAction::Arrived);
        assert_eq!(arrival_action(0.5, 0.0, true),  ArrivalAction::FollowHold);
        // Just inside the vertical tolerance (standing height / a step-up) still counts as arrived...
        assert_eq!(arrival_action(0.5, Z_ARRIVAL_TOL - 0.5, false), ArrivalAction::Arrived);
        // ...and just outside it does not. (Mutation check: delete the gdz gate in `arrival_action`
        // and the wrong-floor asserts above flip to Arrived — the test goes RED.)
        assert_eq!(arrival_action(0.5, Z_ARRIVAL_TOL + 0.5, false), ArrivalAction::Drive);
    }

    /// #311 regression: the fast-steering loop re-aims every ~10ms, but `local_path` is only
    /// rebuilt on the 150ms gate. Waypoints are LOCAL_CELL(2u) apart and RUN_SPEED(44u/s) covers
    /// ~6.6u over one gate — more than three segments — so a cursor pinned to segment 0 for the
    /// whole gate saturates its projection (t=1) almost immediately and starts measuring the
    /// carrot from a point BEHIND the walker once a bend is reached. Drive `fast_steer_aim`
    /// through a full 150ms gate (fifteen ~10ms ticks) against a FIXED bending `local_path` — no
    /// rebuild, exactly the gap between rebuilds — and assert the aim keeps leading forward
    /// through the turn instead of collapsing/inverting.
    ///
    /// A hand-simulation of this exact scenario with the index pinned at 0 (the pre-#311 code,
    /// `carrot_along(&self.local_path, 0, ...)`) inverts hard by tick 14: wish_dir flips to
    /// point back down the east leg (dot -0.97) even though the route continues north. The
    /// advancing cursor stays positive throughout (min dot ~0.46) — confirming this scenario
    /// actually reproduces the bug and that the fix (not just a coincidentally-passing test)
    /// is what keeps it green.
    #[test]
    fn fast_steer_carrot_tracks_a_bend_across_a_full_gate_without_inverting() {
        // East leg (0,0)->(6,0), then a 90° bend onto a north leg (6,0)->(6,12); LOCAL_CELL(2u)
        // spacing like the real fine plan.
        let local_path: Vec<[f32; 3]> = vec![
            [0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [4.0, 0.0, 0.0], [6.0, 0.0, 0.0],
            [6.0, 2.0, 0.0], [6.0, 4.0, 0.0], [6.0, 6.0, 0.0], [6.0, 8.0, 0.0],
            [6.0, 10.0, 0.0], [6.0, 12.0, 0.0],
        ];
        let mut local_i = 0usize;
        let mut pos = [0.0f32, 0.0f32, 0.0f32]; // flat path (z=0): 3D projection ≡ the 2D it replaced
        const DT: f32 = 0.01; // ~10ms fast-steering tick
        let mut min_forward_dot = f32::MAX;
        for _ in 0..15 { // 150ms — exactly one local_path gate, deliberately NOT rebuilt
            let (wish_dir, _heading) = fast_steer_aim(&local_path, &mut local_i, pos, 5.0, |_, _| true)
                .expect("a bending path within reach must always produce an aim");
            // Forward tangent of the segment the cursor is currently tracking — wish_dir must
            // never point backward along it.
            let (a, b) = (local_path[local_i], local_path.get(local_i + 1).copied().unwrap_or(local_path[local_i]));
            let seg = [b[0] - a[0], b[1] - a[1]];
            let seg_len = (seg[0] * seg[0] + seg[1] * seg[1]).sqrt();
            if seg_len > 1e-3 {
                let dot = (wish_dir[0] * seg[0] + wish_dir[1] * seg[1]) / seg_len;
                min_forward_dot = min_forward_dot.min(dot);
            }
            pos[0] += wish_dir[0] * eqoxide_core::physics::RUN_SPEED * DT;
            pos[1] += wish_dir[1] * eqoxide_core::physics::RUN_SPEED * DT;
        }
        assert!(min_forward_dot > 0.3,
            "fast-steer aim pointed backward along its tracked segment (dot={min_forward_dot:.2}) \
             at some point in the gate — the carrot cursor collapsed/inverted instead of advancing \
             through the bend (#311)");
        let travelled = (pos[0] * pos[0] + pos[1] * pos[1]).sqrt();
        assert!(travelled > 5.0,
            "walker made almost no net progress over the 150ms gate (ended {travelled:.2}u from \
             start at {pos:?}) — the cursor likely stalled pinned to segment 0 (#311)");
    }

    /// **Water-nav Slice 3 (§8.2): the depth controller must be able to HOLD a mid-water depth —
    /// never surface unbidden.** This is the pure-function heart of the qcat fix (#547/#551): the old
    /// up-only rule returned `0` for any waypoint at/below the swimmer, and `0` hands the swimmer to
    /// buoyancy, which floats it to the surface — so a deep route could not be followed. `swim_vspeed`
    /// returns `0` ONLY when the carrot is at/above the swim plane; below it the wish is ALWAYS nonzero
    /// (buoyancy suppressed) and correctly signed toward the carrot.
    ///
    /// Mutation-discriminating: revert `swim_vspeed` to the old `if carrot > z+1 { 20 } else { 0 }`
    /// and every below-plane assertion here (the ones demanding a nonzero / signed hold) goes RED —
    /// the old rule returns 0 for a carrot at or below the feet.
    #[test]
    fn swim_vspeed_holds_depth_below_the_plane_and_yields_to_buoyancy_at_it() {
        let plane = Some(-6.0); // surface −4, float_depth 2 → swim plane at −6
        // Proportional and signed toward the carrot's depth, clamped to ±SWIM_VRATE.
        assert!(swim_vspeed(-24.0, -20.0, plane) < 0.0, "carrot 4u below the feet → sink (negative)");
        assert!(swim_vspeed(-24.0, -30.0, plane) > 0.0, "carrot 6u above the feet → rise (positive)");
        assert_eq!(swim_vspeed(-100.0, -20.0, plane), -SWIM_VRATE, "a big downward error clamps to −SWIM_VRATE");
        assert_eq!(swim_vspeed(-20.0, -100.0, plane), SWIM_VRATE, "a big upward error clamps to +SWIM_VRATE");
        // An ABOVE-plane entry/haul-out waypoint (toward the surface) must drive an active RISE — the
        // whole reason the up-only rule couldn't be replaced by "0 and let buoyancy do it": buoyancy
        // rests at the plane and never reaches the −4 surface. (Regression: a −4 entry waypoint left
        // the swimmer stuck at the −6 plane, path_i frozen — the first cut of this fix.)
        assert!(swim_vspeed(-4.0, -6.0, plane) > 0.0, "a surface entry waypoint above the plane → rise toward it");
        // THE CRUX (#547): a mid-water hold BELOW the plane must keep a NONZERO wish — a 0 would let
        // buoyancy float the swimmer to the plane (surfacing). At the target the wish is tiny (a hold).
        let hold = swim_vspeed(-24.0, -24.0, plane); // resting exactly at a mid-water goal
        assert!(hold != 0.0, "a mid-water hold must keep a NONZERO wish so buoyancy stays suppressed \
            (0 would let the swimmer float to the surface — the #547 bug)");
        assert!(hold.abs() <= 0.5, "…but at the target the wish is tiny (the SKIN clamp makes it a hold): {hold}");
        // AT/above the plane, a 0 wish is safe — buoyancy simply rests the swimmer at the plane (the
        // ordinary surface-pool crossing), so no spurious hold nudge there.
        assert_eq!(swim_vspeed(-6.0, -6.0, plane), 0.0, "resting AT the plane → 0 (buoyancy rests here)");
        // No bounded surface (open/unbounded water): buoyancy can't act, so a 0 hold is safe there too.
        assert_eq!(swim_vspeed(-50.0, -50.0, None), 0.0, "no surface → 0 is a safe hold (buoyancy inert)");
        assert!(swim_vspeed(-40.0, -50.0, None) > 0.0 && swim_vspeed(-60.0, -50.0, None) < 0.0,
            "no surface → still signed toward the carrot");
    }

    /// Standard sign-of-orientation segment crossing (proper crossings only; collinear touching is
    /// treated as non-crossing, which is fine for the corner scenes below). Pure 2D, for the analytic
    /// LOS closures — the Collision-backed pin lives in `collision.rs` (`los_clamp_rounds_a_baked_l_corner`).
    fn segments_cross(p: [f32; 2], p2: [f32; 2], q: [f32; 2], q2: [f32; 2]) -> bool {
        let o = |a: [f32; 2], b: [f32; 2], c: [f32; 2]| (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
        let (d1, d2) = (o(q, q2, p), o(q, q2, p2));
        let (d3, d4) = (o(p, p2, q), o(p, p2, q2));
        ((d1 > 0.0) != (d2 > 0.0)) && ((d3 > 0.0) != (d4 > 0.0))
    }

    /// **#685: the LOS-clamped carrot ROUNDS a convex corner instead of cutting the chord across it.**
    ///
    /// The plain `carrot_along` advances a fixed arclength ahead, so on an L-path its straight
    /// walker→carrot aim is the CHORD across the corner — which crosses the wall on the inside of the
    /// turn (a convex obstacle the path wraps). `carrot_along_los` stops the carrot at the last point
    /// whose straight shot is clear, so the aim lands on the corner and the walker rounds it.
    ///
    /// MUTATION-DISCRIMINATING: make `carrot_along_los` ignore `los` (behave like `carrot_along`) and
    /// the "rounded carrot is LOS-clear" assertion goes RED — the chord across the corner is exactly
    /// what `los` rejects.
    #[test]
    fn los_clamp_rounds_a_convex_corner_instead_of_cutting_the_chord() {
        // An L-path: east to the corner (10,0), then north. Reach overshoots the corner onto leg 2.
        let path: Vec<[f32; 3]> = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 10.0, 0.0]];
        let from = [4.0, 0.0, 0.0];
        let reach = 10.0;
        // A wall panel on the INSIDE of the turn — the triangle the chord cuts. It touches NEITHER path
        // leg (leg1 runs y=0, leg2 runs x=10), so it blocks ONLY the chord, never the path itself.
        let (w0, w1) = ([9.0f32, 1.0], [9.0f32, 5.0]);
        let los = |a: [f32; 3], b: [f32; 3]| !segments_cross([a[0], a[1]], [b[0], b[1]], w0, w1);

        // The UNCLAMPED carrot cuts the corner: its chord crosses the wall (proves the scene repros).
        let plain = carrot_along(&path, 0, from, reach).unwrap();
        assert!(!los(from, plain),
            "sanity: the unclamped carrot {plain:?} must cut the corner (its chord crosses the wall), \
             else this scene does not reproduce #685");

        // The CLAMPED carrot rounds the corner: its chord is clear, and it sits at/behind the vertex.
        let clamped = carrot_along_los(&path, 0, from, reach, los).unwrap();
        assert!(los(from, clamped),
            "the LOS-clamped carrot {clamped:?} must be reachable in a STRAIGHT line — it must not cut \
             the corner. MUTATION: ignore `los` in carrot_along_los and this goes RED.");
        // Anti-crawl: the clamp shortens toward the corner, it does not retreat behind the walker.
        let ahead = (clamped[0] - from[0]).hypot(clamped[1] - from[1]);
        assert!(ahead >= 4.0,
            "the clamped carrot {clamped:?} must still lead the walker forward toward the corner \
             (ahead={ahead:.1}u) — the clamp rounds the corner, it must never crawl in place");
        assert!(clamped[0] <= 10.0 + 1e-3 && clamped[1] <= 1.0 + 1e-3,
            "the clamped carrot {clamped:?} should rest at/behind the corner vertex (10,0), not up the far leg");
    }

    /// **No over-tightening on a clear straight shot** (#685 dominant risk). With an always-clear `los`
    /// (open ground) the clamped carrot is IDENTICAL to the unclamped one across positions and reaches —
    /// the full LOOK_AHEAD is kept, so straight/gently-curving routes are unchanged and never slow.
    #[test]
    fn los_clamp_keeps_full_reach_on_a_clear_path() {
        let path: Vec<[f32; 3]> = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [20.0, 0.0, 0.0], [30.0, 0.0, 0.0]];
        for &from in &[[0.0f32, 0.0, 0.0], [3.0, 0.0, 0.0], [12.0, 0.0, 0.0]] {
            for &reach in &[5.0f32, 12.0, 24.0] {
                let plain = carrot_along(&path, 0, from, reach).unwrap();
                let clamped = carrot_along_los(&path, 0, from, reach, |_, _| true).unwrap();
                assert_eq!(plain, clamped,
                    "a clear-LOS clamp must equal the unclamped carrot (from={from:?}, reach={reach}) — \
                     shortening a clear straight shot is the over-tightening #685 must avoid");
            }
        }
    }

    /// **The LOS clamp can never STALL the walker** (#685 no-stall / over-tightening extreme). Even if
    /// EVERY forward straight shot is blocked (an adversarial all-false `los`), `carrot_along_los` still
    /// returns a finite aim — the nearest path point — never `None`/NaN, and `steer_target` stays TOTAL.
    #[test]
    fn los_clamp_never_stalls_even_when_everything_is_blocked() {
        let path: Vec<[f32; 3]> = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 10.0, 0.0]];
        let from = [4.0, 0.0, 0.0];
        let blocked = |_: [f32; 3], _: [f32; 3]| false;
        let c = carrot_along_los(&path, 0, from, 10.0, blocked).expect("must still return an aim");
        assert!(c.iter().all(|v| v.is_finite()), "aim must be finite even with all LOS blocked: {c:?}");
        let mut li = 0usize;
        let aim = steer_target(&path, 0, &path, &mut li, from, 5.0, [0.0, 0.0, 0.0], blocked);
        assert!(aim.iter().all(|v| v.is_finite()), "steer_target must stay total under a blocking los: {aim:?}");
    }

    /// **#631 gap 2: horizontal goal re-anchoring is DISCLOSED, and the vertical case is not it.**
    ///
    /// `goal_snapped` (from #344) covers only the VERTICAL snap; a route that does not reach the
    /// requested XY left `goal_snapped: false` and no way to tell (the #482 observation: a goto
    /// planned to a point 55u horizontally from the ask). `route_goal_offset` is the horizontal
    /// companion: 0 for a complete route (it ends exactly at the goal XY — no false positive), the
    /// horizontal shortfall for a partial that stops at its closest approach.
    ///
    /// Mutation-discriminating: hard-code `route_goal_offset` to return `0.0` and the #482-shape and
    /// on-a-plane-partial assertions go RED — a relocated destination becomes invisible again.
    #[test]
    fn route_goal_offset_reports_horizontal_shortfall_only() {
        // A COMPLETE route ends exactly at the requested XY (collision.rs snaps the final waypoint to
        // goal.xy): 0 offset, whatever the z — no false positive on a real route to the goal.
        assert_eq!(route_goal_offset(Some([10.0, 20.0, 5.0]), [10.0, 20.0, -3.0]), 0.0,
            "a route ending at the goal XY is not re-anchored, regardless of its z");
        // The #482 shape: committed endpoint 55u (horizontally) from the named goal.
        let off = route_goal_offset(Some([-607.1, -66.1, -7.0]), [-601.0, -121.0, -8.7]);
        assert!((off - 55.24).abs() < 0.5, "the ~55u horizontal shortfall must be surfaced: {off}");
        // A pure Z difference is NOT a horizontal re-anchoring — that is `goal_snapped`'s job, and
        // double-reporting it here would be a second, confusing signal for the same fact.
        assert_eq!(route_goal_offset(Some([0.0, 0.0, 100.0]), [0.0, 0.0, 0.0]), 0.0,
            "a vertical-only difference is goal_snapped's channel, not a horizontal offset");
        // A DEFINITIVE no-route is not a re-anchoring — there is no committed destination to disclose.
        assert_eq!(route_goal_offset(None, [1.0, 2.0, 3.0]), 0.0);
    }

    /// **#851 — THE UNIVERSAL: the walker's published driving word NEVER reads as unqualified
    /// progress while the body has stopped making progress.**
    ///
    /// "Never" is a universal, and no finite number of live runs discharges one — a race that
    /// usually wins is indistinguishable from a race that cannot lose. So this is an EXHAUSTIVE
    /// MODEL CHECK of the state machine, not a sampled or bounded-length one: it closes, by
    /// breadth-first search, the whole reachable product of ([`RouteExecution`] × an INDEPENDENT
    /// oracle) under every input, and asserts the invariant at every reachable pair. Because the
    /// search CLOSES, the result holds for input sequences of any length, including ones longer than
    /// any enumeration could reach.
    ///
    /// **The oracle is computed from the input history, not from the machine.** It is a single
    /// number — how many consecutive ticks the caller has reported no progress — advanced by the
    /// test's own rule (`+1` on no progress, `0` on progress). The invariant is stated over the
    /// ORACLE: whenever it says the body has been quiet for at least [`NAV_STUCK_TICKS`] ticks, the
    /// word [`driving_nav_state`] produces must not be `navigating` or `navigating_partial`, for
    /// EITHER committed route. A machine that cleared its own stall on a re-path, or on a fresh
    /// route, or on anything other than real progress, is caught, because the oracle does not clear
    /// on those.
    ///
    /// **Why the search is finite, stated rather than assumed.** `RouteExecution::quiet_ticks` grows
    /// without bound, so the raw product is infinite; the search is run over a QUOTIENT that caps it
    /// at `NAV_STUCK_TICKS`. That quotient is a bisimulation of the real machine, for two reasons
    /// that are both *checked below* rather than merely argued:
    ///   1. [`driving_nav_state`] never reads `quiet_ticks` — checked by the first assertion in the
    ///      body below (its condition is bound to a named `let` that says so).
    ///   2. two states the cap identifies step to the SAME capped successor, for every input —
    ///      checked by the second assertion, as an equality of successors and not merely of
    ///      `.is_stalled()`. That distinction is the whole premise: successor equality is what a
    ///      bisimulation needs, and the parity form this test carried first was measurably weaker —
    ///      a `tick` branching on the SOURCE state's exact `quiet_ticks` into `repaths` breaks the
    ///      quotient and leaves parity, and the entire model check, green (#851 review round 1). Do
    ///      not weaken it back. The body below records what this premise does NOT cover, measured
    ///      rather than assumed. (`Advancing` can never hold `quiet_ticks >= NAV_STUCK_TICKS` by
    ///      construction, so the cap is a no-op on that variant — asserted inside `cap` itself.)
    /// So two states identified by the cap have identical futures and identical published words, and
    /// covering the quotient covers the infinite original.
    ///
    /// **What this test does and does not establish.** It establishes the invariant over the
    /// reachable product of the verdict machine and its oracle: no input sequence, of any length,
    /// drives [`driving_nav_state`] to a progress word while the body has been quiet for
    /// [`NAV_STUCK_TICKS`] ticks. It says nothing about whether the WALKER feeds this machine the
    /// right `progressed` signal, nor about what any other writer of `NavStatus::state` publishes —
    /// those are `walker.rs`'s tests and the source scan
    /// `the_driving_nav_state_word_is_only_ever_written_through_the_verdict_851`.
    ///
    /// **Three controls, so a vacuously-green run is impossible.** REACH: the search must visit the
    /// cap. NON-DEGENERACY: all three driving words must be produced somewhere in the reachable set
    /// (otherwise a `driving_nav_state` gutted to always answer `navigating_stalled` passes
    /// perfectly). COUNTING: the reachable set must be larger than the threshold.
    ///
    /// **Mutation checks, both directions** (run at authoring time — outputs in the PR):
    ///   * break `quiet_ticks` monotonicity in [`RouteExecution::tick`] by resetting it on a re-path
    ///     (the shape a "the re-path fixed it" bug takes) → RED on the invariant (the
    ///     machine un-stalls itself on a re-path while the oracle stays quiet). Dropping the
    ///     `self.is_stalled() ||` clause the first draft had instead was EQUIVALENT — see
    ///     [`RouteExecution::tick`], which is why that clause is gone;
    ///   * WRAP the `Stalled` arm of [`driving_nav_state`] in `if false { … }` so it falls through
    ///     to the route match → RED on the invariant;
    ///   * make [`driving_nav_state`] answer `NAV_STATE_NAVIGATING_STALLED` unconditionally → RED on
    ///     the non-degeneracy control, not silently green.
    #[test]
    fn a_stalled_verdict_can_never_be_published_as_unqualified_progress_851() {
        use std::collections::HashSet;
        let routes = [CommittedRoute::Complete, CommittedRoute::Partial];

        // Cap `quiet_ticks` at the threshold. Sound by the two premises checked immediately below —
        // which are stated in terms of this closure, hence its position above them.
        let cap = |e: RouteExecution| match e {
            RouteExecution::Advancing { quiet_ticks } => {
                assert!(quiet_ticks < NAV_STUCK_TICKS,
                    "an `Advancing` verdict must never hold {quiet_ticks} >= NAV_STUCK_TICKS quiet ticks");
                e
            }
            RouteExecution::Stalled { quiet_ticks, repaths } =>
                RouteExecution::Stalled { quiet_ticks: quiet_ticks.min(NAV_STUCK_TICKS), repaths },
        };
        // The input alphabet. `repaths` is modelled as {0, 1, 8} — never re-pathed, mid-recovery,
        // and at the walker's give-up cap — because it is carried into the verdict, and a machine
        // that (wrongly) reset on a re-path must have a re-path to reset on.
        let inputs: Vec<(bool, u32)> = [false, true].iter()
            .flat_map(|&p| [0u32, 1, 8].iter().map(move |&r| (p, r))).collect();

        // ── The two bisimulation premises, CHECKED (see the doc above) ────────────────────────
        // 1. the published word does not depend on `quiet_ticks`…
        for &r in &routes {
            let word_ignores_quiet_ticks = (NAV_STUCK_TICKS..NAV_STUCK_TICKS + 50).all(|q|
                driving_nav_state(r, RouteExecution::Stalled { quiet_ticks: q, repaths: 3 })
                    == driving_nav_state(r, RouteExecution::Stalled { quiet_ticks: NAV_STUCK_TICKS, repaths: 3 }));
            assert!(word_ignores_quiet_ticks,
                "the quotient below is unsound: `driving_nav_state` reads `quiet_ticks`");
        }
        // 2. …and the TRANSITION, once stalled, lands two capped-identical states on the SAME capped
        //    successor — for every input, not merely on the same `is_stalled()` answer.
        //
        //    Successor EQUALITY is what a bisimulation needs, and the first draft of this premise
        //    compared `.is_stalled()` parity instead (#851 review round 1). That is strictly weaker:
        //    a `tick` that branches on the SOURCE state's exact `quiet_ticks` into any field the
        //    parity check does not inspect breaks the quotient while satisfying the premise.
        //
        //    "Write `repaths: 999` at the cap" has TWO readings, and they do not behave alike. Both
        //    were RUN, because the difference is exactly the scope of this premise (round-2 outputs
        //    are in the PR comment):
        //
        //      * SOURCE reading — branch on the state being stepped FROM. This is the one that
        //        breaks the quotient, and it is RED right here, reporting
        //        `left: Stalled { quiet_ticks: 20, repaths: 0 }` against
        //        `right: Stalled { quiet_ticks: 20, repaths: 999 }`. Do not weaken this back.
        //
        //      * SUCCESSOR reading — branch on the value being written. This premise CANNOT see it,
        //        and does not claim to: every source state quantified over below already holds
        //        `quiet_ticks >= NAV_STUCK_TICKS`, so every successor holds one MORE than that, and
        //        the mutated branch never fires. It fires only on the ENTRY transition, which the
        //        quotient is not about. Measured GREEN here and across the whole model check, with
        //        the flip-boundary test below the one that kills it (263 passed / 1 failed over
        //        `-p eqoxide-nav --lib`). That division of labour is deliberate: this premise is
        //        about the cap being SOUND, not about the flip being in the right place.
        for q in NAV_STUCK_TICKS..NAV_STUCK_TICKS + 50 {
            for (progressed, repaths) in inputs.iter().copied() {
                let from_q   = cap(RouteExecution::Stalled { quiet_ticks: q, repaths: 3 }
                                    .tick(progressed, repaths));
                let from_cap = cap(RouteExecution::Stalled { quiet_ticks: NAV_STUCK_TICKS, repaths: 3 }
                                    .tick(progressed, repaths));
                assert_eq!(from_q, from_cap,
                    "the quotient below is unsound: `tick` branches on the exact `quiet_ticks` ({q}) \
                     — capped successors differ under (progressed={progressed}, repaths={repaths})");
            }
        }

        // The oracle saturates at the same place as `cap`, for the same reason: past the threshold
        // the invariant's antecedent is already true and cannot become false without a `progressed`.
        let oracle_cap = NAV_STUCK_TICKS;

        let start = (RouteExecution::fresh(), 0u32);
        let mut seen: HashSet<(RouteExecution, u32)> = HashSet::new();
        let mut queue = vec![start];
        seen.insert(start);
        let mut reached_saturation = false;
        let mut words: HashSet<&'static str> = HashSet::new();
        let mut visited = 0usize;

        while let Some((exec, quiet)) = queue.pop() {
            visited += 1;
            if quiet >= oracle_cap { reached_saturation = true; }
            for route in routes {
                let word = driving_nav_state(route, exec);
                words.insert(word);
                // THE UNIVERSAL, stated over the ORACLE.
                if quiet >= NAV_STUCK_TICKS {
                    assert_ne!(word, NAV_STATE_NAVIGATING,
                        "#851: published `navigating` after {quiet} quiet ticks (verdict {exec:?})");
                    assert_ne!(word, NAV_STATE_NAVIGATING_PARTIAL,
                        "#851: published `navigating_partial` after {quiet} quiet ticks (verdict {exec:?})");
                }
                // The other direction, so the machine cannot buy the invariant by crying wolf: the
                // stalled WORD may only come from a stalled VERDICT, and a stalled verdict may never
                // coexist with an oracle that has just seen real progress.
                if word == NAV_STATE_NAVIGATING_STALLED {
                    assert!(exec.is_stalled(), "the stalled WORD must come from a stalled VERDICT");
                }
                assert!(!(exec.is_stalled() && quiet == 0),
                    "#851: the verdict must clear the instant real progress is made (quiet=0)");
            }
            for (progressed, repaths) in inputs.iter().copied() {
                let next = (cap(exec.tick(progressed, repaths)),
                            if progressed { 0 } else { (quiet + 1).min(oracle_cap) });
                if seen.insert(next) { queue.push(next); }
            }
        }

        // CONTROL 1 (REACH): the search really did explore past the stall threshold.
        assert!(reached_saturation,
            "the model check never reached {oracle_cap} quiet ticks — the invariant above was checked \
             only where it is trivially true (visited {visited} states)");
        // CONTROL 2 (COUNTING): the reachable set is at least one state per tick up to the threshold.
        assert!(visited > NAV_STUCK_TICKS as usize,
            "the reachable product collapsed to {visited} states — the machine is not counting");
        // CONTROL 3 (NON-DEGENERACY): all three driving words are genuinely produced.
        for w in [NAV_STATE_NAVIGATING, NAV_STATE_NAVIGATING_PARTIAL, NAV_STATE_NAVIGATING_STALLED] {
            assert!(words.contains(w),
                "`{w}` is never produced anywhere in the reachable set — the invariant is vacuous");
        }
    }

    /// **The threshold is the walker's own, and it is REACHED — not merely written (#851).**
    ///
    /// The model check above proves the machine never contradicts its oracle. It does not pin WHERE
    /// the machine flips, and a machine that flipped at tick 1 (or at tick 10_000) would satisfy it
    /// just as well. One of those is a client that shouts `navigating_stalled` at every wall-slide;
    /// the other is the ~32 s lie #851 exists to remove. This walks the machine tick by tick and
    /// names the boundary on both sides.
    #[test]
    fn the_execution_verdict_flips_at_the_walkers_own_stall_threshold_851() {
        let mut exec = RouteExecution::fresh();
        for i in 1..NAV_STUCK_TICKS {
            exec = exec.tick(false, 0);
            assert!(!exec.is_stalled(),
                "flipped at tick {i}, before the walker's own {NAV_STUCK_TICKS}-tick stall line — a \
                 brief wall-slide would be reported as a wedge");
            assert_eq!(driving_nav_state(CommittedRoute::Complete, exec), NAV_STATE_NAVIGATING);
        }
        exec = exec.tick(false, 0);
        assert_eq!(exec, RouteExecution::Stalled { quiet_ticks: NAV_STUCK_TICKS, repaths: 0 },
            "the verdict must flip exactly at NAV_STUCK_TICKS (~{}s at the 150ms nav tick)",
            NAV_STUCK_TICKS * 150 / 1000);
        assert_eq!(driving_nav_state(CommittedRoute::Complete, exec), NAV_STATE_NAVIGATING_STALLED);
        assert_eq!(driving_nav_state(CommittedRoute::Partial,  exec), NAV_STATE_NAVIGATING_STALLED,
            "a stalled PARTIAL is not `navigating_partial` either — that word is a progress claim too");

        // A re-path does not launder it: eight of them, the walker's whole recovery budget, with the
        // body never moving. This is the #851 measurement's own shape.
        for repaths in 1..=8u32 {
            for _ in 0..(NAV_STUCK_TICKS + NAV_BACKOFF_TICKS) {
                exec = exec.tick(false, repaths);
                assert!(exec.is_stalled(), "re-path {repaths} laundered the stall into progress");
            }
        }
        // …and REAL progress clears it on the very next tick.
        exec = exec.tick(true, 8);
        assert_eq!(exec, RouteExecution::fresh(), "real progress must clear the verdict immediately");
        assert_eq!(driving_nav_state(CommittedRoute::Complete, exec), NAV_STATE_NAVIGATING);
    }

    /// **A verdict about one goal is not evidence about another (#851 review round 2, B1) — the
    /// rule, in isolation from any walker.**
    ///
    /// [`GoalVerdict`] exists because the walker kept the verdict and the goal id in two fields and
    /// one publisher read only the first. Here the whole rule is one function, so there is nothing
    /// left to forget to consult: every read names a goal. The walker-level trajectory that was
    /// measured wrong is pinned separately by
    /// `crate::walker::tests::a_moving_follow_chase_never_publishes_the_previous_goals_851_stall`.
    ///
    /// Mutation checks: make [`GoalVerdict::as_of`] ignore its argument → RED here (and RED at the
    /// walker level); make [`GoalVerdict::tick`] continue the stored verdict instead of
    /// `self.as_of(goal_id)` → RED on the last block here.
    #[test]
    fn a_goal_verdict_is_only_evidence_about_its_own_goal_851() {
        // Wedge goal #1 all the way to the latch.
        let mut v = GoalVerdict::fresh_for(1);
        for _ in 0..NAV_STUCK_TICKS { v = v.tick(1, false, 3); }
        assert!(v.as_of(1).is_stalled(), "PREMISE: goal #1 really is latched stalled");
        assert_eq!(driving_nav_state(CommittedRoute::Complete, v.as_of(1)),
            NAV_STATE_NAVIGATING_STALLED, "and it publishes as such for its OWN goal");

        // The same value, read for a DIFFERENT goal, is not evidence of anything about that goal.
        assert!(!v.as_of(2).is_stalled(),
            "#851 B1: goal #1's wedge must not be readable as goal #2's verdict");
        assert_eq!(v.as_of(2), RouteExecution::fresh(),
            "…and what it reads instead is the honest 'nothing observed on this journey yet'");
        assert_eq!(driving_nav_state(CommittedRoute::Complete, v.as_of(2)), NAV_STATE_NAVIGATING);
        assert!(v.is_about(1) && !v.is_about(2), "the verdict names which journey it is about");

        // Ticking for a new goal starts THAT goal's count — it does not continue goal #1's.
        let v2 = v.tick(2, false, 0);
        assert_eq!(v2.as_of(2), RouteExecution::Advancing { quiet_ticks: 1 },
            "#851 B1: goal #2's first quiet tick is its FIRST, not goal #1's twenty-first");
        assert!(!v2.as_of(1).is_stalled(),
            "…and goal #1's verdict is gone, not shadowed — it is one value, not two");
    }

    /// **#631 gap 3: the no-progress property — TERMINATE a lap that never closes on the goal, and
    /// NEVER a route that is genuinely (even slowly, even via a detour) getting there.**
    ///
    /// This is the over-firing-prone half of #631, so it is pinned as a property over trajectories,
    /// not one example. The policy is exactly the walker's: `progress_improved` records the closest
    /// 3-D approach; the clock resets whenever it improves by [`NAV_PROGRESS_EPS`]; navigation
    /// terminates only when it has NOT improved for [`NAV_NO_PROGRESS_WINDOW`].
    ///
    /// Mutation-discriminating both ways: widen `NAV_PROGRESS_EPS` to `f32::MAX` (nothing counts as
    /// progress) and the APPROACH/DETOUR/SLOW cases fire → RED; shrink it to `0.0` (jitter counts as
    /// progress) and the CIRCLING case never fires → RED.
    #[test]
    fn no_progress_terminates_a_lap_but_never_a_route_that_is_getting_there() {
        use std::time::{Duration, Instant};
        // Replays (t_secs, closest-3D-dist) through the walker's exact policy; returns the time the
        // walker would terminate `no_progress`, or None if it never does.
        let fires_at = |samples: &[(f32, f32)]| -> Option<f32> {
            let base = Instant::now();
            let mut best = f32::MAX;
            let mut improved_at = base;
            for &(t, d) in samples {
                let now = base + Duration::from_secs_f32(t);
                if progress_improved(&mut best, d, NAV_PROGRESS_EPS) {
                    improved_at = now;
                } else if now.duration_since(improved_at) >= NAV_NO_PROGRESS_WINDOW {
                    return Some(t);
                }
            }
            None
        };
        let win = NAV_NO_PROGRESS_WINDOW.as_secs_f32();

        // CIRCLING (the moat): constant distance forever. Best is set on the first sample and never
        // improves, so it MUST terminate — right at the window, not before, not never.
        let circling: Vec<(f32, f32)> = (0..80).map(|i| (i as f32 * 5.0, 60.0)).collect();
        let fired = fires_at(&circling).expect("a lap that never closes on the goal MUST terminate");
        assert!((fired - win).abs() <= 5.0 + 1e-3,
            "the moat must terminate at ~{win}s (the window), got {fired}s");

        // STEADILY APPROACHING (even SLOWLY): distance falls by >EPS within every window. A very slow
        // 0.4 u/s crawl (12u per 30s > the 8u EPS) still resets the clock each step — never killed.
        let slow: Vec<(f32, f32)> = (0..40).map(|i| (i as f32 * 30.0, 500.0 - i as f32 * 12.0)).collect();
        assert_eq!(fires_at(&slow), None,
            "a slow-but-steady approach keeps improving closest-approach — it must NEVER be killed");

        // A NECESSARY DETOUR that first goes AWAY (distance rises for ~25s) then closes. Best stays
        // flat during the away-leg but the clock has NOT yet run out, and the approach then resets it
        // well within the window — the detour survives. (This is the exact false-positive the issue
        // warns about: a route that temporarily increases distance must not be killed.)
        let mut detour: Vec<(f32, f32)> = Vec::new();
        for i in 0..6 { detour.push((i as f32 * 5.0, 100.0 + i as f32 * 12.0)); } // away 0..25s: 100→160
        for i in 1..20 { detour.push((25.0 + i as f32 * 5.0, 160.0 - i as f32 * 15.0)); } // back, brisk
        assert_eq!(fires_at(&detour), None,
            "a detour that temporarily increases distance then closes must survive — killing it is \
             the over-firing #631 explicitly forbids");

        // A LONG STRAIGHT RUN: monotone decrease over minutes. Never fires (improves every sample).
        let straight: Vec<(f32, f32)> = (0..60).map(|i| (i as f32 * 5.0, 3000.0 - i as f32 * 44.0)).collect();
        assert_eq!(fires_at(&straight), None, "a long straight run keeps closing — never killed");
    }
}
