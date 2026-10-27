// Common material response: explicit/legacy unlit or bounded reference-lit PBR factors.
struct Camera { view_projection: mat4x4<f32>, eye_projection:vec4<f32>, toward_eye:vec4<f32> }
struct Material { factor: vec4<f32>, parameters:vec4<f32>, flags:vec4<u32> }
struct Lighting { direction:vec4<f32>, direct:vec4<f32>, ambient:vec4<f32>, exposure:vec4<f32> }
@group(0) @binding(0) var<uniform> camera: Camera;
@group(0) @binding(1) var<uniform> lighting: Lighting;
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
    @location(7) normal:vec3<f32>,
}
struct VertexOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) world_position:vec3<f32>,
    @location(3) normal:vec3<f32>,
}
// Scaling before length avoids overflow/underflow; zero vectors use an explicit finite direction.
fn safe_direction(value:vec3<f32>, fallback:vec3<f32>) -> vec3<f32> {
    let scale=max(max(abs(value.x),abs(value.y)),abs(value.z));
    if scale == 0.0 { return fallback; }
    let v=value/scale;
    return v*inverseSqrt(dot(v,v));
}
fn fresnel(f0:vec3<f32>, cosine:f32) -> vec3<f32> {
    let x=1.0-clamp(cosine,0.0,1.0);let x2=x*x;
    return f0+(vec3<f32>(1.0)-f0)*(x2*x2*x);
}
fn pbr_response(base:vec3<f32>,n:vec3<f32>,v:vec3<f32>,metal:f32,rough:f32) -> vec3<f32> {
    let f0=mix(vec3<f32>(0.04),base,metal);
    let nv=clamp(dot(n,v),0.0,1.0);
    let fv=fresnel(f0,nv);
    // Deliberately approximate constant ambient fill, not filtered cubemap IBL/GI.
    var result=lighting.ambient.rgb*((vec3<f32>(1.0)-fv)*(1.0-metal)*base+fv);
    let l=lighting.direction.xyz;let nl=clamp(dot(n,l),0.0,1.0);
    if nl > 0.0 && nv > 0.0 {
        let h=safe_direction(l+v,n);
        let nh=clamp(dot(n,h),0.0,1.0);let vh=clamp(dot(v,h),0.0,1.0);
        let alpha=max(rough,0.045)*max(rough,0.045);let a2=alpha*alpha;
        let dterm=max(1.0+nh*nh*(a2-1.0),1.0e-7);
        let distribution=a2/(3.141592653589793*dterm*dterm);
        // Algebraically cancel NdotL/NdotV from separable Smith visibility;
        // this avoids division by a grazing-angle product that can underflow.
        let gv=nv+sqrt(a2+(1.0-a2)*nv*nv);
        let gl=nl+sqrt(a2+(1.0-a2)*nl*nl);
        let f=fresnel(f0,vh);
        let specular=f*(distribution/(gv*gl));
        let diffuse=(vec3<f32>(1.0)-f)*(1.0-metal)*base/3.141592653589793;
        result+=lighting.direct.rgb*(diffuse+specular)*nl;
    }
    return result;
}
@vertex fn vs_main(v: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    let model=mat4x4<f32>(v.c0,v.c1,v.c2,v.c3);
    let world=model*vec4<f32>(v.position,1.0);
    out.clip=camera.view_projection*world;
    out.world_position=world.xyz;
    // Positive uniform scale is validated by the decoder; normalize columns first
    // so huge scales cannot overflow otherwise valid normal directions.
    let rotation=mat3x3<f32>(safe_direction(v.c0.xyz,vec3<f32>(1.0,0.0,0.0)),safe_direction(v.c1.xyz,vec3<f32>(0.0,1.0,0.0)),safe_direction(v.c2.xyz,vec3<f32>(0.0,0.0,1.0)));
    out.normal=rotation*v.normal;
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
@fragment fn fs_main(v: VertexOutput, @builtin(front_facing) front:bool) -> @location(0) vec4<f32> {
    var color = repeat_bilinear(v.uv) * material.factor * v.color;
    if material.flags.x == 1u && color.a < material.parameters.x { discard; }
    if material.flags.x != 2u { color.a = 1.0; }
    if material.flags.y == 1u {
        var n=safe_direction(v.normal,vec3<f32>(0.0,0.0,1.0));
        if !front { n = -n; }
        var to_eye=camera.toward_eye.xyz;
        if camera.eye_projection.w == 1.0 { to_eye=safe_direction(camera.eye_projection.xyz-v.world_position,to_eye); }
        let radiance=pbr_response(color.rgb,n,to_eye,material.parameters.y,material.parameters.z);
        let exposed=radiance*lighting.exposure.x;
        color=vec4<f32>(exposed/(vec3<f32>(1.0)+exposed),color.a);
    }
    return color;
}
