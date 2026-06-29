# Player currency decodes to garbage values (RoF2 player profile)

**Summary:** A freshly created character with **zero coin** reports nonsense currency from the
client — `platinum: 2147483648` (0x80000000) and `gold: 119012671` — in both `/v1/observe/debug` and
`/v1/observe/inventory`, while the DB has no `character_currency` row (i.e. 0/0/0/0).

**Severity:** Medium (coin is unusable/misleading; would break any buy/sell budgeting logic)

**Zone / area:** Player profile decode (`OP_PlayerProfile`) — currency fields, RoF2 layout.

## Steps to reproduce
1. Create a fresh character (e.g. Brogan, Human Warrior) — see
   `.claude/skills/eq-character-creation`. New chars get no `character_currency` row.
2. Zone in, then read currency:
   - `curl -s "http://127.0.0.1:$PORT/v1/observe/debug"   | python3 -c "import sys,json;print(json.load(sys.stdin)['player']['currency'])"`
   - `curl -s "http://127.0.0.1:$PORT/v1/observe/inventory"| python3 -c "import sys,json;print(json.load(sys.stdin)['currency'])"`

## Expected
`{'copper': 0, 'silver': 0, 'gold': 0, 'platinum': 0}` (matches the DB — no currency row).

## Actual
`{'copper': 0, 'silver': 0, 'gold': 119012671, 'platinum': 2147483648}` from both endpoints.
DB truth: `SELECT platinum,gold,silver,copper FROM character_currency WHERE id=16;` → no row (NULL/0).

## Diagnosis notes
- Both `/v1/observe/debug` and `/v1/observe/inventory` share one source (the decoded player profile), so the error is in
  the profile decode, not the HTTP layer.
- `copper` and `silver` read 0 (correct); only `gold` and `platinum` are garbage — suggests the
  currency field **offsets** into the RoF2 `PlayerProfile_Struct` are off, so platinum/gold read
  bytes from an adjacent (wrong) region. `platinum = 0x80000000` looks like a sign/top-bit field,
  not real coin.
- This is the RoF2-migration analogue of the older (fixed) Titanium currency issue
  (`inventory-api-missing-currency.md`); that one was missing values, this one is wrong values.

## Root cause (confirmed)
The currency offsets came from `rof2_structs.h` `PlayerProfile_Struct` (`/*12869*/ platinum`),
but RoF2 does **not** send that flat struct — it *streams* the profile via
`rof2.cpp ENCODE(OP_PlayerProfile)` (a sequence of `outapp->WriteUIntXX(...)`). The struct's
offset comments are only accurate up to `disciplines`: ENCODE writes
`structs::MAX_PP_DISCIPLINES = 300` discipline entries, but the struct reserves only 200
(`/*05124*/ disciplines`, 800 bytes) — a 100-entry / **+400-byte** undercount. So every field
*after* `disciplines` (timestamps, spellbook, mem_spells, buffs, **coin**) sits 400 bytes later
on the wire than the struct claims. Reading platinum at @12869 landed inside the 42-entry buff
array → `0x80000000`-style garbage.

Verified two independent ways:
- Simulating every write in `ENCODE(OP_PlayerProfile)` puts platinum at byte **13269**.
- Struct offset 12869 + the proven 400-byte disciplines undercount = 13269.
Landmark fields *before* disciplines (STR @952, face @896, aa_array @1012, skills @4616) match
between the struct and the stream simulation exactly, confirming the divergence begins at
disciplines.

## Fix
`parse_player_profile` now reads coin at @13269/@13273/@13277/@13281 (and the same-cause
`mem_spells` at @9784 instead of @9384). See `src/eq_net/packet_handler.rs`.

## Status
Fixed
