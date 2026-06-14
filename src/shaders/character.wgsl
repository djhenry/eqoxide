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

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal:   vec3<f32>,
    @location(2) uv:       vec2<f32>,
};

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) uv:     vec2<f32>,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    let world_pos = entity.model * vec4<f32>(in.position, 1.0);
    out.clip_pos  = camera.view_proj * world_pos;
    out.normal    = normalize((entity.model * vec4<f32>(in.normal, 0.0)).xyz);
    out.uv        = in.uv;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let sun  = max(dot(n, normalize(vec3<f32>(0.3, -0.5, 1.0))), 0.0);
    let fill = max(dot(n, normalize(vec3<f32>(-0.3, 0.5, 0.5))), 0.0);
    let light = 0.5 + sun * 0.35 + fill * 0.15;
    let texel = textureSample(t_diffuse, s_diffuse, in.uv);
    return vec4<f32>(texel.rgb * entity.tint.rgb * light, texel.a * entity.tint.a);
}
