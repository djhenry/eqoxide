// Diagnostic compute shader: skin each vertex with the SAME math as
// character_skinned.wgsl's vertex stage, writing the skinned model-space position to
// an output buffer. Used by render_model to read back the GPU's actual skinning result
// and compare it against the CPU (skin_point) — to isolate GPU-vs-CPU divergence.

struct JointMatrices { mats: array<mat4x4<f32>, 128> };
@group(0) @binding(0) var<uniform> joints: JointMatrices;

struct Vtx {
    px: f32, py: f32, pz: f32,
    j0: u32, j1: u32, j2: u32, j3: u32,
    w0: f32, w1: f32, w2: f32, w3: f32,
};
@group(0) @binding(1) var<storage, read> verts: array<Vtx>;
@group(0) @binding(2) var<storage, read_write> outpos: array<vec4<f32>>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= arrayLength(&verts)) { return; }
    let v = verts[i];
    let p = vec4<f32>(v.px, v.py, v.pz, 1.0);
    var acc = vec4<f32>(0.0);
    acc += v.w0 * (joints.mats[v.j0] * p);
    acc += v.w1 * (joints.mats[v.j1] * p);
    acc += v.w2 * (joints.mats[v.j2] * p);
    acc += v.w3 * (joints.mats[v.j3] * p);
    outpos[i] = acc;
}
