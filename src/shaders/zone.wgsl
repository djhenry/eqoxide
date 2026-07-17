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
// client's D3DFOG_LINEAR (confirmed via the EQGraphicsDX9.dll decompile; fog_density is never
// wired to D3DRS_FOGDENSITY there, so this is NOT exponential fog). `fog_params.w` is a hard
// enable gate: 0.0 for a zone with no/degenerate fog range, so a fogless zone renders unchanged.
fn apply_fog(color: vec3<f32>, world_pos: vec3<f32>) -> vec3<f32> {
    let dist  = length(world_pos - camera.camera_pos.xyz);
    let range = max(camera.fog_params.y - camera.fog_params.x, 0.001);
    let t     = clamp((dist - camera.fog_params.x) / range, 0.0, 1.0)
                * camera.fog_params.z * camera.fog_params.w;
    return mix(color, camera.fog_color.rgb, t);
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
