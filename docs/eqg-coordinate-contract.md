# EQG source and server coordinate contract

This records the source-level coordinate investigation for [#1136](https://github.com/djhenry/eqoxide/issues/1136). It is an integration constraint, not a live gameplay acceptance result.

## Numeric mapping

For a static EQG world-space point `(a,b,c)`, the established mappings are:

| Space | Position |
| --- | --- |
| EQG source and native geometry query | `(a,b,c)` |
| Collision staging GLB | `(a,c,-b)` |
| Current offline collision probe | `(a,b,c)` |
| Current render preview world | `(a,b,c)` |
| RoF2 wire / EQEmu server geometry position | `(b,a,c)` |

The native terrain loader preserves source coordinates. Native geometry queries use that same axis order as the entity position triplet. The movement serializer places the first component in wire `y_pos` and the second in wire `x_pos`. The RoF2 wire structure and server handling preserve those named fields. Consequently, source X maps to server Y, and source Y maps to server X. Use numeric field mappings rather than compass-direction labels, which are inconsistent in existing comments.

The collision staging metadata `eqg_gltf_y_up` describes source-derived geometry. It does not claim server coordinates. The existing probe accurately reports `native_source_xyz`. A successful preview and export-to-consumer probe therefore do not prove alignment with a server position.

## Z datum

Static geometry needs no character-origin offset. Source terrain height `c` remains geometry height `c` in server axes. Player and NPC wire positions are a separate datum: the client currently converts ordinary grounded humanoid model-origin Z to foot Z at the packet boundary using `WIRE_Z_OFFSET` (3.125). This existing convention is not proof of a universal native actor offset; flying, floating, model size, and movement behavior require their own rules.

Do not add or subtract the actor offset from terrain vertices. A controlled player comparison must record whether its observed Z is a wire model origin or an already converted foot position.

## Integration sequence

1. Preserve the existing source-coordinate staging artifact and diagnostic contract.
2. Introduce an explicit source-to-server adapter when adding server-aligned preview or gameplay support. Apply it to visual geometry, collision geometry, placement matrices, normals, and camera/probe coordinates consistently.
3. Account for the orientation change: swapping X and Y has determinant -1. Triangle winding must be adjusted at that boundary so upward floor faces remain upward after transformation. Test an asymmetric floor and wall fixture with distinct X and Y coordinates; origin-centered or symmetric fixtures cannot detect the swap.
4. Validate several distinct native landmarks against server positions in a controlled session. Include floor heights with the actor datum handled explicitly and wall probes that distinguish swapped axes. Record native/source, server/wire, and client foot coordinates separately.
5. Independently accept rendering, collision, and movement before changing readiness or publication behavior. Regions, water, runtime actor filters, and navigation remain separate gates.

The existing render-only preview and offline source-coordinate collision diagnostic remain useful. They must not be silently reinterpreted as server-aligned assets.

## Client trace

- `crates/eqoxide-assets/src/eqg_collision.rs` converts staging GLB positions to `(p[0], -p[2], p[1])`, recovering source axes.
- `crates/eqoxide-assets/src/lib.rs` adapts preview geometry to the renderer's upload convention; the renderer's final permutation recovers source axes.
- `crates/eqoxide-protocol/src/protocol/mod.rs::decode_position_update` decodes wire X and Y into equally named fields.
- `crates/eqoxide-net/src/packet_handler.rs` assigns those fields directly to player X/Y and converts grounded self Z to foot datum.
- `crates/eqoxide-renderer/src/scene.rs` uses player X/Y/Z directly for the player scene position.

Native source and query/serialization findings were checked against the RoF2 client. Detailed research provenance is retained outside tracked public documentation. Live source-to-server landmark acceptance remains outstanding.
