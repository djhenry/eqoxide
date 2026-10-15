# Canonical Query-Unit Frame Declaration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans task by task. Follow the repository's independent acceptance process and obtain human review before merging.

**Goal:** Transport an explicit canonical query-unit frame declaration through the existing collision GLB, descriptor, manifest and offline CPU reader without implementing a query.

**Architecture:** Add one required capability in the existing requirements array. Keep geometry, matrices, existing policy IDs and operation requirements unchanged. A typed writer argument opts in; an immutable decoded header reports the declaration, never original-source verification.

**Tech Stack:** Existing Rust, serde/serde_json, glam, GLB and BLAKE3 package APIs; no dependency or lockfile changes.

**Spec:** [Selected frame strategy and declaration unit](static-segment-query-policy-proposal.md). Producer work is [asset-server issue #82](https://github.com/djhenry/eqoxide_asset_server/issues/82); consumer work is [client issue #1168](https://github.com/djhenry/eqoxide/issues/1168).

## Global constraints

- Asset schema 1, collision metadata version 1, reader contract 2, `eqoxide-static-y-up-v1`, unitScale 1 and existing declared candidate scope remain unchanged.
- Mandatory collision capability is `static-collision-triangles-v1`; optional declaration capability is exactly `static-collision-query-unit-frames-v1`.
- No query policy meaning changes, proximity policy IDs, tolerance fields, required operations, online advertisement, gameplay activation or native-fidelity claim.
- `requiredOperations` remains present and empty. Old writer callers and old packages remain unspecified; no declaration is inferred from source family, names or matrix scale.
- Logical limits and strict duplicate/unknown-field rules continue to apply. Two required capabilities must fit the resource limit even for known-empty output.
- Terrain canonical frames use source world query units; models retain decoded model query units and poses. Vertex quantization decoding is required. Instance-scale folding or unit-changing output reparameterization is forbidden for a canonical declaration.
- Bare geometry cannot certify the source frame. Importer/source acceptance must establish it and include the frame policy in bake revision before selecting the explicit writer argument.

## Review focus

- A scaled local mesh must remain scaled through its matrix; an unchanged world surface does not excuse changing its query units.
- A known-empty component still carries and validates its requirements; absence of geometry cannot bypass capability/resource checks.
- Removing a capability in one layer with otherwise fresh hashes must fail header/descriptor agreement rather than producing a partial verified package.
- Component capability arrays accept order permutations and reject duplicates, unknown tokens and missing geometry capability. Descriptor root retains its sorted exact union; manifest capabilities retain existing canonical normalization.
- A declaration is not source verification or operation support. Probes must say declared-canonical and keep segment operations false.

## Task 1: Producer declaration and descriptor binding

**Files in the asset-server repository:** Modify `src/static_collision.rs`, `src/static_scene_descriptor.rs`, `tests/static_collision.rs`, `tests/static_scene_descriptor.rs`, `examples/static_collision_fixture.rs`, `examples/static_scene_descriptor_fixture.rs`, `docs/static-collision-contract.md` and `docs/static-scene-descriptor.md`.

**Interfaces:**

```rust
pub const CANONICAL_QUERY_UNIT_FRAMES_CAPABILITY: &str =
    "static-collision-query-unit-frames-v1";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryUnitFrames { Unspecified, DeclaredCanonical }
impl QueryUnitFrames {
    pub fn identifier(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::DeclaredCanonical => "declared-canonical",
        }
    }
    pub fn required_capabilities(self) -> &'static [&'static str] {
        match self {
            Self::Unspecified => &["static-collision-triangles-v1"],
            Self::DeclaredCanonical => &[
                "static-collision-query-unit-frames-v1",
                "static-collision-triangles-v1",
            ],
        }
    }
}
```

Add `write_static_collision_with_query_units(scene, bake_revision, policy, query_unit_frames, output, limits)` with the existing parameter types and `Result<ReaderRequirements>` return. The old `write_static_collision` delegates with `QueryUnitFrames::Unspecified`.

- [ ] Write `canonical_query_units_preserve_local_vertices_and_instance_scale`: use the existing fixture, replace its matrix with uniform scale four, write with the explicit declaration, import the GLB and assert the old local position accessor and converted scale-four matrix are retained. Assert requirements are the two sorted capabilities and the old front policy is unchanged.
- [ ] Run `cargo test --locked --offline --test static_collision canonical_query_units`. Expected RED for the absent declaration API; do not mistake a missing toolchain/dependency for that result.
- [ ] Rename the existing writer body to the explicit entry point and add the legacy delegate. Before constructing owned requirement strings, enforce the fixed capability-array budget:

```rust
let capabilities = query_unit_frames.required_capabilities();
ensure!(capabilities.len() <= limits.max_resources,
        "collision capability resource limit exceeded");
let requirements = ReaderRequirements {
    reader_version: 2,
    capabilities: capabilities.iter().map(|c| (*c).to_string()).collect(),
};
```

The rest of the existing position, matrix, index, JSON and publication code stays shared.

- [ ] In descriptor collision validation, retain mandatory geometry and allow only geometry plus the optional declaration capability. Existing unique-set validation, role scope, sorted union and binding remain authoritative; visuals/unavailable roles cannot smuggle the collision declaration.
- [ ] Add `canonical_query_units_empty_budget_and_legacy_defaults`: old empty output succeeds with max_resources one and one capability; canonical empty refuses at one, succeeds at two and retains no dummy geometry/BIN. A failed write preserves the previous file.
- [ ] Add `canonical_query_units_descriptor_union_and_refusals`: canonical available/empty collision contributes the declaration to the exact descriptor union; duplicate/unknown/missing-base capabilities and a stray root declaration with unavailable collision refuse.
- [ ] Run the two targeted integration test files, then `cargo test --locked --offline`. Expected all executed tests pass; record ignored native tests separately.
- [ ] Extend authored fixture tools with a strict optional `--canonical-query-units` mode. Include the selected declaration identifier in fixture bake identity. Default mode remains unspecified; no tool reports segment support or native source acceptance.
- [ ] Commit only the producer unit on its isolated branch; no merge.

## Task 2: Client decoding and immutable package agreement

**Files in this repository:** Modify `crates/eqoxide-assets/src/static_collision.rs`, `crates/eqoxide-assets/tests/static_collision.rs`, `src/static_scene_package/descriptor.rs`, `tests/static_scene_package.rs`, `crates/eqoxide-assets/examples/static_collision_probe.rs`, `examples/static_scene_package_probe.rs` and the corresponding decoder/package docs.

**Interfaces:** Define the same public capability constant and `QueryUnitFrames` variants/identifier method in the collision module. Add `CollisionHeader::query_unit_frames: QueryUnitFrames`. Preserve supported capability strings in `CollisionReaderRequirements`.

- [ ] Add the failing decoder test using existing fixture editing helpers:

```rust
#[test]
fn canonical_query_units_are_declared_not_inferred() {
    let bytes = edited(FIXTURE, |j, _| {
        j["extras"]["eqoxideAsset"]["requirements"]["capabilities"] = json!([
            "static-collision-query-unit-frames-v1", "static-collision-triangles-v1"
        ]);
    });
    let decoded = decode_static_collision(&bytes, &CollisionDecodeLimits::default()).unwrap();
    assert_eq!(decoded.header.query_unit_frames, QueryUnitFrames::DeclaredCanonical);
    let old = decode_static_collision(FIXTURE, &CollisionDecodeLimits::default()).unwrap();
    assert_eq!(old.header.query_unit_frames, QueryUnitFrames::Unspecified);
}
```

- [ ] Run `cargo test -p eqoxide-assets --locked --offline --test static_collision canonical_query_units`. Expected RED for missing status/support, with its actual cause recorded.
- [ ] Validate only the two permitted capability sets, with mandatory geometry and no duplicates. Derive status from explicit capability presence; preserve that set in the header. Keep the capability array's existing preflight limit and all geometry/policy validation.
- [ ] Extend private package descriptor collision allowed-capabilities with the declaration token. Do not modify requiredOperations or online client requirements. Existing header/descriptor set comparison must enforce agreement.
- [ ] Add package tests with the existing `Bundle` helpers: retag GLB/descriptor/manifest consistently and rehash to obtain a canonical package; independently strip/add the declaration at one layer with valid content hashes and assert refusal. Assert old package status remains unspecified, canonical known-empty binds, and no collision component yields no invented declaration.
- [ ] Add cap-order, duplicate, missing-base, unknown, non-string and resource-one/two tests to the role decoder. Exercise canonical available and legal JSON-only empty GLBs.
- [ ] Update both probes with `query_unit_frames` using the identifier method, keeping every segment-support field false. Label declarations honestly; no original-source verification field becomes true.
- [ ] Run `cargo test -p eqoxide-assets --locked --offline --lib --test static_collision --test static_visual`, then `cargo test -p eqoxide --locked --offline --lib --test static_scene_package`. Expected executed tests pass and ignored cases are reported independently.
- [ ] Commit the client unit on a branch based on the owned package reader, separate from the design PR; no merge.

## Task 3: Actual producer/consumer and independent acceptance

- [ ] Run the real producer fixture tools in default and canonical modes. Decode actual nonempty and known-empty outputs through a freshly compiled collision probe. Bind actual canonical packages through a fresh package probe, including an empty collision file generated by the producer with descriptor/manifest references rebuilt from its actual bytes.
- [ ] Verify local pools, indices, owner identity and instance scale from the actual decoded outputs; declaration is canonical only in explicit mode. Default assets remain unspecified. Keep query, gameplay, source coverage and native-parity observables false.
- [ ] Prove the preserved frame matters: inject a producer fault that folds the scale into the local pool and replaces the matrix with identity. The preservation test must fail despite unchanged world geometry. Restore exact source and rebuild.
- [ ] Inject distinct capability faults: drop the explicit writer capability, skip role duplicate/missing-base rejection, ignore descriptor/header agreement, or infer canonical status from matrices. Appropriate runtime assertions must fail; restore exact source and rebuild before each normal acceptance run.
- [ ] Run an old frame-unaware decoder on actual new producer output and record its explicit unsupported-requirements refusal; run the new decoder on old output and record unspecified. No fallback or reinterpretation is permitted.
- [ ] A separate agent independently reviews, runs the relevant suites, creates its own adversarial assertions/faults and generates DIFFERENT authored examples through the actual producer-to-reader path. This is CPU declaration acceptance only; there is no live game behavior or native corpus parity to claim.
- [ ] Check exact source/body hashes, references, privacy and byte identity after all restorations. Open ready PRs, attach them to this chat and wait for human review before merging. Enable no auto-merge.
