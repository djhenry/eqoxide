# Zone sound files: `<zone>_sndbnk.eff` and `<zone>_sounds.eff` (RoF2)

Source of ground truth: `eqgame.exe` decompile (2019 RoF2 build).
Key functions (all in `everquest_rof2/decompiled/ghidra/eqgame.exe.c`):

- `FUN_004ab150` @ `eqgame.exe.c:119288` — loads `<zone>_sndbnk.eff` then
  `<zone>_sounds.eff` for the current zone. **This is the file reader; read it
  first when re-verifying anything below.**
- `FUN_004aac30` @ `eqgame.exe.c:119021` — builds one runtime "emitter" object
  from a decoded record's fields (the actual playback-mode dispatch/switch).
- `FUN_004aa750` @ `eqgame.exe.c:118863` — inserts one `_sndbnk.eff` name into
  a direct-indexed per-zone array (`this+0x1004`, stride 8: 4B resource handle
  + 1B flag).
- `FUN_004aa500` @ `eqgame.exe.c:118745` — resolves a **global, hardcoded**
  built-in SFX by 1-based index from a fixed table baked into the exe at
  `this+0xae58` (32 bytes/entry), cache array at `this+0xacc8`, count at
  `this+0xbad8`. Completely separate address space from the per-zone sndbnk
  array.
- `FUN_004aa2a0` @ `eqgame.exe.c:118592` — the actual `.wav` loader; builds
  `sounds\<name>` and appends a default extension if the name has none.

## `_sndbnk.eff` (confirmed): single combined index space, not two lists

`FUN_004ab150` @ `eqgame.exe.c:119320-119359`:

```
iVar7 = 1;                              // running index, 1-based
... line == "EMIT"  -> iVar7 = 1;
... line == "LOOP"  -> iVar7 = 0xa2;    // = 162, a FIXED reset, not "wherever EMIT left off"
... else (a name line) -> FUN_004aa750(name, iVar7, 1); iVar7 = iVar7 + 1;
```

So:
- EMIT name #k (1-based, in file order) -> combined index **k** (k = 1, 2, 3, ...)
- LOOP name #k (1-based, in file order) -> combined index **161 + k** (starts at 162)
- It is **one array** (`this+0x1004`, `FUN_004aa750`/`FUN_004aac30` both index it
  directly), addressed by this single combined id. There is no separate
  "which list" flag stored per-slot — the 1-byte flag written alongside each
  entry (`this+0x1008+id*8`) is always the literal constant `1` for both EMIT
  and LOOP inserts (`eqgame.exe.c:119352`: `FUN_004aa750(local_300,iVar7,1)`),
  so it cannot be used to tell EMIT from LOOP. **Only the numeric value of the
  id, relative to the 162 boundary, tells you EMIT vs LOOP.**
- This hard-codes an implicit cap of 161 EMIT entries before the id-space
  would collide with LOOP (never hit in practice — real files are far
  smaller: airplane=35 sounds records, akheva=70).
- Index 0 is never assigned by the parser (EMIT starts at 1) -> id 0 in a
  `_sounds.eff` record's sound-id field is a **"no sound" sentinel** for
  playback types 0/2/3 (the array slot is always-zero, causing
  `FUN_004aac30` to return 0 — see below).

## `_sounds.eff`: 84-byte (`0x54`) binary records — CONFIRMED

`FUN_004ab150` @ `eqgame.exe.c:119365`: `fread(local_354, 1, 0x54, pFVar3)`.
Matches every observed file size being an exact multiple of 84.

### Field layout (byte offset, size, day/night-ness), all little-endian (x86 PE)

Derived from the Ghidra stack-frame layout of the read buffer at
`eqgame.exe.c:119298-119317` cross-referenced against the two calls into
`FUN_004aac30` at `eqgame.exe.c:119375` (day variant) and `:119386` (night
variant), which pass **different** locals for 5 of the fields and the
**same** locals for the rest — i.e. the record physically stores one day
value and one night value for those 5 fields, and one shared value for the
others.

| Offset | Size | Day/Night | Field (semantic) | Confidence |
|---|---|---|---|---|
| 0x00–0x0F | 16B | shared | unreferenced by the client parser at all (no load instruction touches these bytes in `FUN_004ab150`/`FUN_004aac30`) — likely tool-only metadata/name/padding | evidence: absence, not directly proven |
| 0x10–0x13 | 4B f32 | shared | X position | inferred (position triple, see below) |
| 0x14–0x17 | 4B f32 | shared | Y position | inferred |
| 0x18–0x1B | 4B f32 | shared | Z position — **stored negated** at runtime (`*(float*)(obj+0x30) = -param_5;`, `eqgame.exe.c:119121`/`:119155`) | confirmed sign-flip, axis identity inferred |
| 0x1C–0x1F | 4B f32 | shared | Radius. If `< 0.0`, engine substitutes a default (`eqgame.exe.c:119055-119059`, calls `FUN_008dc6c0()` with no explicit args and takes the FPU return) | confirmed |
| 0x20–0x23 | 4B i32 | **day** | "range base" — only consumed by TYPE==1 records, forms `(base, base+width)` at emitter+0x44/+0x48 (`eqgame.exe.c:119124-119125`) | low confidence on exact meaning |
| 0x24–0x27 | 4B i32 | **night** | mirrors 0x20–0x23 | same |
| 0x28–0x2B | 4B i32 | shared | "range width" — see above | low confidence |
| 0x2C–0x2F | 4B | — | **gap**: genuinely never read by `FUN_004ab150` (no local variable maps here at all) | evidence: absence |
| 0x30–0x33 | 4B i32 (**signed**) | **day** | **SOUND-ID / MUSIC-SELECTOR** — the field that answers "which sound/music plays"; see resolution rules below | confirmed (sign explicitly tested) |
| 0x34–0x37 | 4B i32 (signed) | **night** | mirrors 0x30–0x33 | confirmed |
| 0x38 | 1B | **day** | **TYPE** discriminator, values `0,1,2,3` observed (`switch(param_2)` at `eqgame.exe.c:119060`). This is the byte actually used to build BOTH the day and night runtime objects (the caller always passes the *day* type byte, `local_31c`, to both `FUN_004aac30` calls — see gotcha below) | confirmed |
| 0x39 | 1B | **night** | second TYPE byte — used **only** to test day/night sameness (`local_31c==local_31b`); never itself passed to `FUN_004aac30` | confirmed |
| 0x3C–0x3F | 4B i32 | **day** | rolloff/attenuation-curve parameter. Sign-tested: `if (-param_6 < 0)` -> feeds `FUN_005bdb40(-param_6)` to build a curve exponent stored at emitter+8; else per-type default constant is used (`eqgame.exe.c:119156-119165`) | medium-high confidence this is effectively "min distance"/rolloff shape, not a plain distance value |
| 0x40–0x43 | 4B i32 | **night** | mirrors 0x3C–0x3F | same |
| 0x44–0x47 | 4B i32 | **day** | repeat-delay **MAX** (ms). Only used by TYPE==1; clamped to 20000 (`if (20000 < param_11) param_11 = 20000;`, `eqgame.exe.c:119074`/`:119102`) | confirmed |
| 0x48–0x4B | 4B i32 | **night** | mirrors 0x44–0x47 | confirmed |
| 0x4C–0x4F | 4B i32 | **day** | repeat-delay **MIN** (ms). Used by TYPE==1 as the lower bound of a random range (`FUN_008dc6c0(param_10, 0x5dc /*1500*/, param_11, ...)`); also reused as an additive base in TYPE==3's curve setup at `eqgame.exe.c:119195` (dual-purpose field) | confirmed usage, label inferred |
| 0x50–0x53 | 4B i32 | **night** | mirrors 0x4C–0x4F | confirmed |

Total accounted span: 0x00–0x53 = 84 bytes. Matches `fread(...,0x54,...)` exactly.

### Day/night "collapse to one record" logic (confirmed, `eqgame.exe.c:119367-119395`)

The parser compares **4** of the 5 day/night pairs for equality:
type byte (0x38 vs 0x39), sound-id (0x30-33 vs 0x34-37), range-base
(0x20-23 vs 0x24-27), and rolloff param (0x3C-3F vs 0x40-43).
**It does NOT compare the repeat-timer pair (0x44-4B vs 0x4C-53).**

- If all 4 checked pairs are equal, **or** the day TYPE byte `== 2`, the
  record is "identical" -> exactly **one** emitter is created, using the
  day-side values throughout, flagged `always` (see below). This means if a
  file author set different day/night *repeat timers* but left everything
  else equal, the night timer values are silently discarded — a real engine
  quirk, not a parser bug to "fix".
- Otherwise, **two** emitters are created: one from the day fields, one from
  the night fields.
- The runtime emitter object stores a day/night flag at `+0x50`:
  `0 = always` (day==night, single record), `1 = day-only`, `2 = night-only`
  (`eqgame.exe.c:119378-119394`). This matches the commonly-cited
  "0/1/2 = always/day/night" convention.

### Sound-id resolution — the core of "which .wav/music plays" (confirmed)

The TYPE byte (offset 0x38) selects the playback mode in `FUN_004aac30`
(`eqgame.exe.c:119060`):

- **TYPE 0, 2, 3** ("positional per-zone sound"): the 32-bit sound-id field
  is used **directly, unsigned/non-negative**, as the 1-based combined index
  into the per-zone sndbnk array built from `_sndbnk.eff`
  (`this+0x1004+id*8`, `eqgame.exe.c:119139`). Resolve via the EMIT/LOOP
  boundary above:
  `id == 0` -> no sound (empty slot, function returns 0, nothing plays).
  `1 <= id <= 161` -> EMIT list, 0-based `EMIT[id-1]`.
  `id >= 162` -> LOOP list, 0-based `LOOP[id-162]`.
  Types 2 and 3 additionally apply different distance/falloff-curve math
  than type 0 (`eqgame.exe.c:119190-119207`), but all three resolve the
  sound name identically.
- **TYPE 1** ("randomized/triggered sound OR music region"): the SAME field
  is instead **sign-tested** (`eqgame.exe.c:119066`, `:119102`):
  - `id < 0` -> play from a **separate, global, hardcoded** built-in SFX
    table baked into `eqgame.exe` itself (`FUN_004aa500`,
    `eqgame.exe.c:118745`), 1-based index = `-id`, completely independent of
    the current zone's `_sndbnk.eff`. This is the "global/hardcoded SFX
    index" case.
  - `id >= 0` -> instead of a discrete sound, this triggers the **zone's
    background music** file, `<zone>.xmi` (loaded once per zone as
    `%s.xmi`, `eqgame.exe.c:119476-119478`), passing `id` itself through as
    an extra parameter (track/variation selector) to the low-level player.
    If the zone has no `.xmi` loaded (`this+0x24 == 0`), the record is
    silently dropped (`eqgame.exe.c:119098`: `return 0`).
  - Correction vs a naive "`.mp3`" assumption: RoF2's per-zone ambient/region
    music triggered by `_sounds.eff` type-1 records is **`.xmi`**, not
    `.mp3`. `.mp3` files (`eqtheme.mp3`, `deaththeme.mp3`,
    `eqgame.exe.c:118952-118955`) exist in this client but are used for
    login-theme/death-theme, not for zone ambient music selection.

### Gotchas / edge cases for the Rust parser

- **Endianness**: 32-bit x86 PE, all fields little-endian.
- **Sound-id field is signed** (0x30-33/0x34-37) — must sign-extend correctly
  when TYPE==1; for TYPE 0/2/3 treat as an unsigned/non-negative index
  (0 is the explicit "none" sentinel).
- **`_sndbnk.eff` EMIT is 1-based**; first EMIT line = combined id 1, not 0.
- **LOOP always starts at combined id 162** regardless of how many EMIT
  lines exist — it is not "EMIT count + 1".
- **EMIT vs LOOP has no separate flag** in the resolved record; membership
  is purely id-relative-to-162.
- The **night TYPE byte (0x39) is only used for the sameness test**, never
  for actual playback dispatch — a record can't really have day/night differ
  in playback *mode*, only in id/position/radius/timers.
- Bytes **0x2C-0x2F are a genuine unread gap** — don't assume they're a
  meaningful field; treat as reserved/unknown and just pass them through if
  you round-trip raw bytes.
- Bytes **0x00-0x0F (16B header)** are likewise unread by this parser —
  same treatment.
- X/Y/Z axis identity at 0x10-0x1B is **inferred, not proven** by this
  investigation (Z's sign flip and explicit `float` typing are confirmed;
  which of 0x10/0x14 is X vs Y was not independently verified against a
  known-position reference). Cheapest verification: parse a real zone
  (e.g. `airplane_sounds.eff`) and compare a record's position against a
  known landmark's world coords in the already-working zone-geometry
  pipeline.

## Recommendation for eqoxide's Rust parser

```rust
#[repr(C)]
struct SoundRecordRaw {
    _reserved0: [u8; 16],      // 0x00, unread by client — pass through raw
    x: f32,                    // 0x10
    y: f32,                    // 0x14
    z_negated: f32,             // 0x18 — negate on load: z = -z_negated
    radius: f32,                // 0x1C — if < 0.0, treat as "engine default radius"
    range_base_day: i32,        // 0x20
    range_base_night: i32,      // 0x24
    range_width: i32,           // 0x28 (shared)
    _reserved1: [u8; 4],        // 0x2C, unread — pass through raw
    sound_id_day: i32,          // 0x30 (SIGNED)
    sound_id_night: i32,        // 0x34 (SIGNED)
    type_day: u8,               // 0x38 — authoritative for playback mode
    type_night: u8,             // 0x39 — only for day/night-sameness test
    _pad: [u8; 2],
    rolloff_day: i32,           // 0x3C
    rolloff_night: i32,         // 0x40
    repeat_max_day_ms: i32,     // 0x44 (clamp to 20000 to match client)
    repeat_max_night_ms: i32,   // 0x48
    repeat_min_day_ms: i32,     // 0x4C
    repeat_min_night_ms: i32,   // 0x50
}
```

Resolution algorithm per emitted manifest entry:

1. Determine day/night split: compare (type, sound_id, range_base, rolloff)
   day-vs-night; if all equal OR `type_day == 2`, emit ONE entry
   (`day_night = Always`) using day-side values. Otherwise emit TWO entries
   (`day_night = Day` and `day_night = Night`) using the respective sides.
2. For each emitted entry, resolve `sound_name`/`is_music` from its `type`
   and `sound_id`:
   - `type in {0,2,3}`: `id==0` -> drop (no sound). `1..=161` -> `sndbnk.emit[id-1]` (`is_loop=false`). `>=162` -> `sndbnk.loop_[id-162]` (`is_loop=true`).
   - `type == 1`: `id < 0` -> global/hardcoded SFX index `-id` (not resolvable from `_sndbnk.eff` at all — flag as `SoundRef::GlobalSfx(-id)` since eqoxide has no copy of the client's baked-in table; treat as unsupported/best-effort until that table is extracted separately) `id >= 0` -> `is_music=true`, `music_track = id` (zone's `<zone>.xmi`; if eqoxide has no `.xmi`/music pipeline yet, at least record the flag+track so it's not silently lost).
3. Position: `(x, y, -z)`, radius as-is (or `None`/default sentinel if `< 0.0`).

Open item worth flagging back to the caller: the exact semantic of the
range-base/width fields (0x20-0x2C) and whether rolloff (0x3C-40) is truly
"min distance" vs a curve exponent is inferred from control flow, not
proven against a rendering/audio reference — fine to encode structurally
now, but don't hard-code assumptions about their *units* into user-facing
config until spot-checked against a couple of real zone files where the
in-game behavior is known (e.g. a zone with an obvious waterfall/torch loop).

## Implemented in the asset server

`eqoxide_asset_server` `src/audio.rs` + `build::build_audio_from_raw` implement
this (PR #36 / asset-server issue #32): `sound/<zone>` emitter manifests
(`emitters.json` with resolved day/night `SoundRef`s + the referenced wavs from
`sounds/` / `snd*.pfs`) and `music/<name>` sets grouping `<name>.xmi`/`.mp3`.
Verified against `beholder`. Client consumption is tracked in eqoxide #226.
