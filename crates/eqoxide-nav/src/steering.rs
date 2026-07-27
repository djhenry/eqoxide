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
pub const CURSOR_STALE_DIST: f32 = 8.0;

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
///   segment, so the budget is eaten by a phantom leg and the carrot lands essentially ON TOP of the
///   character — in the measured #673 case, 0.2–0.5 u away, flipping to the opposite side each tick.
/// * The stall detector watches `path_i`, which can now never advance.
///
/// The result is a stable limit cycle: the character oscillates over ~0.4 u, makes zero net progress,
/// exhausts its re-paths and stops with `blocked` / `walker_stalled` on a route it is physically
/// standing on. (Measured in South Qeynos on the qcat aqueduct ramp; reproduced offline from the
/// captured live route — see `walker_cursor_resync` tests.)
///
/// This restores the cursor's invariant — *`path_i` names the segment the character is actually on* —
/// with two hard guards that keep it strictly conservative:
///
/// 1. **Forward only.** The scan starts at `start_i`; the cursor can never move backwards, so a lap
///    can never be un-counted and the #631/#309 progress channels keep their meaning.
/// 2. **Only onto a reachable segment.** A later segment is adopted only if `clear(from, closest)`
///    holds — the same straight-line sweep the carrot's LOS clamp uses — so the cursor can never
///    jump across a wall and declare a leg of the route walked that the character never walked.
///
/// A character within [`CURSOR_STALE_DIST`] of its current segment is left alone entirely, so the
/// normal case is untouched (and the `clear` predicate is not even called).
pub fn resync_cursor(
    path: &[[f32; 3]],
    start_i: usize,
    from: [f32; 3],
    clear: impl Fn([f32; 3], [f32; 3]) -> bool,
) -> usize {
    // A cursor needs at least one segment ahead of it to be resyncable; `path_i + 2 <= len` mirrors
    // the walker's own advance bound so a resync can never park the cursor past the last segment.
    if path.len() < 3 || start_i + 2 >= path.len() {
        return start_i;
    }
    let (_, d0_sq) = seg_closest(path[start_i], path[start_i + 1], from);
    if d0_sq <= CURSOR_STALE_DIST * CURSOR_STALE_DIST {
        return start_i;
    }
    let (mut best_i, mut best_sq) = (start_i, d0_sq);
    for i in (start_i + 1)..(path.len() - 1) {
        let (c, d_sq) = seg_closest(path[i], path[i + 1], from);
        if d_sq < best_sq && clear(from, c) {
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

    /// **#673 — the OBSERVABLE consequence: the carrot must lead, not sit on the character.**
    ///
    /// This is the deadlock itself, expressed as a pure function. With the stale cursor the
    /// LOCAL_REACH-scale carrot lands 0.2 u from the character (and flips side each tick — a limit
    /// cycle at zero net displacement, which the walker reports as `blocked` / `walker_stalled`).
    /// With the cursor resynced it leads by ~the full reach, down the ramp the character is on.
    #[test]
    fn a_stale_cursor_collapses_the_carrot_onto_the_character() {
        const REACH: f32 = 24.0; // the walker's LOCAL_REACH
        let stale = carrot_along(&HAIRPIN, STALE_I, LANDED, REACH).unwrap();
        let d_stale = (stale[0] - LANDED[0]).hypot(stale[1] - LANDED[1]);
        assert!(d_stale < 1.0,
            "fixture no longer reproduces the collapse (carrot was {d_stale:.2} u ahead)");

        let i = resync_cursor(&HAIRPIN, STALE_I, LANDED, always_clear);
        let fixed = carrot_along(&HAIRPIN, i, LANDED, REACH).unwrap();
        let d_fixed = (fixed[0] - LANDED[0]).hypot(fixed[1] - LANDED[1]);
        assert!(d_fixed > 8.0,
            "after resync the carrot must actually lead the character; it was {d_fixed:.2} u ahead");
        // …and it must lead DOWN the ramp (west), i.e. forward along the route.
        assert!(fixed[0] < LANDED[0], "carrot must lead forward along the route, got {fixed:?}");
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

    /// **Why the [`CURSOR_STALE_DIST`] guard is load-bearing.** On a tight switchback a walker that
    /// is genuinely mid-leg can be geometrically CLOSER to a later segment than to its own — here
    /// 1.5 u from segment 0 but 0.5 u from segment 2, an out-and-back only 2 u wide. A
    /// nearest-segment snap would skip the entire outbound leg and quietly cut the route; the
    /// distance guard means a walker still on its segment is never touched.
    #[test]
    fn a_walker_cutting_a_tight_switchback_keeps_its_cursor() {
        let switchback = [[0.0f32, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 2.0, 0.0], [0.0, 2.0, 0.0]];
        assert_eq!(resync_cursor(&switchback, 0, [5.0, 1.5, 0.0], always_clear), 0,
            "a walker mid-leg must not be snapped onto a nearer later segment");
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

    /// **Property — the cursor stays a valid segment index for ANY position.** Never backwards,
    /// never onto the final waypoint (which is not the start of a segment), for a dense sweep of
    /// positions over and around the fixture, from every starting cursor.
    #[test]
    fn resync_always_returns_a_valid_forward_segment_index() {
        for start_i in 0..HAIRPIN.len() - 1 {
            let mut x = -570.0f32;
            while x <= -510.0 {
                let mut y = 130.0f32;
                while y <= 175.0 {
                    for z in [-20.0f32, -6.0, 0.0, 12.0] {
                        let i = resync_cursor(&HAIRPIN, start_i, [x, y, z], always_clear);
                        assert!(i >= start_i, "moved backwards: {start_i} -> {i}");
                        assert!(i + 1 < HAIRPIN.len(), "cursor {i} is not a segment start");
                    }
                    y += 3.0;
                }
                x += 3.0;
            }
        }
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
