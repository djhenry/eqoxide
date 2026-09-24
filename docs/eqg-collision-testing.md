# Offline EQG collision candidate testing

Issue [#1134](https://github.com/djhenry/eqoxide/issues/1134) adds an explicit consumer for the collision-only staging GLB exported by the asset server. This is the geometry verification step after the render preview. It does not enable collision in the running preview client or make a zone gameplay-ready.

The loader requires `extras.eqCollision` version 1, coordinates `eqg_gltf_y_up`, and scope `default_static_triangle_candidates`. Its collision node is flat and identity-transformed; positions already include terrain and object placements. The consumer converts each position from `(x,z,-y)` back to native source coordinates `(x,y,z)` once and preserves the exported collision winding. This coordinate convention still requires a separate comparison with server gameplay coordinates.

A dedicated typed input contains only validated collision triangles. The explicit grid builder has no visual asset argument, so rendered objects cannot be appended and object names cannot create ladder volumes. Ordinary unmarked zone assets retain their existing collision behavior. Unsupported or malformed staging artifacts fail instead of falling back to rendered geometry. The ordinary zone, preview, and object loaders reject any root `eqCollision` marker, including unknown versions.

The staging consumer bounds coordinates to one million source units per axis. Grid construction rejects nonfinite cell sizes or sizes below one unit, more than four million cells, and a conservative estimate above sixteen million triangle-to-cell references. These limits prevent unexpectedly large staging grids; they are not a claim that arbitrary input files have bounded decoding memory.

## Probe an exported artifact

The diagnostic example reads a collision staging artifact and builds the client's collision grid without starting a renderer or connecting to a server:

```sh
cargo run -j 1 -p eqoxide-nav --example eqg_collision_probe -- /path/to/crescent-collision.glb
```

The output reports source triangle count and bounds in source coordinates. Optional segment endpoints allow a concrete collision query:

```sh
cargo run -j 1 -p eqoxide-nav --example eqg_collision_probe -- \
  /path/to/crescent-collision.glb 0 0 100 0 0 -100
```

Coordinates are `x y z` for each endpoint. A hit reports a fraction along the supplied segment and a normal opposing its direction. A miss only describes that segment against these static candidates. It does not establish a walkable route or account for doors, regions, water, ladders, actor filters, or alternate collision queries.

## Remaining integration gates

- Compare floor and wall probes with known source placements and server positions.
- Establish runtime collision filtering and region semantics independently of visual appearance.
- Bind render and collision geometry in one atomic artifact with a capability gate before normal publication.
- Wire an explicitly scoped client testing mode while retaining honest readiness reporting.
- Independently validate movement and navigation before declaring human gameplay testing ready.

## Recorded validation

The assets and navigation all-target test suites passed 325 tests, with 20 environment-dependent tests ignored. Deliberately reversing the coordinate sign failed the coordinate regression; deliberately duplicating candidates failed the triangle-count regression. Both mutations were restored before the final passing run.

The diagnostic loaded the three representative staging exports with these triangle counts:

| Export | Loaded triangles |
| --- | ---: |
| Crescent | 300,306 |
| Guild Hall | 40,431 |
| Anguish | 459,859 |

For each export, a separate GLB inspection selected an upward-facing triangle and formed a short vertical segment centered on its centroid. All three grid queries hit within 0.000007 of the expected midpoint fraction, with upward-facing normals. This checks decoding and grid queries against the exported geometry; it is not an independent comparison with native runtime collision or server positions.

Independent acceptance passed for the offline geometry scope. The reviewer repeated the full suite (325 passed, 20 ignored), observed a coordinate-sign mutation fail, restored the source, and repeated the passing suite. Separately selected wall-oriented probes on all three native exports matched triangle counts and expected facing normals, with midpoint error below 0.000196. No running-client acceptance applies to this library and diagnostic change because it does not wire collision into the preview or gameplay.
