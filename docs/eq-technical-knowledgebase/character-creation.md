# Character Creation — Titanium Wire Rules

All facts in this file are confirmed from EQEmu source unless noted.
Primary sources:
- `EQEmu/world/client.cpp` — `CheckCharCreateInfoTitanium` (lines 2002–2165), `OPCharCreate` (1677), `HandleNameApprovalPacket` (567)
- `EQEmu/common/patches/titanium_structs.h` — `CharCreate_Struct` (line 576), `NameGeneration_Struct` (563)
- `EQEmu/common/patches/titanium.cpp` — `DECODE(OP_CharacterCreate)` (line 2604)
- `EQEmu/common/eq_constants.h` — `StartZoneIndex` enum (line 974)
- `EQEmu/common/deity.h` — deity IDs (line 26)
- `EQEmu/common/races.cpp` — `RaceAppearance::IsValid*` (lines 1519–1992)
- `EQEmu/common/database.cpp` — `CheckNameFilter` (line 830)
- `EQEmu/utils/sql/svn/2024_required_update.sql` — `char_create_combinations` and `char_create_point_allocations` table data

---

## Wire Format (Titanium CharCreate_Struct)

80 bytes, 20 LE u32 fields. Source: `titanium_structs.h:576`.

```
offset  field
0000    class_        (1=Warrior .. 16=Berserker)
0004    haircolor
0008    beardcolor
0012    beard
0016    gender        (0=male, 1=female)
0020    race
0024    start_zone    (StartZoneIndex, 0–14, see table below)
0028    hairstyle
0032    deity
0036    STR
0040    STA
0044    AGI
0048    DEX
0052    WIS
0056    INT
0060    CHA
0064    face
0068    eyecolor1
0072    eyecolor2
0076    tutorial      (0=no, 1=go to tutorial zone)
```

Note: the struct comment says "Length: 140 bytes" — that is wrong. Field offsets confirm 80 bytes, 20 u32.
Note: `haircolor` is at offset 4 and `hairstyle` at offset 28 — the in-code comments say "Might be swapped" but the titanium.cpp DECODE (`IN(haircolor)`, `IN(hairstyle)`) maps them directly by name.
Drakkin fields (`drakkin_heritage`, `drakkin_tattoo`, `drakkin_details`) are NOT in the Titanium wire format; titanium.cpp does not map them.

---

## Race IDs (Titanium playable)

| Race        | race ID | racetemp (internal) |
|-------------|---------|---------------------|
| Human       | 1       | 0                   |
| Barbarian   | 2       | 1                   |
| Erudite     | 3       | 2                   |
| Wood Elf    | 4       | 3                   |
| High Elf    | 5       | 4                   |
| Dark Elf    | 6       | 5                   |
| Half Elf    | 7       | 6                   |
| Dwarf       | 8       | 7                   |
| Troll       | 9       | 8                   |
| Ogre        | 10      | 9                   |
| Halfling    | 11      | 10                  |
| Gnome       | 12      | 11                  |
| Iksar       | 128     | 12                  |
| Vah Shir    | 130     | 13                  |

Froglok (ID=330, racetemp=14) and Drakkin (ID=522, racetemp=15) are in the validation table but require post-Titanium expansions. Do NOT offer them in the Titanium UI.

---

## Class IDs

1=Warrior, 2=Cleric, 3=Paladin, 4=Ranger, 5=ShadowKnight, 6=Druid, 7=Monk, 8=Bard, 9=Rogue, 10=Shaman, 11=Necromancer, 12=Wizard, 13=Magician, 14=Enchanter, 15=Beastlord, 16=Berserker

---

## 1. Race/Class Validity Matrix

Confirmed from `CheckCharCreateInfoTitanium`, `ClassRaceLookupTable`, `client.cpp:2053`.
`1`=valid, `0`=invalid.

```
              Hum Bar Eru  WE  HE  DE HlfE Dwr Trl Ogr Hlf Gno Iks VaS
Warrior  (1)   1   1   0   1   0   1   1   1   1   1   1   1   1   1
Cleric   (2)   1   0   1   0   1   1   1   1   0   0   1   1   0   0
Paladin  (3)   1   0   1   0   1   0   1   1   0   0   1   1   0   0
Ranger   (4)   1   0   0   1   0   0   1   0   0   0   1   0   0   0
SK       (5)   1   0   1   0   0   1   0   0   1   1   0   1   1   0
Druid    (6)   1   0   0   1   0   0   1   0   0   0   1   0   0   0
Monk     (7)   1   0   0   0   0   0   0   0   0   0   0   0   1   0
Bard     (8)   1   0   0   1   0   0   1   0   0   0   0   0   0   1
Rogue    (9)   1   1   0   1   0   1   1   1   0   0   1   1   0   1
Shaman  (10)   0   1   0   0   0   0   0   0   1   1   0   0   1   1
Necro   (11)   1   0   1   0   0   1   0   0   0   0   0   1   1   0
Wizard  (12)   1   0   1   0   1   1   0   0   0   0   0   1   0   0
Mage    (13)   1   0   1   0   1   1   0   0   0   0   0   1   0   0
Enchant (14)   1   0   1   0   1   1   0   0   0   0   0   1   0   0
Beastlord(15)  0   1   0   0   0   0   0   0   1   1   0   0   1   1
Berserker(16)  0   1   0   0   0   0   0   1   1   1   0   0   0   1
```

---

## 2. Stat Tables

### Race Base Stats (BaseRace table, `client.cpp:2013`)

Order: STR, STA, AGI, DEX, WIS, INT, CHA

| Race        | STR | STA | AGI | DEX | WIS | INT | CHA |
|-------------|-----|-----|-----|-----|-----|-----|-----|
| Human       |  75 |  75 |  75 |  75 |  75 |  75 |  75 |
| Barbarian   | 103 |  95 |  82 |  70 |  70 |  60 |  55 |
| Erudite     |  60 |  70 |  70 |  70 |  83 | 107 |  70 |
| Wood Elf    |  65 |  65 |  95 |  80 |  80 |  75 |  75 |
| High Elf    |  55 |  65 |  85 |  70 |  95 |  92 |  80 |
| Dark Elf    |  60 |  65 |  90 |  75 |  83 |  99 |  60 |
| Half Elf    |  70 |  70 |  90 |  85 |  60 |  75 |  75 |
| Dwarf       |  90 |  90 |  70 |  90 |  83 |  60 |  45 |
| Troll       | 108 | 109 |  83 |  75 |  60 |  52 |  40 |
| Ogre        | 130 | 122 |  70 |  70 |  67 |  60 |  37 |
| Halfling    |  70 |  75 |  95 |  90 |  80 |  67 |  50 |
| Gnome       |  60 |  70 |  85 |  85 |  67 |  98 |  60 |
| Iksar       |  70 |  70 |  90 |  85 |  80 |  75 |  55 |
| Vah Shir    |  90 |  75 |  90 |  70 |  70 |  65 |  65 |

### Class Stat Bonuses and Bonus Points (BaseClass table, `client.cpp:2033`)

Order: STR, STA, AGI, DEX, WIS, INT, CHA, POINTS

| Class        | STR | STA | AGI | DEX | WIS | INT | CHA | POINTS |
|--------------|-----|-----|-----|-----|-----|-----|-----|--------|
| Warrior      |  10 |  10 |   5 |   0 |   0 |   0 |   0 |  25    |
| Cleric       |   5 |   5 |   0 |   0 |  10 |   0 |   0 |  30    |
| Paladin      |  10 |   5 |   0 |   0 |   5 |   0 |  10 |  20    |
| Ranger       |   5 |  10 |  10 |   0 |   5 |   0 |   0 |  20    |
| ShadowKnight |  10 |   5 |   0 |   0 |   0 |  10 |   5 |  20    |
| Druid        |   0 |  10 |   0 |   0 |  10 |   0 |   0 |  30    |
| Monk         |   5 |   5 |  10 |  10 |   0 |   0 |   0 |  20    |
| Bard         |   5 |   0 |   0 |  10 |   0 |   0 |  10 |  25    |
| Rogue        |   0 |   0 |  10 |  10 |   0 |   0 |   0 |  30    |
| Shaman       |   0 |   5 |   0 |   0 |  10 |   0 |   5 |  30    |
| Necromancer  |   0 |   0 |   0 |  10 |   0 |  10 |   0 |  30    |
| Wizard       |   0 |  10 |   0 |   0 |   0 |  10 |   0 |  30    |
| Magician     |   0 |  10 |   0 |   0 |   0 |  10 |   0 |  30    |
| Enchanter    |   0 |   0 |   0 |   0 |   0 |  10 |  10 |  30    |
| Beastlord    |   0 |  10 |   5 |   0 |  10 |   0 |   5 |  20    |
| Berserker    |  10 |   5 |   0 |  10 |   0 |   0 |   0 |  25    |

### Validation Algorithm (`client.cpp:2104–2164`)

```
bSTR = BaseRace[race][0] + BaseClass[class][0]
... (same for all 7 stats)
stat_points = BaseClass[class][7]
bTOTAL = sum(bSTR..bCHA)
cTOTAL = sum(cc->STR..cc->CHA)

REQUIRED: cTOTAL == bTOTAL + stat_points  (EXACT — must spend ALL points)
REQUIRED: each stat cc->S >= bS  (can't go below base)
REQUIRED: each stat cc->S <= bS + stat_points  (can't put more than all points in one stat)
```

The stat floor per stat is the base value; there is no ceiling other than base+stat_points.
The stat total is an exact check — not >=, but ==. Any unspent points will fail.

### Example: Dark Elf Shadow Knight
- BaseRace[DE] = {60,65,90,75,83,99,60}
- BaseClass[SK] = {10,5,0,0,0,10,5,20}
- base = {70,70,90,75,83,109,65} = 562 total; POINTS=20
- Must send total = 582, allocating 20 across any stats

---

## 3. Gender Rules

`gender` field: 0=male, 1=female, 2=neuter (NPC only, not a valid PC choice).

All 14 Titanium playable races have both male and female models. The server does not restrict gender by race for Titanium. The Luclin expansion added female Troll and Ogre player models; both are valid in Titanium.

Source: `races.cpp` has `TrollFemale`, `OgreFemale` cases throughout the appearance functions.

---

## 4. Deity Selection

**Server validation**: Deity is NOT validated server-side for Titanium clients. The `CheckCharCreateInfoTitanium` function has a comment: "TODO: Check for deity/class/race.. it'd be nice, but probably of any real use to hack" (`client.cpp:2159`). Any valid deity ID in `deity.h` is accepted.

### Deity IDs (`deity.h:26`)

| Deity           | ID  | Alignment |
|-----------------|-----|-----------|
| Agnostic        | 140 | neutral   |
| Bertoxxulous    | 201 | evil      |
| Brell Serilis   | 202 | neutral   |
| Cazic-Thule     | 203 | evil      |
| Erollisi Marr   | 204 | good      |
| Bristlebane     | 205 | neutral   |
| Innoruuk        | 206 | evil      |
| Karana          | 207 | good      |
| Mithaniel Marr  | 208 | good      |
| Prexus          | 209 | good      |
| Quellious       | 210 | good      |
| Rallos Zek      | 211 | evil      |
| Rodcet Nife     | 212 | good      |
| Solusek Ro      | 213 | neutral   |
| The Tribunal    | 214 | neutral   |
| Tunare          | 215 | good      |
| Veeshan         | 216 | good      |
| Agnostic (alt)  | 396 | neutral   |

Use 396 for the "Agnostic" selection (it's the newer ID; both 140 and 396 map to "Agnostic" in `deity.h`). The `char_create_combinations` data uses 396 for agnostic choices.

### Class-by-class deity lists (from `char_create_combinations` expansions_req=0)

These are the deities the native UI presents (for reference; not enforced by Titanium server):

| Class        | Allowed Deities (IDs)                                       |
|--------------|-------------------------------------------------------------|
| Warrior      | 396, 201, 204, 206, 207, 208, 211, 212 (human); race-varies |
| Cleric       | 202, 203, 204, 205, 206, 207, 208, 209, 210, 212, 215      |
| Paladin      | 202, 203, 204, 207, 208, 210, 212, 215                      |
| Ranger       | 207, 215                                                    |
| ShadowKnight | 201, 203, 206, 211                                          |
| Druid        | 207, 215                                                    |
| Monk         | 210, 396                                                    |
| Bard         | 202, 204, 205, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 396 |
| Rogue        | 396, 201, 204, 205, 206, 207, 209, 210, 212                 |
| Shaman       | 202, 203, 205, 206, 207, 211, 214, 396                      |
| Necromancer  | 201, 203, 206, 213, 396                                     |
| Wizard       | 201, 202, 204, 207, 208, 209, 210, 212, 213, 215, 396       |
| Magician     | 201, 202, 204, 207, 208, 209, 210, 212, 213, 215, 396       |
| Enchanter    | 201, 202, 204, 207, 208, 209, 210, 212, 213, 215, 396       |
| Beastlord    | 202, 203, 205, 206, 207, 211, 214, 396                      |
| Berserker    | 202, 211, 214, 396                                          |

Note: These lists are per the SoF-format combinations table and vary by race too. Since Titanium doesn't enforce deity, offering all class-appropriate deities (and agnostic 396) is safe.

---

## 5. Start Zone / StartZoneIndex

The `start_zone` field in `CharCreate_Struct` is the `StartZoneIndex` (0–14), NOT a zone ID.
Source: `eq_constants.h:974`, `titanium_structs.h:585–598`.

| Index | Name            | Server zone_id (spawn)        | bind zone_id              |
|-------|-----------------|-------------------------------|---------------------------|
| 0     | Odus            | erudnext (Cazic→paineel)      | tox (Cazic→paineel)       |
| 1     | Qeynos          | qeynos2                       | qeynos2                   |
| 2     | Halas           | halas                         | everfrost                 |
| 3     | Rivervale       | rivervale                     | kithicor                  |
| 4     | Freeport        | freportw                      | freportw                  |
| 5     | Neriak          | neriaka                       | nektulos                  |
| 6     | Grobb           | grobb                         | innothule                 |
| 7     | Oggok           | oggok                         | feerrott                  |
| 8     | Kaladim         | kaladima                      | butcher                   |
| 9     | Greater Faydark | gfaydark                      | gfaydark                  |
| 10    | Felwithe        | felwithea                     | gfaydark                  |
| 11    | Akanon          | akanon                        | steamfont                 |
| 12    | Cabilis         | cabwest                       | fieldofbone               |
| 13    | Shar Vahl       | sharvahl                      | sharvahl                  |
| 14    | Rathe Mountains | rathemtn                      | rathemtn                  |

Source: `worlddb.cpp:634–742` (`SetTitaniumDefaultStartZone`).

Special case: Odus (index 0) with deity=CazicThule (203) → Paineel, not Erudin.

### Per-Race Default Start Zones

From `char_create_combinations` (expansions_req=0, start_zone is SoF zone ID there — use indices for Titanium):

| Race       | Typical index | Notes                              |
|------------|---------------|------------------------------------|
| Human      | 1 (Qeynos) or 4 (Freeport) | deity-dependent        |
| Barbarian  | 2 (Halas)     |                                    |
| Erudite    | 0 (Odus)      | Cazic-Thule → 0 (Paineel variant)  |
| Wood Elf   | 9 (GFaydark)  |                                    |
| High Elf   | 10 (Felwithe) |                                    |
| Dark Elf   | 5 (Neriak)    |                                    |
| Half Elf   | 1 or 4 or 10  | deity-dependent                    |
| Dwarf      | 8 (Kaladim)   |                                    |
| Troll      | 6 (Grobb)     |                                    |
| Ogre       | 7 (Oggok)     |                                    |
| Halfling   | 3 (Rivervale) |                                    |
| Gnome      | 11 (Akanon)   |                                    |
| Iksar      | 12 (Cabilis)  |                                    |
| Vah Shir   | 13 (Shar Vahl)|                                    |

---

## 6. Appearance Ranges

The server does NOT validate appearance at character creation for Titanium (`CheckCharCreateInfoTitanium` only checks stats and race/class combo). These ranges are from `races.cpp` RaceAppearance validation and describe what the native client offers.

### Face (0–7 for all playable races)

All 14 Titanium playable races: `face` in range [0, 7]. Source: `races.cpp:1721`.

### Eye Color (eyecolor1, eyecolor2)

| Races               | Range |
|---------------------|-------|
| All except Troll    | 0–9   |
| Troll               | 0–10  |

Source: `races.cpp:1660`.

### Hair Style (hairstyle) — Luclin models enabled

| Race/Gender                                     | Range |
|-------------------------------------------------|-------|
| Human M/F, Barb M/F, WE M/F, HE M/F, DE M/F   | 0–3   |
| HalfElf M/F, Dwarf M/F, HalflingM/F, Gnome M/F| 0–3   |
| TrollFemale, OgreFemale                         | 0–3   |
| Erudite Male                                    | 0–5   |
| Erudite Female                                  | 0–8   |
| TrollMale, OgreMale, Iksar M/F, VahShir M/F    | no hair (send 0) |

Without Luclin (classic models), only Drakkin have hair — i.e., for Titanium classic mode, Iksar/VahShir/Troll/Ogre have no hair options. The UI should still send 0 for hairless races.

Source: `races.cpp:1782`.

### Hair Color (haircolor)

| Race/Gender                                      | Range  |
|--------------------------------------------------|--------|
| Human M/F, Barb M/F, WE M/F, HalfElf M/F       | 0–19   |
| Dwarf M/F, Halfling M/F                         | 0–19   |
| High Elf M/F                                    | 0–14   |
| Dark Elf M/F                                    | 13–18  |
| Gnome M/F                                       | 0–24   |
| TrollFemale, OgreFemale                         | 0–23   |
| TrollMale, OgreMale, Iksar M/F, VahShir M/F    | no hair color (send 0) |

Source: `races.cpp:1861`.

### Beard (beard) — Luclin models enabled

| Race/Gender                                           | Range |
|-------------------------------------------------------|-------|
| Human M, Barb M, Erudite M, Dwarf M, Halfling M, Gnome M | 0–5 |
| High Elf M, Dark Elf M, Half Elf M                   | 0–3   |
| Dwarf Female                                          | 0–1   |
| All others (all females except Dwarf; Troll M/F, Ogre M/F, Iksar M/F, VahShir M/F) | no beard (send 0) |

Source: `races.cpp:1519`.

### Beard Color (beardcolor)

| Race/Gender                                              | Range  |
|----------------------------------------------------------|--------|
| Human M, Barb M, Erudite M, HalfElf M, Dwarf M/F, Halfling M | 0–19 |
| High Elf M                                               | 0–14   |
| Dark Elf M                                               | 13–18  |
| Gnome M                                                  | 0–24   |
| All others                                               | send 0 |

Source: `races.cpp:1586`.

---

## 7. Name Rules

All rules confirmed from `HandleNameApprovalPacket` (`client.cpp:567`) and `CheckNameFilter` (`database.cpp:830`).

1. **Length**: 4–15 characters inclusive.
2. **Characters**: alphabetic only (`isalpha` check; digits and punctuation rejected).
3. **Capitalization**: first character MUST be uppercase (`islower(name[0])` must be false); all subsequent characters MUST be lowercase (`isupper` check for i>=1).
4. **No spaces** (strstr check).
5. **No triple+ consecutive identical characters** (e.g. "aaa" fails; "aa" ok).
6. **Forbidden word filter**: server checks against `name_filter` DB table (substring match, case-insensitive). Server-specific; varies by installation.
7. **Uniqueness**: `ReserveName` DB check; rejected if name already taken.

Response packet: `OP_ApproveName`, 1 byte: `1` = approved, `0` = rejected.

The `NameApproval_Struct` (72 bytes) contains: `uint32 race_id`, `uint32 gender`, `char name[64]`.
Race and class in the name request ARE validated as valid player race/class IDs, but only to prevent crashes — not cross-validated with each other.

---

## 8. Default Stat Allocation / Screen Seeding

The `char_create_point_allocations` table (SQL:674) shows how the native client pre-spends bonus points (the `alloc_*` columns). These are the "default" stat screen values before the player moves sliders.

Example for Human Warrior (allocation_id 58):
- base: STR=85,DEX=75,AGI=80,STA=85,INT=75,WIS=75,CHA=75 (sum=550)
- alloc (pre-spent): alloc_sta=25 → default sends STA=110
- Total sent = 575 = 550 + 25 ✓

**Critical**: The server validates that sent total == base_total + bonus_points EXACTLY. The Rust UI must ensure all points are spent before allowing character creation (the "create" button should be disabled if any points remain).

For sensible UI defaults:
- Pre-spend all points into the stat with highest alloc_* in the combinations table for that race/class.
- face=0, hairstyle=0, haircolor=0, beard=0, beardcolor=0, eyecolor1=0, eyecolor2=0.
- gender=0 (male) default.
- tutorial=0 unless a tutorial zone is configured.

---

## Key Differences: Titanium vs SoF Validation

| Feature          | Titanium (CheckCharCreateInfoTitanium) | SoF+ (CheckCharCreateInfoSoF)        |
|------------------|----------------------------------------|--------------------------------------|
| Race/Class       | Hardcoded table in client.cpp          | char_create_combinations DB table    |
| Deity            | NOT validated (TODO comment)           | Validated via DB table               |
| Start Zone       | NOT validated                          | Validated via DB table               |
| Stats            | Exact total, per-stat bounds           | Same logic but via DB allocations    |
| Appearance       | Not validated at all                   | Not validated at all                 |
