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
//! 1. **`boat.glb` is the only model `model_for` can load that lands on the STATIC arm.** State the
//!    gate first, because an earlier version of this note stated the wrong one: `renderer.rs:663-664`
//!    picks the skinned path when `0 < joint_count <= 128`, so the static arm is
//!    `!(0 < joint_count <= 128)` — no skin, a skin with zero joints, **or a skin with more than 128
//!    joints**. `skins == 0` is therefore *sufficient but not necessary*, and a scan that only reads
//!    `len(skins)` does not establish which arm a model takes.
//!
//!    Re-scanned on 2026-07-28 reading `len(skins[0].joints)` as well. Loadable names: the 18
//!    distinct archetypes `race_to_archetype` can return (`humanoid`, `elf`, `dwarf`, `gnoll`,
//!    `skeleton`, `zombie`, `creature`, `rat`, `snake`, `frog`, `wasp`, `wolf`, `bat`, `bird`,
//!    `worm`, `fish`, `bear`, `boat`) plus the 29 `race_*` player models plus the 3 `<key>_f.glb`
//!    female variants that exist (`humanoid_f`, `elf_f`, `dwarf_f`; `model_for` prefers them for
//!    `gender == 1`, `renderer.rs:610`) — **50 files, all present**. Exactly one lands on the static
//!    arm: `boat.glb`, `skins == 0`, 0 joints. Nothing is above the cap. The remaining files on disk
//!    are zone/door/weapon assets `model_for` never names.
//!
//!    **This is a margin of one joint on a directory rebaked outside this repo.**
//!    `race_pcfroglok.glb` sits at **127** joints against the 128 cap, and 11 rigs are at ≥ 109
//!    (next highest 110). A two-joint rebake of that file moves a PC race — for which
//!    `Entity::floating()` is false — onto the grounded arm. The grounded arm being unreached today
//!    is a fact about the current asset bake, not a property of the code.
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

/// Build the production model matrix for a static `boat.glb` entity — the exact calls the render
/// passes make (`models::static_placement` fed into `camera::entity_model_matrix_heading`), with the
/// literal `0.0` the static call sites pass for `visual_scale` since #768.
///
/// Column-major (`glam`'s `to_cols_array_2d`), so `m[col][row]`.
fn boat_matrix(pos: [f32; 3], heading: f32, floating: bool) -> [[f32; 4]; 4] {
    let p = static_placement("boat", boat_y_bottom(), boat_center_xz(), floating);
    entity_model_matrix_heading(
        pos, heading, 0.0, p.mesh_scale, p.center_xz, true, p.y_bottom,
        archetype_correction("boat"),
    )
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
    let grounded = static_placement("boat", boat_y_bottom(), boat_center_xz(), false);
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
    let floating = static_placement("boat", boat_y_bottom(), boat_center_xz(), true);
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
        let p = static_placement("boat", boat_y_bottom(), boat_center_xz(), floating);
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
/// arguments and build the matrix with a literal `0.0`, so they grade the helper, never the call.
///
/// There are exactly two channels, and both were measured green before being closed:
///
/// 1. **Out of the placement, into the matrix.** `model` is in scope at all four call sites and
///    `GpuStaticModel::y_extent` is public, so `2.0 * model.y_extent * p.mesh_scale` can be handed
///    to `entity_model_matrix_heading`'s `visual_scale`. Measured with this test present, that edit
///    at the entity call site reddens this test and nothing else in the crate:
///    `--no-fail-fast` over the whole crate gave **214 passed / 1 failed / 11 ignored**, the one
///    failure being this test. (Without `--no-fail-fast` cargo stops after the failing binary and
///    only 2 of the 10 binaries report — a run that reddens here is not evidence about the rest.)
/// 2. **Into the placement.** Nothing about dropping the `visual_scale` FIELD constrains what is
///    passed as `y_bottom`. `static_placement(archetype, model.y_bottom + model.y_extent, …)`
///    restores the identical pre-#768 lift `(y_extent + y_bottom) * mesh_scale`. Measured by the
///    round-1 reviewer of PR #773 and reproduced here before writing anything down: with only the
///    channel-1 half present, the crate stayed **green at 215 passed / 0 failed / 11 ignored**, this
///    test included — it read the matrix call, not the placement call. That is the finding this test
///    was extended for; with the `REVIEWED_ARGS` check below, the same mutation now fails on the
///    `REVIEWED_ARGS` assert (10 passed / 1 failed in this binary), printing the offending call.
///
/// Channel 2 is closed by pinning the whole argument list, not one argument: an expression is not
/// something source text can bound (`model.y_bottom` vs `model.y_bottom + 0.0` vs a helper call are
/// all "the second argument"), so the test instead requires each call to be written EXACTLY as one
/// of two reviewed spellings. **The consequence is deliberate and is the loud direction**: renaming
/// the `model` binding, or adding a fifth site with any other argument, turns this RED on correct
/// code and asks for a review. That is the trade this repo's verification hierarchy prefers over a
/// comment.
///
/// **What this still does not do**, stated because the previous version of this doc overstated it:
/// - It is source text, not semantics. It proves the argument is *written* that way, not that the
///   call is reached, and not that `model.y_bottom` itself holds what the loader intended.
/// - It only reads `pass.rs`. A static placement built in another file is invisible to it, and
///   "four call sites" is a count of `pass.rs` call sites — it is not a bound on callers of
///   `static_placement` anywhere else in the workspace.
/// - Whitespace is normalized, so a re-wrapped argument list is not a violation; a renamed
///   *variable* is.
#[test]
fn every_static_placement_in_pass_rs_is_written_exactly_as_reviewed() {
    // Anti-escape, same class as #769's: the extractor needs the name and the paren adjacent.
    assert!(!PASS_RS.contains("entity_model_matrix_heading ("),
        "an `entity_model_matrix_heading (…)` call (space before the paren) is invisible to the \
         parser below");

    // ── Channel 2: what goes IN to static_placement ──────────────────────────────────────────
    // (the sibling test above grades the `floating` argument's provenance and counts the sites;
    // this grades the exact spelling of all four arguments, which is what bounds the lift.)
    const REVIEWED_ARGS: [&str; 2] = [
        "archetype, model.y_bottom, [model.x_center, model.z_center], false",
        "archetype, model.y_bottom, [model.x_center, model.z_center], b.floating",
    ];
    let placements: Vec<String> = PASS_RS
        .match_indices("static_placement(")
        .map(|(i, _)| {
            let rest = &PASS_RS[i..];
            let call = &rest[..rest.find(");").expect("unterminated static_placement( call")];
            call.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .collect();
    assert_eq!(placements.len(), 4,
        "pass.rs must call static_placement at exactly the 4 known sites; found {}",
        placements.len());
    for call in &placements {
        let args = call.split_once('(').expect("no argument list").1.trim();
        assert!(REVIEWED_ARGS.contains(&args),
            "#768: a static model's whole vertical lift is the `y_bottom` argument, so each call \
             site must pass the model's OWN measured bounds and nothing derived from them — \
             `model.y_bottom + model.y_extent` here restores the exact pre-#768 over-lift and no \
             behavioural test in this crate would fail. Expected one of {REVIEWED_ARGS:?}, found: \
             {args}");
    }

    // ── Channel 1: what comes OUT of the placement, into the matrix ──────────────────────────
    let statics: Vec<String> = PASS_RS
        .match_indices("entity_model_matrix_heading(")
        .map(|(i, _)| {
            let rest = &PASS_RS[i..];
            let call = &rest[..rest.find(");").expect("unterminated matrix call in pass.rs")];
            call.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        // A static placement is the only thing that feeds `p.y_bottom` into this call; the skinned
        // and non-entity sites do not.
        .filter(|c| c.contains("p.y_bottom"))
        .collect();

    assert_eq!(statics.len(), 4,
        "pass.rs must build exactly the 4 known static-model matrices (entity, player, and both \
         static shadow-caster arms); found {}", statics.len());

    for call in &statics {
        assert!(call.contains(", 0.0, p.mesh_scale"),
            "#768: a static model's whole vertical lift comes from `p.y_bottom`, so every static \
             call site must pass the literal `0.0` for `visual_scale` (the argument immediately \
             before `mesh_scale`). `entity_model_matrix_heading` adds `visual_scale * 0.5` on top \
             of `y_bottom * mesh_scale`, so anything else here re-creates the over-lift that put \
             the model a full rendered height above its stored z. Offending call: {call}");
    }
}
