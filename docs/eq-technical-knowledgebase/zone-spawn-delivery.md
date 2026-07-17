# RoF2 spawn delivery: OP_ZoneEntry / OP_ZoneSpawns / OP_NewSpawn all collapse to ONE wire opcode

## The core fact

In `EQEmu/common/patches/rof2.cpp` there are only TWO real spawn-broadcast
encoders and one of them is a pass-through:

```
rof2.cpp:2358  ENCODE(OP_NewSpawn)  { ENCODE_FORWARD(OP_ZoneSpawns); }
rof2.cpp:4542  ENCODE(OP_ZoneEntry) { ENCODE_FORWARD(OP_ZoneSpawns); }
rof2.cpp:4575  ENCODE(OP_ZoneSpawns) { ... }   // the real body, shared by all three
```

Inside the shared body, for **every** `Spawn_Struct` record in the internal
(pre-encode) buffer — whether that buffer held 1 record (a single
`OP_ZoneEntry`/`OP_NewSpawn` call) or N records (a `BulkZoneSpawnPacket`,
`entity.cpp:3514`, internal opcode `OP_ZoneSpawns`) — the loop **always**
allocates a fresh `EQApplicationPacket` with the opcode **hardcoded to
`OP_ZoneEntry`** and queues it individually:

```
rof2.cpp:4660  auto outapp = new EQApplicationPacket(OP_ZoneEntry, PacketSize);
...
rof2.cpp:4934  dest->FastQueuePacket(&outapp, ack_req);   // inside the per-record for() loop
```

**Consequence (confirmed by reading the encoder, not just inferred):** on the
real RoF2 wire, there is no such thing as a genuine multi-record
`OP_ZoneSpawns`-opcode packet, and `OP_NewSpawn` (wire value `0x6097`,
`patch_RoF2.conf:265`) is never the opcode actually sent for a runtime
new-spawn either. **Every single spawn delivery — the initial zone-in roster,
a runtime "NPC just popped" broadcast, and a bulk resend — arrives as an
individual `OP_ZoneEntry` (wire `0x5089`, `patch_RoF2.conf:63`) packet, one
`Spawn_Struct` per packet.** `OP_ZoneSpawns` (wire `0x5237`,
`patch_RoF2.conf:66`) is effectively dead on the wire for spawn delivery.

This matches (and now hard-confirms with citations) the existing comment at
`eqoxide/src/eq_net/packet_handler.rs:1063-1067`. It does **not** confirm the
second half people sometimes assume ("so OP_ZoneSpawns is unreachable and
apply_zone_spawns is dead code") — that's a reasonable inference from this
same evidence but hasn't been checked against a live packet capture.

## A secondary finding: OP_NEW_SPAWN (0x6097) dispatch is probably unreachable too

`eqoxide`'s dispatch table separately routes wire opcode `OP_NEW_SPAWN`
(`0x6097`) to `apply_new_spawn` (`packet_handler.rs:16,848`), which has
corpse-specific side effects `apply_zone_entry` does NOT have — it queues
`gs.pending_loot` for auto-loot when a freshly-arrived NPC corpse spawn's name
contains "corpse" (`packet_handler.rs:867-874`). Since the server always
rewrites the wire opcode to `OP_ZoneEntry` (`0x5089`) per the finding above,
a real "an NPC just died, corpse just spawned" broadcast should arrive
wire-tagged `0x5089` and be routed through `apply_zone_entry` instead — which
does **not** queue auto-loot. **This is an inference from static code
reading, not yet verified against a live capture** — worth a packet-capture
check before treating it as confirmed, but if true it means runtime corpse
auto-loot may only ever fire for zone-in corpses (which flow through
`register_spawn`'s `is_corpse` flag anyway, per `packet_handler.rs:2514-2519`)
and never for corpses created by kills during the live session. Flagging for
follow-up; not the subject of this note's primary finding.

## Why this rules out a client decode/dispatch bug for "duplicate spawn_id" reports

`parse_rof2_spawn` (`eqoxide/src/eq_net/protocol/mod.rs:672`) reads
`spawn_id` directly off the wire (`mod.rs:708`, `let spawn_id = rd_u32!();`)
— the client never derives, increments, or fabricates it. `register_spawn`
(`packet_handler.rs:2512`) upserts into `gs.entities` keyed by exactly that
value (`packet_handler.rs:2564`, `gs.upsert_entity(Entity{ spawn_id:
info.spawn_id, ... })`). Combined with the single-opcode/one-record-per-packet
fact above (`apply_zone_entry` parses exactly one record per call, no loop),
**the client has no mechanism to produce two different `spawn_id`s for what
was genuinely a single incoming wire record.** Two different ids at the same
name+position in `gs.entities` can only mean the client received two
genuinely different `Spawn_Struct` wire payloads, i.e. the **server** created
and broadcast two distinct `Mob` objects.

## The server-side fingerprint: consecutive ids come from a FIFO id pool

```
EQEmu/zone/entity.cpp:280-282   // EntityList ctor: free_ids seeded 1..1500 in ascending order
    for (uint16 i = 1; i <= 1500; i++)
        free_ids.push(i);
EQEmu/zone/entity.cpp:1281-1296 // GetFreeID() pops the FRONT of that FIFO
EQEmu/zone/entity.cpp:671       // npc->SetID(GetFreeID());  — called at Mob/NPC construction
```

At zone boot (`zone.cpp:1192` → `content_db.PopulateZoneSpawnList`,
`zone/spawn2.cpp:475`), the `spawn2` rows for the zone are pulled with
**no `ORDER BY`** (`spawn2.cpp:508-515`, `Spawn2Repository::GetWhere(...
"TRUE {} AND zone = '{}' AND (version = {} OR version = -1)")`) — i.e.
natural/table order — and a `Mob`/`NPC` object is created per row in that
order. **Two `spawn2` rows that are adjacent in that natural scan order
(e.g. an accidentally-duplicated row from a bad content import/merge) yield
two `Mob` objects created back-to-back, which pull consecutive ids off the
`free_ids` FIFO** — exactly the `id, id+1` pattern (526/527, 818/819)
reported for gfaydark. This is the textbook fingerprint of a duplicated
`spawn2` row (or two distinct spawn2 rows placed at literally identical
coordinates), not a wire/decode artifact.

## What I could and couldn't verify locally

- Confirmed in code: the opcode-collapse behavior above, the id-FIFO
  mechanism, and that the client cannot manufacture a second id.
- **Could not** reach the live game DB (`jimbo.lan`, per
  `eq-infra-moved-to-jimbo.md`) from this environment — `nc -zv jimbo.lan
  3306` → "No route to host" — so I could not directly dump gfaydark's live
  `spawn2` rows for Geeda/Bidl Frugrin to show the duplicate row(s) by id.
- Queried the **local dev** `eqemu_mariadb_1` (`peq` DB) as a sanity check:
  `select x,y,z,count(*) c ... from spawn2 where zone='gfaydark' group by
  x,y,z having c>1` → exactly **one** exact-coordinate duplicate pair
  (`spawn2.id` 58511/9500, non-consecutive — consistent with NOT being
  created in the same population pass, so a different mechanism, e.g. one
  added later), and it is **not** Geeda/Bidl Frugrin (each has exactly one
  `spawn2` row locally, `spawngroupID` 1535 and 50672 respectively, both
  `chance=100`, single-entry spawngroups). This local snapshot is documented
  stale/drifted from the live jimbo DB (`eq-infra-moved-to-jimbo.md`), so it
  neither confirms nor refutes the live content — it just means the bug isn't
  reproducible from this local DB copy, and the live server's gfaydark
  `spawn2` content has almost certainly diverged (re-import, manual edit,
  etc.) to introduce ~155 duplicated placements that aren't in this snapshot.

## Recommendation for eqoxide / bug attribution

- **Attribute the duplicate-entity report to server-side `spawn2` content
  duplication on the live DB, not a client decode/dispatch bug.** The
  client's upsert-by-`spawn_id` behavior is correct given the wire contract;
  it's doing exactly what an honest client should do with two genuinely
  different server-assigned ids.
- If eqoxide wants a *display-layer* mitigation (not a "fix" — the underlying
  data is still wrong) for `/v1/observe/entities`, a name+exact-position
  dedup pass could collapse obvious duplicate NPCs for agent consumption, but
  clearly label it as `deduped` (agent-honesty invariant — don't silently
  drop entities without saying so) and keep the raw `gs.entities` map
  untouched, since e.g. hailing/looting logic should still be able to target
  either physical instance.
- The real fix is a **server/content fix**: query the live `spawn2` (or
  `spawngroup`/`spawnentry`) tables for gfaydark for rows resolving to
  duplicate coordinates and remove the extra one(s). This requires DB access
  on jimbo (not reachable from this sandbox) — hand off to whoever manages
  the live DB.
- Separately worth a follow-up (not blocking this bug): verify with a live
  packet capture whether `OP_NEW_SPAWN` (`0x6097`) ever actually appears on
  the wire; if not, `apply_new_spawn`'s corpse auto-loot side effect
  (`packet_handler.rs:867-874`) may be effectively dead code for
  runtime kills, and that logic should move into (or be duplicated into)
  `apply_zone_entry`.

Related: `spawn-struct-level-field.md`, `spawn-struct-race-equipment-branch.md`
(same `Spawn_Struct` wire family).
