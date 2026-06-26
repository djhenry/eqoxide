struct Camera { view_proj: mat4x4<f32>, };
@group(0) @binding(0) var<uniform> camera: Camera;
@group(1) @binding(0) var t_diffuse: texture_2d<f32>;
@group(1) @binding(1) var s_diffuse: sampler;

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
    return out;
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
    return vec4<f32>(texel.rgb * light, texel.a);
}

// Blended/additive instanced surfaces: no alpha-test discard (opacity baked into
// the texture alpha). The pipeline supplies the blend equation.
@fragment
fn fs_blend(in: VertexOutput) -> @location(0) vec4<f32> {
    let light = max(dot(normalize(in.normal), normalize(vec3<f32>(0.5, 1.0, 0.3))), 0.1);
    let texel = textureSample(t_diffuse, s_diffuse, in.uv);
    return vec4<f32>(texel.rgb * light, texel.a);
}
