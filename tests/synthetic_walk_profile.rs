//! **The #630 "laundered grade" claim, on HAND-AUTHORED geometry — no assets, no network.**
//!
//! #630 is the planner defect four separate nav wedges converged on (#617, #309, #329, #482): a
//! walk edge's grade was computed as the endpoint rise divided by the WHOLE hop, so a flat run
//! ending in a near-vertical 12.8 u face averaged to grade `12.8 / 11.31 = 1.13` on the DIAGONAL
//! and slipped under `MAX_WALK_GRADE = 1.2` — while the very same face taken orthogonally
//! (`12.8 / 8 = 1.6`) was correctly refused. The controller's real capability is a 2 u step plus a
//! walkable slope; it cannot climb 12.8 u of vertical rock, so the walker stalled on a route the
//! planner had promised.
//!
//! The fix (`Collision::walk_profile_ok`) had **no test of its own anywhere in the tree** — the
//! issue asked for "a synthetic mini-zone with a ~12.8 u vertical face reached on a diagonal cell
//! hop", and this is it (#659).

mod synthetic_scenes;

use eqoxide::nav::collision::MAX_WALK_GRADE;
use eqoxide::traversability::PLAYER_BODY;
use synthetic_scenes as scenes;

/// The coarse plan cell — the tier where the diagonal laundering happens.
const CELL: f32 = 8.0;

/// **THE UNIT CLAIM: the profile check refuses the face the AVERAGE grade accepts.**
///
/// Stated as a two-sided assertion on purpose, so the test cannot pass because the numbers drifted
/// out of the interesting range: it FIRST proves the diagonal hop's whole-hop average grade is
/// legal (i.e. the pre-#630 test really would have accepted this edge), and only THEN asserts the
/// profile check rejects it. Both halves have to hold for the fixture to be the #630 case at all.
#[test]
fn the_diagonal_hop_that_launders_a_vertical_face_is_refused_by_the_profile_check() {
    let col = scenes::flat_run_into_a_vertical_face();
    // A diagonal 8 u cell hop between two REAL coarse-grid cell centres (the grid origin is the
    // scene's min corner, so centres fall on …, −4, +4, …): run = 8√2 ≈ 11.31.
    let a = [-4.0f32, -4.0];
    let b = [4.0f32, 4.0];
    let run = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
    let rise = scenes::HIGH_Z - scenes::LOW_Z;

    assert!(rise / run < MAX_WALK_GRADE,
        "fixture: the whole-hop AVERAGE grade must be LEGAL ({:.3} < {MAX_WALK_GRADE}) or this is \
         not the #630 case — the average is what launders the face", rise / run);
    assert!(rise / CELL > MAX_WALK_GRADE,
        "fixture: and the same face must be ILLEGAL orthogonally ({:.3} > {MAX_WALK_GRADE}) — the \
         diagonal's longer run is the whole mechanism", rise / CELL);
    assert!(rise > 4.0 * PLAYER_BODY.step_up,
        "fixture: the face must be far beyond the controller's real step-up");

    assert!(!col.walk_profile_ok(a, scenes::LOW_Z, b, scenes::HIGH_Z, 20.0),
        "#630: a {rise} u vertical face behind a flat run must be REJECTED by the maximum-local-\
         rise check, however long the hop that averages over it");
}

/// **AND IT MUST NOT OVER-TIGHTEN: a genuinely walkable ramp of the same rise stays accepted.**
///
/// Rejecting a real slope would turn a wedge into a `no_path` — a different regression, and the
/// one the #630 fix was explicitly designed around. Same 12.8 u of altitude, spread over a 16 u
/// run (grade 0.8), and the profile check must let it through.
#[test]
fn a_walkable_ramp_of_the_same_rise_is_still_accepted() {
    let col = scenes::flat_run_into_a_walkable_ramp();
    let a = [-16.0f32, 0.0];
    let b = [scenes::FACE_EAST, 0.0];
    assert!(col.walk_profile_ok(a, scenes::LOW_Z, b, scenes::HIGH_Z, 20.0),
        "#630 over-tightening guard: a grade-0.8 ramp carrying the same {} u of rise must remain \
         walkable", scenes::FACE_RISE);
}

/// **THE END-TO-END CLAIM: A\\* does not route up the face.**
///
/// The face spans the entire north extent of the scene and both ends are walled, so there is no
/// ramp, stair, gap, or detour: **every** route from the low ground to the plateau must cross this
/// one face. A planner that returns a route to the plateau has accepted it. (With the #630 check
/// removed, this scene does yield such a route — that is the mutation this test is checked
/// against, and it is why the assertion is not vacuous.)
#[test]
fn astar_finds_no_route_up_a_near_vertical_face() {
    let col = scenes::flat_run_into_a_vertical_face();
    let start = [-40.0, 0.0, scenes::LOW_Z];
    let goal = [40.0, 0.0, scenes::HIGH_Z];
    // Both ends must be genuine standable ground, or "no route" would be trivially true.
    assert!(col.nearest_floor(start[0], start[1], start[2], 4.0, 20.0).is_some(),
        "fixture: the start must stand on the low ground");
    assert!(col.nearest_floor(goal[0], goal[1], goal[2], 4.0, 20.0).is_some(),
        "fixture: the goal must stand on the plateau");

    let path = col.find_path(start, goal, PLAYER_BODY.radius, &[], false);
    assert!(path.is_none(),
        "#630: the planner must NOT commit a route the controller cannot walk — the only way from \
         the low ground to the plateau in this scene is a {} u vertical face. Got {:?}",
        scenes::FACE_RISE, path.map(|p| p.len()));
}

/// **AND THE SCENE IS NOT SIMPLY UNROUTABLE**: the same planner, in the same scene, finds a route
/// between two points on the low ground, and (in the ramp control) all the way onto the plateau.
///
/// Without this, `astar_finds_no_route_up_a_near_vertical_face` would pass just as well against a
/// broken planner, an empty grid, or a fixture whose floors never registered at all — the trap
/// that let two earlier synthetic fixtures ship green while proving nothing.
#[test]
fn the_same_planner_and_scenes_do_route_where_a_route_exists() {
    let face = scenes::flat_run_into_a_vertical_face();
    let along_the_low_ground =
        face.find_path([-56.0, -32.0, scenes::LOW_Z], [-16.0, 32.0, scenes::LOW_Z],
                       PLAYER_BODY.radius, &[], false);
    assert!(along_the_low_ground.is_some(),
        "the face scene must still route across its own low ground — otherwise the no-route result \
         above proves nothing about the face");

    let ramp = scenes::flat_run_into_a_walkable_ramp();
    let up_the_ramp = ramp.find_path([-40.0, 0.0, scenes::LOW_Z], [40.0, 0.0, scenes::HIGH_Z],
                                     PLAYER_BODY.radius, &[], false);
    assert!(up_the_ramp.is_some(),
        "and the identical two levels joined by a WALKABLE ramp must route to the plateau — this \
         is what proves the rejection above is about the FACE and not about the two levels");
}
