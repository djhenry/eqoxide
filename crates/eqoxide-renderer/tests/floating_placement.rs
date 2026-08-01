//! #756 — the grounding lift must not be applied to a floating spawn's z.
//! #768 — and when it IS applied, it must put the model's lowest vertex ON the stored z rather than
//! a full rendered model height above it.
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
//! `camera::entity_model_matrix_static` — the exact production calls, per the `#357` rule that a
//! hand-copied formula is not a test) and assert where the model's authored origin lands.
//!
//! Since #781 that is a *stronger* statement than it was. Before #781 the `visual_scale = 0.0` that
//! the static arm depends on was spelled at each of the four `pass.rs` call sites, and these tests
//! could not reach it — they built the matrix with their own literal `0.0`, so only the source-text
//! pin at the bottom of this file graded the shipped one. #781 moved that `0.0` into
//! `camera::entity_model_matrix_static`, which these tests DO call, so it is now graded
//! behaviourally: measured by replacing it with `2.0 * p.y_bottom * p.mesh_scale`, which turns
//! `a_grounded_static_model_is_drawn_with_its_lowest_vertex_on_the_stored_z` RED (crate
//! `--no-fail-fast`: 261 passed / 1 failed / 12 ignored).
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
//! 1. **`boat.glb` is the only model `model_for` can load that lands on the STATIC arm.** State the
//!    gate first, because an earlier version of this note stated the wrong one. As of eqoxide#780
//!    the gate is named — `renderer::SkinFit::classify` picks the skinned path for `Fits`, so the
//!    static arm is everything else: no skin, a skin with zero joints (`EmptySkin`), **or a skin
//!    with more than `JOINT_CAP` (128) joints (`ExceedsCap`)**. `skins == 0` is therefore
//!    *sufficient but not necessary*, and a scan that only reads `len(skins)` does not establish
//!    which arm a model takes. (Before #780 this was the unnamed boolean
//!    `asset.skin.as_ref().is_some_and(|s| s.joint_count > 0 && s.joint_count <= 128)` at
//!    `renderer.rs:663-664` of the #773 merge-base — same gate, no line to cite anymore now that
//!    it has a name.)
//!
//!    Re-scanned on 2026-07-28 reading `len(skins[0].joints)` as well, and again independently for
//!    #780 (2026-07-31, parsing the GLB JSON chunk directly rather than via this predicate).
//!    Loadable names: the 18 distinct archetypes `race_to_archetype` can return (`humanoid`, `elf`,
//!    `dwarf`, `gnoll`, `skeleton`, `zombie`, `creature`, `rat`, `snake`, `frog`, `wasp`, `wolf`,
//!    `bat`, `bird`, `worm`, `fish`, `bear`, `boat`) plus the 29 `race_*` player models plus the 3
//!    `<key>_f.glb` female variants that exist (`humanoid_f`, `elf_f`, `dwarf_f`; `model_for`
//!    prefers them for `gender == 1`, `renderer.rs:610`) — **50 files, all present**. Exactly one
//!    lands on the static arm: `boat.glb`, `skins == 0`, 0 joints. Nothing is above the cap. The
//!    remaining files on disk are zone/door/weapon assets `model_for` never names.
//!
//!    **This is a margin of one joint on a directory rebaked outside this repo.**
//!    `race_pcfroglok.glb` sits at **127** joints against the 128 cap, and 11 rigs are at ≥ 109
//!    (next highest 110). A two-joint rebake of that file moves a PC race — for which
//!    `Entity::floating()` is false — onto the grounded arm. The grounded arm being unreached today
//!    is a fact about the current asset bake, not a property of the code. #780 does not change
//!    that fact; it changes what happens the day it stops being true — see
//!    `tests/skin_cap_selection.rs` for the classifier this note now describes and
//!    `EqRenderer::skin_cap_downgrades` / the `error!` log `build_character_model` now emits when
//!    `ExceedsCap` is reached.
//! 2. **A skinned rig's raw vertex origin is not a stable EQ datum.** `race_hum.glb` reported
//!    `y_min = -10.430519`; `race_huf.glb` reported `y_min = -3.688780`. Both are nominally
//!    6.0-foot humans (`race_target_height("HUM") == 6.0`), so their raw origins sit at wildly
//!    different heights within the same body. This is why #756's zero-lift rule was NOT extended to
//!    the skinned path: "put the origin at the stored z" has no defined meaning there.

use eqoxide_renderer::camera::entity_model_matrix_static;
use eqoxide_renderer::models::{
    archetype_correction, archetype_native_units, archetype_scale, humanoid_placement,
    skinned_target_height, static_placement, target_height_for, ModelBounds,
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

/// `boat.glb`'s measured bounds as the ONE value `static_placement` takes since #781.
///
/// Note what this being writable at all means, and it is the point rather than an oversight:
/// `ModelBounds` is a plain public-field struct, so this test crate can build one without a
/// `wgpu::Device` — which is exactly why the unforgeable-newtype form was declined (this is an
/// *integration* test and cannot see a `#[cfg(test)]` mint). The same freedom is available to
/// `pass.rs`; see `every_static_placement_in_pass_rs_is_written_exactly_as_reviewed`.
fn boat_bounds() -> ModelBounds {
    ModelBounds {
        y_bottom: boat_y_bottom(),
        y_extent: boat_y_extent(),
        x_center: boat_center_xz()[0],
        z_center: boat_center_xz()[1],
    }
}

/// Build the production model matrix for a static `boat.glb` entity — the exact calls the render
/// passes make (`models::static_placement` fed into `camera::entity_model_matrix_static`).
///
/// Since #781 the `visual_scale = 0.0` and `y_up = true` that used to be spelled here are inside
/// `entity_model_matrix_static`, so this helper no longer *chooses* them; that is why the pin below
/// still has to grade the call sites' function NAME.
///
/// Column-major (`glam`'s `to_cols_array_2d`), so `m[col][row]`.
fn boat_matrix(pos: [f32; 3], heading: f32, floating: bool) -> [[f32; 4]; 4] {
    let p = static_placement("boat", &boat_bounds(), floating);
    entity_model_matrix_static(pos, heading, &p, archetype_correction("boat"))
}

/// Where the model's local origin (0,0,0) is drawn in world space — the matrix's translation column.
///
/// Only the **z** of this is asserted on below. The horizontal recentre survives into the x/y of
/// this column, and #756 deliberately did not touch it: the wire *xy* datum was never established
/// (see `static_placement`'s doc). Conveniently the recentre contributes exactly 0 to z — it is a
/// `(-c0, 0, -c1)` translate that the `+90°` X rotation maps to `(-c0, c1, 0)`, after which only
/// the heading yaw (a Z rotation) is applied — so the z assertion is unaffected by it either way.
fn drawn_origin(pos: [f32; 3], heading: f32, floating: bool) -> [f32; 3] {
    let m = boat_matrix(pos, heading, floating);
    [m[3][0], m[3][1], m[3][2]]
}

/// World z of the lowest point of the drawn hull, by mapping `boat.glb`'s measured bounding box
/// through the production matrix.
///
/// **Why a bounding-box corner is a real vertex here, checked rather than assumed.** The AABB's
/// minimum world z is only the mesh's minimum world z if world z depends on exactly one local axis
/// — otherwise the lowest AABB corner can be a point no vertex occupies. So this asserts the
/// matrix's own third row first: the x and z columns must contribute 0 to world z, leaving
/// `world_z = m[1][2] * local_y + m[3][2]`. A real vertex attains `local_y == BOAT_Y_MIN` (that is
/// where the accessor's `min` comes from), so the corner IS a vertex. That check also re-derives,
/// from the matrix rather than from a second test's name, the fact `camera.rs`'s
/// `static_model_y_up_axis_maps_to_world_up` pins: local +Y becomes world +Z.
fn drawn_lowest_vertex_z(pos: [f32; 3], heading: f32, floating: bool) -> f32 {
    let m = boat_matrix(pos, heading, floating);
    assert!(m[0][2].abs() < 1e-5 && m[2][2].abs() < 1e-5,
        "world z must depend only on the model's local Y for a bbox corner to be a real vertex; \
         x-col {} z-col {} (heading {heading})", m[0][2], m[2][2]);

    let mut lowest = f32::INFINITY;
    for x in [BOAT_X_MIN, BOAT_X_MAX] {
        for y in [BOAT_Y_MIN, BOAT_Y_MAX] {
            for z in [BOAT_Z_MIN, BOAT_Z_MAX] {
                let wz = m[0][2] * x + m[1][2] * y + m[2][2] * z + m[3][2];
                if wz < lowest { lowest = wz; }
            }
        }
    }
    lowest
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

/// **#768's regression.** The grounded arm puts the model's LOWEST VERTEX on the stored z.
///
/// Graded on the lowest vertex, not on the lift scalar, because that is how the bug hid: the lift
/// was `(y_extent + y_bottom) * mesh_scale` instead of `y_bottom * mesh_scale`, and every assertion
/// #756 shipped read either the lift magnitude or the drawn ORIGIN — both of which the buggy arm
/// satisfies just as happily as the correct one. Mapping the bounding box through is what shows the
/// hull sitting `9.945171` in the air.
///
/// Swept over headings because the lift and the horizontal recentre are applied on opposite sides of
/// the heading yaw; a lift that leaked into the rotated part would land on the stored z at heading 0
/// and drift elsewhere.
#[test]
fn a_grounded_static_model_is_drawn_with_its_lowest_vertex_on_the_stored_z() {
    let pos = [1200.0_f32, -640.0, 4.0];
    for heading in [0.0_f32, 90.0, 137.0, 180.0, 270.0, 359.5] {
        let lowest = drawn_lowest_vertex_z(pos, heading, false);
        assert!((lowest - pos[2]).abs() < 1e-3,
            "a grounded static model's lowest vertex must be drawn on its stored z; heading \
             {heading}: lowest vertex z {lowest}, stored z {} (delta {}). Before #768 this delta \
             was 9.945171 — `y_extent * mesh_scale`, a full rendered model height.",
            pos[2], lowest - pos[2]);
    }
}

/// The size of the correction, stated as the difference between the two arms rather than as a bare
/// constant: with `boat.glb`'s measured bounds the grounded arm lifts the hull 3.9823u — its
/// `y_bottom`, LESS than the model's own 9.9452u height — and slides it 3.9849u along its length
/// axis.
///
/// The `lift < height` direction is the half that would have caught #768. It is not a general law
/// about static models (a model whose origin sits above its own top would legitimately exceed it);
/// it is a statement about `boat.glb`'s measured bounds, which is why the constants are named here.
#[test]
fn the_grounded_arm_lifts_a_boat_by_its_y_bottom_not_by_its_whole_height() {
    let grounded = static_placement("boat", &boat_bounds(), false);
    let arch = archetype_scale("boat");

    // `entity_model_matrix_heading` lifts by `visual_scale * 0.5 + y_bottom * mesh_scale`, and the
    // static call sites pass a literal `0.0` for `visual_scale` (there is no longer a field for it).
    let lift = grounded.y_bottom * grounded.mesh_scale;
    assert!((lift - 3.9823).abs() < 1e-3, "grounded lift for boat.glb: expected 3.9823, got {lift}");
    assert!(lift < boat_y_extent() * arch,
        "#768: the lift {lift} must not reach the model's whole rendered height {} — that is the \
         over-lift that put the hull in the air",
        boat_y_extent() * arch);

    // And with the same bounds, the floating arm removes the lift — and ONLY that. The horizontal
    // recentre is passed through untouched, because #756 established the z datum and not the xy
    // one; pinning that here keeps a later change from quietly widening the exemption into an axis
    // nobody measured.
    let floating = static_placement("boat", &boat_bounds(), true);
    assert_eq!(floating.y_bottom, 0.0, "floating placement must carry no lift");
    assert_eq!(floating.center_xz, grounded.center_xz,
        "the floating arm must not change the horizontal recentre — that datum is unestablished");
}

/// The exemption must not become "nothing is ever grounded". A grounded spawn with the *same*
/// bounds still has its ORIGIN lifted off the stored z — without this, deleting the whole formula
/// would pass. `boat.glb`'s origin ends up `y_bottom * mesh_scale` = 3.9823u up, so the `> 1.0`
/// threshold is loose on purpose: this test grades "some lift survives", and
/// `a_grounded_static_model_is_drawn_with_its_lowest_vertex_on_the_stored_z` grades how much.
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
        let p = static_placement("boat", &boat_bounds(), floating);
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
///
/// ## The parser's own escape hatches, closed (#769)
///
/// The parse is literally "arrow, space, double-quote". Four match-arm spellings return an
/// archetype it cannot see, and `cargo fmt` normalizes only two of them — and this repo runs no
/// `cargo fmt` job in CI at all (`.github/workflows/test.yml` has `no-local-detail` and `test`, and
/// no fmt or clippy step), so even that mitigation is a convention rather than a gate:
///
/// | spelling | `cargo fmt` normalizes |
/// |---|---|
/// | `"AIR"=>"airship"` | yes |
/// | `"AIR" =>`⏎`"airship"` | yes |
/// | `"AIR" => { "airship" }` | no |
/// | `"AIR" => AIRSHIP` (named const) | no |
///
/// The arrow-count assert below reddens all four: it requires every `=>` in the parsed region to be
/// the start of a `=> "` string-literal arm, so an arm the extractor skips shows up as a bare arrow
/// with no matching literal. Each of the four was applied to `models.rs` and measured RED
/// individually — see the PR for #769 for the counts.
#[test]
fn all_archetypes_is_every_archetype_race_to_archetype_can_return() {
    let body = MODELS_RS
        .split_once("pub fn race_to_archetype(")
        .expect("race_to_archetype not found in models.rs").1;
    let body = body.split_once("\npub fn ").map(|(b, _)| b).unwrap_or(body);

    // Anti-escape: the extractor below only sees `=> "…"`. Any other arm spelling would be silently
    // dropped from `from_source` and the set-equality would then compare a short list against a
    // short list and pass. Requiring the two counts to match means an unparseable arm cannot hide.
    // 19/19 today. (This assumes no `=>` appears in a comment or string inside the parsed region —
    // which the equality itself enforces, since such an arrow would have no matching `=> "`.)
    assert_eq!(
        body.matches("=>").count(),
        body.matches("=> \"").count(),
        "every arm of race_to_archetype must return a string literal spelled `=> \"…\"` — the \
         archetype parser below only sees that spelling, so an arm written `=> {{ \"x\" }}` or \
         `=> NAMED_CONST` would be invisible to it and to the property it feeds",
    );

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
///
/// ## This parser examined for #769's escape class, and what is left open
///
/// #769 asked whether this sibling has the same "one keystroke away from blind" hole. It has one,
/// and it is closed below: `match_indices("static_placement(")` requires the name and the paren to
/// be adjacent, so a call written `static_placement (…)` is invisible — `calls.len()` stays 4 and
/// both halves stay 2, i.e. green with an unreviewed call site. `cargo fmt` would remove that space,
/// but this repo runs no fmt job in CI, so the assert is the guard.
///
/// Four residual holes are NOT closed, stated rather than papered over:
/// - **A call site in another file.** This test reads only `pass.rs`. A `static_placement` call
///   added to a new render module is invisible to it. What still forces a decision there is the
///   API, not this test: `floating` is a required argument with no default (#756). "Four call
///   sites" is a count of `pass.rs` call sites; it is not a bound on callers elsewhere.
/// - **A fifth producer of the flag, outside `pass.rs`.** `Billboard.floating` is set in two
///   places: `Scene::from_game_state` derives it (`src/scene.rs:337`, `e.floating()`), and
///   `Scene::inject_test_billboards` hardcodes `floating: false` (`src/scene.rs:248`). The
///   hardcode is not currently reachable by a static-arm model: it is called only when
///   `self.scene.zone == "testzone"` (`src/app.rs:1267-1269`), and its race table
///   (`src/scene.rs:189-205`) lists 16 character races with no `SHP` entry, so the one model that
///   takes the static arm today (`boat.glb`) is never injected. That chain holds by asset and
///   zone facts, not by construction — adding `SHP` to that table would grade a grounded static
///   model with a hardcoded flag, and nothing here would go red.
/// - **A call whose argument list contains `);` before its own close** — the slice would end early
///   and the two halves would read a truncated call. No such call exists today (that needs a
///   statement inside an argument, e.g. a block or closure body).
/// - **A trailing comma after `false`.** `player` matches on `ends_with("false")`, so
///   `…, false,\n)` would count 0 and this test would go RED on correct code. That direction is
///   loud rather than silent, so it is left as-is.
#[test]
fn every_static_placement_call_site_in_pass_rs_decides_floating_explicitly() {
    // Anti-escape for the extractor immediately below (#769): it only sees the name and the paren
    // adjacent, so a space between them would drop a whole call site out of every count here.
    assert!(!PASS_RS.contains("static_placement ("),
        "a `static_placement (…)` call site (space before the paren) is invisible to the parser \
         below, which would leave this pin green with an unreviewed call site");

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
         passing `false` there puts the hull above the water (13.9275u before #768 corrected the \
         grounded lift, 3.9823u after) and no other test in this crate would notice; \
         found {from_spawn} of 2");
    assert_eq!(player, 2,
        "the player pass and the player shadow-caster arm must pass a literal `false`; \
         found {player} of 2");
}

/// **#768's lift, pinned at both source-text channels through which `pass.rs` can re-create it.**
/// The behavioural tests above cannot reach either one: they call `static_placement` with their own
/// arguments, so they grade the helpers, never the calls.
///
/// There are exactly two channels. Both were measured green on `41cca4e` before #781, and both are
/// now **compile errors in the spelling that was measured** — but neither is unrepresentable, and
/// this doc does not say it is.
///
/// 1. **Out of the placement, into the matrix.** Before #781 the four call sites passed a literal
///    `0.0` for `entity_model_matrix_heading`'s `visual_scale`, so handing it
///    `2.0 * model.y_extent * p.mesh_scale` restored the pre-#768 over-lift. Measured on `41cca4e`
///    with that edit at the entity call site: crate `--no-fail-fast` **261 passed / 1 failed /
///    12 ignored**, the one failure being this test. Since #781 the static sites call
///    `camera::entity_model_matrix_static`, which has **no `visual_scale` parameter**, so that exact
///    edit no longer compiles (measured: `error[E0061]: this function takes 4 arguments but 5
///    arguments were supplied`). What is left open at the type level is calling
///    `entity_model_matrix_heading` directly instead — a different function NAME. What bounds THAT
///    is the whitelist at the bottom of this test, and only for `pass.rs`. #781 round 1 wrote it as
///    a blacklist of the field names `.y_bottom` / `.center_xz`, and #828's reviewer measured the
///    over-lift written through `model.bounds.y_extent` / `.x_center` / `.z_center` as **fully
///    green** against it (262 passed / 0 failed / 12 ignored); it is a whitelist of the six
///    reviewed argument lists since.
/// 2. **Into the placement.** Before #781 `static_placement` took a bare `y_bottom: f32`, so
///    `static_placement(archetype, model.y_bottom + model.y_extent, …)` restored the identical
///    pre-#768 lift. Measured on `41cca4e`: **261 passed / 1 failed / 12 ignored**, the failure
///    being this test's `REVIEWED_ARGS` assert. (Before #773 extended this pin it was measured
///    GREEN at 215 / 0 by PR #773's round-1 reviewer — that is why the pin exists at all.) Since
///    #781 `static_placement` takes `&ModelBounds`, so that edit no longer compiles either. WHICH
///    error depends on how it is transcribed, and both were measured on this branch, because round
///    1 reported only the second: transcribed literally, the pre-#781 spelling also still passes a
///    separate `center_xz` argument, so it is `error[E0061]: this function takes 3 arguments but 4
///    arguments were supplied`, with `expected &ModelBounds, found f32` as a sub-note rather than a
///    standalone error; transcribed minimally onto the 3-argument signature it is `error[E0308]:
///    mismatched types`, expected `&ModelBounds`, found `f32`. Neither compiles.
///
/// **What #781 leaves possible, measured, not reasoned.** `ModelBounds` has public fields, so the
/// same over-lift can be written as a struct literal at the call site —
/// `&ModelBounds { y_bottom: model.bounds.y_bottom + model.bounds.y_extent, ..model.bounds }` —
/// and that compiles. It is caught by `REVIEWED_ARGS` below and by nothing else in this crate
/// (measured: **261 passed / 1 failed**, this test). So #781 changed the two measured evasions from
/// *type-checks-and-only-a-text-pin-catches-it* to *does-not-type-check*, and left a third,
/// louder-to-write shape in the first category. It did **not** make the bad state unrepresentable;
/// the newtype that would is declined for the reason on `models::ModelBounds`.
///
/// Both halves are closed by pinning the whole argument list, not one argument: an expression is not
/// something source text can bound (`model.bounds` vs a helper call returning the same value are
/// both "the second argument"), so the test requires each call to be written EXACTLY as one of the
/// reviewed spellings. **The consequence is deliberate and is the loud direction**: renaming the
/// `model` binding, or adding a fifth site with any other argument, turns this RED on correct code
/// and asks for a review.
///
/// **What this still does not do:**
/// - It is source text, not semantics. It proves the argument is *written* that way, not that the
///   call is reached, and not that `model.bounds` itself holds what the loader intended. PR #773's
///   reviewer measured two instances of exactly this and both are still green today: rebinding
///   `model` to a local with the same field names (E1b), and corrupting the loader's reduction that
///   produces `y_bottom` (E2). #828's reviewer measured a third, on this branch: shadowing `p`
///   between the two reviewed calls with a hand-built `StaticPlacement` carrying
///   `p.y_bottom + model.bounds.y_extent`, which leaves all four argument texts byte-identical —
///   **262 passed / 0 failed / 12 ignored**. #781 addresses none of the three.
/// - It only reads `pass.rs`. A static placement built in another file is invisible to it, and
///   "four call sites" is a count of `pass.rs` call sites — it is not a bound on callers of
///   `static_placement` anywhere else in the workspace.
/// - Whitespace is normalized and trailing commas are stripped — `trim_end_matches(',')` removes
///   every one of them, not one — so a re-wrapped argument list is not a violation; a renamed
///   *variable* is.
#[test]
fn every_static_placement_in_pass_rs_is_written_exactly_as_reviewed() {
    /// Extract every call to `name(` in `pass.rs`, whitespace-normalized, argument list only.
    ///
    /// The extractor needs the name and the paren adjacent, so the caller asserts the
    /// `name (` spelling is absent first (#769's escape class).
    fn arg_lists(name: &str) -> Vec<String> {
        PASS_RS
            .match_indices(&format!("{name}("))
            .map(|(i, _)| {
                // Depth-count to the paren that closes THIS call. The `rest.find(");")` this used
                // before #828 round 2 stops at the first `);` anywhere after the call, so a call
                // NESTED in another — `from_cols_array_2d(&entity_model_matrix_heading(…))` at
                // `pass.rs:1447` and `:1850` — captured one `)` too many. That was harmless while
                // the only consumer was a `contains` check; it is not harmless for the exact-match
                // whitelists below, which would otherwise need two entries per spelling differing
                // only by a trailing paren. Assumes parens inside an argument list are balanced —
                // an unbalanced one in a string literal would over-capture, which fails these
                // whitelists loudly rather than silently.
                let open = i + name.len();
                let mut depth = 0usize;
                let mut end = None;
                for (k, c) in PASS_RS.as_bytes().iter().enumerate().skip(open) {
                    match c {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 { end = Some(k); break; }
                        }
                        _ => {}
                    }
                }
                let end = end.unwrap_or_else(|| panic!("unterminated {name}( call in pass.rs"));
                let norm = PASS_RS[open + 1..end].split_whitespace().collect::<Vec<_>>().join(" ");
                norm.trim().trim_end_matches(',').trim().to_string()
            })
            .collect()
    }

    for name in ["static_placement", "entity_model_matrix_static", "entity_model_matrix_heading"] {
        assert!(!PASS_RS.contains(&format!("{name} (")),
            "a `{name} (…)` call (space before the paren) is invisible to the parser in this \
             test, which would leave the pin green with an unreviewed call site");
    }

    // ── Channel 2: what goes IN to static_placement ──────────────────────────────────────────
    // (the sibling test above grades the `floating` argument's provenance and counts the sites;
    // this grades the exact spelling of all arguments, which is what bounds the lift.)
    const REVIEWED_ARGS: [&str; 2] = [
        "archetype, &model.bounds, false",
        "archetype, &model.bounds, b.floating",
    ];
    let placements = arg_lists("static_placement");
    assert_eq!(placements.len(), 4,
        "pass.rs must call static_placement at exactly the 4 known sites; found {}",
        placements.len());
    for args in &placements {
        assert!(REVIEWED_ARGS.contains(&args.as_str()),
            "#768/#781: a static model's whole vertical lift is its `y_bottom`, so each call \
             site must pass the model's OWN measured bounds and nothing derived from them. \
             Since #781 the parameter is `&ModelBounds`, so the measured \
             `model.y_bottom + model.y_extent` evasion no longer compiles — but a struct \
             literal such as `&ModelBounds {{ y_bottom: model.bounds.y_bottom + \
             model.bounds.y_extent, ..model.bounds }}` still does, and restores the exact \
             pre-#768 over-lift with no behavioural test in this crate failing. This assert \
             is what catches it. Expected one of {REVIEWED_ARGS:?}, found: {args}");
    }

    // ── Channel 1: what comes OUT of the placement, into the matrix ──────────────────────────
    // Since #781 the static sites call `entity_model_matrix_static`, which has no `visual_scale`
    // parameter. Two things are graded: that the four calls are spelled as reviewed, and that no
    // static placement reaches the general `entity_model_matrix_heading` (which does have one).
    const REVIEWED_MATRIX_ARGS: [&str; 4] = [
        "scene.player_pos, scene.player_heading, &p, crate::models::archetype_correction(archetype)",
        "b.pos, b.heading, &p, crate::models::archetype_correction(archetype)",
        "scene.player_pos, scene.player_heading, &p, archetype_correction(archetype)",
        "b.pos, b.heading, &p, archetype_correction(archetype)",
    ];
    let statics = arg_lists("entity_model_matrix_static");
    assert_eq!(statics.len(), 4,
        "pass.rs must build exactly the 4 known static-model matrices (entity, player, and both \
         static shadow-caster arms); found {}", statics.len());
    for args in &statics {
        assert!(REVIEWED_MATRIX_ARGS.contains(&args.as_str()),
            "#768/#781: a static model's whole vertical lift comes from the placement's `y_bottom`, \
             and `entity_model_matrix_static` binds `visual_scale = 0.0` internally so there is no \
             lift argument here. This pins the argument TEXT: the matrix must be built from `&p`, \
             the placement spelled one line above. It does NOT bind what `p` denotes — shadowing it \
             with a hand-built `StaticPlacement` carrying `p.y_bottom + model.bounds.y_extent` \
             leaves this text unchanged and was measured fully green by #828's reviewer, and \
             `camera::entity_model_matrix_static`'s doc discloses the same capability. Expected one \
             of {REVIEWED_MATRIX_ARGS:?}, found: {args}");
    }

    // The escape #781 leaves open at the type level: call the GENERAL matrix function, which still
    // takes a `visual_scale`, and hand it a static model's numbers.
    //
    // #781 round 1 wrote this as a BLACKLIST — no `entity_model_matrix_heading` argument list may
    // contain `.y_bottom` or `.center_xz` — reasoning that those are `StaticPlacement`'s two fields
    // `HumanoidPlacement` does not have, so their presence "identifies a static placement". That
    // reasoning was wrong in both directions, and #828's reviewer MEASURED the load-bearing one:
    //
    // - **False negative, measured, fully green.** `GpuStaticModel::bounds` is in scope at all four
    //   static sites and carries the same numbers under other names. Adding, at `pass.rs:1672`, a
    //   direct `entity_model_matrix_heading(b.pos, b.heading, 2.0 * model.bounds.y_extent *
    //   p.mesh_scale, p.mesh_scale, [model.bounds.x_center, model.bounds.z_center], true, 0.0, …)`
    //   restores the whole #768 over-lift without spelling either blacklisted field: **262 passed /
    //   0 failed / 12 ignored**, byte-identical to the green baseline. That spelling is not exotic —
    //   `[model.x_center, model.z_center]` is literally what all four of these sites passed to
    //   `static_placement` on `41cca4e`, before #781 folded the fields into `&model.bounds`. And a
    //   field name need not appear at all: binding the numbers to plain locals first
    //   (`let lift = 2.0 * model.bounds.y_extent * p.mesh_scale; let ctr = p.center_xz;`) leaves an
    //   argument list of `b.pos, b.heading, lift, p.mesh_scale, ctr, true, 0.0, …`, which was ALSO
    //   measured green against the blacklist — 262 passed / 0 failed / 12 ignored — and which no
    //   list of field names can catch.
    // - **False positive.** `GpuSkinnedModel` has `y_bottom` / `x_center` / `z_center` too
    //   (`gpu.rs`), so `.y_bottom` in an argument list never did identify a *static* placement; a
    //   future skinned site passing `model.y_bottom` would have tripped an assert naming a bug it
    //   did not have.
    //
    // A blacklist has to enumerate the aliases of four numbers, and they are not enumerable. A
    // whitelist has to enumerate the legitimate calls, and there are six, in one file: four
    // per-model matrices (player `:1356`, skinned entity `:1818`, and the two shadow-pass arms
    // `:2070`/`:2103`) and two held-item matrices (the player's `:1447` in `encode_player_pass` and
    // a spawn's `:1850` in `encode_skinned_entity_pass` — all six are on skinned paths, and the
    // second held-item one IS an entity site; round 1 called the pair "the two non-entity ones").
    // So this is the same trade the two asserts above already make.
    //
    // **What this bounds, and what it does not.** It bounds the argument TEXT of every
    // `entity_model_matrix_heading` call in `pass.rs`, and their count. It does not bound what the
    // names in a reviewed text denote, which is the hole the `&p` assert above also has and
    // discloses; and it reads no other file.
    const REVIEWED_HEADING_ARGS: [&str; 4] = [
        "scene.player_pos, scene.player_heading, visual_scale, dominant_mesh_scale, [0.0, 0.0], \
         true, 0.0, crate::models::archetype_correction(archetype)",
        "b.pos, b.heading, visual_scale, dominant_scale, [0.0, 0.0], true, 0.0, \
         crate::models::archetype_correction(archetype)",
        "scene.player_pos, scene.player_heading, p.visual_scale, p.mesh_scale, [0.0, 0.0], true, \
         0.0, archetype_correction(archetype)",
        "b.pos, b.heading, visual_scale, dominant_scale, [0.0, 0.0], true, 0.0, \
         archetype_correction(archetype)",
    ];
    let headings = arg_lists("entity_model_matrix_heading");
    // The per-call whitelist runs BEFORE the count assert deliberately: an ADDED call with a novel
    // argument list must fail here, naming the offending arguments, rather than fail on a count
    // that says only "six became seven". The count then catches the one shape the whitelist cannot
    // — an added call whose argument list is byte-identical to a reviewed one, whose names denote
    // something else at the new site. Both orders are measured; see the PR's mutation table.
    for args in &headings {
        assert!(REVIEWED_HEADING_ARGS.contains(&args.as_str()),
            "#768/#781: `entity_model_matrix_heading` adds `visual_scale * 0.5` ON TOP of \
             `y_bottom * mesh_scale`, which is exactly the #768 over-lift, and a static model's \
             numbers are reachable at every static call site under several names \
             (`model.bounds.y_extent`, a local bound from it, …). Enumerating those names is not \
             possible, so this enumerates the legitimate CALLS instead: every \
             `entity_model_matrix_heading` in pass.rs must be spelled as reviewed. A new one is not \
             necessarily wrong — it has to be reviewed for the #768 lift, which is why the list is \
             pinned. Expected one of {REVIEWED_HEADING_ARGS:?}, found: {args}");
    }
    assert_eq!(headings.len(), 6,
        "pass.rs must call entity_model_matrix_heading at exactly the 6 known skinned sites (four \
         per-model matrices, two held-item matrices); found {}. This catches the one addition the \
         whitelist above cannot: a call copied verbatim from a reviewed site into a static one, \
         where the same argument names denote a static model's numbers.", headings.len());
}
