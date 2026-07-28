//! #756 — a floating spawn must be drawn at the position the server sent, not lifted off it.
//!
//! The failure is a numeric offset, so it is pinned numerically rather than by screenshot: these
//! tests build the SAME model matrix the render passes build (`models::static_placement` fed into
//! `camera::entity_model_matrix_heading` — the exact production calls, per the `#357` rule that a
//! hand-copied formula is not a test) and assert where the model's authored origin lands.
//!
//! ## The fixture is measured, not invented
//!
//! Every number below was read out of the baked `boat.glb` on 2026-07-27 by parsing the GLB's JSON
//! chunk directly (glTF `accessors[POSITION].min/max`, one mesh named `row.mod`, one node carrying
//! no `scale`/`rotation`/`translation`, `skins: []`):
//!
//! ```text
//!   POSITION min = [-14.881587, -3.982317, -8.304960]
//!   POSITION max = [ 22.851353,  5.962854,  8.413661]
//!   skins        = 0                       → takes the STATIC model path
//! ```
//!
//! from which the loader's own reductions (`models.rs`: `y_bottom = -y_min` when `y_min < 0`,
//! `y_extent = y_max - y_min`, centres = bbox midpoints) give the constants used here.
//!
//! ## Provenance of the two supporting scans quoted below
//!
//! Both were run against the installed asset set (130 `.glb` files) with the same GLB-JSON parse
//! — for each file, read the 12-byte header, walk the chunks, take the `JSON` chunk, and report
//! `len(skins)` plus the min/max over every `meshes[*].primitives[*].attributes.POSITION`
//! accessor's `min`/`max`:
//!
//! 1. **`boat.glb` is the only entity-archetype GLB with `skins == 0`.** Of the 130 files, the
//!    ones `model_for` can load are the archetype names (`humanoid`, `elf`, `dwarf`, `gnoll`,
//!    `skeleton`, `zombie`, `creature`, `rat`, `snake`, `frog`, `wasp`, `wolf`, `bat`, `bird`,
//!    `worm`, `fish`, `bear`, `boat`) plus the 29 `race_*` player models. Every one of those
//!    reported `skins == 1` except `boat.glb`, which reported `skins == 0`. The remaining files
//!    are zone/door/weapon assets that never reach `model_for`.
//! 2. **A skinned rig's raw vertex origin is not a stable EQ datum.** `race_hum.glb` reported
//!    `y_min = -10.4305`; `race_huf.glb` reported `y_min = -3.6888`. Both are nominally 6.0-foot
//!    humans (`race_target_height("HUM") == 6.0`), so their raw origins sit at wildly different
//!    heights within the same body. This is why #756's zero-lift rule was NOT extended to the
//!    skinned path: "put the origin at the stored z" has no defined meaning there.

use eqoxide_renderer::camera::entity_model_matrix_heading;
use eqoxide_renderer::models::{
    archetype_correction, archetype_native_units, archetype_scale, humanoid_placement,
    skinned_target_height, static_placement, target_height_for,
};

// ── Measured `boat.glb` constants (see the module doc) ───────────────────────────────────────
const BOAT_Y_MIN: f32 = -3.982317;
const BOAT_Y_MAX: f32 = 5.962854;
const BOAT_X_MIN: f32 = -14.881587;
const BOAT_X_MAX: f32 = 22.851353;
const BOAT_Z_MIN: f32 = -8.304960;
const BOAT_Z_MAX: f32 = 8.413661;

fn boat_y_extent() -> f32 { BOAT_Y_MAX - BOAT_Y_MIN }
fn boat_y_bottom() -> f32 { -BOAT_Y_MIN }
fn boat_center_xz() -> [f32; 2] {
    [(BOAT_X_MIN + BOAT_X_MAX) * 0.5, (BOAT_Z_MIN + BOAT_Z_MAX) * 0.5]
}

/// Build the production model matrix for a static entity and return its translation column —
/// where the model's local origin (0,0,0) is drawn in world space.
///
/// Only the **z** of this is asserted on below. The horizontal recentre survives into the x/y of
/// this column, and #756 deliberately did not touch it: the wire *xy* datum was never established
/// (see `static_placement`'s doc). Conveniently the recentre contributes exactly 0 to z — it is a
/// `(-c0, 0, -c1)` translate that the `+90°` X rotation maps to `(-c0, c1, 0)`, after which only
/// the heading yaw (a Z rotation) is applied — so the z assertion is unaffected by it either way.
fn drawn_origin(pos: [f32; 3], heading: f32, floating: bool) -> [f32; 3] {
    let p = static_placement(
        "boat", boat_y_extent(), boat_y_bottom(), boat_center_xz(), floating);
    let m = entity_model_matrix_heading(
        pos, heading, p.visual_scale, p.mesh_scale, p.center_xz, true, p.y_bottom,
        archetype_correction("boat"),
    );
    [m[3][0], m[3][1], m[3][2]]
}

/// THE regression. A floating hull's authored origin must land exactly on the server-sent
/// position — the position eqoxide stores verbatim for a floating spawn, because
/// `eqoxide_core::coord::wire_z_to_foot` passes a floating entity's wire z through unchanged.
#[test]
fn a_floating_spawn_is_drawn_at_its_server_position_not_lifted_off_it() {
    let pos = [1200.0_f32, -640.0, 4.0];
    let got = drawn_origin(pos, 137.0, true);

    assert!(
        (got[2] - pos[2]).abs() < 1e-3,
        "a floating spawn's model origin must be drawn at the server-sent z; \
         drawn z {}, server z {} (delta {})",
        got[2], pos[2], got[2] - pos[2]
    );
}

/// The size of the bug, stated as the difference between the two arms rather than as a bare
/// constant: with `boat.glb`'s measured bounds the grounded arm lifts the hull 13.9275u — more
/// than the model's own 9.9452u height — and slides it 3.9849u along its length axis.
#[test]
fn the_grounded_arm_would_lift_a_boat_clear_of_its_own_height() {
    let grounded = static_placement(
        "boat", boat_y_extent(), boat_y_bottom(), boat_center_xz(), false);
    let arch = archetype_scale("boat");

    // `entity_model_matrix_heading` lifts by `visual_scale * 0.5 + y_bottom * mesh_scale`.
    let lift = grounded.visual_scale * 0.5 + grounded.y_bottom * grounded.mesh_scale;
    assert!((lift - 13.9275).abs() < 1e-3, "grounded lift for boat.glb: expected 13.9275, got {lift}");
    assert!(lift > boat_y_extent() * arch,
        "the defect: lift {lift} exceeds the model's whole rendered height {}",
        boat_y_extent() * arch);

    // And with the same bounds, the floating arm removes both lift terms — and ONLY those. The
    // horizontal recentre is passed through untouched, because #756 established the z datum and
    // not the xy one; pinning that here keeps a later change from quietly widening the exemption
    // into an axis nobody measured.
    let floating = static_placement(
        "boat", boat_y_extent(), boat_y_bottom(), boat_center_xz(), true);
    assert_eq!(floating.visual_scale, 0.0, "floating placement must carry no lift");
    assert_eq!(floating.y_bottom, 0.0, "floating placement must carry no y_bottom lift");
    assert_eq!(floating.center_xz, grounded.center_xz,
        "the floating arm must not change the horizontal recentre — that datum is unestablished");
}

/// The exemption must not become "nothing is ever grounded". A grounded spawn with the *same*
/// bounds still gets the full lift — without this, deleting the whole formula would pass.
#[test]
fn a_grounded_spawn_with_the_same_bounds_is_still_lifted() {
    let pos = [1200.0_f32, -640.0, 4.0];
    let got = drawn_origin(pos, 137.0, false);
    assert!((got[2] - pos[2]).abs() > 1.0,
        "a grounded static model must still be lifted off its stored z; drawn z {} vs stored {}",
        got[2], pos[2]);
}

/// Scale is NOT part of the exemption: a floating spawn is placed differently, not sized
/// differently. Both arms keep `archetype_scale`.
#[test]
fn the_floating_exemption_changes_placement_only_not_scale() {
    for floating in [false, true] {
        let p = static_placement(
            "boat", boat_y_extent(), boat_y_bottom(), boat_center_xz(), floating);
        assert_eq!(p.mesh_scale, archetype_scale("boat"),
            "mesh_scale must stay archetype_scale for floating={floating}");
    }
}

/// The latent companion (#756): a boat that shipped with a skeleton would take the skinned path,
/// where `archetype_target_height` has no `"boat"` arm and its `_ => 6.0` fallback would squash
/// the hull to a 6-foot character height. `skinned_target_height` renders it at its authored size
/// instead (`mesh_scale == 1.0`), which is the same statement `archetype_scale("boat") == 1.0`
/// already makes on the static path.
#[test]
fn a_skinned_boat_would_render_at_native_size_not_a_character_height() {
    let true_height = boat_y_extent(); // 9.9452
    let target = skinned_target_height("SHP", "boat", true_height);
    let placement = humanoid_placement(true_height, 0.0, target);
    assert!((placement.mesh_scale - 1.0).abs() < 1e-6,
        "a native-units archetype must render at scale 1.0, got {}", placement.mesh_scale);

    // Pin what it would otherwise have been, so this test fails if the exemption is dropped.
    let unexempt = humanoid_placement(true_height, 0.0, target_height_for("SHP", "boat"));
    assert!((unexempt.mesh_scale - 1.0).abs() > 0.3,
        "the un-exempt path must differ, else this test proves nothing; got {}", unexempt.mesh_scale);
}

/// Every other archetype is untouched by the native-units exemption.
#[test]
fn skinned_target_height_is_unchanged_for_non_native_archetypes() {
    for (race, archetype) in [
        ("HUM", "humanoid"), ("GNL", "gnoll"), ("RAT", "rat"), ("WOL", "wolf"),
        ("FIS", "fish"), ("BAT", "bat"), ("SKE", "skeleton"), ("DWF", "dwarf"),
    ] {
        assert!(!archetype_native_units(archetype), "{archetype} must not be native-units");
        assert_eq!(skinned_target_height(race, archetype, 7.0), target_height_for(race, archetype),
            "{race}/{archetype} must keep its character target height");
    }
}

/// The two model paths must agree that a native-units archetype renders at native size. This is
/// the trap #756 names: fixing one path and leaving the other sets a defect that only fires after
/// an unrelated asset change (a boat asset gaining a skeleton).
#[test]
fn both_model_paths_agree_on_native_scale_for_native_units_archetypes() {
    for archetype in ["boat"] {
        assert!(archetype_native_units(archetype));
        assert_eq!(archetype_scale(archetype), 1.0,
            "static path: {archetype} must render at native scale");
        let h = 9.9452_f32;
        assert!((humanoid_placement(h, 0.0, skinned_target_height("SHP", archetype, h)).mesh_scale
                 - 1.0).abs() < 1e-6,
            "skinned path: {archetype} must render at native scale");
    }
}
