# RoF2 Spawn_Struct equipment block: the race-gated 60-byte vs 216-byte fork

## The finding (issue #407: guards/named NPCs missing from South Qeynos roster)

`ENCODE(OP_ZoneSpawns)` in `EQEmu/common/patches/rof2.cpp` does not emit a
fixed-size equipment/TextureProfile block for every NPC. It branches on
**race**, and the two branches differ by **156 bytes** (60 vs 216). A decoder
that assumes one fixed layout for all NPCs will silently misparse (fail to
fully consume) every record on the wrong side of the fork — this is the
single most likely structural cause of eqoxide issue #407.

### The branch condition (identical predicate used twice)

Size accounting (`EQEmu/common/patches/rof2.cpp:4639-4653`):
```cpp
float SpawnSize = emu->size;
if (!((emu->NPC == 0) || (emu->race <= Race::Gnome) || (emu->race == Race::Iksar) ||
        (emu->race == Race::VahShir) || (emu->race == Race::Froglok2) || (emu->race == Race::Drakkin))
    )
{
    PacketSize += 60;               // "compact" / monster+citizen-race branch
    if (emu->size == 0) { emu->size = 6; SpawnSize = 6; }
}
else
    PacketSize += 216;              // "full" / playable-race branch
```
Actual field write, same predicate, not negated (`rof2.cpp:4849-4891`):
```cpp
if ((emu->NPC == 0) || (emu->race <= Race::Gnome) || (emu->race == Race::Iksar) ||
        (emu->race == Race::VahShir) || (emu->race == Race::Froglok2) || (emu->race == Race::Drakkin)
    )
{
    // FULL branch, 216 bytes total:
    for (k = textureBegin; k < materialCount; ++k)   // 9 slots
        VARSTRUCT_ENCODE_TYPE(uint32, Buffer, emu->equipment_tint.Slot[k].Color);   // 9*4 = 36B
    // then 9x Texture_Struct{Material,Unknown1,EliteMaterial,HeroForgeModel,Material2}
    // = 9 * 5*uint32 = 9*20 = 180B  (rof2.cpp:4853-4870, Texture_Struct at rof2_structs.h:205-212)
    // 36 + 180 = 216, matches PacketSize += 216 exactly.
}
else
{
    // COMPACT branch, 60 bytes total = 15x uint32 (rof2.cpp:4872-4891):
    // 5 zero paddings, Primary.Material + 4 zero paddings,
    // Secondary.Material + 4 zero paddings  (weapon materials only, no armor slots)
}
```
- `Race::Gnome = 12` (`EQEmu/common/races.h:56`) — so races **1-12** (Human,
  Barbarian, Erudite, WoodElf, HighElf, DarkElf, HalfElf, Dwarf, Troll, Ogre,
  Halfling, Gnome) take the FULL 216-byte branch, same as any player
  (`emu->NPC==0`).
- `Race::Iksar=128`, `Race::VahShir=130`, `Race::Froglok2=330`,
  `Race::Drakkin=522` (`races.h:172,174,374,566`) also get the FULL branch.
- **Every other race id** — all ordinary monster races AND the NPC-only
  "citizen model" races (`FreeportGuard=44`, `QeynosCitizen=71`,
  `HighpassCitizen=67`, `NeriakCitizen=77`, `EruditeCitizen=78`,
  `HalasCitizen=90`, `GrobbCitizen=92`, `OggokCitizen=93`,
  `KaladimCitizen=94`, `RivervaleCitizen=81`, all `races.h:69-96`) — takes the
  COMPACT 60-byte branch (weapon-only materials, no per-armor-slot texture/tint
  data at all).
- `EQ::textures::materialCount = 9` (`armorHead`..`weaponSecondary`,
  `EQEmu/common/textures.h:25-37`); `Texture_Struct` is exactly 5 `uint32`s =
  20 bytes (`rof2_structs.h:205-212`). `9*4 (tints) + 9*20 (Texture_Struct) =
  216`, confirming the arithmetic.

### Verified against live-adjacent DB content (local `peq` DB snapshot, `podman exec eqemu_mariadb_1`)

Query against `npc_types`/`spawn2`/`spawnentry` for zone `qeynos` (short_name
`qeynos` = **South Qeynos**, confirmed via the `zone` table: id 1 =
"South Qeynos", id 2 `qeynos2` = "North Qeynos" — the reverse of what the
short names suggest):

| NPC (issue #407) | npc_types.id | race | class | bodytype | branch |
|---|---|---|---|---|---|
| Danaria Hollin | 1066 | 3 (Erudite) | 14 | 1 | **FULL (216B)** |
| Hansl Bigroon | 1096 | 1 (Human) | 1 | 1 | **FULL (216B)** |
| Menkes Tabolet | 1142 | 7 (HalfElf) | 9 | 1 | **FULL (216B)** |
| Lieutenant Arathur | 1085 | 71 (QeynosCitizen) | 3 | 1 | compact (60B) *(see caveat)* |
| Guard Phaeton | 1189 | 71 (QeynosCitizen) | 1 | 1 | compact (60B) *(see caveat)* |
| Caleah Herblender (works) | 1118 | 71 (QeynosCitizen) | 41 (Merchant) | 1 | compact (60B) |

3 of the 5 reported-missing NPCs (Danaria Hollin, Hansl Bigroon, Menkes
Tabolet) are unambiguously in the FULL/216-byte branch in this snapshot,
matching the failure pattern exactly (the working merchant Caleah Herblender
is race 71, compact branch). **This is very likely the #407 root cause**: the
"human-looking" city-clothes NPCs that are actually literal classic-race ids
(1-12) take the 216-byte branch; NPCs that only *look* human because they use
the `QeynosCitizen`(71)/`FreeportGuard`(44)/etc. NPC-only "citizen" body model
take the 60-byte branch and parse fine. **Do not assume race==1 for "looks
human" NPCs** — that was the trap in the original bug report premise.

**Caveat on Guard Phaeton / Lieutenant Arathur**: this container's `peq` DB is
a local dev snapshot; per `eq-infra-moved-to-jimbo.md` the live game+DB moved
to jimbo.lan and DB edits here no longer reach the live client, so *content*
here could also have drifted from what the live zone server actually ships.
If these two guards are *also* reported failing on the live server, the two
explanations are (a) they are actually race ≤12 on the live DB (unverified
here — would fully unify the bug with the other 3), or (b) a second,
independent bug. Because `race` (uint32) is written into the record BEFORE
the equipment block (`rof2.cpp:4799`, right after `runspeed`), **a decoder can
self-diagnose without DB access**: read the fixed prefix through `race`, then
branch on the observed race value exactly like the server does, instead of
guessing.

## Item-by-item (from the caller's question)

1. **Title/suffix**: `Mob::FillSpawnStruct` (`EQEmu/zone/mob.cpp:1280` on)
   never touches `ns->spawn.title`/`ns->spawn.suffix` for NPCs — those bytes
   stay whatever `memset(ns, 0, ...)` left them (zero) in both
   `Mob::CreateSpawnPacket` (`mob.cpp:1232-1236`, `memset` then
   `FillSpawnStruct`) and `BulkZoneSpawnPacket::AddSpawn`
   (`EQEmu/zone/entity.cpp:3494-3499`, zeroed array). Only
   `Client::FillSpawnStruct` sets them, from the PC's `PlayerProfile`:
   `strcpy(ns->spawn.title, m_pp.title); strcpy(ns->spawn.suffix, m_pp.suffix);`
   (`EQEmu/zone/client.cpp:2533-2534`). **NPCs never carry title/suffix on
   the wire; `OtherData` bits 0x10/0x20 are always 0 for every NPC in this
   report.** Guard "ranks" like "Lieutenant" are baked into `npc_types.name`
   itself (DB stores `Lieutenant_Arathur`; the underscore→space conversion is
   a client-side display convention, not a separate field).
2. **DestructibleObject / LDoNTreasure**: not set for any of the 6 sampled
   NPCs. `DestructibleObject` comes from `npc_types.special_abilities` id
   `34` (`EQEmu/common/emu_constants.h:558`, wired up via
   `EQEmu/zone/npc.cpp:1973-1974`); observed `special_abilities` values were
   `10,1^14,1` (guards: MagicalAttack+CharmImmunity), `21,1`, `24,1^35,1` —
   none contain `34`. `class_ == LDoNTreasure` (`EQEmu/common/classes.h:65`,
   id `62`) — observed classes were 1/3/7/9/14/41, none match. All 6 NPCs
   have `bodytype = 1` (Humanoid) in the DB. **This block does not apply to
   any NPC in this bug report.**
3. **Guild fields**: confirmed — `rof2.cpp:4804-4808`:
   ```cpp
   if (emu->NPC) {
       VARSTRUCT_ENCODE_TYPE(uint32, Buffer, 0xFFFFFFFF);
       VARSTRUCT_ENCODE_TYPE(uint32, Buffer, 0x00000000);
   } else {
       VARSTRUCT_ENCODE_TYPE(uint32, Buffer, emu->guildID);
       VARSTRUCT_ENCODE_TYPE(uint32, Buffer, emu->guildrank);
   }
   ```
   Both branches write exactly 2×`uint32` = 8 bytes — **guild status never
   changes record length**, only the two field *values*. All spawns in this
   report have `NPC==1` (never the player branch).
4. **`bodytype >= 66`**: applies only when true (`rof2.cpp:4631-4637`,
   forces `race=127, bodytype=11, gender=0, ShowName=0` before the size/branch
   logic runs). All 6 sampled NPCs have DB `bodytype=1` — **not triggered
   here**. Note this rewrite happens *before* the race-branch check, so if it
   ever fires it also forces the race the branch logic sees to `127` (>
   `Gnome`, not in the Iksar/VahShir/Froglok2/Drakkin set) → always routes
   through the compact 60-byte branch regardless of the NPC's real DB race.
5. **Nothing else found.** Given items 1-4 are all ruled out for this
   specific bug report, the confirmed remaining structural fork — and the one
   matching 3/5 of the reported failures exactly — is the race-gated
   60-vs-216-byte equipment block above.

## Recommendation for eqoxide

- Implement the exact branch: after reading the fixed prefix through `race`
  (name, spawnId, level, boundingRadius/meleeRange float, NPC byte,
  `Spawn_Struct_Bitfields` (4 bytes), `OtherData` byte, unknown3 float,
  unknown4 float, properties-count byte + bodytype u32 (or just a 0 byte if
  `DestructibleObject`), curHp/haircolor/beardcolor/eyecolor1/eyecolor2/
  hairstyle/beard (7 bytes), drakkin_heritage/tattoo/details (3×u32),
  equip_chest2/material/variation/helm (4 bytes), size (f32), face (u8),
  walkspeed (f32), runspeed (f32), **race (u32)** — `rof2.cpp:4663-4799` in
  order), branch on that `race` value using
  `race <= 12 || race == 128 || race == 130 || race == 330 || race == 522`:
  - true → read 9×`u32` tint colors + 9×20-byte `Texture_Struct` (216 bytes).
  - false → read 15×`u32` (60 bytes; only offsets 5 and 10, i.e. Primary and
    Secondary weapon Material, carry real data — rest are always 0 on live).
- Do **not** infer this branch from whether the NPC "looks human" in-game —
  many human-looking Qeynos/Freeport/Neriak/etc. citizen/guard NPCs use
  NPC-only "citizen model" race ids (`QeynosCitizen=71`, `FreeportGuard=44`,
  `NeriakCitizen=77`, `EruditeCitizen=78`, `HalasCitizen=90`,
  `KaladimCitizen=94`, `OggokCitizen=93`, `GrobbCitizen=92`,
  `RivervaleCitizen=81`, `HighpassCitizen=67`) that fall on the compact side,
  while literal classic-race NPCs (Human=1, Erudite=3, HalfElf=7, etc. — real
  values confirmed above for Danaria Hollin/Hansl Bigroon/Menkes Tabolet) fall
  on the full side.
- Title/suffix, guild, and DestructibleObject/LDoNTreasure branches are all
  correctly ruled out as causes for this specific bug (see items 1-3 above) —
  don't spend more time on them for #407 specifically, but do keep
  implementing them correctly for general correctness (title/suffix only
  applies to players; DestructibleObject/LDoNTreasure block is +53 fixed +3
  strings, `-4` for "no bodytype" when `DestructibleObject`, see
  `rof2.cpp:4619-4628,4694-4763`).
- To close the loop on Guard Phaeton/Lieutenant Arathur specifically: log the
  raw `race` u32 your decoder reads for every NPC that fails to fully
  consume, right after the fix above — if it's ≤12 or in {128,130,330,522},
  the live DB's race for those two differs from this local snapshot (71) and
  the same fix already covers it; if it's still failing at 60 bytes, that's a
  second bug and worth a fresh capture.

Related: `spawn-struct-level-field.md` (same `Spawn_Struct`/`NewSpawn_Struct`
wire family, `level` field semantics).
