//! EQ transport layer: UDP stream, session management, CRC, compression, fragmentation.
//!
//! Ported from the Python reference at eq_client/connection/stream.py.

use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::net::SocketAddr;
use std::time::Instant;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::protocol::*;

/// Upper bound on datagrams drained (and ACKed) per `poll_recv` call. Generous enough to clear a
/// zone-in spawn burst in one wake, but bounded so a sustained flood can't monopolize the loop —
/// anything beyond this drains on the next wake ~10 ms later. (#153)
const MAX_DATAGRAMS_PER_POLL: usize = 4096;

// ── Reliable-send retransmission (#254) ──────────────────────────────────────
// Mirrors EQEmu's Network resend rules (zone/world main.cpp: base=100ms, factor=1.5, min=300,
// max=5000) with rolling_ping seeded at 500ms → first resend ≈ 500·1.5 + 100 = 850ms, then doubled
// per attempt and clamped. The server drops the session (resend_timeout) at 30s of an un-ACKed oldest
// packet, so resending well inside that window keeps the ordered stream alive.
const RESEND_BASE_MS: u64 = 850;   // first-attempt delay
const RESEND_MIN_MS:  u64 = 300;   // clamp floor
const RESEND_MAX_MS:  u64 = 5000;  // clamp ceiling (steady-state cadence on a dead link)
/// Cap on datagrams resent in a single go-back-N pass (EQEmu MAX_CLIENT_RECV_PACKETS_PER_WINDOW).
const MAX_RESEND_PER_PASS: usize = 300;

/// How often `connect` re-sends OP_SESSION_REQUEST while waiting for OP_SESSION_RESPONSE. This
/// establishes ONLY the UDP session (the transport handshake) — it says nothing about whether the
/// zone has yet ACCEPTED us at the application layer. That distinction is the whole subject of
/// `~/git/eq_kb/zone-entry-handshake-race.md`: on the `zoning==1` reconnect path
/// World fires OP_ZoneServerInfo optimistically and registers our zone auth out-of-band via a
/// fire-and-forget TCP `ServerOP_ZoneIncClient` → `Zone::AddAuth`. A cold on-demand zone answers
/// SESSION_RESPONSE as soon as its listener binds — which can be BEFORE that auth lands — so a fast
/// session handshake here actually pushes OP_ZoneEntry out SOONER and WIDENS the auth race, not
/// narrows it. This constant does NOT resolve that race, and neither `poll_resend` nor an app-level
/// re-send can safely recover a lost race once the entry is session-ACKed (see `run_zone_entry_handshake`
/// in gameplay.rs and zone-entry-duplicate-on-admitted-client.md — a second OP_ZoneEntry self-kicks an
/// admitted client): a lost auth race is surfaced as an honest zone-in failure at the 30s deadline
/// instead. This 250ms cadence (was a hard-coded 1s) only makes the SESSION come up promptly when our
/// first SESSION_REQUEST is dropped; SESSION_REQUEST is idempotent (the server dedupes), so a faster
/// cadence is harmless at the transport layer.
///
/// This value is a LOWER BOUND on the interval actually observed on the wire, not the interval itself
/// (#603): the retry loop in `connect` only re-evaluates `session_request_due` once per iteration of
/// its `recv` call, which has a 100ms timeout — so a retry that becomes due partway through a `recv`
/// wait sits out the remainder of that tick before firing. Measured effective interval: ~250-350ms
/// (i.e. up to +100ms of quantization jitter per retry), not a clean 250ms. Documented rather than
/// "fixed" (making the wait wake exactly at the due instant instead of polling every 100ms) because
/// this is a live network path whose regression test (`connect_does_not_flood_session_request_faster_
/// than_the_cadence`) just had a real flakiness problem removed (#549) — the added complexity and
/// retest burden of an exact-wake timer isn't worth it for jitter that only ever makes the retry later,
/// never faster/floodier.
const SESSION_REQUEST_RETRY: std::time::Duration = std::time::Duration::from_millis(250);

/// Pure cadence predicate for the `connect` retry loop (#549): true once `now` is at least
/// `SESSION_REQUEST_RETRY` past `last_send`. Extracted from the loop body so the retry cadence can
/// be pinned by a fast, deterministic test that constructs synthetic `Instant`s (`Instant + Duration`
/// needs no real sleeping) instead of measuring real elapsed wall-clock time — the previous test drove
/// an actual `connect()` over a real UDP socket and counted datagrams inside a real-time window, which
/// went red under `--test-threads=8` contention purely from scheduler delay, not a transport defect.
/// `connect` calls this exact function with `Instant::now()`, so testing it *is* testing production
/// logic — there is no separate/duplicated cadence check that could drift from what ships.
fn session_request_due(last_send: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_send) >= SESSION_REQUEST_RETRY
}

/// Backoff before the Nth retransmit of the oldest outstanding reliable packet: `RESEND_BASE_MS`
/// doubled per prior attempt, clamped to `[RESEND_MIN_MS, RESEND_MAX_MS]`. `retries` is shifted-capped
/// so the `<<` can't overflow.
fn resend_delay(retries: u32) -> std::time::Duration {
    let ms = (RESEND_BASE_MS << retries.min(4)).clamp(RESEND_MIN_MS, RESEND_MAX_MS);
    std::time::Duration::from_millis(ms)
}

// ── CRC32 table ────────────────────────────────────────────────────────────

const CRC32_TABLE: [u32; 256] = [
    0x00000000, 0x77073096, 0xEE0E612C, 0x990951BA, 0x076DC419, 0x706AF48F, 0xE963A535, 0x9E6495A3,
    0x0EDB8832, 0x79DCB8A4, 0xE0D5E91E, 0x97D2D988, 0x09B64C2B, 0x7EB17CBD, 0xE7B82D07, 0x90BF1D91,
    0x1DB71064, 0x6AB020F2, 0xF3B97148, 0x84BE41DE, 0x1ADAD47D, 0x6DDDE4EB, 0xF4D4B551, 0x83D385C7,
    0x136C9856, 0x646BA8C0, 0xFD62F97A, 0x8A65C9EC, 0x14015C4F, 0x63066CD9, 0xFA0F3D63, 0x8D080DF5,
    0x3B6E20C8, 0x4C69105E, 0xD56041E4, 0xA2677172, 0x3C03E4D1, 0x4B04D447, 0xD20D85FD, 0xA50AB56B,
    0x35B5A8FA, 0x42B2986C, 0xDBBBC9D6, 0xACBCF940, 0x32D86CE3, 0x45DF5C75, 0xDCD60DCF, 0xABD13D59,
    0x26D930AC, 0x51DE003A, 0xC8D75180, 0xBFD06116, 0x21B4F4B5, 0x56B3C423, 0xCFBA9599, 0xB8BDA50F,
    0x2802B89E, 0x5F058808, 0xC60CD9B2, 0xB10BE924, 0x2F6F7C87, 0x58684C11, 0xC1611DAB, 0xB6662D3D,
    0x76DC4190, 0x01DB7106, 0x98D220BC, 0xEFD5102A, 0x71B18589, 0x06B6B51F, 0x9FBFE4A5, 0xE8B8D433,
    0x7807C9A2, 0x0F00F934, 0x9609A88E, 0xE10E9818, 0x7F6A0DBB, 0x086D3D2D, 0x91646C97, 0xE6635C01,
    0x6B6B51F4, 0x1C6C6162, 0x856530D8, 0xF262004E, 0x6C0695ED, 0x1B01A57B, 0x8208F4C1, 0xF50FC457,
    0x65B0D9C6, 0x12B7E950, 0x8BBEB8EA, 0xFCB9887C, 0x62DD1DDF, 0x15DA2D49, 0x8CD37CF3, 0xFBD44C65,
    0x4DB26158, 0x3AB551CE, 0xA3BC0074, 0xD4BB30E2, 0x4ADFA541, 0x3DD895D7, 0xA4D1C46D, 0xD3D6F4FB,
    0x4369E96A, 0x346ED9FC, 0xAD678846, 0xDA60B8D0, 0x44042D73, 0x33031DE5, 0xAA0A4C5F, 0xDD0D7CC9,
    0x5005713C, 0x270241AA, 0xBE0B1010, 0xC90C2086, 0x5768B525, 0x206F85B3, 0xB966D409, 0xCE61E49F,
    0x5EDEF90E, 0x29D9C998, 0xB0D09822, 0xC7D7A8B4, 0x59B33D17, 0x2EB40D81, 0xB7BD5C3B, 0xC0BA6CAD,
    0xEDB88320, 0x9ABFB3B6, 0x03B6E20C, 0x74B1D29A, 0xEAD54739, 0x9DD277AF, 0x04DB2615, 0x73DC1683,
    0xE3630B12, 0x94643B84, 0x0D6D6A3E, 0x7A6A5AA8, 0xE40ECF0B, 0x9309FF9D, 0x0A00AE27, 0x7D079EB1,
    0xF00F9344, 0x8708A3D2, 0x1E01F268, 0x6906C2FE, 0xF762575D, 0x806567CB, 0x196C3671, 0x6E6B06E7,
    0xFED41B76, 0x89D32BE0, 0x10DA7A5A, 0x67DD4ACC, 0xF9B9DF6F, 0x8EBEEFF9, 0x17B7BE43, 0x60B08ED5,
    0xD6D6A3E8, 0xA1D1937E, 0x38D8C2C4, 0x4FDFF252, 0xD1BB67F1, 0xA6BC5767, 0x3FB506DD, 0x48B2364B,
    0xD80D2BDA, 0xAF0A1B4C, 0x36034AF6, 0x41047A60, 0xDF60EFC3, 0xA867DF55, 0x316E8EEF, 0x4669BE79,
    0xCB61B38C, 0xBC66831A, 0x256FD2A0, 0x5268E236, 0xCC0C7795, 0xBB0B4703, 0x220216B9, 0x5505262F,
    0xC5BA3BBE, 0xB2BD0B28, 0x2BB45A92, 0x5CB36A04, 0xC2D7FFA7, 0xB5D0CF31, 0x2CD99E8B, 0x5BDEAE1D,
    0x9B64C2B0, 0xEC63F226, 0x756AA39C, 0x026D930A, 0x9C0906A9, 0xEB0E363F, 0x72076785, 0x05005713,
    0x95BF4A82, 0xE2B87A14, 0x7BB12BAE, 0x0CB61B38, 0x92D28E9B, 0xE5D5BE0D, 0x7CDCEFB7, 0x0BDBDF21,
    0x86D3D2D4, 0xF1D4E242, 0x68DDB3F8, 0x1FDA836E, 0x81BE16CD, 0xF6B9265B, 0x6FB077E1, 0x18B74777,
    0x88085AE6, 0xFF0F6A70, 0x66063BCA, 0x11010B5C, 0x8F659EFF, 0xF862AE69, 0x616BFFD3, 0x166CCF45,
    0xA00AE278, 0xD70DD2EE, 0x4E048354, 0x3903B3C2, 0xA7672661, 0xD06016F7, 0x4969474D, 0x3E6E77DB,
    0xAED16A4A, 0xD9D65ADC, 0x40DF0B66, 0x37D83BF0, 0xA9BCAE53, 0xDEBB9EC5, 0x47B2CF7F, 0x30B5FFE9,
    0xBDBDF21C, 0xCABAC28A, 0x53B39330, 0x24B4A3A6, 0xBAD03605, 0xCDD70693, 0x54DE5729, 0x23D967BF,
    0xB3667A2E, 0xC4614AB8, 0x5D681B02, 0x2A6F2B94, 0xB40BBE37, 0xC30C8EA1, 0x5A05DF1B, 0x2D02EF8D,
];

/// EQ CRC32 keyed by session encode_key — matches EQ::Crc32(data, size, key).
fn eq_crc32(data: &[u8], key: u32) -> u32 {
    let key = key & 0xFFFFFFFF;
    let mut crc: u32 = 0xFFFFFFFF;
    for i in 0..4 {
        let b = ((key >> (i * 8)) & 0xFF) as u8;
        crc = ((crc >> 8) & 0x00FFFFFF) ^ CRC32_TABLE[((crc ^ b as u32) & 0xFF) as usize];
    }
    for b in data {
        crc = ((crc >> 8) & 0x00FFFFFF) ^ CRC32_TABLE[((crc ^ *b as u32) & 0xFF) as usize];
    }
    (!crc) & 0xFFFFFFFF
}

/// XOR-encode/decode with 4-byte rolling key.
fn decode_xor(data: &[u8], key: u32) -> Vec<u8> {
    let key_bytes = key.to_be_bytes();
    data.iter()
        .enumerate()
        .map(|(i, b)| b ^ key_bytes[i % 4])
        .collect()
}

/// EQ compression: 0x5a + zlib if beneficial and data > 30 bytes, else 0xa5 + raw.
fn eq_compress(data: &[u8]) -> Vec<u8> {
    if data.len() > 30 {
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(data, 1);
        if compressed.len() < data.len() {
            let mut result = vec![0x5a];
            result.extend_from_slice(&compressed);
            return result;
        }
    }
    let mut result = vec![0xa5];
    result.extend_from_slice(data);
    result
}

/// EQ decompression: 0x5a = zlib, 0xa5 = raw, else passthrough.
fn eq_decompress(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() {
        return Some(vec![]);
    }
    match data[0] {
        0x5a => {
            miniz_oxide::inflate::decompress_to_vec_zlib(&data[1..]).ok()
        }
        0xa5 => Some(data[1..].to_vec()),
        _ => Some(data.to_vec()),
    }
}

/// Apply the two negotiated decode passes (the reverse order of `encode`). Shared by the
/// reliable body decode (`EqStream::decode`) and the raw-app-packet decode.
fn decode_passes(data: &[u8], pass1: u8, pass2: u8, key: u32) -> Option<Vec<u8>> {
    let mut result = data.to_vec();
    if pass2 == ENCODE_COMPRESSION {
        result = eq_decompress(&result)?;
    } else if pass2 == ENCODE_XOR {
        result = decode_xor(&result, key);
    }
    if pass1 == ENCODE_COMPRESSION {
        result = eq_decompress(&result)?;
    } else if pass1 == ENCODE_XOR {
        result = decode_xor(&result, key);
    }
    Some(result)
}

/// Apply the negotiated encode passes to `data` — the inverse of `decode_passes` (pass1 then
/// pass2). XOR encode is symmetric, so it reuses `decode_xor` with the same key.
fn encode_passes(data: &[u8], pass1: u8, pass2: u8, key: u32) -> Vec<u8> {
    let mut result = data.to_vec();
    if pass1 == ENCODE_COMPRESSION {
        result = eq_compress(&result);
    } else if pass1 == ENCODE_XOR {
        result = decode_xor(&result, key);
    }
    if pass2 == ENCODE_COMPRESSION {
        result = eq_compress(&result);
    } else if pass2 == ENCODE_XOR {
        result = decode_xor(&result, key);
    }
    result
}

/// Build the body (everything except the trailing outer CRC) of a RAW, unreliable application
/// datagram — the inverse of `decode_raw_app`: `[opcode_lo] ++ encode_passes([opcode_hi] ++ payload)`.
/// The `opcode_lo` lead byte is left plaintext so the wire's non-zero-lead-byte rule marks this as
/// a raw application packet rather than a protocol packet (those lead with 0x00).
fn encode_raw_app(opcode: u16, payload: &[u8], pass1: u8, pass2: u8, key: u32) -> Vec<u8> {
    let mut inner = Vec::with_capacity(1 + payload.len());
    inner.push((opcode >> 8) as u8); // opcode high byte, then payload — all encode-passed
    inner.extend_from_slice(payload);
    let encoded = encode_passes(&inner, pass1, pass2, key);
    let mut body = Vec::with_capacity(1 + encoded.len());
    body.push((opcode & 0xFF) as u8); // plaintext non-zero lead byte (opcode low)
    body.extend_from_slice(&encoded);
    body
}

/// Decode a standalone RAW (unreliable) application packet — CRC already stripped — into
/// its `(opcode, payload)`.
///
/// EQEmu sends high-frequency updates this way (`QueuePacket(.., ack_req=false)`), most
/// importantly NPC position updates (`Mob::SendPosUpdate`). Such a datagram is NOT wrapped
/// in `OP_Packet`/`OP_Fragment`; the datagram *is* the application packet. Its lead byte is
/// the app opcode's low byte (always non-zero for our opcodes), which is how the wire
/// distinguishes it from a protocol packet (those lead with `0x00`). The server leaves that
/// first byte plain and runs the encode passes from offset 1
/// (`ReliableStreamConnection::InternalSend`), so we decode `body[1..]` with the same passes
/// and prepend the plain byte to recover `[opcode_lo, opcode_hi, payload…]`.
fn decode_raw_app(body: &[u8], pass1: u8, pass2: u8, key: u32) -> Option<(u16, Vec<u8>)> {
    if body.is_empty() {
        return None;
    }
    let opcode_lo = body[0];
    let decoded = decode_passes(&body[1..], pass1, pass2, key)?;
    if decoded.is_empty() {
        return None;
    }
    let opcode = (opcode_lo as u16) | ((decoded[0] as u16) << 8);
    Some((opcode, decoded[1..].to_vec()))
}

// ── Session info ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub connect_code: u32,
    pub encode_key: u32,
    pub crc_bytes: u8,
    pub encode_pass1: u8,
    pub encode_pass2: u8,
    pub max_packet_size: u16,
    pub connected: bool,
}

impl Default for SessionInfo {
    fn default() -> Self {
        SessionInfo {
            connect_code: 0,
            encode_key: 0,
            crc_bytes: 0,
            encode_pass1: 0,
            encode_pass2: 0,
            max_packet_size: 512,
            connected: false,
        }
    }
}

// ── Fragment buffer ────────────────────────────────────────────────────────

struct FragmentBuffer {
    buf: Vec<u8>,
    total: usize,
}

impl FragmentBuffer {
    fn new() -> Self {
        FragmentBuffer { buf: Vec::new(), total: 0 }
    }

    fn in_progress(&self) -> bool {
        self.total > 0
    }

    /// Feed one fragment. Returns complete reassembled data if done.
    /// For the first fragment, `data` starts with 4-byte big-endian total_size.
    fn add(&mut self, data: &[u8], _is_first: bool) -> Option<Vec<u8>> {
        if !self.in_progress() {
            if data.len() < 4 {
                return None;
            }
            self.total = Cursor::new(&data[..4]).read_u32::<BigEndian>().unwrap() as usize;
            self.buf.extend_from_slice(&data[4..]);
        } else {
            self.buf.extend_from_slice(data);
        }
        if self.buf.len() >= self.total {
            let result = self.buf[..self.total].to_vec();
            self.buf.clear();
            self.total = 0;
            Some(result)
        } else {
            None
        }
    }
}

// ── App packet type ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AppPacket {
    pub opcode: u16,
    pub payload: Vec<u8>,
}

// ── EQ Stream ──────────────────────────────────────────────────────────────

/// How an incoming reliable sequence number relates to what we've already delivered.
#[derive(Debug, PartialEq, Eq)]
enum SeqClass {
    /// Exactly the next expected packet — deliver it (and advance).
    Deliver,
    /// Ahead of the next expected (a gap) — buffer it and send OP_OUT_OF_ORDER.
    Future,
    /// Behind the next expected — an already-delivered packet the server is retransmitting because
    /// our ACK was lost. Re-ACK it (never re-deliver) so the server's resend_timeout doesn't drop
    /// the session (#158).
    Duplicate,
}

/// Classify a reliable `seq` against `next_recv_seq` (the next sequence we expect), handling u16
/// wrap-around near the top of the sequence space. Pure so the resend/duplicate decision is
/// unit-tested independently of the socket.
fn classify_seq(seq: u16, next_recv_seq: u16) -> SeqClass {
    if seq == next_recv_seq {
        SeqClass::Deliver
    } else if seq > next_recv_seq || (seq < 0x1000 && next_recv_seq > 0xF000) {
        SeqClass::Future
    } else {
        SeqClass::Duplicate
    }
}

/// Serial-number "`a` is at or before `b`" (RFC 1982), handling u16 wrap. Used to decide which
/// buffered reliable sends a CUMULATIVE inbound `OP_ACK(acked)` acknowledges: `a` is acked when
/// `seq_leq(a, acked)`. `a == b` counts as ≤. Pure so the ack/window logic is unit-tested off-socket.
fn seq_leq(a: u16, b: u16) -> bool {
    b.wrapping_sub(a) < 0x8000
}

/// A reliable protocol datagram we've sent but the server has not yet ACKed, kept verbatim (the exact
/// on-wire bytes, post-encode + CRC) so it can be retransmitted unchanged until acknowledged. Without
/// retransmission a single lost/reordered reliable stalls the server's ordered receive window and it
/// drops us as linkdead — worst at zone handoffs (#254).
struct Sent {
    seq: u16,
    datagram: Vec<u8>,
    sent_at: Instant,
    retries: u32,
}

pub struct EqStream {
    session: SessionInfo,
    socket: UdpSocket,
    #[allow(dead_code)]
    peer: SocketAddr,
    send_seq: u16,
    next_recv_seq: u16,
    recv_buf: HashMap<u16, (Vec<u8>, bool)>, // seq → (data, is_fragment)
    /// Sent-but-unACKed reliable datagrams, in send order, retransmitted on OP_OutOfOrder / timeout
    /// until the server ACKs their sequence (#254). Cumulative OP_ACK pops from the front.
    sent: VecDeque<Sent>,
    frags: FragmentBuffer,
    app_tx: mpsc::UnboundedSender<AppPacket>,
    /// Wall-clock time of the last inbound **datagram** of any kind — session-layer ACKs and
    /// keepalive replies included, not just decoded application packets. This is the only sound
    /// "is the link alive?" signal: a genuinely idle EQ session can go tens of seconds without a
    /// single APP packet (nothing is happening in the world) while the session layer keeps
    /// ACKing — so app-packet silence must never be mistaken for a dead connection (#343).
    ///
    /// The stream stamps this **itself**, in `poll_recv`, rather than expecting each of the four
    /// loops that own an `EqStream` (login, gameplay, zone-entry handshake, world reconnect) to
    /// remember to mirror it. That discipline is exactly what failed in review of #343: two of the
    /// four loops didn't mirror, so a >15s world reconnect reported `connected: false` on a healthy
    /// link. Whoever receives the datagram owns the clock — a future loop gets this for free.
    net_health: eqoxide_ipc::NetHealthShared,
}

impl EqStream {
    pub async fn connect(
        host: &str,
        port: u16,
        app_tx: mpsc::UnboundedSender<AppPacket>,
        net_health: eqoxide_ipc::NetHealthShared,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let peer: SocketAddr = format!("{}:{}", host, port).parse()?;
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect(peer).await?;

        let mut stream = EqStream {
            session: SessionInfo::default(),
            socket,
            peer,
            send_seq: 0,
            next_recv_seq: 0,
            recv_buf: HashMap::new(),
            sent: VecDeque::new(),
            frags: FragmentBuffer::new(),
            net_health,
            app_tx,
        };

        // The very first send on a freshly connect()ed UDP socket reliably returns WouldBlock and is
        // silently dropped by `send_raw`'s `let _ = try_send(..)` (verified empirically for #603: 50/50
        // trials WouldBlocked, 0/50 succeeded). Cause: tokio only learns a socket is writable from an
        // edge-triggered epoll event, and nothing between `bind`/`connect` above (neither does real I/O)
        // ever gives its reactor a chance to observe one before we'd otherwise call `try_send` here. Before
        // this fix that made the loop below do 100% of the work of getting a SESSION_REQUEST onto the
        // wire, roughly one `SESSION_REQUEST_RETRY` interval after connect — this call looked like the
        // primary attempt but never fired. Awaiting `writable()` once (measured: <30µs over 50 trials — a
        // UDP send buffer is practically always free) primes that readiness so this send genuinely
        // transmits, shaving up to one retry interval off connect time. The retry loop's own sends don't
        // need this: once readiness has been observed, tokio keeps it marked ready.
        let _ = stream.socket.writable().await;
        stream.send_session_request();

        // Wait for SESSION_RESPONSE, re-sending SESSION_REQUEST on the `session_request_due` cadence
        // below in case of UDP loss. NOTE (#603): that cadence is only re-checked once per iteration of
        // this loop's `recv` (100ms timeout, below), so a retry that becomes due mid-recv waits out the
        // rest of that tick before firing — see `SESSION_REQUEST_RETRY`'s doc comment for the measured
        // effective interval this produces.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut last_send = std::time::Instant::now();
        let mut recv_buf = vec![0u8; 4096];
        while !stream.session.connected {
            if std::time::Instant::now() > deadline {
                return Err("Session handshake timeout: no SESSION_RESPONSE from server".into());
            }
            if session_request_due(last_send, std::time::Instant::now()) {
                stream.send_session_request();
                last_send = std::time::Instant::now();
                // #477: keep the LIVENESS clock fresh across the handshake. This loop runs on the
                // gameplay net thread during a zone handoff (the bare `connect()` calls in
                // gameplay.rs), and on an on-demand server a cold zone can take several seconds to
                // answer the first SESSION_REQUEST. Only `publish_snapshot` bumps `last_tick`, which
                // never runs here — so without this bump a healthy, actively-handshaking session would
                // cross `SESSION_STALE_TICK_MS` and get WRITE commands falsely rejected as "the net
                // thread has not ticked (it has exited or wedged)". The thread IS alive and
                // progressing through the handshake; stamping `last_tick` on each retry (and on each
                // recv below) makes the clock reflect that, even when the cold zone is still silent.
                stream.net_health.lock().unwrap().last_tick = std::time::Instant::now();
            }
            match tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                stream.socket.recv(&mut recv_buf),
            ).await {
                Ok(Ok(n)) => {
                    // The other socket read. Stamped for the same reason `poll_recv` is, and stamped
                    // here rather than left as a benign exception: the invariant "whoever receives
                    // the datagram owns the clock" is only load-bearing if it has NO exceptions —
                    // an asterisk on it is how it rots back into #343 (review). `last_tick` is bumped
                    // alongside it (#477): a datagram arriving mid-handshake is proof the net thread
                    // is alive and progressing, so the liveness clock must not be allowed to go stale.
                    let mut h = stream.net_health.lock().unwrap();
                    let now = std::time::Instant::now();
                    h.last_datagram = now;
                    h.last_tick = now;
                    drop(h);
                    stream.on_raw_recv(&recv_buf[..n]);
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {} // recv timeout, keep waiting
            }
        }

        Ok(stream)
    }

    /// Send a session request (must be called after connect, before any other packets).
    fn send_session_request(&mut self) {
        let connect_code: u32 = rand::random::<u32>() & 0x7FFFFFFF;
        self.session.connect_code = connect_code;
        let mut payload = Vec::new();
        payload.write_u32::<BigEndian>(2).unwrap(); // protocol version
        payload.write_u32::<BigEndian>(connect_code).unwrap();
        payload.write_u32::<BigEndian>(self.session.max_packet_size as u32).unwrap();
        self.send_raw(OP_SESSION_REQUEST, &payload);
    }

    /// Send an application-level EQ packet (2-byte LE opcode + payload).
    pub fn send_app_packet(&mut self, opcode: u16, payload: &[u8]) {
        // Telemetry boundary (#525): record BEFORE send_reliable assigns the sequence, so `send_seq`
        // is the sequence this packet's first datagram will carry. Zero cost when disabled.
        super::packet_telemetry::capture(
            super::packet_telemetry::Dir::Out, opcode, payload, true, Some(self.send_seq),
        );
        let mut app_data = Vec::with_capacity(2 + payload.len());
        app_data.write_u16::<byteorder::LittleEndian>(opcode).unwrap();
        app_data.extend_from_slice(payload);
        self.send_reliable(&app_data);
    }

    /// Send an application packet UNRELIABLY: a raw datagram with no sequence number and no
    /// retransmit tracking — the same `ack_req=false` path the server uses for its own
    /// high-frequency position broadcasts (`Mob::SendPosUpdate`). Use this for the streamed
    /// `OP_ClientUpdate` position firehose. A dropped update is harmless (the next one supersedes
    /// it), whereas sending position reliably makes every lost datagram an unfillable sequence gap;
    /// since we never retransmit (`OP_ACK`/`OP_OUT_OF_ORDER` are ignored), the server's ordered
    /// stream stalls and eventually drops the session as linkdead on long continuous runs — the
    /// more a client moves, the more reliable position packets it sends and the sooner one is lost
    /// (eqoxide#127). Only valid for opcodes whose low byte is non-zero (the raw-app-packet marker).
    pub fn send_app_packet_unreliable(&mut self, opcode: u16, payload: &[u8]) {
        // Telemetry boundary (#525): unreliable — no sequence number. Zero cost when disabled.
        super::packet_telemetry::capture(
            super::packet_telemetry::Dir::Out, opcode, payload, false, None,
        );
        debug_assert!(opcode & 0xFF != 0, "raw app packets need a non-zero low opcode byte");
        let body = encode_raw_app(
            opcode, payload,
            self.session.encode_pass1, self.session.encode_pass2, self.session.encode_key,
        );
        let datagram = self.append_crc(body);
        let _ = self.socket.try_send(&datagram);
    }

    /// Poll for incoming data. Non-blocking. Returns false if the socket is closed.
    /// Receive and dispatch every datagram currently queued on the socket — sending an ACK for
    /// each reliable packet as it's processed — not just one per call.
    ///
    /// Reading a single datagram per call (with the ~10 ms gameplay loop) capped intake at ~100
    /// datagrams/sec. A busy zone's inbound reliable burst (e.g. Nektulos's ~700-spawn zone-in)
    /// then backed up in the OS receive buffer while our ACKs lagged, until the server's resend
    /// queue overflowed and dropped us as linkdead ("Stopping resend because we hit thresholds").
    /// Draining fully each wake keeps ACKs current even if the loop is delayed under CPU
    /// contention. Bounded by `MAX_DATAGRAMS_PER_POLL` so a sustained flood can't starve the rest
    /// of the loop in a single wake (the remainder drains on the next wake). (#153)
    pub fn poll_recv(&mut self) -> bool {
        let mut buf = vec![0u8; 4096];
        for _ in 0..MAX_DATAGRAMS_PER_POLL {
            match self.socket.try_recv(&mut buf) {
                Ok(n) => {
                    // Link liveness (#343): ANY datagram, stamped BEFORE decode so session-layer
                    // ACKs/keepalives — which never become application packets — still count.
                    self.net_health.lock().unwrap().last_datagram = std::time::Instant::now();
                    self.on_raw_recv(&buf[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(_) => return false,
            }
        }
        true
    }

    /// Retransmit un-ACKed reliable datagrams (#254). Timer-driven **go-back-N**, mirroring EQEmu's
    /// `ReliableStreamConnection::ProcessResend`: once the OLDEST outstanding packet's backoff has
    /// elapsed, resend the WHOLE outstanding window (verbatim, same sequences) and double each entry's
    /// backoff (clamped). Nothing here fires until a packet has actually gone un-ACKed past its delay,
    /// so calling it every ~10 ms loop tick is cheap. This is what stops a single lost/reordered
    /// reliable from stalling the server's ordered receive window and getting us dropped as linkdead —
    /// most exposed at zone handoffs, where a fresh session must land OP_ZoneEntry/ReqClientSpawn.
    /// Backoff is capped so a truly dead link just re-sends every RESEND_MAX_MS until the server's 30 s
    /// resend_timeout drops the session and the reconnect path takes over.
    pub fn poll_resend(&mut self) {
        let now = Instant::now();
        let due = match self.sent.front() {
            Some(front) => now.saturating_duration_since(front.sent_at) >= resend_delay(front.retries),
            None => return,
        };
        if !due {
            return;
        }
        // Go-back-N: resend the whole outstanding window (bounded per pass like the server's
        // MAX_CLIENT_RECV_PACKETS_PER_WINDOW). Indexed so socket + buffer borrows stay disjoint.
        let n = self.sent.len().min(MAX_RESEND_PER_PASS);
        // Steady-state this should be silent: healthy ACKs keep the window empty. A sustained stream of
        // these means ACKs aren't clearing the window (loss, or an ack-decode mismatch) — useful signal.
        tracing::debug!(
            "NET: retransmitting {} un-ACKed reliable(s), oldest seq={} retries={} (#254)",
            n, self.sent[0].seq, self.sent[0].retries,
        );
        for i in 0..n {
            let _ = self.socket.try_send(&self.sent[i].datagram);
            self.sent[i].sent_at = now;
            self.sent[i].retries = self.sent[i].retries.saturating_add(1);
        }
    }

    /// Send a keepalive response.
    pub fn send_keepalive(&mut self) {
        self.send_raw(OP_KEEPALIVE, &[]);
    }

    /// Send a session-layer disconnect (`OP_SessionDisconnect`, 0x05). Tells the EQStream peer
    /// we are closing this session. Payload is the negotiated `connect_code` as a big-endian u32;
    /// `append_crc` (called inside `send_raw`) appends the CRC. Sent as part of clean shutdown.
    pub fn send_session_disconnect(&mut self) {
        let mut payload = Vec::with_capacity(4);
        payload.write_u32::<BigEndian>(self.session.connect_code).unwrap();
        self.send_raw(OP_SESSION_DISC, &payload);
    }

    // ── Internal send helpers ─────────────────────────────────────────────────

    fn send_raw(&mut self, opcode: u8, payload: &[u8]) {
        let mut raw = vec![0x00, opcode];
        raw.extend_from_slice(payload);
        raw = self.append_crc(raw);
        let _ = self.socket.try_send(&raw);
    }

    fn append_crc(&self, data: Vec<u8>) -> Vec<u8> {
        match self.session.crc_bytes {
            4 => {
                let crc = eq_crc32(&data, self.session.encode_key);
                let mut data = data;
                data.write_u32::<BigEndian>(crc).unwrap();
                data
            }
            2 => {
                let crc = eq_crc32(&data, self.session.encode_key) & 0xFFFF;
                let mut data = data;
                data.write_u16::<BigEndian>(crc as u16).unwrap();
                data
            }
            1 => {
                let crc = eq_crc32(&data, self.session.encode_key) & 0xFF;
                let mut data = data;
                data.push(crc as u8);
                data
            }
            _ => data,
        }
    }

    fn send_ack(&mut self, seq: u16) {
        let seq_bytes = seq.to_be_bytes();
        self.send_raw(OP_ACK, &self.encode(&seq_bytes.to_vec()));
    }

    /// Send an OP_OutOfOrderAck for a buffered out-of-order sequence — the client-side mirror of the
    /// server's `SendOutOfOrderAck` (EQEmu `reliable_stream_connection.cpp:1320`). Framed IDENTICALLY
    /// to `send_ack`: the seq body is ENCODED with the negotiated passes, because the server decodes
    /// the bytes after the `[0x00, opcode]` header before reading the sequence (`:476-493`). Tells the
    /// server we received this future seq (it drops it from its resend window) and, via the server's
    /// `m_acked_since_last_resend` flag, triggers an immediate go-back-N resend of the still-missing
    /// gap on its next tic. (#463)
    fn send_out_of_order(&mut self, seq: u16) {
        let seq_bytes = seq.to_be_bytes();
        self.send_raw(OP_OUT_OF_ORDER, &self.encode(&seq_bytes.to_vec()));
    }

    fn send_reliable(&mut self, app_data: &[u8]) {
        let max_inner = (self.session.max_packet_size as usize) - 5; // 2 proto + 1 compress + 2 crc
        if app_data.len() + 2 <= max_inner {
            let seq = self.next_send_seq();
            let mut inner = seq.to_be_bytes().to_vec();
            inner.extend_from_slice(app_data);
            self.send_tracked(seq, OP_PACKET, &self.encode(&inner));
        } else {
            // Fragment
            let seq = self.next_send_seq();
            let total_size = app_data.len() as u32;
            let first_max = max_inner - 2 - 4; // seq + total_size overhead
            let mut inner = seq.to_be_bytes().to_vec();
            inner.extend_from_slice(&total_size.to_be_bytes());
            inner.extend_from_slice(&app_data[..first_max]);
            self.send_tracked(seq, OP_FRAGMENT, &self.encode(&inner));

            let mut offset = first_max;
            while offset < app_data.len() {
                let seq = self.next_send_seq();
                let end = (offset + max_inner - 2).min(app_data.len());
                let mut inner = seq.to_be_bytes().to_vec();
                inner.extend_from_slice(&app_data[offset..end]);
                self.send_tracked(seq, OP_FRAGMENT, &self.encode(&inner));
                offset = end;
            }
        }
    }

    /// Build, RECORD, and send a reliable protocol datagram (OP_Packet / OP_Fragment). Frames exactly
    /// like `send_raw` but retains the final wire bytes in the resend window so the datagram can be
    /// retransmitted VERBATIM (same `seq`) on OP_OutOfOrder / timeout until the server ACKs it (#254).
    fn send_tracked(&mut self, seq: u16, opcode: u8, encoded_inner: &[u8]) {
        let mut raw = vec![0x00, opcode];
        raw.extend_from_slice(encoded_inner);
        let datagram = self.append_crc(raw);
        let _ = self.socket.try_send(&datagram);
        self.sent.push_back(Sent { seq, datagram, sent_at: Instant::now(), retries: 0 });
    }

    fn next_send_seq(&mut self) -> u16 {
        let seq = self.send_seq;
        self.send_seq = self.send_seq.wrapping_add(1);
        seq
    }

    /// TEST ONLY: decode every reliable (`OP_PACKET`) app packet still tracked in the resend window
    /// into `(app_opcode, payload)` pairs, in send order. Assumes the identity encode used by
    /// `test_stream` (pass1 = pass2 = 0, key = 0) so the tracked datagram is the plain wire framing
    /// `[0x00, OP_PACKET, seq(2 BE), opcode(2 LE), payload.., crc(crc_bytes)]`. The trailing CRC width
    /// is `session.crc_bytes` (0 in `test_stream`). Used to assert that a state machine actually put a
    /// given opcode on the wire (e.g. the #480 phase-2 OP_CancelTrade). Skips fragmented sends (our
    /// small control packets never fragment).
    #[cfg(test)]
    pub(crate) fn sent_app_packets(&self) -> Vec<(u16, Vec<u8>)> {
        let crc = self.session.crc_bytes as usize;
        self.sent.iter().filter_map(|s| {
            let d = &s.datagram;
            // [0x00, OP_PACKET, seq_hi, seq_lo, op_lo, op_hi, payload.., crc(crc_bytes)]
            if d.len() < 6 + crc || d[1] != OP_PACKET { return None; }
            let opcode = u16::from_le_bytes([d[4], d[5]]);
            let payload = d[6..d.len() - crc].to_vec();
            Some((opcode, payload))
        }).collect()
    }

    /// Process an inbound OP_ACK. EQStream ACKs are CUMULATIVE — `acked` acknowledges every reliable
    /// sequence up to and including it — so drop every buffered send at or before it from the front of
    /// the resend window. A stale/duplicate ACK (behind the window base) pops nothing. (#254)
    fn ack_up_to(&mut self, acked: u16) {
        while let Some(front) = self.sent.front() {
            if seq_leq(front.seq, acked) {
                self.sent.pop_front();
            } else {
                break;
            }
        }
    }

    /// Process an inbound OP_OutOfOrder(seq): the server RECEIVED our reliable `seq` but ahead of a gap
    /// (it buffered it), so stop retransmitting THAT packet — it arrived. SELECTIVE: removes only the
    /// exact `seq`, is not cumulative, and does NOT trigger a resend (the still-missing earlier
    /// packet(s) are retransmitted by the resend timer). Mirrors EQEmu `OutOfOrderAck`. (#254)
    fn on_out_of_order(&mut self, seq: u16) {
        if let Some(pos) = self.sent.iter().position(|s| s.seq == seq) {
            self.sent.remove(pos);
        }
    }

    // ── Encoding/decoding ─────────────────────────────────────────────────────

    fn encode(&self, data: &[u8]) -> Vec<u8> {
        encode_passes(data, self.session.encode_pass1, self.session.encode_pass2, self.session.encode_key)
    }

    fn decode(&self, data: &[u8]) -> Option<Vec<u8>> {
        decode_passes(
            data,
            self.session.encode_pass1,
            self.session.encode_pass2,
            self.session.encode_key,
        )
    }

    // ── Receive dispatch ──────────────────────────────────────────────────────

    fn on_raw_recv(&mut self, data: &[u8]) {
        // Strip the outer CRC that trails every datagram.
        let body_end = data.len().saturating_sub(self.session.crc_bytes as usize);
        let body = &data[..body_end];
        if body.len() < 2 {
            return;
        }

        // Two datagram kinds share the wire (see ReliableStreamConnection::ProcessDecodedPacket):
        //   lead byte 0x00 → a PROTOCOL packet; byte[1] is the protocol opcode
        //                    (OP_Packet / OP_Fragment / OP_Combined / …).
        //   lead byte != 0 → a RAW APPLICATION packet sent unreliably (ack_req=false). The
        //                    bytes ARE the app packet (opcode = first 2 bytes LE). EQEmu sends
        //                    NPC position updates this way; we used to read byte[1] as a
        //                    protocol opcode, match nothing, and drop them — so NPCs never moved.
        if body[0] != 0x00 {
            if let Some((opcode, payload)) = decode_raw_app(
                body,
                self.session.encode_pass1,
                self.session.encode_pass2,
                self.session.encode_key,
            ) {
                // Telemetry boundary (#525): raw unreliable app packet (e.g. NPC position
                // broadcasts) — no reliable sequence. Zero cost when disabled.
                super::packet_telemetry::capture(
                    super::packet_telemetry::Dir::In, opcode, &payload, false, None,
                );
                let _ = self.app_tx.send(AppPacket { opcode, payload });
            }
            return;
        }

        self.dispatch_transport(body[1], &body[2..]);
    }

    fn dispatch_transport(&mut self, opcode: u8, payload: &[u8]) {
        match opcode {
            OP_SESSION_RESPONSE => self.handle_session_response(payload),
            OP_KEEPALIVE => { self.send_raw(OP_KEEPALIVE, &[]); }
            OP_STAT_REQUEST => { self.send_raw(OP_STAT_RESPONSE, payload); }
            OP_COMBINED => self.handle_transport_combined(payload),
            OP_PACKET => self.handle_packet(payload),
            OP_FRAGMENT | OP_FRAGMENT_CONT | OP_FRAGMENT_CONT2 | OP_FRAGMENT_CONT3 => {
                self.handle_fragment(payload);
            }
            OP_APP_COMBINED => self.handle_combined(payload),
            // Inbound reliability control (#254). Both carry an encoded BE u16 sequence, framed like a
            // reliable packet's seq — decode the same way ordered packets are decoded.
            OP_ACK => {
                if let Some(dec) = self.decode(payload) {
                    if dec.len() >= 2 { self.ack_up_to(u16::from_be_bytes([dec[0], dec[1]])); }
                }
            }
            OP_OUT_OF_ORDER => {
                if let Some(dec) = self.decode(payload) {
                    if dec.len() >= 2 { self.on_out_of_order(u16::from_be_bytes([dec[0], dec[1]])); }
                }
            }
            _ => {}
        }
    }

    /// Dispatch a protocol sub-packet pulled from an OP_Combined, whose `payload` is ALREADY
    /// plaintext. The reliability opcodes (Packet/Fragment/Ack/OutOfOrder) whose standalone handlers
    /// decode their body must be handled here WITHOUT decoding, since the combined body was decoded
    /// once already. Everything else (session-response/keepalive/stat/nested-combined) does not decode
    /// its body, so it can share `dispatch_transport` unchanged. (#302)
    fn handle_predecoded_transport(&mut self, opcode: u8, payload: &[u8]) {
        match opcode {
            OP_PACKET => self.handle_ordered_decoded(payload, false),
            OP_FRAGMENT | OP_FRAGMENT_CONT | OP_FRAGMENT_CONT2 | OP_FRAGMENT_CONT3 => {
                self.handle_ordered_decoded(payload, true);
            }
            OP_ACK => {
                if payload.len() >= 2 { self.ack_up_to(u16::from_be_bytes([payload[0], payload[1]])); }
            }
            OP_OUT_OF_ORDER => {
                if payload.len() >= 2 { self.on_out_of_order(u16::from_be_bytes([payload[0], payload[1]])); }
            }
            // Non-decoding transport opcodes are safe to route through the standard dispatch.
            _ => self.dispatch_transport(opcode, payload),
        }
    }

    fn handle_session_response(&mut self, payload: &[u8]) {
        if payload.len() < 15 {
            return;
        }
        // ReliableStreamConnectReply layout (all BE):
        //   connect_code(4) encode_key(4) crc_bytes(1) encode_pass1(1) encode_pass2(1) max_size(4)
        self.session.connect_code = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        self.session.encode_key   = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
        self.session.crc_bytes    = payload[8];
        self.session.encode_pass1 = payload[9];
        self.session.encode_pass2 = payload[10];
        if payload.len() >= 15 {
            let max = u32::from_be_bytes([payload[11], payload[12], payload[13], payload[14]]);
            if max > 0 {
                self.session.max_packet_size = max.min(0xFFFF) as u16;
            }
        }
        self.session.connected = true;
    }

    /// A standalone reliable datagram (OP_Packet / OP_Fragment) straight off the wire: its body is
    /// still encoded, so decode once then process. (A reliable arriving as an OP_Combined SUB is
    /// already plaintext — see `handle_ordered_decoded`, called directly from the combined handler,
    /// to avoid decoding it a second time. #302.)
    fn handle_ordered(&mut self, payload: &[u8], is_fragment: bool) {
        if payload.len() < 2 {
            return;
        }
        let decoded = match self.decode(payload) {
            Some(d) => d,
            None => return,
        };
        self.handle_ordered_decoded(&decoded, is_fragment);
    }

    /// Process an ALREADY-DECODED reliable inner (`seq(BE u16) + data`): classify by sequence, ACK,
    /// and deliver in order. Shared by the standalone path (after one decode) and the OP_Combined
    /// sub-packet path (subs are already plaintext). (#302)
    fn handle_ordered_decoded(&mut self, decoded: &[u8], is_fragment: bool) {
        if decoded.len() < 2 {
            return;
        }
        let seq = Cursor::new(&decoded[..2]).read_u16::<BigEndian>().unwrap();
        let data = decoded[2..].to_vec();

        match classify_seq(seq, self.next_recv_seq) {
            SeqClass::Deliver => {
                self.next_recv_seq = self.next_recv_seq.wrapping_add(1);
                self.deliver_seq(seq, data, is_fragment);
                // Drain buffered continuations
                while let Some((ndata, nfrag)) = self.recv_buf.remove(&self.next_recv_seq) {
                    let nseq = self.next_recv_seq;
                    self.next_recv_seq = self.next_recv_seq.wrapping_add(1);
                    self.deliver_seq(nseq, ndata, nfrag);
                }
            }
            SeqClass::Future => {
                self.recv_buf.insert(seq, (data, is_fragment));
                // Mirror the server's own receive path (EQEmu `reliable_stream_connection.cpp:715`,
                // `SendOutOfOrderAck(stream_id, sequence)`): on a gap, immediately OutOfOrderAck the
                // out-of-order packet we just BUFFERED. The server's `OutOfOrderAck` handler drops that
                // exact seq from its resend window AND sets `m_acked_since_last_resend`, which makes its
                // next 60 Hz tic bypass each packet's `resend_delay` and go-back-N resend the whole
                // outstanding window — i.e. FAST-retransmit the still-missing gap packet, rather than
                // waiting out its ~850 ms+ per-packet timer. The seq body must be ENCODED exactly like
                // OP_ACK (`send_ack`): the server decodes the bytes after the `[0x00, opcode]` header
                // with the negotiated passes before reading the sequence
                // (`reliable_stream_connection.cpp:476-493`). Sending it RAW only survived because the
                // live zone stream is compression-encoded and the server's `Decompress` passes a
                // non-flag lead byte through unchanged (`:1089`) — true for low zone-in seqs (hi byte
                // 0x00) but WRONG under an XOR stream (login/world) or when the seq high byte is
                // 0x5a/0xa5. Encode it so the ACK and OutOfOrderAck framings are identical. (#463)
                self.send_out_of_order(seq);
            }
            SeqClass::Duplicate => {
                // A retransmit of an ALREADY-DELIVERED reliable packet: our original OP_ACK for it
                // was lost, so the server is resending it and waiting for the ACK. RE-ACK it (never
                // re-deliver — we already dispatched it). Without this the packet stays "un-ACKed"
                // on the server and its `resend_timeout` (30s) closes the whole session, a spurious
                // linkdead on an otherwise idle client (#158).
                //
                // Re-ACK the CUMULATIVE high-water (`next_recv_seq - 1`, the last in-order seq), NOT
                // this duplicate's own (lower) seq — matching the server's own duplicate path,
                // `SendAck(stream_id, stream->sequence_in - 1)` (`reliable_stream_connection.cpp:719`).
                // A cumulative ACK of the high-water acknowledges this duplicate (it is ≤ high-water)
                // and re-advances the server's ack pointer as far as we actually have, where an ACK of
                // the lower duplicate seq would under-acknowledge. (`Duplicate` only arises after at
                // least one in-order delivery, so `next_recv_seq >= 1` and the `wrapping_sub(1)` is a
                // real prior seq, never a bogus 0xFFFF.) (#463)
                self.send_ack(self.next_recv_seq.wrapping_sub(1));
            }
        }
    }

    fn deliver_seq(&mut self, seq: u16, data: Vec<u8>, is_fragment: bool) {
        self.send_ack(seq);
        if is_fragment {
            // A fragment group spans several reliable seqs; the completing seq is recorded (#525).
            if let Some(complete) = self.frags.add(&data, !self.frags.in_progress()) {
                self.dispatch_app(&complete, Some(seq));
            }
        } else {
            self.dispatch_app(&data, Some(seq));
        }
    }

    fn handle_packet(&mut self, payload: &[u8]) {
        self.handle_ordered(payload, false);
    }

    fn handle_fragment(&mut self, payload: &[u8]) {
        self.handle_ordered(payload, true);
    }

    fn handle_transport_combined(&mut self, payload: &[u8]) {
        let payload = match self.decode(payload) {
            Some(d) => d,
            None => return,
        };
        let mut offset = 0;
        while offset < payload.len() {
            let sub_len = payload[offset] as usize;
            offset += 1;
            if offset + sub_len > payload.len() {
                break;
            }
            let sub = &payload[offset..offset + sub_len];
            if sub.len() >= 2 {
                // OP_Combined mixes protocol and raw application sub-packets, distinguished by
                // the same lead-byte rule. Sub-packets are already plaintext (the encode passes
                // ran over the whole combined body, which `decode` above undid), so a raw app
                // sub goes straight to the app layer — no per-sub decode.
                if sub[0] == 0x00 {
                    // A protocol sub-packet. Its body is ALREADY plaintext (the combined body was
                    // decoded once, above) — exactly like the raw-app subs below, which go straight to
                    // the app layer with no per-sub decode. So the reliability opcodes must be handled
                    // WITHOUT re-decoding; routing them through `dispatch_transport` (which decodes
                    // again) double-decodes the sub → a corrupt seq → the reliable is never ACKed →
                    // the server's resend_timeout (30s) closes the session (idle linkdead, #302).
                    self.handle_predecoded_transport(sub[1], &sub[2..]);
                } else {
                    self.dispatch_app(sub, None);
                }
            }
            offset += sub_len;
        }
    }

    fn handle_combined(&mut self, payload: &[u8]) {
        let mut offset = 0;
        while offset < payload.len() {
            let mut sub_len = payload[offset] as usize;
            offset += 1;
            if sub_len == 0xFF && offset + 2 <= payload.len() {
                sub_len = Cursor::new(&payload[offset..offset + 2]).read_u16::<BigEndian>().unwrap() as usize;
                offset += 2;
            }
            if offset + sub_len > payload.len() {
                break;
            }
            self.dispatch_app(&payload[offset..offset + sub_len], None);
            offset += sub_len;
        }
    }

    /// Emit one decoded app packet to the app layer. `rel_seq` is the reliable transport sequence
    /// this packet was delivered under (`Some` on the ordered reliable path via `deliver_seq`,
    /// `None` for sub-packets pulled from an OP_Combined bundle — those are not individually
    /// sequenced). Used only for telemetry (#525); the app dispatch is unaffected.
    fn dispatch_app(&mut self, data: &[u8], rel_seq: Option<u16>) {
        if data.len() < 2 {
            return;
        }
        let opcode = Cursor::new(&data[..2]).read_u16::<byteorder::LittleEndian>().unwrap();
        // Telemetry boundary (#525): reliable iff we know a sequence for it. Zero cost when disabled.
        super::packet_telemetry::capture(
            super::packet_telemetry::Dir::In, opcode, &data[2..], rel_seq.is_some(), rel_seq,
        );
        let payload = data[2..].to_vec();
        let _ = self.app_tx.send(AppPacket { opcode, payload });
    }
}

/// Build an EqStream wired to a dummy UDP peer for driving the receive path in a test. Its outbound
/// ACKs go nowhere (try_send to a closed local port is a harmless no-op). `pub(crate)` (rather than
/// nested in `mod tests`) so other modules' tests — e.g. `eq_net::gameplay`'s zone-handshake
/// publish-cadence test (#324) — can drive a real `EqStream` without a live UDP session handshake.
#[cfg(test)]
pub(crate) async fn test_stream(pass1: u8, key: u32) -> (EqStream, mpsc::UnboundedReceiver<AppPacket>) {
    test_stream_with_health(pass1, key, Default::default()).await
}

/// As `test_stream`, but with a caller-owned `NetHealthShared` so a test can assert the link clock
/// is stamped on inbound datagrams (#343).
#[cfg(test)]
pub(crate) async fn test_stream_with_health(
    pass1: u8,
    key: u32,
    net_health: eqoxide_ipc::NetHealthShared,
) -> (EqStream, mpsc::UnboundedReceiver<AppPacket>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let _ = socket.connect("127.0.0.1:1").await;
    let stream = EqStream {
        session: SessionInfo { encode_pass1: pass1, encode_key: key, ..SessionInfo::default() },
        socket,
        peer: "127.0.0.1:1".parse().unwrap(),
        send_seq: 0,
        next_recv_seq: 0,
        recv_buf: HashMap::new(),
        sent: VecDeque::new(),
        frags: FragmentBuffer::new(),
        net_health,
        app_tx: tx,
    };
    (stream, rx)
}

/// An `EqStream` wired to a REAL local peer socket, so a test can actually deliver a datagram to it
/// and drive `poll_recv`'s receive path end-to-end. Returns the stream, its app-packet receiver, the
/// peer socket, and the address to send to. (#343)
#[cfg(test)]
pub(crate) async fn test_stream_with_peer(
    net_health: eqoxide_ipc::NetHealthShared,
) -> (EqStream, mpsc::UnboundedReceiver<AppPacket>, UdpSocket, SocketAddr) {
    let (tx, rx) = mpsc::unbounded_channel();
    let peer_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer_sock.local_addr().unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let stream_addr = socket.local_addr().unwrap();
    socket.connect(peer_addr).await.unwrap();
    let stream = EqStream {
        session: SessionInfo::default(),
        socket,
        peer: peer_addr,
        send_seq: 0,
        next_recv_seq: 0,
        recv_buf: HashMap::new(),
        sent: VecDeque::new(),
        frags: FragmentBuffer::new(),
        net_health,
        app_tx: tx,
    };
    (stream, rx, peer_sock, stream_addr)
}

/// Like `test_stream_with_peer` but with a negotiated encode pass (e.g. `ENCODE_XOR`) and key, so a
/// test can assert that outbound reliability control packets (OP_ACK / OP_OutOfOrderAck) put an
/// ENCODED sequence on the wire — not a raw one. (#463)
#[cfg(test)]
pub(crate) async fn test_stream_with_peer_encoded(
    pass1: u8,
    key: u32,
    net_health: eqoxide_ipc::NetHealthShared,
) -> (EqStream, mpsc::UnboundedReceiver<AppPacket>, UdpSocket, SocketAddr) {
    let (tx, rx) = mpsc::unbounded_channel();
    let peer_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer_sock.local_addr().unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let stream_addr = socket.local_addr().unwrap();
    socket.connect(peer_addr).await.unwrap();
    let stream = EqStream {
        session: SessionInfo { encode_pass1: pass1, encode_key: key, ..SessionInfo::default() },
        socket,
        peer: peer_addr,
        send_seq: 0,
        next_recv_seq: 0,
        recv_buf: HashMap::new(),
        sent: VecDeque::new(),
        frags: FragmentBuffer::new(),
        net_health,
        app_tx: tx,
    };
    (stream, rx, peer_sock, stream_addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #343 (review): `connected` is derived from `net_health.last_datagram`, and the ONLY thing
    /// that stamps it is `poll_recv`. This is what makes the clock correct in all four loops that
    /// own an `EqStream` — including `reconnect_via_world`, whose 90-SECOND deadline would otherwise
    /// sail past `CONN_STALE_SECS` and report `connected: false` on a perfectly healthy link
    /// mid-zone-handoff. The stamp must land for ANY inbound datagram, and must happen BEFORE decode
    /// so that a session-layer ACK — which never becomes an application packet — still counts.
    #[tokio::test]
    async fn poll_recv_stamps_link_liveness_for_any_datagram_even_undecodable_ones() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        // Pretend the link has been quiet for a minute — well past CONN_STALE_SECS.
        net_health.lock().unwrap().last_datagram =
            std::time::Instant::now() - std::time::Duration::from_secs(60);

        let (mut stream, _rx, peer, stream_addr) = test_stream_with_peer(net_health.clone()).await;

        // A datagram the app layer will make nothing of — exactly like a session ACK/keepalive.
        peer.send_to(&[0x00, 0x15, 0xde, 0xad, 0xbe, 0xef], stream_addr).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        stream.poll_recv();

        let age = net_health.lock().unwrap().last_datagram.elapsed();
        assert!(age < std::time::Duration::from_secs(1),
            "an inbound datagram must refresh the link clock (age was {age:?}) — otherwise a long \
             world reconnect reports connected:false on a healthy link (#343 review)");
    }

    /// #477 regression: `EqStream::connect`'s SESSION_REQUEST retry loop must keep `last_tick` fresh
    /// while it handshakes, even when the peer is SILENT (a cold on-demand zone that hasn't answered
    /// yet). Only `publish_snapshot` bumps `last_tick`, and it does not run during the bare `connect()`
    /// calls of a zone handoff — so without the fix, `last_tick` freezes for the whole (up-to-20s)
    /// handshake while `last_datagram` stays fresh from the prior zone. That fresh-datagram +
    /// stale-tick state is exactly what the #477 WRITE-command guard rejects as "the net thread has
    /// exited or wedged" — a DISHONEST 503 against a healthy session mid-transition. This proves the
    /// retry loop bumps `last_tick` so a mid-handshake session stays under `SESSION_STALE_TICK_MS`.
    #[tokio::test]
    async fn connect_keeps_last_tick_fresh_across_a_silent_handshake() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        // Enter the handshake with a STALE tick clock (a full minute), so any freshness afterward can
        // only have come from the retry loop bumping it — not from the Default::now() seed.
        net_health.lock().unwrap().last_tick =
            std::time::Instant::now() - std::time::Duration::from_secs(60);

        // A bound but SILENT peer — it never sends SESSION_RESPONSE, so `connect` stays in its retry
        // loop (the recv branch never fires; only the per-second retry path can refresh the clock).
        let silent_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = silent_peer.local_addr().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();

        // Drive `connect` for ~1.5s (well past the SESSION_REQUEST_RETRY cadence), then cancel it by
        // dropping the future.
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(1500),
            EqStream::connect(&addr.ip().to_string(), addr.port(), tx, net_health.clone()),
        ).await;

        let age = net_health.lock().unwrap().last_tick.elapsed();
        assert!(
            (age.as_millis() as u64) < eqoxide_http::SESSION_STALE_TICK_MS,
            "mid-handshake `last_tick` must stay under SESSION_STALE_TICK_MS ({}ms) so the #477 guard \
             does not falsely reject a healthy handshaking session; age was {age:?}",
            eqoxide_http::SESSION_STALE_TICK_MS,
        );
    }

    /// #335: the zone handoff dropped its blind fixed pre-connect sleeps (a 300ms + two 800ms). This
    /// pins the SESSION-layer half of what replaced them — that `connect` re-sends SESSION_REQUEST
    /// sub-second so a cold on-demand zone whose listener has just bound comes up promptly (our first
    /// SESSION_REQUEST may be dropped) instead of stalling ~1s. NOTE this is ONLY the transport
    /// handshake; it does NOT wait for app-layer zone acceptance, and a faster cadence here can even
    /// WIDEN the app-layer AddAuth race (`zone-entry-handshake-race.md`). That race is NOT rescued by a
    /// second OP_ZoneEntry (which self-kicks an admitted client — `run_zone_entry_handshake`,
    /// `zone-entry-duplicate-on-admitted-client.md`); a lost race becomes an honest 30s zone-in failure.
    ///
    /// #549: this used to drive a real `connect()` over a real UDP socket and count SESSION_REQUEST
    /// datagrams received by a silent peer inside a real-time 600ms window. It passed in isolation but
    /// went red under `--test-threads=8` contention — the assertion depended on this thread actually
    /// getting scheduled every ~50ms, which a loaded CI box does not guarantee, so it was measuring
    /// scheduler luck, not the transport. `connect`'s retry decision is `session_request_due(last_send,
    /// now)` (see its doc), which needs no real waiting to exercise — `Instant + Duration` builds a
    /// synthetic timeline instantly. This drives that exact production function over a virtual 600ms
    /// timeline sampled every 10ms (no sleeping, no sockets, no scheduler dependence) and asserts the
    /// same property the old test asserted over real time: at the 250ms cadence that is 3 retries
    /// (~t=250, 500, 600ms boundary catches the 3rd only if sampled finely enough — assert >= 2, exactly
    /// mirroring the old real-time threshold); at the old hard-coded 1s cadence it would be 0.
    #[test]
    fn connect_retries_session_request_faster_than_once_a_second() {
        let base = Instant::now();
        let step = std::time::Duration::from_millis(10);
        let window = std::time::Duration::from_millis(600);

        let mut last_send = base; // `connect` sends once before entering the retry loop.
        let mut retries = 0usize;
        let mut t = base;
        while t <= base + window {
            if session_request_due(last_send, t) {
                retries += 1;
                last_send = t;
            }
            t += step;
        }

        assert!(
            retries >= 2,
            "connect must re-send SESSION_REQUEST faster than once a second so a too-early connect to \
             a cold zone brings its SESSION up promptly (#335 — the app-layer acceptance race is a \
             separate concern, handled by run_zone_entry_handshake's honest 30s failure, not here); \
             saw only {retries} retries over a virtual 600ms window at the {:?} cadence",
            SESSION_REQUEST_RETRY,
        );
    }

    /// #549: pins the exact cadence boundary of `session_request_due` (the predicate `connect` calls
    /// on every retry-loop iteration) — not due a moment before `SESSION_REQUEST_RETRY` has elapsed,
    /// due at and after it. Complements the timeline test above, which exercises it over a window;
    /// this isolates the single-comparison edge case.
    #[test]
    fn session_request_due_boundary() {
        let base = Instant::now();
        assert!(!session_request_due(base, base));
        assert!(!session_request_due(base, base + SESSION_REQUEST_RETRY - std::time::Duration::from_millis(1)));
        assert!(session_request_due(base, base + SESSION_REQUEST_RETRY));
        assert!(session_request_due(base, base + SESSION_REQUEST_RETRY + std::time::Duration::from_millis(1)));
    }

    /// #549 review follow-up: the two tests above exercise `session_request_due` in isolation — they
    /// pass unchanged even if `connect()` is WIRED to it incorrectly, e.g. `let _ =
    /// session_request_due(...); if true { ... }` (ignoring its return value), or `if
    /// session_request_due(...) { send(); /* forgot */ last_send = Instant::now(); }` (never resets
    /// `last_send`, so the predicate free-runs `true` forever after the first fire). Either wiring bug
    /// makes `connect()` flood a cold zone's UDP listener with SESSION_REQUEST roughly every ~100ms
    /// (bounded only by the loop's inner `recv` timeout) instead of every `SESSION_REQUEST_RETRY`
    /// (250ms) — and neither pure-function test above would notice, because neither calls `connect()`.
    ///
    /// This drives the REAL `connect()` over a REAL silent UDP peer and puts an UPPER bound on
    /// datagrams received in a real-time window — deliberately the mirror image of the OLD flaky test
    /// this file used to have (which asserted a LOWER bound and flaked under `--test-threads=8`
    /// contention). The direction matters: scheduler contention can only ever DELAY a send, never
    /// conjure an extra one, so contention can only push this count DOWN, never up — a ceiling cannot
    /// flake from load the way a floor did. At the correct 250ms cadence a 700ms window sees ~3 sends
    /// (t≈0, 250, 500); a wiring bug firing every ~100ms sees ~7. Do NOT tighten this bound "for
    /// precision" — the slack between 3 and 7 is what makes it contention-proof; a tighter ceiling
    /// would reintroduce exactly the flake #549 fixed.
    #[tokio::test]
    async fn connect_does_not_flood_session_request_faster_than_the_cadence() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        // A bound but SILENT peer — never answers SESSION_RESPONSE, so `connect` stays retrying.
        let silent_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = silent_peer.local_addr().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();

        let host = addr.ip().to_string();
        let port = addr.port();

        let connect_fut = tokio::time::timeout(
            std::time::Duration::from_millis(900),
            EqStream::connect(&host, port, tx, net_health.clone()),
        );
        let count_fut = async {
            let mut buf = vec![0u8; 4096];
            let mut count = 0usize;
            let window = tokio::time::Instant::now() + std::time::Duration::from_millis(700);
            while tokio::time::Instant::now() < window {
                if let Ok(Ok(_)) =
                    tokio::time::timeout(std::time::Duration::from_millis(20), silent_peer.recv(&mut buf)).await
                {
                    count += 1;
                }
            }
            count
        };
        let (_, count) = tokio::join!(connect_fut, count_fut);

        assert!(
            count <= 3,
            "connect must not send SESSION_REQUEST faster than its {:?} cadence — saw {count} in a \
             700ms real-time window (expected ~3 at the correct cadence; a wiring bug that ignores \
             the retry predicate's return value, or forgets to reset `last_send` after sending, fires \
             roughly every 100ms instead — ~7 in this window)",
            SESSION_REQUEST_RETRY,
        );
    }

    #[test]
    fn test_crc32_zero_key() {
        let data = b"hello world";
        let crc = eq_crc32(data, 0);
        // Just verify it doesn't panic and produces a deterministic value
        let crc2 = eq_crc32(data, 0);
        assert_eq!(crc, crc2);
    }

    #[test]
    fn test_crc32_keyed() {
        let data = b"test";
        let crc1 = eq_crc32(data, 0x12345678);
        let crc2 = eq_crc32(data, 0x12345678);
        let crc3 = eq_crc32(data, 0x87654321);
        assert_eq!(crc1, crc2);
        assert_ne!(crc1, crc3);
    }

    #[test]
    fn test_xor_roundtrip() {
        let data = vec![0x01, 0x02, 0x03, 0x04, 0x05];
        let key: u32 = 0xDEADBEEF;
        let encoded = decode_xor(&data, key);
        let decoded = decode_xor(&encoded, key);
        assert_eq!(data, decoded);
    }

    #[test]
    fn classify_seq_reacks_duplicates_not_delivers() {
        use super::{classify_seq, SeqClass};
        // The next expected packet is delivered.
        assert_eq!(classify_seq(5, 5), SeqClass::Deliver);
        // A gap ahead is a future packet (buffer + OOO).
        assert_eq!(classify_seq(6, 5), SeqClass::Future);
        // A packet BEHIND the next expected is an already-delivered retransmit — must be re-ACKed,
        // NOT dropped, or the server's 30s resend_timeout linkdeads us (#158).
        assert_eq!(classify_seq(4, 5), SeqClass::Duplicate);
        assert_eq!(classify_seq(0, 5), SeqClass::Duplicate);
        // Wrap-around: next just past the top, a low seq is the FUTURE (wrapped) packet…
        assert_eq!(classify_seq(2, 0xF001), SeqClass::Future);
        // …while a seq just below `next` (also near the top) is a past duplicate.
        assert_eq!(classify_seq(0xF000, 0xF001), SeqClass::Duplicate);
    }

    #[tokio::test]
    async fn combined_reliable_subpacket_is_not_double_decoded() {
        // #302: a reliable OP_Packet bundled inside an OP_Combined has an already-plaintext body
        // (the combined body was decoded once). Re-decoding it — as the old dispatch_transport path
        // did — corrupts its seq under XOR, so it's never ACKed and the server's resend_timeout (30s)
        // linkdeads an idle client. Drive exactly that datagram and assert the inner app packet is
        // delivered intact (which requires the seq to be read correctly, i.e. NOT double-decoded).
        let key = 0x1234_5678u32;
        let (mut stream, mut rx) = test_stream(ENCODE_XOR, key).await;

        // Reliable inner: seq(BE u16 = 0) + app packet (opcode 0x1234 LE, payload AA BB).
        let inner = [0x00u8, 0x00, 0x34, 0x12, 0xAA, 0xBB];
        // Combined sub-packet: [0x00, OP_PACKET] + inner.
        let mut sub = vec![0x00u8, OP_PACKET];
        sub.extend_from_slice(&inner);
        // Combined body: length-prefixed sub.
        let mut body = vec![sub.len() as u8];
        body.extend_from_slice(&sub);
        // The server XOR-encodes the whole datagram body once.
        let encoded = encode_passes(&body, ENCODE_XOR, ENCODE_NONE, key);
        // Datagram: [0x00, OP_COMBINED] + encoded (crc_bytes = 0 in this session).
        let mut dgram = vec![0x00u8, OP_COMBINED];
        dgram.extend_from_slice(&encoded);

        stream.on_raw_recv(&dgram);

        let app = rx.try_recv().expect("combined reliable sub must deliver its app packet (no double-decode)");
        assert_eq!(app.opcode, 0x1234);
        assert_eq!(app.payload, vec![0xAA, 0xBB]);
    }

    /// #463: an inbound reliable GAP must (a) emit an OP_OutOfOrderAck whose sequence is ENCODED with
    /// the negotiated passes — identical framing to OP_ACK, since the server decodes the bytes after
    /// the `[0x00, opcode]` header before reading the seq (EQEmu `reliable_stream_connection.cpp:476`)
    /// — so the server can act on it and go-back-N fast-retransmit the missing packet; and (b) once the
    /// gap-filling packet arrives, DRAIN the buffered tail and deliver the stranded spawns IN ORDER.
    ///
    /// Under an XOR stream (login/world) an unencoded OutOfOrderAck decodes to a WRONG sequence, so the
    /// server drops the wrong entry from its resend window — the exact fragility this test guards. The
    /// encoded-body assertion is the mutation check: revert `send_out_of_order` to `send_raw(.., &seq
    /// .to_be_bytes())` and the decoded-seq assertion fails (the raw bytes XOR-decode to garbage).
    #[tokio::test]
    async fn inbound_gap_emits_encoded_out_of_order_then_drains_tail_in_order() {
        let key = 0xA1B2_C3D4u32;
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        let (mut stream, mut rx, peer, _addr) =
            test_stream_with_peer_encoded(ENCODE_XOR, key, net_health).await;

        // Build an inbound reliable OP_Packet exactly as the server frames it: [0x00, OP_PACKET] then
        // the XOR-encoded (seq(BE) + app-packet). App packet = opcode(LE u16) + payload. crc_bytes = 0.
        let reliable = |seq: u16, opcode: u16, payload: &[u8]| -> Vec<u8> {
            let mut inner = seq.to_be_bytes().to_vec();
            inner.extend_from_slice(&opcode.to_le_bytes());
            inner.extend_from_slice(payload);
            let enc = encode_passes(&inner, ENCODE_XOR, ENCODE_NONE, key);
            let mut dgram = vec![0x00u8, OP_PACKET];
            dgram.extend_from_slice(&enc);
            dgram
        };

        // Warm the stream socket's write registration with one async send so the synchronous
        // `try_send`s inside `on_raw_recv` (our ACK / OutOfOrderAck) reliably transmit to the peer in
        // this single-threaded test runtime. The peer's drain loop skips this <2-byte primer.
        stream.socket.send(&[0u8]).await.unwrap();

        // seq 0 arrives in order → delivered, next_recv_seq advances to 1.
        stream.on_raw_recv(&reliable(0, 0x1000, &[0xA0]));
        // seq 2 arrives with a GAP at 1 → buffered as Future, and an OP_OutOfOrderAck(2) is sent.
        stream.on_raw_recv(&reliable(2, 0x1002, &[0xA2]));

        // Let the just-sent datagrams settle on the peer socket before draining.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Drain every datagram the stream sent to the peer and locate the OP_OutOfOrderAck.
        let mut ooo_seq: Option<u16> = None;
        let mut ooo_body_raw: Option<Vec<u8>> = None;
        let mut buf = [0u8; 256];
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(80), peer.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) => {
                    let d = &buf[..n];
                    if d.len() >= 2 && d[0] == 0x00 && d[1] == OP_OUT_OF_ORDER {
                        ooo_body_raw = Some(d[2..].to_vec());
                        let dec = decode_passes(&d[2..], ENCODE_XOR, ENCODE_NONE, key).unwrap();
                        ooo_seq = Some(u16::from_be_bytes([dec[0], dec[1]]));
                    }
                }
                _ => break, // timed out: no more datagrams
            }
        }

        let raw = ooo_body_raw.expect("a gap must put an OP_OutOfOrderAck on the wire (#463)");
        // It must be ENCODED, not raw: under a non-zero XOR key the encoded body differs from the
        // plain BE seq bytes. (Mutation guard: the pre-fix `send_raw(.., &seq.to_be_bytes())` fails.)
        assert_ne!(raw, 2u16.to_be_bytes().to_vec(),
            "OP_OutOfOrderAck seq must be ENCODED like OP_ACK, not sent raw (#463)");
        assert_eq!(ooo_seq, Some(2),
            "the encoded OP_OutOfOrderAck must decode to the buffered future seq 2 (#463)");

        // The tail is still stranded (seq 1 missing): only seq 0's app packet delivered so far.
        let first = rx.try_recv().expect("seq 0 delivered");
        assert_eq!(first.opcode, 0x1000);
        assert!(rx.try_recv().is_err(), "seq 2 must stay buffered until the gap fills");

        // The server fast-retransmits seq 1 → the gap fills, and the buffered tail (seq 2) drains
        // IN ORDER right behind it.
        stream.on_raw_recv(&reliable(1, 0x1001, &[0xA1]));
        let a1 = rx.try_recv().expect("gap-filler seq 1 delivered");
        assert_eq!(a1.opcode, 0x1001);
        let a2 = rx.try_recv().expect("buffered seq 2 must drain right after the gap fills (#463)");
        assert_eq!(a2.opcode, 0x1002);
        assert!(rx.try_recv().is_err(), "nothing beyond the drained tail");
    }

    #[test]
    fn seq_leq_handles_wraparound() {
        use super::seq_leq;
        // Plain ordering: a cumulative ACK(acked) clears every sent seq <= acked.
        assert!(seq_leq(5, 5));            // equal → acked
        assert!(seq_leq(3, 5));            // before → acked
        assert!(!seq_leq(6, 5));           // after → not yet acked
        // Wrap-around: 0xFFFE precedes 0x0001, so an ACK(0x0001) clears 0xFFFE/0xFFFF/0x0000/0x0001…
        assert!(seq_leq(0xFFFE, 0x0001));
        assert!(seq_leq(0xFFFF, 0x0001));
        assert!(seq_leq(0x0000, 0x0001));
        // …but NOT a seq ahead of the wrapped ack.
        assert!(!seq_leq(0x0002, 0x0001));
        assert!(!seq_leq(0x0001, 0xFFFE)); // ahead across the wrap → not acked
    }

    #[test]
    fn cumulative_ack_pops_front_run_including_wrap() {
        use super::seq_leq;
        // Mirror ack_up_to: pop the front run of outstanding seqs that are <= acked.
        let popped = |outstanding: &[u16], acked: u16| -> usize {
            outstanding.iter().take_while(|&&s| seq_leq(s, acked)).count()
        };
        assert_eq!(popped(&[10, 11, 12, 13], 12), 3); // acks 10,11,12; 13 stays
        assert_eq!(popped(&[10, 11, 12], 9), 0);      // stale ack behind the base → nothing
        assert_eq!(popped(&[10, 11, 12], 12), 3);     // acks the whole window
        // A gap in the window (a seq removed by OP_OutOfOrder) doesn't break cumulative ack.
        assert_eq!(popped(&[10, 12, 13], 13), 3);
        // Wrap: window straddles the u16 top; ACK(1) clears through the wrap.
        assert_eq!(popped(&[0xFFFE, 0xFFFF, 0x0000, 0x0001, 0x0002], 0x0001), 4);
    }

    #[test]
    fn resend_delay_backs_off_and_clamps() {
        use super::{resend_delay, RESEND_MAX_MS};
        assert_eq!(resend_delay(0).as_millis(), 850);   // first attempt ≈ ping-seeded
        assert_eq!(resend_delay(1).as_millis(), 1700);  // doubled
        assert_eq!(resend_delay(2).as_millis(), 3400);
        assert_eq!(resend_delay(3).as_millis(), RESEND_MAX_MS as u128); // 6800 → clamped to 5000
        assert_eq!(resend_delay(9).as_millis(), RESEND_MAX_MS as u128); // shift capped, stays clamped
    }

    #[test]
    fn test_compress_roundtrip() {
        let data = b"hello world this is a test of the compression system";
        let compressed = eq_compress(data);
        let decompressed = eq_decompress(&compressed).unwrap();
        assert_eq!(data.to_vec(), decompressed);
    }

    #[test]
    fn test_compress_small_data() {
        let data = b"short";
        let compressed = eq_compress(data);
        assert_eq!(compressed[0], 0xa5); // raw prefix for small data
        assert_eq!(&compressed[1..], data);
    }

    // ── Raw (unreliable) application packets ──────────────────────────────────
    // EQEmu sends NPC position updates (Mob::SendPosUpdate → QueuePacket(.., ack_req=false))
    // as RAW application packets: the datagram is NOT wrapped in OP_Packet/OP_Fragment. The
    // first byte is the app opcode's low byte (left plain by the server's offset-1 encode);
    // the remainder is encode-pass'd just like a reliable body. The non-zero lead byte is how
    // the wire distinguishes these from protocol packets (which always lead with 0x00).

    #[test]
    fn raw_app_packet_decodes_uncompressed() {
        // EncodeNone: the body is just the app packet [opcode_lo, opcode_hi, payload…].
        let opcode: u16 = 0x14cb; // OP_ClientUpdate (Titanium)
        let payload = [1u8, 2, 3, 4, 5];
        let mut body = opcode.to_le_bytes().to_vec();
        body.extend_from_slice(&payload);

        let (op, pl) = decode_raw_app(&body, ENCODE_NONE, ENCODE_NONE, 0)
            .expect("raw app packet should decode");
        assert_eq!(op, 0x14cb);
        assert_eq!(pl, payload);
    }

    #[test]
    fn raw_app_packet_decodes_compressed() {
        // With a compression pass the server leaves byte0 (opcode low) plain and encodes
        // [opcode_hi, payload…] from offset 1, prepending a 0x5a/0xa5 flag.
        let opcode: u16 = 0x14cb;
        let payload: Vec<u8> = (0u8..40).collect(); // >30 bytes → exercise the zlib path
        let mut plain_rest = vec![(opcode >> 8) as u8];
        plain_rest.extend_from_slice(&payload);

        let mut body = vec![(opcode & 0xff) as u8];
        body.extend_from_slice(&eq_compress(&plain_rest));

        let (op, pl) = decode_raw_app(&body, ENCODE_COMPRESSION, ENCODE_NONE, 0)
            .expect("compressed raw app packet should decode");
        assert_eq!(op, 0x14cb);
        assert_eq!(pl, payload);
    }

    #[test]
    fn raw_app_packet_rejects_empty() {
        assert!(decode_raw_app(&[], ENCODE_NONE, ENCODE_NONE, 0).is_none());
        // Lead byte only, nothing after it to carry opcode_hi.
        assert!(decode_raw_app(&[0xcb], ENCODE_NONE, ENCODE_NONE, 0).is_none());
    }

    #[test]
    fn encode_raw_app_round_trips_through_decode() {
        // What send_app_packet_unreliable puts on the wire (minus outer CRC) must decode back to
        // the same opcode+payload under every negotiated encode mode — this is the OP_ClientUpdate
        // firehose that must reach the server unreliably to avoid the linkdead (eqoxide#127).
        let opcode: u16 = 0x7dfc; // OP_ClientUpdate (RoF2)
        let payload: Vec<u8> = (0u8..46).collect(); // a full 46-byte position struct's worth
        let key = 0x1357_9bdf;
        for &(p1, p2) in &[
            (ENCODE_NONE, ENCODE_NONE),
            (ENCODE_XOR, ENCODE_NONE),
            (ENCODE_COMPRESSION, ENCODE_NONE),
        ] {
            let body = encode_raw_app(opcode, &payload, p1, p2, key);
            assert_ne!(body[0], 0x00, "lead byte must be non-zero so the wire reads it as a raw app packet");
            assert_eq!(body[0], (opcode & 0xFF) as u8, "lead byte is the plaintext opcode low byte");
            let (op, pl) = decode_raw_app(&body, p1, p2, key).expect("encoded raw app packet should decode");
            assert_eq!(op, opcode, "opcode round-trips (pass1={p1}, pass2={p2})");
            assert_eq!(pl, payload, "payload round-trips (pass1={p1}, pass2={p2})");
        }
    }

    #[test]
    fn test_fragment_buffer_single() {
        let mut fb = FragmentBuffer::new();
        let data = vec![0u8; 100];
        let mut prefixed = (data.len() as u32).to_be_bytes().to_vec();
        prefixed.extend_from_slice(&data);
        let result = fb.add(&prefixed, true);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), data);
    }

    #[test]
    fn test_fragment_buffer_multi() {
        let mut fb = FragmentBuffer::new();
        let total = vec![0xABu8; 200];
        let first_chunk = &total[..100];
        let second_chunk = &total[100..];

        let mut prefixed = (total.len() as u32).to_be_bytes().to_vec();
        prefixed.extend_from_slice(first_chunk);
        let result = fb.add(&prefixed, true);
        assert!(result.is_none());

        let result = fb.add(second_chunk, false);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), total);
    }
}
