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
//! swimmer is lifted to the DESTINATION column's swim plane" and "the water-blind push-out mounts
//! a swimmer onto a lid and strands it dry". Both are worth having; neither replaces the other.

mod synthetic_scenes;

use eqoxide::movement::CharacterController;
use eqoxide::nav::collision::Collision;
use eqoxide::traversability::PLAYER_BODY;
use eqoxide_ipc::MoveIntent;
use synthetic_scenes as scenes;

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

/// **THE #649 STRAND, ON SYNTHETIC GEOMETRY: the push-out mounts a swimmer onto a lid.**
///
/// A sealed flooded chamber whose ceiling slab's top face sits **0.009 u above the waterline**. A
/// swimmer floating at the chamber's own swim plane presses into a wall; `footprint_clear` fails,
/// which the controller reads as embedded; the depenetration push-out then hunts for a FLOOR
/// within `STEP_UP + GROUND_ORIGIN = 3.0` u — and the lid is 2.009 u up. It is placed there,
/// `on_ground`, and the position is DRY, so `in_water` is false, buoyancy never fires again, and
/// nothing puts it back.
///
/// This is the same mechanism `tests/water_capability.rs`'s
/// `qcat_pocket_swim_plane_strands_the_swimmer_on_the_ceiling_lid` pins against baked `qcat`, and
/// it reproduces the same three facts (mounted on the lid / dry / stranded) on geometry that
/// contains nothing game-derived. What it does NOT pin is qcat's own −55.9687.
///
/// **The push-out is water-blind, and that is the defect (#649, open).** This test therefore
/// asserts the CURRENT behaviour, exactly like its asset-gated twin — when #649 is fixed, both
/// flip together and should be rewritten to assert the escape.
#[test]
fn a_swimmer_at_the_pocket_swim_plane_is_mounted_onto_the_lid_and_stranded_dry() {
    let col = scenes::sealed_pocket_with_lid();
    let start = [-20.0, 0.0, scenes::POCKET_SWIM_PLANE];
    assert!(col.in_water(start), "fixture: the start must be in water");
    assert!((col.water_surface(start).unwrap() - scenes::POCKET_SURFACE).abs() < 0.01,
        "fixture: pocket surface");

    let end = swim_toward(&col, start, [40.0, 0.0, scenes::POCKET_SWIM_PLANE], 6.0);

    assert!((end[2] - scenes::POCKET_LID_Z).abs() < 0.01,
        "#649: the depenetration push-out must have placed the swimmer on the LID at {:.4}; got \
         {end:?}. The push-out hunts for a floor within STEP_UP + GROUND_ORIGIN = 3.0 u and is \
         WATER-BLIND — it does not ask whether the position it picks is one a swimmer can occupy.",
        scenes::POCKET_LID_Z);
    assert!(!col.in_water(end),
        "#649: and the mounted position is DRY (surface {:.3}) — which is why buoyancy never \
         recovers it. Got in_water=true at {end:?}", scenes::POCKET_SURFACE);
    assert!(end[2] > scenes::POCKET_SURFACE,
        "#649: the character is ABOVE the waterline it was floating in — got {end:?}");
}

/// **AND THE MOUNT IS ONE-WAY: it cannot swim back down through the lid it was placed on.**
///
/// The live #649 evidence was `POST /v1/move/manual {"up":-1,"duration_ms":3000}` moving the
/// character **0.00 u**. Same here: a full-strength downward swim wish for 3 s from the mounted
/// position changes `z` by less than a tenth of a unit, because `want_swim` only does anything
/// when `in_water`, and the mounted position is dry. That is what makes this a strand rather than
/// a stumble.
#[test]
fn the_lid_mount_is_one_way_a_downward_swim_wish_cannot_recover_it() {
    let col = scenes::sealed_pocket_with_lid();
    let mounted = [-20.0, 0.0, scenes::POCKET_LID_Z];
    assert!(!col.in_water(mounted), "fixture: the mounted position must be dry");

    let end = try_to_sink(&col, mounted, 3.0);
    assert!((end[2] - mounted[2]).abs() < 0.1,
        "#649: a 3 s full-strength downward swim wish must move the mounted character essentially \
         nowhere (the live report measured 0.00 u); got {end:?}, a change of {:+.3} u",
        end[2] - mounted[2]);
}

/// **THE CONTROL: remove ONLY the lid, and the character never goes dry.**
///
/// Same chamber, same walls, same floor, same water volume and surface — no ceiling slab. The
/// character presses into the same wall at the same frames and the push-out fires identically:
/// measured frame-by-frame, the XY trajectories of the two scenes are **the same to the last
/// decimal**, and only `z` differs. With the lid, `nearest_floor` finds it 2.009 u UP and the
/// character is mounted dry. Without it, the nearest floor is the chamber floor 12 u DOWN, so the
/// character is slammed there instead — still submerged, still recoverable in principle.
///
/// **A finding worth stating plainly:** the character does NOT then float back to the swim plane.
/// It oscillates between the chamber floor and ~1 u above it, because the same water-blind
/// push-out re-fires every frame it presses the wall and re-slams it down. That is the *same*
/// defect (#649) in its other direction — `nearest_floor` hunting for a floor a swimmer has no
/// business standing on — and `tests/water_capability.rs` documents exactly this downward form
/// against baked `qcat`. The control's claim is therefore the narrower, true one: **the character
/// stays submerged and never ends up above the waterline.**
///
/// This is what makes the two tests above load-bearing rather than incidental: the lid is the
/// single element that turns a wet, in-principle-recoverable push-out into a dry, permanent strand.
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
