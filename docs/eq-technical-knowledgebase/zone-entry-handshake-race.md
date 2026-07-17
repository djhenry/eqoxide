# Zone-hop reconnect: OP_ZoneServerInfo vs zone-auth race (world↔zone), and what OP_ZoneEntry-before-auth actually does

Companion to [[zone-line-crossing]] (trigger/destination resolution) and
[[eqstream-reliable-retransmit]] (transport ACK semantics this note leans on
heavily). This note covers the **world↔zone handoff timing** for a client
that is already in-game and crossing a zone line (i.e. reconnects to WORLD
with `zoning=1`), as opposed to a fresh character-select entry — EQEmu treats
these two cases **differently** and only one of them is synchronized.

## The two EnterWorld() paths are NOT symmetric

`World::Client::EnterWorld()` (`EQEmu/world/client.cpp:1402-1497`) branches on
`seen_character_select`:

```cpp
if (zone_server) {
    if (!enter_world_triggered) {
        ZSList::Instance()->DropClient(GetLSID(), zone_server);
        zone_server->IncomingClient(this);     // fires ServerOP_ZoneIncClient
        enter_world_triggered = true;
    }
}
...
if (seen_character_select) {
    // char-select → zone: SYNCHRONIZED round trip
    // sends ServerOP_AcceptWorldEntrance and WAITS for the zone's reply
    // before Clearance() is ever called (reply comes back async via
    // ServerOP_AcceptWorldEntrance case, world/zoneserver.cpp:770-779,
    // which calls client->Clearance(wtz->response)).
} else {
    // zone-to-zone movement — "should be preauthorized before they leave
    // the previous zone" (verbatim source comment, client.cpp:1494)
    Clearance(1);   // <-- fires IMMEDIATELY, same stack frame, no wait
}
```

- `seen_character_select` only ever flips true inside `Client::SendCharInfo()`
  (`world/client.cpp:230-241`, the char-select list send). A zoning reconnect
  (`is_player_zoning=true` from `OP_SEND_LOGIN_INFO.zoning==1`,
  `world/client.cpp:462`) explicitly skips the char-select flow
  (`!is_player_zoning` guards at `:476,516`), so a fresh `World::Client` object
  reconnecting mid-game **always** takes the `else { Clearance(1); }` branch —
  confirmed, this is exactly eqoxide's zone-hop path.
- `zone_server->IncomingClient(this)` (`world/zoneserver.cpp:1834-1852`) builds
  a `ServerOP_ZoneIncClient` packet and calls `SendPacket` on a
  `WorldTCPConnection` (`world/zoneserver.h:34` — `class ZoneServer : public
  WorldTCPConnection`) — a **fire-and-forget TCP send**, no ack/round-trip.
- `Clearance(1)` (`world/client.cpp:1499-1588`) sends `OP_ZoneServerInfo` to
  the client **in the same function call**, unconditionally (guarded only by
  "is there a zone server at all", not "has it registered my auth").

**So for the zone-to-zone case (the one eqoxide's reconnect path always
hits), World does NOT wait for the target zone to process
`ServerOP_ZoneIncClient`/`AddAuth` before telling the client the zone's
IP:port.** This is optimistic/unsynchronized by protocol design — the source
comment ("should be preauthorized before they leave the previous zone") is
aspirational, not enforced by this code path; the actual pre-leave
`ServerOP_ZoneToZoneRequest` round trip (`zone/worldserver.cpp:362-468`,
`world/zoneserver.cpp:781-899`) only checks **zone capacity**
(`numclients >= zone->GetMaxClients()`), it never calls `AddAuth` on the
ingress zone. `AddAuth` is registered only later, by the fire-and-forget
`ServerOP_ZoneIncClient` sent from the *new* `EnterWorld()` call after the
client reconnects to world.

**Answer to "does World wait for zone bootup+auth before sending
OP_ZoneServerInfo": No for the zone-hop case — fires optimistically.** (It
*is* synchronized for the char-select→zone case, via the
`ServerOP_AcceptWorldEntrance` round trip — don't conflate the two paths.)

## What the zone does with OP_ZoneEntry when auth hasn't landed yet

`Client::Handle_Connect_OP_ZoneEntry` (`EQEmu/zone/client_packet.cpp:1249-1277`):

```cpp
if (!zone->GetAuth(ip, cze->char_name, &WID, &account_id, &character_id,
                    &admin, lskey, &tellsoff)) {
    LogClientLogin("[{}] failed zone auth check", cze->char_name);
    if (nullptr != client) {       // only true for an existing ghost session
        client->Save();
        client->Kick("Failed auth check");
    }
    return;                        // <-- silent no-op for a brand-new session
}
```

For a genuinely new UDP session (no prior `Client*` in `entity_list` under
that name — the normal zone-hop case), a missing `zone->GetAuth()` entry just
`return`s. **No response packet is sent, no `Kick()`, no `Disconnect()`.** The
UDP session/EQStream itself is left alive and `StatusConnected`.

## The critical part: the transport layer ACKs the packet BEFORE the app handler ever runs

`ReliableStreamConnection::ProcessPacket` (`common/net/reliable_stream_connection.cpp:698-730`,
the `OP_Packet`/`OP_Packet2..4` case), on the `SequenceCurrent` branch (packet
arrives in expected order — the normal case for a freshly-connected session's
first reliable send):

```cpp
else {
    RemoveFromQueue(stream_id, sequence);
    SendAck(stream_id, stream->sequence_in);   // <-- ACK sent HERE
    stream->sequence_in++;
    StaticPacket next(...);
    ProcessDecodedPacket(next);                // <-- app dispatch happens AFTER
}
```

`SendAck` fires unconditionally, strictly before `ProcessDecodedPacket` (which
eventually reaches `Handle_Connect_OP_ZoneEntry`). **ACK generation is a pure
transport/session-layer concern, completely decoupled from what the app-level
opcode handler does with the payload — including "silently drop it."** This is
the same mechanism documented in
[[eqstream-reliable-retransmit]] for the general case; this note just traces
it through to the specific `OP_ZoneEntry`-before-`AddAuth` scenario.

Consequence: if the client's `OP_ZoneEntry` lands before `AddAuth` has run,
the zone **ACKs it and drops it** — it is delivered exactly once, the
sequence number is consumed, and the zone does nothing further. A resend of
the *same* sequence number (which a correctly-implemented reliable sender
wouldn't even attempt, since it already saw the ACK and removed the packet
from its own outstanding/unacked set) would in any case only hit the
`SequencePast` branch (`:718-720`, re-ACK the duplicate, no re-dispatch to
`ProcessDecodedPacket`) — so there is **no wire-level mechanism by which a
transport-layer retransmit can ever cause this specific `OP_ZoneEntry` to be
reprocessed once it has been ACKed once.**

## Answer to Q2 (silently ignore vs ack-but-refuse vs disconnect)

**(b), not (a):** the packet is ACKed at the session layer (so a client's own
reliable-retransmit machinery will see the ACK and stop retrying — it has no
way to know the app layer dropped it), while the zone silently declines to
process it further at the app level. It is **not (c)** either — the UDP
session is not torn down; it stays `StatusConnected` and will only die later
via the ordinary `stale_connection_ms` (60s) idle timeout if nothing else
keeps it alive (see [[eqstream-reliable-retransmit]] §3b /
[[zone-server-linkdead-timeout]]).

Net effect if the race is lost: the client is stuck in a fully-alive,
fully-ACKed, but permanently-inert UDP session — `OP_ZoneEntry` was consumed
and discarded, no `OP_NewZone`/`PlayerProfile`/spawn stream will ever arrive,
and `poll_resend`-style transport retransmit provides **zero** self-healing
because there is nothing left in the outstanding/unacked set to resend. The
only way to recover is an **app-level** retry: send a brand-new `OP_ZoneEntry`
app packet (new sequence number, goes through `ProcessPacket`'s
`SequenceCurrent` path again, gets dispatched to `Handle_Connect_OP_ZoneEntry`
again) after the client independently decides "no zone-in progress after N
seconds," by which point `AddAuth` will almost certainly have landed.

## eqoxide state (as of this investigation)

`run_zone_entry_handshake` (`src/eq_net/gameplay.rs:834-884`) currently has
**no such app-level retry** — it calls `stream.poll_resend()` every 10ms
(`gameplay.rs:853`) expecting transport retransmit to eventually get
`OP_ZONE_ENTRY` through, then simply times out after 30s
(`gameplay.rs:846,881-883`) if `OP_NEW_ZONE` never arrives. Per the finding
above, `poll_resend()` **cannot** repair this specific race — once
`OP_ZONE_ENTRY` is ACKed (which happens immediately, before the zone's auth
check even runs), eqoxide's own transport (`src/eq_net/transport.rs:497`,
mirrors the server's go-back-N/whole-window-resend design per
[[eqstream-reliable-retransmit]]) has nothing left in its outstanding set for
that packet and will never resend it. So if the race is lost, the current
code path is a **guaranteed 30s wedge**, not a "slower but eventually
succeeds" case.

The `sleep(Duration::from_millis(800))` this note was asked about
(`gameplay.rs:572`, between dropping the old zone stream and
`EqStream::connect` to the new one) is a **blind, fixed defensive margin**
against exactly this race — it has no relationship to any protocol
acknowledgment; it is pure "hope the world→zone TCP hop (usually localhost,
fast) beats the client's own UDP session setup + first reliable send (usually
slower — WAN round trip + fresh `OP_SessionRequest`/`OP_SessionResponse`
handshake)." In most real deployments the TCP hop genuinely is much faster,
which is presumably why an 800ms margin has been "enough" in practice — but
it is not a guarantee, and the protocol gives eqoxide (or a real client) no
positive signal that auth has landed; there is no ack/response for
`ServerOP_ZoneIncClient` that reaches the client.

## Recommendation for eqoxide

1. **Do not remove the pre-connect delay on the assumption that
   `poll_resend` will self-heal an auth race.** That assumption is false per
   the ACK-before-app-dispatch ordering shown above — confirmed in
   `reliable_stream_connection.cpp:698-730`. If the delay is removed with no
   replacement, an auth-race loss becomes a silent 30s hang, not a fast auto-
   retry.
2. **The robust fix is an app-level `OP_ZoneEntry` re-send, not a bigger
   sleep.** Inside `run_zone_entry_handshake` (or right after the initial
   `send_app_packet(OP_ZONE_ENTRY, ...)` in the `zone_redirect` branch,
   `gameplay.rs:566-600`), if `OP_NEW_ZONE` hasn't arrived within a short
   window (e.g. 2-3s — comfortably longer than any plausible world→zone TCP
   delay, comfortably shorter than the 30s hard deadline), call
   `send_app_packet(OP_ZONE_ENTRY, &cze)` **again** with the same payload.
   Because it's a fresh `send_app_packet` call it gets a new sequence number
   and will go through the zone's `ProcessPacket`→`ProcessDecodedPacket`→
   `Handle_Connect_OP_ZoneEntry` path again as a brand-new delivery — this
   *does* have a real chance of succeeding once `AddAuth` has landed, unlike
   relying on `poll_resend` retransmitting the original (already-ACKed)
   packet. This mirrors the only recovery path the protocol actually
   supports.
3. If keeping some fixed pre-connect delay as a low-risk first line of
   defense (reducing how often the retry in #2 is even needed), it can likely
   be shortened well below 800ms given the race window is bounded by a
   same-host/LAN TCP hop, not a WAN round trip — but treat this as a
   probability-reduction knob, not correctness; the retry in #2 is what
   actually closes the gap.
4. **Do not expect any explicit rejection/NAK from the zone** on an
   auth-not-ready `OP_ZoneEntry` — there is none (confirmed,
   `client_packet.cpp:1270-1277`, silent `return`). Absence of `OP_NewZone`
   within the retry window is the only signal available.
5. If `zone->GetAuth()` *would* eventually fail permanently (e.g. genuinely
   wrong LSID, not just a timing race), the zone still just silently drops
   every `OP_ZoneEntry` attempt forever with no NAK — eqoxide's existing 30s
   hard timeout (`gameplay.rs:846`) remains the correct backstop for that
   case; don't loop the app-level retry indefinitely.
