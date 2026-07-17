# OP_Consider — wire struct, ConsiderColor tiers, and native client presentation (eqoxide#336)

## 1. `Consider_Struct` — 20 bytes, confirmed for RoF2

`EQEmu/common/patches/rof2_structs.h:1531-1539`:

```c
struct Consider_Struct{
/*000*/ uint32  playerid;
/*004*/ uint32  targetid;
/*008*/ uint32  faction;   // FACTION_VALUE (attitude), 1..9
/*012*/ uint32  level;     // ConsiderColor enum value, NOT a raw mob level
/*016*/ uint8   pvpcon;    // 0 normal, 1 = PVP-flagged target in a PVP zone, 4 = raid-target special case
/*017*/ uint8   unknown017[3];
/*020*/
};
```

**eqoxide's assumed 20-byte layout (playerid/targetid/faction/level) is correct** for
both `OP_Consider` and `OP_ConsiderCorpse` (the corpse variant DECODE_FORWARDs to the
same struct: `EQEmu/common/patches/rof2.cpp:5427`). ENCODE/DECODE are both
`ENCODE_LENGTH_EXACT`/`DECODE_LENGTH_EXACT(Consider_Struct)` — `rof2.cpp:1109-1114`,
`rof2.cpp:5411-5414` — no variable-length tail, no cur_hp/max_hp fields. **There is no
HP data in this struct at all.** Byte 16 is `pvpcon` (single byte + 3 bytes padding),
not part of `level`. If eqoxide's Rust struct currently reads a `pvpcon`-shaped u32 as
something else past byte 16, that's the field to fix, but there's nothing past byte 20.

Opcodes (`EQEmu/utils/patches/patch_RoF2.conf`):
- `OP_Consider=0x742b` (line 183)
- `OP_ConsiderCorpse=0x5204` (line 213)

## 2. `level` field IS the `ConsiderColor` enum — confirmed

Server assignment: `EQEmu/zone/client_packet.cpp:5153`
```cpp
con->level = GetLevelCon(t->GetLevel());
```
`GetLevelCon` returns one of the `ConsiderColor` constants
(`EQEmu/common/emu_constants.h:486-495`):
```cpp
namespace ConsiderColor {
    constexpr uint32 Green         = 2;
    constexpr uint32 DarkBlue      = 4;
    constexpr uint32 Gray          = 6;
    constexpr uint32 White         = 10;
    constexpr uint32 Red           = 13;
    constexpr uint32 Yellow        = 15;
    constexpr uint32 LightBlue     = 18;
    constexpr uint32 WhiteTitanium = 20;
};
```
These are exactly the numbers eqoxide already assumed. `client_packet.cpp:5155-5161`
remaps `Gray→Green` and `White→WhiteTitanium` **only** `if
(ClientVersion() <= EQ::versions::ClientVersion::Titanium)` — **irrelevant for RoF2**,
do not apply that remap.

`Mob::GetLevelCon(uint8 mylevel, uint8 iOtherLevel)` — `EQEmu/zone/mob_ai.cpp:2156-2369`.
Two algorithms, selected by `RuleB(Character, UseOldConSystem)`
(`EQEmu/common/ruletypes.h:172`, **default `false`** — i.e. RoF2 uses the "new"
branch below unless a server operator overrides the rule):

New system (`mob_ai.cpp:2323-2367`, default):
```cpp
int16 diff = iOtherLevel - mylevel;               // target level - my level
uint32 conGrayLvl  = mylevel - (mylevel + 5) / 3;
uint32 conGreenLvl = mylevel - (mylevel + 7) / 4;

if (diff == 0)                    return White;
if (diff >= 1 && diff <= 3)       return Yellow;
if (diff >= 4)                    return Red;

if (mylevel <= 15) {
    return (diff <= -6) ? Gray : DarkBlue;
} else if (mylevel <= 20) {
    if (iOtherLevel <= conGrayLvl)       return Gray;
    else if (iOtherLevel <= conGreenLvl) return Green;
    else                                  return DarkBlue;
} else {
    if (iOtherLevel <= conGrayLvl)       return Gray;
    else if (iOtherLevel <= conGreenLvl) return Green;
    else if (diff <= -6)                 return LightBlue;
    else                                  return DarkBlue;
}
```
Old system (`mob_ai.cpp:2156-2322`, `UseOldConSystem=true` only) uses a per-`mylevel`-
bracket table of `diff` cutoffs (e.g. `mylevel<=8`: Gray at `diff<=-4` else DarkBlue;
`mylevel<=55`: Gray at `diff<=-20`, LightBlue at `diff<=-15`, else DarkBlue; ... — see
`mob_ai.cpp:2160-2322` for the full per-bracket table if a target server has this rule
on). Same-level always `White`, `diff 1-2`→Yellow, `diff>=3`→Red in this branch (vs.
1-3/>=4 in the new branch).

Name→value table: `EQEmu/common/emu_constants.cpp:391-405`
(`GetConsiderColorMap`/`GetConsiderColorName`) — `Green/"Green"`, `DarkBlue/"Dark Blue"`,
`Gray/"Gray"`, `White/"White"`, `Red/"Red"`, `Yellow/"Yellow"`, `LightBlue/"Light Blue"`,
`WhiteTitanium/"White"`.

## 3. Attitude (`faction` byte, 1..9) — the wire value is SWAPPED from the enum

`EQEmu/common/faction.h:27-35`:
```cpp
enum FACTION_VALUE {
    FACTION_ALLY=1, FACTION_WARMLY=2, FACTION_KINDLY=3, FACTION_AMIABLY=4,
    FACTION_INDIFFERENTLY=5, FACTION_APPREHENSIVELY=6, FACTION_DUBIOUSLY=7,
    FACTION_THREATENINGLY=8, FACTION_SCOWLS=9
};
```
Before sending, the server swaps two pairs (`client_packet.cpp:5184-5192`):
`APPREHENSIVELY(6)↔SCOWLS(9)`, `DUBIOUSLY(7)↔THREATENINGLY(8)`. So the **wire** byte for
a mob that's actually going to attack you (`FACTION_SCOWLS=9` internally) arrives as
`6`, etc. Client-side attitude text table (`eqstr_us.txt:5538-5546`, ids 12212-12220,
matched 1:1 against wire value 1..9 in `eqgame.exe.c:176724-176808`,
`FUN_00522e20` — see below):

| wire faction | eqstr id | text |
|---|---|---|
| 1 | 12212 | regards you as an ally |
| 2 | 12213 | looks upon you warmly |
| 3 | 12214 | kindly considers you |
| 4 | 12215 | judges you amiably |
| 5 | 12216 | regards you indifferently |
| 6 | 12217 | scowls at you, ready to attack |
| 7 | 12218 | glares at you threateningly |
| 8 | 12219 | glowers at you dubiously |
| 9 | 12220 | looks your way apprehensively |

(If eqoxide's attitude text already matches this table you're fine; this is included
for completeness/cross-check since #336 said attitude was already working.)

## 4. What the native RoF2 client actually PRINTS — confirmed via decompile + raw jump-table extraction

**The native client does NOT convey difficulty by color alone.** It builds and prints
**one single chat line** containing BOTH an attitude clause AND a separate idiomatic
difficulty-assessment clause, joined by `" -- "`, and colors the **whole line** with
the ConsiderColor.

Function: `FUN_00522e20` (`eqgame.exe.c:176633`), the only caller of which is the
consider-result path (`eqgame.exe.c:176970`, `FUN_005239a0`). Traced with
`param_1`=self client, `param_2`=target's resolved Spawn object, `param_3`=the raw
`Consider_Struct` packet pointer (confirmed: it only ever reads `param_3+8` = faction,
`eqgame.exe.c:176673`, and `param_3+0x10` = pvpcon, `eqgame.exe.c:176809/176890/176945`
— **it never reads `param_3+0xC`, i.e. never reads the wire `level`/ConsiderColor
field** — see §5).

Flow inside `FUN_00522e20`:
1. `eqgame.exe.c:176697-176723` — picks a subject pronoun ("he"/"she"/"it", eqstr
   12209-12211) based on a gender byte on the target object.
2. `eqgame.exe.c:176724-176808` — picks the attitude clause (table in §3) from the wire
   `faction` byte, with a `faction==5` fallback for the "indifferent" case
   (`LAB_00523074`, eqstr 12216).
3. `eqgame.exe.c:176809-176866` — special-cased text for `pvpcon==4` (raid-target /
   "army to defeat" flavor, unrelated to normal considers).
4. `eqgame.exe.c:176867-176889` — computes the **print color** `iVar6` via
   `switch(FUN_00577cb0(param_2))`: case 2→**2**(Green), case 3→**0x12=18**(LightBlue),
   case 4→**4**(DarkBlue), case 5→**10**(White), case 6→**0xf=15**(Yellow),
   case 7→**0xd=13**(Red), default→**6**(Gray). These are exactly the `ConsiderColor`
   numeric values.
5. `eqgame.exe.c:176890-176920` — for a normal (`pvpcon==0`) NPC consider: branches on
   the **player's own level** into 3 brackets (`<15` @176892, `<25` @176903, else
   @176912) and, within each bracket, dispatches on `iVar6` (the color from step 4) to
   pick a **difficulty-assessment prose sentence**. Ghidra couldn't resolve this dispatch
   as a normal switch ("Could not recover jumptable ... Too many branches ... Treating
   indirect jump as call") — I extracted the real x86 jump tables directly from
   `everquest_rof2/eqgame.exe` with a `pefile` script (jump instruction at
   `capstone/eqgame.exe.asm:385396`, `jmp dword ptr [ecx*4 + 0x5238fc]`) and disassembled
   each case target in `capstone/eqgame.exe.asm`. Confirmed real targets/strings
   (partial table, i.e. this is what I could confirm, not necessarily 100% exhaustive of
   every level-difference sub-branch — several color buckets have a further internal
   `diff`-threshold split, e.g. `eqgame.exe.c`-equivalent code at asm `0x5234df`/`0x5235c5`
   compares the raw level difference against -5/-8/-10 to choose between two adjacent
   strings):

   | player level bracket | color(iVar6) | eqstr id | text |
   |---|---|---|---|
   | <15  | Green(2)     | 12227 | looks like you would have the upper hand. |
   | <15  | DarkBlue(4)  | 12228 | looks kind of risky, but you might win. |
   | <15  | Gray(6)      | 12226 | looks like a reasonably safe opponent. |
   | <15  | White(10)    | 12229\* | looks like an even fight. (\*sub-branch on diff>=8 picks a different string, not fully resolved) |
   | <15  | Red(13)      | 12225 | what would you like your tombstone to say? |
   | <15  | Yellow(15)   | 12231 | looks like quite a gamble. |
   | <15  | LightBlue(18)| 12227 | looks like you would have the upper hand. |
   | 15-24| Green(2)     | 12232 | You would probably win this fight... it's not certain though. |
   | 15-24| DarkBlue(4)  | 12234 | looks quite risky, but might be worth a try. |
   | 15-24| Gray(6)      | 12232/12226\* | sub-branch on diff>=-5 |
   | 15-24| White(10)    | 12235 | %1 appears to be quite formidable. |
   | 15-24| Red(13)      | 12225 | what would you like your tombstone to say? |
   | 15-24| Yellow(15)   | 12231 | looks like quite a gamble. |
   | 15-24| LightBlue(18)| 12233 | looks kind of dangerous. |
   | ≥25  | Green(2)     | 12232 | You would probably win this fight... it's not certain though. |
   | ≥25  | Gray(6)      | 12236/12237\* | sub-branch on diff>=-10 |
   | ≥25  | White(10)    | 12231 | looks like quite a gamble. |
   | ≥25  | Red(13)      | 12225 | what would you like your tombstone to say? |
   | ≥25  | Yellow(15)   | 12238 | looks like %1 would wipe the floor with you! |
   | ≥25  | LightBlue(18)| 12233 | looks kind of dangerous. |

   (`eqstr_us.txt:5549-5565`, ids 12223-12238, is the full source pool of these
   difficulty phrases; `12225` = "what would you like your tombstone to say?" is a joke
   line reused for Red at every bracket in this table.)

6. `eqgame.exe.c:176921-176935` — for a **PVP-target** consider (`pvpcon!=0`, only
   possible when `zone->IsPVPZone()` and the target is a PVP-flagged player,
   `client_packet.cpp:5163-5166`), a much simpler 3-way pick is used instead
   (eqstr 12223/12224/12225 for `iVar6==2/10/13`).
7. `eqgame.exe.c:176936-176944` — **final assembly and print**: builds the combined
   line with template eqstr **12239 = `"%1 %2 -- %3"`** (`eqstr_us.txt:5565`) — arg 2 =
   the attitude clause from step 2, arg 3 = the difficulty clause from step 5/6 — then
   prints it **once**, in color `iVar6`, via `FUN_0051f1a0(&uStack_200, iVar6, 1, 1)`
   (`eqgame.exe.c:176944`).
8. `eqgame.exe.c:176945-176949` — an *additional* line ("This creature would take an
   army to defeat!") is appended only for `pvpcon==4` raid targets, also colored via
   the raid-target color table at `client_packet.cpp:5198-5228`.

**Net result, for an ordinary NPC consider**: one colored chat line shaped like
`"<Name> <attitude phrase> -- <difficulty phrase>"`, e.g.
`"a decaying skeleton glares at you threateningly -- looks kind of risky, but you might win."`,
entirely in the ConsiderColor's color (green/darkblue/gray/white/red/yellow/lightblue).
**There is no bare color-name label anywhere ("red"/"gray"/etc. are never printed as
words)** — the difficulty is conveyed by BOTH the line color AND an idiomatic English
clause, never a literal tier name.

## 5. Client re-derives the color independently of the wire `level` field (confirmed, narrow claim)

`FUN_00522e20` never reads packet offset `+0xC` (`Consider_Struct.level`). The print
color `iVar6` instead comes from `FUN_00577cb0(param_2)` (`eqgame.exe.c:228014`), which
takes the **target spawn object** (not the packet) and independently recomputes a
con-color from `self.level` vs. `target.GetLevel()` using its own threshold table gated
by a global `DAT_00dcee70` (an internal ruleset/era selector, not investigated further
here) — structurally parallel to, but a separate implementation from, the server's
`Mob::GetLevelCon`. Since the client already has the mob's real level (from
`Spawn_Struct`) this reconstruction produces the same practical result as trusting the
wire `level` field, so this is a "how the retail client happens to work" curiosity, not
a reason for eqoxide to avoid using the server-provided `Consider_Struct.level` — it's
computed by the exact same algorithm server-side and is simpler to consume.

## Recommendation for eqoxide (#336)

- **Struct**: keep the 20-byte `Consider_Struct` as-is (playerid/targetid/faction/level
  @0/4/8/12); there is no HP data — drop any assumption of trailing cur_hp/max_hp
  fields. Add `pvpcon: u8` (+3 pad) @16 if not already present (harmless to ignore its
  value for a standalone PvE consider).
- **`level` field**: treat it as the `ConsiderColor` enum exactly as eqoxide already
  does (2/4/6/10/13/15/18/20 = Green/DarkBlue/Gray/White/Red/Yellow/LightBlue/
  WhiteTitanium) — confirmed correct, do not apply the Titanium Gray→Green/White→
  WhiteTitanium remap (server already skips it for RoF2 clients).
- **Difficulty tier IS faithful RoF2 behavior to show** — the real client shows it as
  BOTH a text color AND an explicit prose clause (never a bare "red"/"gray" word). For
  #336's "standalone consider reports faction but never conveys difficulty" gap, the
  most faithful fix is: color the whole consider chat line by the `ConsiderColor` (this
  alone matches ~80% of the native behavior and is cheap/robust), and — if going for full
  fidelity — append a short difficulty clause. Given the native phrase table is large,
  keyed by (player level bracket × color × sometimes fine level-diff sub-thresholds) and
  only partially reverse-engineered here (§4 table has a few `*` unresolved sub-branches),
  a pragmatic middle ground for eqoxide is a **simplified fixed clause per ConsiderColor**
  (not literally matching retail's exact per-bracket wording) rather than committing to
  reproducing the full idiomatic table — call this out explicitly as "simplified, not
  verbatim retail text" in code comments/PR description so it isn't later assumed to be
  a verified 1:1 match.
- **Color mapping for eqoxide's UI/chat color enum**: Green=2, DarkBlue=4, Gray=6,
  White=10, Red=13, Yellow=15, LightBlue=18 (WhiteTitanium=20 is Titanium-only, will
  never arrive from an RoF2-patched server).
- **Edge case**: `faction==FACTION_INDIFFERENTLY(5)` on the wire is also the fallback
  used whenever the client can't otherwise classify (see `LAB_00523074` /
  `eqgame.exe.c:176724`); nothing eqoxide needs to special-case beyond the existing 1..9
  table in §3 (which is presumably already implemented, per the issue description).

## Reproduction notes
- Jump-table extraction script (ad hoc, not saved to a permanent path): `pefile`-based
  read of `everquest_rof2/eqgame.exe` at RVA `0x5238fc-0x400000` /
  `0x523930-0x400000` / `0x523964-0x400000` (pointer arrays) and
  `0x523918-0x400000` / `0x52394c-0x400000` / `0x523980-0x400000` (byte case-index
  tables), then cross-referenced each resolved code address against
  `capstone/eqgame.exe.asm` to find the `push 0x2fXX; call 0x7d0660` (i.e.
  `GetEQStr(id)`) pattern. Re-derivable in ~10 min if the exact full table (including
  the `*`-marked sub-branches) is ever needed.

## Related
- `eqstr-nested-string-ids.md` — `%T<n>` nested string-id resolution in eqoxide's
  `eqstr.rs`; `FUN_007d0660` in the client is the native-side equivalent of
  "resolve eqstr id → template string" (`GetEQStr`).
