# Unified baked assets for eqoxide

Status: architectural proposal for user review. Independent design review accepted the corrected proposal on 2026-10-06. No runtime migration is implemented by this document.

Tracks the asset-format work in eqoxide #1126 and eqoxide_asset_server #51. The open client adapter PRs #1138 and #1140 remain unmerged pending human review and assessment against this direction. The older review findings the owner placed on hold remain deferred.

## Intent and success criteria

The owner requested one GLB-based format for EQG and pre-EQG content, including collision evaluation. Capabilities belong to the baked artifact, not its historical source. Enhanced WLD content must be able to use every implemented capability that EQG content can use, and vice versa.

Both importers must produce the same normalized intermediate representation and artifact contract. The client may branch on an explicit supported capability or artifact role, but never on source family, archive suffix, native shader name, or raw native collision flags. Source provenance is retained for diagnostics only.

Success means equivalent authored scenes from either source decode to equivalent visual geometry, collision semantics, and runtime behavior. A GLB that merely parses is not evidence of material fidelity, complete collision, or server alignment.

## Approaches considered

1. **Recommended: a versioned GLB-based package.** Separate visual and collision GLBs, with one manifest binding immutable artifacts and semantic metadata. Use glTF fields where they already express the feature; use a documented eqoxide schema for game semantics. Reuses existing geometry tooling and supports independent loading without conflating render and physics meshes.
2. **One monolithic GLB.** Technically viable with explicit scene/node roles and semantic metadata. Reduces the number of package components, but couples download and cache invalidation, makes physics-only consumers load a larger container, and is easier for old loaders or general viewers to misinterpret. A common format does not require a single file.
3. **GLB visuals plus a custom physics binary.** Could eventually reduce collision load time or memory, but introduces another writer, validator, debugging toolchain, and compatibility boundary before measurement demonstrates a need. Defer this. A disposable acceleration cache is a separate concern from the authoritative asset format.

The recommendation is not to invent a general-purpose scene engine or adopt a full rigid-body simulator. Define the smallest extensible contract that accurately describes the features we actually support.

## What exists today

These observations refer to the inspected client main and EQG work branches, not a claim that all branches have shipped.

- Asset-server legacy and EQG exporters already share mesh/node structures and the instanced GLB writer. This is a useful foundation for a common intermediate representation.
- Legacy zone geometry and EQG staging output use different coordinate conventions. EQG source-specific client adapters currently compensate for that difference.
- Standard glTF alpha properties coexist with application extras for additive blending and animated textures. The client currently consumes vertex alpha but does not provide a complete common vertex-color/material feature implementation.
- Legacy exports include terrain collision in a specially named mesh. The production client appends expanded visual objects and infers climb volumes from object names.
- EQG collision export includes terrain and static placements, with a declared scope of static triangle candidates. It is not a complete movement, region, or dynamic-actor representation.
- Legacy character baking already writes skins and animations. The EQG visual preview does not establish equivalent skeletal support.
- Existing content hashes help identify artifacts, but do not by themselves specify their coordinate convention, capabilities, completeness, or association with other artifacts.

Thus unification is more than renaming the current preview loader. The main work is agreeing on and enforcing truthful semantics at the exporter boundary.

## Architecture and responsibilities

```text
WLD/S3D importer ----\
                     -> normalized scene + semantic data -> validation -> package writer
EQG importer --------/                                          |
                                                                v
                                     immutable manifest + visual GLB + collision GLB
                                                                |
                                                                v
                                     one versioned client package loader
                                            /                   \
                                    renderer data           collision/region data
```

**Importers / libeq:** decode original files and preserve source facts. Upstream libeq changes should expose format data and validated parsing; eqoxide-specific publication rules, server axes, or renderer policy belong in the asset server unless upstream explicitly adopts a general abstraction.

**Asset server:** interpret source flags/shaders, normalize coordinates and transforms, resolve references, generate explicit collision geometry and semantic records, report unsupported content, validate the package, and publish it atomically.

**Client:** validate schema and capability support, decode the common representation, create GPU/runtime acceleration resources, animate supported content, and combine static data with current server-driven state. Runtime entity positions and actor-origin/foot conversions remain protocol responsibilities.

A shared contract still permits specialized renderer passes and collision query policies. Those distinctions describe behavior, not source formats.

## Package identity and compatibility

Extend the existing asset-server manifest and content-addressed distribution system; do not create a competing manifest service. Its current `set`, `digest`, file hashes, and chunk references already provide content revisions. What is new is an explicit format/reader compatibility contract, distinct from the content revision and glTF's own asset version. The manifest contains:

- schema major/minor, package identity, immutable bake revision, and producer policy revision;
- visual and collision artifact references with content digests and explicit roles;
- required capabilities and optional capabilities with defined fallback behavior;
- component availability and scope, including regions and semantic volumes;
- the fixed coordinate-profile identifier and unit convention;
- optional provenance and a conversion report for diagnostics.

### Client compatibility and cache identity

Add a compatibility declaration to the existing manifest. The machine-enforced requirement should identify an asset-reader contract version and required capabilities. The client advertises supported contract versions/capabilities; the server refuses incompatible requests with a structured upgrade-required error, and the client independently checks the returned manifest before downloading or activating assets. An error should identify the required reader contract, the client's support, and an appropriate minimum client release when a reliable release mapping exists.

A `minimum_client_version` release field can make that requirement understandable to users, but it must not be the only compatibility check. The inspected client package currently reports `0.1.0`, which does not distinguish these development builds. Asset-reader compatibility should be explicitly advanced when its semantics change, rather than inferred from a Git hash or assumed from an unchanged application version. Reject unsupported required features even when a client release number is nominally high enough.

Adding JSON fields alone does not protect already deployed clients: their current manifest deserializer ignores unknown fields. Gate new packages server-side on an explicit supported-reader handshake or a versioned endpoint/namespace. A request without compatibility support must not receive a new-format package. This gate applies before conditional `304` responses as well as normal manifest delivery. Offline loading must still validate the manifest/artifact header; it cannot rely solely on a previous server decision.

The current set digest hashes sorted file paths and file hashes, not manifest metadata, and it also drives the ETag. A compatibility-only change must therefore acquire a new immutable manifest revision and ETag; otherwise caches can miss the new requirement or the server can overwrite different metadata under the same identity. Preserve the existing digest as a content digest if useful, and add a manifest revision over canonical compatibility metadata plus content identity. Version that envelope through the compatibility-gated transport; update client cache records accordingly. This preserves file/chunk deduplication without treating identical payload bytes as identical compatibility declarations.

Each GLB carries a small `extras.eqoxideAsset` header identifying schema, role, coordinate profile, and bake revision. The manifest supplies artifact digests; avoid self-referential hashes inside a GLB. Contract metadata is application-specific, not a claim of a registered Khronos extension. If a capability later uses a formal glTF extension, declare it correctly in `extensionsUsed` / `extensionsRequired` as well.

The client rejects unsupported major versions, unknown required capabilities, incompatible coordinate profiles, invalid semantic references, mismatched revisions or digests, and incomplete downloads. A minor version is additive only; optional fallback must be specified per capability. Unknown metadata must not silently change gameplay behavior.

Publication is transactional at the manifest pointer: upload and verify all immutable components first, then expose the package. Do not serve a new visual asset with an old collision asset because each filename happens to exist.

Owner-approved migration policy (2026-10-06): new packages may require an updated client. Use a versioned cutover requiring a compatible client for the new package; retain the old asset set for rollback. Continuing to generate both old and new formats is possible but adds a maintained exporter and test matrix. Never overwrite an old client-visible asset path with a different coordinate contract.

## Coordinates, placement, and units

Use ordinary right-handed, Y-up glTF coordinates in the new geometry artifacts, with one fixed game-to-asset transform shared by every source importer. The client performs one fixed asset-to-runtime conversion. This common transform is not an EQG mode or a per-file correction knob.

For server geometry coordinates `(sx, sy, sz)`, the proposed mapping is:

```text
gltf = u * (sx, sz, -sy)
server_geometry = (gltf.x, -gltf.z, gltf.y) / u
```

Here `u` is a single contract-wide authoring scale in glTF meters per game unit, not a source-dependent value. Recommended initial convention is `u = 1` for simple numeric authoring and migration. Any fixed positive scale preserves gameplay distances when the inverse conversion is applied consistently. That is an authoring convention, not an assertion about EverQuest's real-world scale. A physically calibrated scale would affect DCC imports, light ranges, and future physics settings; select it before freezing version 1 if realistic meter scale is a requirement. Do not rescale the protocol, movement constants, or actor data as a side effect of this work.

Each source importer must first establish its mapping into server geometry coordinates. The existing EQG source-to-server X/Y swap belongs there. Source normalization must consistently transform positions, normals, centers, placements, winding, and any supported animation/bind data. Static geometry receives no actor-origin Z offset.

Preserve rendering instancing. Bake unsupported shear or reflection into mesh variants, or explicitly support and test those transforms; do not assume the existing heading/uniform-scale placement path covers arbitrary transforms. Correct normal transformation, winding, and skin/bind consistency are required where applicable.

The coordinate profile is fixed within a major version. Validate asymmetric terrain and rotated/scaled placements from both sources, then verify landmarks against server positions independently. Exporter/loader round trips alone cannot establish that the initial coordinate interpretation was correct.

## Visual capabilities

Use standard glTF geometry, node transforms, textures, vertex colors, materials, skins, and animation fields where applicable. The common representation must not discard data merely because the older pipeline did not expose it.

| Capability | Representation / rule | Rollout |
| --- | --- | --- |
| Static meshes and instances | Indexed triangles, reusable meshes, explicit node transforms | Baseline |
| Opacity and cutouts | glTF alpha mode, cutoff, base-color alpha and vertex color semantics | Baseline, with parity tests |
| Vertex color | Preserve RGB and alpha when present; advertise support truthfully | Generic client support required before publication with this capability |
| Additive surfaces | Versioned common material behavior; no source shader names in runtime dispatch | Explicit capability |
| Texture sequences | Versioned sequence/timing metadata referencing stable texture indices | Explicit capability; replaces filename inference |
| Normal maps, secondary UVs, lighting/material variants | Standard fields/extensions where appropriate, with documented engine interpretation | Incremental generic capabilities, not silently ignored inputs |
| Skeletons and action animations | glTF skin/animation data plus common action and attachment semantics | Dedicated migration milestone |

Preserve original appearance by default. A move to a physically based lighting model is a separate decision, not an automatic consequence of storing glTF materials. Do not replace an unknown source shader with opaque material and call the export faithful. Unsupported features produce explicit diagnostics and either block the requested production profile or use a declared, approved approximation.

A feature's schema and its actual renderer implementation are separate work. Supporting future enhanced legacy content does not require implementing every possible EQG shader now. It requires that added support be shared, capability-driven, and tested with source-independent fixtures.

## Enhancing older content

The original source format need not encode every future capability. Add an authored enhancement layer after import into the common intermediate representation, or accept validated authored GLB input through that same normalization boundary. Richer materials, extra UVs, replacement geometry, and explicit semantic volumes then have the same representation regardless of source provenance.

The package digest and conversion report must include enhancement inputs and their policy revision. Overrides resolve against stable importer-assigned identities, not incidental array order or non-unique display names. Geometry or placement edits require regenerating or revalidating associated collision and regions; a visual replacement does not automatically supply truthful collision. Keeping this authoring route open is part of the contract design; building a complete content editor is not part of the initial migration.

## Collision and world semantics

Adopt the same strategy for collision: one source-independent, validated collision representation. Use a separate collision-only GLB for geometry, with explicit referenced node/primitive roles and semantic metadata. Mesh names such as `__collision__` must not be the new contract's authority.

### Static collision

The exporter includes eligible terrain and static placements exactly once. The common client builder consumes only this explicit geometry; it must not append visual objects afterward. Legacy source collision eligibility must be resolved at bake time rather than blindly copying all rendered triangles.

Normalize raw source flags into named, documented query semantics. Keep raw bits only as source-qualified provenance. Movement blocking, line-of-sight, camera occlusion, and surface properties are not necessarily one boolean. Version each supported query policy, specify face-sidedness, and mark other query semantics unavailable rather than inventing equivalence.

Begin with the accurately bounded static-triangle scope. A component can be available without claiming full world collision completeness. Explicitly distinguish validated known-empty data from data that was not extracted or whose semantics are unsupported. A producer cannot self-certify runtime gameplay readiness with a single boolean.

### Volumes and dynamic state

Regions, liquids, zone transitions, climb volumes, and other gameplay semantics belong to the same package contract but are not all triangle meshes. Use typed records and shape data suited to each semantic, tied to the same coordinate profile and revision. Do not force every volume into a render mesh merely to keep it in a GLB.

Move evidenced ladder/name inference into the bake stage when producing the new contract, with explicit generated volumes and documented policy. Do not apply a legacy name heuristic to EQG or enhanced content without establishing that it has the same meaning there. Retain the ability to author those volumes directly for either source family.

Dynamic doors or moving platforms require stable entity/asset bindings, local collision shapes, and runtime transforms/state. Exclude runtime-controlled actor shapes from the static aggregate to prevent permanent closed-door barriers or duplicate collision. Baking their local geometry is possible; baking their current open/closed state is not. Their integration is an additional feature milestone, not something the new static schema magically supplies.

Navigation consumes the validated collision/semantic data. Agent dimensions, clearance, slope limits, movement capabilities, and route budgets remain runtime policy. Navmeshes and spatial indexes may be derived caches keyed by package digest, policy, and implementation version; do not make the current grid layout the interchange contract.

The first production milestone must name the exact operations it supports. Static data publication may proceed as an inspection capability, but enabling walking/navigation requires the applicable movement, region, server-alignment, and runtime acceptance gates. Missing capabilities must not be reported as working.

## Validation and migration sequence

1. Agree on the contract, compatibility policy, coordinate/unit convention, initial supported capability set, and collision scope.
2. Define the common intermediate representation and package validator. Make equivalent synthetic WLD/EQG importer outputs converge on the same decoded semantics; byte identity is not required where provenance differs.
3. Implement the common writer and both bake adapters behind a versioned output namespace. Preserve rendering instances; make static collision placement ownership explicit.
4. Add the versioned client loader and capability checks. During migration, an old-artifact compatibility path may coexist, but a marked new artifact must never silently fall back to legacy interpretation.
5. Validate representative existing zones and EQG zones, plus synthetic enhanced-legacy fixtures. Check materials, full vertex colors, placements, winding, coordinate bounds, collision query results, component association, and unsupported-capability errors.
6. Perform independent live landmark and gameplay acceptance for the declared production scope. Only then enable that scope in package publication and readiness.
7. Migrate existing bakes and retire temporary EQG-specific runtime adapters and source-format switches. Keep raw/source inspection in exporter tools if useful. Migrate characters, attachments, and animated objects in separately reviewable increments of the same contract.

Meaningful regression tests include duplicate-object collision detection, visible-but-passable surfaces, invisible barriers, asymmetric transforms, malformed/mismatched component references, missing versus empty regions, unknown required capabilities, and enhanced legacy fixtures using a newer generic material capability. Do not test only successful GLB parsing.

No production code changes are authorized by the existence of this proposal, and no PR merge is implied. The owner requested human review before merges.

## Costs and decisions to surface

- **Worth doing:** unification moves source quirks to the correct boundary and makes future enhancement work reusable. It is not a bad architectural direction.
- **Migration cost:** coordinate and collision changes require new bakes, cache separation, a client capability gate, and regression coverage for existing content. Updating old rigs correctly is more demanding than updating static vertices.
- **Runtime cost remains:** a bake cannot replace runtime support for animation, material effects, query policies, or dynamic state. Feature parity is incremental.
- **Format cost:** core glTF is not a complete EQ world-semantics specification. We will own a small versioned semantic schema and its validation/tooling. General GLB viewers can inspect geometry but do not validate gameplay meaning.
- **Collision cost:** including authoritative object collision and eliminating visual inference can change movement compared with today's legacy behavior. Those differences require investigation and explicit acceptance, not blanket preservation or silent replacement.
- **Fidelity cost:** some source shaders cannot be represented exactly by a chosen common material profile. Ask before accepting appearance loss, a more complex renderer implementation, or a broader profile.
- **Performance cost:** preserve render instancing and profile collision memory/load time before choosing flattening, primitive partitioning, or a specialized cache. No memory or bake-time estimate is justified yet.

Prompt the owner before extending old-client support, choosing lossy conversion, changing established visual appearance or movement behavior, requiring full dynamic physics for the first milestone, or choosing a physically calibrated unit scale that changes authoring assumptions. These are consequential product/scope decisions rather than routine implementation details.

## Evidence and references

Repository evidence inspected for this proposal:

- Asset server: `src/convert/mod.rs` common mesh/node/material writer and skin/animation output; `src/zone.rs` legacy placement and collision export; `src/eqg/export.rs` staging transforms and material subset; `src/eqg/collision.rs` static candidate export on the collision-export branch; `src/manifest.rs` publication identity.
- Client: `crates/eqoxide-assets/src/lib.rs` current loaders/material representation; `crates/eqoxide-nav/src/collision.rs` terrain/object assembly; `crates/eqoxide-nav/src/zone_assets.rs` readiness; `docs/eqg-coordinate-contract.md` source/server geometry distinction.
- [glTF 2.0 specification](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html): geometry/material/animation structure, coordinate conventions, and application metadata.
- [Khronos extension registry](https://github.com/KhronosGroup/glTF/blob/main/extensions/README.md): extension status must be checked before depending on a proposed physics extension. This proposal does not require an unratified physics extension.
