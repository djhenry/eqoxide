# RoF2 Combat Anim + Death Fix Report

## Bug 1 — OP_Animation: wrong byte offset for action (p[3] → p[2])

**Root cause**: `apply_animation` in `packet_handler.rs` read the animation action code
from byte offset 3 instead of 2. The comment above the function had the field order
reversed ("spawnid(u16) speed(u8) action(u8)") which caused the off-by-one.

**RoF2 Animation_Struct** (`rof2_structs.h` line 1454):
```
/*00*/  uint16 spawnid    bytes 0-1
/*02*/  uint8  action     byte 2  ← was being read from byte 3
/*03*/  uint8  speed      byte 3  ← was incorrectly used as action
```

**Confirmed by** `rof2.cpp` line 318 ENCODE(OP_Animation): `OUT(spawnid); OUT(action); OUT(speed);`

**Fix**: `src/eq_net/packet_handler.rs` — changed `let action = p[3]` to `let action = p[2]`
and updated the comment to show the correct field order.

**Effect**: Combat anim codes (1–9) were never in the speed byte (typically ≥ 10), so
no combat clips ever fired. Now `action = p[2]` correctly reads the swing type and
`gs.combat_anims.insert(spawnid, ...)` populates for the renderer.

---

## Bug 2 — OP_Death: animation not set to Lying (115) on kill

**Root cause**: `apply_death` correctly parsed the Death_Struct (spawn_id at bytes 0-3,
killer_id at bytes 4-7 — verified against both `eq_packet_structs.h` and `rof2_structs.h`;
no ENCODE for OP_Death in rof2.cpp so wire is server-native layout). It set `e.dead = true`
and `e.hp_pct = 0.0` but did NOT set `e.animation = 115` (Animation::Lying).

**RoF2 Death_Struct** — wire uses `eq_packet_structs.h` (no ENCODE in `rof2.cpp`, `rof.cpp`,
or any patch): spawn_id@0, killer_id@4, corpseid@8, bindzoneid@12, spell_id@16,
attack_skill@20, damage@24, unknown028@28. Note: `rof2_structs.h` has attack_skill@12 and
bindzoneid@20 (swapped vs wire), but only spawn_id and killer_id are used.

**Scene renderer** (`scene.rs`): maps `e.animation == 115` → `action = "dead"`. Without
setting animation=115, the entity was marked dead (greyed out) but still played the idle clip.

**Fix 1** (`packet_handler.rs` apply_death): added `e.animation = 115;` alongside
`e.dead = true` and `e.hp_pct = 0.0`.

**Fix 2** (`scene.rs` `from_game_state`): added `if e.dead { "dead" }` guard before the
animation match, so even if animation state is stale the dead clip is always used for dead
entities.

---

## Bonus fix — OP_BecomeCorpse: spawn_id read from wrong offset (4 → 0)

`apply_become_corpse` had a Titanium-era comment "unknown(4) + spawn_id(4)" and read
`spawn_id` from bytes 4-7. **RoF2 BecomeCorpse_Struct** (`rof2_structs.h` line 1558 /
`eq_packet_structs.h` line 1378) has NO 4-byte prefix:
```
uint32 spawn_id   bytes 0-3
float  y          bytes 4-7
float  x          bytes 8-11
float  z          bytes 12-15
```
Fixed to read from `payload[0..4]` and updated the length check to `< 4` (was `< 8`).

---

## Files Changed

- `src/eq_net/packet_handler.rs` — Bug 1 fix + Bug 2 fix + BecomeCorpse fix + 4 new tests
- `src/scene.rs` — dead-entity animation guard (small tweak)

## Build + Test

`cargo build --release`: clean  
`cargo test`: 261 passed, 0 failed, 18 ignored

## Uncertainties

None. All struct layouts verified against `rof2_structs.h`, `eq_packet_structs.h`, `rof2.cpp`
ENCODE(OP_Animation), and zone/attack.cpp Death/BecomeCorpse send paths.
