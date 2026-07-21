//! Regression coverage for the additive-blend fog fix (review defect on PR #523, eqoxide#517).
//!
//! `fs_blend` is shared by two pipelines with different fixed-function blend equations
//! (src/pipeline.rs): `zone_blend`/`zone_instanced_blend` use `ALPHA_BLENDING`, where
//! `mix(color, fog_color, t)` is correct, but `zone_additive`/`zone_instanced_additive` use a
//! pure `src:One, dst:One, Add` blend with no destination term to mix against — mixing toward
//! `fog_color` there makes deep fog ADD a bright, non-fading patch instead of letting distant
//! glow (lava, fire, torches) fade into the haze.
//!
//! The fix splits the fragment entry point: `fs_blend_additive` (using `apply_fog_additive`,
//! which attenuates the lit color by `(1.0 - t)` instead of mixing toward `fog_color`) is bound
//! to the two additive pipelines; `fs_blend` (unchanged `mix`) stays on the two alpha-blend ones.
//!
//! There's no GPU device in this crate's test harness (see `src/pipeline.rs`'s `#[cfg(test)]`
//! tests, which only check signatures/field types, never construct a real `wgpu::Device`), so
//! this can't render a frame and read pixels back. Instead it verifies, without a GPU:
//!   1. Both zone shader modules still parse and validate as legal WGSL (`naga`), including the
//!      new `apply_fog_additive` function and `fs_blend_additive` entry point.
//!   2. `src/pipeline.rs` wires the additive pipelines' fragment `entry_point` to
//!      `fs_blend_additive`, not `fs_blend` — a direct regression test for the review defect
//!      recurring (someone reverting the pipeline wiring back to the shared mix-based entry
//!      point would silently reintroduce the bug without touching the WGSL at all).
//!   3. A pure-Rust mirror of `fog_t`/`apply_fog`/`apply_fog_additive` (kept in lockstep with the
//!      WGSL by the literal formula, not by execution) demonstrates the actual semantic
//!      difference this fix depends on: at full fog (`t=1`) the additive variant attenuates to
//!      zero (fades out), while the alpha-blend variant converges on `fog_color` (as intended for
//!      that pipeline) — proving the additive path is no longer "brighter than the mix path" at
//!      any fog depth, which was the reviewer's concrete complaint.

const ZONE_WGSL: &str = include_str!("../src/shaders/zone.wgsl");
const ZONE_INSTANCED_WGSL: &str = include_str!("../src/shaders/zone_instanced.wgsl");
const PIPELINE_RS: &str = include_str!("../src/pipeline.rs");

fn parse_and_validate(source: &str, label: &str) -> naga::Module {
    let module = naga::front::wgsl::parse_str(source)
        .unwrap_or_else(|e| panic!("{label}: WGSL failed to parse: {e}"));
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|e| panic!("{label}: WGSL failed validation: {e}"));
    module
}

fn has_fragment_entry_point(module: &naga::Module, name: &str) -> bool {
    module
        .entry_points
        .iter()
        .any(|ep| ep.name == name && ep.stage == naga::ShaderStage::Fragment)
}

#[test]
fn zone_wgsl_parses_and_validates() {
    let module = parse_and_validate(ZONE_WGSL, "zone.wgsl");
    for name in ["fs_main", "fs_blend", "fs_blend_additive"] {
        assert!(
            has_fragment_entry_point(&module, name),
            "zone.wgsl: expected fragment entry point `{name}`"
        );
    }
}

#[test]
fn zone_instanced_wgsl_parses_and_validates() {
    let module = parse_and_validate(ZONE_INSTANCED_WGSL, "zone_instanced.wgsl");
    for name in ["fs_main", "fs_blend", "fs_blend_additive"] {
        assert!(
            has_fragment_entry_point(&module, name),
            "zone_instanced.wgsl: expected fragment entry point `{name}`"
        );
    }
}

/// Direct regression test for the review defect on #523: the two additive pipelines must bind
/// `fs_blend_additive`, and the two alpha-blend pipelines must keep binding the unchanged
/// `fs_blend`. This is a source-level assertion (rather than constructing real
/// `wgpu::RenderPipeline`s, which needs a GPU device unavailable in this harness) that would
/// fail immediately if someone "simplified" the additive pipelines back onto the shared
/// mix-based entry point.
#[test]
fn additive_pipelines_bind_fs_blend_additive_not_fs_blend() {
    let zone_additive_block = extract_pipeline_block(PIPELINE_RS, "zone_additive");
    let zone_instanced_additive_block = extract_pipeline_block(PIPELINE_RS, "zone_instanced_additive");
    let zone_blend_block = extract_pipeline_block(PIPELINE_RS, "zone_blend");
    let zone_instanced_blend_block = extract_pipeline_block(PIPELINE_RS, "zone_instanced_blend");

    assert!(
        zone_additive_block.contains(r#"entry_point: "fs_blend_additive""#),
        "zone_additive pipeline must bind fs_blend_additive (found: {zone_additive_block})"
    );
    assert!(
        zone_instanced_additive_block.contains(r#"entry_point: "fs_blend_additive""#),
        "zone_instanced_additive pipeline must bind fs_blend_additive (found: {zone_instanced_additive_block})"
    );
    assert!(
        zone_blend_block.contains(r#"entry_point: "fs_blend""#)
            && !zone_blend_block.contains("fs_blend_additive"),
        "zone_blend pipeline must keep binding the unchanged fs_blend"
    );
    assert!(
        zone_instanced_blend_block.contains(r#"entry_point: "fs_blend""#)
            && !zone_instanced_blend_block.contains("fs_blend_additive"),
        "zone_instanced_blend pipeline must keep binding the unchanged fs_blend"
    );
}

/// Slices out the `let <name> = device.create_render_pipeline(...);` statement for `name` from
/// pipeline.rs's source text, up to its matching close-paren `});`. Good enough for this file's
/// consistent formatting; not a general Rust parser.
fn extract_pipeline_block<'a>(source: &'a str, name: &str) -> &'a str {
    let needle = format!("let {name} = device.create_render_pipeline(");
    let start = source
        .find(&needle)
        .unwrap_or_else(|| panic!("pipeline.rs: couldn't find `{needle}`"));
    let end_marker = "\n    });\n";
    let end = source[start..]
        .find(end_marker)
        .unwrap_or_else(|| panic!("pipeline.rs: couldn't find end of `{name}` pipeline block"));
    &source[start..start + end]
}

/// Pure-Rust mirror of zone.wgsl's `fog_t` — literal transcription of the WGSL formula, not
/// executed WGSL. Kept in sync with the shader by inspection (both are tiny, and the entry-point
/// tests above catch the shader itself failing to parse/validate).
fn fog_t(dist: f32, minclip: f32, maxclip: f32, density: f32, enabled: f32) -> f32 {
    let range = (maxclip - minclip).max(0.001);
    ((dist - minclip) / range).clamp(0.0, 1.0) * density * enabled
}

/// Mirror of `apply_fog` (alpha-blend / opaque path): mixes toward `fog_color`.
fn apply_fog_mix(color: [f32; 3], fog_color: [f32; 3], t: f32) -> [f32; 3] {
    std::array::from_fn(|i| color[i] + (fog_color[i] - color[i]) * t)
}

/// Mirror of `apply_fog_additive`: attenuates toward zero, never toward `fog_color`.
fn apply_fog_additive(color: [f32; 3], t: f32) -> [f32; 3] {
    color.map(|c| c * (1.0 - t))
}

#[test]
fn additive_fog_math_fades_to_zero_not_fog_color() {
    let lit = [0.9, 0.6, 0.1]; // bright lava-glow-ish color
    let fog_color = [0.5, 0.5, 0.6]; // arbitrary distinct fog tint

    // No fog at the camera.
    assert_eq!(apply_fog_additive(lit, fog_t(0.0, 100.0, 500.0, 1.0, 1.0)), lit);

    // Fully fogged (dist far past maxclip): additive must fade to black...
    let t_far = fog_t(10_000.0, 100.0, 500.0, 1.0, 1.0);
    assert_eq!(t_far, 1.0);
    let additive_far = apply_fog_additive(lit, t_far);
    assert_eq!(additive_far, [0.0, 0.0, 0.0], "additive glow must fade fully at t=1, not persist");

    // ...while the alpha-blend mix (correct for that pipeline) converges on fog_color, which is
    // nonzero. This is exactly the reviewer's point: applying THAT formula to the additive
    // pipeline would add a nonzero, non-fading `fog_color` term on top of the background.
    let mix_far = apply_fog_mix(lit, fog_color, t_far);
    assert_eq!(mix_far, fog_color);
    assert_ne!(
        mix_far, additive_far,
        "the two blend modes must diverge at full fog — that divergence is the fix"
    );

    // Monotonic non-increasing brightness as fog deepens (no "bump" partway through the range).
    let samples: Vec<f32> = (0..=10)
        .map(|i| {
            let dist = 100.0 + (i as f32 / 10.0) * 400.0; // sweep minclip..maxclip
            let t = fog_t(dist, 100.0, 500.0, 1.0, 1.0);
            apply_fog_additive(lit, t)[0]
        })
        .collect();
    for pair in samples.windows(2) {
        assert!(pair[1] <= pair[0] + 1e-6, "additive brightness must not increase with distance: {samples:?}");
    }

    // fog_params.w (enabled) = 0.0 must fully disable attenuation, matching the no-fog-zone gate.
    let t_disabled = fog_t(10_000.0, 100.0, 500.0, 1.0, 0.0);
    assert_eq!(t_disabled, 0.0);
    assert_eq!(apply_fog_additive(lit, t_disabled), lit);
}
