# Design: Pre-game UI — Login / Character-Select / Character-Create screens

**Date:** 2026-06-26
**Status:** Design approved-in-progress; **implementation deferred** (may conflict with in-flight
work — startup reorder is broad). Resume from this doc.
**Branch context:** drafted on `worktree-mordeth`, which already added the client-side
character-creation handshake (commit `8a03a15`) that this UI builds on.

## Goal

When the client is launched **without `--config`**, present interactive **Login → Character-Select
→ Character-Create** screens that follow the same rules the server enforces for **RoF2**, the client
this repo targets. (Some tables in Section 3 are EQEmu's *Titanium*-path validator arrays; each is
labelled with the client it applies to, and the RoF2 rule is given alongside where the two differ.)
With `--config <name>` the current non-interactive behavior is unchanged.

## Decisions (from brainstorming)

| Question | Decision |
|----------|----------|
| Visual fidelity | **Functional egui** widgets that faithfully follow native *rules & flow* — not pixel-faithful EQ window art. |
| Create-screen depth | **Full rules + live 3D character preview** (rotating model that updates with race/gender/face/hair). |
| New-account behavior | **Implicit auto-create**: typing a new username+password and clicking Login auto-creates the account (loginserver `auto_create_accounts`). Existing username + wrong password still fails. No separate account UI. |
| Char-select actions | **Enter World + Create New + Delete** (Delete needs `OP_DeleteCharacter`). |

## Section 1 — App phases & startup reorder (the core architectural change)

**Problem:** today `main()` reads credentials from the config file, performs asset-server login and
spawns the EQ network thread, *then* opens the window. A login UI inverts this: credentials don't
exist until the user types them into a screen that only exists after the window is up. So the
window/egui must come up **first**, and network + asset-sync must be **driven by UI events**.

**Phase state machine** (selects which egui screen renders):

```
            ┌───────────────────── --config given ─────────────────────┐
            v                                                           │
   [Login] ──submit creds──▶ (auth on net thread) ──▶ [CharSelect] ──Enter──▶ [Loading] ──▶ [InGame]
            ▲  auth fail          │                       │  ▲
            └─────────────────────┘                  Create│  │back     (asset-sync runs
                                                           v  │          during Loading)
                                                      [CharCreate] ──created──┘
                                                        Delete ──▶ (refresh CharSelect)
```

- **`--config <name>` path unchanged:** if a config is given, skip straight through Login/CharSelect
  using the file's creds + `character_name` (today's behavior). The UI only appears when `--config`
  is omitted.
- **Asset-sync moves** from "before window" to the **Loading** phase (after a character is chosen,
  before zone-in): it needs the username/password the user just typed, and gameplay needs the synced
  `gamedata`/`gameequip` sets.

**UI ↔ network thread channel** — reuse the existing `Arc<Mutex<…>>` command-slot pattern (same as
the HTTP API in `http.rs` / `main.rs`):
- `LoginCreds` (UI→net): `{username, password}` to start auth.
- `PregameStatus` (net→UI): enum `Connecting | AuthFailed(msg) | CharList(Vec<CharSummary>) |
  Creating | CreateFailed(msg) | Entering`.
- `PregameCmd` (UI→net): `EnterWorld(name) | Create(CharCreateParams) | Delete(name)`.

`LoginProtocol` gains an **interactive mode**: after auth it publishes the parsed char list and
*waits* on `PregameCmd` instead of auto-entering. Create/Delete run their handshakes and re-publish
the refreshed list; EnterWorld proceeds into the existing zone-in path. The network state machine
stays the single source of truth; egui screens are thin views over shared slots.

## Section 2 — Components (to detail when implementation resumes)

1. **App phase enum + screen dispatch** (`app.rs` egui pass): generalize the current `loading: bool`
   into `AppPhase { Login, CharSelect, CharCreate, Loading, InGame }`.
2. **Pre-game screens** (new module, e.g. `src/pregame/`): three egui views (login, char-select,
   create) as thin functions over shared state → emit `PregameCmd`/`LoginCreds`.
3. **Create-rules data module** (new, e.g. `src/chardata.rs`): the hardcoded native tables below.
   Independently unit-testable (valid-combo predicate, stat-allocation exact-total, per-race city
   list, appearance ranges). **This is the lowest-risk piece and can land first/independently.**
4. **Char-list parsing**: parse the 1704-byte `CharacterSelect_Struct` into `Vec<CharSummary>`
   (replaces today's substring scan in `login.rs`).
5. **Interactive `LoginProtocol`** + the command-slot channel (Section 1).
6. **`OP_DeleteCharacter`** (RoF2 `0x1808`, `utils/patches/patch_RoF2.conf:37`; `0x26c9` is the
   Titanium value).
7. **Live 3D preview** (`src/pregame/preview.rs`): render the selected race/gender/face/hair model to
   an offscreen texture exposed to egui. Reuse `render_model` / renderer model-loading. Re-render
   only on input change. **Highest-risk integration — stage last.**

## Section 3 — Native rules data (cited to EQEmu source)

Summary tables below; each cites its own EQEmu source directly. (This section originally pointed
here at a private working note, `~/git/eq_kb/character-creation.md`, as the consolidated citation
for the whole section. That note has no history in the private knowledge-base repo at all — checked
via `git rev-list --objects --all` there, zero blobs of that name — so it dangled from the day the
KB migrated. It is not restored, and it is not repointed at a lookalike: each heading below names
its own EQEmu source inline instead — including the per-class deity restrictions and the default
stat pre-spend, both sourced below to EQEmu's own seed rows.)

**The scope of that claim, so it can be checked instead of trusted.** It ranges over the **ten**
headings below and was discharged by classifying all ten, not by spot-checking, and it covers the
**server rules** they state. It does *not* cover the one thing in this section that is a UI choice
rather than a server rule — the create screen's own initial appearance values and gender, under
*Default stat pre-spend* — which is labelled in place as this document's default, not EQEmu's.
`UNCITED` is a marker with a fixed meaning in this repo (`crates/eqoxide-core/src/physics.rs:38`,
`:512`): *no source for this exists in this tree or the private one*. No rule in this section is in
that state; where an earlier revision left one that way, it is now either sourced or deleted, and
the deletion is recorded where the text stood.

### Race IDs (`common/races.h`)
Human=1, Barbarian=2, Erudite=3, Wood Elf=4, High Elf=5, Dark Elf=6, Half Elf=7, Dwarf=8, Troll=9,
Ogre=10, Halfling=11, Gnome=12, Iksar=128, Vah Shir=130, Froglok=330, Drakkin=522. All **sixteen**
match `Race` namespace constants in `common/races.h` one-for-one: `races.h:45–56` for Human…Gnome,
`:172` Iksar, `:174` VahShir, `:374` Froglok2 (=330), `:566` Drakkin.

**All sixteen are creatable under RoF2 — do not hide froglok or drakkin.** The creatable set is
exactly the distinct `race` values in `char_create_combinations`, the table `CheckCharCreateInfoSoF`
validates against (the deity section below names the file and the row count): its 641 seed rows carry
**16** distinct race ids, the sixteen above, with 330 in **18** rows and 522 in **13**.
`CheckCharCreateInfoSoF` matches only `Class`/`Race`/`Deity`/`Zone` (`world/client.cpp:1912–1915`)
and never gates on `ExpansionRequired`, which occurs in `world/` only where it is read off the row
(`worlddb.cpp:827`, `sof_char_create_data.h:30`) and echoed back to the client (`client.cpp:732`).
Drive the race list off the combo rows rather than a hardcoded list.

> ⚠ **This corrects an earlier revision of this section, which said froglok and drakkin "exist in
> tables but are not Titanium-creatable — hide".** The directive is removed rather than hedged,
> because it was wrong under both clients. Under RoF2 the combo rows above refute it. It is also
> wrong about EQEmu's **Titanium** validator, which admits both: `ClassRaceLookupTable` is
> dimensioned `[…][_TABLE_RACES]` with `_TABLE_RACES 16` (`world/client.cpp:2011`), its column header
> names Froglok and Drakkin as columns 15–16 (`:2054`), the Warrior row is `true` in both (`:2055`),
> and `CheckCharCreateInfoTitanium` maps them in explicitly — `Race::Froglok2` to index 14 (`:2081`)
> and `Race::Drakkin` to index 15 (`:2084`). What a retail Titanium client offered in its own UI is a
> client-side question this document does not source; it is also moot, because this repo targets
> RoF2.

> ⚠ **Name collision — the two high ids do not both match by name.** Drakkin=522 is `Race::Drakkin`
> (`races.h:566`), but 330 is `Race::Froglok2` (`races.h:374`); `Race::Froglok` is **26**
> (`races.h:70`), a different race that appears in **no** row of the seed and is not creatable. 330
> is the right id for the creatable froglok — it is the only froglok id present in
> `char_create_combinations`. So the number is right and the *name* is the trap. Same shape as
> `Agnostic1`/`Agnostic2` below; code that resolves either of these by constant name will pick the
> wrong one.

### Race/class validity matrix — Titanium validator table (`ClassRaceLookupTable`, `world/client.cpp:2053`)
All 16 classes × all 16 races are rendered; the column header naming Froglok and Drakkin as columns
15–16 is `world/client.cpp:2054`.
```
              Hum Bar Eru  WE  HE  DE HlfE Dwr Trl Ogr Hlf Gno Iks VaS Frg Drk
Warrior  (1)    Y   Y   -   Y   -   Y   Y   Y   Y   Y   Y   Y   Y   Y   Y   Y
Cleric   (2)    Y   -   Y   -   Y   Y   Y   Y   -   -   Y   Y   -   -   Y   Y
Paladin  (3)    Y   -   Y   -   Y   -   Y   Y   -   -   Y   Y   -   -   Y   Y
Ranger   (4)    Y   -   -   Y   -   -   Y   -   -   -   Y   -   -   -   -   Y
SK       (5)    Y   -   Y   -   -   Y   -   -   Y   Y   -   Y   Y   -   Y   Y
Druid    (6)    Y   -   -   Y   -   -   Y   -   -   -   Y   -   -   -   -   Y
Monk     (7)    Y   -   -   -   -   -   -   -   -   -   -   -   Y   -   -   Y
Bard     (8)    Y   -   -   Y   -   -   Y   -   -   -   -   -   -   Y   -   Y
Rogue    (9)    Y   Y   -   Y   -   Y   Y   Y   -   -   Y   Y   -   Y   Y   Y
Shaman  (10)    -   Y   -   -   -   -   -   -   Y   Y   -   -   Y   Y   Y   -
Necro   (11)    Y   -   Y   -   -   Y   -   -   -   -   -   Y   Y   -   Y   Y
Wizard  (12)    Y   -   Y   -   Y   Y   -   -   -   -   -   Y   -   -   Y   Y
Mage    (13)    Y   -   Y   -   Y   Y   -   -   -   -   -   Y   -   -   -   Y
Enchant (14)    Y   -   Y   -   Y   Y   -   -   -   -   -   Y   -   -   -   Y
Beastlord(15)   -   Y   -   -   -   -   -   -   Y   Y   -   -   Y   Y   -   -
Berserker(16)   -   Y   -   -   -   -   -   Y   Y   Y   -   -   -   Y   -   -
```
> ⚠ **This is the Titanium path's table; it is not what RoF2 validates against.** RoF2 creates go
> through `CheckCharCreateInfoSoF`, which matches `char_create_combinations` rows (see the deity
> section below), not this array. The two nearly agree: all **112** distinct race/class pairs in the
> 641 seed rows are `true` here, and this table carries exactly **one** pair the seed does not —
> **Half Elf Cleric**. Race 7 has 116 seed rows, across classes 1/3/4/6/8/9 (Warrior, Paladin,
> Ranger, Druid, Bard, Rogue), and **zero** with class 2. So a half-elf cleric is admitted by the
> Titanium validator and rejected by RoF2's. Build the UI's validity predicate from the combo rows.

### Race base stats — Titanium validator array `BaseRace` (`world/client.cpp:2013`), order STR/STA/AGI/DEX/WIS/INT/CHR
All 16 rows of the array are rendered (`_TABLE_RACES` is 16, `world/client.cpp:2011`).
```
Human      75 75 75 75 75 75 75      Dwarf     90 90 70 90 83 60 45
Barbarian 103 95 82 70 70 60 55      Troll    108 109 83 75 60 52 40
Erudite    60 70 70 70 83 107 70     Ogre     130 122 70 70 67 60 37
Wood Elf   65 65 95 80 80 75 75      Halfling  70 75 95 90 80 67 50
High Elf   55 65 85 70 95 92 80      Gnome     60 70 85 85 67 98 60
Dark Elf   60 65 90 75 83 99 60      Iksar     70 70 90 85 80 75 55
Half Elf   70 70 90 85 60 75 75      Vah Shir  90 75 90 70 70 65 65
Froglok    70 80 100 100 75 75 50    Drakkin   70 80 85 75 80 85 75
```
> ⚠ **RoF2 does not use this array.** `CheckCharCreateInfoSoF` range-checks the submitted stats
> against the `char_create_point_allocations` row named by the matched combo's `AllocationIndex`
> (`world/client.cpp:1927–1941`), and that struct's `BaseStats[7]` is indexed in a **different order**
> — STR, DEX, AGI, STA, INT, WIS, CHA (`world/client.cpp:1951–1984`). Reading those values with this
> table's column order swaps STA with DEX and WIS with INT.

### Class stat bonuses + bonus-point pool — Titanium validator array `BaseClass` (`world/client.cpp:2033`), order STR/STA/AGI/DEX/WIS/INT/CHR/ADD
All 16 rows are rendered. RoF2 does not use this array either — see the base-stats note above.
```
Warrior      10 10  5  0  0  0  0  25      Rogue       0  0 10 10  0  0  0  30
Cleric        5  5  0  0 10  0  0  30      Shaman      0  5  0  0 10  0  5  30
Paladin      10  5  0  0  5  0 10  20      Necromancer 0  0  0 10  0 10  0  30
Ranger        5 10 10  0  5  0  0  20      Wizard      0 10  0  0  0 10  0  30
ShadowKnight 10  5  0  0  0 10  5  20      Magician    0 10  0  0  0 10  0  30
Druid         0 10  0  0 10  0  0  30      Enchanter   0  0  0  0  0 10 10  30
Monk          5  5 10 10  0  0  0  20      Beastlord   0 10  5  0 10  0  5  20
Bard          5  0  0 10  0  0 10  25      Berserker  10  5  0 10  0  0  0  25
```

### Stat validation — RoF2 (`CheckCharCreateInfoSoF`, `world/client.cpp:1901`) vs Titanium (`CheckCharCreateInfoTitanium`, `:2002`)
**RoF2 (`CheckCharCreateInfoSoF`, `world/client.cpp:1901`) — an inequality, and the numbers come
from the database, not from a table in the source.** The matched combo's `AllocationIndex` selects a
`char_create_point_allocations` row (`world/client.cpp:1927–1941`); that row supplies both halves:

```
base[s] = allocation.BaseStats[s]                    (the row's base_* columns)
pool    = sum(allocation.DefaultPointAllocation[])   (the row's alloc_* columns, `client.cpp:1943–1949`)
sent[s] >= base[s]                                   (per stat, `client.cpp:1951–1984`)
sent[s] <= base[s] + pool                            (per stat, same lines)
sum(sent - base) <= pool                             (INEQUALITY, `client.cpp:1986–1997`)
```

So under RoF2 **a create with points left unspent is accepted** — there is no exact-sum test — and
the per-stat floor/ceiling are the only hard bounds. Deity *is* validated on this path and a
mismatch aborts the create (see the deity section below). `pool` is the same seed data the pre-spend
section below is derived from, so the two must be read together: the pre-spend is a *point in* the
allowed region, not the region.

**Titanium (`CheckCharCreateInfoTitanium`, `world/client.cpp:2002`) — must hold EXACTLY. Not this
client.**

```
base[s] = BaseRace[race][s] + BaseClass[class][s]   (`client.cpp:2013` / `:2033`, summed `:2107–2113`)
pool    = BaseClass[class][7]                       (the "ADD" column, `world/client.cpp:2034`)
sum(sent) == sum(base) + pool                       (EXACT, `client.cpp:2115–2116` and `:2125`)
sent[s] >= base[s]                                  (per stat, `client.cpp:2130–2157`)
sent[s] <= base[s] + pool                           (per stat, same lines)
```

Three differences that matter, none of them cosmetic: Titanium's numbers are **hard-coded arrays in
`world/client.cpp`**, not database rows; its sum test is an **equality**, so Create must be disabled
while points remain; and it does **not** validate deity or appearance at all — the function carries
an explicit `/*TODO: Check for deity/class/race..*/` where that check would be
(`world/client.cpp:2159`) and then returns purely on the counted stat errors (`:2164`). Do not port
the exact-sum rule, the hard-coded tables, or the missing deity check to RoF2.

### Deity IDs (`namespace Deity`, `common/deity.h:26`)
Constant names exactly as EQEmu spells them (`common/deity.h:28–45`): Agnostic2=396,
Bertoxxulous=201, BrellSirilis=202, CazicThule=203, ErollisiMarr=204, Bristlebane=205, Innoruuk=206,
Karana=207, MithanielMarr=208, Prexus=209, Quellious=210, RallosZek=211, RodcetNife=212,
SolusekRo=213, TheTribunal=214, Tunare=215, Veeshan=216.

> ⚠ **Do not use the constant names as UI strings — for two of them they differ.** The player-facing
> strings live in a separate map, `deity_names` (`common/deity.h:73–92`), and it renders 202 as
> `"Brell Serilis"` (`:77`, an *e*, against the constant's `BrellSirilis`) and 203 as `"Cazic-Thule"`
> (`:79`, hyphenated, against the constant's `CazicThule`). An earlier revision of this list mixed
> the two conventions — it wrote `BrellSerilis` (which is neither) and `Cazic-Thule` (the display
> string) while spelling every other entry as a constant. Take ids from the constants and labels from
> `deity_names`.

> ⚠ `common/deity.h` defines **two** Agnostic constants and this list carries only one.
> `Deity::Agnostic1 = 140` (`common/deity.h:28`) and `Deity::Agnostic2 = 396` (`common/deity.h:45`)
> map to the *same* display string `"Agnostic"` (`common/deity.h:74–75`) and the *same*
> `Deity::Bitmask::Agnostic = 1` (`common/deity.h:48`, joined by `deity_bitmasks` at
> `common/deity.h:95–96`). 396 is the char-create-reachable one: 140
> appears in **0** of the 641 seeded `char_create_combinations` rows (below), so a create can never
> carry it. It is still reachable on the **read** path — a `deity` field read back off a
> `PlayerProfile` can be 140, and an id→name map built only from the list above will fail to name it.

**Per-class/per-race deity restrictions — sourced to EQEmu.** EQEmu ships the seed data for
`char_create_combinations` in a tracked file: `utils/sql/svn/2024_required_update.sql`, whose
`CREATE TABLE char_create_combinations` is line 1 and which carries **641** contiguous
`INSERT INTO char_create_combinations` rows at lines 11–651, every one with the same column list
`(expansions_req, race, class, deity, allocation_id, start_zone)`. The per-class deity list is just a
projection of the `deity` column of those rows — e.g. class 1 (Warrior) →
`{201,202,203,204,206,207,208,209,211,212,214,215,216,396}`, class 2 (Cleric) →
`{201,202,203,204,205,206,207,208,209,210,212,215,216}`. Across all 641 rows the `deity` column takes
only the values `201`–`216` and `396`.

> ⚠ **Caveat on that citation, which does not discharge it but does bound it:** that file is a
> one-shot svn-era migration script — a *seed* snapshot, not the live table. A long-lived PEQ
> database may have drifted from it, and the server validates against the live rows. Two ways to get
> the live values, in preference order:
>
> 1. **Read them off the wire.** On SoF-and-later the world server answers
>    `OP_CharacterCreateRequest` by serialising the whole live combo vector to the client:
>    `Client::HandleCharacterCreateRequestPacket` (`world/client.cpp:699–744`) writes a leading
>    `uint8` (always 0 — `client.cpp:711–712`, and counted in the length at `:705–707`), then a
>    `uint32` allocation count + `RaceClassAllocation` array, then a `uint32` combo count + one
>    `RaceClassCombos` per row
>    (`world/sof_char_create_data.h:29–36`: `ExpansionRequired, Race, Class, Deity, AllocationIndex,
>    Zone`). That is authoritative for the server actually connected to, and it is what the
>    char-create UI should drive off — including the start-zone resolution noted below.
> 2. Read `SELECT * FROM char_create_combinations` on the target server
>    (`WorldDatabase::LoadCharacterCreateCombos`, `world/worlddb.cpp:811–833`, is the exact query the
>    server runs into `character_create_race_class_combos`).

**Do not ship char-create without deity filtering — under RoF2 it is a correctness gap, not a
nicety.** `Client::OPCharCreate` (`world/client.cpp:1677`) branches on client version at
`world/client.cpp:1734–1738`: `m_ClientVersionBit & EQ::versions::maskSoFAndLater` →
`CheckCharCreateInfoSoF(cc)`, and a `false` return aborts the create. `CheckCharCreateInfoSoF`
(`world/client.cpp:1901`) scans `character_create_race_class_combos` for a row matching `Class`,
`Race`, **`Deity`** and `Zone` (`world/client.cpp:1912–1915`); on no match it logs
`"Could not find class/race/deity/start_zone combination"` and returns `false`
(`world/client.cpp:1922–1924`). Same function and same match expression as the start-zone warning
below — `deity` is one field over from `start_zone`, and a wrong deity fails exactly the way a
`start_zone` that is not a valid zone_id for the combo does. Titanium is the exception, not the rule: `CheckCharCreateInfoTitanium`
carries an explicit `/*TODO: Check for deity/class/race..*/` at `world/client.cpp:2159` and performs
no such check.

### Start city → `start_zone` wire value: a ZONE_ID under RoF2 (`WorldDatabase::GetStartZone`, `world/worlddb.cpp:500`)
> ⚠ **The `start_zone` wire value is a ZONE_ID, not the Titanium StartZoneIndex.** RoF2's
> `CheckCharCreateInfoSoF` matches `cc->start_zone` against `char_create_combinations.start_zone`
> (zone_ids), so the UI must resolve the chosen start city to a **zoneidnumber valid for that
> race/class/deity** (e.g. Dark Elf Necromancer → 42 `neriakc` or 394 `crescent`). Sending the raw
> index makes the server reject every create (eqoxide#5). One named city can also resolve to
> different zone_ids per combo — Neriak has two, 41 and 42 — so resolve it from
> `char_create_combinations`, not from a fixed city→id table.

EQEmu splits the two lineages at one call site, and that split is why this heading carries no index
table. `WorldDatabase::GetStartZone` (`world/worlddb.cpp:500`) takes an `is_titanium` flag, passed as
`m_ClientVersionBit & EQ::versions::maskTitaniumAndEarlier` (`world/client.cpp:1816`). Its own comment
is explicit: *"SoF doesn't send the player_choice field in character creation, it now sends the real
zoneID instead"* (`world/worlddb.cpp:506`). Titanium-and-earlier looks the row up by
`start_zones.player_choice` (`:531–542`); everything later, **RoF2 included**, looks it up by
`start_zones.zone_id` (`:544–555`). On no matching row the same ternary picks the matching defaulter
(`:564–566`); the RoF2 one is `WorldDatabase::SetSoFDefaultStartZone` (`:616–632`), whose last resort
is Crescent Reach (`:630`). So the picker's rows under RoF2 are the distinct `start_zone` values of
the combos matching the chosen race/class/deity — read off the wire or the table, per the deity
section above.

For orientation only, here is what the seed contains today, **all sixteen races enumerated** rather
than sampled (zone names from the `Zones` namespace in `common/eq_constants.h`, line cited per id).
This is a snapshot of one seed file, not a rule to hardcode — the caveat on the deity section's
citation applies here identically:

```
Human      1,2,3,9,10,45,394          Troll      52,394
Barbarian  29,394                     Ogre       49,394
Erudite    23,24,75,394               Halfling   19,394
Wood Elf   54,394                     Gnome      55,394
High Elf   61,62,394                  Iksar      82,106,394
Dark Elf   41,42,394                  Vah Shir   155,394
Half Elf   1,2,3,9,10,45,54,61,394    Froglok    50,394
Dwarf      60,67,394                  Drakkin    394
```

1 QEYNOS (`:201`), 2 QEYNOS2 (`:202`), 3 QRG (`:203`), 9 FREPORTW (`:208`), 10 FREPORTE (`:209`),
19 RIVERVALE (`:218`), 23 ERUDNINT (`:222`), 24 ERUDNEXT (`:223`), 29 HALAS (`:228`),
41 NERIAKB (`:240`), 42 NERIAKC (`:241`), 45 QCAT (`:244`), 49 OGGOK (`:248`), 50 RATHEMTN (`:249`),
52 GROBB (`:251`), 54 GFAYDARK (`:253`), 55 AKANON (`:254`), 60 KALADIMA (`:259`),
61 FELWITHEA (`:260`), 62 FELWITHEB (`:261`), 67 KALADIMB (`:266`), 75 PAINEEL (`:274`),
82 CABWEST (`:281`), 106 CABEAST (`:304`), 155 SHARVAHL (`:333`), 394 CRESCENT (`:546`) — 26 distinct
ids across the 641 rows, and **394 Crescent Reach is the one value every one of the sixteen races
carries**, which is why it is also the SoF defaulter's last resort (`world/worlddb.cpp:630`).
Drakkin carries *only* 394. The narrowing is per class and deity as well as per race — Dark Elf
Necromancer, for instance, is `{42, 394}`, not the full Dark Elf set `{41, 42, 394}` — so query the
combos with all three fields, never race alone.

> ⚠ **This deletes a table an earlier revision of this section carried:** a start-city index table
> numbered 0–13 (`0 Odus(erudnext; paineel if deity=203)` … `13 Shar Vahl`) beside a per-race index
> column (`Human 1,4` … `Vah Shir 13`). Deleted rather than relabelled, on three counts. **(1)** Its
> left half is Titanium's `enum StartZoneIndex` (`common/eq_constants.h:974–990`), whose only
> consumer is `WorldDatabase::SetTitaniumDefaultStartZone` (`world/worlddb.cpp:634`, the Odus
> deity-203 branch at `:643–655`) — reached only on the `is_titanium` side of the split above, so it
> never governed this client. **(2)** It was short by one anyway: that enum runs 0–**14**, ending
> `RatheMtn = 14` (`common/eq_constants.h:989`), handled at `world/worlddb.cpp:735`. **(3)** Its right
> half, the per-race index assignments, had **no EQEmu source at all** — UNCITED in this repo's sense,
> without saying so. Attributing this client's start-city behaviour to a Titanium enum is the defect
> class this whole change exists to remove, so the table goes rather than gaining a label.

### Appearance ranges (`common/races.cpp`; not server-validated, UI guidance)
The ranges are the `RaceAppearance::IsValid*` functions in `common/races.cpp`, EQEmu's own tabulation
of what each race **and gender** accepts. Two properties of that file bound how far these go, and
both are load-bearing:

- **It is not on the character-creation path.** Its only callers in the tree are the bot appearance
  commands (`zone/bot_commands/bot_appearance.cpp:68`–`:539`). `CheckCharCreateInfoSoF` matches only
  `Class`/`Race`/`Deity`/`Zone` (`world/client.cpp:1912–1915`), so a wrong value here is a cosmetic
  bug, not a create failure. Use the ranges to constrain the UI, not to predict a reject.
- **Every one of these ten functions returns `true` for an "unset" sentinel before its switch, and
  `false` for any race/gender it does not name.** The sentinel is `std::numeric_limits<T>::max()` of
  the *value parameter's own type*, so it is **`0xFF` for the seven `uint8` attributes** (beard,
  beard colour, eye colour, face, hair, hair colour, woad) and **`0xFFFFFFFF` for the three `uint32`
  ones** (detail, heritage, tattoo) — do not use one sentinel for all ten. So "no valid value" below
  means exactly that: the validator accepts nothing but the sentinel — **not** that 0 is accepted.
- **The Luclin branch is the reachable one.** Each function's `use_luclin` parameter defaults to
  `true` (`common/races.h:822–831`) and every caller in the tree takes the default, so where a
  function branches on it the ranges below are read off the Luclin side.

All **sixteen** creatable races are covered, per gender wherever the source distinguishes them, and
every one of the ten `IsValid*` functions is accounted for below. A `—` means the validator names no
value at all for that race/gender. "The fourteen" below always means the sixteen creatable races
minus Froglok and Drakkin, which is the split EQEmu's own switches use.

- **Face** — `IsValidFace` (`:1721–1780`): Drakkin **0–6** (`:1728–1730`); the fourteen **0–7**
  (`:1735–1763`); Froglok **0–9** (`:1768–1770`).
- **Eye color 1 and 2** — one function, `IsValidEyeColor` (`:1660–1719`), governs both fields:
  **0–9** for thirteen of the fourteen (`:1667–1693`); Troll **0–10** (`:1698–1700`); Froglok and
  Drakkin **0–11** (`:1705–1709`).
- **Hairstyle** — `IsValidHair` (`:1782–1859`; Luclin branch `:1788–1838`): **0–3** for
  Human, Barbarian, Wood Elf, High Elf, Dark Elf, Half Elf, Dwarf, Halfling and Gnome (both genders)
  plus Troll F and Ogre F (`:1790–1810`); Erudite M **0–5** (`:1815–1816`); Drakkin F **0–7**
  (`:1821–1822`); Erudite F and Drakkin M **0–8** (`:1827–1829`); **—** for Troll M, Ogre M, Iksar,
  Vah Shir and **Froglok**.
- **Hair color** — `IsValidHairColor` (`:1861–1927`): Gnome **0–24** (`:1868–1870`); Troll F and Ogre F **0–23**
  (`:1875–1877`); Human, Barbarian, Wood Elf, Half Elf, Dwarf, Halfling **0–19** (`:1882–1894`);
  Dark Elf **13–18** (`:1899–1901`); High Elf **0–14** (`:1906–1908`); Froglok and Drakkin **0–3**
  (`:1913–1917`); **—** for **Erudite** (both genders), Troll M, Ogre M, Iksar and Vah Shir.
- **Beard** — `IsValidBeard` (`:1519–1584`; Luclin branch `:1525–1563`): Dwarf F **0–1** (`:1527–1528`); High Elf M,
  Dark Elf M, Half Elf M and Drakkin F **0–3** (`:1533–1537`); Human M, Barbarian M, Erudite M,
  Dwarf M, Halfling M, Gnome M **0–5** (`:1542–1548`); Drakkin M **0–11** (`:1553–1554`); **—** for
  every other race/gender.
- **Beard color** — `IsValidBeardColor` (`:1586–1637`): Gnome M **0–24** (`:1593–1594`); Human M, Barbarian M, Erudite M,
  Half Elf M, Dwarf M, **Dwarf F** and Halfling M **0–19** (`:1599–1606`); Dark Elf M **13–18**
  (`:1611–1612`); High Elf M **0–14** (`:1617–1618`); Froglok and Drakkin, both genders, **0–3**
  (`:1623–1627`); **—** for every other race/gender.
- **Drakkin only** — the three extra fields in the RoF2 create struct (Section 4): heritage **0–7**
  (`IsValidHeritage`, `:1929–1948`), tattoo **0–7** (`IsValidTattoo`, `:1950–1969`), details **0–7**
  (`IsValidDetail`, `:1639–1658`). No other race has a valid value for any of them. These are the
  three `uint32` ones, so their sentinel is `0xFFFFFFFF`.
- **Barbarian only** — woad **0–8**, `IsValidWoad`, Luclin branch only (`:1971–1992`). Not a field in
  the create struct; listed so the absence is deliberate rather than an omission.

> ⚠ **Do not infer one attribute's race set from another's — two pairs cross.** Erudite has **no**
> valid hair colour yet Erudite M has beard colour 0–19, so "beard colour follows the hair-colour
> race set" is false. Froglok has **no** hairstyle yet has beard colour 0–3. Earlier revisions of
> this section stated the first of those as a rule and gave face as "0–7 (all races)", which stopped
> being true the moment this document corrected the creatable set from fourteen races to sixteen.

### Name rules (`Client::HandleNameApprovalPacket`, `world/client.cpp:567`)
Checked in order, first failure wins: length 4–15 (`:601`), first character not lower-case (`:603`),
no space anywhere (`:605`), then `Database::CheckNameFilter` (`:607`), then no upper-case character
after the first (`:611`), then uniqueness via `ReserveName` (`:619`). `CheckNameFilter`
(`common/database.cpp:830`) is where the rest live: alphabetic-only (`:843`), no **3** identical
consecutive characters (`:858`, the test is `num_c > 2`), and a case-insensitive substring match
against every row of the `name_filter` table (`:870`). Reply is the same opcode with a 1-byte body,
`0x01`=ok / `0x00`=reject (`:622–624`).

`HandleNameApprovalPacket` contains **no client-version branch** — unlike the stat and combo checks
above, these rules are the same for Titanium and for RoF2. Only the opcode number differs:

> ⚠ **The opcode value is per-client.** RoF2 is `0x56a2` (`utils/patches/patch_RoF2.conf:39`), which
> is what eqoxide sends (`crates/eqoxide-protocol/src/protocol/mod.rs:104`); `0x3ea6` is the
> **Titanium** value (`utils/patches/patch_Titanium.conf:27`).

### Default stat pre-spend — RoF2/SoF path only, UI seed (`char_create_point_allocations`, `utils/sql/svn/2024_required_update.sql:655`)
**This is per combo, not per class.** The pre-spend is the `alloc_str`…`alloc_cha` columns of the
`char_create_point_allocations` row named by the matched combo's `allocation_id`, which the server
looks up by `AllocationIndex` (`world/client.cpp:1927–1941`). EQEmu's tracked seed carries the table
definition at `utils/sql/svn/2024_required_update.sql:655` and **109** rows, ids 0–108, at
`:674–782` — the same file the deity section above cites for `char_create_combinations`.

Projecting the seed's 641 combo rows onto the allocation row each names gives, per class:

```
Warrior     STA+25  or  STR+7/STA+18      Rogue        STR+25/DEX+5
Cleric      WIS+25/STA+5                  Shaman       WIS+25/STA+5
Paladin     STA+20                        Necromancer  INT+25/STA+5
Ranger      DEX+20                        Wizard       INT+25/STA+5
ShadowKnight STA+20                       Magician     INT+25/STA+5
Druid       WIS+25/STA+5                  Enchanter    CHA+25/INT+5
Monk        AGI+20                        Beastlord    STA+5/AGI+5/DEX+5/WIS+5
Bard        CHA+25                        Berserker    STR+25  or  STR+10/STA+15
```

Warrior and Berserker are the only two classes whose combos span more than one allocation vector, so
they are the only two for which a single per-class default is not well defined; drive the UI off
`allocation_id` and the per-class column above is only a sanity check.

> ⚠ **This corrects an earlier revision of this section**, which gave `Cleric→WIS(+STR)` (the second
> stat is STA, not STR), `Enchanter→INT(+CHA)` (reversed — it is CHA+25 with INT+5),
> `Beastlord→WIS` (it is an even +5 across STA, AGI, DEX and WIS) and `Berserker→STA` (it is STR+25,
> or STR+10/STA+15). Those four were unsourced and wrong; the values above are derived from the seed
> rows cited here.

**A UI choice, not a server rule:** this document's create screen starts with gender male and every
appearance field at 0. EQEmu neither requires nor supplies that — it is the only thing under this
heading that is not read off an EQEmu source, and 0 is not necessarily a value the appearance
validator accepts (see *Appearance ranges* above).

## Section 4 — Wire formats

⚠ **Opcode values below are RoF2**, from `utils/patches/patch_RoF2.conf` — `OP_SendCharInfo` `:20`,
`OP_CharacterCreate` `:36`, `OP_DeleteCharacter` `:37`, `OP_ApproveName` `:39`. Three of the four
match a constant eqoxide already defines: `OP_SEND_CHAR_INFO`, `OP_APPROVE_NAME` and
`OP_CHARACTER_CREATE` at `crates/eqoxide-protocol/src/protocol/mod.rs:103–105`. **`OP_DeleteCharacter`
has no constant in the Rust sources at all** — it is unimplemented work (Section 2, item 6), so its
value here is cited to the patch file only and has never been exercised against a server. Earlier
revisions of this section gave the **Titanium** values for all four (`0x4513`/`0x10b2`/`0x26c9`/`0x3ea6`,
`patch_Titanium.conf:22`/`:24`/`:21`/`:27`) in a document that targets RoF2; every opcode literal in
this spec now says which client it belongs to.
- **`OP_ApproveName` (RoF2 `0x56a2`), 72B, C→S.** ⚠ Layout discrepancy to resolve at implementation: the
  knowledgebase/expert describes `race_id u32, gender u32, name[64]`, but the **live-verified Mordeth
  code** (`build_approve_name` in `login.rs`) uses `name[64] @0, race u32 @64, class u32 @68` and the
  server accepted it (created "Mordeth" with the correct name). **Trust the working layout** (name at
  offset 0); only the name + race materially matter to the server. Re-verify if changing.
- **`OP_CharacterCreate` (RoF2 `0x6bbf`), 96B (24 LE u32), C→S.** ⚠ The 80B/20-u32 Titanium layout
  below is NOT what we send — the live `build_char_create` (`login.rs`) emits the RoF2 96-byte
  struct in order: gender, race, class, deity, **start_zone (zone_id)**, haircolor, beard,
  beardcolor, hairstyle, face, eyecolor1, eyecolor2, drakkin_heritage/tattoo/details, STR, STA,
  AGI, DEX, WIS, INT, CHA, tutorial, unknown0092. (Titanium order was: class, haircolor, beardcolor,
  beard, gender, race, start_zone, hairstyle, deity, STR..CHA, face, eyecolor1/2, tutorial.) Success
  = server resends `OP_SendCharInfo`; failure = `OP_ApproveName{0}`.
- **`OP_SendCharInfo` (RoF2 `0x00d2`), 1704B fixed, S→C.** 10 fixed slots (Titanium hard-caps at 8 but emits
  10); empty slot `Name == "<none>"`. Struct-of-arrays layout (offsets in the knowledgebase doc): per
  slot Race, Class, Level, Zone, Gender, Face, HairStyle/HairColor/Beard/BeardColor, EyeColor1/2,
  Deity, the 9-slot Equip material array + 9-slot color array, Primary/SecondaryIDFile, Name[64].
  Parse all 10, skip `<none>`. Equip/colors feed the char-select 3D model (if added there later).
- **`OP_DeleteCharacter` (RoF2 `0x1808`), C→S.** Body = null-terminated character name only (no struct).
  Server verifies ownership, deletes, and replies with a fresh `OP_SendCharInfo` (no separate ack) —
  client re-parses that to refresh the list.

## Implementation staging (when resumed)

1. **Create-rules data module + tests** (`chardata.rs`) — pure data/logic, no deps, lowest risk.
2. **Phase enum + Login screen + interactive auth + CharSelect (text) + Enter World** — gets the
   no-`--config` path working end-to-end with an existing character; includes the startup reorder and
   command-slot channel (the broad/conflict-prone part).
3. **CharCreate screen (full rules + stat allocation) + Delete** — uses the data module + the create
   handshake already in `login.rs`.
4. **Live 3D preview on the create screen** — highest-risk renderer integration; stage last.

## Open items / risks

- **Startup reorder breadth** — touches `main.rs` init ordering, asset-sync timing, and the network
  thread's lifecycle; the reason implementation is deferred until in-flight work lands.
- **`NameApproval` layout discrepancy** — see Section 4; trust the live-verified code.
- **Live preview** — needs an offscreen render-to-texture path into egui; confirm the renderer can
  pose a single character model cheaply on demand.
- **`--config` regression guard** — the interactive path must not alter the existing config-driven
  auto-login (keep a test/launch check for both paths).
