# Static collision geometry decoder

The `eqoxide-assets::static_collision` module decodes the source-independent collision component defined by the [asset server's version-1 geometry contract](https://github.com/djhenry/eqoxide_asset_server/blob/b3faa47e70f88d81c58ac90ef33d3488ecde7c4d/docs/static-collision-contract.md). It implements the geometry-only step tracked in [issue #1154](https://github.com/djhenry/eqoxide/issues/1154), part of [project #1126](https://github.com/djhenry/eqoxide/issues/1126).

`decode_static_collision(bytes, &CollisionDecodeLimits)` returns owned server-axis position pools, indexed triangle primitives and flat instances with stable opaque owner identities. Each mesh retains one shared position pool. Indices preserve counterclockwise orientation. The decoder applies the inverse proper asset rotation `(x,y,z) -> (x,-z,y)` and conjugates instance matrices without reversing winding or adding an actor height offset.

The returned `CollisionHeader` requires artifact schema 1, collision metadata version 1, reader contract 2 and exactly `static-collision-triangles-v1`. `CollisionAvailability` distinguishes `Available` from `AvailableEmpty`; it cannot represent missing or unsupported data as known-empty. `InspectionQueryPolicy` records recognized front-face or both-face segment inspection metadata. This module implements no query operation, movement, navigation, regions or dynamic actor behavior. A recognized policy is not operation support, and this change adds no online capability advertisement.

Available geometry requires embedded FLOAT VEC3 positions with truthful min/max, u32 scalar indices, explicit TRIANGLES, one JSON and one BIN chunk, and one scene selecting every flat owner node exactly once. Every declared descriptor and mesh primitive is validated, including unused resources. Unsupported visual attributes, materials, extensions, URI resources and hidden application keys are refused. Owner IDs are required and unique; names and local node order cannot invent identities. Placement allows only finite affine proper rotation, positive uniform scale and translation.

Known-empty components use a JSON-only GLB with `scenes: [{}]`, omitted core geometry arrays and no BIN chunk. This legal core scene representation does not need a dummy triangle. Its decoded bounds are absent; it certifies only an empty declared triangle candidate dataset, not an empty world.

Local triangles and instanced asset/server triangles must have nonzero cross products widened from f32 positions to f64. World transforms use f32 operations before that check, so triangles that collapse through placement rounding are refused. Near-degenerate nonzero triangles remain representable without a promise of robust contact arithmetic. Bounds use indexed transformed surfaces only. Unreferenced positions are finite-validated locally and included in accessor min/max, but do not enlarge surface bounds or undergo per-instance world transforms. The visual decoder keeps its existing framing rule, which includes unreferenced vertices.

Private shared helpers handle bounded GLB chunks, original fixed-number tokens, dense accessor layouts and coordinate mechanics. Validated accessor handles contain their borrowed BIN and private descriptors; read methods check type, width and index instead of accepting substitute bytes. Visual and collision APIs retain distinct role validation, output types and limits. The visual profile still requires BIN, materials and its existing attribute formats.

## Limits

All counters use checked arithmetic before their corresponding scans or retained copies. These limits bound logical bytes and work; they do not measure allocator overhead or total process memory. Accessor validation charges all declared aliases before payload scans. Mesh primitive, retained pool/index and expanded instance budgets are separate. World work counts paired asset/server evaluations at each indexed reference, three per instanced triangle.

| `CollisionDecodeLimits` field | Default |
| --- | ---: |
| `max_glb_bytes` | 134217728 |
| `max_json_bytes` | 8388608 |
| `max_geometry_bytes` | 134217728 |
| `max_accessor_validation_bytes` | 268435456 |
| `max_resources` | 100000 |
| `max_vertices` | 5000000 |
| `max_indices` | 15000000 |
| `max_expanded_triangles` | 5000000 |
| `max_world_vertices` | 50000000 |

Callers must bound ingestion before allocating the input slice. The [inspection example](../crates/eqoxide-assets/examples/static_collision_probe.rs) checks file size and uses a capped read that also handles growth during ingestion:

```sh
cargo run -p eqoxide-assets --example static_collision_probe -- INPUT.glb
```

The example reports geometry/instance counts, bounded samples, indexed bounds, availability and declared policy; it explicitly reports segment operations unimplemented. It is a decoder inspection tool. Hashed scene-component pairing, query arithmetic and operation budgets, transactional activation, source coverage and gameplay acceptance remain separate work.

The [synthetic fixture provenance](../crates/eqoxide-assets/tests/fixtures/static-collision-v1.md) records regeneration through the actual producer. These producer-to-decoder checks establish the geometry boundary; they do not demonstrate gameplay or native collision completeness.
