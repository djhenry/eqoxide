// Weather precipitation particles (eqoxide#542, Slice 1). A field of instanced billboard quads
// recycled around the camera: rain = thin vertical streaks, snow = soft drifting flakes. The CPU
// draws `plan.count` instances (density scaled by server intensity) and nothing when clear.
//
// group 0 = shared camera uniform (view_proj + camera_pos). group 1 = per-frame weather params.

struct Camera {
    view_proj:  mat4x4<f32>,
    camera_pos: vec4<f32>,
    fog_color:  vec4<f32>,
    fog_params: vec4<f32>,
};
@group(0) @binding(0) var<uniform> camera: Camera;

struct Weather {
    right:   vec4<f32>,  // camera right (xyz)
    up:      vec4<f32>,  // camera up (xyz)
    params:  vec4<f32>,  // x=time, y=kind(0 rain/1 snow), z=box_xy, w=box_h
    params2: vec4<f32>,  // x=fall_speed, y=particle_size, z=alpha_scale, w=reserved
};
@group(1) @binding(0) var<uniform> weather: Weather;

struct VsOut {
    @builtin(position) clip:  vec4<f32>,
    @location(0)       local: vec2<f32>,  // quad corner in [-1,1], for the fragment falloff
    @location(1)       kind:  f32,
};

// corner: quad corner in [-1,1]^2 (static). inst: per-particle base position (xyz in [0,1)) + phase (w).
@vertex
fn vs_main(@location(0) corner: vec2<f32>, @location(1) inst: vec4<f32>) -> VsOut {
    let time   = weather.params.x;
    let kind   = weather.params.y;
    let box_xy = weather.params.z;
    let box_h  = weather.params.w;
    let fall   = weather.params2.x;
    let psize  = weather.params2.y;

    let cam   = camera.camera_pos.xyz;
    let base  = inst.xyz;
    let phase = inst.w;

    // Horizontal: a column fixed relative to the camera (wraps with the camera, so the field is
    // always centered on the player — continuous with no visible edge).
    var wx = cam.x + (base.x - 0.5) * box_xy;
    var wy = cam.y + (base.y - 0.5) * box_xy;

    // Vertical: fall over time and wrap within the box height (recycles the particle to the top).
    // World is Z-up, so "down" is -Z. fract() gives the continuous recycle.
    let z_frac = fract(base.z + (time * fall) / box_h + phase);
    var wz = cam.z + box_h * 0.5 - z_frac * box_h;

    // Snow drifts side to side; rain falls straight.
    if (kind > 0.5) {
        let tau = 6.2831853;
        wx = wx + sin(time * 0.6 + phase * tau) * 4.0;
        wy = wy + cos(time * 0.5 + phase * tau) * 4.0;
    }
    let center = vec3<f32>(wx, wy, wz);

    var offset: vec3<f32>;
    if (kind < 0.5) {
        // Rain streak: thin along camera-right, elongated along world-down (a vertical dash).
        let world_down = vec3<f32>(0.0, 0.0, -1.0);
        offset = weather.right.xyz * (corner.x * psize * 0.06)
               + world_down * (corner.y * psize);
    } else {
        // Snow flake: small camera-facing square.
        offset = (weather.right.xyz * corner.x + weather.up.xyz * corner.y) * psize;
    }

    var out: VsOut;
    out.clip  = camera.view_proj * vec4<f32>(center + offset, 1.0);
    out.local = corner;
    out.kind  = kind;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let alpha_scale = weather.params2.z;
    if (in.kind < 0.5) {
        // Rain: bright, thin, crisp in the middle and faded at the horizontal edges.
        let a = (1.0 - abs(in.local.x)) * 0.55 * alpha_scale;
        return vec4<f32>(0.62, 0.72, 0.86, a);
    } else {
        // Snow: soft round flake (radial falloff from the quad center).
        let d    = length(in.local);
        let soft = clamp(1.0 - d, 0.0, 1.0);
        return vec4<f32>(0.95, 0.97, 1.0, soft * soft * 0.9 * alpha_scale);
    }
}
