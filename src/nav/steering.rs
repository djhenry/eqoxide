//! Pure navigation/steering math — pursuit carrots, replan/arrival decisions, the fast-steering
//! cursor. Net-independent: this takes positions and paths, and depends only on `assets` types
//! (no `EqStream`, no packets). Extracted out of `eq_net::navigation` (cleanup step 2 — nav must
//! not live inside net). The `Navigator` god-struct and its `tick()`/`sync_*`/`apply_*plan`
//! methods (the net action loop) are a later step and still live in `eq_net::navigation`.

use crate::coord::eq_heading;

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
pub(crate) const NAV_STUCK_TICKS: u32 = 20;
/// After this many consecutive no-progress ticks (well before the `NAV_STUCK_TICKS` give-up), the
/// walker commands the controller to hop — net progress has stalled, which is the real "wedged
/// against a fence/cart" signal (sliding along it still looks like motion frame-to-frame). (#41)
pub(crate) const NAV_HOP_TICKS: u32 = 6;
/// On a hard stall (NAV_STUCK_TICKS), drive the reverse (downhill) direction for this many ticks
/// before re-pathing — long enough to clear a wedged slope-face start (~150 ms/tick). (eqoxide#212)
pub(crate) const NAV_BACKOFF_TICKS: u32 = 3;
/// Proactive re-plan (#246): after this many consecutive ticks where the fine 2u plan can't REACH its
/// carrot on the committed coarse route, the route is treated as blocked ahead and re-planned from the
/// current position — long before the ~3 s NAV_STUCK_TICKS give-up, so the walker detours instead of
/// pressing into the obstacle. Small so the reaction is quick (~0.5 s) but > 1 to ride out a carrot
/// that momentarily lands on a fine-impassable lip.
pub(crate) const NAV_LOCAL_STUCK_TICKS: u32 = 3;
/// Minimum ticks between two proactive coarse re-plans, so a persistently-awkward carrot can't thrash
/// the coarse planner every tick (~1 s). The existing stall/back-off recovery still handles a genuine
/// wedge the fresh coarse plan can't route around.
pub(crate) const REPLAN_COOLDOWN_TICKS: u32 = 6;
/// How many PROACTIVE coarse re-plans (#246) may fire at ONE spot — without the journey getting
/// meaningfully closer to the goal — before the walker stops honestly (#378 Phase 2). Each proactive
/// re-plan reinstalls a fresh coarse route and so resets the stall clock, which is why the ~3 s
/// `NAV_STUCK_TICKS` give-up never trips at a fine-impassable spot and the walker oscillated
/// `navigating` forever (the live qcat L-corner). At ~(NAV_LOCAL_STUCK_TICKS + REPLAN_COOLDOWN_TICKS)
/// ≈ 9 ticks per proactive re-plan, 8 of them is ~11 s of trying to detour before the honest
/// `blocked / local_no_way_through`. Resets on real goal-ward progress (like `nav_repaths`), so a
/// long multi-corner journey that keeps progressing never trips it.
pub(crate) const PROACTIVE_REPLAN_CAP: u32 = 8;
/// After auto-escaping a sealed interior through an in-zone teleport (#266), block another escape for
/// this long (~10 s at 150 ms/tick) so a goal that's STILL unreachable after the teleport can't
/// ping-pong the char back and forth through the portal. One escape attempt, then it walks/stalls.
pub(crate) const PORTAL_COOLDOWN_TICKS: u32 = 66;
/// A path segment longer than this (horizontal) is a find_path JUMP-EDGE, not a walk — normal
/// adjacent nav cells are ≤ 8·√2 ≈ 11.3u apart, jump-edges span ≥ 16u across a real gap. The walker
/// asks the controller to jump when traversing such a segment. (eqoxide#190)
pub(crate) const JUMP_SEG_MIN: f32 = 12.0;
/// Only fire the jump while within this of the takeoff waypoint — so the leap starts grounded at
/// the near edge and does NOT re-trigger after landing (just under the 8u nav cell). (eqoxide#190)
pub(crate) const JUMP_TAKEOFF_DIST: f32 = 7.0;
// The planner itself now lives on its own thread — see `crate::nav::planner`. `plan_path`
// moved there wholesale: it used to run SYNCHRONOUSLY here, on the network thread, which is the
// single root cause of #340 (up to ~2 s of net-thread stall → linkdead) and #337 (the 150 ms budget
// forced A* to give up, and a give-up was indistinguishable from "no route", so the walker silently
// drove a partial route into a wall and froze). `Navigator::tick` now POSTS a request and returns.

/// A chase goal must move at least this far (one nav cell) before it counts as a different goal
/// worth re-planning for. `/follow` and `/goto <entity>` rewrite the goal with the leader's LIVE
/// position EVERY TICK, so an exact compare called it "changed" ~every tick (#377 review, B1).
pub(crate) const GOAL_REPLAN_DIST: f32 = 8.0;
/// A goal that moves further than this is a different DESTINATION, not a drifting one: the committed
/// route is thrown away, the journey counters reset, and any in-flight plan is superseded.
pub(crate) const GOAL_RESET_DIST: f32 = 40.0;

/// What a tick should do about (re)planning. Pure, so the `/follow` freeze below is unit-testable
/// without a live `EqStream`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) struct Replan {
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
pub(crate) fn replan_decision(
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
pub(crate) fn portal_escape_applies(why: crate::nav::collision::NoRoute) -> bool {
    use crate::nav::collision::NoRoute;
    matches!(why, NoRoute::SearchClosed | NoRoute::StartIsolated)
}

/// What the walker should do on reaching (near) its goal, kept pure so the follow-vs-goto distinction
/// is unit-tested off the tick. `Arrived` = a one-shot /goto is done → stop for good. `FollowHold` = a
/// /follow chase has caught up → stand near the leader but STAY latched so it re-engages when the
/// leader moves (#268). `Drive` = not there yet → keep walking.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ArrivalAction { Drive, Arrived, FollowHold }

/// Arrival radius for a one-shot /goto (melee range is ~14u, so 2u keeps us well inside it).
pub(crate) const STOP_DIST: f32 = 2.0;
/// A /follow settles up to this far behind the leader (a bit behind, still in group range).
pub(crate) const FOLLOW_DIST: f32 = 10.0;

/// Stop within 2u for a one-shot /goto; a /follow settles up to FOLLOW_DIST behind the leader.
pub(crate) fn arrival_action(gdist: f32, following: bool) -> ArrivalAction {
    if following {
        if gdist <= FOLLOW_DIST { ArrivalAction::FollowHold } else { ArrivalAction::Drive }
    } else if gdist <= STOP_DIST {
        ArrivalAction::Arrived
    } else {
        ArrivalAction::Drive
    }
}

/// A pure-pursuit carrot: the point `reach` units along `path` (starting from segment `start_i`),
/// measured from the projection of `from` onto that segment. Returns `[east, north, z]`, carrying the
/// z of the segment the carrot lands on. Used at two scales: a far carrot (~LOCAL_REACH) as the fine
/// plan's goal, and a near carrot (LOOK_AHEAD) along the fine plan as the steering aim.
pub(crate) fn carrot_along(path: &[[f32; 3]], start_i: usize, from: [f32; 2], reach: f32) -> Option<[f32; 3]> {
    let a = *path.get(start_i)?;
    let b = path.get(start_i + 1).copied().unwrap_or(a);
    let ab = [b[0] - a[0], b[1] - a[1]];
    let l2 = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if l2 < 1e-6 { 0.0 } else { (((from[0] - a[0]) * ab[0] + (from[1] - a[1]) * ab[1]) / l2).clamp(0.0, 1.0) };
    let mut cur = [a[0] + ab[0] * t, a[1] + ab[1] * t];
    let (mut rem, mut i, mut cz) = (reach, start_i, b[2]);
    loop {
        match path.get(i + 1).copied() {
            Some(bp) => {
                cz = bp[2];
                let d = [bp[0] - cur[0], bp[1] - cur[1]];
                let dl = (d[0] * d[0] + d[1] * d[1]).sqrt();
                if dl >= rem || i + 2 >= path.len() {
                    let c = if dl < 1e-6 { cur } else { [cur[0] + d[0] * (rem / dl).min(1.0), cur[1] + d[1] * (rem / dl).min(1.0)] };
                    break Some([c[0], c[1], cz]);
                }
                rem -= dl; cur = [bp[0], bp[1]]; i += 1;
            }
            None => break Some([cur[0], cur[1], cz]),
        }
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
pub(crate) fn fast_steer_aim(path: &[[f32; 3]], local_i: &mut usize, from: [f32; 2], reach: f32) -> Option<([f32; 2], f32)> {
    advance_cursor(path, local_i, from);
    let aim = carrot_along(path, *local_i, from, reach)?;
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
pub(crate) fn advance_cursor(path: &[[f32; 3]], i: &mut usize, from: [f32; 2]) {
    // A cursor can only ever index the path it was advanced along, whatever it held before. The fine
    // path is now REPLACED asynchronously, by a worker, with one that may be SHORTER than the one the
    // cursor was walking — so "the cursor outran the path" is a state this code must simply not have.
    // Clamping here makes it unrepresentable everywhere downstream, rather than leaving each caller to
    // remember a bounds check. (Found by the `the_walker_never_stalls_waiting_on_the_fine_plan`
    // property test, which fuzzes exactly this.)
    *i = (*i).min(path.len().saturating_sub(1));
    while *i + 2 < path.len() {
        let (a, b) = (path[*i], path[*i + 1]);
        let ab = [b[0] - a[0], b[1] - a[1]];
        let l2 = ab[0] * ab[0] + ab[1] * ab[1];
        let t = if l2 < 1e-6 { 1.0 } else { ((from[0] - a[0]) * ab[0] + (from[1] - a[1]) * ab[1]) / l2 };
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
pub(crate) fn steer_target(
    coarse: &[[f32; 3]], path_i: usize,
    local:  &[[f32; 3]], local_i: &mut usize,
    from: [f32; 2], look_ahead: f32,
    fallback: [f32; 3],
) -> [f32; 3] {
    // The coarse carrot: the aim we ALWAYS have while a route is committed.
    let coarse_aim = carrot_along(coarse, path_i, from, look_ahead).unwrap_or(fallback);
    // The fine carrot, when the fine tier has given us a path worth steering along. A 1-waypoint
    // "path" is just the character's own position and steers nowhere.
    if local.len() >= 2 {
        // The fine plan was computed a tick or two ago, FROM a point the walker has since driven past
        // (#382) — so advance the cursor onto the segment it is actually on before measuring the
        // carrot, or the aim is taken from behind it (#311).
        advance_cursor(local, local_i, from);
        carrot_along(local, *local_i, from, look_ahead).unwrap_or(coarse_aim)
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
pub(crate) fn arms_coarse_replan(outcome: &crate::nav::collision::LocalOutcome) -> bool {
    matches!(outcome, crate::nav::collision::LocalOutcome::NoWayThrough { .. })
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
        use crate::nav::collision::NoRoute;
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
        // One-shot /goto (following=false): stops for good only within STOP_DIST(2u).
        assert_eq!(arrival_action(1.0, false), ArrivalAction::Arrived);
        assert_eq!(arrival_action(3.0, false), ArrivalAction::Drive);
        // /follow (following=true): HOLDS within FOLLOW_DIST(10u) — keeps the chase, never "arrives" —
        // and drives again once the leader moves past it (#268). A one-shot goto never HoldFollows.
        assert_eq!(arrival_action(1.0, true),  ArrivalAction::FollowHold);
        assert_eq!(arrival_action(9.0, true),  ArrivalAction::FollowHold);
        assert_eq!(arrival_action(12.0, true), ArrivalAction::Drive); // leader walked off → re-engage
        // Crucially, a follower within melee range does NOT get the terminal `Arrived` a goto would.
        assert_ne!(arrival_action(1.0, true), ArrivalAction::Arrived);
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
        let mut pos = [0.0f32, 0.0f32];
        const DT: f32 = 0.01; // ~10ms fast-steering tick
        let mut min_forward_dot = f32::MAX;
        for _ in 0..15 { // 150ms — exactly one local_path gate, deliberately NOT rebuilt
            let (wish_dir, _heading) = fast_steer_aim(&local_path, &mut local_i, pos, 5.0)
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
            pos[0] += wish_dir[0] * crate::eq_net::navigation::RUN_SPEED * DT;
            pos[1] += wish_dir[1] * crate::eq_net::navigation::RUN_SPEED * DT;
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
}
