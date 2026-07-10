# EQStream / ReliableStreamConnection wire protocol (RoF2 world+zone transport)

Ground truth: `EQEmu/common/net/reliable_stream_connection.{h,cpp}` (the "EQStream"
UDP session layer used by RoF2 world/zone, wraps `common/net/eqstream.cpp` which
does opcode-manager translation). This is confirmed current EQEmu server source
(not Titanium-specific — this layer hasn't diverged by client version; RoF2 uses
it identically to Titanium at the transport level. Only the app-opcode table /
struct payloads differ by client build, which is handled one layer up in
`eqstream.cpp`/`opcodemgr`).

## Protocol opcode enum (byte 1, after a leading 0x00 marker byte)
`reliable_stream_connection.h:37-66`. Four parallel stream "channels" exist
(stream_id 0-3): `OP_Packet`/`OP_Packet2..4` (0x09,0x0a,0x0b,0x0c),
`OP_Fragment..4` (0x0d-0x10), `OP_OutOfOrderAck..4` (0x11-0x14),
`OP_Ack..4` (0x15-0x18). **EQEmu only ever uses stream 0** — `EQStream::QueuePacket`
(`eqstream.cpp:117-122`) calls `m_connection->QueuePacket(out)` with no stream
arg, and `ReliableStreamConnection::QueuePacket(Packet&)` defaults to stream 0
(`reliable_stream_connection.cpp:389-391`). So for a real client/server session
you only ever see `OP_Packet`(0x09)/`OP_Fragment`(0x0d)/`OP_Ack`(0x15)/
`OP_OutOfOrderAck`(0x11) — the *2/*3/*4 variants are dead code paths in practice.

## Wire header layout
`ReliableStreamReliableHeader` (`reliable_stream_structs.h:105-119`): `zero:u8=0`,
`opcode:u8`, `sequence:u16`. `sequence` is serialized through
`HostToNetwork`/`NetworkToHost` (`reliable_stream_connection.h`'s callers, impl
in `common/net/endian.h:48-66`) which byte-swaps on a little-endian host — i.e.
**sequence is big-endian on the wire**, confirming the user's assumption. This
exact header shape is reused verbatim for `OP_Ack` and `OP_OutOfOrderAck`
payloads (`SendAck`/`SendOutOfOrderAck`, `reliable_stream_connection.cpp:1307-1331`)
— so an Ack/OOO-Ack packet is just `[00][opcode][seq_hi][seq_lo]`, 4 bytes total.

## 1. OP_Ack is CUMULATIVE (sliding-window base advance)
`ReliableStreamConnection::Ack(stream, seq)` (`reliable_stream_connection.cpp:1254-1279`):
iterates every entry in `sent_packets` (a `std::map<uint16_t, ReliableStreamSentPacket>`
keyed by that packet's own send sequence) and erases (=treats as fully acked) any
entry whose sequence satisfies `CompareSequence(seq, entry_seq) != SequenceFuture`,
i.e. `entry_seq <= seq` in wraparound-aware terms. **Every outstanding packet with
seq ≤ the acked seq is dropped from the resend set**, not just the one named.
`CompareSequence` (`reliable_stream_connection.cpp:1617-1638`) is a signed-diff
wraparound comparator with a ±10000 threshold to disambiguate wrapped u16 space.

Generation side (how the peer emits an Ack, so you can mirror it): on receiving
an in-order reliable packet, `sequence_in` is incremented and
`SendAck(stream_id, stream->sequence_in)` is sent — i.e. **the ack seq equals the
new expected-next sequence**, not "last good seq". Compare: on a duplicate/stale
(`SequencePast`) reliable packet it re-sends `SendAck(stream_id, stream->sequence_in - 1)`
— a duplicate-ack naming the last cumulatively-complete sequence. Both paths at
`reliable_stream_connection.cpp:713-724` (OP_Packet) and `:742-754` (OP_Fragment).
**Caveat:** because the ack value on the fast path is `sequence_in` (post-increment,
i.e. "next expected"), and `Ack()`'s cumulative-erase test is `entry_seq <= seq`,
sending an ack with value `sequence_in` after incrementing correctly still only
clears entries ≤ the packet just processed (since `sequence_in` before increment
== that packet's own seq, and entries are keyed by their own send seq, so `entry_seq
<= sequence_in` after increment includes the just-acked packet and everything
older — this is intentional cumulative semantics, just be aware the numeric value
you receive is "one past the last fully-received seq" on the fast path but
"exactly the last fully-received seq" on the duplicate/stale path). Either way:
**when you as sender receive `OP_Ack(seq)`, drop from your resend/unacked set
every packet whose own sequence is ≤ seq** (mod wraparound) — that's the safe,
correct behavior regardless of which of the two paths produced it.

## 2. OP_OutOfOrderAck is SELECTIVE (single-entry), NOT a retransmit trigger by itself
`OutOfOrderAck(stream, seq)` (`reliable_stream_connection.cpp:1281-1299`): does
`sent_packets.find(seq)` and erases **only that one exact entry** if present — no
cumulative effect on lower/higher sequences.

Generation: when a reliable `OP_Packet`/`OP_Fragment` arrives whose sequence is
`SequenceFuture` relative to `sequence_in` (i.e. it arrived ahead of the gap),
the receiver calls `SendOutOfOrderAck(stream_id, sequence)` where `sequence` is
**the sequence number of the packet that just arrived out of order** (the one
ahead of the gap), NOT the missing/gap sequence
(`reliable_stream_connection.cpp:713-716` for OP_Packet, `:742-746` for
OP_Fragment). The receiver also buffers that packet in `packet_queue`
(`AddToQueue`, `:554-565`) for later in-order delivery once the gap fills
(`ProcessQueue`, `:524-541`).

So semantically: `OP_OutOfOrderAck(seq)` means **"I received your packet #seq,
but it's ahead of what I actually need next — I've buffered it, don't bother
resending #seq specifically, but I'm still missing everything between my last
cumulative ack and #seq."** It is purely a bookkeeping optimization (stop
resending the one packet that *did* arrive) — it is **not** a NACK/fast-retransmit
signal and does not itself cause any immediate resend of the gap.

## 3. Actual retransmission is periodic, timer-driven, whole-window ("go-back-N"), not single-packet
`ProcessResend(stream)` (`reliable_stream_connection.cpp:1126-1252`), called every
tic from `ReliableStreamConnectionManager::ProcessResend()`:
- Looks only at the **oldest** unacked entry (`sent_packets.begin()`, map is
  ordered by seq) to decide *whether* to run a resend pass this tic: skip
  (`return`) if `time_since_first_sent <= first_packet.resend_delay` and no ack
  has landed since the last pass (`:1175-1186`).
- If it decides to run, it resends **every entry currently in `sent_packets`
  for that stream** in one pass (`:1205-1248`) — true go-back-N of the whole
  outstanding window, not just the oldest packet — capped per pass at
  `MAX_CLIENT_RECV_PACKETS_PER_WINDOW = 300` packets / `MAX_CLIENT_RECV_BYTES_PER_WINDOW
  = 140*1024` bytes (`reliable_stream_connection.cpp:28-29`, enforced at `:1206-1219`).
- Each resent entry's own `resend_delay` is then doubled and clamped to
  `[resend_delay_min, resend_delay_max]` (`:1243-1247`) — exponential backoff,
  tracked **per packet**, but the *decision to enter the loop at all* each tic
  is gated only by the oldest packet's timer.
- **Hard session drop:** if the oldest unacked packet's age
  (`time_since_first_sent`) reaches `resend_timeout` (default **30000 ms**,
  `reliable_stream_connection.h:297`, no `RuleI` override exists for it — grep
  confirmed no `resend_timeout` rule in `common/ruletypes.h`), the connection
  calls `Close()` immediately and gives up (`:1148-1161`). There is **no
  separate max-retry-count** field checked — it is purely elapsed-time based.

Initial per-packet `resend_delay` and backoff parameters
(`InternalQueuePacket`, `:1523-1526` / `:1555-1558`):
`resend_delay = clamp(rolling_ping * resend_delay_factor + resend_delay_ms, resend_delay_min, resend_delay_max)`,
with `rolling_ping` seeded to 500 ms at connection creation
(`reliable_stream_connection.cpp:338`/`363`) and updated as an EWMA
(`(old*2+sample)/3`) on every Ack/OutOfOrderAck (`:1268`, `:1292`).

**Struct hardcoded defaults** (`reliable_stream_connection.h:277-300`):
`resend_delay_ms=30`, `resend_delay_factor=1.25`, `resend_delay_min=150`,
`resend_delay_max=5000`, `resend_timeout=30000`, `keepalive_delay_ms=9000`,
`stale_connection_ms=60000`, `connect_stale_ms=5000`, `connection_close_time=2000`.

**Actual RoF2 zone/world runtime values** — `zone/main.cpp:540-543` and
`world/main.cpp:309-312` override the first four from Rules
(`common/ruletypes.h:1011-1016`, category `Network`):
`ResendDelayBaseMS=100`, `ResendDelayFactor=1.5`, `ResendDelayMinMS=300`,
`ResendDelayMaxMS=5000`. **`resend_timeout` (30000ms), `keepalive_delay_ms`
(9000ms), `stale_connection_ms` (60000ms), and `connect_stale_ms` (5000ms) are
NOT exposed as rules** — they stay at the struct defaults above unless a given
server's launcher code (not found in zone/world/main.cpp) sets them directly.

So with default rules, first resend attempt ≈ `clamp(500*1.5+100, 300, 5000)` =
**850 ms** after initial send, then doubles each subsequent pass
(850→1700→3400→5000-capped→5000→...) until 30000 ms total age is hit and the
session is dropped.

## 3b. Two independent drop conditions — don't conflate them
- **`resend_timeout` (30000ms default):** age of the single oldest *specific*
  unacked reliable packet, checked in `ProcessResend` — this is what your
  retransmit implementation must beat by actually getting acks flowing again.
- **`stale_connection_ms` (60000ms default):** time since the peer has received
  **anything at all** from you (`m_last_recv`, updated unconditionally at the
  top of `ProcessPacket`, `reliable_stream_connection.cpp:446`, before any
  opcode dispatch — even an unparsed/garbage packet bumps it), checked in
  `ReliableStreamConnectionManager::Process` (`:162-168`). `OP_KeepAlive`
  (0x06) is special-cased to short-circuit immediately after bumping
  `m_last_recv` (`:454-458`) — it needs no ack and isn't in the main opcode
  switch at all (an unhandled OP_KeepAlive would otherwise hit the `default:`
  "Unhandled opcode" branch at `:865-869`, but it never reaches there).
  **The peer (EQEmu, playing either client or server role — same `Process()`
  code path) sends `OP_KeepAlive` every `keepalive_delay_ms` (9000ms) of no
  outgoing traffic** (`:180-186`), but that only refreshes *your* `m_last_recv`,
  not theirs — you must yourself periodically send something (a real reliable
  packet, or at minimum your own ack traffic) or the peer's 60s stale timer
  will fire even with zero packet loss. In practice normal app traffic + your
  acks for their sends is enough during active gameplay; during idle you
  should send `OP_KeepAlive` yourself if nothing else goes out within ~9s.
  "Dropped by world CLE subsystem" (`zone/worldserver.cpp:634,660`) is a
  higher-level consequence of the zone↔world CLE sync noticing the client
  entry vanished after either of these drops — it is not itself a distinct
  timeout to satisfy.

## 4. Sequence space
- **One monotonic `sequence_out` counter per stream (u16, wraps), shared by
  `OP_Packet` and `OP_Fragment`.** Confirmed in `InternalQueuePacket`
  (`reliable_stream_connection.cpp:1488-1588`): the non-fragmented path uses
  `header.sequence = HostToNetwork(stream->sequence_out); ... stream->sequence_out++;`
  (`:1567-1584`); the fragmented path assigns a fresh `stream->sequence_out++`
  to the first fragment header AND to every subsequent fragment chunk in the
  `while` loop (`:1508,1527-1528`, `:1538,1559-1560`) — **every individual
  fragment consumes its own sequence number**, not just the first.
- **`OP_Combined` (0x03) carries no sequence of its own.** It's a UDP-frame-level
  wrapper: length-prefixed concatenation of complete sub-packets (each with its
  own `[00][opcode]...` header, so if a sub-item is `OP_Packet`/`OP_Fragment` it
  has its own sequence) — see the recursive `ProcessDecodedPacket` call at
  `reliable_stream_connection.cpp:574-593`.
- **`OP_AppCombined` (0x19) also carries no sequence of its own** and is a
  different, payload-level combining scheme: it packs multiple *raw application
  packets* (no `[00][opcode]` reliable-protocol framing on the sub-items) inside
  the payload of a single already-sequenced `OP_Packet`/`OP_Fragment`, using a
  length-prefix scheme (1-byte len, or `0xFF+2-byte` len, or `0xFF 0xFF 0xFF
  +4-byte` len for big payloads) — `:596-645`. Confirmed receive-only in this
  file (no send-side generator found in `common/net/`) — i.e. **the RoF2
  client is the one that emits `OP_AppCombined`** to batch several app packets
  under one reliable sequence; EQEmu server only ever needs to parse it.
- **No explicit max-in-flight/window-size cap on sending** — `sequence_out`
  free-runs and `sent_packets` grows unbounded until acked; the only "window"
  concept is the resend-pass cap (300 pkts / 140KB per tic, §3) and the
  ±10000 wraparound-disambiguation threshold in `CompareSequence`
  (`:1617-1638`), which is a classification heuristic, not an enforced limit.

## 5. Zone handshake / new session
Each `ReliableStreamConnection` is a fresh object per UDP session — both
constructors (`:322-346` as-server, `:349-369` as-client) go through
`ReliableStream`'s default member-initializers (`reliable_stream_connection.h:220-225`):
`sequence_in = 0; sequence_out = 0;` for all 4 stream slots, plus a fresh
`connect_code`/`encode_key`/CRC/encode-pass negotiation via
`OP_SessionRequest`/`OP_SessionResponse`. **There is nothing to carry over
across a zone boundary** — world login gets one session, each zone entry gets
an entirely independent one starting at sequence 0. A correctly-implemented
fresh session (new `OP_SessionRequest` → wait for `OP_SessionResponse` before
relying on encode/CRC parameters, though nothing in the code actually
*blocks* sending reliable packets pre-handshake — `QueuePacket`/`InternalQueuePacket`
has no status guard) with the same retransmit logic as world naturally covers
post-`OP_ZoneEntry` reliable sends (e.g. `ReqClientSpawn`) — no special-casing
needed beyond "this is a new stream, seq starts at 0, don't reuse any world-session
resend state."

## Recommendation for eqoxide's client-side sender
1. Track outstanding reliable sends in a `BTreeMap<u16, SentPacket{bytes,
   first_sent, last_sent, resend_delay}>` per stream (stream 0 only needed
   in practice, but keep the field for protocol fidelity).
2. On `OP_Ack(seq)` received: cumulatively remove every entry with
   `seq_of_entry <= seq` (wraparound-aware, mirror `CompareSequence`'s
   ±10000 heuristic in u16 space) from the outstanding map — do not treat it
   as "ack exactly this one."
3. On `OP_OutOfOrderAck(seq)` received: remove **only** the exact entry
   `seq` if present; take no other action. Do not use it as a fast-retransmit
   trigger — you don't need to react by immediately resending anything else;
   your periodic resend timer will naturally catch the real gap.
4. Run a periodic resend tic (e.g. every ~50-100ms, matching the server's own
   `tic_rate_hertz=60.0` cadence is unnecessary — any interval smaller than
   your min resend_delay is fine): if the oldest outstanding entry's age
   exceeds its own `resend_delay` (seed ≈ 850ms with an initial 500ms RTT
   guess and default Rule values, doubling per resend, clamped to
   `[300ms, 5000ms]`), resend **every** currently-outstanding entry for that
   stream (go-back-N), then double each resent entry's own delay (clamped).
5. If the oldest outstanding entry's total age reaches **30000ms**, treat the
   session as dead client-side too (matches server's `resend_timeout`) —
   no point continuing past the point the server will have already dropped you.
6. Separately, if you have sent nothing (no app packet, no ack, no keepalive)
   in ~9000ms, send `OP_KeepAlive` (0x06, 2-byte packet: `[00][0x06]`, no
   sequence) so the peer's 60000ms `stale_connection_ms` silence timer doesn't
   fire independently of packet loss.
7. Give `OP_Packet`/`OP_Fragment` one shared monotonic `u16` counter per
   stream; every fragment chunk gets its own sequence (don't reuse the first
   fragment's sequence for the rest).
8. New zone session = brand-new sequence/ack state from 0; don't carry
   anything from the world-server stream into the zone-server stream.
