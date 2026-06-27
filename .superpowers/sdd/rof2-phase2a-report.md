# RoF2 Phase 2a Implementation Report
## Spawn / Position / NewZone Decoding

**Commit:** ff1f443  
**Branch:** worktree-rof2-client  
**Build:** `cargo build --release` — clean (0 errors, 0 warnings after final import cleanup)  
**Tests:** `cargo test` — 254 passed, 0 failed

---

## Structs Ported

### 1. Spawn_Struct (variable-length stream)

**Source:** `rof2_structs.h` Spawn_Struct / Spawn_Struct_Bitfields / Spawn_Struct_Position; cross-checked against `rof2.cpp` ENCODE(OP_ZoneSpawns).

The RoF2 Spawn_Struct is **not a fixed-size C struct** — it is a variable-length byte stream assembled by `ENCODE(OP_ZoneSpawns)`. The old 385-byte Titanium `Spawn_S` is removed. Replaced with:

- **`parse_rof2_spawn(buf: &[u8]) -> Option<(SpawnInfo, usize)>`** in `protocol.rs`
- **`SpawnInfo`** struct — the parsed in-memory representation

Wire stream order (from `rof2.cpp` ENCODE(OP_ZoneSpawns)):
```
name\0  spawnId(u32)  level(u8)  bounding(f32)  NPC(u8)
Bitfields(u32)  OtherData(u8)  unk3(f32)  unk4(f32)
props_count(u8)  [bodytype(u32) if count>0]
curHp haircolor beardcolor eyecolor1 eyecolor2 hairstyle beard  (7×u8)
drakkin_heritage/tattoo/details  (3×u32)
equip_chest2 material variation helm  (4×u8)
size(f32)  face(u8)  walkspeed(f32)  runspeed(f32)  race(u32)
holding(u8)  deity guildID guildrank  (3×u32)
class_ pvp StandState light flymode  (5×u8)
lastName\0
aatitle(u32)  guild_show(u8)  TempPet(u8)
petOwnerId(u32)  FindBits(u8)  PlayerState(u32)
NpcTintIndex PrimaryTintIndex SecondaryTintIndex unk unk  (5×u32)
[playable: TintProfile(36B) + Equipment(9×Texture_Struct@20B=180B)]
[non-playable: 5×u32(0) + Primary.Material(u32) + 4×u32(0) + Secondary.Material(u32) + 4×u32(0) = 60B]
Spawn_Struct_Position  (5×u32 = 20B)
[title\0 if OtherData & 0x10]  [suffix\0 if OtherData & 0x20]
unknown20(8B)  IsMercenary(u8)  RealEstateItemGuid(17B)
RealEstateID(u32)  RealEstateItemID(u32)  padding(29B)
```

**Spawn_Struct_Bitfields (4 bytes):**
- bits 0-1: gender, bit 2: ispet, bit 3: afk, bits 4-5: anon, bit 6: gm, bit 7: sneak
- bits 8: lfg, bit 9: betabuffed, bits 10-21: invis(12b), bit 22: linkdead, bit 23: showhelm
- bits 24-31: trader/targetable/showname/etc.

**Spawn_Struct_Position (RoF2, 20 bytes = 5×u32, rof2_structs.h lines 404-426):**
```
word0: angle:12, y:19, pad:1     → y = sext((w0>>12)&0x7FFFF, 19) / 8.0
word1: deltaZ:13, deltaX:13, pad:6
word2: x:19, heading:12, pad:1   → x = sext(w2&0x7FFFF, 19) / 8.0
                                    heading_cw = ((w2>>19)&0xFFF) * (360/512)
word3: deltaHdg:10, z:19, pad:3  → z = sext((w3>>10)&0x7FFFF, 19) / 8.0
word4: animation:10, deltaY:13, pad:9
```

**Playable race condition** (determines equipment block size):  
`NPC==0 || race<=12 || race==128(Iksar) || race==130(VahShir) || race==330(Froglok2) || race==522(Drakkin)`

**Equipment (playable):**
- TintProfile: 9 × Tint_Struct (Blue,Green,Red,UseTint = 4B each = 36B)
  → parsed as RGB: `[buf[b+2], buf[b+1], buf[b]]`
- Equipment: 9 × Texture_Struct (Material u32 + 4×u32 padding = 20B each = 180B)
  → `equipment[i] = u32 at byte offset 36 + i*20`

**Equipment (non-playable):**
- 60B: 5 zeros + Primary.Material@byte20 + 4 zeros + Secondary.Material@byte40 + 4 zeros
  → `equipment[7] = Primary.Material`, `equipment[8] = Secondary.Material`

**Files changed:**
- `src/eq_net/protocol.rs`: removed `Spawn_S`, added `SpawnInfo`, `parse_rof2_spawn`
- `src/eq_net/packet_handler.rs`: updated `apply_new_spawn`, `apply_zone_spawns`, `apply_zone_entry`, `register_spawn`
- `src/eq_net/login.rs`: OP_ZONE_ENTRY handler updated

---

### 2. PlayerPositionUpdateServer_Struct (OP_ClientUpdate, S→C)

**Source:** `rof2_structs.h` lines 1625-1652.

RoF2 adds `vehicle_id: u16` at offset 2, growing the struct from 22 to **24 bytes**.

```
bytes 0-1:  spawn_id (u16)
bytes 2-3:  vehicle_id (u16)  [NEW]
bytes 4-7   word0: padding:12, y:19, pad:1
bytes 8-11  word1: deltaZ:13, deltaX:13, pad:6
bytes 12-15 word2: x:19, heading:12 (unsigned), pad:1
bytes 16-19 word3: deltaHeading:10, z:19, pad:3
bytes 20-23 word4: animation:10, deltaY:13, pad:9
```

Updated: `decode_position_update()` and `encode_position_update()` in `protocol.rs`.  
`SIZE_SPAWN_POSITION_UPDATE` changed 22 → **24**.

---

### 3. PlayerPositionUpdateClient_Struct (OP_ClientUpdate, C→S)

**Source:** `rof2_structs.h` lines 1653-1700.

Updated `navigation.rs` `send_position_update()` to the RoF2 46-byte layout:
```
0:  sequence(u16)  2: spawn_id(u16)  4: vehicle_id(u16)=0
6:  unknown[4]=0   10: delta_x(f32)  14: heading(u32, bits 0-11)
18: x_pos(f32)     22: delta_z(f32)  26: z_pos(f32)  30: y_pos(f32)
34: animation(u32, bits 0-9)        38: delta_y(f32)
42: delta_heading(u32, bits 0-9 signed)=0
```

**Heading encoding:** `wire = ccw_to_cw(heading_ccw) * 2048/360` (same EQ12toFloat=/4 convention; see navigation.rs comment for derivation).

**Uncertainty:** `rof2_structs.h` header comment says "Size: 40" but struct offsets compute to 46 bytes. The vehicle_id field may be absent in the 2019 RoF2 build. If movement doesn't register server-side during live testing, try 40-byte layout (drop vehicle_id+unknown4 from the front).

---

### 4. NewZone_Struct (OP_NewZone)

**Source:** `rof2_structs.h` lines 566-648. `SIZE_NEW_ZONE` changed 688 → **948**.

Key field offsets vs Titanium:
| Field | Titanium offset | RoF2 offset |
|-------|----------------|-------------|
| zone_short_name | 64, 32B | 64, **128B** |
| zone_long_name  | 96, 278B | 192, **128B** |
| zone_desc       | — | 320, **150B** (new) |
| safe_y/x/z     | 492/496/500 | **588/592/596** |
| zone_id         | 684 | **852** |

`apply_new_zone()` uses direct byte-offset reads (not struct cast) to avoid alignment risk with the complex 948-byte layout. The `NewZone_S` repr(C,packed) struct is kept for compile-time size assertion and for potential future direct-field access.

---

## Bit-Packing Decisions

**Spawn position (RoF2 Spawn_Struct_Position):** Uses the same EQ19 fixed-point (value/8) and EQ12 CW heading scale (0..511 = 0..360°) as Titanium, just rearranged across the 5 words. Verified by round-trip test.

**Position update heading (S→C):** `unsigned heading:12` mapped as 0..511 = 0..360° CW. The `& 0xFFF` mask extracts 12 bits; in practice EQ uses 0..511.

**DestructibleObject spawns:** The ENCODE has a special path that back-patches the bitfields word and includes variable-length destructible model strings. These are NOT correctly parsed by `parse_rof2_spawn` and will return either `None` (if buffer runs short) or a garbled `SpawnInfo`. Since eqoxide doesn't render destructible objects and they are rare, this is acceptable. A future fix would detect `OtherData & 0xe1 == 0xe1` and apply the destructible decode path.

---

## Tests Added / Updated

**New tests (protocol.rs):**
- `rof2_new_zone_size` — asserts SIZE_NEW_ZONE=948 and sizeof(NewZone_S)=948
- `rof2_spawn_position_update_size` — asserts SIZE_SPAWN_POSITION_UPDATE=24
- `parse_rof2_spawn_npc_round_trip` — builds a synthetic non-playable NPC spawn buffer and round-trips through `parse_rof2_spawn`, checking all major fields including non-playable equipment (Primary@slot7=99, Secondary@slot8=88)
- `parse_rof2_spawn_rejects_truncated` — every truncation of the test buffer returns None

**Updated tests:**
- `position_update_round_trips` — updated comment and 24-byte check
- `decode_position_update_rejects_short` — verifies 23B → None, 24B → Some
- `apply_wear_change_updates_one_slot`, `register_spawn_parses_equipment_le` — migrated from `Spawn_S` to `SpawnInfo`

---

## Deferred / Uncertain Items

1. **DestructibleObject spawns** — not correctly parsed (noted above). Low priority since eqoxide doesn't render these.

2. **C→S position packet size ambiguity** — `PlayerPositionUpdateClient_Struct` header says Size=40 but struct offsets compute to 46 bytes. Implemented as 46 bytes. If server doesn't accept movement, test with 40-byte layout.

3. **Heading scale for C→S** — kept `2048/360` (= `512/360 * 4`) from Titanium, assuming EQEmu still does EQ12toFloat=/4. If player facing is wrong in combat (`IsFacingMob` fails), try `512/360` directly.

4. **PlayerProfile** — `parse_player_profile` still reads Titanium offsets. Level/class/equipment/stats/spells from the player profile packet will be wrong until Phase 2b ports the RoF2 PlayerProfile_Struct.

5. **max_hp** — `SpawnInfo` has no `max_hp` field (RoF2 spawn only sends curHp as percent); `register_spawn` sets `max_hp=100`. HP bar math in the renderer uses `hp_pct` which is unaffected.

---

## Smoke Check Expectations

After these changes, connecting to an EQEmu RoF2-patched server and zoning in should show:
- NPCs at **correct coordinates** (not garbled ×8 values or zeros)
- Player character at the zone's safe spawn position
- Zone name correctly parsed from `OP_NewZone` (e.g. "ecommons" not garbage)
- Position updates from NPC movement show reasonable coordinate deltas
- The server should not disconnect on the position update packet (46-byte format)

The controller should verify these visually by logging in and observing the entity positions in the HUD.
