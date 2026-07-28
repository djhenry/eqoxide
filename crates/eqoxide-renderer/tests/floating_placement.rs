//! #756 — the grounding lift must not be applied to a floating spawn's z.
//!
//! ## What is certain here, and what is inferred
//!
//! Read this before quoting a test name out of this file.
//!
//! **Certain, from the code alone:** applying the grounding lift to a floating spawn is wrong.
//! `eqoxide_core::coord::wire_z_to_foot` (`coord.rs:115-117`) returns a floating entity's wire z
//! UNCHANGED, so its stored z is by construction *not* in the FOOT datum. The grounding lift is a
//! foot-datum→placement conversion; applying it to a z that was never converted is wrong whatever
//! the wire datum turns out to be. That is what these tests grade.
//!
//! **Inferred, NOT measured:** that the resulting placement — authored origin exactly on the
//! stored z — is where a hull actually sits at the waterline. `coord.rs:8-9` states that EQ's wire
//! z is the model-origin position, but says it of *characters*; extending it to a boat hull is an
//! inference from `coord.rs`, not something measured against a running server, and no live E2E run
//! backs it (see `static_placement`'s doc). So where a test below says a floating spawn's origin
//! "lands on" the stored z, that is a statement about what the code does and why it is not the old
//! (definitely wrong) lift — it is not a verified claim about the waterline.
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
//! Both were run against the installed asset set with the same GLB-JSON parse — for each file,
//! read the 12-byte header, walk the chunks, take the `JSON` chunk, and report `len(skins)` plus
//! the min/max over every `meshes[*].primitives[*].attributes.POSITION` accessor's `min`/`max`.
//! The set held **136** `.glb` files when the scan was last re-run (2026-07-27). An earlier run the
//! same day counted 130: the model directory is a live sync target, so the *total* moves. The
//! loadable subset below and the result did not.
//!
//! 1. **`boat.glb` is the only model `model_for` can load with `skins == 0`.** The loadable names
//!    are the 18 distinct archetypes `race_to_archetype` can return (`humanoid`, `elf`, `dwarf`,
//!    `gnoll`, `skeleton`, `zombie`, `creature`, `rat`, `snake`, `frog`, `wasp`, `wolf`, `bat`,
//!    `bird`, `worm`, `fish`, `bear`, `boat`) plus the 29 `race_*` player models — 47 names, all 47
//!    present. Every one reported `skins == 1` except `boat.glb`, at `skins == 0`. The remaining
//!    files are zone/door/weapon assets `model_for` never names. Also scanned, because the first
//!    pass missed them: the `<key>_f.glb` female variants `model_for` prefers for `gender == 1`
//!    (`renderer.rs:610`). Three exist on disk — `humanoid_f`, `elf_f`, `dwarf_f` — and all three
//!    reported `skins == 1`, so they do not change the conclusion.
//! 2. **A skinned rig's raw vertex origin is not a stable EQ datum.** `race_hum.glb` reported
//!    `y_min = -10.430519`; `race_huf.glb` reported `y_min = -3.688780`. Both are nominally
//!    6.0-foot humans (`race_target_height("HUM") == 6.0`), so their raw origins sit at wildly
//!    different heights within the same body. This is why #756's zero-lift rule was NOT extended to
//!    the skinned path: "put the origin at the stored z" has no defined meaning there.

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

/// THE regression: no grounding lift is applied to a floating spawn's stored z, which leaves the
/// hull's authored origin on that z.
///
/// The name states the *mechanism* deliberately, not "the boat floats correctly". Zero applied lift
/// is what the code is certain to owe (the stored z never went through `wire_z_to_foot`'s foot
/// conversion, so the foot→placement lift does not apply to it). That the resulting placement is
/// the true waterline is the INFERENCE this file's header flags and no test here can settle.
#[test]
fn a_floating_spawn_gets_no_grounding_lift_so_its_origin_stays_on_the_stored_z() {
    let pos = [1200.0_f32, -640.0, 4.0];
    let got = drawn_origin(pos, 137.0, true);

    assert!(
        (got[2] - pos[2]).abs() < 1e-3,
        "no grounding lift may be applied to a floating spawn's stored z, so its model origin \
         stays on that z; drawn z {}, stored z {} (delta {}). NB: that zero is the RIGHT lift is \
         inferred from coord.rs, not measured live — see this file's header.",
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

/// Every archetype `race_to_archetype` can return — the closed set, so this quantifies over the
/// whole domain rather than over the one member that motivated it.
const ALL_ARCHETYPES: [&str; 18] = [
    "humanoid", "elf", "dwarf", "boat", "gnoll", "skeleton", "zombie", "creature", "bear", "wolf",
    "rat", "snake", "frog", "bat", "bird", "wasp", "worm", "fish",
];

/// **Property**, over every archetype: `archetype_native_units(a)` ⟹ both model paths render `a`
/// at native size. This is the trap #756 names — fixing one path and leaving the other sets a
/// defect that only fires after an unrelated asset change (a boat asset gaining a skeleton).
///
/// Quantified rather than written as `for archetype in ["boat"]`, because the single-member loop
/// graded only today's membership: adding, say, `"creature"` to `archetype_native_units` would make
/// the two paths disagree silently (`archetype_scale("creature") == 0.45`, while the skinned path
/// would render it at 1.0) and nothing would redden. Now that addition fails here.
///
/// Note the direction. This is a one-way implication, and only that: it does NOT say a non-native
/// archetype scales to anything in particular — those are per-archetype calibration constants with
/// no shared law, which is exactly why the property is stated as a conditional.
#[test]
fn native_units_archetypes_render_at_native_scale_on_both_model_paths() {
    let mut checked = 0;
    for archetype in ALL_ARCHETYPES {
        if !archetype_native_units(archetype) { continue; }
        checked += 1;
        assert_eq!(archetype_scale(archetype), 1.0,
            "static path: native-units {archetype} must render at native scale");
        for h in [1.5_f32, 9.9452, 220.0] {
            let scale =
                humanoid_placement(h, 0.0, skinned_target_height("SHP", archetype, h)).mesh_scale;
            assert!((scale - 1.0).abs() < 1e-6,
                "skinned path: native-units {archetype} at true_height {h} must render at native \
                 scale, got {scale}");
        }
    }
    assert!(checked > 0, "no archetype is native-units, so this property graded nothing");
}

// ── Source-text pins ────────────────────────────────────────────────────────────────────────────

const PASS_RS: &str = include_str!("../src/pass.rs");
const MODELS_RS: &str = include_str!("../src/models.rs");

/// The property above quantifies over `ALL_ARCHETYPES`, a hardcoded list — so it is only total if
/// that list really is every archetype. It is not total one step outside itself: adding
/// `"AIR" => "airship"` to `race_to_archetype` **and** `"airship"` to `archetype_native_units`,
/// while leaving `"airship"` out of `ALL_ARCHETYPES`, reintroduces exactly the silent two-path
/// disagreement the property was written to close, and the property never looks at it.
///
/// So the list is pinned set-equal to the archetypes `race_to_archetype` can actually return,
/// parsed from its source. Same technique and same caveat as the call-site pin below: source text,
/// not semantics.
#[test]
fn all_archetypes_is_every_archetype_race_to_archetype_can_return() {
    let body = MODELS_RS
        .split_once("pub fn race_to_archetype(")
        .expect("race_to_archetype not found in models.rs").1;
    let body = body.split_once("\npub fn ").map(|(b, _)| b).unwrap_or(body);

    let mut from_source: Vec<&str> = body
        .match_indices("=> \"")
        .map(|(i, _)| {
            let rest = &body[i + 4..];
            &rest[..rest.find('"').expect("unterminated archetype string literal")]
        })
        .collect();
    from_source.sort_unstable();
    from_source.dedup();
    assert!(!from_source.is_empty(), "parsed no archetypes — the parser broke, not the code");

    let mut listed = ALL_ARCHETYPES.to_vec();
    listed.sort_unstable();
    assert_eq!(from_source, listed,
        "ALL_ARCHETYPES must be exactly the set race_to_archetype can return, or the property \
         above silently stops covering whatever is missing");
}

/// Nothing above would fail if `pass.rs` computed a correct `StaticPlacement` and then passed a
/// literal `false` for `floating` at the two entity call sites — which is the whole user-visible
/// fix, reverted. Measured, not argued: with `b.floating` changed to a literal `false` at both
/// sites, this pin is the ONLY test in `eqoxide-renderer` that fails; every other test in the crate
/// stays green.
///
/// This is a **source-text** assert, the same technique (and the same caveat) as
/// `shadow_caster_selection.rs`'s `encode_shadow_pass_calls_the_planner_this_file_grades`: it
/// proves the argument is written, not that the branch is reached. Semantic coverage would need the
/// draw path to be device-free, which is a larger refactor than #756.
///
/// Split 2/2 by design. The two entity sites take `floating` from the spawn; the two player sites
/// pass a literal `false` on purpose — the player's z is the `CharacterController`'s FOOT datum, not
/// a wire passthrough, so the player is never a model-origin placement.
///
/// Asserting BOTH halves is what makes this more than a `contains` check, and it catches an attack
/// the "must pass `b.floating`" half alone would miss. Measured: rewriting the entity sites to
/// `b.floating && false` — which still mentions `b.floating`, so `from_spawn` stays at 2 — trips the
/// `player` half instead, at `found 4 of 2`, because those call sites now end in `false` too.
#[test]
fn every_static_placement_call_site_in_pass_rs_decides_floating_explicitly() {
    let calls: Vec<&str> = PASS_RS
        .match_indices("static_placement(")
        .map(|(i, _)| {
            let rest = &PASS_RS[i..];
            &rest[..rest.find(");").expect("unterminated static_placement( call in pass.rs")]
        })
        .collect();

    assert_eq!(calls.len(), 4,
        "pass.rs must place static models at exactly the 4 known sites (entity, player, and both \
         static shadow-caster arms); found {}. A new site is not necessarily wrong — but it has to \
         be reviewed for the #756 exemption, which is why this count is pinned.", calls.len());

    let from_spawn = calls.iter().filter(|c| c.contains("b.floating")).count();
    let player = calls.iter().filter(|c| c.trim_end().ends_with("false")).count();

    assert_eq!(from_spawn, 2,
        "the entity pass and the nearby shadow-caster arm must pass `b.floating`, not a literal — \
         passing `false` there restores the pre-#756 rendering (a boat 13.9275u above the water) \
         and no other test in this crate would notice; found {from_spawn} of 2");
    assert_eq!(player, 2,
        "the player pass and the player shadow-caster arm must pass a literal `false`; \
         found {player} of 2");
}
