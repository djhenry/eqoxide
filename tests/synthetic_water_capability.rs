//! **The water/controller capability claims, on HAND-AUTHORED geometry — no assets, no network.**
//!
//! Companion to `tests/water_capability.rs`, which pins the same physics against BAKED zone
//! geometry and is `#[ignore]`d because those assets live on a LAN-only server (#357/#654/#657).
//! Those tests stay exactly as they are. This file adds a second layer that actually RUNS: every
//! test here builds its world in code (see `tests/synthetic_scenes/mod.rs`) and runs in a plain
//! `cargo test --workspace` on a bare runner.
//!
//! **What is pinned here is the PHYSICAL CLAIM, not a shipped zone's coordinates.** The
//! asset-gated tests assert things like "the qcat pocket surface is −55.978"; these assert "a
//! swimmer is lifted to the DESTINATION column's swim plane" and (since #658) "the depenetration
//! push-out recovers an afloat swimmer AT ITS OWN DEPTH rather than mounting it onto a lid". Both
//! are worth having; neither replaces the other.

mod synthetic_scenes;

use eqoxide::movement::{CharacterController, PLAYER_RADIUS};
use eqoxide::nav::collision::Collision;
use eqoxide::traversability::PLAYER_BODY;
use eqoxide_ipc::MoveIntent;
use synthetic_scenes as scenes;

/// Mirrors `movement.rs`'s private `GROUND_ORIGIN` (probe origin above the feet) — that module
/// keeps it `const`-private, so the fixture checks below (which must ask the exact same question
/// the depenetration net asks) restate the value here rather than duplicate the whole predicate as
/// a public API surface just for a test. If `movement.rs`'s value ever changes, the corresponding
/// in-crate test (`movement::tests::an_afloat_body_with_no_floor_below_is_never_pushed_out_into_a_drift`)
/// changes with it and this one does not automatically follow — that drift risk is accepted in
/// exchange for not widening `movement`'s public surface for a test-only constant.
const GROUND_ORIGIN: f32 = 1.0;
/// Mirrors `movement.rs`'s private `GROUND_DEPTH` (ground-probe downward range). See `GROUND_ORIGIN` above.
const GROUND_DEPTH: f32 = 200.0;

/// The depenetration net's own "is this body embedded" question (`movement::is_embedded`,
/// private), restated here from its two PUBLIC halves (`Collision::footprint_clear`,
/// `Collision::ground_below`) so the fixture checks and the iteration-invariant assertion below ask
/// the identical question the net asks, not an approximation of it.
fn embedded(col: &Collision, p: [f32; 3]) -> bool {
    !col.footprint_clear(p[0], p[1], p[2], PLAYER_RADIUS, 8)
        || col.ground_below(p[0], p[1], p[2] + GROUND_ORIGIN, GROUND_DEPTH).is_none()
}

/// Drive the controller from `from` toward the XY of `to` for `secs`, with exactly the intent the
/// walker sends at a water waypoint: `want_swim`, a horizontal wish, and **no vertical wish** — so
/// every unit of rise observed here is buoyancy's, not a swim-up drive's. Identical to the helper
/// in `tests/water_capability.rs`, deliberately: the two layers must drive the controller the same
/// way for their results to be comparable.
fn swim_toward(col: &Collision, from: [f32; 3], to: [f32; 3], secs: f32) -> [f32; 3] {
    let mut c = CharacterController::new(from);
    let dt = 1.0 / 60.0;
    for _ in 0..((secs / dt) as usize) {
        let d = [to[0] - c.pos[0], to[1] - c.pos[1]];
        let l = (d[0] * d[0] + d[1] * d[1]).sqrt();
        let dir = if l > 0.2 { [d[0] / l, d[1] / l] } else { [0.0, 0.0] };
        c.step(MoveIntent { wish_dir: dir, wish_vspeed: 0.0, jump: false, want_swim: true,
                            speed: 44.0, climb: 0.0, hop: false }, dt, col);
    }
    c.pos
}

/// Drive a DOWNWARD swim wish (what `/v1/move/manual {"up":-1}` sends) and return the net descent.
fn try_to_sink(col: &Collision, from: [f32; 3], secs: f32) -> [f32; 3] {
    let mut c = CharacterController::new(from);
    let dt = 1.0 / 60.0;
    for _ in 0..((secs / dt) as usize) {
        c.step(MoveIntent { wish_dir: [0.0, 0.0], wish_vspeed: -44.0, jump: false, want_swim: true,
                            speed: 0.0, climb: 0.0, hop: false }, dt, col);
    }
    c.pos
}

// ───────────────────────────── scene 1: the lid mount ─────────────────────────────

/// **#649 FIXED, ON SYNTHETIC GEOMETRY: the push-out holds an afloat swimmer at its own depth
/// instead of mounting it onto the lid.**
///
/// A sealed flooded chamber whose ceiling quad sits **0.009 u above the waterline**. A swimmer
/// floating at the chamber's own swim plane swims into the sealed chamber's east wall (there is
/// nowhere else to go); `footprint_clear` fails there, which the controller's depenetration net
/// reads as embedded, and its ring push-out runs.
///
/// Before #658, that push-out hunted for the nearest FLOOR within `STEP_UP + GROUND_ORIGIN = 3.0`
/// u of the body regardless of medium — water-blind — found the lid 2.009 u up, and placed the
/// swimmer there: `on_ground = true`, DRY, buoyancy never firing again. Since #658 the net measures
/// the medium once, at the body's own position (`movement::Recovery::at_column`): an afloat body is
/// recovered **at its own depth** in any ring candidate whose column is still water, never onto a
/// floor. Here every candidate at the swim plane's height is still water (the lid is 2 u higher),
/// so the ring finds a nearby clear spot along the same wall and returns `Recovery::Afloat` at the
/// UNCHANGED z — the swimmer ends up nudged into a corner of the chamber, still floating at its own
/// swim plane, never touching the lid.
///
/// # How this compares to the baked-`qcat` twin, precisely
///
/// `tests/water_capability.rs`'s `qcat_pocket_swim_plane_strands_the_swimmer_on_the_tile_floor`
/// (renamed alongside #658) shows the identical push-out fix at the real coordinate — one frame
/// from the qcat swim plane now holds depth (`the_depenetration_push_out_holds_a_qcat_swimmer_at_
/// its_own_depth`) instead of mounting the tile floor as it did pre-fix. But driven for the full
/// 12 s toward the shaft, qcat's swimmer STILL ends up dry at −55.9687 — not through this
/// mechanism, but through a SECOND, independent one: the swimming step-up (the #191 haul-out
/// branch) climbs the same 2.009 u once the swimmer has drifted onto the tile floor's own
/// footprint. That residual live wedge is tracked separately as **#661**.
///
/// This synthetic chamber has no bank for that second mechanism to reach — it is sealed on all six
/// sides with nothing at swim-plane height but water and the wall it presses into (see
/// `synthetic_scenes`'s module doc for why the swimming step-up has zero coverage in this layer),
/// so this test is not expected to flip again when #661 is fixed.
///
/// The SUB-MECHANISM fixed here was confirmed identical to qcat's by mutation, not by resemblance:
/// removing only the push-out's UPWARD reach (`nearest_floor(e, n, p[2], 0.0, GROUND_DEPTH)` —
/// leaving depenetration otherwise intact) used to turn both this test and its qcat twin red for
/// the same reason; #658's fix is what makes both hold depth instead.
///
/// What this test does NOT pin is qcat's own coordinates — `POCKET_LID_Z` was copied from that file
/// (see its doc) purely so this chamber's geometry sits in the same band; the near-agreement of the
/// two numbers was always arithmetic, not evidence.
#[test]
fn a_swimmer_at_the_pocket_swim_plane_holds_its_own_depth_not_the_lid() {
    let col = scenes::sealed_pocket_with_lid();
    let start = [-20.0, 0.0, scenes::POCKET_SWIM_PLANE];
    assert!(col.in_water(start), "fixture: the start must be in water");
    assert!((col.water_surface(start).unwrap() - scenes::POCKET_SURFACE).abs() < 0.01,
        "fixture: pocket surface");

    let end = swim_toward(&col, start, [40.0, 0.0, scenes::POCKET_SWIM_PLANE], 6.0);

    assert!((end[2] - scenes::POCKET_SWIM_PLANE).abs() < 0.05,
        "#649 (fixed): an afloat body recovered by the depenetration push-out must hold ITS OWN \
         DEPTH ({:.3}) — the only thing wrong with its position was the horizontal overlap the ring \
         is already resolving, and moving it vertically too was the withdrawn defect; got {end:?}",
        scenes::POCKET_SWIM_PLANE);
    assert!(col.in_water(end),
        "#649 (fixed): and it must still be IN WATER — the push-out must never carry an afloat body \
         dry above the waterline (surface {:.3}); got {end:?}", scenes::POCKET_SURFACE);
    assert!((end[2] - scenes::POCKET_LID_Z).abs() > 1.0,
        "#649 (fixed): sanity — this must not have landed back on the OLD strand coordinate, the \
         lid at {:.3}; got {end:?}", scenes::POCKET_LID_Z);
}

/// **A DRY MOUNT ON THE LID, IF ONE EVER HAPPENS, IS ONE-WAY: nothing swims back down through it.**
///
/// This no longer builds on the test above: since #658, ordinary swimming in this chamber never
/// puts a character on the lid (see `a_swimmer_at_the_pocket_swim_plane_holds_its_own_depth_not_
/// the_lid`), so the position here is manually authored rather than reached by driving the
/// controller. What survives is the property that used to make the pre-#658 push-out mount
/// permanent — and would make any FUTURE dry mount on this lid equally permanent, whether from a
/// regression in #658, from #661's separate swimming-step-up mechanism reaching this height in some
/// other geometry, or from a GM teleport: `want_swim` only does anything when `in_water`, so a
/// downward swim wish from a DRY position moves nothing. The live #649 evidence was
/// `POST /v1/move/manual {"up":-1,"duration_ms":3000}` moving the character **0.00 u**; reproduced
/// here to a tenth of a unit.
#[test]
fn a_dry_mount_on_the_lid_is_one_way_a_downward_swim_wish_cannot_recover_it() {
    let col = scenes::sealed_pocket_with_lid();
    let mounted = [-20.0, 0.0, scenes::POCKET_LID_Z];
    assert!(!col.in_water(mounted), "fixture: the mounted position must be dry");

    let end = try_to_sink(&col, mounted, 3.0);
    assert!((end[2] - mounted[2]).abs() < 0.1,
        "#649: a 3 s full-strength downward swim wish must move the mounted character essentially \
         nowhere (the live report measured 0.00 u); got {end:?}, a change of {:+.3} u",
        end[2] - mounted[2]);
}

/// **THE CONTROL: remove ONLY the lid, and the character still never goes dry.**
///
/// Same chamber, same walls, same floor, same water volume and surface — no ceiling slab. The
/// character presses into the same wall at the same frames and the push-out fires identically.
///
/// # This control's own claim changed when #658 landed — read this before touching it
///
/// Before #658 the two scenes (with/without the lid) diverged sharply, because the water-blind
/// push-out always hunted for the NEARER floor regardless of medium: with the lid it found the
/// lid 2.009 u UP and mounted the swimmer dry; without it, the nearer floor was the chamber floor
/// 12 u DOWN, so the swimmer was slammed there instead and oscillated between the floor and roughly
/// 1 u above it every time it pressed the wall again — still submerged, but by the same defective
/// mechanism in its other direction.
///
/// Since #658 the push-out no longer hunts a floor for an afloat body at all when the ring
/// candidate is still water — it recovers the body AT ITS OWN DEPTH instead (`movement::Recovery::
/// Afloat`). So removing the lid no longer changes the outcome: measured here, this scene now ends
/// at essentially the SAME z as the with-lid scene above (-57.978, its own swim plane), not at the
/// chamber floor. The control's claim is unchanged in substance — **the character stays submerged
/// and never ends up above the waterline** — but the reason it holds is no longer "the nearer floor
/// happens to be below the surface too"; it is that the push-out stopped moving an afloat body
/// vertically at all.
#[test]
fn without_the_lid_the_same_pocket_never_strands_the_swimmer_above_the_waterline() {
    let col = scenes::sealed_pocket_without_lid();
    let start = [-20.0, 0.0, scenes::POCKET_SWIM_PLANE];
    let end = swim_toward(&col, start, [40.0, 0.0, scenes::POCKET_SWIM_PLANE], 6.0);

    // The controller's own water probe: feet first, then chest — a body resting on a pool floor
    // can have its FEET below the water volume's lower bound while it is fully submerged (#329).
    let body_wet = col.in_water(end) || col.in_water([end[0], end[1], end[2] + 3.0]);
    assert!(body_wet,
        "control: with no lid the character's BODY must still be submerged — got {end:?}");
    assert!(end[2] <= scenes::POCKET_SWIM_PLANE + 0.01,
        "control: and it must never rise above the swim plane {:.3}, let alone the waterline \
         {:.3} — got {end:?}", scenes::POCKET_SWIM_PLANE, scenes::POCKET_SURFACE);
}

// ─────────────────── scene 2: rise to the DESTINATION column ───────────────────

/// **A SWIMMER RISES TO THE DESTINATION COLUMN'S SURFACE, NOT ITS OWN (#648's withdrawn premise).**
///
/// PR #648 proposed a planner gate on the premise that *"a swimmer cannot rise more than
/// `haul_out_up` above the surface of the water it is in"*. That is false. The rise is not
/// performed in place: `movement.rs` recomputes `col.water_surface(water_at)` at the character's
/// OWN position every frame, so a swimmer that moves LATERALLY into a column with a higher surface
/// is floated to *that* column's swim plane by ordinary buoyancy. The source column's surface
/// bounds nothing.
///
/// Here that is a **+16 u** rise — 8× `haul_out_up` — in a scene containing no climbable geometry
/// at all, so buoyancy is the only thing that could have produced it. Any future cap on water-edge
/// rise must keep this green.
#[test]
fn a_swimmer_rises_to_the_destination_columns_surface_not_its_own() {
    let col = scenes::stepped_water_surfaces();
    let dest_plane = scenes::HIGH_SURFACE - PLAYER_BODY.float_depth;
    let src_plane = scenes::LOW_SURFACE - PLAYER_BODY.float_depth;

    for z in [src_plane, src_plane - 4.0, src_plane - 12.0] {
        let from = [-40.0, 0.0, z];
        assert!((col.water_surface(from).unwrap() - scenes::LOW_SURFACE).abs() < 0.01,
            "fixture: the start column's surface must be the LOW one");
        let end = swim_toward(&col, from, [40.0, 0.0, dest_plane], 8.0);

        assert!((end[2] - dest_plane).abs() < 0.05,
            "from z={z} the swimmer must settle on the DESTINATION column's swim plane \
             {dest_plane:.3}, got {end:?}. It rose {:+.2} u — far past the {} u `haul_out_up` \
             measured from its OWN surface ({:.3}). That is the premise #648 got wrong: buoyancy \
             re-reads the surface at the character's position every frame, so the rise happens at \
             the DESTINATION and the source column's surface bounds nothing.",
            end[2] - z, PLAYER_BODY.haul_out_up, scenes::LOW_SURFACE);
        assert!((end[0] - 40.0f32).abs() < 2.0 && end[1].abs() < 2.0,
            "and it must actually arrive at the destination XY, got {end:?}");
        assert!(end[2] - z > 4.0 * PLAYER_BODY.haul_out_up,
            "sanity: this case is only interesting because the rise vastly exceeds the haul-out \
             reach — got {:+.2} u", end[2] - z);
    }
}

/// **AND THE SAME CLAIM AS A PROPERTY OF THE WATER MAP ITSELF**, so a scene that quietly lost its
/// two-surface structure could not let the test above pass by accident.
#[test]
fn the_stepped_scene_really_has_two_surfaces_and_no_climbable_geometry() {
    let col = scenes::stepped_water_surfaces();
    let low = col.water_surface([-40.0, 0.0, -70.0]).expect("west column must be water");
    let high = col.water_surface([40.0, 0.0, -70.0]).expect("east column must be water");
    assert!((low - scenes::LOW_SURFACE).abs() < 0.01 && (high - scenes::HIGH_SURFACE).abs() < 0.01,
        "fixture: surfaces {low} / {high}");
    assert!(high - low >= 8.0 * PLAYER_BODY.haul_out_up,
        "fixture: the step must dwarf the haul-out reach");
    // Nothing to climb: the only floor anywhere in either column is the shared basin floor.
    for e in [-40.0f32, -8.0, 8.0, 40.0] {
        let floors = col.column_floors(e, 0.0, -10.0, 0.0, 200.0);
        assert_eq!(floors.len(), 1,
            "fixture: column at east={e} must contain exactly ONE floor (the basin) or the rise \
             could be a step-up rather than buoyancy; got {floors:?}");
        assert!((floors[0] - scenes::BASIN_FLOOR_Z).abs() < 0.01, "fixture: {floors:?}");
    }
}

// ─────────────── scene 4: an afloat body with no floor within reach (#664) ───────────────

/// **#664 / #649 REVIEW FINDING 1 — AN AFLOAT BODY WITH NO FLOOR BELOW MUST NEVER DRIFT.**
///
/// This is the ITERATION invariant a one-shot harness structurally cannot see (#664): the withdrawn
/// first cut of the #649 fix answered a clear-footprint-but-no-floor-below body with an `Afloat`
/// recovery at the FIRST ring candidate the push-out tried — which is itself equally embedded
/// (still afloat, still no floor below at the new spot), so the NEXT frame re-entered the net from
/// that position and picked the next candidate, and so on: the body walked east one ring radius
/// (1.0 u) per embedded frame — 60 u/s at 60 fps — ignoring wish input entirely, with `in_water`
/// reporting stale-false the whole time (the net's early-return freezes the rest of `step`, so the
/// real water probe never runs). `Recovery::at_column`'s `!is_embedded` guard on the afloat arm is
/// the fix that shipped instead: a recovery must ALSO not be embedded, or by definition it is not a
/// recovery — it is the next frame's starting point for exactly the same failure.
///
/// Driven for two input-free seconds (110 frames to let anything that is going to happen settle,
/// then 10 more to check it has actually stopped) with a completely idle intent — no wish direction,
/// no vertical wish, `speed: 0.0`. The fail condition mirrors
/// `movement::tests::depenetration_corpus_over_baked_zones`'s (both pin the SAME mechanism, one on
/// hand-authored geometry that runs in ordinary CI, one on baked zones that only runs under
/// `--ignored`): "still moving AND still embedded" after the settle checkpoint is the drift
/// signature and fails the test. A body that came to rest — even somewhere still technically
/// embedded — is the separate, pre-existing "the push-out gave up, last-good fallback had nothing to
/// fall back to" state and is deliberately NOT flagged by that half of the assertion; the second,
/// stricter assertion below closes that gap for THIS specific scene, where the correct behaviour is
/// not merely "stops eventually" but "never moves at all".
#[test]
fn an_afloat_body_with_no_floor_in_reach_never_drifts() {
    let col = scenes::open_water_no_floor_in_reach();
    let start = [0.0, 0.0, 0.0];

    // CHECK THE FIXTURE, not just the assertion below — confirm this scene actually presents the
    // exact case #664 is about, on both halves of `embedded`'s OR independently, or a scene that
    // silently drifted into testing something else (e.g. a blocked footprint) would still pass this
    // test for the wrong reason.
    assert!(col.footprint_clear(start[0], start[1], start[2], PLAYER_RADIUS, 8),
        "fixture: the footprint at the origin must be CLEAR — nothing pierces it, so the ONLY thing \
         that can call this body embedded is the missing floor below, not an overlap");
    assert!(col.ground_below(start[0], start[1], start[2] + GROUND_ORIGIN, GROUND_DEPTH).is_none(),
        "fixture: there must be NO floor within GROUND_DEPTH below the origin — that absence, \
         combined with the clear footprint just above, is the whole #664 case");
    assert!(embedded(&col, start), "fixture: consequently the body starts EMBEDDED by the net's own \
        predicate — if it didn't, `step` would never even enter the ring search and this test would \
        prove nothing");
    assert!(col.in_water(start), "fixture: and the body is afloat");
    // The paired control: move the SAME floor directly under the origin and confirm the identical
    // start point is then NOT embedded — i.e. it really is the floor's distance, not the water slab
    // or some other property of the scene, that makes the case above hold.
    let with_floor = scenes::open_water_with_floor_in_reach();
    assert!(!embedded(&with_floor, start),
        "control: the SAME start point, with a floor in vertical reach instead of 500 u away, must \
         NOT be embedded — otherwise scene 4 isn't testing the no-floor-in-reach case specifically");

    let mut ctrl = CharacterController::new(start);
    let idle = MoveIntent { wish_dir: [0.0, 0.0], wish_vspeed: 0.0, jump: false, want_swim: true,
                            speed: 0.0, climb: 0.0, hop: false };
    let dt = 1.0 / 60.0;
    for _ in 0..110 { ctrl.step(idle, dt, &col); }
    let settle_from = ctrl.pos;
    for _ in 0..10 { ctrl.step(idle, dt, &col); }
    let end = ctrl.pos;

    let still_moving = ((end[0] - settle_from[0]).powi(2) + (end[1] - settle_from[1]).powi(2)).sqrt() > 1e-3
        || (end[2] - settle_from[2]).abs() > 1e-3;
    assert!(!(still_moving && embedded(&col, end)),
        "#664: the net must never walk an afloat body across open water one ring radius at a time — \
         started at {start:?}, settled-checkpoint (after 110 frames) {settle_from:?}, ended {end:?} \
         after two input-free seconds of ZERO wish input (the withdrawn first cut reached [60,0,0] \
         after 60 frames — 60 u/s)");

    // The stricter, scene-specific floor under that invariant: here the correct behaviour is not
    // merely "eventually stops", it is "never moves at all" (no last-good grounded sample exists to
    // fall back to either, since the body has never once been grounded) — so pin that directly too.
    let total_drift = ((end[0] - start[0]).powi(2) + (end[1] - start[1]).powi(2)).sqrt();
    assert!(total_drift < 1e-3,
        "#664: zero wish input, on a body with a clear footprint and no floor below, must produce \
         ZERO horizontal displacement over two seconds — got {total_drift:.3} u, ending at {end:?}");
}
