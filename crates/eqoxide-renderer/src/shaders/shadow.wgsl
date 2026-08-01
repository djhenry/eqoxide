// Sun shadow-map DEPTH pass (#518). Renders shadow casters (characters + placed objects) from the
// directional light's point of view into the shadow-map depth texture. Depth-only: no fragment
// stage, no color target — only @builtin(position) matters. The lit zone shaders later project each
// terrain fragment into this same light space and compare depths to decide if it's shadowed.
//
// Three vertex entry points mirror the three color-pass geometry kinds:
//   vs_static    — static-mesh casters (a static player/NPC model), one model matrix.
//   vs_skinned   — GPU-skinned casters (the usual humanoid), joint palette + model matrix.
//   vs_instanced — GPU-instanced placed objects, per-instance matrix + the EQ→render axis swizzle.

struct ShadowLight { light_vp: mat4x4<f32> };
@group(0) @binding(0) var<uniform> light: ShadowLight;

// group(1) reuses the color passes' entity_bgl (EntityUniform = model + tint); only `model` is read.
struct Model { model: mat4x4<f32> };
@group(1) @binding(0) var<uniform> entity: Model;

// The palette length below is a PLACEHOLDER, not a WGSL constant: `pipeline::wgsl()` substitutes
// the Rust `renderer::JOINT_CAP` into this text before `create_shader_module` sees it, so the
// palette length exists as a number in exactly one place in the tree (eqoxide#798). Do not write a
// numeric length here — an un-substituted source is not valid WGSL and naga rejects it, which
// is the point: forgetting the substitution is a hard failure, never a silent wrong cap.
// The placeholder is delimited so it can never be a substring of an identifier, and so that
// substitution rewrites no comment on this page (eqoxide#812).
struct JointMatrices { mats: array<mat4x4<f32>, ${JOINT_CAP}> };
@group(2) @binding(0) var<uniform> joints: JointMatrices;

@vertex
fn vs_static(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return light.light_vp * (entity.model * vec4<f32>(position, 1.0));
}

struct SkinnedIn {
    @location(0) position:      vec3<f32>,
    @location(1) normal:        vec3<f32>,
    @location(2) uv:            vec2<f32>,
    @location(3) joint_indices: vec4<u32>,
    @location(4) joint_weights: vec4<f32>,
};

@vertex
fn vs_skinned(in: SkinnedIn) -> @builtin(position) vec4<f32> {
    var pos = vec4<f32>(0.0);
    for (var i = 0u; i < 4u; i++) {
        let w = in.joint_weights[i];
        let m = joints.mats[in.joint_indices[i]];
        pos += w * (m * vec4<f32>(in.position, 1.0));
    }
    return light.light_vp * (entity.model * pos);
}

struct InstancedIn {
    @location(0) position: vec3<f32>,
    @location(1) normal:   vec3<f32>,
    @location(2) uv:       vec2<f32>,
    @location(3) m0: vec4<f32>,
    @location(4) m1: vec4<f32>,
    @location(5) m2: vec4<f32>,
    @location(6) m3: vec4<f32>,
};

@vertex
fn vs_instanced(in: InstancedIn) -> @builtin(position) vec4<f32> {
    let inst  = mat4x4<f32>(in.m0, in.m1, in.m2, in.m3);
    let world = inst * vec4<f32>(in.position, 1.0);
    // Same EQ WLD → render axis swizzle the instanced color shader applies (see zone_instanced.wgsl).
    let render = vec3<f32>(world.z, world.x, world.y);
    return light.light_vp * vec4<f32>(render, 1.0);
}
