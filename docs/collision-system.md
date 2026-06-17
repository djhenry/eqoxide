# Collision System

Implemented in `src/assets.rs`. Provides spatial queries against the loaded zone
geometry for three purposes: player grounding, camera collision, and nameplate
occlusion culling.

---

## Overview

```
ZoneAssets (meshes + textures)
    ↓  Collision::build(assets, cell_size=32.0)
Collision {
    tris:  Vec<[[f32;3];3]>,   // flattened triangles in world space [east,north,height]
    cells: Vec<Vec<u32>>,      // uniform XY grid: each cell → triangle indices
    origin, cell_size, cols, rows
}
```

The grid is built once per zone load and shared via:
```rust
pub type SharedCollision = Arc<RwLock<Option<Arc<Collision>>>>
```

The render thread builds and publishes; the nav thread reads for movement gating.
Both hold the `Arc<Collision>` so triangle data is not duplicated.

---

## Coordinate Space

All positions in `Collision` are GPU world space: `[east, north, height]`.

libeq_wld mesh positions come as `[east, height, north]` — height in the middle.
`Collision::build` re-orders them:
```rust
[pos[i][0] + center[0],   // east
 pos[i][2] + center[2],   // north  ← swap [2] ↔ [1]
 pos[i][1] + center[1]]   // height
```

---

## Public Methods

### `floor_z(east, north, fallback) → f32`

Samples the floor directly beneath `(east, north)`. Returns the highest triangle
the downward ray passes through that is at or just below `fallback`.

Triangle-based barycentric interpolation — **not** nearest-vertex. Nearest-vertex
was the original implementation and caused the player to float up to wall height
when standing close to a vertical surface. The triangle test correctly ignores
walls (their XY-projection has ~zero area).

Used in `app.rs: ground_z()`, cached per 2 EQ units of horizontal movement.

### `nearest_hit_t(from, to) → Option<f32>`

Möller–Trumbore ray-triangle intersection along segment `from → to`.
Returns the hit parameter `t ∈ (0, 1]` (fraction along the segment) or `None`.

Used for:
- Camera collision (`app.rs`): pull the eye back before the wall
- Nameplate occlusion (`hud.rs: draw_labels`): skip label if segment to head is blocked

### `segment_blocked(from, to) → bool`

Convenience: `nearest_hit_t < 0.92`. The 0.92 cutoff prevents the entity's own
floor from counting as an occluder (its feet are at the far end of the segment).

### `path_clear(from, to, radius) → bool`

Movement gating. Extends the segment past `to` by `radius` so the player stops
short of the wall instead of clipping into it. Returns `true` (clear) when no
geometry is loaded.

---

## WASD Collision (app.rs)

```
attempt full diagonal move (Δeast, Δnorth)
    → clear?  → move
    → blocked → try east-only
        → clear?  → slide east
        → blocked → try north-only
            → clear?  → slide north
            → blocked → stop
```

Both cast are at chest height (`z + 3.0`) so stair lips and knee-high floor edges
don't block the move.

---

## Navigation Collision (navigation.rs)

`slide_move()` implements the same three-attempt logic for the nav thread's
automated `/goto` movement. Returns `None` only when fully boxed in, which
logs "Path blocked by a wall" and clears the goto target.

---

## Nameplate Occlusion (hud.rs)

`draw_labels` skips a nameplate if:
1. The entity's screen projection is off-screen, OR
2. `col.segment_blocked(cam_eye, [b.pos[0], b.pos[1], b.pos[2] + 4.0])`

The `+4.0` on height aims toward the entity's head/label, preventing a low floor
edge in front from hiding an otherwise-visible NPC.

---

## Performance

Cell size of 32 EQ units means a typical zone query touches 1–4 cells. Query time
scales with the number of triangles in those cells, not the total zone size.
Previous implementation (per-frame linear scan of all triangles) dropped to 33 fps
in large zones; the grid keeps it at 60+ fps.

---

## Tests

`src/assets.rs` includes unit tests that create synthetic geometry (floor quad +
vertical wall) and verify:
- `floor_z` returns floor height, not wall height
- `segment_blocked` correctly identifies occluded vs. clear segments
- `path_clear` blocks walking into a wall and allows sliding parallel to it
- Empty collision: `floor_z` returns fallback, `path_clear` always returns true
