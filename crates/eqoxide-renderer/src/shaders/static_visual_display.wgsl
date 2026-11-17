// HDR radiance is composited before this single exposure/display operation.
struct Display { exposure: vec4<f32> }
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var<uniform> display: Display;
@vertex fn vs_display(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    return vec4<f32>(x * 2.0 - 1.0,y * 2.0 - 1.0,0.0,1.0);
}
@fragment fn fs_display(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let radiance = textureLoad(hdr,vec2<i32>(position.xy),0).rgb;
    let exposed = radiance * display.exposure.x;
    return vec4<f32>(exposed/(vec3<f32>(1.0)+exposed),1.0);
}
