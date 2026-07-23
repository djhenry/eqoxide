//! **The #639 "unvalidated goal-append" claim, on HAND-AUTHORED geometry — no assets, no network.**
//!
//! #639 is the sibling of #630 (PR #635): every INTERMEDIATE walk edge in a planned route passes
//! `Collision::walk_profile_ok`, but the FINAL waypoint is APPENDED — the reconstruction snaps the
//! last waypoint from the reached goal-cell centre (or, for a same-cell walk, the start) to the
//! EXACT goal — and that appended hop skipped the walk-edge predicate every other edge gets. Measured
//! in `permafrost`, routes ended with final hops up to grade **6.61** against `MAX_WALK_GRADE = 1.2`:
//! a near-vertical final face the controller cannot climb, reported as a COMPLETE route — the #630
//! lie reintroduced at exactly the point the agent is most likely to believe it has arrived.
//!
//! The fix validates the appended hop with the SAME predicate and, when it fails, rejects the route
//! as `Unreachable(GoalNotWalkable)` (→ `nav_reason: goal_not_walkable` — re-aim, don't retry),
//! exactly as an intermediate edge that fails the predicate is dropped.
//!
//! These tests are checked BOTH ways: they go RED if the goal-append guard is reverted (the route
//! comes back complete), and the over-tightening guards confirm a WALKABLE final hop still routes.

mod synthetic_scenes;

use eqoxide::nav::collision::MAX_WALK_GRADE;
use eqoxide::traversability::PLAYER_BODY;
use synthetic_scenes as scenes;

/// The scene's coarse plan cell.
const CELL: f32 = 8.0;
/// `walk_profile_ok`'s downward floor-probe range, matching `astar`'s local `MAX_STEP_DOWN`.
const PROBE_DOWN: f32 = 60.0;

/// **THE REGRESSION: a route whose only un-walkable segment is the appended final hop is REFUSED.**
///
/// In [`scenes::flat_run_into_a_final_goal_face`] A\* reaches the goal cell across flat low ground —
/// every intermediate hop is a legal flat walk — but the exact goal sits atop a 12.8 u face on a
/// sub-cell patch no cell centre touches. Before #639 the planner snapped the final waypoint onto
/// that patch and returned a COMPLETE route; the final hop (grade ≈ 12.8/8 = 1.6, well over
/// `MAX_WALK_GRADE`) was never checked. After #639 the goal is honestly un-walkable-to.
#[test]
fn a_route_whose_final_appended_hop_is_unwalkable_is_refused() {
    let col = scenes::flat_run_into_a_final_goal_face();
    let start = [-40.0, 0.0, scenes::LOW_Z];
    let goal = scenes::GOAL_ATOP_FACE;

    // Fixture validity 1: the start stands on real low ground.
    assert!(col.nearest_floor(start[0], start[1], start[2], 4.0, 20.0).is_some(),
        "fixture: the start must stand on the low ground");
    // Fixture validity 2: the goal has a REAL floor (the raised patch) — this is NOT the off-mesh
    // `goal_not_walkable` case; the goal is a perfectly good standable spot, it just can't be
    // WALKED onto. If the patch never registered, the refusal below would be trivially true.
    let gf = col.nearest_floor(goal[0], goal[1], goal[2], 4.0, 8.0);
    assert!(gf.is_some_and(|z| (z - scenes::HIGH_Z).abs() < 0.5),
        "fixture: the goal must stand on the raised patch at HIGH_Z, got {gf:?}");
    // Fixture validity 3: and the final approach really is beyond the walk envelope — i.e. this IS
    // an un-walkable hop, so a fix that refuses it is refusing something real. (The penultimate
    // waypoint is a low-ground cell centre ≤ ~11 u from the goal; the rise is the whole 12.8 u.)
    assert!(scenes::FACE_RISE / CELL > MAX_WALK_GRADE,
        "fixture: 12.8 u over one 8 u cell is grade {:.2} > {MAX_WALK_GRADE}", scenes::FACE_RISE / CELL);

    let path = col.find_path(start, goal, PLAYER_BODY.radius, &[], /*allow_partial=*/ false);
    assert!(path.is_none(),
        "#639: the planner must NOT return a complete route whose APPENDED final hop climbs a 12.8 u \
         face the controller cannot walk. Got a {:?}-waypoint route ending {:?}",
        path.as_ref().map(|p| p.len()), path.as_ref().and_then(|p| p.last()));
}

/// **THE ISOLATION CONTROL: the SAME goal XY on the low ground still routes.**
///
/// Same scene, same approach, same penultimate cell — only the goal's TIER differs
/// ([`scenes::GOAL_BESIDE_FACE`] stands on the low ground under the patch). Its final hop is flat, so
/// it must route. This is what proves the refusal above is about the APPENDED FINAL HOP and not about
/// the approach, the scene being unroutable, or the goal XY: hold everything fixed but the goal's z.
#[test]
fn the_same_goal_xy_on_the_low_ground_still_routes() {
    let col = scenes::flat_run_into_a_final_goal_face();
    let start = [-40.0, 0.0, scenes::LOW_Z];
    let path = col.find_path(start, scenes::GOAL_BESIDE_FACE, PLAYER_BODY.radius, &[], false);
    assert!(path.is_some(),
        "over-tightening guard: the same goal XY on the low ground (a flat final hop) MUST still \
         route — otherwise the refusal is not specific to the un-walkable appended hop");
}

/// **AND THE OVER-TIGHTENING GUARD ON A REAL RAMP: a walkable final hop up 12.8 u still routes.**
///
/// The #630 control scene joins the two levels with a grade-0.8 ramp. A goal ON the plateau is
/// reached by walking UP the ramp, and its final hop is a legal slope — the goal-append check must
/// NOT refuse it. (Mirror image of the regression: same rise, walkable this time.)
#[test]
fn a_goal_reached_up_a_walkable_ramp_still_routes() {
    let col = scenes::flat_run_into_a_walkable_ramp();
    let start = [-40.0, 0.0, scenes::LOW_Z];
    let goal = [40.0, 0.0, scenes::HIGH_Z];
    let path = col.find_path(start, goal, PLAYER_BODY.radius, &[], false);
    let path = path.expect("a goal atop a WALKABLE ramp must route — the final hop is a legal slope");
    let last = *path.last().unwrap();
    assert!((last[0] - goal[0]).hypot(last[1] - goal[1]) < CELL,
        "and the route must actually REACH the goal, not stop short: ended {last:?}");
}

/// **THE UNIVERSAL: every consecutive waypoint pair of ANY returned route — INCLUDING THE LAST —
/// satisfies the walk-edge predicate.**
///
/// This is the property the goal-append gap violated. Swept over the three dry scenes and a fan of
/// goals (including [`scenes::GOAL_ATOP_FACE`], which on the unfixed planner returns a route whose
/// LAST pair fails — that is the mutation this property is checked against, so it is not vacuous).
/// Any route the planner returns must be walkable end to end, by the same `walk_profile_ok` gated on
/// `rise > step_up` that `astar` applies to intermediate edges.
#[test]
fn every_returned_route_is_walkable_end_to_end_including_the_final_hop() {
    let step_up = PLAYER_BODY.step_up;
    let scenes_and_goals: Vec<(&str, eqoxide::nav::collision::Collision, Vec<[f32; 3]>)> = vec![
        ("final_goal_face", scenes::flat_run_into_a_final_goal_face(), vec![
            scenes::GOAL_ATOP_FACE,          // the #639 case (unfixed: a route with a bad last hop)
            scenes::GOAL_BESIDE_FACE,        // the walkable control
            [40.0, 24.0, scenes::LOW_Z],
            [-8.0, -30.0, scenes::LOW_Z],
        ]),
        ("walkable_ramp", scenes::flat_run_into_a_walkable_ramp(), vec![
            [40.0, 0.0, scenes::HIGH_Z],
            [48.0, 30.0, scenes::HIGH_Z],
            [-40.0, -20.0, scenes::LOW_Z],
        ]),
        ("vertical_face", scenes::flat_run_into_a_vertical_face(), vec![
            [40.0, 0.0, scenes::HIGH_Z],     // (unreachable — no route, skipped)
            [-56.0, -32.0, scenes::LOW_Z],
        ]),
    ];

    let mut routes_checked = 0usize;
    for (name, col, goals) in &scenes_and_goals {
        for &goal in goals {
            let start = [-40.0, 0.0, scenes::LOW_Z];
            let Some(route) = col.find_path(start, goal, PLAYER_BODY.radius, &[], false) else { continue };
            routes_checked += 1;
            for pair in route.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                // Mirror `astar`'s gate exactly: only a rise past the discrete step-up can hide a
                // face the average grade launders. (Downhill / flat / stepped hops are always OK.)
                if b[2] - a[2] > step_up {
                    assert!(
                        col.walk_profile_ok([a[0], a[1]], a[2], [b[0], b[1]], b[2], PROBE_DOWN),
                        "#639 universal: scene {name}, goal {goal:?} — the route hop {a:?} → {b:?} \
                         (rise {:.1} u) fails the walk-edge predicate every edge must satisfy, \
                         INCLUDING the final one", b[2] - a[2]);
                }
            }
        }
    }
    assert!(routes_checked >= 5,
        "the sweep must actually exercise routes (checked {routes_checked}) or it proves nothing");
}

/// **THE START/SAME-CELL SIBLING: a goal ATOP a face inside the start's OWN cell is refused too.**
///
/// The same-cell fast path (`astar`'s `(sc,sr) == (gc,gr)` shortcut) is a second append site — it
/// emits `[start, goal]` directly. #639 validates that hop as well. Here start and goal share one 8 u
/// cell but the goal sits atop the raised patch, so the straight walk onto it is un-walkable.
#[test]
fn a_same_cell_goal_atop_a_face_is_refused() {
    let col = scenes::flat_run_into_a_final_goal_face();
    // Start on the low ground UNDER the patch, a unit from the goal — same 8 u cell (col 9, spanning
    // east [8,16)) as the patch-top goal, standing on the low floor beneath the raised patch.
    let start = [9.0, 0.0, scenes::LOW_Z];
    let goal = scenes::GOAL_ATOP_FACE;
    // Confirm they really are the same coarse cell (else this tests the reconstruction path, not the
    // same-cell shortcut). The grid origin is the scene min corner (east −64), 8 u cells:
    let cell_of = |e: f32| ((e - (-64.0)) / CELL) as i32;
    assert_eq!(cell_of(start[0]), cell_of(goal[0]),
        "fixture: start and goal must be in the same coarse column for the same-cell path");

    let path = col.find_path(start, goal, PLAYER_BODY.radius, &[], false);
    assert!(path.is_none(),
        "#639 same-cell: a straight walk onto a goal atop a 12.8 u face inside the start's own cell \
         must be refused too, not appended unchecked. Got {:?}", path.map(|p| p.len()));
}
