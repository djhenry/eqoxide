# Unified static visual CPU decoder implementation plan

> **For agentic workers:** Use the agent-fleet development and independent acceptance process. Steps use checkboxes for tracking.

**Goal:** Decode the common static visual artifact into validated source-independent CPU data without activating it in the game.

**Architecture:** A bounded GLB parser validates the explicit visual header and static profile before reading embedded buffers/images. It retains shared meshes and instances, material semantics, full RGBA and original PNG payloads. One fixed inverse maps Y-up asset geometry and instance matrices into server geometry axes.

**Tech stack:** Rust, gltf, glam, serde_json, image PNG decoder.

**Spec:** `docs/unified-baked-assets-design.md`; producer contract in [asset-server PR #60](https://github.com/djhenry/eqoxide_asset_server/pull/60). Tracks #1145 and #1126.

## Global constraints

- Only schema 1, visual role, `eqoxide-static-y-up-v1`, unit scale 1, a valid lowercase 64-digit bake revision, reader 2 and implemented required capabilities are accepted. Require `static-visual-v1`; colored primitives also require `vertex-rgba-v1`.
- Resource counts/offsets/references require unsigned JSON integer tokens, with no floating-point repair. Version fields are integral numeric values: accept equivalent JSON `1`/`1.0`, reject fractional/out-of-range numbers. Unknown required capabilities, roles/profiles and header semantics fail. No missing-field compatibility defaults.
- Convert asset `(x,y,z)` to server `(x,-z,y)`, and conjugate node matrices by the same inverse. This positive-determinant mapping preserves winding. Check world positions in both asset and decoded server arithmetic to prevent addition-order overflow.
- Preserve normalized material RGBA, MASK cutoff, BLEND, double-sided choice and optional per-primitive full RGBA. Images remain exact embedded PNG bytes after bounded decode validation, retaining 16-bit input without quantization. Reference resources by indices, not names or archive provenance.
- Static baseline is flat reusable meshes/instances, shared position/normal/UV pools per mesh, indexed triangles and primary UVs. Reject unsupported hierarchy, skins, animations, morphs, extra attributes, nondefault sampler/material features or behavioral extras rather than dropping them.
- Reject external/data URIs; accept only GLB BIN-backed buffers and PNG buffer views. No additional file or network reads from document references.
- Default limits: encoded GLB 128 MiB; JSON 8 MiB; decoded attribute/index/color storage 128 MiB; aggregate retained PNG bytes 128 MiB; aggregate decoded PNG validation bytes 256 MiB; each PNG at most 64 MiB encoded/decoder allocation and 8192 pixels per dimension; each resource collection at most 100,000 entries; aggregate accessor validation payload work (including unused aliases) 256 MiB; world-bound evaluation 50 million mesh-vertex visits. Expose limits explicitly, fail on checked budget overflow before collection allocation. These are payload budgets, not a guarantee of total process memory usage.
- The public entry point catches dependency panics and returns a contextual failure. No partial scene activation or fallback to the old source-specific loader.
- Existing production loaders, shaders, asset sync advertisements and readiness stay unchanged. CPU parsing is not renderer support; do not advertise reader 2 online. No collision inference from visual mesh names or geometry.
- One local build at a time, one low-priority job, debug/incremental disabled. No merge or deployment without human review.

## Review focus

- Underdeclared vertex-color data must fail, not lose RGB/alpha or silently accept a baseline-only header.
- Oversized/truncated accessors, buffer views and PNGs must fail before allocation or partial output; external references must not access another file or URL.
- Asymmetric translated/rotated/scaled instances must decode to server axes with normal/winding parity and finite world bounds.
- Textured tint, base alpha, nondefault MASK cutoff and color isolation must survive actual producer-to-client decoding.
- A visual mesh named `__collision__` or with different source diagnostic names/generator must remain visual and decode by contract, without gameplay claims.

## Task 1: Public typed decoder and bounded validation

**Owner:** implementation worker. Create `crates/eqoxide-assets/src/static_visual.rs`, export it from `src/lib.rs`, update crate dependencies if needed. Create `crates/eqoxide-assets/tests/static_visual.rs`.

Public entry point:

```rust
pub fn decode_static_visual(bytes: &[u8], limits: &DecodeLimits) -> anyhow::Result<StaticVisual>;
```

`StaticVisual` retains header/bake identity, shared geometry, primitive material/index/color bindings, normalized materials, image PNG payloads and indexed textures, instance mesh references/server-axis matrices, and finite server-world bounds. Keep image/texture resources separate to preserve aliases. Publish the exact type interface early for the orchestrator's probe.

- [x] Add failing tests with the producer-generated synthetic fixture. Assert one mesh/two instances, bounds `[-3,-7,2]` to `[16.44179,13,6.5]`, material factors/cutoff and full color values.
- [x] Validate GLB/JSON limits and header before decoding buffers or images. Validate all resources, including unused ones, and check declared element counts/buffer bounds with overflow-safe arithmetic before collecting arrays.
- [x] Implement strict static profile checks and budget accounting. Validate equal attribute/color counts, finite positions/UVs, unit normals, normalized factors/colors/cutoffs, triangle indices/references, proper positive-uniform affine matrices and world bounds. Preserve primitive color isolation and instance references.
- [x] Decode/validate embedded PNGs within limits and retain original payloads. Check aggregate decode/retention budgets before decoding/copying, including alias entries.
- [x] Convert geometry and instance matrices to server axes once; preserve winding. Wrap dependency decoding in panic isolation and contextual Result errors.

## Task 2: Cross-repository conformance and inspection probe

**Owner:** orchestrator owns fixture, documentation, example; worker owns decoder tests.

- [x] Check in the small native-free GLB produced by `examples/static_visual_fixture.rs` from producer PR #60, with source/revision provenance in a fixture README.
- [x] Add a public-API example `crates/eqoxide-assets/examples/static_visual_probe.rs` that bounds file reading, decodes through the actual API and reports header, counts and server bounds. It must not print readiness or activate gameplay.
- [x] Add malformed header/version/requirement, missing/unknown capability, buffer/accessor truncation, texture URI/size, unsupported feature, matrix/overflow, name/provenance independence and budget tests. Verify errors leave caller-owned source bytes unchanged.
- [x] Keep legacy asset loader tests passing; no legacy loader invocation for a marked unified artifact.
- [x] Run crate all-target tests, meaningful mapping/color/capability mutations and restored checks. Run the actual producer writer and this probe against its output sequentially.

## Task 3: Independent acceptance and review request

**Owner:** independent reviewer plus orchestrator.

- [ ] Reviewer refutes the decoder, repeats suites and mutations, and independently parses the producer fixture for actual CPU semantic agreement. Renderer/game runtime is outside this unit; actual artifact decoding is the observable boundary.
- [ ] Document limits, pixel preservation, unsupported features and the distinction between CPU decode support and online/runtime compatibility. Scan public prose and fixture provenance before committing.
- [ ] Publish a PR linked to #1145, ready for human review. Keep #60 and all new PRs unmerged until reviewed.

## Follow-on units

Add the generic rendering path and controlled offline preview lifecycle; then enable truthful reader advertisements only for supported operations. Source adapters must normalize raw WLD/EQG axes/material semantics at bake time; collision and world semantics use a separately declared contract. Package association, server landmark and gameplay acceptance remain required before production publication.
