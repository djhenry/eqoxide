# Near spawns silently wiped by the FIRST `OP_NewZone`'s purge (inverts #463)

Companion to [[zone-spawn-delivery]] (opcode-collapse mechanics) and
[[zone-spawn-range-gating]] (confirms EQEmu never withholds a spawn by
distance — only picks bulk-vs-individual framing). Those two notes answer
"what does the wire carry"; this note answers "in what order does it arrive
relative to `OP_NewZone`, and what does that do to eqoxide's own purge logic."
**This is a CONFIRMED eqoxide root cause (live telemetry + source, both sides),
not just EQEmu ground truth** — see the Verdict section for the evidence.

## The server-side ordering fact (all confirmed, `EQEmu/zone/client_packet.cpp`,
`EQEmu/zone/entity.cpp`, `EQEmu/zone/client.cpp`)

Inside a single synchronous call to `Client::Handle_Connect_OP_ZoneEntry`
(triggered by the client's own `OP_ZoneEntry`), packets are queued to the
`EQStream` in this exact call order — and because `EQStream`'s `sequence_out`
counter increments once per `InternalQueuePacket` call
(`EQEmu/common/net/reliable_stream_connection.cpp:1567-1584`, confirmed in
[[eqstream-reliable-retransmit]] §4), **call order == wire sequence order ==
client delivery order** for this whole burst (single reliable stream, strictly
ordered delivery — a receiver only dispatches a packet to the app layer once
its sequence is `SequenceCurrent`; anything ahead of a gap is buffered, not
dispatched, until the gap fills):

1. **Self-spawn**, `client_packet.cpp:1752-1761`:
   `outapp = new EQApplicationPacket(OP_ZoneEntry, ...); FastQueuePacket(&outapp);`
   — default args (`ack_req=true`, `required_state=CLIENT_CONNECTINGALL`,
   `client.h:445`). Sent **immediately**, unconditional of `client_state`.
2. **`entity_list.SendZoneSpawnsBulk(this)`**, `client_packet.cpp:1764` →
   `entity.cpp:1351-1422`. For every mob in `mob_list` (no distance skip,
   [[zone-spawn-range-gating]]):
   - `max_spawns = min(100, mob_list.size())` (`entity.cpp:1357-1361`) —
     caps each internal `BulkZoneSpawnPacket` chunk; if the near-set exceeds
     this the packet auto-flushes mid-loop (`entity.cpp:3501-3504`), but
     ordering/gating below is unaffected.
   - **≤600u (near)**: batched via `BulkZoneSpawnPacket::AddSpawn`
     (`entity.h:641`) → flushed by `SendBuffer()` (`entity.cpp:3508-3524`),
     internal opcode `OP_ZoneSpawns`, **`pSendTo->FastQueuePacket(&outapp)`**
     (`entity.cpp:3517`) — again the **default** args: `ack_req=true`,
     `required_state=CLIENT_CONNECTINGALL`. Sent **immediately**, same as the
     self-spawn — `client_state` (still `CLIENT_CONNECTING` at this point in
     the handshake) is never checked.
   - **>600u, or PC illusioned as MinorIllusion/Tree (far/delayed)**:
     individual `EQApplicationPacket` via `Mob::CreateSpawnPacket`
     (internal opcode `OP_NewSpawn`, `mob.cpp:1238`), sent via
     **`client->QueuePacket(app, true, Client::CLIENT_CONNECTED)`**
     (`entity.cpp:1384`). `Client::QueuePacket` (`client.cpp:1160-1182`):
     `if (client_state != CLIENT_CONNECTED && required_state == CLIENT_CONNECTED) { AddPacket(app, ack_req); return; }`
     — since `client_state == CLIENT_CONNECTING` here, this branch is taken:
     the packet is **held** in the `clientpackets` deque
     (`Client::AddPacket`, `client.cpp:1132-1147`), **not sent to the
     transport at all yet.**
3. `SendZoneCorpsesBulk`, `SendZonePVPUpdates`, `OP_TimeOfDay`, tribute
   packets, etc. (`client_packet.cpp:1765-~1850`) — all sent immediately
   (`FastQueuePacket`/`QueuePacket` with no `CLIENT_CONNECTED` gate).
4. `OP_Weather` (`client_packet.cpp:~1860-1873`) sent immediately.
5. **`OP_NewZone` — FIRST delivery.** For `ClientVersion() >= RoF`
   (RoF2 included), `Handle_Connect_OP_ReqNewZone(nullptr)` is called
   **directly, in the same stack frame** (`client_packet.cpp:1875-1877`) —
   no round trip. This is the *last* thing queued in this handshake call.

**So on the wire: self-spawn → every near/bulk individual `OP_ZoneEntry`
record → corpses/PVP/time/tribute → weather → the FIRST `OP_NewZone`. The
near-spawn burst is strictly BEFORE the first `OP_NewZone`, not after it.**

The held far-spawn packets in `clientpackets` are only flushed later, by
`Client::SendAllPackets()` (`client.cpp:1149-1158`), called from
`Client::CompleteConnect()` (`client_packet.cpp:518-522`,
`client_state = CLIENT_CONNECTED; SendAllPackets();`), which itself only runs
from `Handle_Connect_OP_ClientReady` (`client_packet.cpp:1060-1067`) — i.e.
only after the **client** sends `OP_ClientReady`, which (per the standard
RoF2 handshake `OP_ZoneEntry → OP_Weather → OP_ReqNewZone → OP_NewZone(2nd)
→ ... → OP_ClientReady`) is well after **both** deliveries of `OP_NewZone`.
**Far/delayed spawns arrive strictly AFTER both `OP_NewZone`s; near/bulk
spawns arrive strictly BEFORE the first one.**

## The eqoxide-side mechanism this collides with

`apply_new_zone` (`src/eq_net/packet_handler.rs:915-973`) purges world state
on its first application and is guarded by a one-shot flag that's explicitly
designed only to stop the **second** `OP_NewZone` (arriving after
`OP_ReqClientSpawn`) from re-clearing:

```rust
// packet_handler.rs:920-927
// Apply at most once per zone-server session (#322). A zone-in delivers OP_NewZone twice — the
// server sends it while handling OP_ZoneEntry and again in reply to the OP_ReqNewZone we send on
// OP_Weather — and the second copy arrives AFTER OP_ReqClientSpawn, while the spawn/door stream
// it requested is landing. Re-running the clears below would wipe that stream (missing NPCs and
// doors after zoning), and re-log "Entered <zone>" + a second navigate/zone event. Both copies
// carry identical zone fields, so there is nothing to redo.
if gs.new_zone_applied { return; }
gs.new_zone_applied = true;
gs.doors.clear();
gs.entities.clear();
```

The comment's mental model: "all real spawn/door data arrives via the stream
that follows `OP_ReqClientSpawn`, i.e. after both `OP_NewZone`s — so a purge
on the *first* `OP_NewZone` is a safe, redundant-with-`begin_zone_in` no-op."
**That model is false for the near/bulk set per the server ordering proven
above.** The near-bulk burst is not "the stream that follows
`OP_ReqClientSpawn`" — it's part of the original `OP_ZoneEntry` response,
delivered and (per `run_zone_entry_handshake`, `gameplay.rs:834-884`, which
drains `net_rx` strictly in arrival order via `apply_packet` inside a
`while let Ok(packet) = net_rx.try_recv()` loop, `gameplay.rs:854-876`)
**registered into `gs.entities` before the first `OP_NewZone` is even
received.**

Sequence, per-packet, as eqoxide processes it:

1. `begin_zone_in()` (`gameplay.rs:844`, before `OP_ZoneEntry` is even sent)
   clears `gs.entities`/`gs.doors` and sets `gs.new_zone_applied = false`
   (`game_state.rs:794-810`) — correct, guards against stale prior-zone state.
2. Self-spawn `OP_ZoneEntry` arrives → `apply_zone_entry` → `register_spawn`
   (player's own entity registered).
3. **Near-bulk individual `OP_ZoneEntry` records arrive** (Katie,
   Guard Frostfallen, ...) → `apply_zone_entry` → `register_spawn` for each.
   `gs.entities` now correctly holds them.
4. **First `OP_NewZone` arrives** → `apply_new_zone` → `gs.new_zone_applied`
   is still `false` (only `begin_zone_in` had touched it) → the guard does
   **not** block → `gs.doors.clear(); gs.entities.clear();` runs →
   **Katie and Guard Frostfallen (and every other near spawn registered in
   step 3) are wiped.**
5. `OP_REQ_CLIENT_SPAWN` sent (`gameplay.rs:860`), second `OP_NewZone`
   eventually arrives but is now correctly no-op'd by the one-shot guard.
6. Far/delayed spawns (>600u), which the *server* held back until
   `CompleteConnect()` (well after this point), arrive and register normally
   into the now-empty `gs.entities` — **nothing purges again**, so they
   survive.

Net effect: **spawns delivered in the near/bulk burst (everyone within
600u of the player at zone-in — exactly the "standing right next to the
player" case in the bug report) are registered then silently discarded by
the first `OP_NewZone`'s purge; spawns delivered later in the delayed/far
queue survive because nothing purges after they land.** This is the exact
inverse of #463 (which was about far spawns >600u going missing) — same
600u/`SendZoneSpawnsBulk` split, opposite side wiped, because the point in
the handshake where eqoxide purges falls **between** the two halves of the
server's own delivery, not before both (as `begin_zone_in` alone would
guarantee) or after both.

## Verdict / confidence

- **Confirmed via source, both sides:** the exact server queuing/gating code
  (§1) and the exact eqoxide purge/one-shot code and its drain-order
  contract (§2). The causal chain — near-bulk lands before first
  `OP_NewZone`, first `OP_NewZone`'s purge is not guarded against firing
  mid-stream, far spawns land after all purges are done — is airtight given
  these two source facts and the confirmed strictly-ordered reliable-stream
  delivery ([[eqstream-reliable-retransmit]] §4-5).
- **CONFIRMED LIVE (2026-07-17, #527rc), via the #532 packet-telemetry rig.**
  A `gmkblr` client zoned into gfaydark with `EQOXIDE_PKTLOG=1` from t=0 and
  `GET /v1/observe/packets?op=0x5089` captured the whole zone-in burst:
  - **416** individual `OP_ZoneEntry` (`0x5089`) packets arrived in a single
    ~456ms burst (t≈4350–4806ms) — matching gfaydark's **416** `spawn2` rows
    in the EQEmu DB (`SELECT COUNT(*) FROM spawn2 WHERE zone='gfaydark'`).
    There were ZERO wire `OP_ZoneSpawns` (`0x5237`) packets, confirming the
    ENCODE-splits-into-individual-`OP_ZoneEntry` framing.
  - `GET /v1/observe/entities` held only **332**; **80** decoded spawns were
    received-but-absent. Cross-referencing each packet's transport `rel_seq`
    against the entity map: the **first 82 spawns (rel_seq 37–219, earliest in
    the burst) were ALL absent**, and everything from **rel_seq 220 onward was
    present** — except exactly 3 genuinely `bodytype∈{11,60}`-filtered utility
    spawns (`zone_controller001`, `A_Mystic_Voice002`, `Faydwer_Spires002`).
    A clean prefix wipe at one point in the ordered stream — the signature of
    a single mid-burst `entities.clear()`, not a scattered decode failure.
  - The wiped prefix included players **Katie** (id 511) and **Acceptancetest**
    (id 506) and NPC **Guard_Frostfallen002** (id 1291) — precisely the
    "standing right next to the player" entities the native client showed and
    eqoxide could not see. **NO** `parse_rof2_spawn` failure warnings were
    logged, ruling out decode/transport loss: every one of the 416 parsed, and
    the mid-burst `apply_new_zone` purge alone accounts for the 82 lost.
- **Fixed** by removing the purge from `apply_new_zone` (Preferred fix #1
  below). Regression tests `first_new_zone_does_not_wipe_near_spawns_that_
  preceded_it` and `next_zone_in_purges_the_prior_zone_via_begin_zone_in`
  (`src/eq_net/packet_handler.rs`) mutation-checked: they go RED if the two
  `clear()` lines are restored to `apply_new_zone`.

## Recommendation for eqoxide

The purge in `apply_new_zone` needs to not depend on "this is the first
`OP_NewZone` of the session" as its safety condition — that's exactly the
condition that's true for BOTH the (harmless, already-empty) case
`begin_zone_in` was written for AND this (harmful, near-spawns-already-landed)
case. Two independent, both-real problems (#322 and this one) share one
one-shot flag that only solves #322.

1. **Preferred fix:** don't purge inside `apply_new_zone` at all — `begin_zone_in`
   already clears `gs.entities`/`gs.doors` unconditionally at the true start
   of every zone-in (`gameplay.rs:844`, before `OP_ZoneEntry` is even sent,
   which is provably before ANY spawn packet of the new session can arrive).
   `apply_new_zone`'s purge is redundant with that for the *legitimate*
   first-`OP_NewZone` case and actively harmful for the near-spawn race
   documented here. Removing `gs.doors.clear(); gs.entities.clear();` from
   `apply_new_zone` (packet_handler.rs:934-935) and relying solely on
   `begin_zone_in`'s pre-handshake clear should fix both #322 (already
   guarded by the one-shot, now simply unnecessary) and this near-spawn wipe,
   with no loss of the "purge stale prior-zone state" property (still
   guaranteed once at the top of `begin_zone_in`).
2. **If some clear-inside-`apply_new_zone` behavior is load-bearing for a
   case not covered by `begin_zone_in`** (e.g. a server-initiated zone push
   that doesn't route through `run_zone_entry_handshake` — worth checking
   whether every `OP_NewZone` receipt is guaranteed to be preceded by a
   `begin_zone_in()` call, per the other `begin_zone_in()` call sites at
   `packet_handler.rs:2875,2957,2981,2986,5101`), keep the purge but gate it
   on something that actually distinguishes "before this session's near-bulk
   arrived" from "after" — e.g. purge only if `gs.entities.is_empty()` is
   *not* the trigger (empty is expected either way); a real fix needs a
   marker set at the true start of the handshake (which `begin_zone_in`
   already sets, if reused) rather than "first `OP_NewZone` seen," since this
   note proves the near-bulk always arrives before that.
3. Add a regression test mirroring the existing `#322` test at
   `packet_handler.rs:2951-2967` but for **this** ordering: register a near
   spawn (simulating the bulk burst), *then* feed a well-formed first
   `OP_NewZone`, and assert the near spawn survives — the existing test only
   proves the *second* `OP_NewZone` doesn't wipe a *post*-NewZone stream; it
   does not cover a pre-NewZone stream being wiped by the *first* `OP_NewZone`,
   which is exactly this bug.
4. Do not "fix" this by delaying/reordering on the eqoxide side (e.g. holding
   `apply_zone_entry` registrations until after `OP_NewZone`) — the server
   ordering (near-bulk before first `OP_NewZone`) is fixed EQEmu behavior,
   not something eqoxide can or should negotiate around; the client must
   simply not discard already-valid registrations.

Related: [[zone-spawn-delivery]], [[zone-spawn-range-gating]],
[[eqstream-reliable-retransmit]], [[zone-entry-handshake-race]],
[[zone-entry-duplicate-on-admitted-client]] (same handshake window, different
failure mode).
