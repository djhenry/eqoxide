# Hiding controller/utility NPCs from the target/spawn UI (issue #478)

## Answer: it's `bodytype`, NOT `race`

The wire-level signal the real RoF2 client uses (and that EQEmu's own server
independently re-checks as an anti-cheat backstop, implying the real client
never lets these be selected in the first place) is **`Spawn_Struct.bodytype`
being one of the "untargetable" body types**: `NoTarget = 11`,
`NoTarget2 = 60`, `Special = 67` (`EQEmu/common/bodytypes.h:37,64,67`, values
1/60/67). In practice, on the **wire**, only `11` and `60` are ever actually
observed — see the auto-rewrite below, which collapses everything `>=66`
(including 67) down to `11` before it's sent.

`race == 127` ("InvisibleMan") is a **correlated but unreliable** signal —
confirmed wrong as a sole gate by real content (see DB dump below: these
utility NPCs ship with `race = 240`, not 127).

## Evidence chain (server → wire → client)

1. **DB/content-authored fields**, `npc_types.bodytype` / `npc_types.race`
   (`EQEmu/common/repositories/base/base_npc_types_repository.h:45,323`
   `bodytype`; defaults `bodytype=1` at line 494).
2. **`Mob::bodytype`** is set straight from `npc_types.bodytype` at NPC
   construction and copied verbatim onto the wire struct with no
   transformation: `ns->spawn.bodytype = bodytype;`
   (`EQEmu/zone/mob.cpp:1345`). `Mob::GetBodyType()` returns the same field
   (`EQEmu/zone/mob.h:1117`).
3. **RoF2 wire-encode-time rewrite** (`EQEmu/common/patches/rof2.cpp:4629-4636`,
   inside `ENCODE(OP_ZoneSpawns)`, which `OP_ZoneEntry` and `OP_NewSpawn`
   both forward to — `rof2.cpp:4542,2358` — so this applies to **every**
   spawn-delivery path in RoF2):
   ```cpp
   bool ShowName = emu->show_name;
   if (emu->bodytype >= 66)
   {
       emu->race = 127;
       emu->bodytype = 11;
       emu->gender = 0;
       ShowName = 0;
   }
   ```
   Any NPC whose *real* `bodytype` is `>= 66` gets force-rewritten to
   `bodytype=11`/`race=127`/`gender=0`/`ShowName=0` **before** the packet is
   ever built. NPCs whose real `bodytype` is already exactly `11` (or `60`)
   are `< 66`, so this branch does **not** fire, and their *actual*
   content-authored `race` passes straight through unmodified.
4. `emu->bodytype` is then written verbatim onto the wire as a `uint32`
   (`rof2.cpp:4774`, `VARSTRUCT_ENCODE_TYPE(uint32, Buffer, emu->bodytype);`),
   documented field position in `rof2_structs.h:448`.
5. **Server-side anti-cheat backstop confirming this is the real client's
   own selection rule** — `EQEmu/zone/client_packet.cpp:15245-15258`
   (`OP_TargetCommand` handler): if a target request names a mob whose
   `GetBodyType()` is `NoTarget2(60)`, `Special(67)`, or `NoTarget(11)`, the
   server does **not** allow the target and logs a `POSSIBLE_HACK` event —
   i.e. the real client's own targeting/mouseover code is expected to never
   let the player select one of these in the first place; if a target
   request for one arrives anyway, only a modified client could have sent
   it.
6. `EQEmu/common/bodytypes.h` header itself documents the client-observed
   semantics directly in comments: `NoTarget = 11, // no name, can't target
   this bodytype` (line 37), `// body types above 64 make the mob invisible`
   (line 26), `InvisibleMan = 66, // no name, seen on 'InvisMan', can be
   /targeted` (line 65 — note 66 itself is nominally targetable per this
   comment, but any NPC actually shipping `bodytype==66` on RoF2 gets
   auto-rewritten to `bodytype=11` by step 3 above, so on the RoF2 wire this
   nuance is moot — it always arrives as 11).

## Verified against real content (local `peq` DB, `npc_types` table)

```
name                  race  bodytype  gender  untargetable  trackable  show_name
campday                240        11       2             0          0         1
campnight               240        11       2             0          0         1
mischief_controller     240        11       2             0          0         1
#cursed_controller      240        11       2             0          0         1
Swarmcontroller         240        11       2             0          0         1
spider_controller       127        11       0             0          0         1
#coirnav_controller     127        67       2             1          0         0
Drakeen_Controller       89        26       2             0          1         1   <- NOT hidden (real dragon-race NPC, just named "Controller")
```
(Queried via `podman exec eqemu_mariadb_1 mariadb -uagent -pagentpass peq`.)

Key findings from this data:
- **`campday`/`campnight`/`zone_controller`-class NPCs ship with `race=240`
  (`TeleportMan`, `EQEmu/common/races.h:284`), not 127.** Race 127 is only
  seen where content authors happened to also set it directly (e.g.
  `spider_controller`), or where the `bodytype>=66` auto-rewrite fires
  (`#coirnav_controller`, DB `bodytype=67`, ends up wire `race=127/bodytype=11`
  regardless of its DB race). **Race is not a reliable universal signal.**
- **`npc_types.untargetable` (DB column, default 0) is 0 for `campday` et al.**
  — so the separate `Spawn_Struct_Bitfields.targetable` wire bit (set from
  `Mob::IsTargetable()`/`!npc_type_data->untargetable`,
  `EQEmu/zone/npc.cpp:440`, `EQEmu/zone/mob.cpp:1317`,
  `rof2.cpp:4689 Bitfields->targetable = emu->NPC ? emu->untargetable : 1;`)
  would actually be **1 (targetable)** on the wire for these NPCs. **The
  `targetable` bitfield bit is NOT the mechanism that hides these mobs** —
  it's a separate, independently-set flag that happens to not be set for
  this content. Do not rely on it as the primary signal.
- `Drakeen_Controller` (bodytype 26 = Dragon, untargetable=0, trackable=1)
  is a **real, normally-targetable NPC** that merely has "Controller" in its
  name — proof that name-substring heuristics (`*controller*`, `camp*`) are
  unsafe and must not be used.
- `zone_controller` itself (hardcoded, not DB-driven):
  `race=240, bodytype=11, gender=2, untargetable=1, trackable=0`
  (`EQEmu/zone/npc.cpp:868-906`), spawned at position `(30000, 10000, -10000)`
  (`npc.cpp:911-913`) — matches the bug report's observed coordinates
  `(30000, 10000, -9999.875)` almost exactly (minor offset is a
  ground-snap/z-adjustment at spawn time, not a different NPC).

## Recommendation for eqoxide

- In the RoF2 `Spawn_Struct`/`NewSpawn_Struct` decoder, read the `bodytype`
  `uint32` field (`rof2_structs.h:448`, right after the properties-count
  byte and `race`/`showname` fields — see the decode-order note in
  `spawn-struct-race-equipment-branch.md`) and treat
  **`bodytype == 11 (NoTarget) || bodytype == 60 (NoTarget2)`** as the
  authoritative "this spawn is not a player-visible/targetable entity"
  signal. (`bodytype == 67 (Special)` never actually appears on the RoF2
  wire — it's always pre-collapsed to 11 by the server's `>=66` rewrite — so
  checking for 67 specifically is unnecessary, though harmless to include
  defensively for other client patches that may not apply the same rewrite.)
- Do **not** use `race == 127` as the check — real content (campday,
  campnight, zone_controller, mischief_controller, etc.) ships with
  `race == 240` on the wire, not 127. Race is a rendering/model-selection
  concern (both 127 `InvisibleMan` and 240 `TeleportMan` happen to map to
  no-geometry models), not the targeting-exclusion signal.
- Do **not** rely solely on the `Spawn_Struct_Bitfields.targetable` bit
  (RoF2 `rof2_structs.h:356`) — it can be `1` (targetable) even for these
  hidden utility NPCs, per the `campday` DB dump above (DB
  `npc_types.untargetable = 0` for that content).
- A coords-only heuristic (e.g. filtering `(0,0,0)`) is correctly identified
  by the caller as unsafe/too broad — `bodytype` is the precise, low-risk
  field to gate on instead. It's a single `uint32` already present in every
  decoded spawn record, so this is a cheap filter to add at parse time (or
  as a post-decode classification flag `is_untargetable_utility_npc =
  bodytype == 11 || bodytype == 60`) without needing coordinate heuristics.
- If eqoxide's `/observe/entities` (or the client's own target/mouseover
  code) needs to also match "Tracking skill list" exclusion, that's a
  **separate, unrelated** field: `npc_types.trackable`
  (`EQEmu/zone/mob.h:1366,1577`), delivered only via `OP_Track`
  (`rof2.cpp:4043` `ENCODE(OP_Track)`), not part of `Spawn_Struct` at all —
  don't conflate the two.
- Not independently confirmed in the decompiled `eqgame.exe` binary itself
  (it's stripped; a targeted grep for `bodytype`-driven logic only turned up
  an unrelated LDoN `ReqBodyType` adventure-requirement text-key parser at
  `eqgame.exe.c:642942`, and a `ReqBodyType::vftable` RTTI class name at
  `eqgame.exe.c:623693` — both belong to the LDoN task-requirement system,
  not spawn/target-list filtering). The client-side mechanism is inferred
  with high confidence from (a) EQEmu's own source comments documenting
  known live-client behavior per exact bodytype value
  (`common/bodytypes.h:26,37,65`), and (b) the server's anti-cheat backstop
  at `client_packet.cpp:15245-15258` explicitly checking the same three
  bodytype values and logging a hack-attempt if the (real, unmodified)
  client's own filtering is ever bypassed. If a fully client-confirmed
  citation is needed, the cheapest next step is a live capture: target
  `campday000` with the native Wine RoF2 client (see
  `eq-native-rof2-wine-client.md`) via `#targetid`/API and confirm the
  server logs the `POSSIBLE_HACK` event (proving the real client normally
  never sends that target request).

Related: `spawn-struct-race-equipment-branch.md` (same `Spawn_Struct` field
layout/decode-order, race-gated equipment block),
`spawn-struct-level-field.md` (same struct family, documents `zone_controller`
level/position).

## Addendum (agent-honesty review, 2026-07): `properties_count` is never 0 for these NPCs

Re-verified for a correctness review of a fix that filters on this field. Extra
findings, confirmed directly in source:

- `properties_count` (the `uint8` immediately before `bodytype` on the wire,
  `rof2_structs.h:448` docs it as position `/*0000*/` right after this byte) is
  **hardcoded to `1`** for every non-`DestructibleObject` spawn —
  `rof2.cpp:4771` `VARSTRUCT_ENCODE_TYPE(uint8, Buffer, 1);` — immediately
  followed by `rof2.cpp:4772`
  `VARSTRUCT_ENCODE_TYPE(uint32, Buffer, emu->bodytype);`. It is **not**
  conditional on the spawn's bodytype value or NPC-ness; it is only `0` in the
  `else` branch for `emu->DestructibleObject` spawns (`rof2.cpp:4766,4776`),
  which is a narrow, separate flag for breakable/interactive-object NPCs
  (`IsNPC() && IsDestructibleObject()`, `zone/mob.cpp:1408-1409`) — LDoN
  doors/breakables, not controller/utility mobs.
- `zone_controller` (`zone/npc.cpp:868-906`, `bodytype=11` at line 892) and
  DB-authored controller NPCs (`campday`, `campnight`, `mischief_controller`,
  etc.) are ordinary `Mob`-derived NPCs, never `DestructibleObject` — so they
  always take the `properties_count=1` branch and their real `bodytype` (11,
  already `< 66` so the `>=66` rewrite doesn't touch it) is written verbatim.
  **There is no wire path where a controller NPC's `bodytype` is absent or
  defaults to 0.**
- Anti-cheat backstop confirmed at **two** sites in the same handler
  (`zone/client_packet.cpp`, `Handle_OP_TargetCommand`), both checking all
  three values `{NoTarget=11, NoTarget2=60, Special=67}`:
  `client_packet.cpp:15100-15102` (gates `can_target` when acquiring a new
  target) and `client_packet.cpp:15245-15258` (logs `POSSIBLE_HACK` and clears
  an already-set target). `bodytypes.h` constant line numbers: `NoTarget=11`
  at line 37, `NoTarget2=60` at line 61, `InvisibleMan=66` at line 64,
  `Special=67` at line 65.
- Confirmed no legitimate spawn class can carry `bodytype` 11/60 on the RoF2
  wire:
  - **Player characters**: hardcoded `BodyType::Humanoid` at construction,
    `zone/client.cpp:88` and `:395` — never 11/60.
  - **Summoned pets**: `bodytype` comes from the pet spawn record, not copied
    from any source NPC; the one appearance-copy path that does borrow fields
    from another NPC (`monsterflag` "monster summoning" pets,
    `zone/pets.cpp:235-253`) copies `race/size/texture/gender/luclinface/
    helmtexture/herosforgemodel` only, explicitly not `bodytype`, and its
    candidate-NPC SQL pool already excludes `bodytype IN (11, 33, 66, 67)`
    (`zone/pets.cpp:222`).
  - **Charmed pets** keep the source NPC's real `bodytype`, so a charmed
    `bodytype=11/60` mob is theoretically possible in the data model, but not
    reachable through the real client: you cannot target such an NPC to cast
    Charm on it in the first place, per the same `client_packet.cpp:15100-15102`
    check.
  - No real/quest-facing targetable NPC content was found using 11/60; both
    the header comments (`bodytypes.h:37` "no name, can't target this
    bodytype") and every DB sample queried are hidden utility/controller mobs.

**Conclusion for the fix under review:** filtering RoF2 spawns on
`bodytype == 11 || bodytype == 60` is safe with no known false-positive class
(no dropped PC, no dropped real/targetable NPC, no dropped pet), and the
`properties_count` field can be assumed always-present-and-correct for any
spawn where `bodytype` matters (i.e. any non-`DestructibleObject` spawn) — no
additional guard for `properties_count == 0` is needed on this specific check.
