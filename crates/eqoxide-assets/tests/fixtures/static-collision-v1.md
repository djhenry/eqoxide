# Collision decoder fixtures

`static-collision-v1.glb` and `static-collision-empty-v1.glb` were regenerated through `examples/static_collision_fixture.rs` in `djhenry/eqoxide_asset_server` at merged revision `b3faa47e70f88d81c58ac90ef33d3488ecde7c4d`. The generator and writer match the reviewed writer revision `5506c20b7452a6f3041952bfbf13d9e052ec190b` from [PR #74](https://github.com/djhenry/eqoxide_asset_server/pull/74). They contain only authored synthetic geometry and metadata.

| File | Bytes | SHA-256 |
| --- | ---: | --- |
| `static-collision-v1.glb` | 1488 | `5690344fc74c063db1c9a0901b5ce2b444d8f3bfa3902d3ac6cf0f742eac5cac` |
| `static-collision-empty-v1.glb` | 584 | `31cf699e0f6b2b424d248554bc0569cbffba0dd909069486d9dfb90ac3020dd9` |

The nonempty input contains four server-axis floor positions, one six-index triangle primitive and two distinct owners sharing the mesh. The second placement combines a 90-degree Z rotation, scale 2 and translation `(10,20,5)`. The empty component uses the legal omitted-node scene representation and both-face policy metadata. Both fixtures use a synthetic precomputed bake identity of 64 `a` characters, not a component content hash.

Regeneration from that producer revision:

```sh
cargo run --example static_collision_fixture -- OUTPUT_DIRECTORY
```

The fixture tests discriminate inverse axes, matrix conjugation, preserved index order, shared ownership, explicit empty availability and indexed world bounds. Recognized inspection policy metadata is not an implemented query capability.
