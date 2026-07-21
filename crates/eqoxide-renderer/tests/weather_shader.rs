//! Regression coverage for the weather precipitation pass (eqoxide#542, Slice 1).
//!
//! There's no GPU device in this crate's test harness (see `tests/fog_shader.rs` and pipeline.rs's
//! `#[cfg(test)]` tests, which never construct a real `wgpu::Device`), so this can't render a frame
//! and read pixels back. Instead, without a GPU, it verifies:
//!   1. `weather.wgsl` parses and validates as legal WGSL (`naga`), with the `vs_main`/`fs_main`
//!      entry points the pipeline binds — a broken shader would otherwise only surface at runtime
//!      when `build_pipelines` compiles it, blanking the frame.
//!   2. `pipeline.rs` wires the weather pipeline correctly: bind-group layouts `[camera_bgl,
//!      weather_bgl]`, two vertex buffers (the static quad + the per-instance particle buffer),
//!      alpha blending, and the depth-write-OFF `transparent_depth` (so precipitation is occluded
//!      by geometry in front but doesn't pollute depth). A source-level assertion that would fail
//!      if someone rewired it onto opaque depth or dropped the instance buffer.
//!   3. A pure-Rust mirror of the shader's particle-recycle math proves the field is *continuous*:
//!      every particle's world Z stays inside the camera-centered box for all time (never a gap
//!      above or a pile below), and the `fract()` wrap actually recycles (a particle that falls out
//!      the bottom reappears at the top). This is the property the "recycle around the camera"
//!      requirement depends on, and it's untestable by a single live screenshot.

const WEATHER_WGSL: &str = include_str!("../src/shaders/weather.wgsl");
const PIPELINE_RS: &str = include_str!("../src/pipeline.rs");

fn parse_and_validate(source: &str, label: &str) -> naga::Module {
    let module = naga::front::wgsl::parse_str(source)
        .unwrap_or_else(|e| panic!("{label}: WGSL failed to parse: {e}"));
    naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|e| panic!("{label}: WGSL failed validation: {e}"));
    module
}

fn has_entry_point(module: &naga::Module, name: &str, stage: naga::ShaderStage) -> bool {
    module.entry_points.iter().any(|ep| ep.name == name && ep.stage == stage)
}

#[test]
fn weather_wgsl_parses_and_validates() {
    let module = parse_and_validate(WEATHER_WGSL, "weather.wgsl");
    assert!(
        has_entry_point(&module, "vs_main", naga::ShaderStage::Vertex),
        "weather.wgsl: expected vertex entry point `vs_main`"
    );
    assert!(
        has_entry_point(&module, "fs_main", naga::ShaderStage::Fragment),
        "weather.wgsl: expected fragment entry point `fs_main`"
    );
}

/// Source-level assertion that the weather pipeline stays wired for a transparent, instanced,
/// camera-relative particle field. Would fail if someone dropped the instance buffer, bound opaque
/// depth (which would let precipitation write depth and occlude later transparent passes), or
/// removed the weather bind group.
#[test]
fn weather_pipeline_is_wired_transparent_and_instanced() {
    let block = extract_pipeline_block(PIPELINE_RS, "weather");
    assert!(
        block.contains(r#"entry_point: "vs_main""#) && block.contains(r#"entry_point: "fs_main""#),
        "weather pipeline must bind vs_main + fs_main (found: {block})"
    );
    assert!(
        block.contains("weather_quad_vbl") && block.contains("weather_inst_vbl"),
        "weather pipeline must use both the quad and the per-instance vertex buffers (found: {block})"
    );
    assert!(
        block.contains("ALPHA_BLENDING"),
        "weather particles must be alpha-blended (found: {block})"
    );
    assert!(
        block.contains("transparent_depth"),
        "weather pipeline must use depth-write-OFF transparent_depth so it doesn't pollute depth (found: {block})"
    );
    // The pipeline layout must include the camera + weather bind groups.
    let layout_block = extract_let_block(PIPELINE_RS, "weather_layout");
    assert!(
        layout_block.contains("camera_bgl") && layout_block.contains("weather_bgl"),
        "weather pipeline layout must bind [camera_bgl, weather_bgl] (found: {layout_block})"
    );
}

/// Slices out the `let <name> = device.create_render_pipeline(...);` statement up to its matching
/// `});`. Mirror of fog_shader.rs's helper — good enough for this file's consistent formatting.
fn extract_pipeline_block<'a>(source: &'a str, name: &str) -> &'a str {
    let needle = format!("let {name} = device.create_render_pipeline(");
    let start = source.find(&needle).unwrap_or_else(|| panic!("pipeline.rs: couldn't find `{needle}`"));
    let end_marker = "\n    });\n";
    let end = source[start..]
        .find(end_marker)
        .unwrap_or_else(|| panic!("pipeline.rs: couldn't find end of `{name}` pipeline block"));
    &source[start..start + end]
}

/// Slices out a `let <name> = device.create_pipeline_layout(...);` statement.
fn extract_let_block<'a>(source: &'a str, name: &str) -> &'a str {
    let needle = format!("let {name} = device.create_pipeline_layout(");
    let start = source.find(&needle).unwrap_or_else(|| panic!("pipeline.rs: couldn't find `{needle}`"));
    let end_marker = "\n    });\n";
    let end = source[start..]
        .find(end_marker)
        .unwrap_or_else(|| panic!("pipeline.rs: couldn't find end of `{name}` layout block"));
    &source[start..start + end]
}

/// Pure-Rust mirror of weather.wgsl's vertical particle-recycle math (literal transcription of the
/// WGSL, not executed WGSL — kept in sync by inspection; the entry-point test above catches the
/// shader itself failing to parse). Returns the particle's world Z given its base offset, phase,
/// and the current time. `cam_z` is the camera height; `box_h` the field height; `fall` speed.
fn particle_world_z(cam_z: f32, box_h: f32, fall: f32, base_z: f32, phase: f32, time: f32) -> f32 {
    let z_frac = (base_z + (time * fall) / box_h + phase).fract();
    // fract() of a positive argument is in [0,1); base_z/phase are in [0,1) and time>=0 here.
    cam_z + box_h * 0.5 - z_frac * box_h
}

#[test]
fn particle_field_stays_inside_the_camera_box_for_all_time() {
    // The "recycle around the camera" requirement: for any particle and any time, its world Z must
    // stay within [cam_z - box_h/2, cam_z + box_h/2] — the field is a continuous slab centered on
    // the camera, never a gap above nor a pile draining out the bottom.
    let cam_z = 123.0_f32;
    let box_h = 170.0_f32;
    let fall = 150.0_f32;
    let lo = cam_z - box_h * 0.5;
    let hi = cam_z + box_h * 0.5;
    let bases = [0.0_f32, 0.13, 0.5, 0.87, 0.999];
    let phases = [0.0_f32, 0.31, 0.66, 0.99];
    for &b in &bases {
        for &p in &phases {
            // Sweep 12 seconds of animation at 60 fps.
            for step in 0..=720 {
                let t = step as f32 / 60.0;
                let z = particle_world_z(cam_z, box_h, fall, b, p, t);
                assert!(
                    z >= lo - 1e-3 && z <= hi + 1e-3,
                    "particle z {z} escaped the camera box [{lo}, {hi}] at t={t} (base={b}, phase={p})"
                );
            }
        }
    }
}

#[test]
fn particles_fall_downward_and_recycle() {
    // Within a wrap period the particle descends (world is Z-up, so falling = decreasing Z), and
    // when it crosses the bottom it recycles to the top (a large positive jump), never vanishing.
    let cam_z = 0.0_f32;
    let box_h = 100.0_f32;
    let fall = 100.0_f32;
    let mut prev = particle_world_z(cam_z, box_h, fall, 0.2, 0.0, 0.0);
    let mut saw_recycle = false;
    for step in 1..=600 {
        let t = step as f32 / 60.0;
        let z = particle_world_z(cam_z, box_h, fall, 0.2, 0.0, t);
        if z > prev + box_h * 0.5 {
            // Jumped back up by more than half the box — a recycle from bottom to top.
            saw_recycle = true;
        } else {
            // Otherwise it must be descending (falling), not drifting upward.
            assert!(z <= prev + 1e-3, "particle drifted upward without recycling: {prev} -> {z} at t={t}");
        }
        prev = z;
    }
    assert!(saw_recycle, "a falling particle must recycle to the top within the sweep");
}
