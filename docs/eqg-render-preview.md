# EQG render preview

The offline preview loads an explicit EQG preview GLB into the client's zone renderer. It does not log into a game server or publish assets. It provides no collision, navigation, region, or gameplay validation.

Generate a GLB with the asset server's `export-eqg-preview` command, then launch:

```sh
eqoxide --testzone --preview-glb /path/to/crescent-preview.glb --api-port 8841
```

Choose an unused API port. `--preview-glb` requires `--testzone`; an unreadable or unsupported file reports a load error instead of substituting the synthetic test scene. The loader accepts the exporter's flat scene nodes and adapts their geometry and placements to the client's rendering coordinates. This does not establish alignment with server gameplay coordinates.

Use `GET /v1/observe/debug` to inspect status, `GET /v1/observe/frame` to capture the frame, and `POST /v1/camera` to inspect another location:

```json
{"focus":[0,0,100],"azimuth":0,"elevation":0.6,"radius":400}
```

Camera angles are radians. The initial view frames the complete scene; reduce radius and move focus to inspect building placement, terrain seams, foliage, and cutouts closely. Preview status is `render_preview`, with collision unavailable. Geometry and navigation endpoints must continue refusing gameplay queries.

The renderer preserves GLB vertex alpha and material alpha cutoff for masked terrain and placed objects, including masked object shadows. Ordinary zone assets retain legacy texture color-key recovery; explicit previews use their texture alpha unchanged.

This remains a visual preview. Unsupported exporter materials may be opaque approximations; vertex RGB, native lighting, secondary UVs, normal maps, dynamic tint/fade, animation, culling fidelity, doors, and collision semantics are not established by a successful render. Human testing should compare terrain and object placement plus supported cutouts, and record concrete discrepancies without treating visual success as gameplay readiness.

Known shutdown limitation: API `/v1/lifecycle/exit` can reach the offline watchdog and crash during teardown ([#1133](https://github.com/djhenry/eqoxide/issues/1133)). Sending `SIGTERM` to the specific preview process was verified to exit cleanly. Do not use a command that terminates other running clients.

For an explicitly server-axis visual inspection, use the adapter mode:

```sh
eqoxide --testzone --preview-glb /path/to/crescent-preview.glb \
  --preview-server-axes --api-port 8841
```

`--preview-server-axes` requires a preview GLB and offline testzone mode. It swaps source X/Y consistently across terrain, object placements, normals, and camera bounds, with triangle winding corrected. The default command retains source coordinates. Logs identify the selected coordinate convention. Camera focus values must use that same convention; swap source X/Y when switching to server axes.

Both modes remain `render_preview`: collision and navigation queries are unavailable, and no connection to a game server is made. The server-axis option implements the numeric transform documented in `eqg-coordinate-contract.md`; it does not prove live server alignment or apply an actor-origin height offset.
