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

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // y: -1 = bottom/horizon, +1 = top/zenith
    let t       = clamp(in.y * 0.5 + 0.5, 0.0, 1.0);
    let zenith  = vec3<f32>(0.26, 0.50, 0.82);   // deep sky blue
    let horizon = vec3<f32>(0.74, 0.86, 0.97);   // pale hazy blue
    return vec4<f32>(mix(horizon, zenith, pow(t, 0.45)), 1.0);
}
