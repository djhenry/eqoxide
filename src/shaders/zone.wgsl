struct Camera {
    view_proj:  mat4x4<f32>,
    camera_pos: vec4<f32>,
    fog_color:  vec4<f32>,
    fog_params: vec4<f32>, // x=minclip, y=maxclip, z=density, w=enabled(0/1)
};
@group(0) @binding(0) var<uniform> camera: Camera;

@group(1) @binding(0) var t_diffuse: texture_2d<f32>;
@group(1) @binding(1) var s_diffuse: sampler;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal:   vec3<f32>,
    @location(2) uv:       vec2<f32>,
};

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) uv:     vec2<f32>,
    @location(2) world_pos: vec3<f32>,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_pos  = camera.view_proj * vec4<f32>(in.position, 1.0);
    out.normal    = in.normal;
    out.uv        = in.uv;
    out.world_pos = in.position;
    return out;
}

// RoF2 zone distance fog (eqoxide#517): linear fade between fog_params.x (minclip) and
// fog_params.y (maxclip), scaled by density as a blend-intensity cap — matches the native
// client's D3DFOG_LINEAR (confirmed via the REDACTED-CLIENT decompile; fog_density is never
// wired to D3DRS_FOGDENSITY there, so this is NOT exponential fog). `fog_params.w` is a hard
// enable gate: 0.0 for a zone with no/degenerate fog range, so a fogless zone renders unchanged.
fn fog_t(world_pos: vec3<f32>) -> f32 {
    let dist  = length(world_pos - camera.camera_pos.xyz);
    let range = max(camera.fog_params.y - camera.fog_params.x, 0.001);
    return clamp((dist - camera.fog_params.x) / range, 0.0, 1.0)
           * camera.fog_params.z * camera.fog_params.w;
}

fn apply_fog(color: vec3<f32>, world_pos: vec3<f32>) -> vec3<f32> {
    return mix(color, camera.fog_color.rgb, fog_t(world_pos));
}

// Additive-blend variant: the fixed-function blend stage does `dst + src` (BlendFactor::One /
// BlendFactor::One), so there is no "destination" term here to mix toward — `mix(color,
// fog_color, t)` would instead ADD a converging-to-fog_color term on top of the already-fogged
// background, which gets brighter (not fainter) as fog deepens (review defect on #523). Distance
// fog on an additive surface (lava glow, fire, torches) should instead fade the glow's own
// contribution toward zero, letting the opaque/alpha-blended fog behind it show through
// untouched. So: attenuate `color` by `(1-t)` rather than mixing toward `fog_color`.
fn apply_fog_additive(color: vec3<f32>, world_pos: vec3<f32>) -> vec3<f32> {
    return color * (1.0 - fog_t(world_pos));
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let light = max(dot(normalize(in.normal), normalize(vec3<f32>(0.5, 1.0, 0.3))), 0.1);
    let texel = textureSample(t_diffuse, s_diffuse, in.uv);
    // Alpha-test cutout for masked materials (foliage/branches): EQ opaque textures
    // decode to alpha 1.0, so this only discards keyed-transparent texels.
    if (texel.a < 0.5) {
        discard;
    }
    let lit = texel.rgb * light;
    return vec4<f32>(apply_fog(lit, in.world_pos), texel.a);
}

// Blended/additive surfaces: no alpha-test discard (opacity is baked into the texture
// alpha by the asset server). The pipeline supplies the blend equation.
@fragment
fn fs_blend(in: VertexOutput) -> @location(0) vec4<f32> {
    let light = max(dot(normalize(in.normal), normalize(vec3<f32>(0.5, 1.0, 0.3))), 0.1);
    let texel = textureSample(t_diffuse, s_diffuse, in.uv);
    let lit = texel.rgb * light;
    return vec4<f32>(apply_fog(lit, in.world_pos), texel.a);
}

// Additive surfaces only (zone_additive pipeline — lava glow, fire, torches). Same shading as
// `fs_blend` but uses `apply_fog_additive` so the glow fades toward zero instead of toward
// `fog_color` under the fixed-function One/One add (see apply_fog_additive doc comment; review
// defect on #523).
@fragment
fn fs_blend_additive(in: VertexOutput) -> @location(0) vec4<f32> {
    let light = max(dot(normalize(in.normal), normalize(vec3<f32>(0.5, 1.0, 0.3))), 0.1);
    let texel = textureSample(t_diffuse, s_diffuse, in.uv);
    let lit = texel.rgb * light;
    return vec4<f32>(apply_fog_additive(lit, in.world_pos), texel.a);
}
