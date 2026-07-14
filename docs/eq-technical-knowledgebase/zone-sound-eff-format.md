# Zone sound files: `<zone>_sndbnk.eff` and `<zone>_sounds.eff` (RoF2)

**Status.** This is a description of the on-disk `.eff` data-file format, as reconstructed and
**implemented in eqoxide's asset server** (`src/audio.rs`, PR #36 / asset-server issue #32) and
validated against real zone files. Confidence markers below record how well each field's *semantic*
is pinned down; the byte layout itself is confirmed by parsing real files (every observed
`_sounds.eff` is an exact multiple of 84 bytes).

Two files per zone:
- `<zone>_sndbnk.eff` — a text file listing sound *names*, in two sections (`EMIT` and `LOOP`).
- `<zone>_sounds.eff` — a binary file of fixed 84-byte records, each placing an emitter in the
  world and referencing a name from the sndbnk by index.

## `_sndbnk.eff`: a single combined index space, not two lists

The two sections share **one** index space, with a fixed reset at the `LOOP` boundary:

- `EMIT` name #k (1-based, in file order) → combined index **k** (k = 1, 2, 3, …)
- `LOOP` name #k (1-based, in file order) → combined index **161 + k** (i.e. LOOP starts at 162)

Consequences:
- **LOOP always starts at combined id 162** regardless of how many EMIT lines exist — it is *not*
  "EMIT count + 1".
- There is no separate "which section" flag stored per entry. **Only the numeric value of the id,
  relative to the 162 boundary, tells you EMIT vs LOOP.**
- This implies a cap of 161 EMIT entries before the id space would collide with LOOP. Never hit in
  practice — real files are far smaller (airplane = 35 `sounds` records, akheva = 70).
- Index 0 is never assigned. An id of 0 in a `_sounds.eff` record's sound-id field is therefore a
  **"no sound" sentinel** for playback types 0/2/3.

## `_sounds.eff`: 84-byte (`0x54`) binary records

### Field layout (byte offset, size, day/night-ness), all little-endian

Five of the fields are stored as a **day value and a night value**; the rest are shared. The layout:

| Offset | Size | Day/Night | Field (semantic) | Confidence |
|---|---|---|---|---|
| 0x00–0x0F | 16B | shared | not consumed by the engine — likely tool-only metadata/name/padding | evidence: absence, not directly proven |
| 0x10–0x13 | 4B f32 | shared | X position | inferred (position triple, see below) |
| 0x14–0x17 | 4B f32 | shared | Y position | inferred |
| 0x18–0x1B | 4B f32 | shared | Z position — **stored negated** relative to world z (negate on load) | confirmed sign-flip, axis identity inferred |
| 0x1C–0x1F | 4B f32 | shared | Radius. If `< 0.0`, the engine substitutes a default | confirmed |
| 0x20–0x23 | 4B i32 | **day** | "range base" — only consumed by TYPE==1 records, forms a `(base, base+width)` range | low confidence on exact meaning |
| 0x24–0x27 | 4B i32 | **night** | mirrors 0x20–0x23 | same |
| 0x28–0x2B | 4B i32 | shared | "range width" — see above | low confidence |
| 0x2C–0x2F | 4B | — | **gap**: never read by the engine | evidence: absence |
| 0x30–0x33 | 4B i32 (**signed**) | **day** | **SOUND-ID / MUSIC-SELECTOR** — the field that answers "which sound/music plays"; see resolution rules below | confirmed (sign is explicitly significant) |
| 0x34–0x37 | 4B i32 (signed) | **night** | mirrors 0x30–0x33 | confirmed |
| 0x38 | 1B | **day** | **TYPE** discriminator, values `0,1,2,3` observed. This byte is authoritative for playback mode and is used to build BOTH the day and night runtime emitters (see gotcha below) | confirmed |
| 0x39 | 1B | **night** | second TYPE byte — used **only** to test day/night sameness; never itself drives playback | confirmed |
| 0x3C–0x3F | 4B i32 | **day** | rolloff/attenuation-curve parameter. Its sign is significant: a negative value feeds a curve exponent; otherwise a per-type default is used | medium-high confidence this is effectively "min distance"/rolloff shape, not a plain distance value |
| 0x40–0x43 | 4B i32 | **night** | mirrors 0x3C–0x3F | same |
| 0x44–0x47 | 4B i32 | **day** | repeat-delay **MAX** (ms). Only used by TYPE==1; clamped to 20000 | confirmed |
| 0x48–0x4B | 4B i32 | **night** | mirrors 0x44–0x47 | confirmed |
| 0x4C–0x4F | 4B i32 | **day** | repeat-delay **MIN** (ms). Used by TYPE==1 as the lower bound of a random range; also reused as an additive base in TYPE==3's curve setup (dual-purpose field) | confirmed usage, label inferred |
| 0x50–0x53 | 4B i32 | **night** | mirrors 0x4C–0x4F | confirmed |

Total accounted span: 0x00–0x53 = 84 bytes.

### Day/night "collapse to one record" logic

The engine compares **4** of the 5 day/night pairs for equality: the type byte (0x38 vs 0x39), the
sound-id (0x30-33 vs 0x34-37), the range-base (0x20-23 vs 0x24-27), and the rolloff param
(0x3C-3F vs 0x40-43). **It does NOT compare the repeat-timer pair (0x44-4B vs 0x4C-53).**

- If all 4 checked pairs are equal, **or** the day TYPE byte `== 2`, the record is treated as
  "identical" → exactly **one** emitter is created, using the day-side values throughout, flagged
  `always`. This means if a file author set different day/night *repeat timers* but left everything
  else equal, the night timer values are silently discarded — a real engine quirk, not a parser bug
  to "fix".
- Otherwise, **two** emitters are created: one from the day fields, one from the night fields.
- The runtime emitter carries a day/night flag: `0 = always` (day==night, single record),
  `1 = day-only`, `2 = night-only`. This matches the commonly-cited "0/1/2 = always/day/night"
  convention.

### Sound-id resolution — the core of "which .wav/music plays"

The TYPE byte (offset 0x38) selects the playback mode:

- **TYPE 0, 2, 3** ("positional per-zone sound"): the 32-bit sound-id field is used **directly,
  non-negative**, as the 1-based combined index into the per-zone sndbnk name list built from
  `_sndbnk.eff`. Resolve via the EMIT/LOOP boundary above:
  - `id == 0` → no sound (empty slot, nothing plays).
  - `1 <= id <= 161` → EMIT list, 0-based `EMIT[id-1]`.
  - `id >= 162` → LOOP list, 0-based `LOOP[id-162]`.

  Types 2 and 3 additionally apply different distance/falloff-curve math than type 0, but all three
  resolve the sound *name* identically.
- **TYPE 1** ("randomized/triggered sound OR music region"): the SAME field is instead
  **sign-tested**:
  - `id < 0` → play from a **separate, global, built-in SFX table** internal to the client, 1-based
    index = `-id`, completely independent of the current zone's `_sndbnk.eff`. eqoxide has no copy
    of that table, so these are surfaced as `SoundRef::GlobalSfx(-id)` and treated as
    unsupported/best-effort.
  - `id >= 0` → instead of a discrete sound, this triggers the **zone's background music** file,
    `<zone>.xmi`, passing `id` itself through as a track/variation selector. If the zone has no
    `.xmi` loaded, the record is silently dropped.
  - Correction vs a naive "`.mp3`" assumption: RoF2's per-zone ambient/region music triggered by
    `_sounds.eff` type-1 records is **`.xmi`**, not `.mp3`. `.mp3` files (`eqtheme.mp3`,
    `deaththeme.mp3`) exist in this client but are used for login-theme/death-theme, not for zone
    ambient music selection.

### Gotchas / edge cases for the Rust parser

- **Endianness**: all fields little-endian.
- **Sound-id field is signed** (0x30-33/0x34-37) — must sign-extend correctly when TYPE==1; for
  TYPE 0/2/3 treat as a non-negative index (0 is the explicit "none" sentinel).
- **`_sndbnk.eff` EMIT is 1-based**; first EMIT line = combined id 1, not 0.
- **LOOP always starts at combined id 162** regardless of how many EMIT lines exist.
- **EMIT vs LOOP has no separate flag** in the resolved record; membership is purely
  id-relative-to-162.
- The **night TYPE byte (0x39) is only used for the sameness test**, never for actual playback
  dispatch — a record can't really have day/night differ in playback *mode*, only in
  id/position/radius/timers.
- Bytes **0x2C-0x2F are a genuine unread gap** — don't assume they're a meaningful field; treat as
  reserved/unknown and pass them through if you round-trip raw bytes.
- Bytes **0x00-0x0F (16B header)** are likewise unread — same treatment.
- X/Y/Z axis identity at 0x10-0x1B is **inferred, not proven** (Z's sign flip and explicit `float`
  typing are confirmed; which of 0x10/0x14 is X vs Y was not independently verified against a
  known-position reference). Cheapest verification: parse a real zone (e.g. `airplane_sounds.eff`)
  and compare a record's position against a known landmark's world coords in the already-working
  zone-geometry pipeline.

## Recommendation for eqoxide's Rust parser

```rust
#[repr(C)]
struct SoundRecordRaw {
    _reserved0: [u8; 16],      // 0x00, unread by the engine — pass through raw
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
    repeat_max_day_ms: i32,     // 0x44 (clamp to 20000 to match the engine)
    repeat_max_night_ms: i32,   // 0x48
    repeat_min_day_ms: i32,     // 0x4C
    repeat_min_night_ms: i32,   // 0x50
}
```

Resolution algorithm per emitted manifest entry:

1. Determine the day/night split: compare (type, sound_id, range_base, rolloff) day-vs-night; if all
   equal OR `type_day == 2`, emit ONE entry (`day_night = Always`) using day-side values. Otherwise
   emit TWO entries (`day_night = Day` and `day_night = Night`) using the respective sides.
2. For each emitted entry, resolve `sound_name`/`is_music` from its `type` and `sound_id`:
   - `type in {0,2,3}`: `id==0` → drop (no sound). `1..=161` → `sndbnk.emit[id-1]` (`is_loop=false`).
     `>=162` → `sndbnk.loop_[id-162]` (`is_loop=true`).
   - `type == 1`: `id < 0` → global built-in SFX index `-id` (not resolvable from `_sndbnk.eff` at
     all — flag as `SoundRef::GlobalSfx(-id)`; treat as unsupported/best-effort).
     `id >= 0` → `is_music=true`, `music_track = id` (zone's `<zone>.xmi`; if eqoxide has no
     `.xmi`/music pipeline yet, at least record the flag+track so it's not silently lost).
3. Position: `(x, y, -z)`, radius as-is (or `None`/default sentinel if `< 0.0`).

Open item worth flagging: the exact semantic of the range-base/width fields (0x20-0x2C) and whether
rolloff (0x3C-40) is truly "min distance" vs a curve exponent is **inferred**, not proven against a
rendering/audio reference — fine to encode structurally now, but don't hard-code assumptions about
their *units* into user-facing config until spot-checked against a couple of real zone files where
the in-game behavior is known (e.g. a zone with an obvious waterfall/torch loop).

## Implemented in the asset server

`eqoxide_asset_server` `src/audio.rs` + `build::build_audio_from_raw` implement this (PR #36 /
asset-server issue #32): `sound/<zone>` emitter manifests (`emitters.json` with resolved day/night
`SoundRef`s + the referenced wavs from `sounds/` / `snd*.pfs`) and `music/<name>` sets grouping
`<name>.xmi`/`.mp3`. Verified against `beholder`. Client consumption is tracked in eqoxide #226.
