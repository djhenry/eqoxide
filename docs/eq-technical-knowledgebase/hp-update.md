# OP_HPUpdate (RoF2)

## Wire struct (confirmed)

`SpawnHPUpdate_Struct` — `EQEmu/common/patches/rof2_structs.h:1679-1685`:

```c
struct SpawnHPUpdate_Struct
{
/*00*/ int16  spawn_id;
/*02*/ uint32 cur_hp;
/*06*/ int32  max_hp;
/*10*/
};
```

Total size: 10 bytes (matches the comment `Length: 10 Bytes` at rof2_structs.h:1676).

Field order on the wire is **spawn_id, cur_hp, max_hp** — confirmed by the ENCODE handler's
`OUT()` call order in `EQEmu/common/patches/rof2.cpp:1931-1940`:

```c
ENCODE(OP_HPUpdate)
{
    SETUP_DIRECT_ENCODE(SpawnHPUpdate_Struct, structs::SpawnHPUpdate_Struct);
    OUT(spawn_id);
    OUT(cur_hp);
    OUT(max_hp);
    FINISH_ENCODE();
}
```

`SETUP_DIRECT_ENCODE`/`OUT` write fields in declaration order directly into the wire struct, so
this is a 1:1 confirmation of the layout above (no padding, no reordering by the encoder).

There is also a `SpawnHPUpdate_Struct2` (rof2_structs.h:1713-1717, `int16 spawn_id` then
`uint8 hp`, 3 bytes) used for the percent-only NPC HP variant — not the same opcode struct as
OP_HPUpdate above (the one applicable here, 10-byte version, is what eqoxide reads).

## Bug found in eqoxide (src/eq_net/protocol.rs:1167-1173)

eqoxide's `HPUpdate_S` had the **same field widths but the WRONG field order**:

```rust
pub struct HPUpdate_S {
    pub cur_hp: u32,   // should be at offset 2, eqoxide puts it at offset 0
    pub max_hp: i32,   // should be at offset 6, eqoxide puts it at offset 4
    pub spawn_id: i16, // should be at offset 0, eqoxide puts it at offset 8
}
```

Total size happens to still be 10 bytes (so `SIZE_HP_UPDATE = 10` was never wrong), which is why
this bug was harder to spot than the OP_CastSpell (eqoxide#42) and OP_CombatDamage size-mismatch
bugs — only the byte offsets *within* the 10 bytes are shuffled, not the packet length.

Effect: every field is read from the wrong bytes. `spawn_id` (read from wire offset 8-9, which is
actually the high 16 bits of the real `max_hp`) will essentially never equal `gs.player_id`,
so `GameState::update_hp` (`src/game_state.rs:402`) usually falls into the `entities.get_mut`
branch (or matches nothing) instead of the player branch — explaining why the player's own
`max_hp` field looked "stuck" at its placeholder value while other numbers on screen (driven by
other opcodes/paths) looked plausible.

## Fix for eqoxide

```rust
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct HPUpdate_S {
    pub spawn_id: i16,
    pub cur_hp: u32,
    pub max_hp: i32,
}
```

`apply_hp_update` (src/eq_net/packet_handler.rs:400-405) needs no change beyond the struct
reorder — it already does `gs.update_hp(hp.spawn_id as u32, hp.cur_hp as i32, hp.max_hp)`, which
is correct once the bytes are read from the right offsets. `SIZE_HP_UPDATE` stays 10.

No EQG/S3D or sentinel edge cases apply here — this is a flat fixed-size struct, no variable
trailing data.

## Correction: OP_HPUpdate is SELF-ONLY, not "self+group" (see group-protocol.md)

protocol.rs:106 carries the comment `// RoF2: OP_HPUpdate (full cur/max, self+group only)`.
The "+group" half is **wrong** — verified against `EQEmu/zone/mob.cpp`:

- `Mob::SendHPUpdate()` (`zone/mob.cpp:1522-1549`) sends the full 10-byte `SpawnHPUpdate_Struct`
  via `OP_HPUpdate` **only to the client's own connection** (`if (IsClient()) { ... CastToClient()->QueuePacket(&p); }`,
  mob.cpp:1526-1542). It is never queued to group members.
- Everyone else (group members, anyone with this mob targeted, x-target trackers) instead gets
  `Mob::CreateHPPacket()` (`zone/mob.cpp:1487-1490`), which explicitly sets
  `app->SetOpcode(OP_MobHealth)` and uses the **3-byte** `SpawnHPUpdate_Struct2` (`spawn_id:int16,
  hp:uint8` percent) — a different app opcode entirely (`OP_MobHealth=0x37b1`,
  `EQEmu/utils/patches/patch_RoF2.conf:241`), and it passes through rof2.cpp unmodified (no
  ENCODE/DECODE override, so raw 3-byte struct on the wire).
- Group membership specifically unlocks `Group::SendHPPacketsFrom()`
  (`zone/groups.cpp:428-450`), which sends, for every *other* group member: the percent
  `OP_MobHealth` packet, plus (for SoD+/RoF2 clients) `OP_MobManaUpdate`
  (`MobManaUpdate_Struct`: `spawn_id:uint16, mana:uint8`, 3 bytes,
  `common/eq_packet_structs.h:1484-1489`) and `OP_MobEnduranceUpdate`
  (`MobEnduranceUpdate_Struct`: `spawn_id:uint16, endurance:uint8`, 3 bytes,
  `common/eq_packet_structs.h:1491-1496`), both also unmodified pass-through opcodes in rof2.cpp.

**Net effect:** a RoF2 client only ever receives full cur/max HP (`OP_HPUpdate`, 10 bytes) for
*itself*. For every other visible mob/player — including fellow group members — HP arrives as a
percent byte over `OP_MobHealth`, and group membership additionally unlocks percent mana/endurance
for other members via `OP_MobManaUpdate`/`OP_MobEnduranceUpdate`. There is no wire packet that
carries another player's exact HP number, group or not.

Recommendation for eqoxide: fix the protocol.rs:106 comment, and when building the group roster
window, drive member HP/mana/endurance bars off `OP_MobHealth` / `OP_MobManaUpdate` /
`OP_MobEnduranceUpdate` (percent, keyed by `spawn_id`) rather than expecting `OP_HPUpdate` for
group members. See `group-protocol.md` for the full group opcode/struct reference.
