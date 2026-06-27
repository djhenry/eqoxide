# RoF2 Hair/Face Client Implementation Report

**Status:** DONE  
**Branch:** worktree-fix+bug-fix

---

## What was implemented

### 1. Model loader — extras parsing (`src/models.rs`)

Added `HeadPart` enum:
```rust
pub enum HeadPart { Face(u8), Hair(u8) }
```

Added two parallel fields to `ModelAsset`:
- `head_parts: Vec<Option<HeadPart>>` — face/hair tag per mesh primitive; `None` = body/eye
- `head_default_hidden: Vec<bool>` — from `eq_default_hidden` in glTF extras

In the mesh primitive loop, after the existing `equip_slots.push()`, extras are parsed:
```rust
let head_tag: Option<(HeadPart, bool)> = primitive.extras().as_ref().and_then(|ex| {
    let v: serde_json::Value = serde_json::from_str(ex.get()).ok()?;
    let part_name  = v.get("eq_head_part")?.as_str()?;
    let part_index = v.get("eq_part_index")?.as_u64()? as u8;
    let dflt_hidden = v.get("eq_default_hidden").and_then(|h| h.as_bool()).unwrap_or(false);
    let part = match part_name {
        "face" => HeadPart::Face(part_index),
        "hair" => HeadPart::Hair(part_index),
        _ => return None,
    };
    Some((part, dflt_hidden))
});
```

`load_from_chr_s3d` initializes both vecs with `None`/`false` (chr.s3d models have no extras).

### 2. Visibility helper

```rust
pub fn head_part_visible(
    part: Option<HeadPart>, _default_hidden: bool, face: u8, hairstyle: u8,
) -> bool {
    match part {
        None => true,
        Some(HeadPart::Face(idx)) => idx == face.saturating_add(1),
        Some(HeadPart::Hair(idx)) => hairstyle > 0 && idx == hairstyle,
    }
}
```

`_default_hidden` is kept in the signature as specified. It is not needed at runtime because
`face=0, hairstyle=0` (the Entity defaults) already produce correct initial visibility — face 1
visible, all hair hidden — through the spawn-based matching logic.

### 3. Appearance plumbing

**`src/game_state.rs`:**
- Added `face: u8` and `hairstyle: u8` to `Entity`
- Added `player_face: u8` and `player_hairstyle: u8` to `GameState`

**`src/scene.rs`:**
- Added `face: u8` and `hairstyle: u8` to `Billboard`
- Added `player_face: u8` and `player_hairstyle: u8` to `SceneState`
- Propagated from `Entity` in `from_game_state`

**`src/gpu.rs`:**
- Added `head_parts: Vec<Option<HeadPart>>` and `head_default_hidden: Vec<bool>` to both
  `GpuStaticModel` and `GpuSkinnedModel`

**`src/renderer.rs`:**
- Both GPU upload paths (skinned and static) were converted from `unzip()` to explicit loops
  so head_parts/head_default_hidden are filtered in sync with mesh filtering

**`src/bin/render_model.rs`:**
- Same GPU upload conversion (static + skinned paths)

### 4. Spawn_Struct face/hairstyle offsets used

From `~/git/EQEmu/common/patches/rof2_structs.h`:

**Spawn_Struct** (variable-length wire format, rof2.cpp ENCODE order):
- `hairstyle` — byte 6 of the `curHp haircolor beardcolor eyecolor1 eyecolor2 hairstyle beard`
  block (offset relative to start of that 7-byte block: +5)
- `face` — 1 byte immediately after `size(f32)`, in the `size face walkspeed runspeed race` group

Previously both were `skip!()`-ed. Now both are `rd_u8!()` and stored in `SpawnInfo`:
```rust
// 12-18. curHp haircolor beardcolor eyecolor1 eyecolor2 hairstyle beard (7×u8)
let cur_hp = rd_u8!();
skip!(4); // haircolor beardcolor eyecolor1 eyecolor2
let hairstyle = rd_u8!();
skip!(1); // beard
...
// 27. face (u8)
let face = rd_u8!();
```

**PlayerProfile_Struct** fixed offsets:
- `hairstyle` — offset **00896**
- `face` — offset **00898**

Read in `apply_player_profile`:
```rust
if payload.len() >= 899 {
    gs.player_hairstyle = payload[896];
    gs.player_face      = payload[898];
}
```

### 5. Render pass changes (`src/pass.rs`)

Both `DrawCmd` structs (static `encode_entity_pass` and skinned `encode_skinned_entity_pass`)
gained `face: u8, hairstyle: u8`, populated from the `Billboard`.

`head_part_visible` is called in **four** render loops (player base + player overlay + entity
base + entity overlay):

```rust
if !crate::models::head_part_visible(
    model.head_parts[i], model.head_default_hidden[i],
    face, hairstyle,  // from billboard or scene.player_face/player_hairstyle
) { continue; }
```

---

## Files touched

| File | Change |
|------|--------|
| `src/models.rs` | `HeadPart` enum, `head_part_visible()`, extras parsing, new `ModelAsset` fields, unit tests |
| `src/gpu.rs` | `head_parts`/`head_default_hidden` in `GpuStaticModel` + `GpuSkinnedModel` |
| `src/renderer.rs` | GPU upload loops — pass through head-part data (refactored from unzip to for-loop) |
| `src/game_state.rs` | `face`/`hairstyle` on `Entity`; `player_face`/`player_hairstyle` on `GameState` |
| `src/scene.rs` | `face`/`hairstyle` on `Billboard`; `player_face`/`player_hairstyle` on `SceneState`; propagation |
| `src/eq_net/protocol.rs` | `face`/`hairstyle` on `SpawnInfo`; parse from wire |
| `src/eq_net/packet_handler.rs` | Propagate in `register_spawn` + `apply_player_profile`; fix test literals |
| `src/pass.rs` | `face`/`hairstyle` in both `DrawCmd` types; `head_part_visible` calls in 4 render loops |
| `src/hud.rs` | Test Billboard literals — added `face: 0, hairstyle: 0` |
| `src/bin/render_model.rs` | GPU upload loops refactored; new GpuModel fields |

---

## Build + test output

```
cargo build --release  →  Finished `release` profile [optimized] (39.78s, 0 errors)
cargo test             →  278 passed; 0 failed; 18 ignored
```

New unit tests added to `src/models.rs` (`head_part_visible` truth table):
- `head_part_visible_untagged_always_visible`
- `head_part_visible_correct_face_shows`
- `head_part_visible_wrong_faces_hidden`
- `head_part_visible_hairstyle_zero_hides_all_hair`
- `head_part_visible_hairstyle_n_shows_hair_n`
- `head_part_visible_default_hidden_flag_ignored_when_face_matches`

---

## Uncertainties / limitations

1. **Face textures not loaded** (out of scope, noted): the converter report flags that face
   primitive materials have `texture_idx: None` (libeq_wld's high-level API doesn't resolve
   the BitmapInfo chain for face materials). Faces will render in the model's base_color (solid
   color). The SELECTION logic implemented here is correct; the texture loading fix is a separate
   converter task.

2. **`_default_hidden` parameter unused**: the signature is kept as specified. At runtime the
   spawn defaults (face=0, hairstyle=0) already give correct initial visibility, making the flag
   redundant. If a future code path needs "render before spawn data" logic, `default_hidden` is
   in place.

3. **parse_rof2_spawn tests**: the existing test data at line ~1461 passes `0` for the hair/face
   bytes (inside the `b.extend_from_slice(&[0u8; 6]); // hair..beard` block). This means
   `hairstyle=0, face=0` in parsed spawns — correct defaults. The `parse_rof2_spawn_npc_round_trip`
   test checks `info.helm == 5` but does not assert face/hairstyle values; these remain at 0 (the
   test wire data has zeros there).
