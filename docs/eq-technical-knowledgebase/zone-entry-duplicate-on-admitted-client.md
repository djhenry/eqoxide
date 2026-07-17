# Duplicate `OP_ZoneEntry` on an already-admitted client self-disconnects (RoF2)

Companion to [[zone-entry-handshake-race]] (which covers the *pre-auth-drop*
half of this problem — `OP_ZoneEntry` arriving before `Zone::AddAuth`). This
note covers the other half: what happens if the **first** `OP_ZoneEntry` was
already accepted and the zone is mid-zone-in, and a **second**, redundant
`ClientZoneEntry` lands on the **same** session before `OP_NewZone` arrives —
i.e. exactly the case an app-level retry timer (e.g. a blind 2.5s resend) can
trigger if it isn't told "stop, we're already admitted."

## Verdict

**UNSAFE.** A second `OP_ZoneEntry` on an already-admitted client is not
ignored, not NAK'd, and not merely wasteful — it makes the zone **disconnect
the client from itself** (`client->Disconnect()` where `client == this`,
`EQEmu/zone/client.h:486`), via a self-ghost false positive in the antighost
check. An unconditional "resend `OP_ZoneEntry` every N seconds until
`OP_NewZone`" is a live footgun, not a safe idempotent retry.

## 1. Is a second `OP_ZoneEntry` on the same session even re-dispatched?

Yes, for the entire zone-in window. Packet routing is gated by `client_state`
(`Mob::CLIENT_CONN_STATUS` — `CLIENT_CONNECTING`, `CLIENT_CONNECTED`,
`CLIENT_LINKDEAD`, `CLIENT_KICKED`, `DISCONNECTED`, `CLIENT_ERROR`,
`CLIENT_CONNECTINGALL`; enum at `EQEmu/zone/mob.h:96-97`), **not** by the
separate `conn_state` debug-only enum (`EQEmu/zone/client.h:2290-2303`, which
even says so in a comment: `//connecting states, used for debugging only`).

`Client::HandlePacket` (`EQEmu/zone/client_packet.cpp:445-515`) dispatches:

```cpp
switch (client_state) {
case CLIENT_CONNECTING: {
    ...
    p = ConnectingOpcodes[opcode];   // :477 — OP_ZoneEntry maps here
    (this->*p)(app);
    ...
}
case CLIENT_CONNECTED: { p = ConnectedOpcodes[opcode]; ... }
```

`client_state` is set to `CLIENT_CONNECTING` in the `Client` constructor
(`client.cpp:503`) and only flips to `CLIENT_CONNECTED` inside
`Client::CompleteConnect()` (`client_packet.cpp:518-521`), which is called
**only** from `Handle_Connect_OP_ClientReady` (`client_packet.cpp:1060-1067`)
— i.e. only after the client has driven the *entire* zone-in handshake
(`OP_ZoneEntry` → `OP_Weather`/`OP_NewZone` → `OP_ReqClientSpawn` →
`OP_ClientReady`). So **the entire window between "first `OP_ZoneEntry`
accepted" and "client sends `OP_ClientReady`" has `client_state ==
CLIENT_CONNECTING`**, meaning `OP_ZoneEntry` is *still* mapped in
`ConnectingOpcodes` (`client_packet.cpp:89`) and a duplicate delivery gets
redispatched straight back into `Handle_Connect_OP_ZoneEntry` — there is no
state-based short-circuit that ignores it.

## 2. Does the second `zone->GetAuth()` succeed?

**Yes — `GetAuth` is not one-shot/consuming.** `Zone::GetAuth`
(`EQEmu/zone/zone.cpp:1484-1507`) walks `client_auth_list` and, on a name
match, copies out the fields and sets `zca->stale = true` (`zone.cpp:1501`)
— **it does not `RemoveCurrent()` the entry**:

```cpp
bool Zone::GetAuth(...) {
    ...
    if (strcasecmp(zca->charname, iCharName) == 0) {
        ...
        zca->stale = true;
        return true;
    }
    ...
}
```

The entry is only ever pruned by the periodic `clientauth_timer` sweep in
`Zone::Process()` (`zone.cpp:1665-1677`), which needs **two** consecutive
ticks to remove a stale entry (`if stale: remove; else: mark stale`), and
that timer's period is `AUTHENTICATION_TIMEOUT * 1000` =
**60,000ms** (`AUTHENTICATION_TIMEOUT` = 60, `EQEmu/common/database.h:30`;
constructed at `zone.cpp:955`). A 2.5s app-level retry is nowhere near that
window, so **the second `GetAuth()` call finds the same still-present entry
and also returns `true`.** The auth-failure branch
(`client_packet.cpp:1270-1277`, `Kick("Failed auth check")` on a ghost /
silent `return` otherwise — see [[zone-entry-handshake-race]]) is **not** what
fires here; auth passes both times.

## 3. What actually goes wrong: the antighost check matches itself

Right after the (successful, both times) `GetAuth()` check,
`Handle_Connect_OP_ZoneEntry` runs an antighost lookup
(`client_packet.cpp:1266-1289`):

```cpp
/* Antighost code
tmp var is so the search doesnt find this object
*/
Client* client = entity_list.GetClientByName(cze->char_name);   // :1269
if (!zone->GetAuth(...)) { ... return; }                        // :1270-1277

strcpy(name, cze->char_name);                                    // :1279 — *** sets this->name ***
/* Check for Client Spoofing */
if (client != 0) {
    ...
    LogError("Ghosting client: ...");
    client->Save();
    client->Disconnect();                                        // :1288
}
```

`EntityList::GetClientByName` (`EQEmu/zone/entity.cpp:1826-1835`) is a
straight linear scan of `client_list` with **no self-exclusion**:

```cpp
for (const auto& e : client_list) {
    if (e.second && Strings::EqualFold(e.second->GetName(), name))
        return e.second;
}
```

The comment at :1266-1268 documents the intended trick: this `Client` object
was already inserted into `entity_list` **at UDP-stream-identify time**, well
before any `OP_ZoneEntry` is even received (`entity_list.AddClient(client)`,
`EQEmu/zone/main.cpp:569-570`), with `name` initialized to the literal
placeholder `"No name"` (`Client::Client(EQStreamInterface*)` constructor,
`client.cpp:387-388`). On the **first** `OP_ZoneEntry`, `this->name` is still
`"No name"`, so the lookup by `cze->char_name` can only match a genuine
leftover ghost session — not itself. That's the "trick."

**The trick is a one-shot guarantee, and the retry breaks it.** Line 1279
(`strcpy(name, cze->char_name)`) permanently sets `this->name` to the real
character name as an unconditional side effect of the *first* successful
pass — and it happens **before** any early return for the rest of the
function. On a **second** `OP_ZoneEntry` for the same session, `this->name`
is no longer `"No name"`; `entity_list.GetClientByName(cze->char_name)` now
finds **itself** (`client == this`), and the code runs the "ghosting" branch
against itself:

```cpp
inline void Disconnect() { eqs->Close(); client_state = DISCONNECTED; }
// EQEmu/zone/client.h:486
```

`client->Disconnect()` closes this client's own `EQStream` and flips
`client_state` to `DISCONNECTED`. Note the function does **not** early-return
after this — it keeps running the rest of `Handle_Connect_OP_ZoneEntry`
(reloads inventory/guild/group/bots/buffs a second time, re-queues PP,
spawn burst, weather, and even a **second, redundant**
`Handle_Connect_OP_ReqNewZone(nullptr)` call at :1876 for RoF+ clients — see
below) on a stream it just told itself to close. Whether any of that
second burst actually reaches the wire depends on `EQStreamInterface::Close`
semantics (graceful vs. immediate), but the load-bearing fact is:
**`client_state` is now `DISCONNECTED`.** Every subsequent packet from the
real client — including the `OP_ReqClientSpawn`/`OP_ClientReady` it's about
to send to finish zoning in — hits the `case CLIENT_KICKED: case
DISCONNECTED: case CLIENT_LINKDEAD: break;` arm of `HandlePacket`'s switch
(`client_packet.cpp:505-508`) and is **silently dropped**. The player is
functionally kicked out of the zone-in it had already successfully started,
with a duplicate-DB-load/duplicate-bot-spawn/duplicate-group-join side effect
already committed on the way down.

This is a genuine EQEmu server-side bug (the antighost check has no
`client != this` guard), not something eqoxide can "hold correctly" from the
client side except by avoiding triggering it.

## 4. Bonus: how long between admission and `OP_NewZone`, and is >2.5s plausible?

For RoF-and-later clients (RoF2 included), `OP_NewZone` is **not** gated
behind a client round trip through `OP_ReqClientSpawn`/`OP_ReqNewZone` the
way older clients are. `Handle_Connect_OP_ZoneEntry` short-circuits it
directly, in the *same* function call that processed the original
`OP_ZoneEntry`:

```cpp
/*
Weather Packet
This shouldn't be moved, this seems to be what the client
uses to advance to the next state (sending ReqNewZone)
*/
outapp = new EQApplicationPacket(OP_Weather, 12);
...
QueuePacket(outapp);                                    // :1860-1873

if (ClientVersion() >= EQ::versions::ClientVersion::RoF) {
    Handle_Connect_OP_ReqNewZone(nullptr);               // :1875-1877 — sends OP_NewZone directly
}

SetAttackTimer();
conn_state = ZoneInfoSent;
zoneinpacket_timer.Start();
return;                                                  // :1880-1883, end of Handle_Connect_OP_ZoneEntry
```

So the gap between "auth accepted" and "`OP_NewZone` queued" is bounded by
the **synchronous cost of the rest of `Handle_Connect_OP_ZoneEntry` itself**
— all the DB loads (character data, inventory, guild, group, bots, buffs,
AA, spell book, bandolier, etc., `:1319-1646`), `entity_list.SendZoneSpawnsBulk`
(serializes **every mob currently in the zone**, `:1764`), corpses, PVP,
inventory bulk-send — plus however long the reliable stream takes to drain
that whole burst onto the wire (uncapped by default:
`RuleR(Network, ClientDataRate)` defaults to `0.0` = "disabled",
`EQEmu/common/ruletypes.h:1016`, so no artificial egress throttle, but a
populous/raid zone's spawn burst is still non-trivial bytes over one
synchronous, single-threaded zone process).

**Confirmed plausible, not near-impossible.** On a warm, lightly-populated,
otherwise-idle zone this whole sequence is typically sub-second (mostly
local DB roundtrips + LAN/localhost send). But on a **cold zone boot**
(dynamic zone just spun up, `spawn2`/entity_list still populating, database
connection pool contention from other zones booting concurrently) or a
**heavily populated zone** (large bulk-spawn serialization,
`entity_list.SendZoneSpawnsBulk`), a multi-second stall is entirely
believable — this is precisely the same class of load spike the original
[[zone-entry-handshake-race]] note flags for the world→zone `AddAuth` race.
A 2.5s blind resend window is not comfortably clear of this.

## Recommendation for eqoxide

1. **Do not build an unconditional "resend `OP_ZoneEntry` every 2.5s until
   `OP_NewZone`" retry.** Per §3, this is not idempotent on the server — a
   redundant delivery after the first one was already admitted causes a
   guaranteed self-disconnect (`client->Disconnect()` on itself,
   `client_packet.cpp:1288`, `client.h:486`) that silently drops all further
   zone-in packets from that client (`client_packet.cpp:505-508`).
2. **Gate the retry on a state eqoxide itself controls, not blind time.**
   The moment the *first* `OP_ZoneEntry` for this zone connection has been
   sent, eqoxide should track "have I sent this already" and only resend if
   it's genuinely still un-acknowledged *at the transport layer* (i.e. still
   in the outstanding/unacked set — see [[zone-entry-handshake-race]] for why
   that specific case, an auth-not-ready silent drop, can leave nothing to
   retransmit and needs an app-level nudge). Once the zone's ACK for the
   original `OP_ZoneEntry` sequence number has been observed (proof the
   packet was delivered and dispatched, not proof it succeeded), **stop
   resending `OP_ZoneEntry`** — any wedge past that point is not an
   auth-race, and re-sending `OP_ZoneEntry` cannot help it (per this note) or
   can actively make it worse (self-disconnect).
3. **A safer bound than "keep resending until `OP_NewZone`" is "resend
   exactly once, then fall back to the existing 30s hard timeout."** Since
   the race this retry exists to fix (`AddAuth` not yet landed) resolves
   itself well within a second or two of the world→zone TCP hop
   (same-host/LAN, no WAN RTT — see [[zone-entry-handshake-race]] §"Answer to
   Q2"), one retry after e.g. 2-3s covers the realistic race window; a
   second/third automatic retry mostly just increases exposure to the §4
   scenario (first `OP_ZoneEntry` was actually fine, just slow) without
   meaningfully improving recovery odds for a genuinely-still-unauthed
   session (which, per [[zone-entry-handshake-race]] §5, may simply never
   auth and should hit the 30s hard timeout regardless).
4. If a bulletproof design is wanted: track whether the *specific*
   `OP_ZoneEntry` app packet eqoxide sent was ACKed by the transport
   (`EqStream`'s outstanding-set bookkeeping). Only fire the app-level resend
   if that packet is *still outstanding* after the window (transport-level
   evidence the first attempt may not have been delivered/dispatched at
   all); if it was ACKed, do **not** resend — silence past that point means
   "admitted, still working," not "lost," per §4's timing analysis, and a
   resend at that point is the unsafe case this note documents.
5. This is a real EQEmu server bug (missing `client != this` guard in the
   antighost check, `client_packet.cpp:1281`) worth flagging upstream
   independent of what eqoxide does — but eqoxide should not depend on it
   ever being fixed; treat "never send a second `OP_ZoneEntry` on an
   already-admitted session" as a hard client-side invariant.
