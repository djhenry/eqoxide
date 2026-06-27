# RoF2 Phase 2b Report — Spawn Registration + PlayerProfile

**Commit:** 385f026
**Status:** DONE

---

## What Was Fixed

### 1. `apply_zone_entry` — register ALL spawns (primary fix, `/entities` empty root cause)

**Root cause confirmed:** RoF2 server sends every spawn (NPCs, PCs, player) as a separate
`OP_ZoneEntry` (0x5089) packet containing one `Spawn_Struct`. EQEmu `rof2.cpp:4542`
`ENCODE(OP_ZoneEntry) { ENCODE_FORWARD(OP_ZoneSpawns); }` and `:4575/4660` emit a new
`EQApplicationPacket(OP_ZoneEntry, ...)` per entity. With ~151 zone spawns, that's ~151
`OP_ZoneEntry` packets, each carrying one mob/NPC/PC.

**Old code** in `apply_zone_entry` only updated `gs.player_*` if the spawn's name matched
`gs.player_name` — all other spawns (NPCs, other PCs) were silently dropped.

**Fix:** Replace the player-specific conditional with a single `register_spawn(gs, info)` call.
`register_spawn` already contains the player-self detection logic (name match → update
`gs.player_*` and return early; otherwise insert into `gs.entities`). No duplication, no
risk of double-registering the player as an NPC billboard.

**Spawns arriving during login phase:** The login loop in `login.rs` calls `apply_packet`
for side effects, and `apply_packet` dispatches `OP_ZONE_ENTRY` → `apply_zone_entry`. Since
`apply_zone_entry` now calls `register_spawn`, spawns received before "gameplay starts" are
correctly registered.

### 2. `OP_PLAYER_PROFILE` (0x6506) — ported to RoF2 wire offsets

**Old code** used Titanium offsets (`class_ @12, level @20, stats @2236, coin @4428,
mem_spells @4360`). RoF2 `OP_PlayerProfile` is serialized differently by EQEmu's
`ENCODE(OP_PlayerProfile)` (rof2.cpp:2499) using sequential `WriteUInt32/WriteUInt8/WriteFloat`
calls.

**Cited wire offsets** (from `rof2.cpp` ENCODE sequence + `rof2_structs.h` comments):

| Offset | Field | Source |
|--------|-------|--------|
| @16 | `gender` (u8) | rof2.cpp:2518 `WriteUInt8(emu->gender)` |
| @17 | `race` (u32) | rof2.cpp:2519 `WriteUInt32(emu->race)` |
| @21 | `class_` (u8) | rof2.cpp:2520 `WriteUInt8(emu->class_)` |
| @22 | `level` (u8) | rof2.cpp:2521 `WriteUInt8(emu->level)` |
| @184 | equipment[9] visual slots (Texture_Struct×9, 20B each, first u32 = Material) | rof2_structs.h:/*00184*/ Texture_Struct equip_helmet..equip_secondary |
| @808 | `tint_count` (u32)=9 | rof2.cpp:2579 |
| @812 | `item_tint[9]` (Tint_Struct×9, 4B each: Blue/Green/Red/UseTint) | rof2_structs.h:/*00812*/ TintProfile item_tint |
| @952 | `STR` (u32) | rof2_structs.h:/*00952*/ uint32 STR |
| @956–976 | STA, CHA, DEX, INT, AGI, WIS | rof2_structs.h |
| @9380 | `mem_spell_count` (u32)=16 | rof2_structs.h:/*09380*/ |
| @9384 | `mem_spells[16]` (int32 each) | rof2_structs.h:/*09384*/ int32 mem_spells[SPELL_GEM_COUNT=16] |
| @12869 | `platinum` (u32) | rof2_structs.h:/*12869*/ uint32 platinum |
| @12873–12881 | gold, silver, copper | rof2_structs.h |

**Note on position fields:** `y @14012, x @14016, z @14020, heading @14024` (rof2_structs.h)
come **after** variable-length bandolier/potionbelt sections (names serialized with `WriteString`
= strlen+1 bytes, no fixed padding before name). These cannot be read at fixed offsets. This is
NOT a problem: the player's actual zone-in position is carried in their `OP_ZoneEntry` spawn
packet, which is now correctly routed through `register_spawn`. Position from PlayerProfile is
redundant.

**Note on equipment:** The encode writes 9 visual Texture_Struct entries starting at @184
for slots 0–8 (helm/chest/arms/wrists/hands/legs/feet/primary/secondary), followed by 13 zero
entries for non-visual slots. Equipment2 (@628) and item_tint2 (@852) are mirrors; EQEmu
typically leaves item_material zeroed for characters relying on spawn packet + WearChange.

### 3. `OP_CLIENT_UPDATE` (0x7dfc) — confirmed working

Phase 2a already wired `OP_CLIENT_UPDATE → apply_position_update → decode_position_update`.
Code is correct: updates `gs.player_*` when `spawn_id == gs.player_id`, otherwise updates
`gs.entities[&spawn_id]`. No changes needed.

---

## Files Changed

**`src/eq_net/packet_handler.rs`** only:
- `apply_zone_entry`: simplified to call `register_spawn(gs, info)` for all spawns
- `parse_player_profile`: completely rewritten with RoF2 offsets; minimum size 980 bytes
- `apply_player_profile`: updated for RoF2 (gender/race/class/level @16-22, equipment @184,
  tint @812, delegates to `parse_player_profile` for stats/coin/mem_spells)
- Tests updated: `parse_player_profile_reads_offsets`, `player_profile_parses_equipment`
- Tests added: `zone_entry_registers_npc_spawn`, `zone_entry_updates_player_when_name_matches`,
  `player_profile_sets_class_and_race`

---

## Build + Test

```
cargo build --release: Finished (37s, zero warnings, zero errors)
cargo test: 257 passed, 0 failed, 18 ignored
```

---

## Assumptions / Uncertainties

1. **PlayerProfile position skipped:** Fields `y/x/z/heading` at rof2_structs.h @14012-14024
   are after variable-length bandolier/potionbelt data; they cannot be read at a fixed byte
   offset. Player position is set from `OP_ZoneEntry` spawn instead, which is correct.

2. **Equipment in PlayerProfile often zeroed:** EQEmu may leave `item_material` (@184) zeros
   and rely on `OP_ZoneEntry` spawn equipment + WearChange packets. The `apply_player_profile`
   code only overwrites `gs.player_equipment[i]` when `mat != 0` (same defensive behavior as
   before), so spawn-supplied equipment is preserved.

3. **Tint Slot[7] and Slot[8]:** rof2.cpp writes only 7 real tint values + 2 zeros (loop
   `r < 7`). Primary/Secondary weapon tints are always zero in the PlayerProfile. This is
   consistent with how EQ handles weapon appearance (IDFile-based, not tinted).

4. **`OP_ZoneSpawns` (0x5237):** The bulk handler `apply_zone_spawns` is still present and
   correct (loops through variable-length records). On the wire RoF2 does NOT send a bulk
   OP_ZoneSpawns; instead it sends ~151 individual OP_ZoneEntry packets. The bulk path is a
   no-op in practice but harmless.
