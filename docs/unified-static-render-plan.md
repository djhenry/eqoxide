# Unified static visual GPU inspection implementation plan

> **For agentic workers:** Use the agent-fleet development and independent acceptance process. Steps use checkboxes for tracking.

**Goal:** Render the common static visual contract through a separate GPU path and inspect actual output pixels without a game session.

**Architecture:** A private prepared scene is created only by the existing bounded decoder, then an upload-budget preflight validates device sizes before creating complete GPU resources. An isolated unlit shader evaluates common material and vertex-color inputs in server axes. A public offscreen example consumes this path without login, asset sync, world collision or the production renderer loop.

**Tech stack:** Rust, wgpu 22, glam, image PNG, existing eqoxide-assets decoder.

**Spec:** `docs/unified-baked-assets-design.md`; `docs/unified-static-reader.md`. Tracks #1147 and #1126. Depends on PR #1146.

## Global constraints

- Reuse `decode_static_visual` for schema, capability, geometry and PNG validation. Do not route through legacy assets or infer behavior from names/provenance. Prepared fields remain private so callers cannot bypass decoded-scene validation.
- Server geometry and instance matrices are already converted. The shader uses matrix times position directly; no WLD swizzle or actor offsets.
- Preserve shared vertex pools and mesh instances. Use a separate RGBA stream per colored primitive and one shared white fallback per mesh. Preserve index/material binding and texture aliases.
- Shader color is texture RGBA times material RGBA times vertex RGBA. Opaque ignores alpha for coverage and outputs alpha one; mask uses the full alpha product and the actual cutoff, with survivors outputting alpha one; blend uses straight alpha and disables depth writes. Single-sided uses back culling; double-sided disables it. Unlit inspection has no lighting, fog, shadows, PBR or gameplay readiness claims.
- Decode original PNGs to float32 without reducing 16-bit input to eight bits. Convert sRGB texture RGB to linear; keep alpha linear. Upload Rgba32Float and implement repeat/bilinear textureLoad sampling, requiring no optional float-filtering feature. Output supports explicit RGBA8 unorm/sRGB targets. Material factors and vertex colors remain linear.
- Opaque/mask use full instancing. Blend uses camera-depth-sorted primitive-instance commands without expanded geometry; equal-depth ties are stable. Primitive-center sorting cannot guarantee correct intersecting surfaces or within-primitive transparency; document this limitation.
- Defaults: 512 MiB aggregate GPU buffer/texture payload, 64 MiB individual float texture payload and 1,000,000 draw commands. Preflight the checked primitive-instance blend-command product before command allocation or texture conversion. Check byte arithmetic and device max buffer/texture sizes before allocation/upload. Caller overrides are explicit. Limits bound these payloads, not total process memory.
- New code lives alongside legacy modules; existing zone resources/shaders/passes, online reader advertisements and app lifecycle are unchanged. The new path is GPU material inspection only. Interactive preview is a separate later consumer.
- One local low-priority build job at a time, debug/incremental disabled; no remote builds or uploads. No merge/deployment without human review. All public docs/prose omit local/proprietary provenance.

## Review focus

- A colored and uncolored primitive sharing a pool must not share the colored RGBA stream.
- Alpha just above/below a nondefault cutoff must agree on GPU with the full texture/factor/vertex alpha product; opaque zero alpha remains visible.
- Shared transformed instances must render in server axes once; camera reversal must change blend order.
- 16-bit PNG precision must survive upload; all image aliases and fallback allocations count toward upload limits before allocation.
- Malformed artifacts, failed adapters and GPU validation failures must return an explicit error rather than an empty successful image or legacy fallback.

## Task 1: Isolated prepared scene and GPU encoder

**Owner:** renderer worker. Files: new `crates/eqoxide-renderer/src/static_visual.rs`, new `src/shaders/static_visual.wgsl`, export in `src/lib.rs`, new renderer integration tests and dev-only dependencies if needed.

Interfaces:

```rust
pub struct UploadLimits { pub max_gpu_bytes: usize, pub max_texture_bytes: usize, pub max_draw_commands: usize }
pub struct PreparedStaticVisual { /* private validated CPU data */ }
impl PreparedStaticVisual {
    pub fn decode(bytes: &[u8], limits: &DecodeLimits, upload: &UploadLimits) -> anyhow::Result<Self>;
    pub fn bounds(&self) -> &Bounds;
    pub fn stats(&self) -> VisualGpuStats;
}
pub struct GpuStaticVisual { /* private complete GPU resources */ }
impl GpuStaticVisual {
    pub fn upload(device: &wgpu::Device, queue: &wgpu::Queue,
        scene: PreparedStaticVisual, format: wgpu::TextureFormat) -> anyhow::Result<Self>;
    pub fn prepare_camera(&mut self, queue: &wgpu::Queue,
        view: glam::Mat4, projection: glam::Mat4) -> anyhow::Result<()>;
    pub fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) -> anyhow::Result<()>;
    pub fn bounds(&self) -> &Bounds;
    pub fn stats(&self) -> VisualGpuStats;
}
```

`VisualGpuStats` exposes counts for shared mesh buffers, instance records, primitives, textures/images and budgeted GPU payload; define its exact fields with the orchestrator before the example is authored. Camera preparation computes stable view-depth ordering and updates the view-projection uniform. Drawing fails until camera preparation succeeds. Failed camera preparation invalidates the prior prepared state, preventing a stale camera draw. Drawing uses a caller-owned pass with matching color format and Depth32Float; it does not clear depth or submit/activate a scene.

- [x] Write failing public-API preparation tests using the producer fixture: one shared mesh/two instances, isolated primitive RGBA, material bindings, texture aliases, correct bounds, checked upload limits and invalid-artifact failures.
- [x] Implement private preparation and budget preflight, normalized float texture conversion, shared buffers and four fixed-function variants (opaque/mask or blend, each culling choice). Validate finite camera input and device limits; return contextual upload failures.
- [x] Add Naga validation of the actual shader and actual GPU offscreen pixel tests, explicitly ignored when a GPU is not supplied by ordinary CI. A deliberate GPU acceptance run must not skip on adapter absence.
- [x] Prove factor/color/cutoff, opaque alpha, blend order, culling, server-axis placement and PNG precision with actual render/readback. Demonstrate targeted guard/shader mutations failing then restore and rerun.
- [x] Run existing renderer all-target tests with no legacy file behavior changes. Commit the reviewed code/tests.

## Task 2: Actual artifact probe and documentation

**Owner:** orchestrator. New `crates/eqoxide-renderer/examples/static_visual_render.rs`, `docs/unified-static-gpu.md`; no app-loop changes.

- [x] Provide a bounded GLB file reader feeding PreparedStaticVisual, then upload and draw to a 512x512 color/depth target using a server-axis camera framed from validated bounds. Copy aligned GPU rows back to an actual PNG; any adapter, decode, upload, map or encoding failure exits nonzero.
- [x] Print inspection statistics, bounds and adapter/output data without gameplay or rendering-readiness assertions. Use linear-to-sRGB output consistently.
- [x] Run the real producer fixture through the actual example and inspect the rendered image. Document CLI, budgets, material/sampling conventions, sorting limitations and separation from runtime support.

## Task 3: Independent acceptance and human review

**Owner:** separate reviewer and orchestrator.

- [ ] Reviewer refutes code/contract, repeats renderer suites and meaningful mutations, independently renders actual pixels/artifacts and verifies restored resources/sources. This unit's actual observable is GPU rendering; no game session behavior is activated.
- [ ] Scan public source/docs/bodies, validate factual references and actual commands. Publish a PR stacked on #1146, ready for human review. No merges.

## Subsequent consumer

Interactive offline preview should enter before login-config/gameplay initialization, reuse these GPU resources and expose render-only camera/frame/local-exit control. It must avoid current testzone asset-sync workers and camp watchdog, distinguish render inspection from world readiness, and report connection/collision/navigation unavailable. This needs its own independently reviewed implementation unit.

## Author acceptance evidence

Renderer all-target tests passed 289 tests with 20 ignored; the seven new GPU cases are explicitly ignored in ordinary runs. The explicit targeted acceptance run passed all 13 new tests, exercising a real Vulkan device without optional features. Ten temporary mutations caused targeted assertion failures, then restored tests passed. The actual example rendered the committed producer fixture to a 512-by-512 PNG, with one shared mesh, two instance records, two primitives and color buffers, two draw commands and 468 budgeted GPU bytes. Server bounds matched `[-3,-7,2]` to `[16.44179,13,6.5]`. Missing input returned nonzero and preserved the prior PNG. Independent acceptance remains the next gate.
