# Plan: Debug Test Zone — Character Model Rendering

## Problem

All character/NPC models (loaded from EQ `_chr.s3d` archives) render with arms and
legs smashed together on top of each other. The debug zone ("testzone") currently
only shows a ground plane + axis sticks — no character models are rendered at all,
so there's no way to visually inspect or debug model rendering.

## Root Cause Analysis

The rendering pipeline for character models depends on `scene.billboards` — a list
of Billboard structs with `race`, `pos`, `heading`, etc. Billboards are populated
from `GameState.entities` which only has data when connected to a live EQ server.
In testzone there's no server connection, so the billboard list is always empty and
character models are never rendered.

Even if billboards existed, the static entity pass (`encode_entity_pass`) and
skinned entity pass (`encode_skinned_entity_pass`) both look up archetypes from
billboard race strings. The `y_bottom` / `x_center` / `z_center` values computed
during `load_from_chr_s3d` may also be incorrect for the EQ coordinate system
(EQ: [east, height, north] vs rendering: [east, north, height]).

## Plan

### Step 1: Add test billboard injection in `app.rs`

When zone == "testzone", after the scene is built from game_state, inject a
hardcoded list of Billboard entries — one per loaded archetype — spaced out along
the east axis so every model is visible side-by-side.

Each billboard gets:
- `race`: the EQ race code (e.g. "HUM", "ELF", "DWF", "GNL", "SKE", "ZOM", etc.)
- `pos`: [east, north, z] with z=0 (ground level) and increasing east offset
- `level`: 50 (high enough to avoid level-0 placeholder logic)
- `action`: "idle" (standing still)
- `heading`: 0 (facing north)
- `id`: unique per archetype (1000+index to avoid collision with real spawn_ids)

Location: `render_frame()` in `app.rs`, right after `self.scene = SceneState::from_game_state(...)`.

### Step 2: Add `inject_test_billboards()` method to `SceneState`

A helper function in `scene.rs` that populates `billboards` with one entry per
loaded archetype. This keeps the test logic self-contained.

### Step 3: Fix `y_bottom` for static chr.s3d models

The `load_from_chr_s3d()` in `models.rs` computes `y_bottom` as:
```rust
let wy = p[1] + m.center[1]; // libeq: [east, height, north]
```
This is correct (p[1] = height in S3D convention). But `y_bottom` is the distance
from Y=0 to the model bottom. If the model's feet are at Y=5 and head at Y=20,
`y_bottom` should be 5.0 (distance feet are below origin). The current code takes
`y_min` (the minimum Y) and negates if negative — this gives 0 when the model sits
at Y>=0, which means no vertical lift and the model is placed at ground level
correctly. But the `visual_scale = 2.0 * y_bottom * arch_scale` formula needs
`y_bottom` to be >0 for the model to have any visible size.

**Bug**: For chr.s3d models where all vertex Y values are >= 0, `y_bottom` is 0,
making `visual_scale = 0` and the model invisible (or infinitely thin).

**Fix**: Change y_bottom computation to measure the model's total height
(max_y - min_y) rather than distance below origin. This gives the model a
non-zero visual_scale.

### Step 4: Add debug logging to `load_character_models()`

Print per-archetype diagnostic info: y_bottom, x_center, z_center, mesh count,
vertex count, and whether it loaded as skinned or static. This helps identify
which models have bad bounds.

### Step 5: Build, test, and iterate

1. `cargo build --release` (auto-restarts client)
2. Connect to testzone: the injected billboards appear as character models
3. Check stderr for model loading diagnostics
4. Use `/frame` API to capture screenshot and inspect rendering
5. Adjust archetype_scale values or y_bottom fix as needed

### Step 6: Fix any remaining rendering issues

Based on visual inspection:
- If models are lying flat: the Y-up to Z-up conversion may be wrong for static
  chr.s3d models (check `y_up=true` in entity_model_matrix_heading)
- If models are too small/large: adjust `archetype_scale()` values
- If models are offset: check x_center/z_center values
- If textures are wrong: check texture binding in entity pass

## Files to Modify

| File | Change |
|------|--------|
| `src/scene.rs` | Add `inject_test_billboards()` method |
| `src/app.rs` | Call inject method when zone == "testzone" |
| `src/models.rs` | Fix y_bottom for chr.s3d, add debug logging |
| `src/models.rs` | Review/update archetype_scale values if needed |

## Verification

- `cargo build --release` succeeds
- Client restarts and shows testzone with all character models standing upright
- Models have correct proportions (not smashed, not stretched)
- Models are grounded on the terrain plane
- `/frame` endpoint returns a PNG showing the models correctly
