# EQ RoF2 Protocol Notes

**This client speaks RoF2.** All struct sizes, offsets, and opcodes must be cross-checked against
the **RoF2** sources:

- EQEmu's [`utils/patches/patch_RoF2.conf`](https://github.com/EQEmu/Server/blob/master/utils/patches/patch_RoF2.conf) (opcode → name mapping)
- EQEmu's [`common/patches/rof2_structs.h`](https://github.com/EQEmu/Server/blob/master/common/patches/rof2_structs.h) (struct layouts)
- EQEmu's [`common/patches/rof2.cpp`](https://github.com/EQEmu/Server/blob/master/common/patches/rof2.cpp) (`ENCODE`/`DECODE` — where the emulator rewrites a packet between the `zone/` sender and the wire)

**Always verify against those three files** when adding new packet handling. Deriving a layout from
the *Titanium* patch (`patch_Titanium.conf`, `titanium_structs.h`, `titanium.cpp`) is the root cause
of **#889** and of every stale figure this document has had to retract. The previous revision of
this header mandated exactly that — it is the instruction that produced the bug, and correcting it
is **#954**.

### Three traps, all of which this repo has already hit

1. **A struct can exist in `rof2_structs.h` and still be dead.** `TaskActivity_Struct` at
   `common/patches/rof2_structs.h:4128-4153` sits inside an `#if 0`. Check the enclosing
   preprocessor state before citing a struct.
2. **A live `ENCODE` can contain a dead `#if 0 // original code` block.**
   `common/patches/rof2.cpp:3895-3922`, inside `ENCODE(OP_TaskDescription)`, is one. A `file:line`
   landing inside it describes code the compiler never sees. PR #944 shipped this mistake and
   corrected it in round 3.
3. **When an opcode has an `ENCODE`, the `zone/` serialiser's output is *not* the wire format.**
   Only an opcode with neither an `ENCODE` nor a `DECODE` in `common/patches/` lets you read the
   layout straight off the `zone/` sender (which is what licensed the `OP_TaskActivity` derivation
   in #944).

### Provenance convention

Every section below carries one **Provenance** line. It is not decorative — it is the difference
between a fact and an inherited guess, and it must be re-earned, per section, whenever the section
changes. The three values are:

- **RoF2 (re-derived)** — checked against the three RoF2 sources above and/or against shipped,
  test-pinned RoF2 code in this repo. The citation says which.
- **Titanium (superseded)** — known wrong for RoF2, kept only so a reader who finds the old number
  elsewhere can recognise it. Never use these values.
- **Version-independent** — the claim is about EQEmu `zone/` or `common/` behaviour that is not
  patch-specific, so the Titanium/RoF2 distinction does not apply.

Do **not** bulk-relabel a section to "RoF2" without doing the derivation. An unverified bullet
labelled RoF2 is strictly worse than one honestly labelled Titanium.

---

## Opcode Table — removed; see the code

**Provenance: Titanium (superseded).**

This document used to carry a 24-row zone opcode table, and RoF2 differs on every row — e.g.
`OP_NewZone` was listed as `0x0920` where RoF2's `OP_NEW_ZONE` is `0x1795`, and `OP_ClientUpdate` as
`0x14cb` where RoF2's `OP_CLIENT_UPDATE` is `0x7dfc`
(`crates/eqoxide-protocol/src/protocol/mod.rs`). The table has been deleted rather than
transcribed, because a second hand-maintained copy of the opcode map is exactly what let the
Titanium values sit here undetected while the client shipped the right ones.

It was in fact worse than "all Titanium". Checked row by row against `patch_Titanium.conf`, **23 of
the 24 values were genuine Titanium opcodes and one matched no shipped client at all**:
`OP_CHANNEL_MESSAGE` was listed as `0x4126`, where Titanium's `OP_ChannelMessage` is `0x1004` and
`0x4126` appears in no shipped patch config. So the table was not even reliably wrong in one
direction — the reason to delete it is that a hand-copied table has no mechanism that would ever
tell you which rows drifted. (An earlier revision of this section claimed every value *was* a
Titanium opcode; that overstated its consistency.)

**The single source of truth is `crates/eqoxide-protocol/src/protocol/mod.rs`**, and the
handshake-critical subset is pinned against `patch_RoF2.conf` by the test
`rof2_handshake_opcodes_match_conf` in that same file. When a packet type never fires, cross-check
`patch_RoF2.conf` — **not** `patch_Titanium.conf`.

Most constants also carry their RoF2 name in a trailing comment, but treat that as a convenience,
not a guarantee: a minority carry no comment or a non-RoF2 one (the login-stream opcodes near the
top are shared with SoD/Titanium and are annotated as such). The comments are unpinned prose; only
the values are pinned by the test above.

(The table's own "critical past bug" note — that `OP_SPECIAL_MESG` should be `0x2372` "from
`patch_Titanium.conf`" — is a fine illustration of the failure mode: the fix was real, and it fixed
the packet to the wrong client version's value. RoF2's `OP_SpecialMesg` is `0x0083`,
the `OP_SPECIAL_MESG` constant in `mod.rs`.)

---

## Position Update Format (bit-packed, 24 bytes)

**Provenance: RoF2 (re-derived)** — from `rof2_structs.h`'s
`PlayerPositionUpdateServer_Struct`, transcribed and pinned in
`decode_position_update` (`crates/eqoxide-protocol/src/protocol/mod.rs`) with
`SIZE_SPAWN_POSITION_UPDATE = 24` in the same file.

`OP_ClientUpdate` carries a bit-packed position struct. On RoF2 it is **24 bytes**, and both its
size and its field order differ from Titanium — there is a `vehicle_id` after `spawn_id`, and `x`
and `y` are in different words:

```
bytes[0..2]   spawn_id   (u16 LE)
bytes[2..4]   vehicle_id (u16 LE)
bytes[4..8]   word0: padding:12, y:19, pad:1
bytes[8..12]  word1: deltaZ:13, deltaX:13, pad:6
bytes[12..16] word2: x:19, heading:12, pad:1
bytes[16..20] word3: deltaHeading:10, z:19, pad:3
bytes[20..24] word4: animation:10, deltaY:13, pad:9
```

Coords are EQ19 fixed-point (wire value / 8). Heading is an unsigned 12-bit CW value on a
**2048-per-circle** scale — `heading_deg = wire_heading * 360 / 2048`. Decoded by
`eq12_server_to_deg_cw` (`raw * (360.0 / 2048.0)`) and encoded by `deg_cw_to_eq12_server`
(`* (2048.0 / 360.0)`), both in `crates/eqoxide-protocol/src/protocol/mod.rs`, and pinned by
`eq12_server_to_deg_cw_uses_2048_not_512_scale` and
`parse_rof2_spawn_heading_uses_2048_scale_not_aliased`.

> **Never apply a 360/512 scale to this wire field (#521).** A 512-per-circle scale is real, but it
> belongs *only* to the legacy Titanium decoder `s12_to_degrees_cw` (same file) and to EQEmu's
> internal `Mob::m_Position.w`, which `FloatToEQ12` converts before it ever reaches the wire.
> Nothing on the wire is 0..511. An earlier revision of this section said
> `heading_deg = heading_units * 360 / 512`; that was correct only while it carried a
> `/4.0 → 0-512 units` bridge that converted the wire value to EQ units first. Applied to the raw
> field the one-step form aliases: wire 1024 (due SOUTH) computes 720 → mod 360 = 0, due NORTH.

> **Superseded (Titanium):** this section previously documented a **22**-byte struct laid out
> `spawn_id | word1:x | word2:y | word3:z | word4:heading` with no `vehicle_id`. Do not use it; it
> is recorded only so the old numbers are recognisable. Its "not 30 — a prior bug that silently
> dropped all NPC movement" note described a real Titanium-era fix.

### Sending heading (critical past bug — broke all melee)

**Provenance: RoF2 (re-derived)** — the `2048/360` scale is live and shipped in
`deg_cw_to_eq12_server` and `deg_cw_to_eq12_client`
(`crates/eqoxide-protocol/src/protocol/mod.rs`).
When the client SENDS a position update, the wire heading must be `deg_cw * 2048/360`
(= `EQ_units * 4`, the same scale the decode paragraph above states), to match the server's decode
`EQ12toFloat = wire/4`. The client used to send
`deg_cw * 4096/360` — **exactly 2×** — so the server saw the player facing the wrong way. Movement
(x/y/z) and the local visual were unaffected, but **every melee swing silently missed** because
EQEmu gates swings on `IsFacingMob` (see Combat below). Fixed in `send_position_update`
(`crates/eqoxide-net/src/action_loop.rs`), which packs through the `2048.0/360.0` encoders cited
above. Internal heading is CCW (0=N, 90=W); `ccw_to_cw` converts before packing.

---

## Combat: the server only swings when you FACE the target

**Provenance: version-independent** — EQEmu `zone/` gameplay logic, not a patch-specific wire
layout, so the Titanium/RoF2 distinction does not apply. (The `OP_TargetMouse` opcode it depends on
*is* patch-specific; take it from `mod.rs`, never from the deleted table above.)

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

## Zone Crossing (OP_ZoneChange)

**Provenance: RoF2 (re-derived)** — size and offsets from the shipped `ZoneChange_Struct` builders
`build_zone_change` (`crates/eqoxide-protocol/src/protocol/mod.rs`, with
`SIZE_ZONE_CHANGE = 100` in the same file) and `send_zone_change_packet`
(`crates/eqoxide-net/src/action_loop.rs`), which annotates the full RoF2 field list.

RoF2 `ZoneChange_Struct` is **100 bytes**:

```
@0   char_name[64]
@64  zoneID (u16)          @66  instanceID (u16)
@68  Unknown068 (u32)      @72  Unknown072 (u32)
@76  y (f32)  @80  x (f32)  @84  z (f32)
@88  zone_reason (u32)     @92  success (i32)     @96  Unknown096 (u32)
```

RoF2 places the coordinate triple **8 bytes later** than Titanium (which had 88 bytes total and
y/x/z at @68/@72/@76). A Titanium-shaped packet lands the coords in RoF2's `Unknown068`/`Unknown072`
padding, so the server reads garbage coords and silently ignores the request.

**What the client puts in `zoneID` — twice-corrected, read this carefully:**

- **Titanium-era text, superseded:** this section used to say "`zoneID` = DESTINATION zone, not
  current". That was written to fix a real bug (the client sent its *current* zone id, so
  target==current and EQEmu cancelled/looped).
- **What the client actually sends today:** `zoneID = 0`, the *resolve-from-my-position* sentinel
  (eqoxide#199, `send_zone_change_packet`). On `zoneID==0` EQEmu's `Handle_OP_ZoneChange`
  (`zone/zoning.cpp`) routes to `GetClosestZonePointWithoutZone` — an XY-only, z-agnostic match —
  and derives the destination itself. Sending a *nonzero* destination instead routes to
  `GetClosestZonePoint`, whose z-bounded `InZoneLine` OBB test rejects a valid walk-in made with a
  stale tracked z and logs `MQZone … with Unknown Destination` (a false positive that could flag or
  kick on a strict server). `0` is a sentinel, not a zone id, so this is **not** a return to the
  original current-zone bug.

NOTE the `OP_SendZonepoints` coords the client receives are **arrival** coords, not in-zone
triggers.

---

## Merchant structs (buying)

**Provenance: RoF2 (re-derived)** — `MerchantClick_Struct` from the shipped builder
`merchant_click` (`crates/eqoxide-protocol/src/protocol/merchant.rs`), whose own comment
records the Titanium delta: Titanium was **16** bytes with no `tab_display`, and without
`tab_display` set the RoF2 server opens the window but sends **no** merchant inventory — so it must
be 1. `Merchant_Sell_Struct` and the `USE_NPC_RANGE2` gate are EQEmu `zone/` behaviour
(version-independent).

- `MerchantClick_Struct` (**24** bytes on RoF2, `OP_ShopRequest`): `npc_id`(u32 merchant entity id),
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

**Provenance: RoF2 (re-derived)** — all three opcodes and all three layouts. Opcodes are the RoF2
values from `utils/patches/patch_RoF2.conf:592-594`, matching
the `OP_TASK_DESCRIPTION`, `OP_TASK_ACTIVITY` and `OP_COMPLETED_TASKS` constants in
`crates/eqoxide-protocol/src/protocol/mod.rs`. (An earlier revision of this section listed
the *Titanium* opcodes — `patch_Titanium.conf:480-482` — and derived the layouts from
`titanium.cpp`. That mismatch is the root cause of #889.)

- `OP_TaskDescription` (0x3714): `Header{seq:u32, task_id:u32, open_window:u8, task_type:u32,
  reward_type:u32}` (17) + `title`(cstr) + `Data1{duration:u32, dur_code:u32, elapsed_time:u32}` (12) +
  `description`(cstr) + `Data2{has_rewards:u8, coin:u32, xp:u32, faction:u32}` (13) + `reward`(cstr) +
  `itemlink`(cstr) + `Trailer{points:u32, has_reward_selection:u8}` (**5**). Nothing follows the
  trailer: the ENCODE computes the output size to end exactly there, and the trailer copy is its
  last write.
  - **Why this one needed its own derivation (#949).** Unlike `OP_TaskActivity`, this opcode *does*
    have a RoF2 ENCODE (`common/patches/rof2.cpp:3846`), which recomputes the size at `:3875-3877`
    and reallocates at **`:3879`** — so the `zone/` serialiser's output is not the wire format and
    #889's method does not transfer. Beware that `:3895`–`:3922` of the same function is an
    `#if 0 // original code` block; the `new EQApplicationPacket(...)` at `:3899` is **dead** and is
    not the live reallocation.
  - **Result: the field order and sizes are identical to Titanium's.** Nothing is inserted, removed,
    resized or reordered. RoF2 changes only the *content* of two fields:
    - `Data1`'s third word is **elapsed time**, not an absolute `start_time` — the ENCODE overwrites
      the sender's timestamp with `now - start_time` in place. This document called it `start_time`
      for as long as it existed; that name was inherited from Titanium and is wrong for RoF2. The
      client discards the field, so nothing is broken today, but anything that starts reading it
      must not treat it as an absolute time.
    - `itemlink` is rewritten by the RoF2 say-link converter, so its bytes and length differ from
      what the `zone/` sender wrote.
  - **Trailer = 5 bytes (#955), settled by two independent methods.** Two
    `TaskDescriptionTrailer_Struct` definitions exist: a global 5-byte one
    (`common/eq_packet_structs.h:4702-4706`) and a 4-byte one in the RoF2 patch's nested struct
    namespace (`common/patches/rof2_structs.h:4249-4252`). Both are live (neither is inside an
    `#if 0`); both are `#pragma pack(1)`. The ENCODE sizes and `memcpy`s the trailer with an
    **unqualified** `sizeof(TaskDescriptionTrailer_Struct)` (`rof2.cpp:3876`, `:3890`), and its body
    is defined at the patch namespace's own scope, not inside the nested struct namespace — so
    unqualified lookup does not descend into the nested namespace and the name binds to the
    **global, 5-byte** definition. That was *not* left as a name-lookup argument: it was confirmed
    by compiling a probe reproducing the real namespace nesting (which printed 5 for the unqualified
    name, 5 for the global, 4 for the nested one), and independently corroborated by tracing the
    real client's own handler for this opcode, which reads a `u32` and then a `u8` immediately after
    `itemlink`'s NUL. Two methods, same answer.
  - **Does the client read it?** Yes — `parse_task_description` reads the trailer **all-or-nothing**
    (a 4-byte remainder is reported as a short packet, never as a `Points`-only trailer) and logs
    both a missing trailer and any bytes left after it. `Points` is not stored, because nothing
    downstream consumes it yet; it is deliberately *not* defaulted to 0, since a fabricated 0 would
    be indistinguishable from a genuine one. Size pinned both ways by
    `task_description_trailer_is_five_bytes_not_four`.
  - **Not verified for RoF2** — do not assert either of these from this document: the exact internal
    byte encoding the RoF2 say-link converter emits (only that it is a content-and-length
    transform), and whether any sender path can set `has_rewards` to 0 — the single call site found
    hardcodes 1. `has_reward_selection` is likewise hardcoded to 0 on the live send path, so a
    nonzero value there is **unmodeled, not impossible**.
- `OP_TaskActivity` (0x08d3): **two** legal wire shapes of different lengths. `grep -rn
  OP_TaskActivity common/patches/` finds no ENCODE and no DECODE (only a deprecated SoF opcode-list
  entry), so the emulator's serialiser output *is* the wire format, and
  `BasePacket(SerializeBuffer&&)` sets `size = m_pos` (`common/base_packet.cpp:36-42`) — the
  payload is exactly the bytes written, with no padding.
  - **Short form**, exactly 25 bytes — `TaskManager::SendTaskActivityShort`
    (`zone/task_manager.cpp:972-987`), sent for activities the player has not unlocked (they show
    as `???` in the client, its own comment at `:974`):
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
    `GiveCash` activities repurpose them (`common/tasks.h:143-150`): `goal_count` is a literal 1
    and `done_count` a 0/1 boolean; the cash amount is not on the wire.
- `OP_CompletedTasks` (0x4eba): `count:u32` then `count` records of
  `{task_id:u32, title:cstr, completed_time:u32}` (`zone/task_manager.cpp:946-966`) — full records,
  not a bare id list.

---

## Spawn_Struct (OP_NewSpawn)

**Provenance: RoF2 (re-derived)** — from `ENCODE(OP_ZoneSpawns)` in `common/patches/rof2.cpp` plus
`rof2_structs.h`'s `Spawn_Struct_Position`, transcribed in the shipped parser
`parse_rof2_spawn` (`crates/eqoxide-protocol/src/protocol/mod.rs`) and pinned by
`parse_rof2_spawn_npc_round_trip`, `parse_rof2_spawn_captures_flymode` and
`parse_rof2_spawn_rejects_truncated` in that file's test module.

**There is no fixed-offset table for RoF2, and there cannot be one.** RoF2's `Spawn_Struct` is
**variable-length and bit-packed**: NUL-terminated `name`/`lastName`/`title`/`suffix`, an equipment
block whose size depends on whether the race is playable (`TintProfile(36) + Equipment(180)` vs 60
bytes for NPCs), optional `title`/`suffix` gated on an `OtherData` bitmask, and a 20-byte
`Spawn_Struct_Position` of five bit-packed `u32`s. Do not index into it — read it with the parser,
which returns the consumed length so the next spawn in the packet can be found.

> **Superseded (wrong for every client):** this section used to give "total ~383 bytes" with
> `spawn_id`@0, `name`@4 and `is_npc`@0x115 (=277). Those are **not** Titanium's offsets — an
> earlier revision of this note said they were, which mislabelled them as merely out-of-date.
> Titanium's own `Spawn_Struct` (`common/patches/titanium_structs.h`) has `name`@7, `NPC`@83 and
> `spawnId`@340, total **385**. So the old numbers matched neither client and were never usable;
> they are recorded only so a reader who remembers them can recognise them.

**Still true on RoF2:** `curHp` in the spawn record is a **percentage** (0–100), not a raw HP value
— register the spawn with `hp_pct = cur_hp as f32` directly. Confirmed by `SpawnInfo::cur_hp`
(declared `u8` and commented "HP percent (100 = full)") and its read in `parse_rof2_spawn`, both in
`crates/eqoxide-protocol/src/protocol/mod.rs`, and pinned by `parse_rof2_spawn_npc_round_trip`. It lives in a 7×`u8` block (`curHp haircolor beardcolor eyecolor1 eyecolor2
hairstyle beard`), at no fixed offset.

---

## OP_ChannelMessage (Say channel)

**Provenance: RoF2 (re-derived)** — from `DECODE(OP_ChannelMessage)` in `common/patches/rof2.cpp`,
transcribed and pinned in `build_channel_message`
(`crates/eqoxide-protocol/src/protocol/chat.rs`) with the layout test
`build_say_packet_matches_rof2_layout` in the same file.

RoF2 uses a **variable-length, NUL-terminated** wire format — **not** a fixed `ChannelMessage_Struct`
with 64-byte name fields:

```
sender\0 | target\0 | u32 unknown | u32 language | u32 chan_num
| u32 unknown | u8 unknown | u32 skill_in_language | message\0
```

`chan_num = 8` is Say (the channel that triggers `EVENT_SAY` on NPCs within 200 units); EQEmu's
other values: 2 group, 3 shout, 5 OOC, 7 tell. `target` is the recipient for directed channels and
empty for broadcasts.

> **Superseded (Titanium), and actively harmful:** this section used to specify a fixed
> `148 + len(message) + 1` struct with `targetname`@0(64), `sender`@64(64), `language`@128,
> `chan_num`@132, `skill_in_language`@144. Sending that shape to a RoF2 server makes it read an
> empty target and a garbage `chan_num`, so **tells and OOC are silently dropped** with no
> cross-zone routing — a wrong-client-version layout that fails quietly rather than loudly.

---

## Consider_Struct (OP_Consider)

**Provenance: RoF2 (re-derived)** — request size from the live send path `build_consider_packet`
(`crates/eqoxide-protocol/src/protocol/combat.rs`), pinned by `build_consider_packet_layout`; reply
offsets from `apply_consider` (`crates/eqoxide-net/src/packet_handler.rs`) and pinned by
`apply_consider_parses_20byte_reply_and_logs_attitude`.

Request and reply are the **same 20-byte** RoF2 `Consider_Struct`: `playerid`@0, `targetid`@4,
`faction`@8, `level`@12, `pvpcon`@16 + 3 pad. The client fills only `playerid`/`targetid` and zeroes
the rest; the server ENCODEs its reply into the same shape, and `apply_consider` requires only the
first 16 bytes. RoF2 dropped Titanium's `cur_hp`/`max_hp`, which is why it is 20 and not 28.

> **Superseded (Titanium):** "client sends 28 bytes" — and the reply size was never stated at all.
>
> **Also retracted (this document's own error):** a previous revision of this section said the
> request is **32** bytes, sourced from a `SIZE_CONSIDER` constant. 32 was neither client's struct;
> the constant was dead code — zero call sites, so nothing ever contradicted it — and it has been
> deleted rather than corrected, precisely so there is no second copy of this number to drift again.
> The size that matters is the one `build_consider_packet` actually allocates. Getting it wrong is
> not a cosmetic error: EQEmu applies `DECODE_LENGTH_EXACT` here, so a wrong-sized request is
> **silently dropped with no reply at all** (#273) — the client cannot tell that from "the server
> has no answer", which is the agent-honesty failure mode this document exists to prevent.

The `ConsiderColor` enum (**version-independent**, EQEmu `common/`): `Ally=1, Warmly=2, Kindly=3,
Amiably=4, Indifferent=5, Apprehensive=6, Dubious=7, Threatning=8, DeathlyAfraid=9` (the
misspelling is EQEmu's).

---

## NPC Race → Model Archetype Mapping

**Provenance: version-independent** — race IDs are EQEmu `common/races.h`, shared by every client
patch.

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

The full mapping is `race_to_archetype()` in
`crates/eqoxide-renderer/src/models.rs`. If you see weird model substitutions (wolf showing as
bear, etc.), cross-check that function against `races.h`.

---

## GM Debug Spam Filter

**Provenance: version-independent** — EQEmu emits this as ordinary NPC chat text; no wire layout is
involved.

Only relevant when logged in as a **GM-flagged** account (the original observer mode; a non-GM
player character like "Claude" doesn't get this). EQEmu sends loot-table debug as NPC text:

```
[Loot] AddLootDrop: item_id=1234 min/max=1/1
```

These are filtered by `is_debug_spam()` in
`crates/eqoxide-net/src/packet_handler.rs` before being added to the NPC Dialogue panel. If
the filter is too aggressive, adjust it there.

---

## eqstr_us.txt

**Provenance: version-independent** — the string table is a shipped client data file, not a wire
struct. (The opcodes that carry a `string_id` are patch-specific; take them from `mod.rs`.)

`OP_FormattedMessage` carries a `string_id` + up to 9 `%1`..`%9` argument strings.
`crates/eqoxide-core/src/eqstr.rs` loads `assets/eqstr_us.txt` at startup into a process-global map
(the `eqstr::load` call in `src/main.rs`). `format_id(string_id, args)` resolves the template
(same `eqstr` module). If the file is missing the client still runs — system
messages just show "[string_id]" instead.
