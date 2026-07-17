# Packet telemetry: hook point + high-value opcode field offsets (#525)

## Hook point: what the hook actually receives

Confirmed in `src/eq_net/transport.rs`:

- `AppPacket` (`transport.rs:270`): `{ opcode: u16, payload: Vec<u8> }`.
- `EqStream::dispatch_app` (`transport.rs:900-907`) is where a combined/single
  app packet's raw bytes become an `AppPacket`: it reads the **opcode as a
  little-endian u16 from `data[0..2]`** (`transport.rs:904`,
  `byteorder::LittleEndian`), then sets `payload = data[2..].to_vec()`
  (`transport.rs:905`) — i.e. **payload is the raw body strictly after the
  2-byte opcode header**, opcode bytes are NOT included in payload. This is
  sent over `app_tx` (an `mpsc::UnboundedReceiver<AppPacket>`).
- Consumers `rx.try_recv()` this same `AppPacket` and call
  `apply_packet(&mut gs, &packet)` (`packet_handler.rs:13`, matched against
  `packet.opcode` at `packet_handler.rs:15`). Real production call sites:
  `src/eq_net/gameplay.rs:227` (main zone loop, right after the `#371`
  self-consider-probe intercept) and `gameplay.rs:855`; login-phase packets
  go through `src/eq_net/login.rs:182`.

**So: yes — at the point a telemetry hook would tap in (either at
`dispatch_app`'s `app_tx.send` or at the `apply_packet` call sites in
`gameplay.rs`), the opcode is already a decoded LE `u16` and the payload
`&[u8]`/`Vec<u8>` is the raw body with the opcode bytes already stripped.**
The best hook point for a ring buffer is right before/after
`apply_packet(&mut gs, &packet)` in `gameplay.rs:227` — you get the
already-parsed `AppPacket` for free and don't need to duplicate any
transport-layer decode.

## Per-opcode summary field offsets

### OP_ClientUpdate (0x7dfc) — position update, bit-packed 24 bytes

Decoder: `decode_position_update(p: &[u8]) -> Option<PositionUpdate>`
(`src/eq_net/protocol/mod.rs:1556-1577`). **Not plain byte-offset fields** —
most of it is a bit-packed layout, so a telemetry hook should call this
existing function rather than re-deriving offsets:

| field | wire location | notes |
|---|---|---|
| `spawn_id` | `p[0..2]` u16 LE | plain bytes, no bit-packing (`mod.rs:1558`) |
| `vehicle_id` | `p[2..4]` u16 LE | skipped by the decoder |
| `x` | bits 0-18 of word2 (`p[12..16]`) | EQ19 fixed-point, `/8.0` (`mod.rs:1568`) |
| `y` | bits 12-30 of word0 (`p[4..8]`) | EQ19 fixed-point (`mod.rs:1566`) |
| `z` | bits 10-28 of word3 (`p[16..20]`) | EQ19 fixed-point (`mod.rs:1573`) |
| `heading` | bits 19-30 of word2 | 12-bit CW -> converted to CCW degrees (`mod.rs:1570-1571`) |
| `animation` | bits 0-9 of word4 (`p[20..24]`) | `mod.rs:1575` |

Min length: `SIZE_SPAWN_POSITION_UPDATE == 24` bytes (checked at
`mod.rs:1557`); shorter -> `None`.

Telemetry line: `"OP_ClientUpdate spawn_id={} x={:.1} y={:.1} z={:.1} hdg={:.0}"`
via `decode_position_update(&packet.payload)`.

### OP_NewSpawn (0x6097) / OP_ZoneSpawns (0x5237) / OP_ZoneEntry (0x5089, S->C) — spawn record(s)

All three carry one-or-more RoF2 variable-length `Spawn_Struct` records and
share the same parser: `parse_rof2_spawn(buf: &[u8]) -> Option<(SpawnInfo,
usize)>` (`src/eq_net/protocol/mod.rs:646`, field order documented at
`mod.rs:621-645`). **No fixed byte offsets exist** — the record starts with
two NUL-terminated C-strings (`name`, later `last_name`), so every field
after `name` floats. Use the parser, not raw offsets:

| `SpawnInfo` field | position in read order | notes |
|---|---|---|
| `name` | 1st (cstr) | `mod.rs:679` |
| `spawn_id` | 2nd (u32) | `mod.rs:682` |
| `level` | 3rd (u8) | `mod.rs:684` |
| `npc` | 5th (u8) | 0=player,1=npc,2=pc_corpse,3=npc_corpse (`mod.rs:688`) |
| `x`,`y`,`z`,`heading` | near the end, in the 20-byte bit-packed `Spawn_Struct_Position` block | same EQ19/heading-CW scheme as `OP_ClientUpdate` (`mod.rs:822-843`) |

Dispatch differs per opcode even though the record parser is shared:
- `OP_NewSpawn` -> `apply_new_spawn` (`packet_handler.rs:804`): one record,
  whole payload.
- `OP_ZoneSpawns` -> `apply_zone_spawns` (`packet_handler.rs:958`): loop —
  `parse_rof2_spawn` repeatedly over `payload[offset..]`, `offset +=
  consumed`, until the buffer runs out. **This loop is where #463's
  "spawn-tail drop" would show up**: `packet_handler.rs:983-993` already logs
  a `tracing::warn!` with a 32-byte preview when a record fails mid-stream
  (agent-honesty #407) and stops registering the rest of that packet's
  roster — a telemetry hook should record `registered` count vs
  `payload.len()` vs bytes actually consumed to catch this without relying on
  grepping logs.
- `OP_ZoneEntry` (S->C) -> `apply_zone_entry` (`packet_handler.rs:998`): RoF2
  sends this **once per spawn**, not once per zone-in — EQEmu's
  `ENCODE(OP_ZoneEntry)` forwards to `ENCODE(OP_ZoneSpawns)` and re-wraps each
  entity as its own `OP_ZoneEntry` packet (`packet_handler.rs:999-1003`,
  citing `rof2.cpp:4542/4575/4660`). So for telemetry purposes it behaves
  like a single-record `OP_NewSpawn`, NOT like the fixed `ClientZoneEntry_Struct`
  C->S handshake struct of the same name.

Telemetry line: `"OP_NewSpawn spawn_id={} name={} npc={} pos=({:.1},{:.1},{:.1})"`
from the `SpawnInfo` returned by `parse_rof2_spawn`.

### OP_DeleteSpawn (0x7280) — the complement of the above, high value for #463

Trivial fixed layout: `id = u32 LE` at `payload[0..4]`
(`packet_handler.rs:836-840`, `apply_delete_spawn`). Correlate delete events
against `OP_ZoneSpawns`/`OP_NewSpawn` registrations in the ring buffer to spot
a spawn that got deleted before/without ever having been fully registered
(a plausible #463 shape).

### "Zone change" — use OP_NewZone (0x1795), not OP_ZoneEntry, for a zone-boundary telemetry marker

`OP_ZoneEntry` (S->C) is NOT the zone-change struct in RoF2 — see above, it's
a per-spawn record with the same opcode name as the (different) C->S
handshake struct. The opcode that actually carries zone identity/safe-point
data with **fixed byte offsets** is `OP_NewZone`, decoded inline in
`apply_new_zone` (`packet_handler.rs:915-956`) against the 948-byte
`rof2_structs.h` `NewZone_Struct`:

| field | offset | type |
|---|---|---|
| `zone_short_name` | 64 | NUL-terminated ASCII, up to 128 bytes (`packet_handler.rs:939-940`) |
| `safe_y` | 588 | f32 LE (`packet_handler.rs:942`) |
| `safe_x` | 592 | f32 LE (`packet_handler.rs:943`) |
| `safe_z` | 596 | f32 LE (`packet_handler.rs:944`) |
| `underworld` (min-z floor) | 608 | f32 LE (`packet_handler.rs:946`) |
| `zone_id` | 852 | u16 LE (`packet_handler.rs:948`) |

Min length `SIZE_NEW_ZONE == 948` (`packet_handler.rs:919`); note RoF2 sends
this opcode **twice** per zone-in (`packet_handler.rs:920-926`) — a
telemetry hook should record both deliveries (useful signal in itself) even
though `apply_packet` only applies the first (`gs.new_zone_applied` latch).

Telemetry line: `"OP_NewZone zone_id={} zone={} safe=({:.1},{:.1},{:.1})"`.

## Recommended opcode set for the #525 ring buffer

For #463 (spawn-tail drop) and #516 (position jitter) diagnosis, in priority
order:

1. `OP_CLIENT_UPDATE` — position/jitter, decode via `decode_position_update`.
2. `OP_ZONE_SPAWNS` — spawn roster stream; record `registered` vs bytes
   consumed vs `payload.len()` (see #463 note above), not just a single
   line.
3. `OP_NEW_SPAWN` / `OP_ZONE_ENTRY` — same `parse_rof2_spawn` decode, one
   record each; useful for catching a spawn that never shows up in a
   `OP_ZoneSpawns` batch.
4. `OP_DELETE_SPAWN` — 4-byte id; correlate against the above for
   removed-before-registered spawns.
5. `OP_NEW_ZONE` — zone-boundary marker (fixed offsets above); every jitter
   sample should be tagged with the current zone/zone-in generation so a
   post-zone jitter burst isn't confused with steady-state jitter.

## Cross-reference

- `docs/eq-technical-knowledgebase/opcodes.md` — where the opcode
  const table lives, and confirmation there is no runtime name lookup fn
  (had to be built for #525, not reused).
- `docs/eq-technical-knowledgebase/zone-spawn-delivery.md`,
  `docs/eq-technical-knowledgebase/spawn-struct-level-field.md` — related
  existing notes on the RoF2 spawn record format.
