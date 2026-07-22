//! Headless coverage for the nav diagnostics overlay pass (#608).
//!
//! No GPU device exists in this crate's test harness (same constraint as `tests/weather_shader.rs`),
//! so this validates the two things a live run would otherwise be needed to catch:
//!   1. `nav_debug.wgsl` parses and validates as legal WGSL with the entry points the pipeline
//!      binds — a broken shader would otherwise only surface at runtime.
//!   2. `pipeline.rs` keeps the overlay wired DEPTH-CORRECT: a LineList pipeline on
//!      `transparent_depth` (LessEqual test, depth-write OFF). Depth correctness is the entire
//!      point of replacing the screen-space egui painter — an overlay drawn without the depth test
//!      paints through walls, which is exactly the ambiguity #423 diagnosis needs removed. A
//!      source-level assertion that goes RED if someone rewires it onto no depth (draw-through) or
//!      opaque depth (the overlay would punch holes into later passes).

const NAV_DEBUG_WGSL: &str = include_str!("../src/shaders/nav_debug.wgsl");
const PIPELINE_RS: &str = include_str!("../src/pipeline.rs");

fn parse_and_validate(source: &str, label: &str) -> naga::Module {
    let module = naga::front::wgsl::parse_str(source)
        .unwrap_or_else(|e| panic!("{label}: WGSL failed to parse: {e}"));
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|e| panic!("{label}: WGSL failed validation: {e}"));
    module
}

#[test]
fn nav_debug_wgsl_parses_and_validates() {
    let module = parse_and_validate(NAV_DEBUG_WGSL, "nav_debug.wgsl");
    assert!(module.entry_points.iter().any(|ep| ep.name == "vs_main" && ep.stage == naga::ShaderStage::Vertex));
    assert!(module.entry_points.iter().any(|ep| ep.name == "fs_main" && ep.stage == naga::ShaderStage::Fragment));
}

/// The overlay pipeline block in `pipeline.rs` must stay: LINE LIST topology, alpha-blended, and on
/// `transparent_depth` (depth-TESTED, depth-write off). This is the "correctly occluded by
/// geometry" requirement of #608 pinned at the source level, since no GPU exists here to render an
/// occlusion fixture.
#[test]
fn nav_debug_pipeline_is_wired_depth_tested_line_list() {
    // Slice out the nav_debug pipeline construction.
    let start = PIPELINE_RS.find("let nav_debug = device.create_render_pipeline")
        .expect("pipeline.rs must build the nav_debug pipeline");
    let block = &PIPELINE_RS[start..start + PIPELINE_RS[start..].find("});").map(|e| e + 3).unwrap()];
    assert!(block.contains("PrimitiveTopology::LineList"),
        "the overlay draws world-space LINES: {block}");
    assert!(block.contains("transparent_depth"),
        "the overlay must be depth-TESTED against the scene with depth-write OFF \
         (transparent_depth). Dropping the depth attachment reintroduces the draw-through-walls \
         ambiguity the #608 rewrite exists to remove: {block}");
    assert!(block.contains("ALPHA_BLENDING"), "overlay lines are alpha-blended: {block}");
    // ...and transparent_depth itself must still mean "test on, write off".
    let td_start = PIPELINE_RS.find("let transparent_depth").unwrap();
    let td = &PIPELINE_RS[td_start..td_start + 220];
    assert!(td.contains("depth_write_enabled: false"), "transparent_depth writes no depth: {td}");
    assert!(td.contains("LessEqual"), "transparent_depth still TESTS depth: {td}");
}
