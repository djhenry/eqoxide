// Neutral unlit inspection: server geometry and placements are already converted.
struct Camera { view_projection: mat4x4<f32> }
struct Material { factor: vec4<f32>, cutoff: f32, mode: u32, padding: vec2<u32> }
@group(0) @binding(0) var<uniform> camera: Camera;
@group(1) @binding(0) var<uniform> material: Material;
@group(1) @binding(1) var color_texture: texture_2d<f32>;
struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) c0: vec4<f32>,
    @location(4) c1: vec4<f32>,
    @location(5) c2: vec4<f32>,
    @location(6) c3: vec4<f32>,
}
struct VertexOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
}
@vertex fn vs_main(v: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip = camera.view_projection * mat4x4<f32>(v.c0, v.c1, v.c2, v.c3) * vec4<f32>(v.position, 1.0);
    out.uv = v.uv;
    out.color = v.color;
    return out;
}
fn repeat_coord(p: vec2<i32>, size: vec2<i32>) -> vec2<i32> { return ((p % size) + size) % size; }
fn repeat_bilinear(uv: vec2<f32>) -> vec4<f32> {
    let size = vec2<i32>(textureDimensions(color_texture));
    // Fraction before integer conversion keeps all finite out-of-range UVs bounded.
    let p = fract(uv) * vec2<f32>(size) - vec2<f32>(0.5);
    let base = vec2<i32>(floor(p));
    let f = fract(p);
    let a = textureLoad(color_texture, repeat_coord(base, size), 0);
    let b = textureLoad(color_texture, repeat_coord(base + vec2<i32>(1, 0), size), 0);
    let c = textureLoad(color_texture, repeat_coord(base + vec2<i32>(0, 1), size), 0);
    let d = textureLoad(color_texture, repeat_coord(base + vec2<i32>(1, 1), size), 0);
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}
@fragment fn fs_main(v: VertexOutput) -> @location(0) vec4<f32> {
    var color = repeat_bilinear(v.uv) * material.factor * v.color;
    if material.mode == 1u && color.a < material.cutoff { discard; }
    if material.mode != 2u { color.a = 1.0; }
    return color;
}
