# RoF2 spawn delivery is NEVER range-gated — the 600u cutoff only picks bulk vs. individual packet framing

Companion to `zone-spawn-delivery.md` (same `Spawn_Struct`/opcode-collapse family).
This note answers a different question: **does EQEmu withhold a spawn from a
client because it's far away, and later push it when the client gets close?**
Answer: **no, never.** Confirmed by reading every spawn-broadcast call site in
`zone/entity.cpp` and `zone/client.cpp`.

## 1. Zone-in: `EntityList::SendZoneSpawnsBulk` sends EVERY in-zone spawn, distance only picks the framing

`EQEmu/zone/entity.cpp:1351-1422`. Called from `Client::SendZoneEntryPacket`-ish
path at `EQEmu/zone/client_packet.cpp:1764`, right after the player's own
`OP_ZoneEntry` self-spawn packet is queued (`client_packet.cpp:1752-1761`) and
before `SendZoneCorpsesBulk`/`SendZonePVPUpdates` (`client_packet.cpp:1765-1766`).

```
entity.cpp:1365   const float distance_max = (600.0 * 600.0);
entity.cpp:1367   for (auto & it : mob_list) {                 // EVERY mob in the zone, no skip
entity.cpp:1370       if (!spawn->ShouldISpawnFor(client)) continue;   // GM-hide / hovering-respawn only
entity.cpp:1376       bool is_delayed_packet = (
entity.cpp:1377           DistanceSquared(client_position, spawn_position) > distance_max ||
entity.cpp:1378           (spawn->IsClient() && (race == MinorIllusion || race == Tree))
entity.cpp:1379       );
entity.cpp:1381       if (is_delayed_packet) {
entity.cpp:1382           app = new EQApplicationPacket;
entity.cpp:1383           spawn->CreateSpawnPacket(app);            // individual packet, per record
entity.cpp:1384           client->QueuePacket(app, true, Client::CLIENT_CONNECTED);
entity.cpp:1386       } else {
entity.cpp:1390           bulk_zone_spawn_packet->AddSpawn(&ns);    // batched into BulkZoneSpawnPacket
entity.cpp:1391       }
entity.cpp:1418   }
```

- `ShouldISpawnFor` (`zone/mob.h:743` base = always `true`; `zone/client.h:442`
  override = `!GMHideMe(c) && !IsHoveringForRespawn()`) has **no distance
  term**. The only spawns skipped entirely are GM-hidden or an NPC still
  hovering pending respawn.
- Everything else in `mob_list` is delivered, unconditionally of distance —
  the `distance_max` (600²) test only decides **how** it's packaged:
  - ≤600u (and not an illusion/tree PC): batched into the shared
    `BulkZoneSpawnPacket` (`entity.h:641`), flushed as internal opcode
    `OP_ZoneSpawns` with N `NewSpawn_Struct` records
    (`entity.cpp:3508-3524`, `SendBuffer()`).
  - >600u, or a PC illusioned as `Race::MinorIllusion`/`Race::Tree`: sent as
    its own `EQApplicationPacket` via `Mob::CreateSpawnPacket`
    (`zone/mob.cpp:1236-1253`), internal opcode **`OP_NewSpawn`**
    (`mob.cpp:1238`), one `NewSpawn_Struct` per packet, queued individually.
- On the wire both paths collapse to the same thing: `rof2.cpp:2358
  ENCODE(OP_NewSpawn) { ENCODE_FORWARD(OP_ZoneSpawns); }` and the shared
  `ENCODE(OP_ZoneSpawns)` body (`rof2.cpp:4575-4934`) rewrites **every**
  record — 1 or many — into its own fresh packet with the opcode hardcoded to
  **`OP_ZoneEntry`** (wire `0x5089`, `rof2.cpp:4660`,
  `patch_RoF2.conf:63`). See `zone-spawn-delivery.md` for the full chain;
  `OP_ZoneSpawns` wire value is `0x5237` (`patch_RoF2.conf:66`) and is
  effectively dead on the wire for spawn delivery, and `OP_NewSpawn` wire
  value is `0x6097` (`patch_RoF2.conf:265`), also never the wire opcode
  actually sent.

**So: a spawn 2000 units away at zone-in is delivered at zone-in**, just as
an individual wire-`OP_ZoneEntry` packet (single `Spawn_Struct` record)
instead of being folded into the multi-record bulk packet. It is not
withheld.

## 2. Runtime: new spawns/logins are broadcast zone-wide, also with no distance filter

When an already-connected client's own spawn is announced to the rest of the
zone (e.g. finishing zone-in, `Client::SendZoneInPackets`,
`zone/client.cpp:814-836`):

```
client.cpp:828   outapp = new EQApplicationPacket();
client.cpp:830   CreateSpawnPacket(outapp);            // internal opcode OP_NewSpawn
client.cpp:832   if (!GetHideMe()) entity_list.QueueClients(this, outapp, true);
```

`EntityList::QueueClients` (`entity.cpp:1767-1781`) iterates **the entire
`client_list`** and calls `ent->QueuePacket(app, ackreq, Client::CLIENT_CONNECTED)`
for every one — **no distance argument, no distance check at all.** (Contrast
with `entity.cpp:1731/1745`, `GetCloseMobList`/message-broadcast helpers that
DO take a distance — those are for chat/emote range, not spawn delivery.)

Same story for NPC pops, corpse creation, etc. — every zone-wide spawn/removal
broadcast in `zone/*.cpp` that reaches other players goes through
`entity_list.QueueClients(...)` (no distance) or the bulk-equivalent, not a
proximity-scoped call.

**Conclusion: there is no "spawn enters range" push in EQEmu.** The ~600u
number is purely a wire-encoding format switch at the single moment of zone
entry (bulk-record vs. individual-record framing); it is never a delivery
gate, then or later. Every spawn that exists in the zone when a client
connects gets delivered to that client during `SendZoneSpawnsBulk`, and every
spawn created afterward gets broadcast to all connected clients
unconditionally. A player moving toward (or teleporting onto) a spawn that
was already in the zone at their own zone-in time triggers **no additional
network traffic** from the server — the client is expected to already hold
that spawn from the zone-in delivery.

## 3. No client-initiated "give me spawn X" opcode exists

The only client→server use of `OP_ZoneEntry` is the initial zone-ready
handshake, not a spawn-pull request:

```
rof2.cpp:6417   DECODE(OP_ZoneEntry)
rof2.cpp:6419       DECODE_LENGTH_EXACT(structs::ClientZoneEntry_Struct);
rof2.cpp:6422       memcpy(emu->char_name, eq->char_name, sizeof(emu->char_name));
```

`structs::ClientZoneEntry_Struct` (`rof2_structs.h:545`) carries only the
character name — it's "I'm ready to receive my zone," sent once, that
triggers `SendZoneEntryPacket`/`SendZoneSpawnsBulk` server-side
(`client_packet.cpp:1764`). Grepping `rof2.cpp` for `DECODE(OP_ZoneEntry)`
finds this as the *only* client-originated decode for any spawn-family
opcode — there is no `DECODE(OP_NewSpawn)` or `DECODE(OP_ZoneSpawns)` at all
(those are server→client only). **Confirmed: spawn delivery in RoF2/EQEmu is
purely server-push; there is no client request mechanism for a missing/far
spawn**, and the native client relies entirely on receiving everything at
zone-in (see §1/§2).

## Diagnosing "far spawn / teleported-onto spawn never appears" in eqoxide

Given §1–§3, the missing-far-spawn symptom **cannot** be explained by a
missing "entered range" trigger on the server side — no such trigger exists,
and none is needed, because nothing is ever withheld by distance. If eqoxide
never registers a spawn that was far away at its own zone-in time, the defect
is one of:

- The individual per-far-spawn wire-`OP_ZoneEntry` packet (single
  `Spawn_Struct`, queued via `Client::QueuePacket(app, true,
  Client::CLIENT_CONNECTED)`, `entity.cpp:1384`) is being dropped/lost at the
  transport or session layer before reaching the app-packet dispatcher —
  e.g. an ack/retransmit gap, or ordering issue with `AddPacket`'s hold-queue
  when `client_state != CLIENT_CONNECTED` at the moment the server tries to
  send it (`zone/client.cpp:1169-1172`, `1184-1198`) — worth a packet capture
  around zone-in to count individual `OP_ZoneEntry` (`0x5089`) records vs.
  bulk-batched ones and compare against what `eqoxide` actually decodes.
- `parse_rof2_spawn` (`eqoxide/src/eq_net/protocol/mod.rs:672`) failing
  specifically on these records and hitting the AGENT-HONESTY drop-log path
  in `apply_zone_entry` (`eqoxide/src/eq_net/packet_handler.rs:1014-1021`,
  `"OP_ZoneEntry spawn FAILED to parse ... entity is MISSING"`) — check logs
  for this warning at zone-in time before assuming a network-layer loss.
- Checked and ruled out as of this note: `register_spawn`
  (`eqoxide/src/eq_net/packet_handler.rs:2576`) has no distance-based filter
  — any record that reaches it gets registered into `gs.entities`
  unconditionally of position. So this is NOT a client-side "cull by
  distance" bug in the registration path itself; if there's a client bug it's
  upstream of `register_spawn` (decode failure or the record never arriving).

## Recommendation for eqoxide

- Do not implement or expect any "spawn enters range" server push — it does
  not exist in RoF2/EQEmu. All spawn state for a zone must be captured from
  the zone-in delivery (`SendZoneSpawnsBulk`'s bulk + individual packets) and
  the ongoing zone-wide broadcasts (`QueueClients`), none of which are
  distance-filtered.
- The individual "delayed" packets for far spawns correspond to the SAME wire
  opcode (`OP_ZoneEntry`, `0x5089`) and struct (`Spawn_Struct`/
  `NewSpawn_Struct`) as the bulk-batched near spawns; both must land in
  `gs.entities` identically. Treat any far-spawn-goes-missing
  report as a decode/transport bug to chase with a packet capture + the
  agent-honesty drop-log lines already in `apply_zone_entry`/
  `apply_zone_spawns`, not a missing-feature (there is no server trigger to
  add).
- If a live capture shows the individual `OP_ZoneEntry` packets for far
  spawns genuinely never arrive on the wire, look at the reliable-stream/ack
  layer (`eq_net`'s session code) for a loss specific to a burst of many
  small individual packets right after the bulk packet — that's the
  EQEmu-side sending pattern to reproduce (1 bulk `OP_ZoneSpawns`-internal
  packet + up to `mob_list.size()` individual `OP_NewSpawn`-internal packets,
  all still wire-tagged `OP_ZoneEntry`, all queued back-to-back in the same
  `SendZoneSpawnsBulk` call).

Related: `zone-spawn-delivery.md` (opcode collapse mechanics),
`zone-line-crossing.md`, `zone-server-linkdead-timeout.md`.
