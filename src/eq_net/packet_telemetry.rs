//! Packet telemetry — a DEFAULT-OFF, low/zero-overhead capture-and-analysis rig for app-layer
//! packets (#525).
//!
//! Repeated hard bugs in this client have been packet/transport-level (#463 spawn-tail drop, #516
//! position jitter, #254/#302 reliable-stream fragility). Each was chased with throwaway `tracing`
//! that floods at send cadence and is gone the next session. This module is the reusable
//! replacement: a bounded ring buffer that records every inbound/outbound app packet — timestamp,
//! direction, opcode (name + hex), size, reliability, reliable sequence number, and a short decoded
//! summary for a few high-value opcodes — behind a single runtime flag, dumpable as JSON over HTTP
//! (`GET /v1/observe/packets`) with a built-in histogram + reliable-sequence-GAP analysis.
//!
//! ## Zero cost when off (the load-bearing property)
//!
//! Capture is gated on ONE relaxed atomic bool ([`enabled`]). When disabled, [`capture`] does a
//! single `AtomicBool::load(Relaxed)` and returns — no allocation, no lock, no name lookup, no
//! summary decode. The name lookup, hex formatting, `PacketRecord` allocation, and ring lock all
//! live in [`capture_slow`], which is `#[inline(never)]` and only reached when enabled. The hook at
//! each transport boundary is therefore a single predicted-not-taken branch on the hot path. Default
//! is OFF; nothing allocates the ring until the flag is turned on.
//!
//! ## Reliable-sequence gaps — what the detector honestly measures
//!
//! `rel_seq` is the reliable TRANSPORT sequence number attached to a record: on the send side, the
//! sequence assigned to that app packet's first datagram; on the receive side, the sequence whose
//! in-order delivery produced the app packet. [`detect_seq_gaps`] walks the recorded reliable-seq
//! stream per direction and reports forward jumps (with u16 wrap handling), ignoring duplicates and
//! reorders. This is exactly the signal for #463: the per-spawn `OP_ZoneEntry` packets are one
//! reliable datagram each, so a dropped spawn tail shows up as the reliable-seq stream ending early
//! or skipping.
//!
//! CAVEAT (kept honest): a single app packet large enough to be FRAGMENTED, or several bundled into
//! one `OP_Combined`, consumes/carries transport sequences that are not represented 1:1 as
//! records — so a "gap" reported on a bulk stream (`OP_ZoneSpawns`, `OP_PlayerProfile`) can be
//! fragmentation, not loss. Gap analysis is most precise on single-datagram streams (the outbound
//! command stream, the per-spawn `OP_ZoneEntry` burst). The endpoint/summary documents this so a
//! reader is never misled.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Default ring capacity (records). A zone-in spawn burst is a few hundred packets; this holds
/// several bursts' worth so an agent polling `/v1/observe/packets` every few seconds never misses a
/// window. Bounded so a long session can't grow memory without limit — oldest records are evicted.
pub const DEFAULT_CAPACITY: usize = 8192;

/// The single hot-path gate. `Relaxed` is sufficient: this is a pure on/off signal with no
/// happens-before requirement against the ring (the ring has its own lock).
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Direction of an app packet relative to this client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    /// Server → client (received).
    In,
    /// Client → server (sent).
    Out,
}

impl Dir {
    /// Parse the `dir=` query filter. Accepts `in`/`out` (case-insensitive); anything else is `None`.
    pub fn parse(s: &str) -> Option<Dir> {
        match s.trim().to_ascii_lowercase().as_str() {
            "in" => Some(Dir::In),
            "out" => Some(Dir::Out),
            _ => None,
        }
    }
}

/// One captured app packet.
#[derive(Clone, Debug, serde::Serialize)]
pub struct PacketRecord {
    /// Monotonic 0-based capture index (global, never reused). Use the last value seen as the
    /// `?since=` cursor to page forward without racing a live tail.
    pub n: u64,
    /// Milliseconds since the telemetry epoch (first capture-enable). Monotonic.
    pub t_ms: u64,
    pub dir: Dir,
    pub opcode: u16,
    /// `"0x7dfc"` — always present, even for opcodes with no symbolic name.
    pub op_hex: String,
    /// Symbolic constant name (`"OP_CLIENT_UPDATE"`) or `"OP_Unknown"` if the opcode isn't in the
    /// table. This is the audited `protocol` constant identifier — see [`opcode_name`].
    pub op_name: &'static str,
    /// Full app-packet size in bytes: the 2-byte opcode + body.
    pub size: usize,
    /// True when a reliable transport SEQUENCE is known for this packet (delivered in order on the
    /// reliable stream). An app packet unbundled from an `OP_Combined` is recorded `false` even if
    /// the enclosing datagram arrived reliably, because the sub-packet carries no individual
    /// sequence — so this field tracks "has a usable `rel_seq`", which is exactly what the gap
    /// detector consumes, rather than a claim about the enclosing datagram's delivery mode.
    pub reliable: bool,
    /// The reliable transport sequence number, when `reliable`. See the module docs for how the
    /// receive-side value relates to fragmentation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rel_seq: Option<u16>,
    /// A short decoded one-liner for a few high-value opcodes (position/spawn/zone/delete). `None`
    /// for opcodes with no decoder or when the body was too short to decode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

struct Ring {
    epoch: Instant,
    next_n: u64,
    buf: VecDeque<PacketRecord>,
    cap: usize,
}

static RING: OnceLock<Mutex<Ring>> = OnceLock::new();

fn ring() -> &'static Mutex<Ring> {
    RING.get_or_init(|| {
        Mutex::new(Ring {
            epoch: Instant::now(),
            next_n: 0,
            buf: VecDeque::with_capacity(DEFAULT_CAPACITY),
            cap: DEFAULT_CAPACITY,
        })
    })
}

/// Is capture active? Single relaxed atomic load — safe to call anywhere.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Turn capture on/off at runtime (startup flag, env var, or the `?enable=` endpoint toggle). When
/// turning ON, the ring is allocated eagerly so the first captured packet doesn't pay init.
pub fn set_enabled(on: bool) {
    if on {
        ring(); // force allocation up-front
    }
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether `EQOXIDE_PKTLOG` requests capture at startup. Truthy = set to anything other than
/// empty / `0` / `false` / `off` / `no` (case-insensitive). Absent ⇒ false (default OFF).
pub fn env_enabled() -> bool {
    match std::env::var("EQOXIDE_PKTLOG") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v.is_empty() || v == "0" || v == "false" || v == "off" || v == "no")
        }
        Err(_) => false,
    }
}

/// The capture hook, called at each transport send/recv boundary.
///
/// HOT PATH: if disabled, this is a single relaxed atomic load and an early return — no allocation,
/// no lock, no work touching `payload`. Everything expensive is deferred to [`capture_slow`].
///
/// `payload` is the app-packet BODY (bytes after the 2-byte opcode), exactly the slice the transport
/// already holds — passing a `&[u8]` costs nothing.
#[inline]
pub fn capture(dir: Dir, opcode: u16, payload: &[u8], reliable: bool, rel_seq: Option<u16>) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    capture_slow(dir, opcode, payload, reliable, rel_seq);
}

/// The enabled-only path: look up the name, decode a summary, build the record, and push it into the
/// ring (evicting the oldest if full). `#[inline(never)]` so none of this — nor its string/format
/// machinery — is inlined into the hot caller, keeping the disabled hook to a bare branch.
#[inline(never)]
fn capture_slow(dir: Dir, opcode: u16, payload: &[u8], reliable: bool, rel_seq: Option<u16>) {
    let summary = summarize(opcode, payload);
    let rec_op_name = opcode_name(opcode);
    let ring = ring();
    let mut g = ring.lock().unwrap();
    let n = g.next_n;
    g.next_n += 1;
    let t_ms = g.epoch.elapsed().as_millis() as u64;
    let rec = PacketRecord {
        n,
        t_ms,
        dir,
        opcode,
        op_hex: format!("{opcode:#06x}"),
        op_name: rec_op_name,
        size: payload.len() + 2,
        reliable,
        rel_seq,
        summary,
    };
    if g.buf.len() >= g.cap {
        g.buf.pop_front();
    }
    g.buf.push_back(rec);
}

/// Drop all captured records and reset the epoch (but leave the enabled flag untouched). Used by the
/// `?clear=1` endpoint control so an agent can zero the window before driving a scenario.
pub fn clear() {
    let mut g = ring().lock().unwrap();
    g.buf.clear();
    g.next_n = 0;
    g.epoch = Instant::now();
}

/// Filter for [`query`] / the HTTP endpoint.
#[derive(Clone, Debug, Default)]
pub struct Query {
    /// Only records with `n >= since`.
    pub since: Option<u64>,
    /// Only this direction.
    pub dir: Option<Dir>,
    /// Only this opcode.
    pub op: Option<u16>,
    /// At most this many records (most recent matching ones win when the cap bites).
    pub limit: Option<usize>,
}

/// Snapshot the ring, applying `q`'s filters. Returns records in capture order (oldest → newest).
/// When `limit` is set and more than `limit` match, the MOST RECENT `limit` are returned (still in
/// order) — an agent tailing wants the newest, not the oldest.
pub fn query(q: &Query) -> Vec<PacketRecord> {
    let g = ring().lock().unwrap();
    let mut out: Vec<PacketRecord> = g
        .buf
        .iter()
        .filter(|r| q.since.is_none_or(|s| r.n >= s))
        .filter(|r| q.dir.is_none_or(|d| r.dir == d))
        .filter(|r| q.op.is_none_or(|o| r.opcode == o))
        .cloned()
        .collect();
    if let Some(limit) = q.limit {
        if out.len() > limit {
            out.drain(0..out.len() - limit);
        }
    }
    out
}

// ── Analysis ────────────────────────────────────────────────────────────────

/// Per-opcode rollup for the histogram.
#[derive(Clone, Debug, serde::Serialize)]
pub struct OpStat {
    pub opcode: u16,
    pub op_hex: String,
    pub op_name: &'static str,
    pub dir: Dir,
    pub count: u64,
    pub bytes: u64,
    /// Timestamp (ms) of the first and last record for this (opcode, dir) in the window.
    pub first_ms: u64,
    pub last_ms: u64,
    /// Records per second across `[first_ms, last_ms]`. `count` itself when the span is zero (a
    /// single record, or several in the same millisecond) — an honest "N in an instant" rather than
    /// a divide-by-zero infinity.
    pub rate_per_sec: f64,
}

/// A detected discontinuity in the reliable-sequence stream for one direction.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SeqGap {
    pub dir: Dir,
    /// Capture index (`n`) of the record just BEFORE the gap.
    pub after_n: u64,
    /// The reliable seq of that record, and of the next reliable record — the gap is between them.
    pub prev_seq: u16,
    pub next_seq: u16,
    /// How many sequence numbers are missing between them (≥ 1).
    pub missing: u16,
}

/// Detect forward gaps in the reliable-sequence stream, per direction, over `records` (assumed in
/// capture order). Only records with `reliable && rel_seq.is_some()` participate; each direction is
/// tracked independently (in/out seq spaces are separate).
///
/// For consecutive reliable records in a direction with sequences `prev` then `next`, let
/// `delta = next.wrapping_sub(prev)` (u16, so wrap at 0xFFFF→0x0000 is handled):
///   - `delta == 0` → duplicate/retransmit of the same seq — not a gap.
///   - `delta == 1` → contiguous — not a gap.
///   - `2 ..= 0x7FFF` → a forward gap; `missing = delta - 1`.
///   - `delta >= 0x8000` → the seq went backwards (reorder/duplicate) — not a forward gap.
///
/// This is the mutation-checked core: the `delta - 1` and the `2..0x8000` window are what the tests
/// pin, so the arithmetic can't silently drift.
pub fn detect_seq_gaps(records: &[PacketRecord]) -> Vec<SeqGap> {
    let mut gaps = Vec::new();
    let mut last: [Option<(u64, u16)>; 2] = [None, None]; // [In, Out] → (n, seq)
    for r in records {
        if !r.reliable {
            continue;
        }
        let Some(seq) = r.rel_seq else { continue };
        let idx = match r.dir {
            Dir::In => 0,
            Dir::Out => 1,
        };
        if let Some((prev_n, prev_seq)) = last[idx] {
            let delta = seq.wrapping_sub(prev_seq);
            if (2..0x8000).contains(&delta) {
                gaps.push(SeqGap {
                    dir: r.dir,
                    after_n: prev_n,
                    prev_seq,
                    next_seq: seq,
                    missing: delta - 1,
                });
            }
        }
        last[idx] = Some((r.n, seq));
    }
    gaps
}

/// Opcode histogram + per-opcode rate over `records`, sorted by descending count.
pub fn histogram(records: &[PacketRecord]) -> Vec<OpStat> {
    use std::collections::HashMap;
    // key: (opcode, dir-as-bool-in)
    let mut map: HashMap<(u16, bool), OpStat> = HashMap::new();
    for r in records {
        let key = (r.opcode, r.dir == Dir::In);
        let e = map.entry(key).or_insert_with(|| OpStat {
            opcode: r.opcode,
            op_hex: r.op_hex.clone(),
            op_name: r.op_name,
            dir: r.dir,
            count: 0,
            bytes: 0,
            first_ms: r.t_ms,
            last_ms: r.t_ms,
            rate_per_sec: 0.0,
        });
        e.count += 1;
        e.bytes += r.size as u64;
        e.first_ms = e.first_ms.min(r.t_ms);
        e.last_ms = e.last_ms.max(r.t_ms);
    }
    let mut out: Vec<OpStat> = map.into_values().collect();
    for s in &mut out {
        let span_ms = s.last_ms.saturating_sub(s.first_ms);
        s.rate_per_sec = if span_ms == 0 {
            s.count as f64
        } else {
            s.count as f64 * 1000.0 / span_ms as f64
        };
    }
    out.sort_by(|a, b| b.count.cmp(&a.count).then(a.opcode.cmp(&b.opcode)));
    out
}

/// Full analysis of a set of records: totals, per-direction counts, opcode histogram, and reliable
/// sequence gaps. Serialized by `GET /v1/observe/packets?summary=1`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Analysis {
    pub total: usize,
    pub in_count: usize,
    pub out_count: usize,
    /// Milliseconds spanned by the window (`last.t_ms - first.t_ms`).
    pub window_ms: u64,
    pub histogram: Vec<OpStat>,
    pub seq_gaps: Vec<SeqGap>,
    /// Honest caveat, echoed into the JSON so a reader of a raw dump can't miss it.
    pub seq_gap_note: &'static str,
}

/// Reliable-seq gap caveat, surfaced in every summary payload.
pub const SEQ_GAP_NOTE: &str =
    "rel_seq is the transport reliable sequence per app packet. A fragmented single packet or an \
     OP_Combined bundle consumes/carries sequences not shown 1:1, so a gap on a bulk stream \
     (OP_ZoneSpawns/OP_PlayerProfile) may be fragmentation, not loss. Gap analysis is most precise \
     on single-datagram streams (outbound commands, the per-spawn OP_ZoneEntry burst).";

/// Build the [`Analysis`] over `records` — histogram/totals AND gaps over the same set.
pub fn analyze(records: &[PacketRecord]) -> Analysis {
    analyze_with_gaps(records, records)
}

/// As [`analyze`], but compute the reliable-seq gaps over a SEPARATE `gap_records` stream while the
/// histogram/totals describe `records`.
///
/// This exists for the HTTP endpoint's `?op=` + `?summary=1` combination. `rel_seq` is a single
/// per-direction counter shared across ALL opcodes, so if the records fed to [`detect_seq_gaps`]
/// were narrowed to one opcode, the intervening reliable packets of OTHER opcodes — which
/// legitimately consumed sequence numbers — would be absent and the detector would report
/// FABRICATED "lost packets". That is an agent-honesty violation (a confident falsehood), so the
/// gap stream must stay dir-filtered but NOT op-filtered (#532 review). The histogram still honors
/// the op filter, because that is the view the caller asked to see.
pub fn analyze_with_gaps(records: &[PacketRecord], gap_records: &[PacketRecord]) -> Analysis {
    let in_count = records.iter().filter(|r| r.dir == Dir::In).count();
    let out_count = records.len() - in_count;
    let window_ms = match (records.first(), records.last()) {
        (Some(a), Some(b)) => b.t_ms.saturating_sub(a.t_ms),
        _ => 0,
    };
    Analysis {
        total: records.len(),
        in_count,
        out_count,
        window_ms,
        histogram: histogram(records),
        seq_gaps: detect_seq_gaps(gap_records),
        seq_gap_note: SEQ_GAP_NOTE,
    }
}

// ── Opcode name table + summaries ─────────────────────────────────────────────

/// Read a NUL-terminated ASCII string starting at `off`, for the zone-name summary.
fn cstr_at(buf: &[u8], off: usize) -> Option<String> {
    let s = buf.get(off..)?;
    let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    Some(String::from_utf8_lossy(&s[..end]).into_owned())
}

/// Short decoded one-liner for the highest-value opcodes (position/spawn/zone/delete). Returns
/// `None` for opcodes with no decoder, or when the body is too short to decode — never a guess.
fn summarize(opcode: u16, payload: &[u8]) -> Option<String> {
    use super::protocol;
    match opcode {
        protocol::OP_CLIENT_UPDATE => {
            let p = protocol::decode_position_update(payload)?;
            Some(format!(
                "spawn={} x={:.1} y={:.1} z={:.1} h={:.0}",
                p.spawn_id, p.x, p.y, p.z, p.heading
            ))
        }
        protocol::OP_NEW_SPAWN | protocol::OP_ZONE_ENTRY => {
            let (s, _) = protocol::parse_rof2_spawn(payload)?;
            Some(format!(
                "id={} '{}' lvl={} npc={} x={:.1} y={:.1} z={:.1}",
                s.spawn_id, s.name, s.level, s.npc, s.x, s.y, s.z
            ))
        }
        protocol::OP_ZONE_SPAWNS => {
            // Bulk stream (often fragmented) — count the records without a full decode of each.
            let mut buf = payload;
            let mut count = 0u32;
            while let Some((_, consumed)) = protocol::parse_rof2_spawn(buf) {
                if consumed == 0 || consumed > buf.len() {
                    break;
                }
                count += 1;
                buf = &buf[consumed..];
            }
            Some(format!("bulk spawns count={} ({} bytes)", count, payload.len()))
        }
        protocol::OP_DELETE_SPAWN => {
            let b = payload.get(0..4)?;
            let id = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            Some(format!("id={}", id))
        }
        protocol::OP_NEW_ZONE => {
            // zone_short_name is a cstr at offset 64 in the RoF2 NewZone struct.
            let name = cstr_at(payload, 64)?;
            Some(format!("zone='{}'", name))
        }
        _ => None,
    }
}

/// Map a u16 app opcode to its symbolic name, or `"OP_Unknown"`.
///
/// The arms are the constant NAMES from [`super::protocol`], compared by value — so this table is
/// compile-time-linked to the audited opcode constants (`protocol/mod.rs`) and cannot silently drift
/// from them. First match wins for the handful of values shared across login/world revisions.
pub fn opcode_name(op: u16) -> &'static str {
    use super::protocol as p;
    macro_rules! names {
        ($($name:ident),* $(,)?) => {
            $( if op == p::$name { return stringify!($name); } )*
        };
    }
    names!(
        OP_CLIENT_UPDATE, OP_NEW_SPAWN, OP_DELETE_SPAWN, OP_ZONE_ENTRY, OP_ZONE_SPAWNS,
        OP_NEW_ZONE, OP_REQ_CLIENT_SPAWN, OP_REQ_NEW_ZONE, OP_CLIENT_READY, OP_SEND_EXP_ZONE_IN,
        OP_PLAYER_PROFILE, OP_CHAR_INVENTORY, OP_ITEM_PACKET, OP_TIME_OF_DAY, OP_WEATHER,
        OP_SEND_ZONE_POINTS, OP_SPAWN_DOOR, OP_MOVE_DOOR, OP_CLICK_DOOR, OP_ACK_PACKET,
        OP_SPAWN_APPEARANCE, OP_ANIMATION, OP_WEAR_CHANGE, OP_FLOAT_LIST_THING,
        OP_HP_UPDATE, OP_MOB_HEALTH, OP_DEATH, OP_DAMAGE, OP_AUTO_ATTACK, OP_AUTO_ATTACK2,
        OP_TARGET_COMMAND, OP_TARGET_MOUSE, OP_CONSIDER, OP_ENV_DAMAGE,
        OP_BEGIN_CAST, OP_CAST_SPELL, OP_INTERRUPT_CAST, OP_MANA_CHANGE, OP_MEMORIZE_SPELL,
        OP_CHANNEL_MESSAGE, OP_SPECIAL_MESG, OP_FORMATTED_MESSAGE, OP_SIMPLE_MESSAGE,
        OP_EMOTE, OP_CHAT_MESSAGE, OP_SET_CHAT_SERVER,
        OP_ZONE_CHANGE, OP_REQUEST_CLIENT_ZONE_CHANGE, OP_ZONE_PLAYER_TO_BIND, OP_TRANSLOCATE,
        OP_ZONE_SERVER_INFO, OP_SET_SERVER_FILTER, OP_RESPAWN_WINDOW,
        OP_MONEY_UPDATE, OP_MONEY_ON_CORPSE, OP_EXP_UPDATE, OP_LEVEL_UPDATE, OP_SKILL_UPDATE,
        OP_MOVE_ITEM, OP_DELETE_ITEM, OP_DELETE_CHARGE, OP_ITEM_LINK_CLICK,
        OP_LOOT_REQUEST, OP_LOOT_ITEM, OP_LOOT_COMPLETE, OP_END_LOOT_REQUEST,
        OP_SHOP_REQUEST, OP_SHOP_END, OP_SHOP_END_CONFIRM, OP_SHOP_PLAYER_BUY, OP_SHOP_PLAYER_SELL,
        OP_TRADE_REQUEST, OP_TRADE_REQUEST_ACK, OP_TRADE_ACCEPT_CLICK, OP_CANCEL_TRADE,
        OP_FINISH_TRADE,
        OP_GROUP_INVITE, OP_GROUP_FOLLOW, OP_GROUP_FOLLOW2, OP_GROUP_DISBAND,
        OP_GROUP_DISBAND_OTHER, OP_GROUP_DISBAND_YOU, OP_GROUP_ACKNOWLEDGE, OP_GROUP_UPDATE,
        OP_GROUP_UPDATE_B, OP_GROUP_LEADER_CHANGE, OP_GROUP_MAKE_LEADER,
        OP_GUILD_LIST, OP_GUILD_MEMBER_LIST, OP_GUILD_MEMBER_UPDATE, OP_GUILD_INVITE,
        OP_GUILD_INVITE_ACCEPT, OP_GUILD_REMOVE,
        OP_PET_COMMANDS, OP_GM_TRAINING, OP_GM_END_TRAINING, OP_GM_TRAIN_SKILL, OP_GMKICK,
        OP_TASK_ACTIVITY, OP_TASK_DESCRIPTION, OP_COMPLETED_TASKS,
        OP_READ_BOOK, OP_FINISH_WINDOW, OP_FRIENDS_WHO, OP_WHO_ALL_REQUEST, OP_WHO_ALL_RESPONSE,
        OP_CAMP, OP_LOGOUT, OP_APPROVE_NAME, OP_CHARACTER_CREATE,
        OP_ENTER_WORLD, OP_POST_ENTER_WORLD, OP_WORLD_CLIENT_READY, OP_WORLD_COMPLETE,
        OP_WORLD_CRC1, OP_WORLD_CRC2, OP_EXPANSION_INFO, OP_SEND_CHAR_INFO, OP_MOTD,
        OP_SEND_LOGIN_INFO, OP_APPROVE_WORLD, OP_LOG_SERVER,
        OP_SESSION_READY, OP_LOGIN, OP_LOGIN_ACCEPTED, OP_LOGIN_EXPANSION_PACKET_DATA,
        OP_SERVER_LIST_REQUEST, OP_SERVER_LIST_RESPONSE, OP_PLAY_EVERQUEST_REQ,
        OP_PLAY_EVERQUEST_RESP,
    );
    "OP_Unknown"
}

/// Shared serialization lock for any test that toggles capture. `ENABLED`, `RING`, and the epoch are
/// process-global, so tests in DIFFERENT modules (this module's unit tests AND the HTTP endpoint
/// test in `crate::http::observe`) that enable/clear/read capture must hold this SAME lock or they
/// clobber each other under cargo's parallel test runner. `pub(crate)` so `observe`'s test can reach
/// it. Poison-tolerant: a panicking test still releases a usable guard.
#[cfg(test)]
pub(crate) fn test_capture_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(n: u64, dir: Dir, opcode: u16, reliable: bool, rel_seq: Option<u16>) -> PacketRecord {
        PacketRecord {
            n,
            t_ms: n,
            dir,
            opcode,
            op_hex: format!("{opcode:#06x}"),
            op_name: opcode_name(opcode),
            size: 4,
            reliable,
            rel_seq,
            summary: None,
        }
    }

    // Serialize enabling/disabling across ALL tests that touch capture (see `test_capture_lock`).
    use super::test_capture_lock as test_lock;

    #[test]
    fn disabled_is_a_no_op_and_does_not_capture() {
        let _g = test_lock();
        set_enabled(false);
        clear();
        // Even a "packet" handed to the hook must not be recorded while disabled.
        capture(Dir::In, 0x1234, &[1, 2, 3, 4], true, Some(7));
        assert_eq!(query(&Query::default()).len(), 0, "disabled capture must record nothing");
        assert!(!enabled());
    }

    #[test]
    fn enabled_captures_direction_opcode_size_and_seq() {
        let _g = test_lock();
        set_enabled(true);
        clear();
        capture(Dir::Out, 0x7dfc, &[0u8; 22], true, Some(42));
        capture(Dir::In, 0x6097, &[9u8; 10], false, None);
        set_enabled(false);

        let all = query(&Query::default());
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].dir, Dir::Out);
        assert_eq!(all[0].opcode, 0x7dfc);
        assert_eq!(all[0].size, 24, "size = opcode(2) + body(22)");
        assert!(all[0].reliable);
        assert_eq!(all[0].rel_seq, Some(42));
        assert_eq!(all[0].op_name, "OP_CLIENT_UPDATE", "name from the const table");
        assert_eq!(all[1].dir, Dir::In);
        assert_eq!(all[1].rel_seq, None);
        assert!(!all[1].reliable);
    }

    #[test]
    fn query_filters_by_dir_op_since_and_limit() {
        let _g = test_lock();
        set_enabled(true);
        clear();
        capture(Dir::In, 0x0001, &[], true, Some(0)); // n=0
        capture(Dir::Out, 0x0002, &[], true, Some(0)); // n=1
        capture(Dir::In, 0x0001, &[], true, Some(1)); // n=2
        set_enabled(false);

        assert_eq!(query(&Query { dir: Some(Dir::In), ..Default::default() }).len(), 2);
        assert_eq!(query(&Query { op: Some(0x0002), ..Default::default() }).len(), 1);
        assert_eq!(query(&Query { since: Some(2), ..Default::default() }).len(), 1);
        // limit keeps the MOST RECENT
        let last1 = query(&Query { limit: Some(1), ..Default::default() });
        assert_eq!(last1.len(), 1);
        assert_eq!(last1[0].n, 2);
    }

    #[test]
    fn ring_is_bounded_and_evicts_oldest() {
        let _g = test_lock();
        set_enabled(true);
        clear();
        // Overflow the ring by `extra` past DEFAULT_CAPACITY and check the bound + eviction hold.
        let extra = 5usize;
        for _ in 0..DEFAULT_CAPACITY + extra {
            capture(Dir::In, 0x0001, &[], false, None);
        }
        set_enabled(false);
        let all = query(&Query::default());
        assert_eq!(all.len(), DEFAULT_CAPACITY, "ring must not grow past its cap");
        // Oldest `extra` were evicted → the first surviving record's n is `extra`.
        assert_eq!(all[0].n, extra as u64, "oldest records are evicted first");
    }

    #[test]
    fn detect_seq_gaps_finds_a_synthetic_gap() {
        // Reliable in-stream: 10, 11, [12,13 missing], 14 → one gap of 2 after n=1.
        let records = vec![
            rec(0, Dir::In, 0x1, true, Some(10)),
            rec(1, Dir::In, 0x1, true, Some(11)),
            rec(2, Dir::In, 0x1, true, Some(14)),
        ];
        let gaps = detect_seq_gaps(&records);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0], SeqGap {
            dir: Dir::In,
            after_n: 1,
            prev_seq: 11,
            next_seq: 14,
            missing: 2,
        });
    }

    #[test]
    fn detect_seq_gaps_contiguous_and_wrap_have_no_gap() {
        // Contiguous incl. the 0xFFFF→0x0000 wrap boundary → no gaps.
        let records = vec![
            rec(0, Dir::Out, 0x1, true, Some(0xFFFE)),
            rec(1, Dir::Out, 0x1, true, Some(0xFFFF)),
            rec(2, Dir::Out, 0x1, true, Some(0x0000)),
            rec(3, Dir::Out, 0x1, true, Some(0x0001)),
        ];
        assert!(detect_seq_gaps(&records).is_empty(), "wrap-around must not read as a gap");
    }

    #[test]
    fn detect_seq_gaps_wrap_boundary_gap_counts_correctly() {
        // 0xFFFE then 0x0001 → seqs 0xFFFF and 0x0000 missing → gap of 2 across the wrap.
        let records = vec![
            rec(0, Dir::Out, 0x1, true, Some(0xFFFE)),
            rec(1, Dir::Out, 0x1, true, Some(0x0001)),
        ];
        let gaps = detect_seq_gaps(&records);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].missing, 2, "gap must span the u16 wrap");
        assert_eq!(gaps[0].prev_seq, 0xFFFE);
        assert_eq!(gaps[0].next_seq, 0x0001);
    }

    #[test]
    fn detect_seq_gaps_ignores_duplicates_and_reorders() {
        // Duplicate (same seq) and a backward step (reorder) must NOT be reported as forward gaps.
        let records = vec![
            rec(0, Dir::In, 0x1, true, Some(5)),
            rec(1, Dir::In, 0x1, true, Some(5)),  // duplicate
            rec(2, Dir::In, 0x1, true, Some(6)),
            rec(3, Dir::In, 0x1, true, Some(4)),  // reorder / older retransmit
            rec(4, Dir::In, 0x1, true, Some(7)),  // 4→7 IS a forward gap of 2
        ];
        let gaps = detect_seq_gaps(&records);
        assert_eq!(gaps.len(), 1, "only the 4→7 forward jump is a gap");
        assert_eq!(gaps[0].prev_seq, 4);
        assert_eq!(gaps[0].next_seq, 7);
        assert_eq!(gaps[0].missing, 2);
    }

    #[test]
    fn detect_seq_gaps_separates_directions_and_skips_unreliable() {
        // In-stream 1,3 (gap) and Out-stream 10,11 (no gap) are tracked independently; unreliable
        // records (no rel_seq) never participate even if interleaved.
        let records = vec![
            rec(0, Dir::In, 0x1, true, Some(1)),
            rec(1, Dir::Out, 0x1, true, Some(10)),
            rec(2, Dir::In, 0x7dfc, false, None), // unreliable — ignored
            rec(3, Dir::Out, 0x1, true, Some(11)),
            rec(4, Dir::In, 0x1, true, Some(3)),  // In: 1→3 gap of 1
        ];
        let gaps = detect_seq_gaps(&records);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].dir, Dir::In);
        assert_eq!(gaps[0].missing, 1);
    }

    #[test]
    fn histogram_counts_bytes_and_rate_per_opcode_and_dir() {
        let mut records = vec![
            rec(0, Dir::In, 0x1, true, Some(0)),
            rec(1, Dir::In, 0x1, true, Some(1)),
            rec(2, Dir::Out, 0x1, true, Some(0)),
        ];
        records[0].t_ms = 0;
        records[1].t_ms = 1000; // 2 In records spanning 1s → 2/s
        records[2].t_ms = 500;
        let h = histogram(&records);
        // Two buckets: (0x1,In) count 2, (0x1,Out) count 1.
        let in_stat = h.iter().find(|s| s.dir == Dir::In).unwrap();
        assert_eq!(in_stat.count, 2);
        assert_eq!(in_stat.bytes, 8); // 2 × size 4
        assert!((in_stat.rate_per_sec - 2.0).abs() < 1e-9, "2 records over 1s = 2/s");
        let out_stat = h.iter().find(|s| s.dir == Dir::Out).unwrap();
        assert_eq!(out_stat.count, 1);
        assert_eq!(out_stat.rate_per_sec, 1.0, "single record → count, not div-by-zero");
    }

    #[test]
    fn analyze_rolls_up_totals_and_gaps() {
        let records = vec![
            rec(0, Dir::In, 0x1, true, Some(1)),
            rec(1, Dir::In, 0x1, true, Some(3)), // gap
            rec(2, Dir::Out, 0x2, true, Some(0)),
        ];
        let a = analyze(&records);
        assert_eq!(a.total, 3);
        assert_eq!(a.in_count, 2);
        assert_eq!(a.out_count, 1);
        assert_eq!(a.seq_gaps.len(), 1);
        assert!(!a.seq_gap_note.is_empty());
    }

    /// The #532-review honesty fix: op-filtering the histogram must NOT fabricate a seq gap. The
    /// full in-stream is contiguous (op 0x1 @seq0, op 0x2 @seq1, op 0x1 @seq2); an agent asking for
    /// just op 0x1 would see seqs 0 and 2 — a fake "gap of 1" IF gaps were computed on the filtered
    /// set. `analyze_with_gaps` computes gaps over the UNFILTERED stream, so there are none.
    #[test]
    fn analyze_with_gaps_op_filter_does_not_fabricate_a_gap() {
        let full = vec![
            rec(0, Dir::In, 0x1, true, Some(0)),
            rec(1, Dir::In, 0x2, true, Some(1)), // intervening OTHER opcode consumed seq 1
            rec(2, Dir::In, 0x1, true, Some(2)),
        ];
        let op1_only: Vec<PacketRecord> = full.iter().filter(|r| r.opcode == 0x1).cloned().collect();
        // Sanity: computing gaps on the OP-FILTERED set alone WOULD fabricate a gap (0→2).
        assert_eq!(detect_seq_gaps(&op1_only).len(), 1, "filtered-set gap is the trap we avoid");
        // The real path: histogram over op-filtered, gaps over the full stream → no fabricated gap.
        let a = analyze_with_gaps(&op1_only, &full);
        assert!(a.seq_gaps.is_empty(), "op filter must not fabricate a reliable-seq gap");
        assert_eq!(a.total, 2, "totals still describe the op-filtered view");
        // And a REAL gap in the full stream is still reported.
        let full_gapped = vec![
            rec(0, Dir::In, 0x1, true, Some(0)),
            rec(1, Dir::In, 0x1, true, Some(2)), // real gap: seq 1 missing entirely
        ];
        assert_eq!(analyze_with_gaps(&full_gapped, &full_gapped).seq_gaps.len(), 1);
    }

    #[test]
    fn opcode_name_maps_known_and_unknown() {
        assert_eq!(opcode_name(0x7dfc), "OP_CLIENT_UPDATE");
        assert_eq!(opcode_name(0x6097), "OP_NEW_SPAWN");
        assert_eq!(opcode_name(0x5089), "OP_ZONE_ENTRY");
        assert_eq!(opcode_name(0xABCD), "OP_Unknown");
    }

    #[test]
    fn env_enabled_truthiness() {
        // Serialize env mutation with other capture tests (all share process globals).
        let _g = test_lock();
        for (v, want) in [("1", true), ("true", true), ("on", true), ("yes", true),
                          ("0", false), ("false", false), ("off", false), ("", false)] {
            std::env::set_var("EQOXIDE_PKTLOG", v);
            assert_eq!(env_enabled(), want, "EQOXIDE_PKTLOG={v:?}");
        }
        std::env::remove_var("EQOXIDE_PKTLOG");
        assert!(!env_enabled(), "unset ⇒ default OFF");
    }
}
