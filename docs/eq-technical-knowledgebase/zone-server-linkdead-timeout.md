# Zone-server linkdead detection: three timeouts, don't conflate them

Companion to [[eqstream-reliable-retransmit]] (transport/EQStream mechanics). That
file covers the wire protocol; this one covers the **zone-server `Client` state
machine** that decides "this client is linkdead" and traces it back to exactly
which transport timer fired.

## The three numbers in play

1. **`resend_timeout` = 30000ms** (`EQEmu/common/net/reliable_stream_connection.h:297`,
   no `RuleI` override — grep-confirmed absent from `common/ruletypes.h`). Age of
   the **single oldest specific un-ACKed reliable packet** the zone server sent
   to the client, checked every tic in `ProcessResend`
   (`reliable_stream_connection.cpp:1140-1161`). Fires even if the client is
   sending keepalives/other traffic just fine — this timer only cares whether
   **its own outbound reliables are getting ACKed**, nothing else. On fire it
   logs (category `net_client`) `"Closing connection for ... time_since_first_sent
   [...] >= ... resend_timeout [...]"` (`:1151-1159`) and calls `Close()` →
   `ChangeStatus(StatusDisconnecting)` immediately.
2. **`stale_connection_ms` = 60000ms** (`reliable_stream_connection.h:285`, also
   no `RuleI`). Time since the zone server received **anything at all** from the
   client (`m_last_recv`, bumped unconditionally at the very top of `ProcessPacket`,
   `reliable_stream_connection.cpp:446`, before opcode dispatch — even a bare
   `OP_KeepAlive`(0x06) counts, `:454-458`). Checked in
   `ReliableStreamConnectionManager::Process` (`:162-169`). **This is the one an
   application-level `OP_KeepAlive` every N seconds defends against** — any N
   comfortably under 60000ms (eqoxide uses 15000ms,
   `eqoxide/src/eq_net/gameplay.rs:18,364-367`) keeps this timer from ever
   firing.
3. **`RuleI(Zone, ClientLinkdeadMS)` = 90000ms**
   (`EQEmu/common/ruletypes.h:361`, DB override in
   `utils/sql/git/optional/2019_07_13_linkdead_changes.sql:1`). This is **not a
   detection timer at all** — it's the grace period the character's spawn stays
   in the zone as a limp "linkdead" body (visible to others, `AppearanceType::Linkdead`,
   `zone/client.cpp:4092-4095`, `zone/client_process.cpp:599-605`) *after*
   `client_state` has already flipped to `CLIENT_LINKDEAD`, before the server
   gives up, saves, and despawns it (`zone/client_process.cpp:159-183`,
   `zone/client.h:2177`). Common misdiagnosis: reading "client stays linkdead
   ~90s" and assuming that's the detection threshold — it's the *post-detection*
   window, detection already happened via #1 or #2 above (or a raw
   `eqs->CheckState(ESTABLISHED)` failure from an explicit
   disconnect/error/kick, `zone/client_process.cpp:622`).

## The state flip that actually matters
`Client::Process()` (`zone/client_process.cpp:583-607`): if
`client_state != CLIENT_LINKDEAD && !eqs->CheckState(ESTABLISHED)`, the client
is marked `CLIENT_LINKDEAD` **on the very next zone tic** after the transport
layer leaves `StatusConnected`. `EQStream::GetState()`
(`common/net/eqstream.cpp:239-251`) maps `ReliableStreamConnection`'s
`m_status` straight through: `StatusConnected → ESTABLISHED`,
anything else (`StatusDisconnecting`/`StatusDisconnected`) → not `ESTABLISHED`.
So there is **no separate zone-level idle timer** — the zone only ever asks
"is the EQStream still `StatusConnected`", and the transport layer (`resend_timeout`
or `stale_connection_ms`, both in `reliable_stream_connection.cpp`) is the sole
arbiter of that.

## Answering "does idle liveness depend on session keepalive alone, or also OP_ClientUpdate?"
- **Keepalive alone is sufficient against `stale_connection_ms`** — confirmed,
  see #2 above.
- **It is NOT sufficient against `resend_timeout`** — that timer only resets
  when the **client ACKs the server's outstanding reliable sends**, not by the
  client unilaterally sending anything of its own. If the zone server pushes a
  reliable packet to an idle client (it does: `Mob::SendHPUpdate`,
  `zone/mob.cpp:1522-1548`, fires on every HP/mana regen tick that actually
  changes `current_hp`, driven by `hpupdate_timer(2000)` /
  `mana_timer(2000)`, `zone/client.cpp:138,445` / `zone/mob.cpp:111` — so a
  resting-but-not-full-HP/mana idle character gets an occasional reliable
  `OP_HPUpdate` even doing nothing) and the client's ACK for that specific
  packet is lost or never generated, the server will retry it, then give up at
  30s **regardless of keepalive traffic**. A real client's idle `OP_ClientUpdate`
  cadence isn't recoverable from EQEmu server source (client-only decision);
  RoF2 `Handle_OP_ClientUpdate` (`zone/client_packet.cpp:4832-4863`) just
  accepts whatever size/cadence arrives — there's no server-side rule requiring
  a minimum client send rate. Server-side, `m_position_update_timer(10000)`
  (`zone/client.cpp:174,482`) exists purely to **paper over a client that has
  stopped sending `OP_ClientUpdate` while stationary**, broadcasting a synthetic
  position update to *other nearby clients* every 10s
  (`zone/client_process.cpp:114-117`) — it implies the real client can and does
  go quiet on `OP_ClientUpdate` while truly idle, and that's expected/handled,
  not itself a linkdead cause.

## eqoxide status (as of this investigation)
Already implemented/fixed in `eqoxide/src/eq_net/transport.rs`:
- `#254`/PR #255 — go-back-N reliable retransmit + cumulative `OP_ACK` +
  selective `OP_OUT_OF_ORDER` handling (`transport.rs:446-478,578-593`).
- `#158` — re-ACK a **duplicate** delivery (server retransmitting because our
  original ACK was lost) instead of silently dropping it, which otherwise left
  the server's copy permanently un-ACKed until its own 30s `resend_timeout`
  (`transport.rs:724-731`).
- `#127` — client's own `OP_ClientUpdate` position stream sent **unreliably**
  (`send_app_packet_unreliable`, `transport.rs:404-421`) so a lost position
  packet during movement can't stall the ordered reliable-receive window.
- 15s `OP_KEEPALIVE` cadence (`gameplay.rs:18,364-367`) comfortably beats the
  60s `stale_connection_ms`.

**Given all of the above is already in place, a *persisting* idle-linkdead
report should be diagnosed against `resend_timeout` (30s), not
`stale_connection_ms`** — i.e. look for a server→client reliable packet whose
ACK path breaks under some specific condition while otherwise idle (fragmented
single-packet payload, an ACK datagram itself lost with no self-heal because
the *client's* ACK send isn't itself retried/verified, etc.), not for "needs
more/faster client-side keepalives." Cross-check by grepping the zone log
(category `net_client`, must be enabled) for the exact string `"Closing
connection for"` (`reliable_stream_connection.cpp:1151`) vs the manager's
silent stale-connection erase (`:162-169`, no log line emitted there — its
absence in logs, paired with a `"Closing connection for ... resend_timeout"`
line, is itself the fingerprint that distinguishes the two root causes).

## Recommendation for further eqoxide work
1. To confirm which of the two transport timers is actually firing in a given
   idle-linkdead repro, get zone server logs with `net_client` logging enabled
   and grep for `"Closing connection for"` — present ⇒ `resend_timeout` (our
   ACK correctness bug); absent ⇒ `stale_connection_ms` (our keepalive not
   reaching the server at all — check the *session*/socket is actually the one
   the server thinks it's talking to, e.g. after a silent NAT/socket rebind).
2. Add a debug counter/log in `transport.rs::send_ack` for how many
   consecutive sequence numbers we've ACKed with no gaps, and a log in
   `deliver_seq`/`handle_ordered`'s `SeqClass::Future` branch — a client stuck
   in "buffered as out-of-order, gap never fills" for a specific seq during an
   otherwise-idle session is the concrete symptom that would explain
   `resend_timeout` firing despite healthy keepalives.
3. Don't chase `RuleI(Zone, ClientLinkdeadMS)` (90000ms) as a lever — it's
   downstream of detection, not the detection threshold.
