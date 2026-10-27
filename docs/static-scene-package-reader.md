# Offline static package CPU reader

`eqoxide::static_scene_package::decode_static_scene_package` verifies one explicitly requested immutable inspection package and returns owned CPU geometry. Both source families use the same descriptor and role profiles after baking. This stage provides no segment queries, gameplay collision, online advertisement, global activation or world-generation lease.

The pure API takes an expected zone and manifest revision, the existing typed `Manifest`, explicit `PackageFile { path, bytes }` pairs and `PackageDecodeLimits`. Supply `scene.json` plus exactly its available visual/collision references. No filenames establish role authority, and the reader never discovers files, reads a cache, appends visual geometry as collision or falls back to a previous package.

The reader bounds file/chunk metadata work and supplied bytes before canonical hashing. It reuses existing manifest integrity validation, canonical revision and whole-file BLAKE3. It requires the exact expected set/zone/revision, hashes the actual descriptor bytes, strictly decodes its typed schema, checks closed membership and requirement/reference agreement, and verifies **all** supplied files' actual size/hash before invoking either existing GLB decoder. Only after both available role decoders and their header associations succeed can a `StaticScenePackage` be constructed. A failure returns an error and creates no partial package.

This API verifies assembled whole files. Ordered chunk identifiers participate in manifest identity and bounded encoding checks, but individual CAS chunks are not read or verified and their concatenation is not proved here. Existing online transport remains on reader `1`/`legacy-assets-v1`; the compatibility helper's integrity extraction does not enable reader `2` online. Production chunk transport, transactional activation and query leases remain separate work.

The private descriptor wire schema conforms to `scene.json` schema `1` from `eqoxide_asset_server`, reader contract exactly `2`. This is an exact supported contract, not a semantic-version inequality. Required operations must be present and empty. Root capabilities are the sorted unique union of `static-scene-package-v1` plus available components. Available visuals require `static-visual-v1` and optionally `vertex-rgba-v1`; collision requires `static-collision-triangles-v1` and optionally `eqoxide-source-query-frames-preserved-v1`. The optional collision capability carries eqoxide's producer assertion that supplied source query frames are preserved: terrain uses decoded world query units, and models use decoded local query units with scale retained in instance transforms. Bare coordinates cannot prove preservation. This declaration establishes no universal EQ/RoF2 unit, source/game authentication, verified native behavior or query implementation. Future query policy and reference-engine fidelity remain separate. Manifest capability ordering and duplicates normalize through its existing identity rules, while descriptor requirements reject duplicates and unknown capabilities.

Both role records are mandatory. Each distinguishes `not-extracted`, `unsupported` and `available`; collision additionally supports `available-empty` with a real positive-size known-empty GLB. Unavailable records have no file, requirements, scope or policy payload. Both unavailable is a valid metadata-only inspection package with `scene.json` alone, never proof of an empty world or a query miss. Missing roles or a `missing` state refuse.

Descriptor fields reject unknown/duplicate decoded keys, including escaped aliases. Numeric schema/unit/reader/size fields use unsigned integer tokens; schema/unit are `1`. The shared bake revision is a precomputed lowercase64 input/policy hash, separate from descriptor/component hashes and manifest revision. Zone uses the existing bounded lowercase token grammar; set must be `zone/<zone>`. Referenced relative paths follow the existing manifest grammar, are distinct and cannot be `scene.json`. The initial package is closed, at most three files. Future roles or operations require a reviewed extension.

Decoded role headers must agree on asset schema, role, coordinate profile, unit, shared bake revision and required capability **sets**. Collision additionally agrees on scope, version, availability and front/both inspection policy. The visual header has no availability/scope field: its supported fixed declared scope and successful nonempty visual decoding establish available visual data. The reader does not require equal role bounds, meshes or instance counts. Visual framing bounds include vertex pools; collision bounds describe indexed transformed surfaces. Existing role decoders already normalize into server geometry axes, so no additional axis swap occurs.

The supplied-source query-frame preservation declaration must agree across the collision GLB, collision descriptor requirements, exact descriptor root union and manifest requirements. Fresh whole-file hashes do not excuse a disagreement. Available and known-empty collision components follow the same rule. An unavailable collision component contributes no declaration; it cannot assert preserved source frames through a stray root capability. The decoded collision header reports `DeclaredSourceQueryFramesPreserved` only for the explicit capability and `Unspecified` for older packages. Neither status is inferred from names, source type or instance scale. The old explicit `static-collision-query-unit-frames-v1` capability is refused even when every component and envelope hash matches; it is neither an alias nor unspecified.

Returned package fields are private and accessors immutable. It owns CPU arrays and identity strings, so caller input bytes may be dropped. Accessors expose zone/set/revision/bake identity, verified descriptor/component digests, explicit availability and optional `StaticVisual`/`StaticCollision`. Recognized collision policy remains metadata; it provides no query handler. A separately retained prior package stays intact after an error, but never substitutes for the newly requested identity. An explicitly requested older revision is inspectable; latest/current-world freshness is not inferred.

| Package limit | Default |
| --- | ---: |
| Descriptor bytes | 65,536 |
| Each available component | 134,217,728 |
| Aggregate assembled bytes | 268,500,992 |
| Supplied files | 3 |
| Manifest file work | 1,024 |
| Aggregate chunk references | 65,536 |

Visual and collision decoder limits are forwarded separately, including geometry/accessor/resource/world work and visual PNG limits. Declared sizes must fit outer and applicable role GLB bounds. These are logical input/work bounds, not total RSS estimates. The pure API receives an already typed manifest and cannot bound the caller's prior parsing or allocations.

The standalone example uses a separate 8MiB encoded-manifest limit before typed parsing, caps explicit file pairs before opening them, and reads caller-selected stable regular files with metadata preflight, opened-file recheck and a bounded `Read::take`. This is not an adversarial filesystem or race-proof path-security contract. No manifest path is joined to a directory or used for discovery.

```text
cargo run --locked --offline --example static_scene_package_probe -- \
  MANIFEST_FILE EXPECTED_ZONE EXPECTED_REVISION \
  --file scene.json DESCRIPTOR_FILE \
  --file visual.glb VISUAL_FILE \
  --file collision.glb COLLISION_FILE
```

Omit unavailable-role file pairs; logical paths must exactly match declared references. The JSON summary reports immutable identity, availability, decoded counts/bounds, collision `query_unit_frames` (`unspecified` or `declared-source-query-frames-preserved`) and `assembled_file_content_verified: true`, with `individual_cas_chunks_verified`, `runtime_activation` and `segment_queries` explicitly false. It needs no GPU, game session or network.

The fixture in `tests/fixtures/static-scene-package` comes from the actual pinned producer example; its README records revision/source/output hashes. `scripts/check-static-scene-package.sh CLIENT_CHECKOUT PRODUCER_CHECKOUT` regenerates that producer package, compares all four fixture files and invokes the actual consumer probe. Optional `STATIC_PACKAGE_PRODUCER_TARGET_DIR` and `STATIC_PACKAGE_CLIENT_TARGET_DIR` select independent build caches. This establishes authored producer-to-owned-CPU inspection, not native/real-zone completeness, game integration or gameplay readiness.
