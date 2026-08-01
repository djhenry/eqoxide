//! Regression coverage for the shadow ambient-floor darkening (eqoxide#614, follow-up to #518).
//!
//! The owner's feedback on the #518 sun-shadow demo was that the shadowed-terrain floor (how much
//! brightness a fully-shadowed fragment keeps instead of going black) was too faint to read: "I'd
//! like them to be a little darker and more noticeable. I can barely see it." A live A/B of
//! 0.45 (original) / 0.35 / 0.25 / 0.15 at identical camera/zone/hour picked 0.25 — dark enough to
//! read clearly, without flattening the shadowed area's texture detail into a silhouette the way
//! 0.15 started to (see the PR description for the comparison screenshots and reasoning).
//!
//! `zone.wgsl` (terrain) and `zone_instanced.wgsl` (placed objects) each define their own
//! `apply_shadow` with the same `mix(FLOOR, 1.0, shadow_factor(world_pos))` literal — WGSL has no
//! `#include`/module system, so `include_str!`ing each `.wgsl` file straight into its own
//! `ShaderModuleDescriptor` (see src/pipeline.rs) is the only option, and the floor constant is
//! necessarily duplicated source text rather than a single shared symbol. If the two literals ever
//! drift, terrain and placed objects (a building vs. the ground next to it, say) would visibly
//! shadow to different darkness levels. This file enforces they stay equal at the source-string
//! level — the only enforcement mechanism available without a build-time preprocessor or touching
//! pipeline.rs (out of scope for #614, and would be a much bigger change for a one-constant fix).
//!
//! There's no GPU device in this crate's test harness (see fog_shader.rs / weather_shader.rs for
//! the established pattern), so this can't render a frame and read pixels back. Instead it:
//!   1. Extracts the numeric floor literal from each shader's `apply_shadow` function via a
//!      source-level string search (no naga needed for this part — it's the *literal text*, not
//!      the parsed AST, that must match) and asserts they're equal.
//!   2. Asserts the extracted value equals the intended 0.25 in both files — catches someone
//!      "fixing" only one file (or fat-fingering a different number into both) as a distinct
//!      failure from a same-but-wrong drift.
//!   3. Re-confirms both shaders still parse/validate as legal WGSL, matching the fog_shader.rs /
//!      weather_shader.rs precedent (this would already be caught there, but a shadow-focused test
//!      file should not depend on an unrelated test file to catch a broken `apply_shadow` edit).

const ZONE_WGSL: &str = include_str!("../src/shaders/zone.wgsl");
const ZONE_INSTANCED_WGSL: &str = include_str!("../src/shaders/zone_instanced.wgsl");
const SHADOW_WGSL: &str = include_str!("../src/shaders/shadow.wgsl");
const SHADOW_MASKED_INSTANCED_WGSL: &str = include_str!("../src/shaders/shadow_masked_instanced.wgsl");

/// The value picked in the eqoxide#614 A/B (see this file's module doc). Both shaders must carry
/// exactly this literal in their `apply_shadow` function.
const EXPECTED_AMBIENT_FLOOR: f32 = 0.25;

/// The alpha-test cutout threshold used by every masked-material fragment shader in the renderer
/// (zone.wgsl's `fs_main`, zone_instanced.wgsl's `fs_main`, and — since eqoxide#707 —
/// shadow_masked_instanced.wgsl's `fs_instanced_masked`). All three MUST agree: if the shadow pass's
/// cutout diverges from the color pass's, the shadow silhouette disagrees with the rendered
/// silhouette, which is the exact bug class #707 fixes.
const EXPECTED_ALPHA_CUTOUT: f32 = 0.5;

fn parse_and_validate(source: &str, label: &str) -> naga::Module {
    // The `.wgsl` files are not standalone WGSL: they carry the `JOINT_CAP` placeholder that
    // `pipeline::wgsl` substitutes on the way into `create_shader_module` (eqoxide#798). Parse the
    // SAME text the GPU is handed, not the raw file, or a joint-palette shader won't even parse.
    let source = eqoxide_renderer::pipeline::wgsl(source);
    let module = naga::front::wgsl::parse_str(&source)
        .unwrap_or_else(|e| panic!("{label}: WGSL failed to parse: {e}"));
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|e| panic!("{label}: WGSL failed validation: {e}"));
    module
}

/// Extracts the `X` in `apply_shadow`'s `return color * mix(X, 1.0, shadow_factor(world_pos));` —
/// the raw, trimmed *source text* of the literal, via a string search — and separately its
/// parsed `f32` value. The drift check between the two shader files (below) compares the string
/// form, not the parsed form: `"0.250"` and `"0.25"` parse equal as floats but are not the same
/// source text, and a maintainer diffing the two files would see them disagree. Comparing the
/// string keeps that visible-drift case failing loudly instead of silently passing.
fn extract_shadow_floor_literal<'a>(source: &'a str, label: &str) -> &'a str {
    let fn_start = source
        .find("fn apply_shadow(")
        .unwrap_or_else(|| panic!("{label}: couldn't find `fn apply_shadow(`"));
    let body = &source[fn_start..];
    let mix_marker = "mix(";
    let mix_at = body
        .find(mix_marker)
        .unwrap_or_else(|| panic!("{label}: apply_shadow body doesn't contain `mix(`"));
    let after_mix = &body[mix_at + mix_marker.len()..];
    let comma_at = after_mix
        .find(',')
        .unwrap_or_else(|| panic!("{label}: couldn't find the `,` after `mix(` in apply_shadow"));
    after_mix[..comma_at].trim()
}

/// Parsed `f32` form of [`extract_shadow_floor_literal`], for the range/value checks below where
/// numeric equality (not source-text equality) is the right comparison.
fn extract_shadow_floor(source: &str, label: &str) -> f32 {
    let literal = extract_shadow_floor_literal(source, label);
    literal
        .parse::<f32>()
        .unwrap_or_else(|e| panic!("{label}: `{literal}` (the mix() floor argument) isn't a valid f32 literal: {e}"))
}

/// Extracts the `X` in `if (texel.a < X) { discard; }` — the raw, trimmed source text of the
/// alpha-test cutout threshold — via a string search (same technique as
/// `extract_shadow_floor_literal` above, for the same "no #include" reason).
fn extract_alpha_cutout_literal<'a>(source: &'a str, label: &str) -> &'a str {
    let marker = "texel.a < ";
    let at = source
        .find(marker)
        .unwrap_or_else(|| panic!("{label}: couldn't find `{marker}` (no alpha-test cutout?)"));
    let after = &source[at + marker.len()..];
    let paren_at = after
        .find(')')
        .unwrap_or_else(|| panic!("{label}: couldn't find the `)` closing the `texel.a < ...` condition"));
    after[..paren_at].trim()
}

/// Parsed `f32` form of [`extract_alpha_cutout_literal`].
fn extract_alpha_cutout(source: &str, label: &str) -> f32 {
    let literal = extract_alpha_cutout_literal(source, label);
    literal
        .parse::<f32>()
        .unwrap_or_else(|e| panic!("{label}: `{literal}` (the alpha-cutout threshold) isn't a valid f32 literal: {e}"))
}

/// Recursively walks a naga IR statement block — including nested `if`/`loop`/`switch`/`block`
/// statements — looking for a `discard;` statement, which naga's parsed & validated IR represents as
/// [`naga::Statement::Kill`] regardless of how it's spelled, commented around, or where it sits in
/// the source file. Operating on the IR rather than source text is what makes this immune to comment
/// style (line or block), to substrings inside identifiers (e.g. a local named
/// `discard_threshold`), and to entry-point ordering in the file — see the doc comment on
/// `shadow_masked_alpha_cutout_actually_discards` below for why a source-text version of this check
/// failed against exactly those three things, across two independent-review rounds.
fn block_has_kill(block: &naga::Block) -> bool {
    block.iter().any(|stmt| match stmt {
        naga::Statement::Kill => true,
        naga::Statement::If { accept, reject, .. } => block_has_kill(accept) || block_has_kill(reject),
        naga::Statement::Block(inner) => block_has_kill(inner),
        naga::Statement::Loop { body, continuing, .. } => {
            block_has_kill(body) || block_has_kill(continuing)
        }
        naga::Statement::Switch { cases, .. } => cases.iter().any(|c| block_has_kill(&c.body)),
        _ => false,
    })
}

/// Looks up the entry point named `entry_point_name` in an already-parsed-and-validated
/// [`naga::Module`] and asks [`block_has_kill`] whether its body contains a `discard` anywhere in its
/// control flow. Targeting a SPECIFIC named entry point (rather than "the first cutout-shaped region
/// found in the raw source") is what makes this immune to a second, unrelated
/// `if (texel.a < ...) { discard; }` appearing earlier in the same file.
fn entry_point_kills(module: &naga::Module, entry_point_name: &str, label: &str) -> bool {
    let ep = module
        .entry_points
        .iter()
        .find(|ep| ep.name == entry_point_name)
        .unwrap_or_else(|| panic!("{label}: no entry point named `{entry_point_name}` in the parsed module"));
    block_has_kill(&ep.function.body)
}

/// Extracts the raw source text of the EQ WLD → render axis swizzle
/// (`let render = vec3<f32>(world.z, world.x, world.y);`) from a vertex shader's instanced entry
/// point — the same string-search technique as the other `extract_*_literal` helpers above, and for
/// the same reason (WGSL has no `#include`, so this line is duplicated source text across
/// `shadow.wgsl`, `zone_instanced.wgsl`, and `shadow_masked_instanced.wgsl`).
fn extract_render_swizzle_literal<'a>(source: &'a str, label: &str) -> &'a str {
    let marker = "let render = vec3<f32>(";
    let at = source
        .find(marker)
        .unwrap_or_else(|| panic!("{label}: couldn't find `{marker}` (no EQ->render swizzle?)"));
    let after = &source[at + marker.len()..];
    let paren_at = after
        .find(')')
        .unwrap_or_else(|| panic!("{label}: couldn't find the `)` closing the swizzle's vec3<f32>(...)"));
    after[..paren_at].trim()
}

#[test]
fn zone_and_zone_instanced_wgsl_parse_and_validate() {
    parse_and_validate(ZONE_WGSL, "zone.wgsl");
    parse_and_validate(ZONE_INSTANCED_WGSL, "zone_instanced.wgsl");
}

/// The de-duplication guard requested by eqoxide#614: fails RED if `zone.wgsl` and
/// `zone_instanced.wgsl` ever disagree on the shadow ambient floor, which WGSL's lack of
/// `#include` makes otherwise silent (each file compiles fine on its own; only a live scene with
/// both terrain and a placed object in the same shadow would show the mismatch, and even then only
/// as "huh, that looks a little different," not an error).
#[test]
fn ambient_floor_matches_between_zone_and_zone_instanced() {
    let zone_literal = extract_shadow_floor_literal(ZONE_WGSL, "zone.wgsl");
    let instanced_literal = extract_shadow_floor_literal(ZONE_INSTANCED_WGSL, "zone_instanced.wgsl");
    assert_eq!(
        zone_literal, instanced_literal,
        "zone.wgsl's apply_shadow floor literal ({zone_literal}) must match zone_instanced.wgsl's \
         ({instanced_literal}) — terrain and placed objects must shadow to the same darkness. \
         Update both files together (this is a source-text comparison, so e.g. \"0.250\" vs \"0.25\" \
         also fails even though they're numerically equal)."
    );
}

/// Pins the extracted value to the eqoxide#614 A/B result, separately from the equality check
/// above (equal-but-wrong, e.g. both accidentally reset to 0.45, would pass the drift test but
/// silently undo the fix; this test catches that case).
#[test]
fn ambient_floor_matches_the_614_ab_result() {
    let zone_floor = extract_shadow_floor(ZONE_WGSL, "zone.wgsl");
    let instanced_floor = extract_shadow_floor(ZONE_INSTANCED_WGSL, "zone_instanced.wgsl");
    assert_eq!(
        zone_floor, EXPECTED_AMBIENT_FLOOR,
        "zone.wgsl's shadow floor drifted from the eqoxide#614 A/B result ({EXPECTED_AMBIENT_FLOOR}); \
         if this is an intentional re-tune, update EXPECTED_AMBIENT_FLOOR here too"
    );
    assert_eq!(
        instanced_floor, EXPECTED_AMBIENT_FLOOR,
        "zone_instanced.wgsl's shadow floor drifted from the eqoxide#614 A/B result ({EXPECTED_AMBIENT_FLOOR}); \
         if this is an intentional re-tune, update EXPECTED_AMBIENT_FLOOR here too"
    );
}

/// Sanity bound: a shadow floor outside [0,1) would either do nothing (>=1.0, shadow_factor's
/// `mix` becomes a no-op-ish range) or push shadowed fragments negative/brighter-than-lit (<0.0 or
/// the low/high ends swapped). This isn't a taste opinion like the 0.25 pin above — it's a "this
/// literal cannot possibly be a valid ambient floor" guard.
#[test]
fn ambient_floor_is_a_plausible_darkening_fraction() {
    for (source, label) in [(ZONE_WGSL, "zone.wgsl"), (ZONE_INSTANCED_WGSL, "zone_instanced.wgsl")] {
        let floor = extract_shadow_floor(source, label);
        assert!(
            (0.0..1.0).contains(&floor),
            "{label}: shadow floor {floor} is outside the plausible [0.0, 1.0) darkening range"
        );
    }
}

// ── eqoxide#707: masked-foliage shadow caster ──────────────────────────────────────────────────
//
// Tree shadows were square: the sun shadow-map depth pass (shadow.wgsl) had no fragment stage at
// all, so a `RenderMode::Masked` instanced caster (foliage/branches with a keyed-transparent
// texture) wrote full depth for every triangle of its quad instead of discarding the transparent
// texels the color pass (zone_instanced.wgsl/zone.wgsl) discards. The fix adds a fragment stage —
// `shadow_masked_instanced.wgsl`'s `fs_instanced_masked` — used only for `RenderMode::Masked`
// instanced casters (pass.rs routes by render_mode; `RenderMode::Opaque` casters keep using the
// cheap fragment-less `shadow_instanced` pipeline). These tests can't spin up a GPU device (no
// device in this crate's test harness — see this file's module doc), so they cover what's testable
// without one: the new shader parses/validates as legal WGSL, declares the expected entry points,
// and — the thing most likely to silently drift later — its alpha-test cutout threshold agrees with
// the color pass's.

#[test]
fn shadow_wgsl_and_shadow_masked_instanced_wgsl_parse_and_validate() {
    parse_and_validate(SHADOW_WGSL, "shadow.wgsl");
    let masked_module = parse_and_validate(SHADOW_MASKED_INSTANCED_WGSL, "shadow_masked_instanced.wgsl");

    // Structural guard: the masked pipeline (pipeline.rs) binds these two entry points by name: if
    // either is renamed/removed here without updating pipeline.rs, pipeline creation panics at
    // startup — but this test catches the drift at build time instead of at first launch.
    let entry_names: Vec<&str> = masked_module
        .entry_points
        .iter()
        .map(|ep| ep.name.as_str())
        .collect();
    assert!(
        entry_names.contains(&"vs_instanced_masked"),
        "shadow_masked_instanced.wgsl: missing `vs_instanced_masked` entry point (found {entry_names:?})"
    );
    assert!(
        entry_names.contains(&"fs_instanced_masked"),
        "shadow_masked_instanced.wgsl: missing `fs_instanced_masked` entry point (found {entry_names:?})"
    );
}

/// The core regression guard for #707: the shadow pass's masked-caster cutout *condition* must
/// agree with the color pass's, in both directions — the color shaders must still have theirs (in
/// case a future edit strips the discard the shadow fix is supposed to mirror), and the new shadow
/// shader must have picked up the SAME literal, not an independently-chosen one.
///
/// This only checks the `texel.a < X` condition text, not what the block guarded by that condition
/// actually does. An independent review of this PR (eqoxide#718) found that this test — and
/// `shadow_masked_alpha_cutout_matches_expected_value` below — both stay green even if
/// `fs_instanced_masked`'s `if (texel.a < 0.5) { }` body is emptied out, because naga parses and
/// validates an empty `if` block without complaint; an emptied body reproduces #707 exactly (every
/// texel compares against the cutout and then nothing happens, so the caster writes full depth
/// again). See `shadow_masked_alpha_cutout_actually_discards` below for the test that catches that
/// specific mutation; it is a separate, necessary check, not a superset of this one.
///
/// Mutation-checked: deleting `shadow_masked_instanced.wgsl` (or otherwise breaking
/// `SHADOW_MASKED_INSTANCED_WGSL`'s `include_str!` target) is a **compile error** for this entire
/// test binary, not a runtime failure of this specific test — there is no pre-fix `main` on which
/// this test exists to "go red" against, since the test and the fix landed together. What DOES turn
/// this test red at runtime is changing the threshold literal so the three files disagree, or so any
/// one drifts from `EXPECTED_ALPHA_CUTOUT` (covered by the sibling test below).
#[test]
fn shadow_masked_alpha_cutout_matches_color_pass() {
    let zone_literal = extract_alpha_cutout_literal(ZONE_WGSL, "zone.wgsl");
    let zone_instanced_literal = extract_alpha_cutout_literal(ZONE_INSTANCED_WGSL, "zone_instanced.wgsl");
    let shadow_masked_literal =
        extract_alpha_cutout_literal(SHADOW_MASKED_INSTANCED_WGSL, "shadow_masked_instanced.wgsl");

    assert_eq!(
        zone_literal, zone_instanced_literal,
        "zone.wgsl's alpha cutout ({zone_literal}) must match zone_instanced.wgsl's \
         ({zone_instanced_literal}) — both are color-pass cutouts for the same asset-server-decoded \
         textures."
    );
    assert_eq!(
        zone_literal, shadow_masked_literal,
        "zone.wgsl's alpha cutout ({zone_literal}) must match shadow_masked_instanced.wgsl's \
         ({shadow_masked_literal}) — a divergent shadow-pass threshold would make the shadow \
         silhouette disagree with the rendered silhouette (eqoxide#707)."
    );
}

/// Pins the extracted value, separately from the equality check above (equal-but-wrong — e.g. all
/// three accidentally retuned to some other number together — would pass the drift test but
/// silently change every masked material's cutout).
#[test]
fn shadow_masked_alpha_cutout_matches_expected_value() {
    for (source, label) in [
        (ZONE_WGSL, "zone.wgsl"),
        (ZONE_INSTANCED_WGSL, "zone_instanced.wgsl"),
        (SHADOW_MASKED_INSTANCED_WGSL, "shadow_masked_instanced.wgsl"),
    ] {
        let cutout = extract_alpha_cutout(source, label);
        assert_eq!(
            cutout, EXPECTED_ALPHA_CUTOUT,
            "{label}: alpha cutout {cutout} drifted from the expected {EXPECTED_ALPHA_CUTOUT}"
        );
    }
}

/// Structural guard added in response to an independent review (eqoxide#718 BLOCKING-2), and
/// REWRITTEN in response to a second independent-review round after the first version turned out to
/// prove less than its own doc comment claimed.
///
/// **Round 1** established that the two tests above only look at the `texel.a < X` *condition* —
/// never at what the `if` block controlled by that condition does. naga parses and validates
/// `if (texel.a < 0.5) { }` (an empty body) without complaint, so both tests above stay green even
/// when nothing is actually discarded — which reproduces #707 exactly. The round-1 fix was a
/// source-text helper (`extract_alpha_cutout_block`, since removed) that located the first
/// `texel.a < ` in the file and checked whether the following `{ ... }` block's text — with `//`
/// line comments stripped — contained the substring `"discard"`.
///
/// **Round 2** found that helper proved a *weaker* property than its doc comment claimed, via two
/// mutations against `shadow_masked_instanced.wgsl`'s `fs_instanced_masked`, both of which left every
/// test in this file green:
///   - **Mutation A**: `discard;` → `/* discard; */`. Only `//` line comments were stripped, so
///     `"discard"` survived inside a `/* ... */` block comment.
///   - **Mutation B**: an unbound `fs_unused_masked` entry point added *earlier* in the file, with
///     its own `if (texel.a < ...) { discard; }`, while the real `fs_instanced_masked` was emptied
///     out. The helper located the FIRST `texel.a < ` in the source, so it graded the decoy and never
///     looked at the entry point `pipeline.rs` actually binds.
///
/// This version drops the source-text search entirely and instead walks the parsed & validated
/// `naga::Module`'s IR: [`entry_point_kills`] looks up a SPECIFIC entry point by name and asks
/// [`block_has_kill`] whether a `Statement::Kill` (naga's IR form of `discard`) appears anywhere in
/// its control flow, recursively through nested `if`/`loop`/`switch`/`block` statements. This is
/// immune to comment style, to substrings inside identifiers, and to entry-point ordering in the
/// source file — mutations A and B above were both re-run against this version: both go RED, with
/// the other 8 tests in this file staying green, matching the pristine-green/mutant-red pattern the
/// round-1 test was supposed to establish in the first place.
///
/// **Two remaining limits, both known and both deliberate** (eqoxide#721, from #718's round-3
/// review):
///   - [`block_has_kill`] does not descend `Statement::Call`, so factoring the `discard` out into a
///     helper function called from the cutout branch makes this test go RED on a shader that is
///     *correct*. WGSL permits `discard` in any function reachable from a fragment stage. That is
///     the safe direction — it fails loud and never blesses a bug — and the right tradeoff for an
///     18-line helper, but if you refactor that shader and hit a red here, this is why.
///   - The entry-point NAME above is a string literal, and the name `pipeline.rs` binds is a
///     separate literal that merely reads the same. `masked_shadow_pipeline_binds_the_entry_point_
///     this_file_grades` below couples them.
#[test]
fn shadow_masked_alpha_cutout_actually_discards() {
    for (source, label, entry_point) in [
        (ZONE_WGSL, "zone.wgsl", "fs_main"),
        (ZONE_INSTANCED_WGSL, "zone_instanced.wgsl", "fs_main"),
        (SHADOW_MASKED_INSTANCED_WGSL, "shadow_masked_instanced.wgsl", "fs_instanced_masked"),
    ] {
        let module = parse_and_validate(source, label);
        assert!(
            entry_point_kills(&module, entry_point, label),
            "{label}: entry point `{entry_point}` never executes a `discard` statement (naga's \
             `Statement::Kill`) anywhere in its body — texels are compared against the alpha cutout \
             but nothing is ever dropped. That reproduces #707 (or the color-pass equivalent) even \
             though the threshold literal is present and numerically correct."
        );
    }
}

const PIPELINE_RS: &str = include_str!("../src/pipeline.rs");

/// eqoxide#721, third instance: `shadow_masked_alpha_cutout_actually_discards` grades an entry point
/// it looks up by the string literal `"fs_instanced_masked"`. The name `pipeline.rs` actually binds
/// is a *separate* string literal that happens to read the same today, and nothing coupled them.
///
/// The owner recorded a measured mutation on #721: leave `fs_instanced_masked` byte-identical, append
/// a discard-less `fs_instanced_masked_hq` to `shadow_masked_instanced.wgsl`, and point
/// `pipeline.rs`'s `entry_point` at the new name. All 9 tests in this file stayed green while the
/// shader that ships reproduced #707 exactly. `shadow_masked_instanced.wgsl`'s own header anticipates
/// the file growing a second masked path, so that is a foreseeable edit, not a contrived one.
///
/// This is source-text matching — the technique #718 spent two review rounds *removing* from the
/// shader-side checks. The distinction is deliberate: the shader side is IR-based because that is
/// where comment-style and decoy-entry-point mutations live; the drift this assert catches is on the
/// **Rust** side, where there is no IR to walk, and its failure mode is loud rather than silently
/// green. Its own weakness is the usual one for source-text pins: a decoy comment containing this
/// exact substring, or a legitimate reformat of that descriptor, would defeat/false-red it.
#[test]
fn masked_shadow_pipeline_binds_the_entry_point_this_file_grades() {
    assert!(
        PIPELINE_RS.contains(r#"module: &shadow_masked_shader, entry_point: "fs_instanced_masked","#),
        "pipeline.rs no longer binds `fs_instanced_masked` for the masked shadow caster, so \
         shadow_masked_alpha_cutout_actually_discards is grading an entry point that does not ship."
    );
}

/// Regression guard for eqoxide#718 NON-BLOCKING-3: `shadow_masked_instanced.wgsl` duplicates
/// `shadow.wgsl`'s (and `zone_instanced.wgsl`'s) EQ WLD → render axis swizzle — WGSL has no
/// `#include`, the same reason the alpha-cutout threshold and the ambient-shadow floor above are
/// duplicated source text rather than a shared symbol — but nothing pinned the three copies staying
/// identical. A divergent swizzle is a worse failure than the threshold drift the tests above guard:
/// a wrong cutout makes a masked shadow's silhouette slightly off; a wrong swizzle would land a
/// masked caster's shadow geometry somewhere else in the scene entirely, silently, since all three
/// files independently parse and validate as legal WGSL either way.
#[test]
fn shadow_masked_render_swizzle_matches_shadow_and_zone_instanced() {
    let shadow_literal = extract_render_swizzle_literal(SHADOW_WGSL, "shadow.wgsl");
    let zone_instanced_literal =
        extract_render_swizzle_literal(ZONE_INSTANCED_WGSL, "zone_instanced.wgsl");
    let shadow_masked_literal =
        extract_render_swizzle_literal(SHADOW_MASKED_INSTANCED_WGSL, "shadow_masked_instanced.wgsl");

    assert_eq!(
        shadow_literal, zone_instanced_literal,
        "shadow.wgsl's EQ->render swizzle ({shadow_literal}) must match zone_instanced.wgsl's \
         ({zone_instanced_literal}) — both transform the same instanced placed-object geometry."
    );
    assert_eq!(
        shadow_literal, shadow_masked_literal,
        "shadow.wgsl's EQ->render swizzle ({shadow_literal}) must match \
         shadow_masked_instanced.wgsl's ({shadow_masked_literal}) — a divergent swizzle would land \
         masked shadow casters somewhere else in the scene entirely (eqoxide#718 NON-BLOCKING-3)."
    );
}
