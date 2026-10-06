struct Camera {
    view_proj:  mat4x4<f32>,
    camera_pos: vec4<f32>,
    fog_color:  vec4<f32>,
    fog_params: vec4<f32>, // x=minclip, y=maxclip, z=density, w=enabled(0/1)
};
@group(0) @binding(0) var<uniform> camera: Camera;
@group(1) @binding(0) var t_diffuse: texture_2d<f32>;
@group(1) @binding(1) var s_diffuse: sampler;

// Sun shadow map (#518) — shares the zone pipeline layout's group(2). See zone.wgsl for docs.
struct ShadowLight { light_vp: mat4x4<f32> };
@group(2) @binding(0) var<uniform> shadow_light: ShadowLight;
@group(2) @binding(1) var shadow_map: texture_depth_2d;
@group(2) @binding(2) var shadow_samp: sampler_comparison;

fn shadow_factor(world_pos: vec3<f32>) -> f32 {
    let lp = shadow_light.light_vp * vec4<f32>(world_pos, 1.0);
    if (lp.w <= 0.0) { return 1.0; }
    let ndc = lp.xyz / lp.w;
    if (ndc.x < -1.0 || ndc.x > 1.0 || ndc.y < -1.0 || ndc.y > 1.0 || ndc.z < 0.0 || ndc.z > 1.0) {
        return 1.0;
    }
    let uv = vec2<f32>(ndc.x * 0.5 + 0.5, ndc.y * -0.5 + 0.5);
    let cur = ndc.z - 0.0015;
    let texel = 1.0 / 2048.0; // must track gpu::SHADOW_MAP_SIZE
    var sum = 0.0;
    for (var dx = -1; dx <= 1; dx++) {
        for (var dy = -1; dy <= 1; dy++) {
            let off = vec2<f32>(f32(dx), f32(dy)) * texel;
            sum += textureSampleCompareLevel(shadow_map, shadow_samp, uv + off, cur);
        }
    }
    return sum / 9.0;
}

// Ambient floor for shadowed instanced (placed-object) surfaces — MUST match zone.wgsl's
// `apply_shadow` (see that file for the eqoxide#614 rationale behind 0.25). Enforced by
// shadow_shader.rs::ambient_floor_matches_between_zone_and_zone_instanced (fails if they drift).
fn apply_shadow(color: vec3<f32>, world_pos: vec3<f32>) -> vec3<f32> {
    return color * mix(0.25, 1.0, shadow_factor(world_pos));
}

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal:   vec3<f32>,
    @location(2) uv:       vec2<f32>,
    @location(3) m0: vec4<f32>,
    @location(4) m1: vec4<f32>,
    @location(5) m2: vec4<f32>,
    @location(6) m3: vec4<f32>,
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
    let inst = mat4x4<f32>(in.m0, in.m1, in.m2, in.m3);
    let world = inst * vec4<f32>(in.position, 1.0);
    let render = vec3<f32>(world.z, world.x, world.y);
    out.clip_pos = camera.view_proj * vec4<f32>(render, 1.0);
    let inst3 = mat3x3<f32>(in.m0.xyz, in.m1.xyz, in.m2.xyz);
    let nw = inst3 * in.normal;
    out.normal = vec3<f32>(nw.z, nw.x, nw.y);
    out.uv = in.uv;
    out.world_pos = render;
    return out;
}

// See zone.wgsl's apply_fog for the rationale (RoF2 linear distance fog, eqoxide#517).
fn fog_t(world_pos: vec3<f32>) -> f32 {
    let dist  = length(world_pos - camera.camera_pos.xyz);
    let range = max(camera.fog_params.y - camera.fog_params.x, 0.001);
    return clamp((dist - camera.fog_params.x) / range, 0.0, 1.0)
           * camera.fog_params.z * camera.fog_params.w;
}

fn apply_fog(color: vec3<f32>, world_pos: vec3<f32>) -> vec3<f32> {
    return mix(color, camera.fog_color.rgb, fog_t(world_pos));
}

// See zone.wgsl's apply_fog_additive for the rationale (avoids brightening additive glow toward
// fog_color under One/One add; review defect on #523).
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
    let lit = apply_shadow(texel.rgb * light, in.world_pos);
    return vec4<f32>(apply_fog(lit, in.world_pos), texel.a);
}

// Blended/additive instanced surfaces: no alpha-test discard (opacity baked into
// the texture alpha). The pipeline supplies the blend equation.
@fragment
fn fs_blend(in: VertexOutput) -> @location(0) vec4<f32> {
    let light = max(dot(normalize(in.normal), normalize(vec3<f32>(0.5, 1.0, 0.3))), 0.1);
    let texel = textureSample(t_diffuse, s_diffuse, in.uv);
    let lit = texel.rgb * light;
    return vec4<f32>(apply_fog(lit, in.world_pos), texel.a);
}

// Additive instanced surfaces only (zone_instanced_additive pipeline). See zone.wgsl's
// fs_blend_additive for the rationale (review defect on #523).
@fragment
fn fs_blend_additive(in: VertexOutput) -> @location(0) vec4<f32> {
    let light = max(dot(normalize(in.normal), normalize(vec3<f32>(0.5, 1.0, 0.3))), 0.1);
    let texel = textureSample(t_diffuse, s_diffuse, in.uv);
    let lit = texel.rgb * light;
    return vec4<f32>(apply_fog_additive(lit, in.world_pos), texel.a);
}
