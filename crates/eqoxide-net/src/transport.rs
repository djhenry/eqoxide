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

/// How many session-layer control datagrams (#641) may sit queued for retry at once. The measured
/// live bursts are in the low hundreds over a couple of seconds, so this holds a whole burst with
/// room to spare while still bounding memory if the socket were wedged for a long time. On overflow
/// the OLDEST entry is dropped — an `OP_ACK` is cumulative, so the newest one supersedes it — and is
/// counted as a genuine, unretried loss.
const MAX_PENDING_CONTROL: usize = 1024;

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
/// wait sits out the remainder of that tick before firing. The bound this quantization implies is
/// 250-350ms (up to +100ms of jitter per retry); measured effective interval in practice clusters
/// tighter, around ~300ms (300-308ms across sampled runs), not the full range nor a clean 250ms.
/// Documented rather than "fixed" (making the wait wake exactly at the due instant instead of polling
/// every 100ms) because this is a live network path whose regression test
/// (`connect_does_not_flood_session_request_faster_than_the_cadence`) just had a real flakiness
/// problem removed (#549) — the added complexity and retest burden of an exact-wake timer isn't worth
/// it for jitter that only ever makes the retry later, never faster/floodier.
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

/// #612 review (F5): after a send failure is WARNed, further failures log at debug for this long.
/// A 30s outage otherwise emits ~10^3 WARN lines (20Hz position firehose + go-back-N retransmits).
/// The COUNTERS are never suppressed — only the log line is.
const SEND_FAIL_WARN_QUIET: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether a datagram handed to [`EqStream::transmit`] is retained by the reliable resend window,
/// i.e. whether THIS EXACT datagram will be re-sent if the send fails (#612).
///
/// Deliberately narrow: `Retransmitted` is a claim about *this* datagram being kept verbatim in
/// `EqStream::sent` and re-sent by `poll_resend` until ACKed — a structural guarantee visible right
/// there in `send_tracked`. It is NOT a claim that "the information gets there eventually" for the
/// `None` cases, several of which do have looser higher-level recovery. Keeping the distinction that
/// tight is what lets `NetHealth::send_failures_unretried` mean something checkable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendRetry {
    /// Retained in the resend window; `poll_resend` re-sends this same datagram until it is ACKed.
    Retransmitted,
    /// Session-layer control (ACK / OutOfOrderAck / keepalive / SessionRequest / SessionDisconnect).
    /// A transient `WouldBlock` on one of these is queued in `pending_control` and re-sent on the
    /// next tick rather than dropped (#641) — so it is counted in `send_deferred`, not
    /// `send_failures`. Any OTHER error is a real loss and counted as such: retrying an `EMSGSIZE`
    /// forever would just be a lie with a different shape.
    Deferred,
    /// Not retained. If this send fails, this datagram is gone.
    None,
}

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

/// Is this send error transient — i.e. would the very same datagram plausibly go out if we simply
/// tried again in ~10ms? (#641)
///
/// `EAGAIN`/`EWOULDBLOCK` is the obvious one. `ENOBUFS` belongs here too and was missed in the first
/// cut (#641 review, finding 7): on UDP it means the device/qdisc transmit queue was momentarily
/// full, which is *exactly* the pressure this fix is about, and it drains on the same timescale.
/// Rust maps it to `ErrorKind::Uncategorized`, which is unstable to match on, so it is identified by
/// errno — hence the Linux gate; on any other platform it falls through to "not transient", which
/// degrades to the pre-#641 behaviour of counting it as a loss rather than mis-deferring something.
///
/// Everything else (`EMSGSIZE`, `ENETUNREACH`, a closed socket) is NOT transient: retrying it on
/// every tick would never deliver it and would hide a real, permanent loss behind a "will be
/// retried" counter — the #612 bug wearing a different hat.
fn is_transient(e: &std::io::Error) -> bool {
    /// `ENOBUFS`. Linux-specific value; other unices differ, which is why this is `cfg`-gated.
    #[cfg(target_os = "linux")]
    const ENOBUFS: i32 = 105;
    if e.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(target_os = "linux")]
    if e.raw_os_error() == Some(ENOBUFS) {
        return true;
    }
    false
}

/// Send a datagram on an already-`connect()`ed tokio `UdpSocket` via a direct `send(2)`, bypassing
/// tokio's cached-readiness gate (#641).
///
/// This exists for exactly one caller — `EqStream::transmit`'s `WouldBlock` rescue — and it is the
/// only way to tell tokio's SYNTHETIC `WouldBlock` (readiness bit empty, no syscall attempted) from
/// a real kernel `EAGAIN`/`ENOBUFS`. `try_send`/`try_io` cannot: both consult the same cache and
/// short-circuit before reaching the kernel.
///
/// The fd is borrowed, never owned: `ManuallyDrop` means the temporary `std::net::UdpSocket` is not
/// closed when it drops, so tokio keeps sole ownership of the descriptor. The socket is non-blocking
/// (tokio set it that way) and connected, so `send` is a single non-blocking `send(2)` — it can
/// never park the net thread.
#[cfg(unix)]
fn raw_send_bypassing_readiness_cache(
    socket: &UdpSocket,
    datagram: &[u8],
) -> std::io::Result<usize> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: `socket` outlives this borrow, and `ManuallyDrop` suppresses the `close(2)` that
    // dropping the reconstructed `std::net::UdpSocket` would otherwise perform — so ownership of
    // the descriptor stays with `socket` and it is not closed twice.
    let borrowed =
        std::mem::ManuallyDrop::new(unsafe { std::net::UdpSocket::from_raw_fd(socket.as_raw_fd()) });
    borrowed.send(datagram)
}

/// Non-unix fallback: no fd to borrow, so there is no rescue and a `WouldBlock` is reported as the
/// failure it is. The client is only built and run on Linux today; this keeps the crate portable
/// without pretending the rescue happened.
#[cfg(not(unix))]
fn raw_send_bypassing_readiness_cache(
    _socket: &UdpSocket,
    _datagram: &[u8],
) -> std::io::Result<usize> {
    Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
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
    /// Session-layer control datagrams whose send hit a transient `WouldBlock` and which are waiting
    /// to be retried on a later tick, in send order (#641).
    ///
    /// The reliable stream has had a resend window since #254; the CONTROL path had nothing, so a
    /// failed ACK was simply dropped — measured live at 44–306 per zone-in on a CPU-starved client,
    /// every one a 7-byte datagram. Since the server retransmits anything it has not seen
    /// acknowledged, and gives up at its ~30s `resend_timeout`, dropping ACKs is exactly the wrong
    /// thing to do under load. ACKs are cheap and idempotent, so the honest response to a transient
    /// refusal is to try again ~10ms later, which is what `flush_pending_control` does.
    ///
    /// Drained FIFO from `poll_recv` (every loop tick, in all four loops that own a stream), from
    /// `connect()`'s handshake loop (which does not call `poll_recv` — #641 review, finding 8), and
    /// again immediately before any new control send, so ordering on the wire is preserved.
    ///
    /// CONTROL ONLY. The unreliable `OP_ClientUpdate` firehose is deliberately not deferred — a
    /// stale position is arguably worse than a missing one — but that is a judgement, not a
    /// measurement, and it is tracked as #655 rather than settled here.
    pending_control: VecDeque<Vec<u8>>,
    /// TEST-ONLY fault injection (#641): the next N `transmit` calls behave as though the kernel
    /// refused the datagram with `EAGAIN`, without touching the socket. There is no portable,
    /// deterministic way to make a real `send(2)` on a connected UDP socket return `EAGAIN` from a
    /// unit test (loopback drains instantly; `tc`/blackhole routes need root), and the branch this
    /// forces — "the kernel really said no, so queue it and retry" — is precisely the half of #641
    /// that the raw-`send(2)` rescue does NOT cover, so it must not go untested.
    /// `Cell` because `transmit` takes `&self`.
    #[cfg(test)]
    force_send_refusals: std::cell::Cell<u32>,
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
            pending_control: VecDeque::new(),
            #[cfg(test)]
            force_send_refusals: std::cell::Cell::new(0),
            frags: FragmentBuffer::new(),
            net_health,
            app_tx,
        };

        // The very first send on a freshly connect()ed UDP socket can return WouldBlock and be silently
        // dropped by `send_raw`'s `let _ = try_send(..)`. Cause: tokio caches socket readiness and starts
        // it EMPTY (`scheduled_io.rs`); a `try_*` call against empty cached readiness returns a SYNTHETIC
        // WouldBlock without even attempting the syscall (`registration.rs`) until some task observes a
        // real writable-readiness event first. Neither `bind` nor `connect` above does real I/O, so
        // nothing has given that a chance to happen yet at this point — UNLESS another OS thread already
        // did it concurrently, which is exactly what makes this runtime-dependent (#603, verified
        // empirically, fresh runtime per trial, cold — no test in this crate covers the current_thread
        // case, since `#[tokio::test]`'s default runtime and this one differ; see below):
        //   - current_thread (`#[tokio::test]`'s default): DETERMINISTIC — no worker thread exists to
        //     race the readiness event against this call, so it WouldBlocks 100/100 trials, every time.
        //   - multi_thread (`Builder::new_multi_thread()`, production's actual shape — `src/main.rs`):
        //     a RACE — a worker thread parked in `epoll_wait` can independently service the readiness
        //     event while this call is in flight, so it sometimes already succeeds. Measured WouldBlock
        //     rate roughly 60-90/100 depending on machine and load (three independent measurements: two
        //     in the 84-90 range, one at 67, one clustered 63-68; worker-count sensitivity pushed it as
        //     low as 35/100 at 2 workers in one run) — i.e. in production this send was sometimes already
        //     reaching the wire, not reliably, but not never either; the earlier claim that this line
        //     "never" worked and the loop did "100%" of the work was itself an overstated universal,
        //     corrected here. Don't treat any single number above as precise — the point is that it's a
        //     RACE, not a constant, and no fixed percentage is true across machines.
        // Either way the outcome the code wants — the datagram genuinely on the wire — was never
        // guaranteed, only sometimes true by luck of scheduling. Awaiting `writable()` once removes the
        // luck: it forces a real readiness event to be observed before the send, converting the
        // nondeterministic race (a majority-but-not-universal failure rate in production's own runtime
        // shape, see above) into a deterministic 100/100 success, at negligible cost (measured on
        // production's multi_thread shape: max 55µs / mean 16µs over 100 cold trials this session;
        // reviewer independently measured max 484µs / mean 14.5µs in a different environment — both
        // negligible next to the ~300ms retry cadence below).
        // The retry loop's own sends don't need this: once readiness has been observed once, tokio keeps
        // it marked ready for the life of the socket.
        //
        // Bounded to 500ms (#603 review, F3): this is the only new unbounded wait on the connect path.
        // No live hang was found in review (driver shutdown returns an `Err` here, which — like a
        // WouldBlock — is swallowed below and degrades to the pre-#603 behavior of relying entirely on
        // the retry loop), but a bound costs one line and turns "degrades on some theoretical future
        // driver-shutdown path" into "provably can't add more than 500ms to connect() even in that case".
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), stream.socket.writable()).await;
        // DELIBERATE (#612): the pre-loop SESSION_REQUEST's send outcome is now observed rather than
        // discarded. It is NOT turned into a `connect()` failure: the retry loop below re-sends on
        // the `SESSION_REQUEST_RETRY` cadence for a full 20s, so one failed send is recoverable and
        // returning `Err` here would abort a handshake that would have succeeded. The failure is
        // counted in `NetHealth::send_failures` by `transmit` and WARNed there, so it is visible
        // instead of invented-away. (This is the #603/#610 site — the `writable().await` above is
        // what makes this send deterministic, and must not be removed.)
        if let Err(e) = stream.send_session_request() {
            tracing::warn!("NET: pre-loop SESSION_REQUEST did not reach the wire ({:?}); the \
                            handshake retry loop will re-send (#612)", e.kind());
        }

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
                // DELIBERATE (#612): a failed retry is counted + WARNed by `transmit`; this loop's
                // own cadence IS the recovery, and the 20s deadline above still bounds it, so there
                // is nothing honest to abort on here.
                let _ = stream.send_session_request();
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
            // #641 review, finding 8: this loop does not call `poll_recv`, so without this the
            // deferral queue would drain only as a side effect of the next SESSION_REQUEST retry
            // (~250ms) — the "every loop ticks the queue every ~10ms" property the design leans on
            // had a hole exactly here. One call per ≤100ms iteration closes it; it is a no-op
            // whenever the queue is empty, which is almost always.
            stream.flush_pending_control();
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
    fn send_session_request(&mut self) -> std::io::Result<()> {
        let connect_code: u32 = rand::random::<u32>() & 0x7FFFFFFF;
        self.session.connect_code = connect_code;
        let mut payload = Vec::new();
        payload.write_u32::<BigEndian>(2).unwrap(); // protocol version
        payload.write_u32::<BigEndian>(connect_code).unwrap();
        payload.write_u32::<BigEndian>(self.session.max_packet_size as u32).unwrap();
        // Returned, not discarded (#612). `connect()` decides what to do with it — see there.
        self.send_raw(OP_SESSION_REQUEST, &payload)
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
        // DELIBERATE (#612) — the call-site decision for the RELIABLE app path, which is what every
        // agent-issued command travels on. The error is NOT propagated to the ~90 call sites, and
        // that is not a discard: a failed reliable send leaves the datagram in the resend window
        // (see `send_tracked`), so `poll_resend` re-sends it verbatim until the server ACKs it —
        // FOR AS LONG AS THE SESSION LIVES. Returning "failed" here would be a lie in the opposite
        // direction while that holds: the packet is delayed by one backoff, not lost.
        // It does NOT hold across a session end (#612 review F1): when a zone handoff or world
        // reconnect replaces the stream, whatever is still outstanding is abandoned — which
        // `EqStream::drop` counts into `NetHealth::reliable_abandoned`, and clean shutdown accounts
        // explicitly (see `abandon_outstanding`). A SERVER-side ~30s `resend_timeout` drop is NOT
        // covered by that counter: the client never notices one today, so the stream is never torn
        // down and nothing counts it (#642). Do not read `reliable_abandoned == 0` as proof that
        // case did not happen. (Round-3 review B2 caught this comment claiming the opposite.)
        // What the agent needs is (a) the fact recorded, which `transmit` does in
        // `NetHealth::send_failures` (pollable at /v1/observe/debug), (b) `reliable_abandoned` for
        // the session-ends-under-our-control case, and (c) `connected: false` — which the 15s link
        // clock raises BEFORE the server's 30s drop, and which is therefore the ONLY honest signal
        // for the uncovered case.
        if let Err(e) = self.send_reliable(&app_data) {
            tracing::debug!(
                "NET: reliable opcode 0x{:04X} failed its first send ({:?}); retained in the resend \
                 window and will be retransmitted (#612)", opcode, e.kind(),
            );
        }
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
    pub fn send_app_packet_unreliable(&mut self, opcode: u16, payload: &[u8]) -> std::io::Result<()> {
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
        // DELIBERATE (#612): unreliable by construction — no sequence, no resend window, nothing
        // re-sends this datagram. Returned to the caller so it can react, and counted in BOTH
        // `send_failures` and `send_failures_unretried` by `transmit`. This is the one send class
        // where "it did not go out" is genuinely unrecoverable at the transport layer, which is
        // exactly why it must be observable rather than discarded.
        self.transmit(&datagram, SendRetry::None)
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
        // #641: retry any control datagram (ACK / OutOfOrderAck / keepalive / session control) that a
        // transient WouldBlock deferred. Here because every loop that owns a stream calls `poll_recv`
        // each ~10ms tick — the same property `poll_resend` depends on — so no loop has to remember
        // to drain the queue itself.
        self.flush_pending_control();
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
            // DELIBERATE (#612): a failed RETRANSMIT is counted (via `transmit`) but not propagated.
            // The entry stays in `self.sent` with its backoff advanced, so the very next due pass
            // re-sends this same datagram — the recovery is this loop itself, and there is no caller
            // above it to hand an error to. Not silent: `transmit` stamps `NetHealth` + WARNs.
            if let Err(e) = self.transmit(&self.sent[i].datagram, SendRetry::Retransmitted) {
                tracing::debug!("NET: retransmit of seq {} failed ({:?}); will retry on the next \
                                 backoff pass (#612)", self.sent[i].seq, e.kind());
            }
            self.sent[i].sent_at = now;
            self.sent[i].retries = self.sent[i].retries.saturating_add(1);
        }
    }

    /// Send a keepalive response.
    pub fn send_keepalive(&mut self) -> std::io::Result<()> {
        self.send_raw(OP_KEEPALIVE, &[])
    }

    /// Send a session-layer disconnect (`OP_SessionDisconnect`, 0x05). Tells the EQStream peer
    /// we are closing this session. Payload is the negotiated `connect_code` as a big-endian u32;
    /// `append_crc` appends the CRC. Sent as part of clean shutdown.
    ///
    /// **NOT deferrable, deliberately (#641 review, finding 2).** Every other control datagram gets
    /// queued and re-sent on the next tick when the socket refuses it — but this one is the LAST
    /// thing a session ever sends, and for it "the next tick" does not exist:
    ///   - `perform_clean_shutdown` calls `abandon_outstanding` a few lines later, which clears the
    ///     queue;
    ///   - the `OP_GMKick` path then parks in `loop { sleep }` forever — it never calls `poll_recv`
    ///     again and is never unwound, so `Drop` never runs either.
    /// Deferring it there would have produced the exact failure this PR exists to prevent: a
    /// datagram that is never sent AND never counted as lost, sitting in `send_deferred`, whose
    /// documented meaning is "it went out, ~10ms late". Pre-#641 `main` counted it as a failure, so
    /// that would have been an honesty REGRESSION.
    ///
    /// `SendRetry::None` keeps the old accounting exactly: if it does not go out, it is counted as
    /// the loss it is. The queue is flushed first so anything already waiting still precedes it on
    /// the wire (best effort — a still-refusing socket keeps them queued, and `abandon_outstanding`
    /// then counts them lost).
    pub fn send_session_disconnect(&mut self) -> std::io::Result<()> {
        let mut payload = Vec::with_capacity(4);
        payload.write_u32::<BigEndian>(self.session.connect_code).unwrap();
        let mut raw = vec![0x00, OP_SESSION_DISC];
        raw.extend_from_slice(&payload);
        raw = self.append_crc(raw);
        self.flush_pending_control();
        self.transmit(&raw, SendRetry::None)
    }

    // ── Internal send helpers ─────────────────────────────────────────────────

    /// THE single point at which this client hands a datagram to the socket (#612).
    ///
    /// Before #612 there were four separate `let _ = self.socket.try_send(..)` calls, and every send
    /// error the kernel or tokio returned — `WouldBlock`, `ENOBUFS`, `EMSGSIZE`, `ENETUNREACH`, a
    /// dead socket — was discarded. A datagram that never left the machine was therefore
    /// indistinguishable from one that reached the server, both to this code and (through it) to the
    /// driving agent, which has no independent channel to reality. That is the agent-honesty
    /// invariant's central failure mode, one layer below #513 and #347.
    ///
    /// Funnelling every send through here makes the counting STRUCTURAL rather than a discipline:
    /// there is exactly one `try_send` in this crate, and it cannot fail without stamping
    /// `NetHealth` (which `/v1/observe/debug` projects at read time, so the agent can poll it). The
    /// `#[must_use]` on the returned `io::Result` means a future call site cannot re-introduce the
    /// bug by accident — it has to write the discard out loud.
    ///
    /// `retry` records whether THIS EXACT datagram is retained for retransmission (the reliable
    /// window) or is gone for good; see `NetHealth::send_failures_unretried`.
    ///
    /// ## The `WouldBlock` rescue (#641)
    ///
    /// `try_send` can return `WouldBlock` **without ever issuing the syscall**: tokio gates every
    /// `try_*` call on a cached readiness bit, and when that bit is empty it returns a SYNTHETIC
    /// `WouldBlock` immediately (`tokio` `io::registration`/`scheduled_io` — the same mechanism
    /// #603/#610 measured on the cold pre-loop `SESSION_REQUEST`). The bit is refilled only by
    /// tokio's io driver, so when the driver is starved of CPU the bit can stay empty across a
    /// whole `poll_recv` drain — and every ACK that drain wanted to send is dropped on the floor,
    /// including the ones the kernel would happily have taken.
    ///
    /// So on `WouldBlock` we re-attempt the datagram once through `send(2)` on the same fd,
    /// bypassing the readiness cache. That is BOTH the fix and the discriminator:
    ///   - the raw send succeeds → the `WouldBlock` was synthetic, the datagram is genuinely on the
    ///     wire now, and it is counted in `send_wouldblock_rescued` (not in `send_failures`, which
    ///     means "never reached the wire" and must not be inflated by a send that did);
    ///   - the raw send fails → the kernel really refused it. It is NOT all synthetic: one measured
    ///     `qeynos` zone-in split 141 rescued / 107 genuinely refused. The genuinely-refused ones
    ///     are handled a layer up — `send_raw` queues control datagrams in `pending_control` and
    ///     re-sends them on the next tick — and anything not deferrable falls through to the
    ///     unchanged failure accounting.
    ///
    /// It is deliberately ONE retry, not a loop: a genuine `EAGAIN` must stay observable rather
    /// than being spun on inside a non-blocking send path, and the tick retry is the recovery.
    #[must_use = "a send outcome must be handled — discarding it is the #612 agent-honesty bug"]
    fn transmit(&self, datagram: &[u8], retry: SendRetry) -> std::io::Result<()> {
        #[cfg(test)]
        let attempt: std::io::Result<usize> = if self.force_send_refusals.get() > 0 {
            self.force_send_refusals.set(self.force_send_refusals.get() - 1);
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        } else {
            self.attempt_send(datagram)
        };
        #[cfg(not(test))]
        let attempt = self.attempt_send(datagram);
        match attempt {
            Ok(_) => Ok(()),
            Err(e) => self.record_send_failure(datagram, retry, e),
        }
    }

    /// `try_send`, plus the raw-`send(2)` rescue described on `transmit`. Split out so the
    /// test-only refusal injection above can replace the whole socket interaction, and so `transmit`
    /// reads as "attempt, then account".
    fn attempt_send(&self, datagram: &[u8]) -> std::io::Result<usize> {
        match self.socket.try_send(datagram) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                match raw_send_bypassing_readiness_cache(&self.socket, datagram) {
                    Ok(n) => {
                        let mut h = self.net_health.lock().unwrap();
                        h.send_wouldblock_rescued = h.send_wouldblock_rescued.saturating_add(1);
                        Ok(n)
                    }
                    // The kernel refused it too. Report the KERNEL's error, not tokio's cached-
                    // readiness stand-in, so `last_send_error` names what actually happened.
                    Err(real) => Err(real),
                }
            }
            other => other,
        }
    }

    /// Account a send that did not reach the wire, and return the error unchanged.
    fn record_send_failure(
        &self,
        datagram: &[u8],
        retry: SendRetry,
        e: std::io::Error,
    ) -> std::io::Result<()> {
        let now = Instant::now();
        // A transient refusal of a DEFERRABLE datagram is not a loss: `send_raw` puts it in
        // `pending_control` and a later tick re-sends it. Counting it in `send_failures` would
        // republish the very number #641 exists to drive to zero, and would be false besides.
        // Anything that is NOT a transient refusal (EMSGSIZE, ENETUNREACH, a dead socket) is a real
        // loss on every path, deferrable or not — retrying those forever would not deliver them.
        //
        // Nothing is COUNTED here for a deferral. `defer_control` owns `send_deferred`, because this
        // function runs once per refusal EVENT — including every ~10ms re-attempt of the same
        // datagram from `flush_pending_control` — whereas the counter is documented, and now tested,
        // as counting DATAGRAMS (#641 review, finding 1: one datagram stuck for three ticks read as
        // `send_deferred == 3`, and the queue-nonempty path in `send_raw` counted nothing at all).
        if retry == SendRetry::Deferred && is_transient(&e) {
            return Err(e);
        }
        // Rate-limit decision is made from the SAME locked snapshot that updates the
        // counters, so it costs no extra lock and cannot race itself (#612 review, F5).
        let (loud, total) = {
            let mut h = self.net_health.lock().unwrap();
            // First failure of a burst is always loud; after that, at most one WARN per
            // `SEND_FAIL_WARN_QUIET`. A sustained outage otherwise produces ~10^3 WARN lines
            // in 30s (the 20Hz position firehose plus go-back-N retransmits), which buries
            // the very signal it is meant to raise.
            let loud = h.last_send_error_at
                .is_none_or(|t| now.saturating_duration_since(t) >= SEND_FAIL_WARN_QUIET);
            h.send_failures = h.send_failures.saturating_add(1);
            if retry != SendRetry::Retransmitted {
                h.send_failures_unretried = h.send_failures_unretried.saturating_add(1);
            }
            h.last_send_error_kind = Some(e.kind());
            h.last_send_error_at = Some(now);
            (loud, h.send_failures)
        };
        // The log is only the OPERATOR's copy; `NetHealth` is the AGENT's, and it is stamped
        // unconditionally above. Suppressing a log line therefore never suppresses a fact —
        // that separation is what makes rate-limiting safe here (#612).
        if loud {
            tracing::warn!(
                "NET: send failed ({:?}, {} bytes, retransmitted={:?}) — this datagram did \
                 NOT reach the wire; {} total since start (#612). Further failures logged at \
                 debug for the next {}s; see /v1/observe/debug send_failures",
                e.kind(), datagram.len(), retry, total, SEND_FAIL_WARN_QUIET.as_secs(),
            );
        } else {
            tracing::debug!(
                "NET: send failed ({:?}, {} bytes, retransmitted={:?}); {} total (#612)",
                e.kind(), datagram.len(), retry, total,
            );
        }
        Err(e)
    }

    /// Re-send session-layer control datagrams that a transient `WouldBlock` deferred (#641), oldest
    /// first, stopping at the first one that is still refused so ordering on the wire is preserved.
    ///
    /// Called at the top of `poll_recv` — which every loop that owns a stream calls each ~10ms tick,
    /// the same discipline `poll_resend` relies on — and again before any new control send.
    fn flush_pending_control(&mut self) {
        while let Some(front) = self.pending_control.front() {
            match self.transmit(front, SendRetry::Deferred) {
                Ok(()) => {}
                // Still refused. It must stay at the FRONT — popping and re-pushing would reorder it
                // behind anything queued in between — and we stop, so nothing overtakes it.
                Err(ref e) if is_transient(e) => return,
                // A PERMANENT error on a queued datagram (the socket died, say). Retrying it every
                // tick would never deliver it and would mint a fresh `send_failures` every 10ms
                // forever, so drop it: `record_send_failure` has already counted it as the loss it
                // is, exactly once.
                Err(_) => {
                    self.pending_control.pop_front();
                    continue;
                }
            }
            self.pending_control.pop_front();
        }
    }

    /// Queue a control datagram for retry on a later tick, and count it (#641).
    ///
    /// This is the ONE place `send_deferred` is incremented, and it is called exactly once per
    /// datagram that gets queued — from both of `send_raw`'s deferral paths (the socket refused it,
    /// and the queue was already non-empty). Counting in `record_send_failure` instead made the
    /// number grow with the DURATION of an outage rather than with the number of datagrams delayed:
    /// one stuck datagram re-attempted on three ticks read as `3` (#641 review, finding 1).
    fn defer_control(&mut self, datagram: Vec<u8>) {
        // Drop the OLDEST on overflow, not the newest: `OP_ACK` is cumulative, so a later ACK
        // supersedes an earlier one, and the freshest control datagram is the one worth keeping.
        // The evicted one IS a genuine loss — nothing will ever re-send it — so it is counted as one.
        let overflowed = self.pending_control.len() >= MAX_PENDING_CONTROL;
        if overflowed {
            self.pending_control.pop_front();
        }
        {
            // ONE lock for both counters — taking the guard twice in one expression deadlocks.
            let mut h = self.net_health.lock().unwrap();
            h.send_deferred = h.send_deferred.saturating_add(1);
            if overflowed {
                h.send_failures = h.send_failures.saturating_add(1);
                h.send_failures_unretried = h.send_failures_unretried.saturating_add(1);
            }
        }
        if overflowed {
            tracing::warn!(
                "NET: control-send retry queue full ({} datagrams) — dropped the oldest; the \
                 socket has been refusing sends for an unusually long time (#641)",
                MAX_PENDING_CONTROL,
            );
        }
        self.pending_control.push_back(datagram);
    }

    /// Session-layer control (SessionRequest / ACK / OutOfOrderAck / keepalive / disconnect).
    ///
    /// None of these is retained by the reliable resend window, and before #641 that meant a failed
    /// one was simply gone — measured live at 44–306 dropped ACKs per zone-in on a CPU-starved
    /// client. They are cheap and idempotent, so a TRANSIENT refusal (`EAGAIN`/`ENOBUFS`) now queues
    /// the datagram in `pending_control` for the next tick instead. `Ok(())` therefore means
    /// "accepted for delivery", which for a deferred datagram is a claim about the next ~10ms, not
    /// about this instant; `send_deferred` is what makes that visible. Any non-transient error is
    /// returned unchanged and counted as a loss.
    ///
    /// `send_session_disconnect` deliberately does NOT come through here — see its doc comment.
    fn send_raw(&mut self, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
        let mut raw = vec![0x00, opcode];
        raw.extend_from_slice(payload);
        raw = self.append_crc(raw);
        // Drain first so a queued datagram cannot end up on the wire AFTER one built later.
        self.flush_pending_control();
        if !self.pending_control.is_empty() {
            self.defer_control(raw);
            return Ok(());
        }
        match self.transmit(&raw, SendRetry::Deferred) {
            Err(e) if is_transient(&e) => {
                self.defer_control(raw);
                Ok(())
            }
            other => other,
        }
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

    /// DELIBERATE (#612): a failed ACK is counted by `transmit` (and lands in
    /// `send_failures_unretried` — we never re-send this datagram) but is not propagated. There is
    /// no honest error to hand the inbound-dispatch path here, and the recovery is the server's:
    /// an un-ACKed reliable is retransmitted by ITS resend window, which re-triggers this ACK.
    fn send_ack(&mut self, seq: u16) {
        let seq_bytes = seq.to_be_bytes();
        if let Err(e) = self.send_raw(OP_ACK, &self.encode(&seq_bytes.to_vec())) {
            tracing::debug!("NET: ACK for seq {} did not reach the wire ({:?}); the server will \
                             retransmit and we will re-ACK (#612)", seq, e.kind());
        }
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
        // DELIBERATE (#612): same as `send_ack` — counted, logged, not propagated. A lost
        // OutOfOrderAck only costs us the fast-retransmit hint; the server's timer-driven resend
        // still fills the gap.
        if let Err(e) = self.send_raw(OP_OUT_OF_ORDER, &self.encode(&seq_bytes.to_vec())) {
            tracing::debug!("NET: OutOfOrderAck for seq {} did not reach the wire ({:?}); the \
                             server's timer-driven resend still fills the gap (#612)", seq, e.kind());
        }
    }

    fn send_reliable(&mut self, app_data: &[u8]) -> std::io::Result<()> {
        // Fragments: EVERY fragment is still built, tracked and attempted even if an earlier one
        // failed — bailing early would leave a half-sent message in the resend window with no
        // remainder to complete it. The FIRST error is returned (`first_err`); the rest are counted
        // in `NetHealth` by `transmit` regardless.
        let mut first_err: Option<std::io::Error> = None;
        let note = |r: std::io::Result<()>, first_err: &mut Option<std::io::Error>| {
            if let Err(e) = r {
                if first_err.is_none() { *first_err = Some(e); }
            }
        };
        let max_inner = (self.session.max_packet_size as usize) - 5; // 2 proto + 1 compress + 2 crc
        if app_data.len() + 2 <= max_inner {
            let seq = self.next_send_seq();
            let mut inner = seq.to_be_bytes().to_vec();
            inner.extend_from_slice(app_data);
            let r = self.send_tracked(seq, OP_PACKET, &self.encode(&inner));
            note(r, &mut first_err);
        } else {
            // Fragment
            let seq = self.next_send_seq();
            let total_size = app_data.len() as u32;
            let first_max = max_inner - 2 - 4; // seq + total_size overhead
            let mut inner = seq.to_be_bytes().to_vec();
            inner.extend_from_slice(&total_size.to_be_bytes());
            inner.extend_from_slice(&app_data[..first_max]);
            let r = self.send_tracked(seq, OP_FRAGMENT, &self.encode(&inner));
            note(r, &mut first_err);

            let mut offset = first_max;
            while offset < app_data.len() {
                let seq = self.next_send_seq();
                let end = (offset + max_inner - 2).min(app_data.len());
                let mut inner = seq.to_be_bytes().to_vec();
                inner.extend_from_slice(&app_data[offset..end]);
                let r = self.send_tracked(seq, OP_FRAGMENT, &self.encode(&inner));
                note(r, &mut first_err);
                offset = end;
            }
        }
        match first_err { Some(e) => Err(e), None => Ok(()) }
    }

    /// Build, RECORD, and send a reliable protocol datagram (OP_Packet / OP_Fragment). Frames exactly
    /// like `send_raw` but retains the final wire bytes in the resend window so the datagram can be
    /// retransmitted VERBATIM (same `seq`) on OP_OutOfOrder / timeout until the server ACKs it (#254).
    fn send_tracked(&mut self, seq: u16, opcode: u8, encoded_inner: &[u8]) -> std::io::Result<()> {
        let mut raw = vec![0x00, opcode];
        raw.extend_from_slice(encoded_inner);
        let datagram = self.append_crc(raw);
        let outcome = self.transmit(&datagram, SendRetry::Retransmitted);
        // Pushed to the resend window on FAILURE too — deliberately, and this is the reason the
        // reliable path can honestly report a failed send without it meaning "lost" (#612): a
        // datagram that never reached the socket is in exactly the same position as one lost in
        // flight, and `poll_resend`'s go-back-N re-sends it verbatim until the server ACKs it.
        self.sent.push_back(Sent { seq, datagram, sent_at: Instant::now(), retries: 0 });
        outcome
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
            // DELIBERATE (#612): session-layer replies to the server's own probes. Counted +
            // WARNed by `transmit`; nothing above this dispatch can act on the error, and the
            // server re-probes on its own cadence.
            OP_KEEPALIVE => { let _ = self.send_raw(OP_KEEPALIVE, &[]); }
            OP_STAT_REQUEST => { let _ = self.send_raw(OP_STAT_RESPONSE, payload); }
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
/// #612 (review F1): account for reliable datagrams abandoned when a session ends.
///
/// `send_failures_unretried` excludes the reliable stream because `poll_resend` re-sends a failed
/// reliable datagram until the server ACKs it — but that is only true WHILE THE SESSION LIVES. The
/// server drops the session at its ~30s `resend_timeout`, and the reconnect at `gameplay.rs`'s
/// `EqStream::connect` builds a fresh stream whose `sent` window starts EMPTY. Everything still
/// outstanding at that instant is genuinely gone.
///
/// Doing this in `Drop` rather than at each teardown call site is deliberate: several paths end a
/// session (zone handoff, world reconnect, zone-in failure), and the #343 review's lesson was that
/// "every loop remembers to mirror it" is exactly the discipline that fails. Whoever drops the
/// stream owns the accounting, so a future path gets it for free.
///
/// **`Drop` is not sufficient on its own, and the round-2 review of #636 measured exactly where
/// (R1). Two session-ending paths do not drop the stream:**
///   - **Clean shutdown** — `perform_clean_shutdown` borrows the stream and returns, then its caller
///     parks in `loop { sleep }` while the process exits from the MAIN thread; a parked tokio task
///     is never unwound, so no destructor runs. That path therefore calls `abandon_outstanding`
///     EXPLICITLY, and `clean_shutdown_accounts_its_outstanding_window_explicitly_not_via_drop`
///     pins the call.
///   - **A server-side session drop (the ~30s `resend_timeout` case) — STILL NOT COVERED.** The
///     client never notices one: inbound `OP_SessionDisconnect` is unhandled, `poll_recv`'s
///     closed-socket return is discarded at every call site, and the gameplay loop has no
///     link-staleness exit, so the stream is never torn down and this counter stays 0 for exactly
///     those datagrams. That is #642, deliberately out of scope for #612 — do NOT write docs that
///     imply this case is covered. `connected: false` (15s, before the server's 30s drop) is the
///     honest signal for it.
impl Drop for EqStream {
    fn drop(&mut self) {
        self.abandon_outstanding();
    }
}

impl EqStream {
    /// Account the outstanding resend window as abandoned and CLEAR it (#612 review F1/R1).
    ///
    /// Called from `Drop` (covers every path that ends a session by dropping the stream) and
    /// explicitly from `perform_clean_shutdown`, whose task parks forever after returning so its
    /// destructors never run — a real gap the round-2 review measured rather than assumed.
    ///
    /// Clearing the window is what makes it safe to call from both: a second call (the eventual
    /// `Drop`, if it ever runs) sees an empty window and does nothing, so no double count.
    pub(crate) fn abandon_outstanding(&mut self) {
        // #641: control datagrams still queued for retry when the session ends are never sent — the
        // next stream is a different socket. Same honesty rule as the reliable window below: a
        // datagram we stop trying to deliver is a loss and must be counted as one.
        let pending = self.pending_control.len() as u64;
        if pending > 0 {
            self.pending_control.clear();
            if let Ok(mut h) = self.net_health.lock() {
                h.send_failures = h.send_failures.saturating_add(pending);
                h.send_failures_unretried = h.send_failures_unretried.saturating_add(pending);
            }
            tracing::warn!(
                "NET: session ended with {} session-control datagram(s) still queued for retry — \
                 these are NOT sent (#641)",
                pending,
            );
        }
        if self.sent.is_empty() {
            return;
        }
        let abandoned = self.sent.len() as u64;
        let oldest = self.sent[0].seq;
        self.sent.clear();
        // `Drop` can run while a panic is unwinding, and a poisoned mutex must not turn a bad
        // situation into a double panic — so this is the one place that tolerates a failed lock.
        if let Ok(mut h) = self.net_health.lock() {
            h.reliable_abandoned = h.reliable_abandoned.saturating_add(abandoned);
        }
        tracing::warn!(
            "NET: session ended with {} un-ACKed reliable datagram(s) still outstanding (oldest \
             seq={}) — the next session's resend window starts empty, so these are NOT retransmitted \
             (#612); see /v1/observe/debug reliable_abandoned",
            abandoned, oldest,
        );
    }
}

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
        pending_control: VecDeque::new(),
        force_send_refusals: std::cell::Cell::new(0),
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
        pending_control: VecDeque::new(),
        force_send_refusals: std::cell::Cell::new(0),
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
        pending_control: VecDeque::new(),
        force_send_refusals: std::cell::Cell::new(0),
        frags: FragmentBuffer::new(),
        net_health,
        app_tx: tx,
    };
    (stream, rx, peer_sock, stream_addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip whole-line `//` comments from Rust source before a source-level guard searches it
    /// (#612 round-3 review, B1).
    ///
    /// Every guard in this file that asserts "this call exists" or "this call appears exactly N
    /// times" MUST run its source through here first. Without it a guard is satisfied by a mention
    /// in a comment — and, worse, `contains("foo()")` on raw source is defeated by simply COMMENTING
    /// OUT the call, which is the realistic way such a call regresses (a review mutation did exactly
    /// that and the suite stayed green). Whether a nearby doc comment happens to match is then pure
    /// accident, which is not a property a guard may depend on.
    ///
    /// **KNOWN LIMIT (#612 round-4 review, C3): this handles whole-line `//` comments ONLY. A
    /// `/* … */` block comment around a guarded call still defeats every guard built on it** — the
    /// reviewer probed exactly that and the suite stayed green. Deliberately not fixed here: a
    /// span-stripper that does not understand string literals could over-strip and produce a FALSE
    /// RED across the eight modules the `try_send` guard scans, several of which contain `/*` tokens
    /// inside code and prose. A guard that occasionally fails for the wrong reason is worse than one
    /// with a documented hole, so the hole is documented and the assert messages are worded to claim
    /// only what is actually checked. If you close it, do it with a real lexer, not a regex.
    fn strip_comments(src: &str) -> String {
        src.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// #603 review follow-up (F2): the `writable().await` line above `stream.send_session_request()`
    /// in `connect` is load-bearing — remove it and the pre-loop send goes back to being a
    /// nondeterministic WouldBlock race (deterministic drop on a current_thread runtime; a majority
    /// but not universal drop rate even on production's multi_thread runtime — see the doc comment on
    /// that call site for the range across independent measurements) —
    /// but nothing about the code *looks* wrong without it, and no timing-based test can safely pin
    /// this without becoming exactly the runtime/scheduler-coupled flaky-test class #549 already had
    /// to remove once (virtual time auto-advances past real socket I/O; real-time is contention-flaky
    /// in the other direction). Source-level assertion instead, mirroring the precedent in
    /// `crates/eqoxide-renderer/tests/weather_shader.rs` and `fog_shader.rs` (both assert on
    /// `include_str!`-embedded source): fails loudly, by name, the instant the awaited call is deleted
    /// or reordered to after the send it's supposed to guard.
    #[test]
    fn connect_awaits_writable_before_the_first_session_request() {
        const TRANSPORT_RS_SRC: &str = include_str!("transport.rs");
        // Isolate the statement immediately preceding the pre-loop `send_session_request()` call, and
        // check it awaits `writable()` (bounded by a timeout, #603 F3) rather than matching the whole
        // statement verbatim — robust to changing the timeout duration or wrapper without weakening
        // what's actually being pinned: that a writability wait happens right before this specific send.
        // No trailing `;`: since #612 this call is wrapped in `if let Err(e) = …` so its send
        // outcome is observed rather than discarded. The first occurrence in the file is still the
        // pre-loop call site this test pins.
        let needle = "stream.send_session_request()";
        let call_site = TRANSPORT_RS_SRC
            .find(needle)
            .unwrap_or_else(|| panic!("transport.rs: couldn't find `{needle}`"));
        let preceding_statement = TRANSPORT_RS_SRC[..call_site]
            .rsplit(';')
            .nth(1)
            .unwrap_or_else(|| panic!("transport.rs: couldn't find the statement before `{needle}`"));
        assert!(
            preceding_statement.contains("stream.socket.writable()")
                && preceding_statement.contains(".await"),
            "connect() must await `stream.socket.writable()` immediately before the pre-loop \
             `stream.send_session_request()` call (#603) — without it, the very first send on a \
             freshly connect()ed UDP socket is a WouldBlock race that can silently miss the wire \
             (see the doc comment on that call site for the measured drop rates), quietly pushing all \
             the real work onto the retry loop instead of the fast-path send this line exists for \
             (found preceding statement: {preceding_statement:?})"
        );
    }

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
    /// (nominal t≈0, 250, 500 — measured effective arrivals cluster more like 0, 303, 607 per
    /// `SESSION_REQUEST_RETRY`'s doc comment, still inside the window); a wiring bug firing every
    /// ~100ms sees ~7. Do NOT tighten this bound "for precision" — the slack between 3 and 7 is what
    /// makes it contention-proof; a tighter ceiling would reintroduce exactly the flake #549 fixed.
    ///
    /// #603 review follow-up: the pre-loop send now awaits `writable()` before firing, so the nominal
    /// count is exactly 3 rather than the ~2 it could land on before (when the first send was a
    /// coin-flip that sometimes silently missed the wire). Detection power only improved —
    /// a wiring bug that degrades the cadence to ~200ms produced 3 sends in this window before (passing,
    /// indistinguishable from correct) and produces 4 now (failing).
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

    // ── #612: outbound send failures must be OBSERVABLE, not discarded ────────────────────────

    /// #612 — THE test. A real `EqStream`, a real socket, a real kernel send failure, traced all the
    /// way to the JSON an agent polls at `GET /v1/observe/debug`.
    ///
    /// The bug: `send_app_packet_unreliable` ended in `let _ = self.socket.try_send(&datagram)`, so a
    /// datagram that never left the machine was indistinguishable — to this code, and therefore to
    /// the agent driving it — from one the server received. The unreliable path is the sharpest case
    /// because nothing retransmits it: when that send fails the update is simply gone.
    ///
    /// Deliberately NOT asserted through a pure helper: the failure is forced on the SAME socket the
    /// production path sends on, and the observable is the real `/debug` body produced by the real
    /// `observe` router over the same `NetHealthShared` `Arc` the net thread stamps. Two recent fixes
    /// passed unit tests while broken because the test exercised a pure function instead of the real
    /// path; this test's only fixture is the peer socket it checks for absence.
    ///
    /// The forced error is `EMSGSIZE`: a UDP payload above the 65507-byte IPv4 maximum. Real errno
    /// from a real `sendto`, deterministic, no timing. `writable().await` first so tokio's cached
    /// readiness is warm and `try_send` actually performs the syscall rather than returning the
    /// synthetic `WouldBlock` documented on `connect()` — either error would be counted, but pinning
    /// which one keeps the assertions exact.
    #[tokio::test]
    async fn send_failure_is_visible_to_the_agent_in_observe_debug() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        let (mut stream, _rx, peer, _addr) = test_stream_with_peer(net_health.clone()).await;
        stream.socket.writable().await.unwrap();

        // CONTROL first: a send that SUCCEEDS must reach the peer and must NOT move any counter.
        // Without this the test would also pass if `transmit` counted every send, failure or not.
        stream.send_app_packet_unreliable(0x1234, &[0xAA, 0xBB]).expect("small send must succeed");
        {
            let h = *net_health.lock().unwrap();
            assert_eq!((h.send_failures, h.send_failures_unretried), (0, 0),
                "a SUCCESSFUL send must not be counted as a failure (#612)");
            assert!(h.last_send_error_kind.is_none());
        }
        let mut buf = vec![0u8; 70_000];
        let n = tokio::time::timeout(std::time::Duration::from_millis(500), peer.recv(&mut buf))
            .await.expect("the control datagram must actually reach the peer").unwrap();
        assert!(n > 0);

        // Now the failure: 70_000 bytes is past the 65507-byte IPv4 UDP payload ceiling → EMSGSIZE.
        let payload = vec![0x5Au8; 70_000];
        let err = stream.send_app_packet_unreliable(0x1234, &payload)
            .expect_err("an oversized datagram cannot be sent — the send path must SAY so (#612)");
        assert_eq!(err.raw_os_error(), Some(90),
            "expected EMSGSIZE (errno 90) from the real sendto; got {err:?}");

        // WIRE-LEVEL truth, not a proxy: nothing arrived. This is the fact the old `let _ =` erased.
        let mut buf = vec![0u8; 70_000];
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), peer.recv(&mut buf))
                .await.is_err(),
            "the failed datagram must NOT be on the wire — if it arrived, this test is measuring \
             the wrong thing",
        );

        // Internal ledger: counted once, and counted as UNRETRIED (nothing re-sends an unreliable).
        {
            let h = *net_health.lock().unwrap();
            assert_eq!(h.send_failures, 1, "the failed send must be counted (#612)");
            assert_eq!(h.send_failures_unretried, 1,
                "an unreliable send has no retransmit path, so it must count as unretried (#612)");
            assert_eq!(h.last_send_error_kind, Some(err.kind()));
        }

        // …and, the part that actually makes this an agent-honesty fix: it must reach the agent.
        // A fix that publishes an honest state only INTERNALLY has not fixed anything.
        let state = eqoxide_http::testkit::empty_state_with_net_health(net_health.clone());
        let json = eqoxide_http::testkit::debug_json(state).await;
        // Read through `serde_json::Value`'s accessors (no `serde_json` dependency needed in this
        // crate — `debug_json` hands the value back already decoded).
        let p = &json["player"];
        assert_eq!(p["send_failures"].as_u64(), Some(1),
            "GET /v1/observe/debug must report the send failure — this is the ONLY channel the \
             driving agent has (#612). Body: {json}");
        assert_eq!(p["send_failures_unretried"].as_u64(), Some(1));
        assert_eq!(p["last_send_error"].as_str(), Some(format!("{:?}", err.kind()).as_str()),
            "last_send_error must be the ErrorKind of the failure that actually happened");
        let age = p["last_send_error_age_ms"].as_u64().expect("age must be present, not null");
        assert!(age < 60_000, "the age is measured at read time, so it must be small here (got {age})");
    }

    // ── #641: a SYNTHETIC WouldBlock must not silently swallow an ACK ─────────────────────────

    /// #641 — THE test. An ACK that tokio refuses with a synthetic `WouldBlock` must still reach
    /// the wire.
    ///
    /// The bug, measured live: a healthy `qeynos` zone-in accrued a burst of `send_failures`, every
    /// one a 7-byte session-layer control datagram (an ACK) and every one `WouldBlock`. Those ACKs
    /// never left the machine, so the server kept retransmitting datagrams it had not seen
    /// acknowledged. Reproduced on demand by pinning the whole client to a single core
    /// (`taskset -c 0`): 188 failures on `main`, 0 with this fix.
    ///
    /// The mechanism this test pins: tokio gates every `try_*` call on a cached readiness bit and
    /// returns `WouldBlock` **without attempting the syscall** while that bit is empty. It is empty
    /// on a socket whose writability no task has ever observed — which is exactly the state
    /// `test_stream_with_peer` leaves, and which on `#[tokio::test]`'s `current_thread` runtime is
    /// deterministic (no worker thread exists to race the readiness event; see the long note on
    /// `connect()`). Live, CPU starvation of the io driver produces the same empty bit mid-session.
    ///
    /// **The assertion that matters is the WIRE, not a counter**: the peer socket must actually
    /// receive the ACK. On `main` (no rescue) nothing arrives and this `recv` times out.
    ///
    /// `send_wouldblock_rescued == 1` is asserted too, and it is not decoration — it is the
    /// FIXTURE CHECK. If a future tokio/runtime change made the readiness bit warm here, `try_send`
    /// would succeed on its own, the ACK would arrive, and the wire assertion would pass while
    /// testing nothing. That assertion fails in exactly that case, so the test cannot quietly stop
    /// exercising the path it exists for.
    #[tokio::test]
    async fn a_synthetically_blocked_ack_still_reaches_the_wire() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        let (mut stream, _rx, peer, _addr) = test_stream_with_peer(net_health.clone()).await;
        // DELIBERATELY no `stream.socket.writable().await` here (unlike the #612 tests above, which
        // want a warm cache so a REAL errno surfaces). Cold cache == synthetic WouldBlock.

        stream.send_ack(0x0007);

        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(std::time::Duration::from_millis(500), peer.recv(&mut buf))
            .await
            .expect(
                "the ACK must be on the wire. A `try_send` WouldBlock that tokio synthesised from \
                 an empty readiness cache means no syscall was even attempted — dropping the ACK \
                 there is #641, and the server then retransmits everything it saw unacknowledged",
            )
            .unwrap();
        assert!(n > 0, "an empty datagram is not an ACK");

        let h = *net_health.lock().unwrap();
        assert_eq!(h.send_wouldblock_rescued, 1,
            "FIXTURE CHECK: this test is only meaningful if `try_send` actually returned a \
             synthetic WouldBlock and the direct send(2) retry rescued it. 0 here means the \
             readiness cache was warm and the wire assertion above proved nothing (#641)");
        assert_eq!(h.send_failures, 0,
            "a datagram that DID reach the wire must not be counted as never having reached it — \
             that would be the #612 honesty bug pointing the other way (#641)");
        assert_eq!(h.send_failures_unretried, 0);
        assert!(h.last_send_error_kind.is_none(),
            "no send failed, so there is no last send error to report (#641)");
    }

    /// #641 — and it must reach the AGENT, through the real `/v1/observe/debug` body, not just an
    /// internal struct. Same standard as `send_failure_is_visible_to_the_agent_in_observe_debug`.
    ///
    /// This also pins the honesty split that #641's issue text asked for: a rescued datagram is
    /// reported as rescued, and is absent from `send_failures` / `send_failures_unretried`. An
    /// agent polling those two fields therefore sees loss, and only loss.
    #[tokio::test]
    async fn a_rescued_send_is_reported_as_rescued_and_not_as_a_loss_in_observe_debug() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        let (mut stream, _rx, _peer, _addr) = test_stream_with_peer(net_health.clone()).await;
        stream.send_ack(0x0007);

        let state = eqoxide_http::testkit::empty_state_with_net_health(net_health.clone());
        let json = eqoxide_http::testkit::debug_json(state).await;
        let p = &json["player"];
        assert_eq!(p["send_wouldblock_rescued"].as_u64(), Some(1),
            "GET /v1/observe/debug must publish the rescue — a client whose io driver is starved \
             enough to need it is a fact the agent is entitled to (#641). Body: {json}");
        assert_eq!(p["send_failures"].as_u64(), Some(0),
            "a rescued datagram reached the wire; reporting it as a failure would be a lie (#641)");
        assert_eq!(p["send_failures_unretried"].as_u64(), Some(0));
        assert!(p["last_send_error"].is_null(),
            "nothing failed, so there is no last send error (#641). Body: {json}");
    }

    /// #641, the OTHER half — the half the raw-`send(2)` rescue does NOT cover.
    ///
    /// Live measurement refuted "it's all synthetic": one instrumented `qeynos` zone-in recorded
    /// **141 rescued** (synthetic — the kernel took them on the direct retry) alongside **107 the
    /// kernel genuinely refused**. Nothing in the rescue helps the second group; those ACKs were
    /// still going on the floor. So a transiently-refused control datagram is now queued and re-sent
    /// on the next tick.
    ///
    /// The refusal is injected (`force_send_refusals`) because there is no portable, deterministic
    /// way to make a real `send(2)` on a connected UDP socket return `EAGAIN` from a unit test —
    /// loopback drains instantly, and `tc`/blackhole routes need root. The injection replaces ONLY
    /// the socket outcome; everything after it — the classification, the queue, `poll_recv`'s drain,
    /// the bytes on the wire — is the production path.
    ///
    /// Asserted on the WIRE: the peer must receive the ACK on the following tick. On `main` (no
    /// queue) it is dropped and this `recv` times out.
    #[tokio::test]
    async fn an_ack_the_kernel_refuses_is_retried_on_the_next_tick_not_dropped() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        let (mut stream, _rx, peer, _addr) = test_stream_with_peer(net_health.clone()).await;
        stream.socket.writable().await.unwrap(); // warm, so ONLY the injected refusal is in play
        stream.force_send_refusals.set(1);

        stream.send_ack(0x0011);

        // Nothing on the wire yet — this is the moment `main` loses the ACK forever.
        let mut buf = [0u8; 64];
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), peer.recv(&mut buf))
                .await.is_err(),
            "the injected refusal must actually have stopped this send; if the ACK is already on \
             the wire the rest of this test proves nothing (#641 fixture check)",
        );
        {
            let h = *net_health.lock().unwrap();
            assert_eq!(h.send_deferred, 1,
                "a transiently-refused control datagram must be recorded as DEFERRED (#641)");
            assert_eq!((h.send_failures, h.send_failures_unretried), (0, 0),
                "it is queued, not lost — counting it as a failure would republish the very number \
                 #641 exists to drive to zero (#641)");
        }

        // The next tick: `poll_recv` drains the queue, and the ACK reaches the server after all.
        stream.poll_recv();
        let n = tokio::time::timeout(std::time::Duration::from_millis(500), peer.recv(&mut buf))
            .await
            .expect(
                "the deferred ACK must go out on the next tick. Dropping it is #641: the server \
                 retransmits everything it has not seen acknowledged, and gives up at its ~30s \
                 resend_timeout",
            )
            .unwrap();
        assert!(n > 0);
        assert!(stream.pending_control.is_empty(), "a delivered datagram must leave the queue");
        let h = *net_health.lock().unwrap();
        assert_eq!((h.send_failures, h.send_failures_unretried), (0, 0),
            "the datagram was delivered; nothing here is a loss (#641)");
    }

    /// #641 review, finding 1 — `send_deferred` counts DATAGRAMS, not refusal events.
    ///
    /// The first cut incremented it in `record_send_failure`, which runs once per refusal —
    /// including every ~10ms re-attempt of the same datagram from `flush_pending_control`. One
    /// datagram stuck for three ticks therefore read as `3`, while the queue-nonempty path in
    /// `send_raw` (which defers without ever calling `transmit`) counted **nothing**. The number was
    /// neither an upper nor a lower bound on datagrams, and grew with the DURATION of an outage —
    /// yet all three doc sites said "each of these datagrams did go out". Under the honesty
    /// invariant that is the counter telling the agent something it cannot know.
    ///
    /// The reviewer's mutation G — moving the increment to `defer_control` — left the whole suite
    /// GREEN, i.e. two incompatible definitions were indistinguishable to the tests. This test is
    /// what makes them distinguishable, so it asserts BOTH halves of the discrimination:
    ///   - one datagram refused on three separate ticks is `1`, not `3`;
    ///   - three datagrams are `3`, so the counter is not simply pinned low.
    #[tokio::test]
    async fn send_deferred_counts_datagrams_not_refusal_events() {
        // One datagram, refused again on each of two further ticks.
        {
            let net_health: eqoxide_ipc::NetHealthShared = Default::default();
            let (mut stream, _rx, _peer, _addr) = test_stream_with_peer(net_health.clone()).await;
            stream.socket.writable().await.unwrap();
            stream.force_send_refusals.set(3); // the send, then the two flush re-attempts
            stream.send_ack(0x0001);
            stream.poll_recv();
            stream.poll_recv();
            assert_eq!(stream.pending_control.len(), 1,
                "the datagram must still be queued — otherwise this is not measuring re-attempts");
            assert_eq!(net_health.lock().unwrap().send_deferred, 1,
                "ONE datagram was delayed, however many ticks it took. Counting refusal events \
                 makes this grow with the duration of an outage and contradicts every doc site \
                 (#641 review, finding 1)");
        }
        // Three distinct datagrams.
        {
            let net_health: eqoxide_ipc::NetHealthShared = Default::default();
            let (mut stream, _rx, _peer, _addr) = test_stream_with_peer(net_health.clone()).await;
            stream.socket.writable().await.unwrap();
            stream.force_send_refusals.set(3);
            stream.send_ack(0x0001);
            stream.send_ack(0x0002); // deferred via the queue-nonempty short-circuit…
            stream.send_ack(0x0003); // …which used to count nothing at all
            assert_eq!(stream.pending_control.len(), 3);
            assert_eq!(net_health.lock().unwrap().send_deferred, 3,
                "every datagram that gets queued must be counted, including the ones deferred by \
                 the queue-nonempty short-circuit in `send_raw` (#641 review, finding 1)");
        }
    }

    /// #641 review, finding 2 — session teardown must NEVER be deferred.
    ///
    /// `OP_SessionDisconnect` is the last datagram a session ever sends, and for it "the next tick"
    /// does not exist: `perform_clean_shutdown` clears the queue immediately afterwards, and the
    /// `OP_GMKick` path then parks in `loop { sleep }` forever — never calling `poll_recv`, never
    /// unwound, so `Drop` never runs either. Deferring it there produced the precise failure this
    /// PR exists to prevent: a datagram that is never sent AND never counted as lost, sitting in
    /// `send_deferred`, whose documented meaning is "it went out, late". Pre-#641 `main` counted it
    /// as a failure, so that was an honesty REGRESSION against main.
    #[tokio::test]
    async fn a_refused_session_disconnect_is_counted_as_lost_never_queued() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        let (mut stream, _rx, _peer, _addr) = test_stream_with_peer(net_health.clone()).await;
        stream.socket.writable().await.unwrap();
        stream.force_send_refusals.set(1);

        let err = stream.send_session_disconnect()
            .expect_err("a refused disconnect must SAY it failed — there is no later tick to fix it");
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);

        assert!(stream.pending_control.is_empty(),
            "the disconnect must NOT be queued: nothing after this point ever drains the queue \
             (#641 review, finding 2)");
        let h = *net_health.lock().unwrap();
        assert_eq!((h.send_failures, h.send_failures_unretried), (1, 1),
            "a datagram nothing will re-send is a LOSS and must be counted as one — pre-#641 main \
             counted it, so anything less is a regression (#641 review, finding 2)");
        assert_eq!(h.send_deferred, 0,
            "`send_deferred` means the datagram was queued for a later tick; claiming that for a \
             datagram nobody will ever re-send is exactly the lie #641 is about");
    }

    /// #641 review, finding 7 — `ENOBUFS` is as transient as `EAGAIN` on UDP under this pressure
    /// (a momentarily full device/qdisc transmit queue), so it must be deferrable too. It maps to
    /// `ErrorKind::Uncategorized`, which is unstable to match on, so `is_transient` identifies it by
    /// errno — this pins that, and that a genuinely permanent error is NOT swept into the queue.
    #[test]
    fn enobufs_is_transient_but_a_permanent_error_is_not() {
        use std::io::{Error, ErrorKind};
        assert!(is_transient(&Error::from(ErrorKind::WouldBlock)));
        #[cfg(target_os = "linux")]
        assert!(is_transient(&Error::from_raw_os_error(105)),
            "ENOBUFS is a momentarily full transmit queue — the very pressure #641 is about");
        assert!(!is_transient(&Error::from_raw_os_error(90)),
            "EMSGSIZE can never succeed on retry; queueing it forever would hide a permanent loss \
             behind a 'will be retried' counter (#641 review, finding 7)");
        assert!(!is_transient(&Error::from(ErrorKind::BrokenPipe)));
    }

    /// #641 — ordering. The control path is a stream of sequence-bearing datagrams, so a queued one
    /// must never land on the wire AFTER a datagram built later. Both `send_raw` (which drains
    /// before every new send) and `flush_pending_control` (which stops at the first still-refused
    /// entry instead of skipping past it) exist for this.
    #[tokio::test]
    async fn deferred_control_datagrams_are_re_sent_in_the_order_they_were_built() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        let (mut stream, _rx, peer, _addr) = test_stream_with_peer(net_health.clone()).await;
        stream.socket.writable().await.unwrap();

        // Three refusals: one per `send_ack` (each drains the queue first, and that drain is itself
        // a refused send), so all three end up queued instead of trickling out mid-test.
        stream.force_send_refusals.set(3);
        stream.send_ack(0x0001);
        stream.send_ack(0x0002);
        stream.send_ack(0x0003);
        assert_eq!(stream.pending_control.len(), 3,
            "the third must queue BEHIND the first two, not overtake them (#641)");

        stream.poll_recv();
        assert!(stream.pending_control.is_empty());

        let mut seen = Vec::new();
        for _ in 0..3 {
            let mut buf = [0u8; 64];
            let n = tokio::time::timeout(std::time::Duration::from_millis(500), peer.recv(&mut buf))
                .await.expect("all three deferred ACKs must arrive (#641)").unwrap();
            seen.push(buf[..n].to_vec());
        }
        let expect: Vec<Vec<u8>> = [0x0001u16, 0x0002, 0x0003].iter()
            .map(|seq| {
                let mut raw = vec![0x00, OP_ACK];
                raw.extend_from_slice(&stream.encode(&seq.to_be_bytes().to_vec()));
                stream.append_crc(raw)
            })
            .collect();
        assert_eq!(seen, expect,
            "deferred control datagrams must reach the wire in the order they were built (#641)");
    }

    /// #641 — the queue is bounded, and overflow is honest.
    ///
    /// A socket refusing sends for long enough to overflow 1024 queued control datagrams is a real
    /// (if extreme) state, and the datagram dropped there is genuinely lost — so it must land in
    /// `send_failures`/`send_failures_unretried`, the counters that mean exactly that. The OLDEST is
    /// dropped because `OP_ACK` is cumulative: the newest supersedes it.
    #[tokio::test]
    async fn the_control_retry_queue_is_bounded_and_a_dropped_datagram_is_counted_as_lost() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        let (mut stream, _rx, _peer, _addr) = test_stream_with_peer(net_health.clone()).await;

        for i in 0..MAX_PENDING_CONTROL {
            stream.defer_control(vec![i as u8]);
        }
        assert_eq!(stream.pending_control.len(), MAX_PENDING_CONTROL);
        assert_eq!(net_health.lock().unwrap().send_failures, 0,
            "filling the queue to capacity loses nothing (#641)");

        stream.defer_control(vec![0xFF]);
        assert_eq!(stream.pending_control.len(), MAX_PENDING_CONTROL,
            "the queue must stay bounded (#641)");
        assert_eq!(stream.pending_control.front(), Some(&vec![1u8]),
            "the OLDEST entry is the one dropped — a later ACK supersedes an earlier one (#641)");
        assert_eq!(stream.pending_control.back(), Some(&vec![0xFFu8]),
            "the newest entry must be kept (#641)");
        let h = *net_health.lock().unwrap();
        assert_eq!((h.send_failures, h.send_failures_unretried), (1, 1),
            "a datagram dropped on overflow is never sent by anything — it is a loss and must be \
             counted as one, not hidden in send_deferred (#641)");
    }

    /// #641 — a session that ends with control datagrams still queued has LOST them: the next
    /// session is a different socket and its queue starts empty. Same reasoning, and the same
    /// counter, as `reliable_abandoned`'s (#612 F1) — a datagram we stop trying to deliver must not
    /// be left sitting in a "will be retried" counter that has quietly stopped being true.
    #[tokio::test]
    async fn control_datagrams_still_queued_when_the_session_ends_are_counted_as_lost() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        {
            let (mut stream, _rx, _peer, _addr) = test_stream_with_peer(net_health.clone()).await;
            stream.socket.writable().await.unwrap();
            stream.force_send_refusals.set(3);
            stream.send_ack(0x0001);
            stream.send_ack(0x0002);
            stream.send_ack(0x0003);
            assert_eq!(stream.pending_control.len(), 3);
            assert_eq!(net_health.lock().unwrap().send_deferred, 3);
        } // stream dropped — the session ends here

        let h = *net_health.lock().unwrap();
        assert_eq!((h.send_failures, h.send_failures_unretried), (3, 3),
            "queued-but-never-sent control datagrams must be reported as the loss they are once \
             the session that would have retried them is gone (#641)");
    }

    /// #641 structural guard, in the same spirit as
    /// `there_is_exactly_one_socket_send_call_in_the_crate_and_it_records_failures`.
    ///
    /// The rescue reconstructs a `std::net::UdpSocket` from a borrowed fd. That is sound in exactly
    /// one shape — wrapped in `ManuallyDrop`, so the descriptor tokio still owns is not closed by
    /// the temporary's destructor. A second, unwrapped `from_raw_fd` added anywhere in this crate
    /// would be a use-after-close waiting to happen, so pin that there is one and that it is
    /// `ManuallyDrop`-wrapped.
    #[test]
    fn the_only_raw_fd_socket_in_the_crate_is_the_manually_dropped_send_rescue() {
        // Needle split so this test's own source does not match it.
        let needle = concat!("UdpSocket::from_raw", "_fd(");
        let code = strip_comments(include_str!("transport.rs"));
        assert_eq!(code.matches(needle).count(), 1,
            "exactly one raw-fd socket reconstruction is expected in this crate (#641's send \
             rescue); a second one is a double-close hazard");
        let at = code.find(needle).unwrap();
        let before = &code[..at];
        assert!(before.ends_with("std::mem::ManuallyDrop::new(unsafe { std::net::"),
            "the reconstructed socket MUST be wrapped in `ManuallyDrop` so dropping it does not \
             close the descriptor tokio still owns (#641). Found: {:?}",
            &before[before.len().saturating_sub(80)..]);
        let fn_start = before.rfind("\nfn ").expect("must live inside a free function");
        assert!(before[fn_start..].contains("fn raw_send_bypassing_readiness_cache("),
            "the raw send must stay confined to the rescue helper (#641)");
    }

    /// #612 companion — the RELIABLE path's deliberate call-site decision, verified rather than
    /// assumed. A failed reliable send is counted, but it is NOT counted as `unretried`, because
    /// `send_tracked` pushes the datagram into the resend window even on failure and `poll_resend`
    /// re-sends it verbatim until the server ACKs it — **while the session lives**. That is why
    /// `send_app_packet` does not propagate an error to its ~90 call sites: within a session the
    /// packet is delayed, not lost, and reporting "failed" would be a lie in the other direction.
    /// Across a session end that guarantee stops (round-2 review R5 caught this one docstring still
    /// stating it unconditionally); `reliable_abandoned` is the counter for that case, and
    /// `reliables_outstanding_when_a_session_ends_are_counted_as_abandoned` is its test.
    ///
    /// If this invariant ever breaks (a future edit pushing to `sent` only on success), the drop
    /// would become silent again — this test is what stops that.
    #[tokio::test]
    async fn failed_reliable_send_is_counted_but_retained_for_retransmission() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        let (mut stream, _rx, _peer, _addr) = test_stream_with_peer(net_health.clone()).await;
        stream.socket.writable().await.unwrap();
        // Raise the negotiated MTU so the packet below is sent as ONE oversized datagram instead of
        // being fragmented into sendable pieces — this is how the reliable path is forced to fail.
        stream.session.max_packet_size = u16::MAX;

        // 65_520-byte payload → one 65_526-byte datagram (2 proto + 2 seq + 2 opcode + payload,
        // crc_bytes = 0), just past the 65_507-byte IPv4 UDP payload ceiling → EMSGSIZE. Below
        // `max_inner` (65_530) so `send_reliable` does NOT fragment it into sendable pieces.
        stream.send_app_packet(0x1234, &vec![0x5Au8; 65_520]);

        let h = *net_health.lock().unwrap();
        assert_eq!(h.send_failures, 1, "a failed reliable send must be counted too (#612)");
        assert_eq!(h.send_failures_unretried, 0,
            "the reliable path retransmits this exact datagram, so it is NOT an unretried loss");
        assert_eq!(stream.sent.len(), 1,
            "the datagram must be retained in the resend window even though its first send failed \
             — that retention is what makes 'counted but not lost' true (#612)");
    }

    /// #612 review (F2) — pins the `SendRetry` CLASSIFICATION, which was previously unpinned.
    ///
    /// The reviewer's own mutation proved the hole: flipping `send_raw`'s `SendRetry::None` to
    /// `Retransmitted` — misclassifying SESSION_REQUEST / ACK / OutOfOrderAck / keepalive /
    /// SessionDisconnect / StatResponse as covered by a resend window that does not retain them —
    /// passed all 1195 tests. `send_failures_unretried` is the field an agent would trust to decide
    /// whether a datagram is really gone, so its discrimination must not rest on review alone.
    ///
    /// Drives all three classes through the SAME forced failure (an oversized datagram → EMSGSIZE)
    /// and asserts each one's effect on the two counters SEPARATELY, so a wrong classification in
    /// either direction is red:
    ///   - `send_raw` (all session-layer control)      → unretried MUST increment
    ///   - `send_app_packet_unreliable` (position)     → unretried MUST increment
    ///   - `send_app_packet` (reliable)                → unretried MUST NOT increment
    ///   - `poll_resend` (retransmit of a tracked one) → unretried MUST NOT increment
    #[tokio::test]
    async fn send_retry_classification_is_pinned_per_send_class() {
        // Big enough to blow past the 65_507-byte IPv4 UDP payload ceiling in every framing below.
        let huge = vec![0x5Au8; 70_000];

        // ── session-layer control (`send_raw`): NOT retained by the resend window ──────────────
        {
            let net_health: eqoxide_ipc::NetHealthShared = Default::default();
            let (mut stream, _rx, _peer, _addr) = test_stream_with_peer(net_health.clone()).await;
            stream.socket.writable().await.unwrap();
            // `send_raw` is the single framing path for OP_SESSION_REQUEST, OP_ACK,
            // OP_OUT_OF_ORDER, OP_KEEPALIVE, OP_SESSION_DISC and OP_STAT_RESPONSE, so classifying
            // it once classifies all six.
            stream.send_raw(OP_KEEPALIVE, &huge).expect_err("oversized control send must fail");
            let h = *net_health.lock().unwrap();
            assert_eq!(h.send_failures, 1);
            assert_eq!(h.send_failures_unretried, 1,
                "session-layer control (`send_raw`) is NOT retained in the resend window, so a \
                 failed one MUST count as unretried (#612 review F2). Classifying it as \
                 `Retransmitted` would tell an agent a datagram will be re-sent that never will be.");
        }

        // ── unreliable app packets: NOT retained ──────────────────────────────────────────────
        {
            let net_health: eqoxide_ipc::NetHealthShared = Default::default();
            let (mut stream, _rx, _peer, _addr) = test_stream_with_peer(net_health.clone()).await;
            stream.socket.writable().await.unwrap();
            stream.send_app_packet_unreliable(0x1234, &huge).expect_err("must fail");
            let h = *net_health.lock().unwrap();
            assert_eq!((h.send_failures, h.send_failures_unretried), (1, 1),
                "an unreliable app packet has no retransmit at all (#612 review F2)");
        }

        // ── reliable app packets AND their retransmits: retained ──────────────────────────────
        {
            let net_health: eqoxide_ipc::NetHealthShared = Default::default();
            let (mut stream, _rx, _peer, _addr) = test_stream_with_peer(net_health.clone()).await;
            stream.socket.writable().await.unwrap();
            stream.session.max_packet_size = u16::MAX;
            stream.send_app_packet(0x1234, &vec![0x5Au8; 65_520]);
            {
                let h = *net_health.lock().unwrap();
                assert_eq!((h.send_failures, h.send_failures_unretried), (1, 0),
                    "a reliable datagram IS retained and retransmitted, so it must NOT be counted \
                     as unretried (#612 review F2)");
            }
            // …and the retransmit itself is classified the same way. Age the window entry past its
            // backoff so `poll_resend` fires now (no sleeping, no timing dependence).
            stream.sent[0].sent_at = Instant::now() - std::time::Duration::from_secs(60);
            stream.poll_resend();
            let h = *net_health.lock().unwrap();
            assert_eq!(h.send_failures, 2, "the failed retransmit must be counted too");
            assert_eq!(h.send_failures_unretried, 0,
                "a retransmit of a still-tracked datagram is by definition retransmitted (#612 F2)");
            assert_eq!(stream.sent.len(), 1, "and it stays in the window for the next pass");
        }
    }

    /// #612 review (F1) — the session-end hole in "reliable sends recover structurally".
    ///
    /// `poll_resend` retries forever, but only while the SESSION lives. EQEmu drops the session at
    /// its ~30s `resend_timeout`, and the reconnect builds a fresh `EqStream` whose `sent` window
    /// starts EMPTY — so every still-outstanding reliable is genuinely lost, and
    /// `send_failures_unretried` reads 0 for all of them. Left unobserved that is the #612 bug one
    /// level up: a contract telling the agent a class of loss cannot have happened when it can.
    ///
    /// Asserts the accounting happens on DROP, which covers every path that tears the stream down
    /// (zone handoff, world reconnect, zone-in failure). The two paths that do NOT drop it are
    /// handled separately: clean shutdown calls `abandon_outstanding` explicitly (see
    /// `clean_shutdown_accounts_its_outstanding_window_explicitly_not_via_drop`), and a server-side
    /// session drop is not covered at all (#642) because the client never notices one.
    #[tokio::test]
    async fn reliables_outstanding_when_a_session_ends_are_counted_as_abandoned() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        {
            let (mut stream, _rx, _peer, _addr) = test_stream_with_peer(net_health.clone()).await;
            stream.socket.writable().await.unwrap();
            // Two reliables that SUCCEED on the wire but are never ACKed — the ordinary case, and
            // the one the counter is about: nothing here failed to send.
            stream.send_app_packet(0x1234, &[0xAA]);
            stream.send_app_packet(0x1234, &[0xBB]);
            assert_eq!(stream.sent.len(), 2, "both are outstanding, awaiting ACK");
            let h = *net_health.lock().unwrap();
            assert_eq!((h.send_failures, h.reliable_abandoned), (0, 0),
                "nothing is abandoned while the session is still alive");
        } // ← the session ends here (this is what a zone handoff / world reconnect does)

        let h = *net_health.lock().unwrap();
        assert_eq!(h.reliable_abandoned, 2,
            "un-ACKed reliables outstanding when the session ended are ABANDONED — the next \
             session's resend window starts empty, so nothing retransmits them (#612 review F1)");
        assert_eq!(h.send_failures, 0,
            "these were not send FAILURES — they reached the wire; conflating the two would make \
             both numbers useless");

        // And it must reach the agent, not just NetHealth.
        let state = eqoxide_http::testkit::empty_state_with_net_health(net_health.clone());
        let json = eqoxide_http::testkit::debug_json(state).await;
        assert_eq!(json["player"]["reliable_abandoned"].as_u64(), Some(2),
            "GET /v1/observe/debug must report abandoned reliables (#612 review F1). Body: {json}");
    }

    /// #612 review R1 — the clean-shutdown trigger, which `Drop` alone does NOT provide.
    ///
    /// `perform_clean_shutdown` borrows the stream and returns; its caller then parks in
    /// `loop { sleep }` while the process exits from the MAIN thread. A tokio task parked on another
    /// thread is never unwound, so no destructor runs and the outstanding window was never accounted
    /// on that path — measured by the round-2 reviewer, not reasoned. Hence the explicit call.
    ///
    /// Two halves, because neither alone is enough: the behavior of `abandon_outstanding` (counts
    /// the window, CLEARS it, and is therefore idempotent under a later `Drop`), and a source-level
    /// assertion that the shutdown path actually calls it — the same technique as
    /// `connect_awaits_writable_before_the_first_session_request`, and for the same reason: nothing
    /// about the code *looks* wrong if the call is deleted, and the parked-task behavior that makes
    /// it necessary cannot be reproduced in a unit test.
    #[tokio::test]
    async fn clean_shutdown_accounts_its_outstanding_window_explicitly_not_via_drop() {
        let net_health: eqoxide_ipc::NetHealthShared = Default::default();
        {
            let (mut stream, _rx, _peer, _addr) = test_stream_with_peer(net_health.clone()).await;
            stream.socket.writable().await.unwrap();
            stream.send_app_packet(0x1234, &[0xAA]);
            stream.send_app_packet(0x1234, &[0xBB]);
            assert_eq!(stream.sent.len(), 2);

            stream.abandon_outstanding();
            assert_eq!(net_health.lock().unwrap().reliable_abandoned, 2,
                "the explicit call must account the window (#612 R1)");
            assert!(stream.sent.is_empty(),
                "…and must CLEAR it, which is what makes a later Drop a no-op instead of a \
                 double count");
        } // Drop runs here on an already-emptied window.
        assert_eq!(net_health.lock().unwrap().reliable_abandoned, 2,
            "the subsequent Drop must NOT double-count (#612 R1)");

        // And the shutdown path must actually call it. `perform_clean_shutdown` lives in gameplay.rs.
        // COMMENTS STRIPPED FIRST (#612 round-3 review, B1). The previous version searched raw
        // source, so COMMENTING OUT the call left the guard green — the accounting was dead on the
        // one path `Drop` cannot reach and nothing failed. It only discriminated against outright
        // DELETION, and even that was accidental: the doc comment above the call happens to omit the
        // parentheses. A guard must not depend on that kind of luck.
        let gameplay_src = strip_comments(include_str!("gameplay.rs"));
        let at = gameplay_src.find("async fn perform_clean_shutdown(")
            .expect("gameplay.rs: perform_clean_shutdown not found");
        // Scan from the fn to the end of the file's next top-level item boundary — the function is
        // short and ends at the first column-0 `}`.
        let body_end = gameplay_src[at..].find("\n}\n").expect("unterminated fn") + at;
        let body = &gameplay_src[at..body_end];
        assert!(body.contains("abandon_outstanding()"),
            "perform_clean_shutdown must call `abandon_outstanding()` explicitly (#612 R1): its \
             task parks forever after returning, so `Drop` never runs and the outstanding reliable \
             window would go unaccounted on the clean-shutdown path. (Whole-line `//` comments are \
             stripped before this check, so line-commenting the call counts as removing it. A \
             `/* … */` block comment is NOT stripped and would defeat this guard — see \
             `strip_comments`.)");
    }

    /// #612 structural guard. The fix is only durable if there stays exactly ONE place in this CRATE
    /// that touches the socket's send path — `EqStream::transmit`, which cannot fail without
    /// stamping `NetHealth`. Four separate `let _ = self.socket.try_send(..)` calls are what made the
    /// bug possible; a fifth added later would silently re-open it. Source-level assertion, in the
    /// same spirit as `connect_awaits_writable_before_the_first_session_request` above.
    ///
    /// The needle is split so this test's own source doesn't match it.
    #[test]
    fn there_is_exactly_one_socket_send_call_in_the_crate_and_it_records_failures() {
        // EVERY module of this crate, not just transport.rs (#612 review, F4): the claim being
        // pinned is "one send call in the crate", so the scan has to cover the crate. `include_str!`
        // is compile-time, so a module that is added but not listed here is NOT covered — that
        // residual gap is real and is why the message below says which files were scanned.
        const SRCS: &[(&str, &str)] = &[
            ("transport.rs",     include_str!("transport.rs")),
            ("gameplay.rs",      include_str!("gameplay.rs")),
            ("action_loop.rs",   include_str!("action_loop.rs")),
            ("login.rs",         include_str!("login.rs")),
            ("packet_handler.rs",include_str!("packet_handler.rs")),
            ("item.rs",          include_str!("item.rs")),
            ("ucs.rs",           include_str!("ucs.rs")),
            ("lib.rs",           include_str!("lib.rs")),
        ];
        let needle = concat!("socket.try_", "send(");
        // Comment lines are excluded (`strip_comments`): several doc comments quote the old
        // `let _ = self.socket.try_send(..)` line precisely to explain what #612 fixed, and counting
        // those would make this guard unmaintainable (and, worse, satisfiable by deleting a comment).
        let per_file: Vec<(&str, usize, String)> = SRCS.iter()
            .map(|(name, src)| { let c = strip_comments(src); (*name, c.matches(needle).count(), c) })
            .collect();
        let hits: usize = per_file.iter().map(|(_, n, _)| n).sum();
        let counts: Vec<String> = per_file.iter().map(|(n, c, _)| format!("{n}={c}")).collect();
        assert_eq!(hits, 1,
            "eqoxide-net must funnel EVERY send through `EqStream::transmit` (#612) — found {hits} \
             direct `socket.try_send` calls across the crate's modules ({}). A send that bypasses \
             `transmit` cannot record its own failure, which is precisely the discarded-error bug \
             this crate used to have.", counts.join(", "));
        let code_only = &per_file.iter().find(|(_, c, _)| *c == 1).unwrap().2;
        let at = code_only.find(needle).unwrap();
        let enclosing = &code_only[..at];
        let fn_start = enclosing.rfind("    fn ").expect("the call must live inside a method");
        // `transmit` = "attempt, then account"; `attempt_send` is the attempt half, split out in
        // #641 so the test-only refusal injection can replace the whole socket interaction. The
        // funnel property is unchanged as long as `attempt_send` has exactly one caller, which the
        // count below pins — one definition plus one call site.
        // Needle split so this test's own source does not match it (same trick as `needle` above).
        let attempt = concat!("attempt_", "send(");
        assert!(enclosing[fn_start..].contains(attempt),
            "the one send call must be inside `fn attempt_send` (found: {:?})",
            &enclosing[fn_start..fn_start + 40.min(enclosing.len() - fn_start)]);
        assert_eq!(code_only.matches(attempt).count(), 3,
            "`attempt_send` must have exactly ONE caller (`transmit`) besides its definition — \
             expected 3 mentions (definition + the two cfg-gated calls in `transmit`). A second \
             caller would bypass `transmit`'s accounting exactly as a raw `try_send` would (#641)");
    }
}
