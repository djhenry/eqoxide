# Spawn_Struct `level` field (RoF2) — can it legitimately be 0?

## Field location and type

`Spawn_Struct.level` is `uint8`, positioned right after the variable-length
`name[]` field and the 4-byte `spawnId`:

```
EQEmu/common/patches/rof2_structs.h:432   char     name[1];   // variable length, null-terminated
EQEmu/common/patches/rof2_structs.h:434   uint32 spawnId;
EQEmu/common/patches/rof2_structs.h:435   uint8  level;
```

Because `name` is variable-length (the struct comment at
`rof2_structs.h:427-431` explicitly says the struct is not used as a fixed
layout — the real packet is hand-assembled in `Server::SerializeSpawn`/
`Live.cpp`-style code), there is no single fixed byte offset; the `level`
byte's offset = `4 (name-including-nul) ... + 4 (spawnId) `. Decoders must
walk the variable name field first. This same struct underlies
`OP_ZoneSpawns` (array of `Spawn_Struct`), `OP_ZoneEntry`/`NewZone` handshake
spawn lists, and `OP_NewSpawn` (`NewSpawn_Struct` wraps one `Spawn_Struct`,
see comment block `rof2_structs.h:321-329`).

## Where the byte comes from (server side)

`Mob::FillSpawnStruct` copies the field straight from the `Mob::level` member,
no transform, no minimum clamp:

```
EQEmu/zone/mob.cpp:1301   ns->spawn.level = level;
```

`level` on a `Mob`/`NPC` is set from `NPCType::level` (DB `npc_types.level`,
data-driven) via the `NPC` constructor:

```
EQEmu/zone/npc.cpp:70    npc_type_data->level,   // passed straight into Mob ctor
```

`NPC::SetLevel` (npc.cpp:2253-2257) also just assigns `level = in_level;`
with **no floor/clamp to 1**. So nothing in the RoF2 server code path
prevents `level` from being 0 on the wire if the underlying `npc_types.level`
DB row is 0 — that would be a **content/data bug**, not a protocol violation,
and the resulting `Spawn_Struct` byte-for-byte would still be a completely
correctly-decoded packet with `level == 0`.

## Per-entity-type answers

- **Regular NPCs**: `level` = `npc_types.level` (DB-driven). Real production
  content always sets this ≥1 (levels are meaningful for con-color, xp,
  resists), but the code has **no server-side guard** against a
  misconfigured 0. Not something stock/expected content ships with.
- **Players (Client)**: character level starts at 1 at creation and the
  in-game level-up/reset paths never target 0 in normal play. No explicit
  `level==0` floor was found in `zone/exp.cpp`/`zone/client.cpp`, but this is
  a business-logic guarantee (chars are always created at level 1), not a
  wire-format one.
- **Corpses (PC or NPC)**: `Corpse` copies the mob's level at time of death
  (`EQEmu/zone/corpse.cpp:665`, `ce.level = level;`). Same caveat as NPCs —
  inherits whatever the source mob's level was.
- **Pets / swarm pets / familiars**: level is derived from the owning
  caster's level or spell-defined pet power (`EQEmu/zone/pets.cpp:131`,
  `npc_type->level += ...`), starting from a pet npc_type base level. In
  practice never 0 for real casters (casters must be level ≥1), but again no
  explicit floor is enforced in code.
- **`zone_controller` utility NPC**: explicitly set to **level 200** (not 0):
  ```
  EQEmu/zone/npc.cpp:874-886
    npc_type->level = 200;
  ```
  Note the `NPCType` is `memset(0, sizeof(NPCType))` first (npc.cpp:875), so
  every unmentioned field IS 0 (e.g. `deity`/`class_` are explicitly set to 1
  here, other numeric fields default to 0) — but `level` itself is explicitly
  overwritten to 200, confirming it is intentionally nonzero.
- **Merchants**: ordinary NPCs (npc_types-driven); no special-casing of
  `level`.
- **Ground spawns / tradeskill containers / world objects**: **NOT sent via
  `Spawn_Struct` at all.** They use a completely separate opcode/struct:
  ```
  EQEmu/utils/patches/patch_RoF2.conf:90   OP_GroundSpawn=0x6fca
  EQEmu/common/patches/rof2_structs.h:2926 struct Object_Struct { ... }
  EQEmu/common/patches/rof2_structs.h:3802 struct GroundSpawn{ ... }
  EQEmu/common/patches/rof2_structs.h:3814 struct GroundSpawns { ... }
  EQEmu/zone/object.cpp:492  app->SetOpcode(OP_GroundSpawn);
  ```
  These structs have no `level` field. Confirms: if you decoded a
  `Spawn_Struct` with `NPC==1` and `level==0` claiming to be a "ground spawn
  object," that would NOT match how RoF2 actually delivers objects — a real
  ground-spawn/tradeskill-container never arrives as a `Spawn_Struct` at all,
  so it can't be the source of an observed `level==0` Spawn_Struct.

## Verdict

**`level == 0` is not, by itself, a reliable signal of a malformed/garbage
spawn decode.** The RoF2 wire format places no constraint on the byte value —
it's a plain `uint8` copied verbatim from `Mob::level`/`npc_types.level` with
no server-side floor (`EQEmu/zone/mob.cpp:1301`, `EQEmu/zone/npc.cpp:2253-2257`).
A byte-for-byte correctly decoded packet CAN legitimately carry `level == 0`
if:
- the source NPC's `npc_types.level` DB row is misconfigured to 0 (a content
  bug, not a protocol bug) — confirmed possible in code, not observed as
  shipped content, and
- notably NOT for `zone_controller` (explicitly 200), NOT for ground
  spawns/tradeskill containers (they never carry a `level` field at all,
  wrong opcode/struct entirely).

For players, pets/swarm pets/familiars level 0 would require a caster/owner
of level 0, which cannot happen in normal play (chars always ≥1) — so in
practice, for those specific spawn kinds, `level == 0` IS a strong (though not
100%-airtight-by-protocol) malformed-decode signal. For plain NPCs and NPC
corpses, it is weaker evidence because DB data could (incorrectly) contain 0.

## Recommendation for eqoxide

- Do **not** use `level == 0` alone as a "drop this spawn, decode is garbage"
  gate for NPC-type spawns — it can be a legitimate (if rare/misconfigured)
  DB value and rejecting it would silently discard a real NPC (violates the
  agent-honesty principle: never silently drop a legitimately-decoded entity).
- If eqoxide wants a garbage-decode detector, combine `level == 0` with other
  jointly-implausible fields from the same struct (e.g. `race` out of any
  known race-ID range, `class_` out of 1-16/typical NPC class range,
  `bodytype` unreasonable, or a `name` that fails UTF-8/printable-ASCII
  validation) rather than trusting `level` in isolation.
- Explicitly special-case `zone_controller` (spawn id `ZONE_CONTROLLER_NPC_ID`,
  name `"zone_controller"`, level 200, bodytype 11, race 240) if eqoxide needs
  to recognize/hide the utility NPC — it is NOT a level-0 entity and should
  never be mistaken for a decode failure.
- Ground spawns/tradeskill containers/world objects must be parsed from
  `OP_GroundSpawn` (`GroundSpawn`/`GroundSpawns`/`Object_Struct`,
  `rof2_structs.h:2926,3802,3814`), never expected inside a `Spawn_Struct`
  array. If eqoxide's decoder is currently trying to interpret ground-spawn
  packets as `Spawn_Struct`s (which would produce nonsensical fields
  including `level`), that is the actual bug to fix — the opcode itself
  (`OP_GroundSpawn = 0x6fca`, `patch_RoF2.conf:90`) tells you it's the wrong
  code path before you even look at `level`.

Related: see `eqoxide` client's `NewSpawn_Struct`/`Spawn_Struct` decode path
(search for `spawnId`/`level` field parsing) to confirm it isn't
misclassifying `OP_GroundSpawn` payloads as spawns.
