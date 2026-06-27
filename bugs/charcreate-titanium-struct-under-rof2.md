# Client-side character creation sends Titanium 80-byte CharCreate_Struct under RoF2 (server expects 96)

**Summary:** `build_char_create` builds the Titanium 80-byte `CharCreate_Struct`, but the
server now negotiates **RoF2**, whose `CharCreate_Struct` is **96 bytes** with a different
field order — so `OP_CharacterCreate` is rejected for wrong size and the character is never
created (name-reservation row stays all-zeros, client never enters world).

**Severity:** High (client-side character creation is completely broken under RoF2)

**Zone / area:** Login / world handshake (character creation), `src/eq_net/login.rs`

## Steps to reproduce
1. Add a `character_create:` block for a not-yet-existing character to a config
   (e.g. `~/.config/eqoxide/config-garrik.yaml`, Human Warrior, Qeynos).
2. Launch: `./target/release/eqoxide --config garrik`.
3. Client logs: `name approved — sent OP_CharacterCreate for 'Garrik'` then nothing further
   (no new `OP_SendCharInfo`, never enters world; `GET /debug` shows empty zone/level 0).
4. Server world log shows:
   `[World] [Netcode] Wrong size on incoming [OP_CharacterCreate] (structs::CharCreate_Struct): Got [80], expected [96]`
5. DB `character_data` has a row for the name with all fields = 0 (the OP_ApproveName
   name-reservation placeholder); the create UPDATE never lands.

## Expected
Character is created and the client enters world (as it did under Titanium — see
`docs/autonomous-play.md` / memory `eq-client-charcreate`, which created "Mordeth" on 2026-06-26).

## Actual
Server rejects `OP_CharacterCreate` for wrong size (80 vs 96); create silently fails.

## Diagnosis notes
- Confirmed ground truth in EQEmu source:
  - Titanium `CharCreate_Struct` = 80 bytes (`common/patches/titanium_structs.h:576`),
    order: class, haircolor, beardcolor, beard, gender, race, start_zone, hairstyle, deity,
    STR, STA, AGI, DEX, WIS, INT, CHA, face, eyecolor1, eyecolor2, tutorial.
  - RoF2 `CharCreate_Struct` = **96 bytes** (`common/patches/rof2_structs.h`), order:
    **gender, race, class_, deity, start_zone, haircolor, beard, beardcolor, hairstyle, face,
    eyecolor1, eyecolor2, drakkin_heritage, drakkin_tattoo, drakkin_details, STR, STA, AGI,
    DEX, WIS, INT, CHA, tutorial, unknown0092**.
- `build_char_create` (`src/eq_net/login.rs:585`) hardcodes the Titanium 20×u32 = 80-byte layout.
- The `OP_CharacterCreate` and `OP_ApproveName` *opcodes* map correctly under RoF2 (the server
  recognized the packet and logged `char_name [Garrik] race_id [Human] class_id [Warrior]` from
  name approval) — only the create **struct size/field-order** is wrong.
- Stat validation itself is fine (Human Warrior 575 total is correct per
  `world/client.cpp` CheckCharCreateInfoTitanium); the packet never reaches validation because
  it's dropped at the size check.

## Suspected root cause
The client is mid-migration from Titanium to RoF2 (it already parses RoF2 spawns/items via
`parse_rof2_spawn`/`parse_rof2_item`), but `build_char_create` was not migrated. It needs to
build the 96-byte RoF2 layout (new field order + drakkin_heritage/tattoo/details + unknown0092).

## Resolution
`build_char_create` (`src/eq_net/login.rs`) now emits the **96-byte RoF2 layout** (24 × u32) in
the RoF2 field order (gender, race, class_, deity, start_zone, hair/face block, drakkin_heritage/
tattoo/details, STR..CHA, tutorial, unknown0092) instead of the Titanium 80-byte layout.
drakkin_* / unknown0092 are 0 for non-Drakkin races. Layout pinned by the
`char_create_layout_and_stat_total` unit test (now asserts 96 bytes + RoF2 offsets).

Verified live: a fresh Human Warrior (Rofcheck) created via the protocol — server world log
`Character creation succeeded for [Rofcheck]` (no more `Wrong size … Got [80], expected [96]`) —
and entered world in North Qeynos (level 1 Warrior). The post-create `OP_EnterWorld` logs a
non-fatal `is trying to go home before they're able` and the server places the new char at the
start zone; that's expected for a fresh character and not part of this bug.

## Status
Fixed — branch `worktree-charcreate-rof2`.
