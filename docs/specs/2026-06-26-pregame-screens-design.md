# Design: Pre-game UI — Login / Character-Select / Character-Create screens

**Date:** 2026-06-26
**Status:** Design approved-in-progress; **implementation deferred** (may conflict with in-flight
work — startup reorder is broad). Resume from this doc.
**Branch context:** drafted on `worktree-mordeth`, which already added the client-side
character-creation handshake (commit `8a03a15`) that this UI builds on.

## Goal

When the client is launched **without `--config`**, present interactive **Login → Character-Select
→ Character-Create** screens that follow the same rules the native Titanium client enforces, instead
of auto-logging-in from a config file. With `--config <name>` the current non-interactive behavior is
unchanged.

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
6. **`OP_DeleteCharacter`** (`0x26c9`).
7. **Live 3D preview** (`src/pregame/preview.rs`): render the selected race/gender/face/hair model to
   an offscreen texture exposed to egui. Reuse `render_model` / renderer model-loading. Re-render
   only on input change. **Highest-risk integration — stage last.**

## Section 3 — Native rules data (ground truth from eq-client-expert, cited)

Full detail + citations in `docs/eq-technical-knowledgebase/character-creation.md`. Summary tables:

### Race IDs
Human=1, Barbarian=2, Erudite=3, Wood Elf=4, High Elf=5, Dark Elf=6, Half Elf=7, Dwarf=8, Troll=9,
Ogre=10, Halfling=11, Gnome=12, Iksar=128, Vah Shir=130. (Froglok=330/Drakkin=522 exist in tables but
are not Titanium-creatable — hide.)

### Race/class validity matrix (`ClassRaceLookupTable`, EQEmu `world/client.cpp:2053`)
```
              Hum Bar Eru  WE  HE  DE HlfE Dwr Trl Ogr Hlf Gno Iks VaS
Warrior  (1)   Y   Y   -   Y   -   Y   Y   Y   Y   Y   Y   Y   Y   Y
Cleric   (2)   Y   -   Y   -   Y   Y   Y   Y   -   -   Y   Y   -   -
Paladin  (3)   Y   -   Y   -   Y   -   Y   Y   -   -   Y   Y   -   -
Ranger   (4)   Y   -   -   Y   -   -   Y   -   -   -   Y   -   -   -
SK       (5)   Y   -   Y   -   -   Y   -   -   Y   Y   -   Y   Y   -
Druid    (6)   Y   -   -   Y   -   -   Y   -   -   -   Y   -   -   -
Monk     (7)   Y   -   -   -   -   -   -   -   -   -   -   -   Y   -
Bard     (8)   Y   -   -   Y   -   -   Y   -   -   -   -   -   -   Y
Rogue    (9)   Y   Y   -   Y   -   Y   Y   Y   -   -   Y   Y   -   Y
Shaman  (10)   -   Y   -   -   -   -   -   -   Y   Y   -   -   Y   Y
Necro   (11)   Y   -   Y   -   -   Y   -   -   -   -   -   Y   Y   -
Wizard  (12)   Y   -   Y   -   Y   Y   -   -   -   -   -   Y   -   -
Mage    (13)   Y   -   Y   -   Y   Y   -   -   -   -   -   Y   -   -
Enchant (14)   Y   -   Y   -   Y   Y   -   -   -   -   -   Y   -   -
Beastlord(15)  -   Y   -   -   -   -   -   -   Y   Y   -   -   Y   Y
Berserker(16)  -   Y   -   -   -   -   -   Y   Y   Y   -   -   -   Y
```

### Race base stats — order STR/STA/AGI/DEX/WIS/INT/CHA (`world/client.cpp:2013`)
```
Human      75 75 75 75 75 75 75      Dwarf     90 90 70 90 83 60 45
Barbarian 103 95 82 70 70 60 55      Troll    108 109 83 75 60 52 40
Erudite    60 70 70 70 83 107 70     Ogre     130 122 70 70 67 60 37
Wood Elf   65 65 95 80 80 75 75      Halfling  70 75 95 90 80 67 50
High Elf   55 65 85 70 95 92 80      Gnome     60 70 85 85 67 98 60
Dark Elf   60 65 90 75 83 99 60      Iksar     70 70 90 85 80 75 55
Half Elf   70 70 90 85 60 75 75      Vah Shir  90 75 90 70 70 65 65
```

### Class stat bonuses + bonus-point pool — STR/STA/AGI/DEX/WIS/INT/CHA/POINTS (`world/client.cpp:2033`)
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

### Stat validation (`world/client.cpp:2104`, Titanium) — must hold EXACTLY
```
base[s] = RaceBase[race][s] + ClassBonus[class][s]
pool    = ClassBonus[class].POINTS
sent[s] >= base[s]                      (per stat)
sent[s] <= base[s] + pool               (per stat)
sum(sent) == sum(base) + pool           (EXACT — Create disabled while points remain)
```
Deity and appearance are **not** server-validated on Titanium (`client.cpp:2159`) but the UI should
still constrain them to native choices.

### Deity IDs (`deity.h`)
Agnostic=396, Bertoxxulous=201, BrellSerilis=202, Cazic-Thule=203, ErollisiMarr=204, Bristlebane=205,
Innoruuk=206, Karana=207, MithanielMarr=208, Prexus=209, Quellious=210, RallosZek=211, RodcetNife=212,
SolusekRo=213, TheTribunal=214, Tunare=215, Veeshan=216. (Per-class deity lists in the knowledgebase
doc; deity not enforced server-side, so the per-class list is a UI nicety.)

### StartZoneIndex (the `start_zone` wire value, 0–13) and per-race start cities
```
0 Odus(erudnext; paineel if deity=203)  7 Oggok          | Human    1,4      Dwarf    8
1 Qeynos(qeynos2)                        8 Kaladim        | Barb     2        Troll    6
2 Halas                                  9 GreaterFaydark | Erudite  0        Ogre     7
3 Rivervale                             10 Felwithe       | Wood Elf 9        Halfling 3
4 Freeport(freportw)                    11 Akanon         | High Elf 10       Gnome    11
5 Neriak(neriaka)                       12 Cabilis(cabwest)| Dark Elf 5       Iksar    12
6 Grobb                                 13 Shar Vahl      | Half Elf 1,4,9    Vah Shir 13
```

### Appearance ranges (`races.cpp`; not server-validated, UI guidance)
- **Face:** 0–7 (all races).
- **Eye color 1/2:** 0–9 (Troll 0–10).
- **Hairstyle:** 0–3 most races; Erudite M 0–5, Erudite F 0–8; Troll/Ogre males, Iksar, Vah Shir = no
  hair (send 0).
- **Hair color:** 0–19 (Human/Barb/WoodElf/HalfElf/Dwarf/Halfling); High Elf 0–14; Dark Elf 13–18;
  Gnome 0–24; Troll/Ogre F 0–23; Troll/Ogre M, Iksar, Vah Shir = 0.
- **Beard:** Human/Barb/Erudite/Dwarf/Halfling/Gnome M 0–5; HighElf/DarkElf/HalfElf M 0–3; Dwarf F
  0–1; all others 0.
- **Beard color:** same race set as hair color where beards exist, else 0.

### Name rules (server-enforced at `OP_ApproveName`)
4–15 chars, alphabetic only, first char uppercase / rest lowercase, no spaces, no 3 identical
consecutive chars, server `name_filter` substring check, uniqueness via `ReserveName`. Reply is the
same opcode `0x3ea6` with a 1-byte body (`0x01`=ok, `0x00`=reject).

### Default stat pre-spend (UI seed; player may redistribute) — `char_create_point_allocations`
Warrior→STA, Cleric→WIS(+STR), Paladin→STA, Ranger→DEX, SK→STA, Druid→WIS(+STA), Monk→AGI, Bard→CHA,
Rogue→STR(+DEX), Shaman→WIS(+STA), Necro→INT(+STA), Wizard→INT(+STA), Mage→INT(+STA), Enchanter→INT(+CHA),
Beastlord→WIS, Berserker→STA. Appearance defaults all 0, gender male.

## Section 4 — Wire formats

- **`OP_ApproveName` (0x3ea6), 72B, C→S.** ⚠ Layout discrepancy to resolve at implementation: the
  knowledgebase/expert describes `race_id u32, gender u32, name[64]`, but the **live-verified Mordeth
  code** (`build_approve_name` in `login.rs`) uses `name[64] @0, race u32 @64, class u32 @68` and the
  server accepted it (created "Mordeth" with the correct name). **Trust the working layout** (name at
  offset 0); only the name + race materially matter to the server. Re-verify if changing.
- **`OP_CharacterCreate` (0x10b2), 80B, C→S.** 20 LE u32 in order: class, haircolor, beardcolor,
  beard, gender, race, start_zone, hairstyle, deity, STR, STA, AGI, DEX, WIS, INT, CHA, face,
  eyecolor1, eyecolor2, tutorial. (Already implemented as `build_char_create`.) Success = server
  resends `OP_SendCharInfo`; failure = `OP_ApproveName{0}`.
- **`OP_SendCharInfo` (0x4513), 1704B fixed, S→C.** 10 fixed slots (Titanium hard-caps at 8 but emits
  10); empty slot `Name == "<none>"`. Struct-of-arrays layout (offsets in the knowledgebase doc): per
  slot Race, Class, Level, Zone, Gender, Face, HairStyle/HairColor/Beard/BeardColor, EyeColor1/2,
  Deity, the 9-slot Equip material array + 9-slot color array, Primary/SecondaryIDFile, Name[64].
  Parse all 10, skip `<none>`. Equip/colors feed the char-select 3D model (if added there later).
- **`OP_DeleteCharacter` (0x26c9), C→S.** Body = null-terminated character name only (no struct).
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
