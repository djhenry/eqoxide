// Masked-foliage sun shadow caster (#707). `shadow.wgsl` states outright that the depth pass has
// "no fragment stage, no color target" — every triangle of a caster writes full depth. That's
// correct for opaque casters, but the instanced caster set deliberately includes RenderMode::Masked
// meshes (foliage/branches with a keyed-transparent texture) alongside Opaque ones (see pass.rs's
// caster selection). Without a fragment stage to discard the transparent texels, a masked quad casts
// its full bounding rectangle instead of its cutout silhouette — the visible tree/branch shape and
// its shadow disagree. This file adds the fragment stage ONLY for that case.
//
// A SEPARATE shader module from shadow.wgsl (its own include_str!/ShaderModuleDescriptor), not
// another entry point tacked onto that file: shadow.wgsl's `entity: Model` uniform already occupies
// @group(1)@binding(0) for the static/skinned casters, and WGSL requires every global to have a
// unique (group, binding) across an ENTIRE parsed module even when a given entry point never
// touches it — so reusing @group(1) here for a texture would collide with that declaration. A fresh
// module gets independent group numbering for free, at the cost of duplicating the small
// vertex-transform already in shadow.wgsl's `vs_instanced` (the same WGSL #include limitation
// documented in tests/shadow_shader.rs's module doc).
//
// Applies ONLY to RenderMode::Masked instanced (placed-object) casters — pass.rs routes by
// render_mode, and RenderMode::Opaque casters keep using the cheap fragment-less `shadow_instanced`
// pipeline (pipeline.rs). Adding a fragment stage to every opaque caster would be a needless
// per-pixel cost for geometry that never needs a cutout test.
//
// Character (static/skinned) shadow casters are NOT given a masked variant here: `models.rs`
// hardcodes `RenderMode::Opaque` for every character mesh it loads (see the `MeshData { ... }`
// construction in `parse_gltf_model`/equivalent), and `GpuSkinnedMesh` (gpu.rs) has no `render_mode`
// field at all to carry one even if it were set — so a character caster can never be `Masked` today,
// and the color-pass character shaders (`character.wgsl`, `character_skinned.wgsl`) never alpha-test
// either (no `discard` in their `fs_main`). There is nothing for a masked shadow variant to agree
// with on that path; adding one would be dead code documenting a distinction the rest of the
// character pipeline doesn't make. If character models ever gain real alpha-cutout materials, this
// same treatment should extend to `shadow_static`/`shadow_skinned` (and the color pass would need
// the cutout too, which it doesn't have yet either).

struct ShadowLight { light_vp: mat4x4<f32> };
@group(0) @binding(0) var<uniform> light: ShadowLight;

// Matches the existing `texture_bgl` layout exactly (binding 0 = texture, binding 1 = sampler) —
// see pipeline.rs, which reuses that layout for group(1) here rather than defining a new one.
@group(1) @binding(0) var t_diffuse: texture_2d<f32>;
@group(1) @binding(1) var s_diffuse: sampler;

struct InstancedIn {
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
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_instanced_masked(in: InstancedIn) -> VertexOutput {
    let inst  = mat4x4<f32>(in.m0, in.m1, in.m2, in.m3);
    let world = inst * vec4<f32>(in.position, 1.0);
    // Same EQ WLD → render axis swizzle as shadow.wgsl's vs_instanced / zone_instanced.wgsl's vs_main.
    let render = vec3<f32>(world.z, world.x, world.y);
    var out: VertexOutput;
    out.clip_pos = light.light_vp * vec4<f32>(render, 1.0);
    out.uv = in.uv;
    return out;
}

// No color target (the shadow pass's render_pass has `color_attachments: &[]`) — this fragment
// stage exists purely to `discard` transparent texels before they write depth; it returns nothing.
@fragment
fn fs_instanced_masked(in: VertexOutput) {
    // Same 0.5 cutout threshold as the color pass (zone_instanced.wgsl / zone.wgsl `fs_main`) — MUST
    // agree, or the shadow silhouette disagrees with the rendered silhouette, which is the same class
    // of bug as #707 itself. WGSL has no #include, so this is duplicated source text rather than a
    // shared symbol (same limitation documented in tests/shadow_shader.rs for the ambient-floor
    // constant); the agreement is pinned in tests/shadow_shader.rs instead.
    let texel = textureSample(t_diffuse, s_diffuse, in.uv);
    if (texel.a < 0.5) {
        discard;
    }
}
