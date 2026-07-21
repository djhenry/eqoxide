// Fullscreen sky gradient rendered as a background layer.
// No vertex buffer — six vertices are generated from vertex_index.
// Depth: none (renders first, everything else writes on top).

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0)       y:    f32,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    var xv = array<f32, 6>(-1.0,  1.0,  1.0, -1.0,  1.0, -1.0);
    var yv = array<f32, 6>(-1.0, -1.0,  1.0, -1.0,  1.0,  1.0);
    var out: VsOut;
    out.clip = vec4<f32>(xv[vid], yv[vid], 0.0, 1.0);
    out.y    = yv[vid];
    return out;
}

// Time-of-day gradient (eqoxide#561): zenith/horizon stops are supplied per frame by the CPU from
// the server world clock (see `eqoxide_core::sky`), replacing the former hardcoded blue constants.
// `.xyz` is the color; `.w` is padding.
struct SkyColors {
    zenith:  vec4<f32>,
    horizon: vec4<f32>,
};
@group(0) @binding(0) var<uniform> sky: SkyColors;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // y: -1 = bottom/horizon, +1 = top/zenith
    let t = clamp(in.y * 0.5 + 0.5, 0.0, 1.0);
    return vec4<f32>(mix(sky.horizon.xyz, sky.zenith.xyz, pow(t, 0.45)), 1.0);
}
