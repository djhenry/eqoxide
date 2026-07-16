# OP_Damage / CombatDamage_Struct (RoF2)

## Wire layout (confirmed)

`EQEmu/common/patches/rof2_structs.h:1511-1524`:

```
struct CombatDamage_Struct
{
/* 00 */ uint16 target;
/* 02 */ uint16 source;
/* 04 */ uint8  type;        // slashing, etc. 231 (0xE7) for spells
/* 05 */ uint32 spellid;     // widened from u16 (Titanium/SoF/SoD/RoF share u16 @5, damage @7)
/* 09 */ int32  damage;
/* 13 */ float  force;
/* 17 */ float  hit_heading;
/* 21 */ float  hit_pitch;
/* 25 */ uint8  secondary;   // 0=primary hand, 1=secondary
/* 26 */ uint32 special;     // 2=Rampage, 1=Wild Rampage
/* 30 */ (total size)
```

eqoxide's decode in `src/eq_net/packet_handler.rs:1506-1515` (`apply_combat_damage`) matches this
exactly: target@0(u16), source@2(u16), type@4(u8), spellid@5(u32), damage@9(i32), force@13(f32).
**Byte offsets were already correct** — confirmed against `rof2_structs.h`, not inferred.

RoF2's `ENCODE(OP_Damage)` (`EQEmu/common/patches/rof2.cpp:1123-1139`) does a straight `OUT(spellid)`
from the common/internal struct (`EQEmu/common/eq_packet_structs.h:1334-1345`, whose `spellid` field
is `uint16` @ offset 5) into the RoF2 wire struct's `uint32 spellid` @ offset 5. That's an implicit
**zero-extension**, not a reinterpretation — the internal engine never carries more than 16 bits of
spell id for this packet, so bits 16-31 of the wire field are always 0.

## Melee sentinel: 0x0000FFFF (65535), NOT 0 and NOT 0xFFFFFFFF

- `SPELL_UNKNOWN` is `0xFFFF` — `EQEmu/common/spdat.h:24`.
- `Mob::Damage(Mob* from, int64 damage, uint16 spell_id, ...)` takes `spell_id` as a **uint16**
  (`EQEmu/zone/mob.h:553-554`).
- Every pure-melee call site passes `SPELL_UNKNOWN` explicitly, e.g.
  `EQEmu/zone/attack.cpp:1755`, `:2395`, `:6528`, `:6577`
  (`other->Damage(this, my_hit.damage_done, SPELL_UNKNOWN, my_hit.skill, ...)`).
- The packet is built in `Mob::Damage` at `EQEmu/zone/attack.cpp:4497-4512`:
  `a->spellid = spell_id;` where `a` is `CombatDamage_Struct*` and `spell_id` is that uint16
  parameter (still `SPELL_UNKNOWN` = 0xFFFF for melee).
- Because the wire field is `uint32` but the source value is only ever a widened `uint16`, the
  **actual bytes on the wire for a melee swing are `FF FF 00 00` (LE) = 0x0000FFFF = 65535**, not
  `0xFFFFFFFF` (`0xFFFFFFFF` never appears in real RoF2 traffic from EQEmu — it would require the
  server to explicitly set the upper 16 bits, which no code path does) and not `0`.
- `IsValidSpell()` (`EQEmu/common/spdat.cpp:951-964`) explicitly excludes `spell_id < 2` and
  `spell_id == UINT32_MAX`, but does **not** special-case `0xFFFF`/`SPELL_UNKNOWN` — validity there
  is really gated by `spell_id < SPDAT_RECORDS && spells[spell_id].player_1[0]`, i.e. whether spell
  slot 65535 happens to be populated in the loaded spell DB. This is a server-side nuance, not
  something eqoxide needs to replicate — eqoxide should treat `spellid == 0` OR `spellid == 0xFFFF`
  (`SPELL_UNKNOWN`) as "no spell" (melee), and additionally guard on failed name lookup so an
  unresolvable spellid degrades to melee-style wording instead of the generic "casts a spell on"
  filler.

## Bug found (2026-07-16): eqoxide misclassifies every melee swing as a spell

`src/eq_net/packet_handler.rs:1523`:

```rust
let msg = if spellid != 0 {
    ...
    None => format!("{source_name} casts a spell on {target_name}"),
```

Since melee `spellid` on the wire is `0x0000FFFF` (65535), `spellid != 0` is **always true** for
melee hits. The spell name DB has no entry for id 65535, so `sname` is `None`, `damage` is
frequently `0`/negative-sentinel for non-caster mobs' regular swings, and the code falls into the
`None => "casts a spell on"` arm — producing exactly the observed bug (basic orc centurions/
legionnaires/pawns logged as "casts a spell on X" for ordinary melee).

The same sentinel also taints `beneficial_spell` at `packet_handler.rs:1562`
(`spellid != 0 && ... is_beneficial(spellid)`) — harmless there only because `is_beneficial(65535)`
resolves false (no such spell), but it's the same latent condition.

### Recommended fix

Treat both `0` and `SPELL_UNKNOWN` (`0xFFFF`) as "no spell":

```rust
const SPELL_UNKNOWN: u32 = 0xFFFF;
let is_spell = spellid != 0 && spellid != SPELL_UNKNOWN;
```

Apply this at both `packet_handler.rs:1523` (`apply_combat_damage`'s `msg` branch) and `:1562`
(`beneficial_spell`). Do **not** check `spellid != 0xFFFFFFFF` — that value never appears on RoF2's
actual wire for this struct, so a check against it alone would leave the `0xFFFF` case (the real
sentinel) unguarded, which is precisely today's bug.

## Related

- `EQEmu/zone/client_packet.cpp:5602-5609` — client→server `OP_Damage` handling (trap/other
  client-originated damage), uses the same `CombatDamage_Struct`.
- `EQEmu/zone/spell_effects.cpp:950,998,1037` and `EQEmu/zone/spells.cpp:4681` — spell-damage send
  sites; these set `cd->spellid` to the real cast spell id (non-sentinel, non-zero, < SPDAT_RECORDS)
  for legitimate "$Mob casts a spell" cases — so the fix does not affect real spell damage.
- Titanium/SoF/SoD/RoF (pre-RoF2) all use `uint16 spellid @5, uint32 damage @7` (23-28 byte struct);
  RoF2 alone widens `spellid` to `uint32` and shifts `damage` to `@9` (30-byte struct) — confirmed
  by diffing `titanium_structs.h:1140`, `sof_structs.h:1264`, `sod_structs.h:1264`,
  `rof_structs.h:1498`, `rof2_structs.h:1511`. Do not backport a Titanium-era `spellid@5(u16)` /
  `damage@7` assumption to RoF2 (this project's stated migration hazard).
