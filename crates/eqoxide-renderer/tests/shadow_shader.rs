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

/// The value picked in the eqoxide#614 A/B (see this file's module doc). Both shaders must carry
/// exactly this literal in their `apply_shadow` function.
const EXPECTED_AMBIENT_FLOOR: f32 = 0.25;

fn parse_and_validate(source: &str, label: &str) -> naga::Module {
    let module = naga::front::wgsl::parse_str(source)
        .unwrap_or_else(|e| panic!("{label}: WGSL failed to parse: {e}"));
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|e| panic!("{label}: WGSL failed validation: {e}"));
    module
}

/// Extracts the `X` in `apply_shadow`'s `return color * mix(X, 1.0, shadow_factor(world_pos));`
/// via a source-level string search — deliberately not going through naga's AST here, since it's
/// the literal *source text* that must stay byte-for-byte identical between the two files (a
/// constant-folded AST comparison would miss e.g. `0.250` vs `0.25` staying "equal" numerically
/// while the two files visibly disagree on what a maintainer sees/greps for).
fn extract_shadow_floor(source: &str, label: &str) -> f32 {
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
    let literal = after_mix[..comma_at].trim();
    literal
        .parse::<f32>()
        .unwrap_or_else(|e| panic!("{label}: `{literal}` (the mix() floor argument) isn't a valid f32 literal: {e}"))
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
    let zone_floor = extract_shadow_floor(ZONE_WGSL, "zone.wgsl");
    let instanced_floor = extract_shadow_floor(ZONE_INSTANCED_WGSL, "zone_instanced.wgsl");
    assert_eq!(
        zone_floor, instanced_floor,
        "zone.wgsl's apply_shadow floor ({zone_floor}) must match zone_instanced.wgsl's ({instanced_floor}) \
         — terrain and placed objects must shadow to the same darkness. Update both files together."
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
