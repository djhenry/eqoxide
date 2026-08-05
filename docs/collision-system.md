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

### `find_path(start, goal, radius) → Option<Vec<[east, north]>>`

**Grid A\*** over the collision cells — routes *around* walls and returns cell-center waypoints
(goal-inclusive), or `None` if no route / no geometry. This is what `/v1/move/goto` uses: it walks the
waypoints by emitting a `MoveIntent` toward the next carrot, and the ONE collide-and-slide mover
(`CharacterController::slide`, movement.rs) resolves each frame's motion against geometry. Added
2026-06-21.

- Walkable = a floor exists under the cell; an edge needs a small floor-height step (`STEP_H=20`)
  and a clear chest-height `path_clear` between cell centers.
- **Floor probe follows the terrain**: each cell's floor is probed relative to the floor of the
  cell it was reached from (`floor_near`), and the start floor is found by trying several reference
  levels — so multi-level dungeons work even when the caller's `start.z` is stale (a common bug:
  `gs.player_z` is often the spawn z, not the real floor). Don't pass a bogus z and expect failure —
  it self-corrects, but a wildly wrong z can still miss.
- Capped at `MAX_NODES` (`collision.rs`), currently **`8_000_000`**. This line said `200000` until
  #861 — wrong by 40×, and stale for long enough that it is worth reading the constant rather than
  this sentence. (The same line also claimed a `find_path: no route` diagnostic; **no such string is
  in the tree** — give-up is reported through the planner's `PlanOutcome`, so that clause went too.)
  The cap is a **precision** floor, not a safety floor: exceeding it returns
  `Exhausted(NodeCap)` ("I don't know"), never a false `Unreachable(SearchClosed)` ("no"). For what
  actually fits under it, and for the measured whole-zone figure, read `MAX_NODES`' own doc comment.
  Deliberately not restated here: a figure copied into a second doc is a figure that will go stale
  in one of them.
- **Limitations**: **doors / sealed pockets** — doors aren't in the collision; a room behind a
  closed door is a disconnected component, so A* correctly finds no route. See `autonomous-play.md`.
- **Water is not a blanket limitation — *when the zone has a water map*.** This section used to say
  A* "can't path across water (no walkable floor — water mobs like fish are unreachable)". That
  described the 2D-with-water-hacks planner and is not what the code does today. **Read the
  condition before the capability**, because both states are real and the client picks between them
  per zone at load:
  - **`.wtr` loaded (`RegionMap::try_load` → `Ok`)** — `astar` builds 4u water columns
    (`WaterColumn`) and emits genuine water-entry, 3D interior, water-descent, surface-crossing and
    haul-out edge families. Water mobs are **not** categorically unreachable.
  - **`.wtr` missing or unusable (`try_load` → `Err`)** — `build_zone_collision` (`app.rs`) always
    *calls* `set_region_data`, but it passes the loader's `Result` through, and `Collision::water`
    stays an `Err`. Every one of those families is gated on `region_map()`, which is
    `self.water.as_ref().ok()` — so on the `Err` arm **none of them exist**, the zone is navigated
    **water-blind**, and water reads as plain missing floor, i.e. the old sentence above becomes
    accurate again. The load failure is logged (`zone '…' has no usable region data: …`) and the
    reason is retained (`RegionDataAbsent`) rather than discarded. This is a modelled operational
    state with named causes in `region_map.rs`, not a corner case — do not assume the `Ok` arm.

  For the degraded arm, `specs/2026-07-16-water-navigation-design.md` §"Degraded-mode visibility"
  states it exactly and **is still current** — read it as fact, not history. Elsewhere both water
  specs are dated snapshots whose *other* current-state material has moved on, so check
  `collision.rs` before relying on those parts.

  On the `Ok` arm the remaining refusals are specific, and there are four, not one:
  `RejectReason::Water` (a water family's precondition is absent), `DescentBlocked` (an intervening
  solid surface under the landing), `HaulOutTooHigh`, and `Clearance` (which covers a blocked swim
  band as well as walls). Note that `PLAYER_BODY.haul_out_up` is a **planner admission threshold,
  not a physical limit on the character**: `tests/synthetic_water_capability.rs` pins the controller
  rising **8×** `haul_out_up` under ordinary buoyancy, because the swim plane is recomputed at the
  body's own column each frame. Do not read it as a capability boundary.

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

`/v1/move/goto` first computes an A\* route via `find_path` (above) when the goal changes, then walks
the waypoints by emitting a `MoveIntent` toward the current carrot each tick. There is now a **single
collide-and-slide model** — `CharacterController::slide` (movement.rs) — shared by nav-driven movement,
free WASD, and the combat auto-engage approach; the walker never resolves collision itself. Its contact
ray heights come from `traversability::PLAYER_BODY` (the same body the planner probes with, #378/#386).
The old `navigation::slide_move` (a second, divergent three-attempt slide with its own `z+3` chest ray)
was **deleted** in the #378 Phase 2 refactor — it had zero production callers; a second slide model that
nothing calls is exactly the planner/walker drift the refactor exists to prevent.

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
