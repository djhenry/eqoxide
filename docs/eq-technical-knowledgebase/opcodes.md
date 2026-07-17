# RoF2 opcode table in eqoxide — where it lives, and the missing name-lookup fn

## There is no `u16 -> name` reverse-lookup function in eqoxide (as of #525 investigation)

Searched the whole tree (excluding `.claude/worktrees/`) for a reusable
"opcode number to human name" function — `fn opcode_name(u16) -> &'static str`,
an `OPCODE_NAMES` table, a `phf_map!`, anything. **None exists.**

What exists instead:

- `src/eq_net/protocol/mod.rs:39-149` (approx) — ~149
  `pub const OP_XXX: u16 = 0x....;` declarations, one per RoF2 app opcode, each
  with an inline `// RoF2: OP_ActualName` comment giving the canonical name
  from `EQEmu/utils/patches/patch_RoF2.conf`. This is the ground-truth
  opcode table, but it's *forward* only (name/const -> value); nothing maps
  value -> name at runtime.
- `src/eq_net/packet_handler.rs:15` (`apply_packet`) — the real dispatcher,
  a giant `match packet.opcode { OP_NEW_SPAWN => ..., OP_CLIENT_UPDATE => ...,
  ... }`. The match arms use the const *names* directly (resolved at compile
  time), so there's no string table backing this either — `packet.opcode` at
  runtime is still just a bare `u16`.
- Nearby but NOT opcode name lookups (don't confuse these with what #525
  needs): `class_name(u32)` (`packet_handler.rs:1026`), `con_level_name(u32)`
  (`packet_handler.rs:2035`), `attitude_name(u32)` (`packet_handler.rs:2050`),
  `eq_race_to_code(u32)` (`protocol/mod.rs:893`) — these decode *field values
  inside* a payload (class id, faction attitude, race id), not the opcode
  itself.

### Recommendation for #525's telemetry rig

Since no reusable function exists, the cheapest correct move is a small
`match` generated once from the existing `pub const OP_XXX` list (copy the
const names, not the values, so it can never drift from
`protocol/mod.rs`), e.g. in a new `src/eq_net/protocol/opcode_name.rs`:

```rust
pub fn opcode_name(op: u16) -> &'static str {
    match op {
        OP_CLIENT_UPDATE => "OP_ClientUpdate",
        OP_NEW_SPAWN => "OP_NewSpawn",
        OP_ZONE_SPAWNS => "OP_ZoneSpawns",
        OP_ZONE_ENTRY => "OP_ZoneEntry",
        OP_DELETE_SPAWN => "OP_DeleteSpawn",
        OP_NEW_ZONE => "OP_NewZone",
        // ... extend as needed; unmapped -> fall through
        _ => "OP_Unknown",
    }
}
```

For the telemetry line's hex, just format `packet.opcode` directly
(`format!("{:#06x}", packet.opcode)`); no lookup needed for that part.

If a full 149-arm table is wanted, generate it by literally copying the
`// RoF2: OP_X` comments out of `protocol/mod.rs:39-149` — do **not**
re-derive names/values from `patch_RoF2.conf` independently, since the
consts here are already the audited, tested source of truth
(`protocol/mod.rs:1132-1140`, `rof2_handshake_opcodes_match_conf` test).

## Cross-reference

- `docs/eq-technical-knowledgebase/opcode-direction.md` — server->client vs
  client->server direction notes for specific opcodes.
- `docs/eq-technical-knowledgebase/packet-telemetry-hook-points.md` — the
  #525 ring-buffer hook point and per-opcode field offsets for a decoded
  one-line summary.
