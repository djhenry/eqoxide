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
///
/// **The comparison is `<`, not `<=`, and that is measured, not cosmetic (#727 round 2).** The
/// deadlock #673 describes has an ATTRACTING FIXED POINT sitting exactly ON this boundary: on a
/// hairpin whose legs are one coarse cell apart the walker converges to a body offset of exactly
/// 8.0 u and parks there. With `<=` that state is *inside* the guard, so the resync never fires and
/// the wedge survives — measured 1 of 8 swept starts still wedged after 400 ticks, and 33 of 288 on
/// the wider 8 u-separation sweep. With `<` it is outside, and both go to zero
/// (`the_deadlock_fixed_point_exactly_on_the_guard_boundary_is_resynced`,
/// `the_resync_clears_the_deadlock_above_the_guard_and_is_inert_below_it`).
///
/// **What this constant does NOT do — the residual class, stated so nobody re-derives it as a
/// surprise.** Below the guard the resync is inert by construction, so a route whose legs are
/// closer together than `CURSOR_STALE_DIST` can still form the #673 cycle. Measured on the same
/// hairpin sweep (wedged starts / total): 8 u and above → 0 wedged; 7 u → 252/252; 6 u → 216/216;
/// 4 u → 144/144, i.e. below the guard the fix is not partial, it is absent. (The round-1 review
/// measured 133/1649 at 8 u against the `<=` code on its own harness; the number above is this
/// harness's own, not that one re-quoted.) The real deadlock invariant is CARROT COLLAPSE (the
/// `LOCAL_REACH` carrot landing within ~the body offset of the body while `path_i` is pinned), and
/// a distance guard is only a proxy for it — wrong on exactly the routes whose legs are closer
/// together than the guard. Tracked as its own issue; do not read this constant as pinning a
/// value, it is unpinned over at least [2, 16].
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
/// walkability on its own. The walker passes a conjunction of a chest-height line-of-sight ray
/// (excludes WALLS) and a floor-column probe along the hop (excludes VOIDS and drops steeper than
/// the controller's own slope+step envelope). That pairing exists because the round-1 review broke
/// the LOS ray alone with a counterexample and it is worth stating plainly: **a hole is not a
/// wall.** `Collision::carrot_los_clear` is documented in its own rustdoc as a chest-height centre
/// ray, chosen deliberately to ride ABOVE ground undulation; asked "has the character reached this
/// segment" it flies straight over a chasm. Measured: two ledges split by a 10 u gap with the next
/// floor 200 u down, and the LOS ray alone moved the cursor 2 → 6, declaring an entire bridge
/// detour walked (`crate::walker`'s `a_resync_must_not_cross_a_chasm_the_character_cannot_walk`).
///
/// Even with the floor probe this is a **necessary, not a sufficient** condition: it samples a
/// line, so a hole narrower than the probe spacing can fall between samples, and it says nothing
/// about the character's WIDTH. So the honest statement of what a resync means is *"the character
/// is within [`CURSOR_RESYNC_MAX_HOP`] of this segment, with no wall and no sampled void between"* —
/// **not** "the character walked this leg". Which is why the walker deliberately does not report a
/// resync jump as PROGRESS: see `Walker::advance_cursor`.
///
/// A character within [`CURSOR_STALE_DIST`] of its current segment is left alone entirely, so the
/// normal case is untouched (and `reachable` is not even called).
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
    // `<`, not `<=`: the deadlock's fixed point sits exactly ON the boundary — see CURSOR_STALE_DIST.
    if d0_sq < CURSOR_STALE_DIST * CURSOR_STALE_DIST {
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
    /// ⚠️ **Correction (round 3).** Through round 2 this test was documented as measuring "the
    /// walker's carrot", and the reach below was commented `// the walker's LOCAL_REACH` as though
    /// 24 u were a steering target. It is not. `drive_walk` steers with `LOOK_AHEAD = 5.0`;
    /// `LOCAL_REACH = 24.0` is only the *goal it hands the fine planner*. The round-2 review was
    /// right that at 5 u off the coarse route this carrot does **not** collapse (17.06 u ahead on
    /// this very fixture). The collapse measured here is real, but it is a collapse of `local_goal`,
    /// and it reaches the steering aim by a different route — see the two tests that follow, which
    /// carry that chain the rest of the way and are the ones that measure a wedge.
    #[test]
    fn a_stale_cursor_collapses_the_fine_planners_goal_onto_the_character() {
        // Not a steering carrot: this is `drive_walk`'s `local_goal`, the point handed to
        // `find_path_local` as the fine plan's destination.
        const REACH: f32 = 24.0;
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
    struct Run {
        arrived: bool,
        ticks: u32,
        /// Straight-line distance from the start position to wherever the run ended.
        net: f32,
        /// Extent of the east coordinate over the whole run — the width of the oscillation, if any.
        x_min: f32,
        x_max: f32,
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
    /// "wedge" is, it only integrates position and reports it; and
    /// `a_walker_whose_cursor_is_honest_walks_this_same_fixture_out` runs it with a correct cursor
    /// and no fix, so a harness that wedged unconditionally would fail its own control.
    ///
    /// `resync` selects the cursor rule: `false` = the monotone advance alone (pre-#727), `true` =
    /// the advance plus [`resync_cursor`] with the walker's own predicate.
    fn fixture_run(col: &crate::collision::Collision, start_i: usize, resync: bool, verbose: bool)
        -> Run
    {
        const DT: f32 = 0.01;          // ~100 Hz controller frame
        const FRAMES: u32 = 14;        // 150 ms nav tick = 1 steer_target + 14 fast_steer_aim frames
        const TICKS: u32 = 200;
        const LOOK_AHEAD: f32 = 5.0;   // walker.rs `drive_walk`
        const LOCAL_REACH: f32 = 24.0; // walker.rs `drive_walk`
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
        let (mut x_min, mut x_max) = (p[0], p[0]);
        for tick in 0..TICKS {
            let before = p;
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
                path_i = resync_cursor(&HAIRPIN, path_i, p, |a, b| {
                    col.carrot_los_clear(a, b, clearance) && col.ground_continuous(a, b)
                });
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
            // A unit wish_dir driven at RUN_SPEED for the whole frame — the controller does NOT slow
            // down for a near carrot, which is why an aim inside one frame's travel
            // (44 * 0.01 = 0.44 u) is overshot rather than reached.
            let step = |p: &mut [f32; 3], aim: [f32; 3], x_min: &mut f32, x_max: &mut f32| {
                let (dx, dy) = (aim[0] - p[0], aim[1] - p[1]);
                let d = (dx * dx + dy * dy).sqrt();
                if d <= 1e-3 { return; }
                p[0] += dx / d * eqoxide_core::physics::RUN_SPEED * DT;
                p[1] += dy / d * eqoxide_core::physics::RUN_SPEED * DT;
                if let Some(fz) = col.ground_below(p[0], p[1], p[2] + 4.0, 40.0) { p[2] = fz; }
                *x_min = x_min.min(p[0]);
                *x_max = x_max.max(p[0]);
            };
            // ONE `steer_target` per nav tick (the 150 ms coarse tick, with the LOS clamp)…
            let aim = steer_target(&HAIRPIN, path_i, &local, &mut local_i, p, LOOK_AHEAD, coarse, &los);
            step(&mut p, aim, &mut x_min, &mut x_max);
            // …then the ~10 ms fast loop, which is plain pursuit along the FINE path with no LOS
            // clamp (`apply_fast_steering`, #685) and does nothing at all without a fine path.
            for _ in 0..FRAMES {
                let aim = if local.is_empty() { None } else {
                    fast_steer_aim(&local, &mut local_i, p, LOOK_AHEAD, |_, _| true).map(|(w, _)| {
                        [p[0] + w[0], p[1] + w[1], p[2]]
                    })
                };
                match aim {
                    Some(a) => step(&mut p, a, &mut x_min, &mut x_max),
                    None => break,
                }
            }
            if verbose {
                println!("  t{tick:<3} cursor {path_i} local.len {:<3} moved {:.3} u  pos ({:.3},{:.3})",
                    local.len(), (p[0] - before[0]).hypot(p[1] - before[1]), p[0], p[1]);
            }
            let net = (p[0] - LANDED[0]).hypot(p[1] - LANDED[1]);
            if (p[0] - goal[0]).hypot(p[1] - goal[1]) <= 3.0 {
                return Run { arrived: true, ticks: tick + 1, net, x_min, x_max };
            }
        }
        Run { arrived: false, ticks: TICKS, net: (p[0] - LANDED[0]).hypot(p[1] - LANDED[1]), x_min, x_max }
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

        let local_goal = carrot_along(&HAIRPIN, STALE_I, LANDED, 24.0).unwrap();
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
    /// enters a limit cycle and stays in it for the whole run: **0.02 u net displacement over 200 nav
    /// ticks** (30 s of simulated time). With the resync it walks the fixture out in **5** nav ticks.
    ///
    /// ⚠️ **Correction (#727 round 4).** Through round 3 this test was named
    /// `..._wedges_the_walker_...` and its doc said "pre-#727 the character never leaves the spot".
    /// **That was a claim about the walker made by an instrument that does not contain the walker.**
    /// [`fixture_run`] has no stall detector, no `NAV_STUCK_TICKS` backoff and no re-plan — the exact
    /// machinery that decides whether a limit cycle is a wedge or a hiccup. The round-3 reviewer
    /// drove the **production** `drive_walk` + `apply_fast_steering` loop on this same fixture with
    /// the resync mutated out and measured the walker sitting in this cycle for ~22 nav ticks
    /// (~3.3 s), then escaping via its own backoff + re-plan and **arriving** at t27. So on this
    /// featureless floor the pre-#727 cost is a wasted re-plan lap, not a permanent stop.
    ///
    /// **What the defect is, then.** The steering loop having no escaping trajectory is the
    /// mechanism; whether that is terminal is decided outside this sim, by whether the re-plan
    /// reproduces the state. Live on qcat it did: #673 records `blocked` / `walker_stalled` at
    /// `[-534.4, 144.4, -6.0]` on **6 of 8** attempts, and `walker.rs` only emits `walker_stalled`
    /// after `nav_repaths` reaches 8 — i.e. eight backoff-and-re-plan attempts ran and none escaped.
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

        let fixed = fixture_run(&col, STALE_I, true, false);
        assert!(fixed.arrived, "the resync must get the character to the goal; it moved {:.2} u", fixed.net);
        assert!(fixed.ticks <= 20, "arrival took {} nav ticks, expected a handful", fixed.ticks);
    }

    /// The control for the test above: same harness, same floor, same route, cursor NOT stale and the
    /// fix NOT applied. It arrives. A harness that wedges whatever you feed it proves nothing, so
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
    /// ⚠️ **Correction (#727 round 4) — the corroboration claim is WITHDRAWN.** Through round 3 this
    /// test was named `..._oscillates_in_the_band_the_live_capture_recorded` and asserted
    /// `|x_min − (−534.73)| < 0.05` against the live capture, with the doc claiming "the sim was not
    /// fitted to those numbers … the agreement is evidence that it reproduces the live defect rather
    /// than a similar-looking one". The round-3 reviewer showed that was an **identity, not
    /// evidence**:
    ///
    /// ```text
    /// LANDED[0]                  = -534.285_583   <- the capture's east end: a harness INPUT
    /// RUN_SPEED * 0.01           =    0.440_000   <- a code constant
    /// LANDED[0] - RUN_SPEED*0.01 = -534.725_586   <- what the assertion was matching
    /// ```
    ///
    /// Any harness seeded at `LANDED` that aims west and steps a full frame produces that number,
    /// whatever the mechanism, so the assertion discriminated "oscillates at the start" from "moves"
    /// and nothing finer. Two further corrections on the same point: the round-4 harness (one
    /// `steer_target` + 14 [`fast_steer_aim`] frames, one tick of planner latency) puts `x_min` at
    /// **-535.185**, so even the identity no longer holds; and the band is **not** reproduced — the
    /// reviewer's production `drive_walk` loop oscillates over roughly `[-536.5, -533.0]` (~2.6 u)
    /// against this sim's 1.32 u, so the sim's width is a property of the harness, not of the defect.
    /// The capture's −534.73 lies inside this sim's band and is *consistent* with an overshoot cycle;
    /// it is **not** independent evidence of one.
    ///
    /// Measured on this branch: `x ∈ [-535.185, -533.865]`, span 1.32 u = 3 frames of travel; at
    /// nav-tick boundaries the body cycles through 3 points. The assertions below pin boundedness and
    /// the eastward limit, and claim nothing about the live capture.
    #[test]
    fn the_simulated_stall_is_a_bounded_overshoot_cycle_a_few_frames_wide() {
        const FRAME: f32 = eqoxide_core::physics::RUN_SPEED * 0.01;
        let run = fixture_run(&fixture_floor(), STALE_I, false, false);
        assert!(run.x_max - run.x_min < 5.0 * FRAME,
            "the excursion must stay within a few frames of travel — a wider band would be drift, \
             not a limit cycle; it was {:.3} u", run.x_max - run.x_min);
        assert!(run.x_max <= LANDED[0] + 2.0 * FRAME,
            "the character must never get materially EAST of where it landed — that would be travel, \
             not an orbit; it reached {:.3} against a landing of {:.3}", run.x_max, LANDED[0]);
        // And it must not creep WEST along the route either: a limit cycle that slowly made ground
        // would be slow progress, which is a different (and much less serious) defect.
        assert!(LANDED[0] - run.x_min < 5.0 * FRAME,
            "the cycle drifted {:.3} u west — that is progress, not a stall", LANDED[0] - run.x_min);
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
    /// ⚠️ **Correction (#727 round 2).** This doc used to open *"Why the `CURSOR_STALE_DIST` guard is
    /// load-bearing"*, which read as pinning the constant. It does not: the round-1 review mutated
    /// `CURSOR_STALE_DIST` to both **2.0** and **16.0** and this test survived both (1.5 u is inside
    /// either guard). What it kills is REMOVING the guard entirely. The constant is unpinned over at
    /// least [2, 16] and is a judgement call, not a measurement — see the constant's own doc for the
    /// one thing about it that *is* measured (the `<` boundary).
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

    /// **THE property test.** For a dense sweep of positions and every starting cursor, the returned
    /// index must be (i) a valid forward segment start, (ii) never FURTHER from the body than the
    /// cursor it was handed, and (iii) once the cursor is stale, no admissible forward segment
    /// (inside [`CURSOR_RESYNC_MAX_HOP`], predicate clear) may be strictly nearer than the one
    /// chosen. Properties (ii) and (iii) are stated as OUTCOMES and checked against an independent
    /// scan — not by re-implementing the function.
    ///
    /// ⚠️ **Correction (#727 round 2).** The round-1 version of this test asserted only `i >= start_i`
    /// and `i + 1 < len`. The round-1 review pointed out that **both are true of the identity
    /// function**, so it passed under the `resync_cursor ≡ identity` mutation and pinned nothing
    /// about the fix. The retracted assertions are still here — they are correct, just not
    /// sufficient — with (ii)/(iii) added, plus a premise counter so the sweep cannot go green by
    /// never exercising a resync at all.
    ///
    /// **Mutation-checked by execution (#727 round 2):** with `resync_cursor` replaced by the
    /// identity function this test now FAILS (it panics on the premise counter at
    /// `moved = 0`), together with 9 others. The tests that still pass under that mutant are exactly
    /// the ones whose assertion IS "the cursor does not move" — `resync_never_moves_the_cursor_
    /// backwards`, `resync_never_jumps_across_blocked_geometry`, `resync_is_inert_on_degenerate_
    /// paths`, `an_on_route_walker_is_left_alone_...`, `a_walker_cutting_a_tight_switchback_...`,
    /// `a_resync_must_not_cross_a_wall_...`. An identity mutant satisfying a "must not move" test is
    /// not a gap in the test; the movement claims are the ones that had to be pinned, and are.
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
        // ⚠️ **Correction (round 3).** Through round 2 the far leg here sat at n = 60, putting the
        // body 30 u from segment 0 *and* 30 u from segment 2. Segment 2 was then refused by the
        // strict `d_sq < best_sq` tie, not by the band at all, so deleting `&& d_sq <= hop_sq`
        // survived this test — it looked sensitive only because of its own premise line. The far leg
        // is now at n = 59 so segment 2 is strictly NEARER than segment 0 and would be adopted on
        // proximity alone; the band is the only thing that can refuse it. Retracted text: "30 u from
        // segment 0 and 30 u from segment 2".
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
    /// ⚠️ **Correction (round 3) — read the name literally.** Through round 2 this was called
    /// `hairpin_wedges` and its results were reported as walker outcomes, on the stated grounds that
    /// 24 u is "the walker's own carrot reach". It is not: `drive_walk` steers with
    /// `LOOK_AHEAD = 5.0`, and 24 u is only the goal it hands the fine planner. A body that steps
    /// straight at `local_goal` is therefore **not the walker's motion**, and the round-2 review was
    /// right to reject arrival claims measured this way. What this loop does measure — soundly — is
    /// whether the CURSOR/CARROT arithmetic can reach a fixed point, because that is pure function
    /// composition and does not depend on how the body is propelled. The tables below are kept for
    /// that, and only that. Arrival is measured instead by
    /// `the_stale_cursor_leaves_the_steering_loop_no_escaping_trajectory_and_the_resync_clears_it`,
    /// which drives the production [`steer_target`] and [`fast_steer_aim`] at `LOOK_AHEAD` (and which
    /// carries its own disclosure that it models the steering loop, not the whole walker).
    ///
    /// Deliberately NOT a physics sim: no collision, no controller, so the reachability predicate is
    /// vacuously clear. That isolates the one thing under test.
    fn hairpin_carrot_stops_leading(sep: f32, start: [f32; 3], mut cursor: usize) -> bool {
        const DT: f32 = 0.15;          // the nav tick
        // `drive_walk`'s `local_goal` reach — the fine planner's destination, NOT a steering carrot.
        const LOCAL_REACH: f32 = 24.0;
        let route = hairpin_route(sep);
        let goal = *route.last().unwrap();
        let mut p = start;
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
            cursor = resync_cursor(&route, cursor, p, always_clear);
            let carrot = carrot_along(&route, cursor, p, LOCAL_REACH).unwrap_or(goal);
            let d = [carrot[0] - p[0], carrot[1] - p[1]];
            let len = (d[0] * d[0] + d[1] * d[1]).sqrt();
            if len > 1e-4 {
                let step = (eqoxide_core::physics::RUN_SPEED * DT).min(len);
                p[0] += d[0] / len * step;
                p[1] += d[1] / len * step;
            }
            if (p[0] - goal[0]).hypot(p[1] - goal[1]) <= 4.0 { return false; }
        }
        true
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
    /// Both numbers were measured on this branch by flipping that single token and re-running; the
    /// same flip takes the 8 u column of
    /// [`the_resync_clears_the_carrot_pinning_above_the_guard_and_is_inert_below_it`] from 0/288 to
    /// 33/288. Mutation check: flip it back to `<=` and this test goes RED.
    ///
    /// ⚠️ **Correction (round 3).** These counts are of *carrot pinning*, not of a walker failing to
    /// arrive — see [`hairpin_carrot_stops_leading`]. Round 2 reported them as wedged walkers.
    #[test]
    fn the_deadlock_fixed_point_exactly_on_the_guard_boundary_is_resynced() {
        let mut pinned = Vec::new();
        for k in 0..8 {
            let y = 6.25 + 0.25 * k as f32;
            if hairpin_carrot_stops_leading(8.0, [40.0, y, 0.0], 5) { pinned.push(y); }
        }
        assert!(pinned.is_empty(),
            "the guard-boundary band still deadlocks at offsets {pinned:?} — the fixed point at \
             exactly CURSOR_STALE_DIST must be OUTSIDE the guard");
    }

    /// **MEASURED (#727 round 2, re-labelled round 3): where the fix stops.** The resync is inert
    /// below [`CURSOR_STALE_DIST`] by construction, so a hairpin whose legs are closer together than
    /// the guard still forms the #673 cycle. This pins both halves of that honestly: **zero** pinned
    /// starts from 8 u separation (the guard itself) up, and a **total** wipe-out below it.
    ///
    /// Measured on this branch (`--nocapture` prints the table):
    ///
    /// ```text
    /// sep  4 u: 144/144 pinned     sep  9 u: 0/324
    /// sep  6 u: 216/216 pinned     sep 10 u: 0/360
    /// sep  7 u: 252/252 pinned     sep 12 u: 0/432
    /// sep  8 u:   0/288
    /// ```
    ///
    /// ⚠️ **Correction (round 3).** The column above counted *carrot pinning* all along, but round 2
    /// labelled it "wedged" and read it as walker arrival. It is not a walker outcome — the loop it
    /// comes from steps straight at `local_goal`, which is not how `drive_walk` moves. Retracted
    /// wording: "**zero** wedged starts … a **total** wipe-out". The counts themselves are unchanged
    /// and were re-run at round 3; only the claim they support is narrower. The round-1 review's own
    /// 133/1649 figure was measured on a comparable 24 u-step model and the reviewer has since
    /// retracted it on the same grounds.
    ///
    /// So the honest claim is *carrot pinning is cleared completely at and above the guard, and the
    /// fix is inert below it* — NOT "fixes the #673 deadlock". The residual `<= 7 u` class is a real,
    /// open defect (a distance guard cannot see a cycle whose whole geometry fits inside the guard);
    /// it is filed as its own issue and described in the PR body, not tolerated here by accident.
    #[test]
    fn the_resync_clears_the_carrot_pinning_above_the_guard_and_is_inert_below_it() {
        let count = |sep: f32| {
            let (mut pinned, mut total) = (0usize, 0usize);
            let mut yi = 1;
            while yi as f32 * 0.25 <= sep {
                let y = yi as f32 * 0.25;
                for xi in 1..10 {
                    let x = xi as f32 * 8.0;
                    total += 1;
                    if hairpin_carrot_stops_leading(sep, [x, y, 0.0], xi) { pinned += 1; }
                }
                yi += 1;
            }
            (pinned, total)
        };
        for sep in [4.0f32, 6.0, 7.0, 8.0, 9.0, 10.0, 12.0] {
            let (w, t) = count(sep);
            println!("hairpin leg separation {sep:>4} u: carrot pinned {w}/{t}");
            if sep >= 8.0 {
                assert_eq!(w, 0, "the fix must be complete at {sep} u separation, got {w}/{t} wedged");
            }
        }
        let (below, _) = count(7.0);
        assert!(below > 0,
            "premise: the sub-guard residual must still reproduce, or this test is not measuring \
             the boundary it claims to");
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
