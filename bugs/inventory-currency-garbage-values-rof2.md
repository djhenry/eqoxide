# Player currency decodes to garbage values (RoF2 player profile)

**Summary:** A freshly created character with **zero coin** reports nonsense currency from the
client — `platinum: 2147483648` (0x80000000) and `gold: 119012671` — in both `/debug` and
`/inventory`, while the DB has no `character_currency` row (i.e. 0/0/0/0).

**Severity:** Medium (coin is unusable/misleading; would break any buy/sell budgeting logic)

**Zone / area:** Player profile decode (`OP_PlayerProfile`) — currency fields, RoF2 layout.

## Steps to reproduce
1. Create a fresh character (e.g. Brogan, Human Warrior) — see
   `.claude/skills/eq-character-creation`. New chars get no `character_currency` row.
2. Zone in, then read currency:
   - `curl -s "http://127.0.0.1:$PORT/debug"   | python3 -c "import sys,json;print(json.load(sys.stdin)['player']['currency'])"`
   - `curl -s "http://127.0.0.1:$PORT/inventory"| python3 -c "import sys,json;print(json.load(sys.stdin)['currency'])"`

## Expected
`{'copper': 0, 'silver': 0, 'gold': 0, 'platinum': 0}` (matches the DB — no currency row).

## Actual
`{'copper': 0, 'silver': 0, 'gold': 119012671, 'platinum': 2147483648}` from both endpoints.
DB truth: `SELECT platinum,gold,silver,copper FROM character_currency WHERE id=16;` → no row (NULL/0).

## Diagnosis notes
- Both `/debug` and `/inventory` share one source (the decoded player profile), so the error is in
  the profile decode, not the HTTP layer.
- `copper` and `silver` read 0 (correct); only `gold` and `platinum` are garbage — suggests the
  currency field **offsets** into the RoF2 `PlayerProfile_Struct` are off, so platinum/gold read
  bytes from an adjacent (wrong) region. `platinum = 0x80000000` looks like a sign/top-bit field,
  not real coin.
- This is the RoF2-migration analogue of the older (fixed) Titanium currency issue
  (`inventory-api-missing-currency.md`); that one was missing values, this one is wrong values.

## Suspected root cause
(unconfirmed) The platinum/gold currency offsets in the RoF2 `OP_PlayerProfile` decoder are
incorrect — the RoF2 `PlayerProfile_Struct` field positions differ from Titanium and weren't
remapped. Verify against `common/patches/rof2_structs.h` `PlayerProfile_Struct` (platinum/gold/
silver/copper plus the bank/cursor variants) and fix the read offsets.

## Status
Open
