# EQ Titanium Protocol Notes

These are hard-won facts about the Titanium wire protocol. All struct sizes,
offsets, and opcodes are cross-checked against:
- EQEmu's [`utils/patches/patch_Titanium.conf`](https://github.com/EQEmu/Server/blob/master/utils/patches/patch_Titanium.conf) (opcode → name mapping)
- EQEmu's [`common/patches/titanium_structs.h`](https://github.com/EQEmu/Server/blob/master/common/patches/titanium_structs.h) (struct layouts)

**Always verify against those files** when adding new packet handling.

---

## Opcode Table (zone server, Titanium)

| Constant | Value | Direction | Notes |
|----------|-------|-----------|-------|
| `OP_NEW_ZONE` | `0x0920` | server→client | Zone metadata on entry |
| `OP_NEW_SPAWN` | `0x1860` | server→client | `Spawn_Struct`, ~383 bytes |
| `OP_CLIENT_UPDATE` | `0x14cb` | both | 22-byte bit-packed position |
| `OP_DELETE_SPAWN` | `0x55bc` | server→client | 4-byte spawn_id |
| `OP_HP_UPDATE` | `0x3bcf` | server→client | cur_hp + max_hp |
| `OP_DEATH` | `0x6160` | server→client | entity death |
| `OP_SPAWN_APPEARANCE` | `0x7c32` | server→client | animation / HP% |
| `OP_EXP_UPDATE` | `0x5ecd` | server→client | XP change |
| `OP_LEVEL_UPDATE` | `0x6d44` | server→client | Level-up |
| `OP_PLAYER_PROFILE` | `0x75df` | server→client | Full profile struct |
| `OP_SEND_ZONE_POINTS` | `0x3eba` | server→client | Zone exit points |
| `OP_SPECIAL_MESG` | `0x2372` | server→client | NPC quest dialogue (SpecialMesg_Struct) |
| `OP_FORMATTED_MESSAGE` | `0x5a48` | server→client | System msg with string_id |
| `OP_SIMPLE_MESSAGE` | `0x673c` | server→client | Simple system msg |
| `OP_EMOTE` | `0x547a` | server→client | Emote/action text |
| `OP_CHANNEL_MESSAGE` | `0x4126` | client→server | Chat; chan_num=8 for Say |
| `OP_TARGET_COMMAND` | `0x1477` | client→server | Set target (alt) |
| `OP_TARGET_MOUSE` | `0x6c47` | client→server | **Set combat target** (4-byte spawn_id) — this is what `/v1/combat/target` uses; sets server-side `GetTarget()` for melee |
| `OP_CONSIDER` | `0x65ca` | both | Send=28-byte request; recv=con reply |
| `OP_AUTO_ATTACK` | `0x5e55` | client→server | 4 bytes; `[1,0,0,0]`=on, `[0,…]`=off |
| `OP_ZONE_CHANGE` | `0x5dd8` | client→server | Request zone crossing (88 bytes). **zoneID field = DESTINATION zone**, not current (see Zone Crossing below) |
| `OP_SHOP_REQUEST` | `0x45f9` | client→server | Open a merchant (MerchantClick_Struct, 24 bytes) |
| `OP_SHOP_PLAYER_BUY` | `0x221e` | client→server | Buy an item from a merchant slot (Merchant_Sell_Struct, 24 bytes) |
| `OP_SHOP_END` | `0x7e03` | client→server | Close merchant |

**Critical past bug**: `OP_SPECIAL_MESG` was `0x0fab` (wrong). The correct value
is `0x2372` from `patch_Titanium.conf`. Always cross-check there when a packet
type is never firing.

---

## Position Update Format (bit-packed, 22 bytes)

`OP_CLIENT_UPDATE` carries a Titanium-specific bit-packed position struct.
The struct size is **22 bytes** — not 30 (a prior bug that silently dropped all
NPC movement by failing the `len < 30` size guard).

See `src/eq_net/protocol.rs: decode_position_update()`:

```
bytes[0..2]   spawn_id (u16 LE)
bytes[2..6]   word1: bits[10..28] = x (19-bit signed fixed, /8.0)
bytes[6..10]  word2: bits[0..18]  = y (19-bit signed fixed, /8.0)
bytes[10..14] word3: bits[0..18]  = z (19-bit signed fixed, /8.0)
bytes[14..18] word4: bits[13..24] = heading (12-bit, /4.0 → 0-512 units)
```

Heading units: 512 = full circle. Convert: `heading_deg = heading_units * 360 / 512`.

### Sending heading (critical past bug — broke all melee)
When the client SENDS a position update, the wire heading must be `deg_cw * 2048/360`
(= `EQ_units * 4`), to match the server's decode `EQ12toFloat = wire/4`. The client used to send
`deg_cw * 4096/360` — **exactly 2×** — so the server saw the player facing the wrong way. Movement
(x/y/z) and the local visual were unaffected, but **every melee swing silently missed** because
EQEmu gates swings on `IsFacingMob` (see Combat below). Fixed in
`navigation.rs: send_position_update` (`2048.0/360.0`). Internal heading is CCW (0=N, 90=W);
`ccw_to_cw` converts before packing.

---

## Combat: the server only swings when you FACE the target

EQEmu gates a client's melee swing (`zone/client_process.cpp` ~line 398) on ALL of:
`may_use_attacks` (alive, not casting/mezzed/stunned, **has a target**) && `attack_timer.Check()` &&
`CombatRange(target)` && `CheckLosFN` (LOS) && **`IsFacingMob`**. `IsFacingMob` (`zone/mob.cpp`)
passes only when `|HeadingAngleToMob - GetHeading()| <= 80` EQ-units (~56°).

Implications for any client-driven combat:
- The combat **target** must be set server-side — send `OP_TARGET_MOUSE` (the client `/v1/combat/target` does).
- The player must **face** the target — send correct-scaled heading in position updates (see above).
  The nav `auto_attack` loop re-faces the target every tick for this reason.
- Must be in melee **range** (and LOS) — get adjacent (~5u) on the same floor level (mind z; a mob
  across water or below in a pit is out of 3D range).

---

## Zone Crossing (OP_ZONE_CHANGE) — send the destination zone id

`ZoneChange_Struct` (88 bytes): char_name[64] + **zoneID(u16) = DESTINATION zone** + instance(u16)
+ y(f32) + x(f32) + z(f32) + zone_reason(u32) + success(i32=0). Past bug: the client put the
*current* zone id in `zoneID`, but EQEmu (`zone/zoning.cpp`, `ZoneUnsolicited`) reads it as the
target → target==current → cancelled/looped. The server then finds the closest zone point matching
that target near the player's tracked position; its range check compares linear distance to a
*squared* max (effectively unlimited), so the player need not be at a precise trigger. NOTE the
`OP_SEND_ZONE_POINTS` coords the client receives are **arrival** coords, not in-zone triggers.

---

## Merchant structs (buying)

- `MerchantClick_Struct` (24 bytes, `OP_SHOP_REQUEST`): `npc_id`(u32 merchant entity id),
  `player_id`(u32), `command`(u32; 1=open, 0=close), `rate`(f32), `tab_display`(i32), `unknown020`(i32).
- `Merchant_Sell_Struct` (24 bytes, `OP_SHOP_PLAYER_BUY`): `npcid`(u32), `playerid`(u32),
  `itemslot`(u32 = `merchantlist.slot`), `unknown12`(u32), `quantity`(u32), `price`(u32; 0 lets the
  server charge its sell price). Must be within `USE_NPC_RANGE2`=40000 (=200u, **3D**) of the merchant.

---

## Task-system quest journal (server→client, the native quest log)

EQ's built-in quest journal (LDoN+, present in Titanium). Server-pushed for *task* quests only —
old-style Lua turn-in quests (Rat Whiskers, Gnoll Fangs) send NONE of these. Decoded in
`packet_handler.rs` into `GameState.tasks` (→ `GET /v1/quests/log`). All are **variable-length, packed**
(no struct padding).

Opcodes are the **RoF2** values from `utils/patches/patch_RoF2.conf:592-594`, matching
`crates/eqoxide-protocol/src/protocol/mod.rs:259-261`. (An earlier revision of this section listed
the *Titanium* opcodes — `patch_Titanium.conf:480-482` — and derived the layouts from
`titanium.cpp`. That mismatch is the root cause of #889; the `OP_TaskActivity` and
`OP_CompletedTasks` layouts below have been re-derived from the RoF2 send path, `OP_TaskDescription`
has not.)

- `OP_TaskDescription` (0x3714): `Header{seq:u32, task_id:u32, open_window:u8, task_type:u32,
  reward_type:u32}` (17) + `title`(cstr) + `Data1{duration:u32, dur_code:u32, start_time:u32}` (12) +
  `description`(cstr) + `Data2{has_rewards:u8, coin:u32, xp:u32, faction:u32}` (13) + `reward`(cstr) +
  `itemlink`(cstr) + `Trailer{points:u32, has_reward_selection:u8}` (5).
  **Not re-derived for RoF2** — unlike `OP_TaskActivity` this opcode *does* have a RoF2 ENCODE
  (`common/patches/rof2.cpp:3846`, reallocating at `:3899`), so the serialiser output is not the
  wire format and #889's derivation method does not transfer. Tracked as #949; treat the field list
  above as the shape `apply_task_description` currently reads, not as a verified RoF2 wire spec.
- `OP_TaskActivity` (0x08d3): **two** legal wire shapes of different lengths. `grep -rn
  OP_TaskActivity common/patches/` finds no ENCODE and no DECODE (only a deprecated SoF opcode-list
  entry), so the emulator's serialiser output *is* the wire format, and
  `BasePacket(SerializeBuffer&&)` sets `size = m_pos` (`common/base_packet.cpp:36-42`) — the
  payload is exactly the bytes written, with no padding.
  - **Short form**, exactly 25 bytes — `TaskManager::SendTaskActivityShort`
    (`zone/task_manager.cpp:972-987`), sent for activities the player has not unlocked (they show
    as `???` in the native client, its own comment at `:974`):
    `0 client_task_index:u32 | 4 task_type:u32 | 8 task_id:u32 | 12 activity_id:u32 |
    16 list_group:u32 | 20 0xffffffff:u32 (literal, :984) | 24 optional:u8`.
    It carries **no** activity type, target name or counts at all.
  - **Long form**, minimum 58 bytes — `TaskManager::SendTaskActivityLong`
    (`zone/task_manager.cpp:989-1014`) writes the same 5×u32 header and then delegates to
    `ActivityInformation::SerializeObjective` (`common/tasks.h:141-189`), RoF+ branch:
    `20 activity_type:i32 (signed — enum class TaskActivityType : int32_t, common/tasks.h:46) |
    24 optional:i8 (ONE byte on RoF+, common/tasks.h:154-158; the pre-RoF branch wrote i32) |
    25 request_type:i32 (dead, always 0) | 29 target_name:cstr | item_list:LenStr |
    goal_count:i32 | skill_list:LenStr | spell_list:LenStr | zones:cstr | dz_switch_id:i32 |
    description_override:cstr | done_count:i32 | 1:i8 (unknown constant) | zones:cstr (again,
    "seems unused", common/tasks.h:187)`.
  - `LenStr` is `SerializeBuffer::WriteLengthString` (`common/serialize_buffer.h:194-203`): a `u32`
    byte count followed by exactly that many raw bytes with **no NUL**. An empty one is the four
    bytes `00 00 00 00`. `cstr` is `WriteString` (`:174-181`), NUL-terminated.
  - The two forms are told apart by **both** the 25-byte length and the `0xffffffff` at offset 20;
    a payload on which those disagree is reported undecodable rather than decoded under a guess.
    A long form's offset-20 word comes from a `uint8_t` DB column
    (`common/repositories/base/base_task_activities_repository.h:43`, `:235`, cast at
    `zone/task_manager.cpp:216`), so it is always 0..=255 and can never be the sentinel.
  - `done_count`/`goal_count` are live objective progress and exist **only** in the long form.
    `GiveCash` activities repurpose them (`common/tasks.h:144-150`): `goal_count` is a literal 1
    and `done_count` a 0/1 boolean; the cash amount is not on the wire.
- `OP_CompletedTasks` (0x4eba): `count:u32` then `count` records of
  `{task_id:u32, title:cstr, completed_time:u32}` (`zone/task_manager.cpp:946-966`) — full records,
  not a bare id list.

---

## Spawn_Struct (OP_NEW_SPAWN)

Total ~383 bytes. Key fields:

| Field | Offset | Type | Notes |
|-------|--------|------|-------|
| spawn_id | 0 | u32 | |
| name | 4 | char[64] | null-terminated |
| is_npc | 0x115 | u8 | 1=NPC, 0=PC |
| curHp | — | u8 | **percent** (0-100), not raw HP |
| level | — | u8 | |
| race | — | u16 | EQEmu race ID |
| x/y/z/heading | — | f32 | raw floats |

**Critical**: `curHp` in `Spawn_Struct` is a **percentage** (0–100), not a raw
HP value. Register spawn with `hp_pct = cur_hp as f32` directly.

---

## ChannelMessage_Struct (Say channel)

`OP_CHANNEL_MESSAGE` with `chan_num = 8` sends text to the Say channel.
Total size: `148 + message.len() + 1` bytes.

| Offset | Field | Size |
|--------|-------|------|
| 0 | targetname | 64 bytes |
| 64 | sender | 64 bytes |
| 128 | language (u32) | 4 — 0 = CommonTongue |
| 132 | chan_num (u32) | 4 — 8 = ChatChannel_Say |
| 136 | cm_unknown4[0] (u32) | 4 |
| 140 | cm_unknown4[1] (u32) | 4 |
| 144 | skill_in_language (u32) | 4 — 100 |
| 148 | message | var + null |

---

## Consider_Struct (OP_CONSIDER)

Client sends 28 bytes; server replies with the same opcode carrying faction + level.

```
bytes[0..4]   playerid (u32 LE)
bytes[4..8]   targetid (u32 LE)
bytes[8..28]  zeroed
```

Server reply carries faction/con color at positions read by `apply_consider()`.
The `ConsiderColor` enum: `Ally=1, Warmly=2, Kindly=3, Amiably=4, Indifferent=5,
Apprehensive=6, Dubious=7, Threatning=8, DeathlyAfraid=9`.

---

## NPC Race → Model Archetype Mapping

Race IDs come from EQEmu's [`common/races.h`](https://github.com/EQEmu/Server/blob/master/common/races.h). Key mappings (past bugs fixed):

| Race ID | Name | Archetype |
|---------|------|-----------|
| 1 | Human | humanoid |
| 2 | Barbarian | humanoid |
| 60 | Skeleton | skeleton |
| 70 | Zombie | zombie |
| 42 | Wolf | wolf |
| 50 | Goblin | goblin |
| 75 | Spectre | spectre |

The full mapping is in `src/eq_net/protocol.rs: race_to_archetype()`. If you see
weird model substitutions (wolf showing as bear, etc.), cross-check that file
against `races.h`.

---

## GM Debug Spam Filter

Only relevant when logged in as a **GM-flagged** account (the original observer mode; a non-GM
player character like "Claude" doesn't get this). EQEmu sends loot-table debug as NPC text:

```
[Loot] AddLootDrop: item_id=1234 min/max=1/1
```

These are filtered by `is_debug_spam()` in `packet_handler.rs` before being
added to the NPC Dialogue panel. If the filter is too aggressive, adjust it there.

---

## eqstr_us.txt

`OP_FORMATTED_MESSAGE` carries a `string_id` + up to 9 `%1`..`%9` argument strings.
`src/eqstr.rs` loads `assets/eqstr_us.txt` at startup into a process-global map.
`format_id(string_id, args)` resolves the template. If the file is missing the
client still runs — system messages just show "[string_id]" instead.
