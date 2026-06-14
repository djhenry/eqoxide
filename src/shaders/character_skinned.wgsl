struct Camera {
    view_proj: mat4x4<f32>,
};
@group(0) @binding(0) var<uniform> camera: Camera;

@group(1) @binding(0) var t_diffuse: texture_2d<f32>;
@group(1) @binding(1) var s_diffuse: sampler;

struct EntityUniform {
    model: mat4x4<f32>,
    tint:  vec4<f32>,
};
@group(2) @binding(0) var<uniform> entity: EntityUniform;

struct JointMatrices { mats: array<mat4x4<f32>, 128> };
@group(3) @binding(0) var<uniform> joints: JointMatrices;

struct SkinnedVertexInput {
    @location(0) position:      vec3<f32>,
    @location(1) normal:        vec3<f32>,
    @location(2) uv:            vec2<f32>,
    @location(3) joint_indices: vec4<u32>,
    @location(4) joint_weights: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) uv:     vec2<f32>,
};

@vertex
fn vs_main(in: SkinnedVertexInput) -> VertexOutput {
    var pos  = vec4<f32>(0.0);
    var norm = vec4<f32>(0.0);
    for (var i = 0u; i < 4u; i++) {
        let w = in.joint_weights[i];
        let m = joints.mats[in.joint_indices[i]];
        pos  += w * (m * vec4<f32>(in.position, 1.0));
        norm += w * (m * vec4<f32>(in.normal,   0.0));
    }
    var out: VertexOutput;
    let world    = entity.model * pos;
    out.clip_pos = camera.view_proj * world;
    out.normal   = normalize((entity.model * norm).xyz);
    out.uv       = in.uv;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let n    = normalize(in.normal);
    let sun  = max(dot(n, normalize(vec3<f32>(0.3, -0.5, 1.0))), 0.0);
    let fill = max(dot(n, normalize(vec3<f32>(-0.3, 0.5, 0.5))), 0.0);
    let light = 0.5 + sun * 0.35 + fill * 0.15;
    let texel = textureSample(t_diffuse, s_diffuse, in.uv);
    return vec4<f32>(texel.rgb * entity.tint.rgb * light, texel.a * entity.tint.a);
}
