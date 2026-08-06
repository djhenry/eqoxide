//! The net action loop: drains HTTP/IPC command slots into EQ wire packets each tick (loot,
//! doors, quests, group, trainer, zone-cross, chat, combat, merchant, …), and walks the player
//! toward a `/goto` target in capped steps at 150 ms intervals, sending movement packets and
//! notifying the render loop.

use std::time::{Duration, Instant};

/// Nav tick interval (ms). Steps are gated to fire no more often than this.
const NAV_TICK_MS: u128 = 150;
/// Native Titanium base run speed in EQ units/second (runspeed 0.7 → 44 u/s; 10 Hz updates of
/// 4.4 u each). Per eq-client-expert, see ~/git/eq_kb/player-movement-speed.md.
/// We must NOT move faster than this: even where THIS server tolerates it, others rubber-band or
/// reject motion the real client can't produce. Defined in `eqoxide-core::physics` (#544 Step 2d)
/// and re-exported here so `crate::action_loop::RUN_SPEED` keeps resolving.
pub(crate) use eqoxide_core::physics::RUN_SPEED;
use crate::protocol::*;
use crate::transport::EqStream;
use eqoxide_core::game_state::{GameState, ZonePoint};
use eqoxide_ipc::{TradeCmd, CampCmd};
use eqoxide_ipc::MoveIntent;

/// Min interval (ms) between OP_ClientUpdate sends while moving (native `0x118` = 280 ms).
const POS_SEND_MOVING_MS: u128 = 280;
/// Forced keepalive interval (ms) when idle (native `0x514` = 1300 ms).
const POS_SEND_KEEPALIVE_MS: u128 = 1300;
/// Interval (ms) between OP_FloatListThing (movement-history) sends. The server's MQGhost detector
/// (`cheat_manager.cpp`) trips ~70s after movement if this packet never arrives, then re-flags on
/// every movement check. Sending one benign entry every 30s keeps the 70s timer alive (eqoxide#105).
const MOVEMENT_HISTORY_MS: u128 = 30_000;

/// Convert an actual movement speed (this client's own controller speed, EQ units/second) to the
/// wire `animation` field of `OP_ClientUpdate`. This field is NOT a moving/idle boolean — EQEmu
/// computes `base_runspeed = eq_runspeed_float * 40`, with the player special-case
/// `eq_runspeed_float` 0.7 (running) → 28 and 0.3 (walking) → 12 (`EQEmu/zone/mob.cpp:190-196`,
/// sent as `spu->animation` at `mob.cpp:1743-1745`). Every other client (native and eqoxide)
/// decodes `speed = animation / 40` to pick the locomotion clip and feeds it to anti-cheat speed
/// limiting (`cheat_manager.cpp:291-298`) and endurance drain (`client_mods.cpp:1676`), so sending
/// a constant `1` (decoding to 0.025 — ~2% of walking pace) is a confident falsehood about our own
/// motion, not a rendering nicety (#624).
///
/// `RUN_SPEED` (44 u/s, `eqoxide_core::physics::RUN_SPEED`) is this client's own controller cap,
/// which is the player-special-cased `eq_runspeed_float = 0.7`. Scaling is linear, so
/// `anim = speed_u_per_s * (0.7 * 40 / RUN_SPEED)` reproduces the native constants exactly:
/// `RUN_SPEED` (44 u/s) → 28, and the native walk speed (`RUN_SPEED * 0.3/0.7 ≈ 18.857 u/s`,
/// per #623) → 12. Rounds to nearest and clamps to the field's ±512 range (10-bit signed).
pub(crate) fn speed_to_wire_animation(speed_u_per_s: f32) -> i32 {
    const EQ_RUNSPEED_FLOAT_AT_RUN: f32 = 0.7; // EQEmu player special-case runspeed (mob.cpp:190-196)
    const ANIM_SCALE: f32 = 40.0; // EQEmu: base_runspeed = runspeed * 40
    let anim_f = speed_u_per_s * (EQ_RUNSPEED_FLOAT_AT_RUN * ANIM_SCALE / RUN_SPEED);
    anim_f.round().clamp(-512.0, 511.0) as i32
}

/// Build a RoF2 OP_FloatListThing payload: one `UpdateMovementEntry` (packed, 17 bytes) at the given
/// server position. `type = Collision` (1) is a normal move — it resets the server's movement-history
/// timer without tripping the TeleportA/ZoneLine special-cases in `ProcessMovementHistory`. Field
/// order matches EQEmu `UpdateMovementEntry`: Y(f32)@0, X(f32)@4, Z(f32)@8, type(u8)@12, ts(u32)@13.
pub fn build_movement_history(x: f32, y: f32, z: f32) -> Vec<u8> {
    const TYPE_COLLISION: u8 = 1; // UpdateMovementType::Collision — benign, skips teleport/zoneline checks
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0);
    let mut b = Vec::with_capacity(17);
    b.extend_from_slice(&y.to_le_bytes()); // Y @0 (server north)
    b.extend_from_slice(&x.to_le_bytes()); // X @4 (server east)
    // Z crosses the wire-datum boundary: callers pass the FOOT-level position, the wire carries
    // the model-origin datum (see `coord::WIRE_Z_OFFSET`, #522).
    b.extend_from_slice(&(z + eqoxide_core::coord::WIRE_Z_OFFSET).to_le_bytes()); // Z @8 (wire datum)
    b.push(TYPE_COLLISION);                // type @12
    b.extend_from_slice(&ts.to_le_bytes()); // timestamp @13
    b
}

/// Encode a RoF2 `PlayerPositionUpdateClient_Struct` (46 bytes) — the client's outbound
/// OP_ClientUpdate. Pure so the wire encoding is unit-testable (#522).
///
/// `pos`/`deltas` are `[east, north, up]` with `pos[2]` at the controller's **foot level**; the
/// z written to the wire is `pos[2] + coord::WIRE_Z_OFFSET`, because EQ's wire z is the MODEL-ORIGIN
/// datum ~3.1u above the feet (see `coord::WIRE_Z_OFFSET`). Broadcasting raw foot z made native
/// observers render this character's feet 3.1u below the floor — the #522 "clips through the
/// Kelethin plank" defect. Deltas are plain differences (the datum offset cancels), so they are
/// written unconverted.
///
/// Layout (rof2_structs.h):
///   0: sequence(u16)  2: spawn_id(u16)  4: vehicle_id(u16)=0
///   6: unknown[4]=0   10: delta_x(f32)  14: heading(u32 field, bits 0-11)
///  18: x_pos(f32)     22: delta_z(f32)  26: z_pos(f32)  30: y_pos(f32)
///  34: animation(u32 field, bits 0-9)   38: delta_y(f32)
///  42: delta_heading(u32 field, bits 0-9 signed) = 0
pub fn encode_client_position_update(
    seq: u16, spawn_id: u16, pos: [f32; 3], deltas: [f32; 3], eq_heading: u32, anim: i32,
) -> [u8; 46] {
    let mut buf = [0u8; 46];
    buf[0..2].copy_from_slice(&seq.to_le_bytes());        // sequence
    buf[2..4].copy_from_slice(&spawn_id.to_le_bytes());   // spawn_id
    // vehicle_id = 0 at [4..6], unknown[4] = 0 at [6..10] (already zeroed)
    buf[10..14].copy_from_slice(&deltas[0].to_le_bytes()); // delta_x
    buf[14..18].copy_from_slice(&eq_heading.to_le_bytes()); // heading (12-bit in u32)
    buf[18..22].copy_from_slice(&pos[0].to_le_bytes());   // x_pos (server east)
    buf[22..26].copy_from_slice(&deltas[2].to_le_bytes()); // delta_z
    let z_wire = pos[2] + eqoxide_core::coord::WIRE_Z_OFFSET;
    buf[26..30].copy_from_slice(&z_wire.to_le_bytes());   // z_pos (height, WIRE datum — #522)
    buf[30..34].copy_from_slice(&pos[1].to_le_bytes());   // y_pos (server north)
    buf[34..38].copy_from_slice(&(anim as u32).to_le_bytes()); // animation (10-bit in u32)
    buf[38..42].copy_from_slice(&deltas[1].to_le_bytes()); // delta_y
    // delta_heading at [42..46] = 0 (already zeroed)
    buf
}
/// A >12u jump in the network gs player position between ticks that we did NOT stream is a genuine
/// server correction (anti-cheat snap / teleport), handed to the render controller to apply.
const CORRECTION_SQ: f32 = 144.0;

/// Pending state of a quest turn-in (POST /give). The trade window spans multiple nav ticks:
/// we send OP_TradeRequest, then must wait for the server's OP_TradeRequestAck before moving the
/// item into the NPC trade slot. `ticks_waiting` counts nav ticks (~150ms each) for the timeout.
///
/// A3 Migration 2 (#448): the honest awaited give (POST /v1/interact/give) parks its HTTP-side
/// `oneshot::Sender` HERE — the existing state machine owns it, so there is no separate `pending_give`
/// field (cf. buy's `pending_buy`). `await_tx` is `None` for the fire-and-forget UI turn-in (no
/// result awaited) and `Some` for the awaited path.
///
/// SERIALIZED — at most ONE trade may be in flight (#475 review). OP_FinishTrade is a 0-byte packet
/// with NO trade/npc id, so — unlike buy, which correlates its echo on merchant_id+slot — a finish
/// cannot be matched to a specific give. The only way it is unambiguous is if exactly ONE give is in
/// flight when it arrives. So `give_state` is held from `begin_give` all the way through the finish (or
/// the finish-timeout) for BOTH paths — the fire-and-forget path does NOT clear at accept. `accepted`
/// splits phase 1 (awaiting OP_TradeRequestAck) from phase 2 (accept sent, awaiting OP_FinishTrade).
/// On finish: an awaited give resolves `Resolved(GiveOk)`, a fire-and-forget give clears silently
/// (no `await_tx`); a no-ack / no-finish timeout yields `Unconfirmed` (awaited) or a silent clear.
/// While a trade is in flight a NEW give is refused (awaited → `Refused`) or dropped (fire-and-forget)
/// — never allowed to start a second, racing trade that would make a later finish ambiguous. Clearing
/// at accept (the pre-review bug) let a late finish from a just-completed give resolve a DIFFERENT
/// give that had since reached phase 2 — a fabricated 200. The `Sender` deliberately lives ONLY here —
/// never in `GameState`, which is `Clone`d into the ArcSwap snapshot every tick and a `oneshot::Sender`
/// is not `Clone`. See `eqoxide_command::result` for the full flow.
struct GiveState {
    npc_id:        u32,
    ticks_waiting: u32,
    /// The parked HTTP `Sender` for the awaited path (#448); `None` for the fire-and-forget UI give.
    await_tx:      Option<tokio::sync::oneshot::Sender<eqoxide_command::CommandResult<eqoxide_command::GiveOk>>>,
    /// Item name captured from the inventory slot at send time, for the `GiveOk` receipt (the trade
    /// slots are cleared by the time OP_FinishTrade is applied, so it can't be read back then).
    item_name:     String,
    /// #486 (review): the item_id captured from the give slot at send time — the KEY the verify-transfer
    /// verdict uses (NOT the name). `None` when the item could not be identified at send time (the
    /// inventory mirror was desynced — a documented #275 condition — so the give slot held no known
    /// item): an unidentifiable give can NEVER be a confident success, so it resolves `Unconfirmed`.
    /// Keying the verdict on `item_id` + the cursor slot (below) is precise where a name-scan was not:
    /// a returned item comes back specifically to the CURSOR under its REAL id, and a same-named (or
    /// same-id) duplicate elsewhere in the pack is irrelevant to whether THIS give transferred.
    item_id:       Option<u32>,
    /// Phase flag: `false` = awaiting OP_TradeRequestAck (phase 1); `true` = OP_TradeAcceptClick sent,
    /// awaiting OP_FinishTrade (phase 2). Only the awaited path enters phase 2 — the fire-and-forget
    /// path clears `give_state` at accept, exactly as before.
    accepted:      bool,
    /// #486: OP_FinishTrade has been observed for this phase-2 give, but the verdict is DEFERRED to the
    /// next `tick_give` (which runs AFTER the gameplay drain loop, so the inventory mirror is settled)
    /// so the give can be VERIFIED instead of trusted. OP_FinishTrade only means the trade SESSION
    /// ended — a rejected / out-of-range NPC turn-in ALSO produces OP_FinishTrade but RETURNS the item
    /// to the player (cursor). `tick_give` then checks whether the item actually left inventory before
    /// resolving `Resolved(GiveOk)` (gone) vs `Unconfirmed` (still held → NOT a success, never a 200).
    finish_seen:   bool,
}

/// ~3 second ack timeout, in nav ticks (tick gating is ~150ms → 20 ticks ≈ 3s). Phase 1 (awaiting
/// OP_TradeRequestAck).
const GIVE_ACK_TIMEOUT_TICKS: u32 = 20;

/// ~3 second finish timeout, in nav ticks — phase 2 of the AWAITED give (#448): after
/// OP_TradeAcceptClick, how long we wait for OP_FinishTrade before declaring the outcome UNKNOWN
/// (`Unconfirmed`). An item-mismatch turn-in returns the item on the cursor with NO OP_FinishTrade,
/// so this timeout is the honest soft-fail for that case. The two net-side timeouts run in sequence,
/// so the worst-case net verdict lands ≈ (GIVE_ACK_TIMEOUT_TICKS + GIVE_FINISH_TIMEOUT_TICKS) × 150ms
/// ≈ 6s after the request; the HTTP-side timeout (`GIVE_HTTP_TIMEOUT_SECS`, in `http::interact`) is
/// set GREATER than that so the NET verdict (Resolved/Unconfirmed) is what the caller receives,
/// never a vaguer HTTP-elapsed 202. See the two-timeout ordering note in `http::interact::post_give`.
const GIVE_FINISH_TIMEOUT_TICKS: u32 = 20;

/// #486 — the "returned-item watch window", in nav ticks (~150ms each → ~300ms). After OP_FinishTrade
/// is SEEN we do NOT judge the give immediately: EQEmu queues the 0-byte OP_FinishTrade FIRST, then
/// `FinishTrade`→`PushItemOnCursor` returns any un-accepted item to the cursor via a SEPARATE
/// OP_ItemPacket sent STRICTLY AFTER the finish (verified in the server source:
/// zone/client_packet.cpp:15488 queues the finish before FinishTrade runs). So the return can land in a
/// later rx-drain than the finish; we wait this settle window (each `tick_give` runs AFTER the full
/// gameplay drain, so a couple of cadences guarantees the trailing return-item packet has been applied
/// to the inventory mirror) before verifying whether the item actually left. Kept small — a give is
/// rare and ~300ms is negligible; correctness (never a fabricated 200) beats latency.
const GIVE_FINISH_SETTLE_TICKS: u32 = 2;

/// #492 — time-based reap deadline for the parked awaited merchant BUY and OPEN slots. Once a
/// `pending_buy`/`pending_open` `sent_at` has exceeded this, `reap_expired_pending` (swept every
/// `tick`) drops it so a subsequent same-type command is ADMITTED instead of 409-blocked by the
/// in-flight singleton guard. It MUST be `>=` the HTTP-side timeout the caller already gave up on —
/// both `/v1/merchant/open` and `/v1/merchant/buy` use `tokio::time::timeout(Duration::from_secs(4)`
/// in `http::merchant` — so a still-waiting caller's GENUINE echo can still fulfil the slot before
/// the reap. The +2s margin keeps the reap strictly after the HTTP timeout, never racing a caller
/// that is a hair from resolving. (Buy and open share the same 4s HTTP timeout, hence one const.)
const SHOP_PENDING_REAP: Duration = Duration::from_secs(6);

/// #492 — same, for the parked awaited self-CAST slot. Its HTTP-side timeout is
/// `CAST_HTTP_TIMEOUT_SECS` (12s) in `http::combat` (sized to exceed the longest RoF2 cast), so the
/// reap deadline is 12 + 2 = 14s: `>=` the HTTP timeout so a genuine `last_cast` transition still
/// fulfils first, +2s so the reap lands strictly after the caller has already received its 202.
const CAST_PENDING_REAP: Duration = Duration::from_secs(14);

/// #492/#475 — how long a TIME-REAPED command's correlation key stays quarantined against a stale
/// echo. The wire echoes (OP_ShopPlayerBuy / OP_ShopRequest / a cast's `last_cast` transition) carry
/// NO per-request token, so echo→pending correlation was only safe while at most ONE command of that
/// type was ever outstanding — a fact the OLD "cleared only by the matching echo or a session-ending
/// zone change" design guaranteed (#475). The time-based reap adds a THIRD, SAME-connection clearing
/// path that does NOT foreclose delivery of the original request's echo: the transport tolerates a
/// delayed-but-alive reliable echo right up to the server's 30s `resend_timeout` (transport.rs:
/// `RESEND_*` / the resend_timeout note — the server drops the session at 30s of an un-ACKed oldest
/// packet), FAR beyond the 6s/14s reap. So a reaped command's echo can still arrive and, without this
/// guard, be mis-credited to a newly-admitted SAME-KEY command (a silent wrong `Resolved` — the
/// honesty regression the reviewer caught). We quarantine the reaped key for the FULL resend window;
/// while it is live, `fulfill_*` DROPS any matching echo (it might belong to the reaped command),
/// so a re-admitted same-key command can never be echo-resolved and instead reaps to an honest
/// `Unconfirmed`/202. Sourced from `transport.rs` (30s resend_timeout), NOT invented.
const ECHO_QUARANTINE: Duration = Duration::from_secs(30);

/// A merchant buy sent via the honest awaited path (A3 Migration 1, #448), parked here until its
/// resolving packet lands. Holds the `oneshot::Sender` HTTP is awaiting plus the merchant/slot the
/// buy was for, so a fulfil can CORRELATE the OP_ShopPlayerBuy echo (rejecting a stray shop echo
/// for a different buy) before it `send`s `Resolved`. `sent_at` is the park time — the authoritative
/// timeout is still HTTP-side, but `reap_expired_pending` (#492) uses it to drop a slot the server
/// never resolved (`SHOP_PENDING_REAP`) so a later buy isn't stranded behind it. The `Sender`
/// deliberately lives ONLY here — never in `GameState`, which is `Clone`d into the ArcSwap snapshot
/// every tick and a `oneshot::Sender` is not `Clone`. See `eqoxide_command::result`.
struct PendingBuy {
    tx:          tokio::sync::oneshot::Sender<eqoxide_command::CommandResult<eqoxide_command::BuyOk>>,
    merchant_id: u32,
    slot:        u32,
    /// Park time — drives the #492 `reap_expired_pending` sweep (`SHOP_PENDING_REAP`).
    sent_at:     Instant,
}

/// An awaited merchant open (eqoxide#479), parked between send and the resolving OP_ShopRequest
/// echo. Holds the `oneshot::Sender` HTTP is awaiting plus the `merchant_id` the open was for, so a
/// fulfil can CORRELATE the echo's npc_id (rejecting a stray shop-open echo for a DIFFERENT
/// merchant) before it `send`s `Resolved`/`Refused`. `sent_at` is the park time; the authoritative
/// timeout is HTTP-side, but `reap_expired_pending` (#492) uses it to drop a slot the server never
/// resolved (`SHOP_PENDING_REAP`) — the non-merchant/out-of-range open sends NO echo at all, so
/// without the reap it would strand this `Sender` and 409-block every later open until a zone
/// change. The `Sender` deliberately lives ONLY here — never in `GameState`, which is `Clone`d into
/// the ArcSwap snapshot every tick and a `oneshot::Sender` is not `Clone`. See
/// `eqoxide_command::result` for the full flow.
struct PendingOpen {
    tx:          tokio::sync::oneshot::Sender<eqoxide_command::CommandResult<eqoxide_command::OpenOk>>,
    merchant_id: u32,
    /// Park time — drives the #492 `reap_expired_pending` sweep (`SHOP_PENDING_REAP`).
    sent_at:     Instant,
}

/// A self-cast sent via the honest awaited path (A3 Migration 3, #448), parked here until the cast
/// machinery computes its outcome into `gs.last_cast`. Casting is naturally serial (one cast bar) so
/// this is a SINGLETON — a second awaited cast while one is parked is `Refused` (the singleton
/// discipline; see `drain_cast`). `sent_at` is the correlation key: the cast's terminal outcome is
/// the one whose `CastOutcome::at` is strictly AFTER we parked (a stale prior outcome carries an
/// earlier `at`, and `begin_cast` clears `last_cast` to `None` on our OP_BeginCast echo), so
/// `fulfill_cast` fires on the `last_cast` TRANSITION rather than on any single opcode — the 3-opcode
/// cast-end path is de-duped in `GameState`, so keying one opcode would double-fire or miss. The
/// `Sender` lives ONLY here, never in `GameState` (it is `Clone`d into the ArcSwap snapshot every
/// tick and a `oneshot::Sender` is not `Clone`). See `eqoxide_command::result` for the flow.
struct PendingCast {
    tx:      tokio::sync::oneshot::Sender<eqoxide_command::CommandResult<eqoxide_command::CastEnd>>,
    sent_at: Instant,
}

/// The result of emitting a cast on the wire — did the cast actually START, or was it refused before
/// any packet went out? Shared by the fire-and-forget UI cast path (which ignores it — the outcome is
/// already logged via `finish_cast`) and the honest awaited path (#448), which maps `NeverStarted`
/// straight to a `Refused` (the cast DEFINITIVELY did not happen) and parks a `PendingCast` only on
/// `Started`.
enum CastSend {
    /// OP_CastSpell was emitted — the cast is genuinely in flight; await its `last_cast` transition.
    Started,
    /// Refused before any packet (empty gem, or a stale/non-clicky item slot). `finish_cast(0,
    /// "cast_failed", …)` was already recorded; the `String` is the human reason for the 409.
    NeverStarted(String),
}

/// Emit the OP_CastSpell for a [`CastRequest`] and report whether the cast STARTED (see [`CastSend`]).
/// Shared by the fire-and-forget UI cast path and the honest awaited path (#448) so both emit
/// byte-identical wire traffic and identical `finish_cast` bookkeeping — the awaited path adds ONLY
/// the parked `Sender`, no behavior change to the wire. Target priority is unchanged: explicit API
/// target > current target > self, with ST_SELF spells always self-targeted and un-aimed beneficial
/// spells self-cast (eqoxide#95). A never-started refusal records `finish_cast(cast_failed)` here —
/// the same honest signal the fire-and-forget path relied on (#348) — but ONLY when
/// `record_never_started` is set.
///
/// `record_never_started` closes a false-Refused residual (#448 review): `finish_cast(cast_failed)`
/// is a CLIENT-SIDE write to `gs.last_cast` (it never touches the server), so a UI fire-and-forget
/// never-started cast, fired while an AWAITED cast is parked, would advance `last_cast` to
/// `cast_failed` with a fresh `at` and let the next `fulfill_cast` resolve the AWAITED cast as a bogus
/// `Refused` (with an unrelated "gem empty" reason). So the caller passes `false` whenever that write
/// could reach a parked awaited cast (the awaited path itself, which reports via its `Sender` and does
/// not need the write; and the UI path while a cast is parked). Safe-direction either way — it can
/// never fabricate a success — but suppressing the stray write keeps the awaited result honest.
fn send_cast(stream: &mut EqStream, gs: &mut GameState, req: eqoxide_ipc::CastRequest, record_never_started: bool) -> CastSend {
    if let Some(item_slot) = req.item_slot {
        // Item "clicky" cast (teleport ring / port potion, etc.). Resolve the click spell from the
        // item currently at that wire slot and refuse if it isn't a clicky, so a stale slot can't fire
        // an unrelated cast. Target: explicit > current > self. (eqoxide#193)
        let click = gs.inventory.iter().find(|i| i.slot == item_slot as i32)
            .map(|i| i.click_spell_id).unwrap_or(0);
        if click == 0 {
            // POST /v1/combat/cast validated the slot, but the item can move/vanish between the
            // handler and this drain. Dropping it with only a tracing line meant the agent saw 200 and
            // then nothing at all — report the failure where the agent can read it (#348).
            let text = format!("Cannot cast: no clickable item in slot {item_slot}.");
            if record_never_started { gs.finish_cast(0, "cast_failed", &text); }
            tracing::info!("EQ: item cast slot={} ignored — no clicky item at that slot", item_slot);
            return CastSend::NeverStarted(text);
        }
        let target = req.target_id.filter(|&t| t != 0)
            .or(gs.target_id.filter(|&t| t != 0))
            .unwrap_or(gs.player_id);
        stream.send_app_packet(OP_CAST_SPELL, &build_item_cast_packet(item_slot, click, target));
        tracing::info!("EQ: item cast slot={} spell={} target={}", item_slot, click, target);
        return CastSend::Started;
    }
    let spell_id = gs.mem_spells.get(req.gem as usize).copied()
        .unwrap_or(eqoxide_core::game_state::EMPTY_GEM);
    if eqoxide_core::game_state::gem_is_empty(spell_id) {
        // POST /v1/combat/cast now 409s on an empty gem, but the gem can be un-memorized between the
        // handler and this drain. This arm used to be a bare `tracing::info!` — the agent got 200 and
        // then ABSOLUTE SILENCE: no packet, no message, no event, no state change, indistinguishable
        // from a cast still in flight. (#348)
        let text = format!("Cannot cast: spell gem {} is empty.", req.gem);
        if record_never_started { gs.finish_cast(0, "cast_failed", &text); }
        tracing::info!("EQ: cast gem={} ignored — empty gem", req.gem);
        return CastSend::NeverStarted(text);
    }
    let explicit = req.target_id.filter(|&t| t != 0);
    let current  = gs.target_id.filter(|&t| t != 0);
    let mut target = explicit.or(current).unwrap_or(gs.player_id);
    if let Some(db) = eqoxide_core::spells::global() {
        if db.is_self_only(spell_id) {
            target = gs.player_id; // ST_SELF: always the caster
        } else if explicit.is_none() && db.is_beneficial(spell_id) {
            // Keep an explicitly-chosen friendly (PC) target for group heals; otherwise (no target,
            // cleared, or a hostile NPC) land the buff/heal on ourselves.
            let friendly = target == gs.player_id
                || gs.world.entities.get(&target).map_or(false, |e| !e.is_npc);
            if !friendly { target = gs.player_id; }
        }
    }
    stream.send_app_packet(OP_CAST_SPELL, &build_cast_packet(req.gem as u32, spell_id, target));
    tracing::info!("EQ: cast gem={} spell={} target={}", req.gem, spell_id, target);
    CastSend::Started
}

/// Emit the OP_ShopRequest (open) + OP_ShopPlayerBuy (buy `slot`, qty 1) pair and mark the buy
/// in flight (`begin_shop_open_for` + `begin_shop_buy`). Shared by the fire-and-forget UI buy path
/// and the honest awaited buy path (#448) so both emit byte-identical wire traffic and identical
/// in-flight/coin-unverified bookkeeping — the awaited path adds ONLY the parked Sender, no
/// behavior change to the wire. See `drain_merchant` for the surrounding #345/#360/#361 rationale.
fn send_shop_buy(stream: &mut EqStream, gs: &mut GameState, merchant_id: u32, slot: u32) {
    // #360/#361: clear a DIFFERENT stale merchant (but don't flicker an already-open one closed) and
    // mark coin unverified until a real OP_PlayerProfile reconciles this buy — a silent
    // inventory-full/LORE refusal sends no echo at all.
    gs.begin_shop_open_for(merchant_id);
    gs.begin_shop_buy();
    let open = merchant_click(merchant_id, gs.player_id, 1);
    stream.send_app_packet(OP_SHOP_REQUEST, &open);
    // RoF2 Merchant_Sell_Struct (32b): npcid@0, playerid@4, itemslot@8, unknown12@12, quantity@16,
    // unknown20@20, price@24, unknown28@28. (The RoF2 server DECODEs an exact 32 bytes, so a short
    // packet is silently dropped.)
    let mut buy = [0u8; 32];
    buy[0..4].copy_from_slice(&merchant_id.to_le_bytes());
    buy[4..8].copy_from_slice(&gs.player_id.to_le_bytes());
    buy[8..12].copy_from_slice(&slot.to_le_bytes());
    buy[16..20].copy_from_slice(&1u32.to_le_bytes()); // quantity = 1 (server sets the price)
    stream.send_app_packet(OP_SHOP_PLAYER_BUY, &buy);
    tracing::info!("EQ: shop buy sent — merchant_id={} slot={} qty=1", merchant_id, slot);
}

// Nav steering math (consts, replan/arrival decisions, pure-pursuit carrot, fast-steering cursor)
// moved to `eqoxide_nav::steering` (cleanup step 2 — nav must not live inside net); the walker
// methods that used them live in `eqoxide_nav::walker::Walker` now too (M1 extraction), so this
// module only needs `eq_heading` for its own remaining melee-approach/position-packet code below
// (the tests module still exercises a couple of `nav::steering` consts directly — see its own
// `use`).
use eqoxide_core::coord::eq_heading;


pub struct ActionLoop {
    /// `/v1/move/*` slots (#M4 — see `ipc::NavSlots`). Production reads/writes were migrated onto
    /// `self.command.{request_goto,request_follow,request_stop,request_cancel_goto,take_zone_cross}`
    /// (#459) — this field's only remaining production use is as the source `.clone()`d into
    /// `Walker::new(...)` below (a move-out, which the dead_code lint doesn't count as a "read").
    /// It stays a field (rather than a local dropped after construction) because `Walker` keeps its
    /// OWN clone privately (`nav::walker::Walker::nav` is not even `pub(crate)`), and several unit
    /// tests below (`dead_player_halts_navigation`, `zone_change_resets_stale_destination_and_path`,
    /// `a_snapped_goal_z_is_reported_not_silently_performed`, `nav_tier_does_not_survive_...`) need a
    /// handle on the exact same shared Arc to seed/assert `nav_state`/`goto_target`/`goto_entity`
    /// directly. (The walker's published diagnostics moved to `eqoxide_nav::diagnostics::NavDebugView`
    /// in #608; tests reach it via `walker.debug_view()`.)
    /// `#[cfg(test)]` code doesn't compile under `cargo build --lib`, so the
    /// lint can't see those reads and flags the field as dead outside `cargo test` — hence the allow.
    #[allow(dead_code)]
    nav:              eqoxide_ipc::NavSlots,
    /// The live entity registry + zone exit points (#M4 — see `ipc::WorldSlots`).
    world:            eqoxide_ipc::WorldSlots,
    /// `/v1/quests/*` slots (#M4 — see `ipc::QuestSlots`).
    quest:            eqoxide_ipc::QuestSlots,
    /// `/v1/group/*` slots (#M4 — see `ipc::GroupSlots`).
    group_slots:      eqoxide_ipc::GroupSlots,
    /// The typed write-path facade (#446). Combat is fully migrated onto it — this thread drains
    /// combat commands via `self.command.take_*` (no direct `ipc::CombatSlots` field any more);
    /// other domains still use their own bundle fields until Wave-2 migrates them. See
    /// `eqoxide_command`.
    command:          eqoxide_command::CommandState,
    /// GET /v1/observe/who registers a oneshot here; drained in `tick` to send OP_WhoAllRequest.
    /// Client-local friends list + a pending friends-presence poll mirror the same shape (#300/#301,
    /// #M4 — see `ipc::SocialSlots`).
    social:           eqoxide_ipc::SocialSlots,
    /// Held between sending the `/who` request and receiving OP_WhoAllResponse; fired by
    /// `fulfill_who`. (#300)
    pending_who:      Option<tokio::sync::oneshot::Sender<Vec<eqoxide_core::game_state::WhoEntry>>>,
    /// The OP_FriendsWho reply arrives on the SAME opcode as /who all (OP_WhoAllResponse), so
    /// `expecting_friends` records that the next such reply is a friends poll, not a /who all. (#301)
    pending_friends:  Option<tokio::sync::oneshot::Sender<Vec<eqoxide_core::game_state::WhoEntry>>>,
    expecting_friends: bool,
    /// `/v1/merchant/*` slots (#M4 — see `ipc::MerchantSlots`).
    merchant_slots:   eqoxide_ipc::MerchantSlots,
    /// A buy sent via the honest awaited path (A3 Migration 1, #448), parked between send and its
    /// resolving packet (OP_ShopPlayerBuy echo → `Resolved`, OP_ShopEndConfirm → `Refused`), or
    /// reaped as `Unconfirmed` on a zone change. Sibling of `pending_who`. See `PendingBuy`.
    pending_buy:      Option<PendingBuy>,
    /// A merchant open sent via the honest awaited path (eqoxide#479), parked between send and its
    /// resolving OP_ShopRequest echo (`command==1` → `Resolved`, `command==0` → `Refused`), or
    /// reaped as `Unconfirmed` on a zone change. A non-merchant/out-of-range target sends NO echo at
    /// all, so that path resolves purely via the HTTP timeout. See `PendingOpen`.
    pending_open:     Option<PendingOpen>,
    /// A self-cast sent via the honest awaited path (A3 Migration 3, #448), parked between send and
    /// the `gs.last_cast` outcome transition (→ `Resolved(CastEnd)` / `Refused` / `Unconfirmed`), or
    /// reaped as `Unconfirmed` on a zone change. Singleton — one self-cast at a time. See `PendingCast`.
    pending_cast:     Option<PendingCast>,
    /// #492/#475 — correlation keys of buys the TIME-BASED reap dropped, each with its quarantine
    /// expiry (`ECHO_QUARANTINE`). While an entry is live, `fulfill_buy_ok` DROPS any OP_ShopPlayerBuy
    /// echo matching `(merchant_id, slot)` — it could be the delayed echo of the reaped buy (the
    /// transport delivers up to ~30s), so crediting it to a re-admitted same-key buy would be a silent
    /// wrong `Resolved`. Zone-change reaps are NOT quarantined (the crossing ends the session, so no
    /// stale echo can follow). Pruned lazily each `reap_expired_pending`.
    reaped_buy_keys:  Vec<((u32, u32), Instant)>,
    /// #492/#475 — same, for reaped opens (key = `merchant_id`; `fulfill_open` correlates on npc_id).
    reaped_open_keys: Vec<(u32, Instant)>,
    /// #492/#475 — cast has NO content correlation key (`fulfill_cast` keys only on `outcome.at`), so
    /// once a cast is time-reaped ANY later cast outcome within the resend window could belong to it.
    /// This suppresses ALL cast echo-fulfillment until the instant recorded here, so a re-admitted
    /// cast reaps to an honest `Unconfirmed` rather than absorbing an unrelated spell's outcome.
    cast_quarantine_until: Option<Instant>,
    /// `/v1/inventory/*` slots (#M4 — see `ipc::InventorySlots`).
    inventory_slots:  eqoxide_ipc::InventorySlots,
    /// In-progress quest turn-in (POST /give), or None when idle. Drives the trade-window
    /// state machine across nav ticks (request → wait for ack → move item + accept).
    give_state:       Option<GiveState>,
    /// `/v1/interact/*` slots — hail, say, loot, give, doors, sit/stand, dialogue, read (#M4 — see
    /// `ipc::InteractSlots`).
    interact:         eqoxide_ipc::InteractSlots,
    /// Outgoing chat + async events + the message log (#M4 — see `ipc::ChatSlots`).
    chat:             eqoxide_ipc::ChatSlots,
    collision:        eqoxide_nav::collision::SharedCollision,
    maps_dir:         std::path::PathBuf,
    current_zone:     String,
    last_zone_cross:  Instant,
    /// One-shot latch for the gated-refusal message (#683 review F1): the zone-line region index
    /// whose `UnresolvedCross::Ignore` refusal has already been reported to the message log.
    /// Cleared when the standing probe finds no region (the character left the line), so re-entry
    /// reports again. Without this the refusal was a `tracing::debug!` only — an agent standing on
    /// a `zone_id: null` exit in a gated zone waited forever with zero feedback.
    gated_cross_reported: Option<i32>,
    /// Set when the auto-cross fires OP_ZoneChange for a SAME-ZONE DRNTP line (an intra-zone
    /// translocator whose zone-point target is the current zone — legitimate retail content, e.g.
    /// the 5 qeynos2 teleport pads). The server answers such a zoneID=0 request with a lightweight
    /// in-zone reposition (`DoZoneSuccess`, `zoning.cpp:536`) and a `success=1` echo naming the
    /// CURRENT zone — it does NOT tear down the zone session. The receive side must therefore NOT
    /// run a world reconnect for that echo (doing so reconnects against a live zone and wedges,
    /// #368). This timestamp lets the OP_ZONE_CHANGE echo handler tell a same-zone reposition (skip
    /// reconnect) from a genuine cross-zone change or a death/bind respawn (reconnect as normal) —
    /// the echo's zone id alone can't, since the server names the current zone in the reposition
    /// case too. `None` once consumed or never set.
    same_zone_cross_at: Option<Instant>,
    position_seq:     u16,
    last_tick:        Instant,
    /// Whether auto-attack is currently engaged (set by the /attack toggle). While true and a
    /// target is set, the nav thread keeps the player facing the target so melee swings land.
    auto_attack:      bool,
    /// The path-walker (M1 extraction, #eq-dev-process) — the `/goto` route, stall/backoff/
    /// oscillation recovery, and arrival. Holds its OWN clones of `nav`/`world`/`collision` (the
    /// same shared state as this struct's own fields, not a copy of it) plus the pathfinding
    /// workers, which it owns exclusively. See `eqoxide_nav::walker` for the intent-only movement
    /// boundary: `Walker` writes ONLY `controller.nav_intent`, never a position or the controller.
    walker:           eqoxide_nav::walker::Walker,
    /// The zone terrain+collision LOAD STATE (#579), the SAME shared handle the walker and the HTTP
    /// surface hold. `drain_zone_cross` consults it through `zone_assets::usable_collision` (#600,
    /// #827 — the verdict half is still `zone_assets::usability`, which that call delegates to) so
    /// its collision-derived refusal is the honest transient `zone_loading` while the assets are
    /// not usable — never a definitive `no_path` an agent would read as "give up permanently" —
    /// and so the grid it then reads is the one that verdict blessed, not a separate slot's.
    zone_assets:      eqoxide_nav::zone_assets::ZoneAssetStateShared,
    /// The spawn id the pet was last ordered to attack (avoids re-spamming OP_PetCommands every
    /// tick). Reset when the target changes; see the auto-pet-combat block.
    last_pet_target:  Option<u32>,
    /// Single-authority controller integration (design §2): `controller_view` is the render
    /// thread's authoritative position snapshot we stream to the server; `nav_intent` is the
    /// `/goto` planner's per-frame wish written for the render controller; `pos_correction` hands a
    /// genuine server correction back to the controller (#M4 — see `ipc::ControllerSlots`).
    controller:       eqoxide_ipc::ControllerSlots,
    /// `/v1/guild/*` slots (#M4 — see `ipc::GuildSlots`).
    guild_slots:      eqoxide_ipc::GuildSlots,
    /// Last time we sent OP_FloatListThing (movement history) — the anti-MQGhost keepalive (#105).
    last_movement_history_send: Instant,
    /// Last position we streamed, and the last-send timestamp (for the 280 ms / 1300 ms cadence).
    /// NOTE (#624 review): `last_streamed` is mirrored on EVERY tick (~10ms, see the bottom of
    /// `stream_position`), not just on an actual send — it exists to detect "did the network
    /// position move THIS tick" (the `moved` throttle test) and "did something outside this
    /// function move `gs.player_*`" (the correction test). Neither of those is the right anchor
    /// for a per-SEND delta/speed calculation, which is why `last_sent_pos` (below) exists
    /// separately: it is the position as of the last packet actually PUT ON THE WIRE, updated
    /// in lockstep with `last_pos_send` and nowhere else. A prior version of this fix reused
    /// `gs.player_x/y/z` (itself mirrored every tick, same problem as `last_streamed`) as the
    /// "from" position for the speed calc — so at a throttled send ~280ms after the last one, the
    /// delta only ever covered the most recent ~10ms tick while `last_pos_send.elapsed()` measured
    /// the full ~280ms, silently flooring every sustained run's reported speed back down near the
    /// walking constant this issue exists to remove. `last_sent_pos` and `last_pos_send` are always
    /// updated together so the numerator and denominator of the speed calc share one window.
    last_streamed:    [f32; 3],
    last_pos_send:    Instant,
    /// Position as of the last packet actually sent by `send_position_update`'s throttled callers
    /// (`stream_position`'s normal + correction paths) — see the note on `last_streamed` above.
    /// The melee-facing call in `drive_auto_engage_melee` deliberately does NOT touch this: it
    /// passes its own `from == to` (an explicit stationary facing update), so it never needs, and
    /// must never perturb, the cadence baseline the throttled path relies on.
    last_sent_pos:    [f32; 3],
    streamed_init:    bool,
}

/// The single, server-authoritative disposition of an inbound OP_ZoneChange `success` echo.
/// One echo maps to EXACTLY one of these — see [`classify_zone_change_echo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZoneChangeEcho {
    /// `success != 1` — the server rejected/ignored the request. Do nothing.
    Ignored,
    /// A SAME-ZONE in-zone reposition (an intra-zone translocator's lightweight `DoZoneSuccess`):
    /// the zone session was NOT torn down, so the receive side must NOT world-reconnect (#368).
    SameZoneReposition,
    /// A genuine zone handoff — reconnect to world for the new zone. Covers a real cross-zone
    /// line, a GM `#zone`, a death/bind respawn, AND (the #554 fix) a translocator the SERVER
    /// resolved to a DIFFERENT zone than the client locally guessed.
    CrossZoneReconnect,
}

/// Classify an OP_ZoneChange server echo into its ONE disposition — the whole same-zone-vs-cross
/// decision, as a single total function of the echo. **Server-authoritative** (#554): the client's
/// local pre-resolution of a translocator's destination is NOT trusted here; only the server's
/// echoed `zone_id` (against the still-current zone) plus the "I just fired a same-zone
/// translocator" pending flag decide.
///
/// - `SameZoneReposition` requires BOTH the server to echo the CURRENT zone AND a same-zone
///   translocator to be pending. A translocator the server resolved to a different zone (#554
///   qeynos2 vault: `echo=1`, `current=2`) fails the `echo == current` test and reconnects — this
///   is the core fix for the bounce.
/// - A death/bind respawn also echoes the current zone but sets NO pending flag, so it correctly
///   reconnects (full re-entry), not repositions.
///
/// Because the disposition is a pure function of the echo (and `same_zone_pending` is read
/// non-consuming, see [`ActionLoop::same_zone_reposition_pending`]), a duplicate / retransmitted
/// echo classifies IDENTICALLY. That makes the #554 double-cross — first echo → reposition,
/// duplicate echo → reconnect, so the char did BOTH and bounced — structurally unrepresentable.
pub(crate) fn classify_zone_change_echo(
    success: i32,
    echo_zone_id: u16,
    current_zone_id: u16,
    same_zone_pending: bool,
) -> ZoneChangeEcho {
    if success != 1 {
        ZoneChangeEcho::Ignored
    } else if echo_zone_id == current_zone_id && same_zone_pending {
        ZoneChangeEcho::SameZoneReposition
    } else {
        ZoneChangeEcho::CrossZoneReconnect
    }
}

// `UnresolvedCross` / `classify_unresolved_cross` — the #679/#683 gate — MOVED to
// `eqoxide_core::zone_cross` by #713, unchanged (same body, same gates, same two tests). It had to
// become reachable from `eqoxide-http` so `/v1/observe/zone_exits` can REPORT the verdict on a
// `zone_id: null` exit (#713 item 3) instead of an agent discovering the refusal from the message
// log after it has already walked there. Both callers now run the SAME function: a reporter with
// its own copy of the rule is two rules that can drift, and a drifted `gated` flag is exactly the
// undetectable falsehood the agent-honesty invariant forbids. See that module for the full
// rationale and the scope of the property-tested guarantee.
use eqoxide_core::zone_cross::{
    classify_unresolved_cross, CrossAttempts, UnresolvedCross, ZoneCrossPlan, ZoneCrossResolution,
};

impl ActionLoop {
    /// Takes the M4 domain bundles (see `ipc.rs`) rather than ~59 flat slot params. Each bundle
    /// passed here MUST be a `.clone()` of the SAME bundle `main.rs` also hands to `HttpState` —
    /// that shared-Arc identity (not a fresh `Default::default()` bundle) is what keeps this the
    /// same cross-thread channel the HTTP/agent side writes into. See `ipc.rs` module docs.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        nav:             eqoxide_ipc::NavSlots,
        world:           eqoxide_ipc::WorldSlots,
        quest:           eqoxide_ipc::QuestSlots,
        group_slots:     eqoxide_ipc::GroupSlots,
        command:         eqoxide_command::CommandState,
        social:          eqoxide_ipc::SocialSlots,
        merchant_slots:  eqoxide_ipc::MerchantSlots,
        inventory_slots: eqoxide_ipc::InventorySlots,
        interact:        eqoxide_ipc::InteractSlots,
        chat:            eqoxide_ipc::ChatSlots,
        controller:      eqoxide_ipc::ControllerSlots,
        guild_slots:     eqoxide_ipc::GuildSlots,
        collision:       eqoxide_nav::collision::SharedCollision,
        maps_dir:        std::path::PathBuf,
        // The published nav diagnostics view (#608): a `.clone()` of the SAME slot `main.rs` hands
        // to the render + HTTP consumers. The Walker is its only writer.
        nav_debug:       eqoxide_nav::diagnostics::NavDebugView,
        // The zone terrain+collision LOAD STATE (#579), the SAME shared handle as the HTTP surface's.
        // The Walker consults it through `zone_assets::usability` for the #600 zone-identity gate.
        zone_assets:     eqoxide_nav::zone_assets::ZoneAssetStateShared,
    ) -> Self {
        let walker = eqoxide_nav::walker::Walker::new(
            nav.clone(), world.clone(), collision.clone(), controller.nav_intent.clone(),
            nav_debug, zone_assets.clone(),
        );
        ActionLoop {
            nav,
            world,
            quest,
            group_slots,
            command,
            social,
            pending_who: None,
            pending_friends: None,
            expecting_friends: false,
            merchant_slots,
            pending_buy: None,
            pending_open: None,
            pending_cast: None,
            reaped_buy_keys: Vec::new(),
            reaped_open_keys: Vec::new(),
            cast_quarantine_until: None,
            inventory_slots,
            give_state: None,
            interact,
            chat,
            collision,
            maps_dir,
            current_zone: String::new(),
            last_zone_cross: Instant::now(),
            gated_cross_reported: None,
            same_zone_cross_at: None,
            position_seq: 0,
            last_tick: Instant::now(),
            auto_attack: false,
            walker,
            zone_assets,
            last_pet_target: None,
            controller,
            guild_slots,
            last_streamed: [0.0, 0.0, 0.0],
            last_pos_send: Instant::now(),
            last_sent_pos: [0.0, 0.0, 0.0],
            last_movement_history_send: Instant::now(),
            streamed_init: false,
        }
    }

    /// Copy all entity positions from `gs` into the shared entity map
    /// (used by the HTTP /entities endpoint and /goto by-name lookup).
    pub fn sync_entities(&self, gs: &GameState) {
        // #643: positions, ids AND pose/gait are published together by `WorldSlots::publish_entities`
        // — the single roster writer, which owns the "all three maps always agree" invariant and the
        // canonical lock order. Do NOT hand-roll the three inserts here again: that is exactly how
        // the login-path seed silently stopped publishing poses. See that method's doc comment.
        self.world.publish_entities(&gs.world.entities);
    }

    /// Publish the native Task-system quest log from `gs` into the shared slot (GET /quests/log).
    pub fn sync_tasks(&self, gs: &GameState) {
        let mut log = self.quest.task_log.lock().unwrap();
        log.clear();
        let mut tasks: Vec<_> = gs.tasks.values().cloned().collect();
        tasks.sort_by_key(|t| t.task_id);
        log.extend(tasks);
        drop(log);

        let mut offers = self.quest.task_offers_shared.lock().unwrap();
        offers.clear();
        offers.extend(gs.task_offers.iter().cloned());
        drop(offers);

        let mut completed = self.quest.completed_tasks_shared.lock().unwrap();
        completed.clear();
        completed.extend(gs.completed_task_history.iter().cloned());
    }

    /// Publish the group roster from `gs` into the shared slot (GET /v1/group/roster + the UI
    /// roster panel). Looks up each other member's HP% from `gs.world.entities` by name (group
    /// membership is what unlocks receiving another mob's OP_MobHealth percent, so this reuses
    /// existing Entity.hp_pct rather than needing a new opcode); the player's own HP% comes
    /// directly from `gs.hp_pct` since the player is never in `gs.world.entities`.
    pub fn sync_group(&self, gs: &GameState) {
        let mut g = self.group_slots.group.lock().unwrap();
        g.leader = gs.group_leader.clone();
        g.pending_invite = gs.pending_invite.clone();
        g.you_are_leader = !gs.player_name.is_empty() && gs.group_leader == gs.player_name;
        g.members = gs.group_members.iter().map(|m| {
            let hp_pct = if m.name == gs.player_name {
                gs.hp_pct
            } else {
                gs.world.entities.values().find(|e| e.name == m.name).map(|e| e.hp_pct).unwrap_or(0.0)
            };
            eqoxide_ipc::GroupMemberView {
                // m.level from OP_GroupUpdateB is a server placeholder (70/65); resolve the real
                // level from our profile / the member's spawn instead. (eqoxide#104)
                name: m.name.clone(), level: gs.group_member_level(&m.name),
                is_leader: m.is_leader, is_merc: m.is_merc,
                tank: m.tank, assist: m.assist, puller: m.puller, offline: m.offline, hp_pct,
            }
        }).collect();
    }

    /// Publish the player's guild identity + roster from `gs` into the shared slot (GET
    /// /v1/guild/roster and the guild fields of /observe/debug). Resolves guild_id → name via the
    /// OP_GuildsList table. (#295)
    pub fn sync_guild(&self, gs: &GameState) {
        let mut g = self.guild_slots.guild.lock().unwrap();
        // GUILD_NONE is 0xFFFFFFFF (and 0 also means none). Normalize both to 0 so the API cleanly
        // reports "no guild" as guild_id 0 / empty name / empty roster.
        let in_guild = gs.player_guild_id != 0 && gs.player_guild_id != 0xFFFF_FFFF;
        if in_guild {
            g.guild_id = gs.player_guild_id;
            g.guild_rank = gs.player_guild_rank;
            g.guild_name = gs.guild_names.get(&gs.player_guild_id).cloned().unwrap_or_default();
            g.members = gs.guild_members.clone();
        } else {
            g.guild_id = 0;
            g.guild_rank = 0;
            g.guild_name.clear();
            g.members.clear();
        }
        g.pending_invite = gs.pending_guild_invite.as_ref().map(|(inviter, _, _)| inviter.clone());
    }

    /// Publish the player's inventory + equipment from `gs` into the shared slot (GET /inventory).
    pub fn sync_inventory(&self, gs: &GameState) {
        let mut inv = self.inventory_slots.inventory.lock().unwrap();
        inv.clear();
        inv.extend(gs.inventory.iter().cloned());
    }

    /// Apply a slot move to `gs` **and immediately republish** the inventory (#347, review round 1,
    /// finding B2).
    ///
    /// EQEmu sends no OP_MoveItem echo for a move we initiated, so this loop mirrors the move into
    /// `gs.inventory` itself. `sync_inventory` — the only thing that copies `gs.inventory` into the
    /// slot HTTP reads — is called from the *packet* loop (`gameplay.rs`), never from `tick()`. So
    /// a bare `gs.move_item(..)` here left the published inventory describing the layout as it was
    /// BEFORE our own move, until the next inbound packet happened to arrive.
    ///
    /// That is now load-bearing rather than merely stale, because #347 step 1 turned that published
    /// snapshot into a door check: `/v1/inventory/move`, `/v1/combat/scribe`, `/v1/interact/give`
    /// and `/v1/merchant/sell` all refuse `404 no item in slot N` when the snapshot says the source
    /// slot is empty. Against a stale snapshot that 404 is a confident falsehood about a request
    /// that would have worked — the agent-honesty invariant inverted by the check meant to serve it.
    ///
    /// Every local mirror edit in this file goes through this method or
    /// [`ActionLoop::mirror_remove_item`]; the test
    /// `every_local_inventory_mirror_edit_in_this_file_goes_through_a_republishing_helper` fails if a
    /// raw `gs.move_item(` / `gs.inventory.retain(` reappears in this file outside them.
    fn mirror_move_item(&self, gs: &mut GameState, from: i32, to: i32) {
        gs.move_item(from, to);
        self.sync_inventory(gs);
    }

    /// Drop the item in `slot` from `gs` **and immediately republish** — the deletion half of
    /// [`ActionLoop::mirror_move_item`], for the case where the server removes an item without
    /// telling us (the scribed scroll it consumes off the cursor). Same staleness reason, same
    /// republish.
    fn mirror_remove_item(&self, gs: &mut GameState, slot: i32) {
        gs.inventory.retain(|i| i.slot != slot);
        self.sync_inventory(gs);
    }

    /// Deliver the freshly-parsed `/who all` roster to the pending GET /v1/observe/who (#300). Called
    /// from the gameplay drain loop right after an OP_WhoAllResponse updates `gs.who_roster`. No-op if
    /// no request is in flight (e.g. an unsolicited/duplicate response).
    pub fn fulfill_who(&mut self, gs: &GameState) {
        if let Some(tx) = self.pending_who.take() {
            let _ = tx.send(gs.who_roster.clone());
        }
    }

    /// True when the next OP_WhoAllResponse should be treated as an OP_FriendsWho reply (a friends
    /// poll) rather than a /who all — so the gameplay loop routes it to `fulfill_friends`. (#301)
    pub fn expecting_friends(&self) -> bool { self.expecting_friends }

    /// Deliver the friends-presence reply (the online subset, parsed into `gs.who_roster` by
    /// `apply_who_all`) to the pending GET /v1/social/friends. Mirrors `fulfill_who`. (#301)
    pub fn fulfill_friends(&mut self, gs: &GameState) {
        if let Some(tx) = self.pending_friends.take() {
            let _ = tx.send(gs.who_roster.clone());
        }
        self.expecting_friends = false;
    }

    /// Resolve a parked awaited-buy on the OP_ShopPlayerBuy echo (A3 Migration 1, #448). Called from
    /// the gameplay loop AFTER `apply_packet`, so `gs` already holds the receipt (coin deducted,
    /// "Bought item" logged). CORRELATES the echo against the parked buy — the echo's npcid@0 and
    /// itemslot@8 must both match, so a stray shop echo (e.g. a UI fire-and-forget buy of a
    /// DIFFERENT slot) can't resolve THIS awaited buy. On a match it sends `Resolved(BuyOk{..})`
    /// read from the applied `gs` (item name from the open ware list, server-recomputed price from
    /// the echo, coin AFTER the local deduction). The send is non-blocking and never `.await`s, so
    /// the net tick is never stalled; a dropped receiver (HTTP already timed out) is ignored. No-op
    /// when nothing is parked or the echo doesn't correlate — leaving an unrelated buy still parked.
    pub fn fulfill_buy_ok(&mut self, gs: &GameState, payload: &[u8]) {
        if payload.len() < 32 { return; }
        let echo_merchant = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let echo_slot     = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
        let price         = u32::from_le_bytes([payload[24], payload[25], payload[26], payload[27]]);
        // #492/#475: if this key was recently TIME-reaped, the echo may be the delayed echo of that
        // reaped buy (the transport delivers up to ~30s). Crediting it to the current same-key
        // `pending_buy` would be a silent wrong `Resolved`. DROP it — the re-admitted buy reaps to an
        // honest `Unconfirmed`/202 instead. Checked BEFORE correlation so a stale echo never resolves.
        if self.buy_key_quarantined(echo_merchant, echo_slot) {
            tracing::warn!("EQ: #492/#475 dropped a quarantined shop-buy echo (merchant={echo_merchant} slot={echo_slot}) — could be the stale echo of a reaped buy; not credited");
            return;
        }
        // Correlate BEFORE taking: a non-matching echo must leave the parked buy in place.
        let correlates = self.pending_buy.as_ref()
            .is_some_and(|pb| pb.merchant_id == echo_merchant && pb.slot == echo_slot);
        if !correlates { return; }
        let pb = self.pending_buy.take().unwrap();
        let item_name = gs.merchant_items.iter()
            .find(|m| m.merchant_slot == echo_slot)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| format!("merchant slot {echo_slot}"));
        let _ = pb.tx.send(eqoxide_command::CommandResult::Resolved(
            eqoxide_command::BuyOk { item_name, price, coin_after: gs.coin },
        ));
    }

    /// Refuse a parked awaited-buy on OP_ShopEndConfirm (A3 Migration 1, #448). That packet is a
    /// 0-byte body with no correlation data, but for this client it is unambiguously a buy refusal
    /// (see `packet_handler::apply_shop_end_confirm`), so a refusal while a buy is parked resolves
    /// THAT buy as `Refused`. No-op when nothing is parked. Non-blocking send; never `.await`s.
    pub fn fulfill_buy_refused(&mut self) {
        if let Some(pb) = self.pending_buy.take() {
            let _ = pb.tx.send(eqoxide_command::CommandResult::Refused("merchant refused".into()));
        }
    }

    /// Reap a parked awaited-buy as `Unconfirmed` (A3 Migration 1, #448) — fired on a zone change so
    /// a crossing mid-buy can't strand the `Sender` or let it mis-correlate a shop echo in the new
    /// zone. The HTTP side maps `Unconfirmed` (and a dropped `Sender` — the disconnect case, handled
    /// for free when `ActionLoop` is dropped) to a 202 "outcome UNKNOWN". No-op when nothing parked.
    pub fn reap_pending_buy(&mut self) {
        if let Some(pb) = self.pending_buy.take() {
            let _ = pb.tx.send(eqoxide_command::CommandResult::Unconfirmed);
        }
    }

    /// Resolve a parked awaited-open on the OP_ShopRequest echo (eqoxide#479). Called from the
    /// gameplay loop AFTER `apply_packet`, so `gs.merchant_open` already reflects the applied echo
    /// (not read here — the echo payload itself carries everything needed). CORRELATES on the
    /// echo's npc_id — a stray shop-open echo for a DIFFERENT merchant (e.g. a UI fire-and-forget
    /// open fired while an awaited open is parked) must not resolve THIS awaited open.
    ///
    /// `command==1` (Open) → `Resolved(OpenOk)`: a real merchant confirmed the window opened.
    /// `command==0` (Close) → `Refused`: a REAL negative ack. Per eq-client-expert's research
    /// against the EQEmu RoF2 source (`~/git/eq_kb/merchant-open-protocol.md`,
    /// `client_packet.cpp` `Handle_OP_ShopRequest`), FIVE distinct server-side reasons — faction
    /// KOS/dubious, engaged in combat, feigned/invisible, charmed, or the window already busy —
    /// all fall through to this SAME `command=0` echo and are not distinguishable from the opcode
    /// alone.
    ///
    /// A target that is not a merchant NPC at all, or out of range, sends NO echo whatsoever
    /// (confirmed early `return` in `Handle_OP_ShopRequest`, no packet of any kind) — that path
    /// never reaches this function; it resolves to `Unconfirmed` via the HTTP timeout / a
    /// zone-change reaper instead, exactly the honest signal eqoxide#479 asks for.
    ///
    /// The send is non-blocking and never `.await`s, so the net tick is never stalled; a dropped
    /// receiver (HTTP already timed out) is ignored. No-op when nothing is parked or the echo
    /// doesn't correlate — leaving an unrelated open still parked.
    pub fn fulfill_open(&mut self, payload: &[u8]) {
        if payload.len() < 12 { return; }
        let echo_npc = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let command  = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
        // #492/#475: same as `fulfill_buy_ok` — a shop-open echo for a recently TIME-reaped merchant
        // may be that reaped open's delayed echo; drop it rather than credit a re-admitted same-merchant
        // open. The re-admitted open reaps to an honest `Unconfirmed`/202.
        if self.open_key_quarantined(echo_npc) {
            tracing::warn!("EQ: #492/#475 dropped a quarantined shop-open echo (merchant={echo_npc}) — could be the stale echo of a reaped open; not credited");
            return;
        }
        let correlates = self.pending_open.as_ref().is_some_and(|po| po.merchant_id == echo_npc);
        if !correlates { return; }
        let po = self.pending_open.take().unwrap();
        if command == 1 {
            let _ = po.tx.send(eqoxide_command::CommandResult::Resolved(
                eqoxide_command::OpenOk { merchant_id: echo_npc },
            ));
        } else {
            let _ = po.tx.send(eqoxide_command::CommandResult::Refused(
                "merchant refused to open the window (faction, engaged, feigned/invisible, \
                 charmed, or busy)".into(),
            ));
        }
    }

    /// Reap a parked awaited-open as `Unconfirmed` (eqoxide#479) — fired on a zone change so a
    /// crossing mid-open can't strand the `Sender` or let it mis-correlate a shop-open echo in the
    /// new zone. Mirrors `reap_pending_buy`. No-op when nothing parked.
    pub fn reap_pending_open(&mut self) {
        if let Some(po) = self.pending_open.take() {
            let _ = po.tx.send(eqoxide_command::CommandResult::Unconfirmed);
        }
    }

    /// Resolve a parked awaited-cast on its `gs.last_cast` TRANSITION (A3 Migration 3, #448). Called
    /// from the gameplay loop AFTER `apply_packet` (and after `resolve_pending_cast_end`), so `gs`
    /// already holds the outcome the cast machinery computed. Deliberately NOT keyed on a single
    /// opcode: a cast ends via one of THREE opcodes (OP_MemorizeSpell scribing=3 / OP_ManaChange /
    /// OP_InterruptCast) that are DE-DUPED against each other in `GameState`, so keying one would
    /// double-fire or miss. Instead we watch `last_cast`: the terminal for THIS cast is the outcome
    /// whose `at` is strictly after we parked (`begin_cast` cleared it to `None` on our OP_BeginCast,
    /// and a stale prior outcome carries an earlier `at`). The kind → honest result mapping:
    ///   • `cast_completed`  → `Resolved(CastEnd{outcome:"completed"})` (the spell LANDED)
    ///   • `cast_fizzled`    → `Resolved(CastEnd{outcome:"fizzled"})`   (resolved, did NOT land)
    ///   • `cast_interrupted`→ `Resolved(CastEnd{outcome:"interrupted"})` (resolved, did NOT land)
    ///   • `cast_failed`     → `Refused` (a real server refusal — no mana / no target / recast — that
    ///                         reached us AFTER we parked; the cast definitively did not happen → 409)
    ///   • `cast_ended_unexplained` (or anything else) → `Unconfirmed` (the server ended the cast and
    ///                         never said why — genuinely UNKNOWN, never rendered as success → 202)
    /// The send is non-blocking and never `.await`s; a dropped receiver (HTTP already timed out) is
    /// ignored. No-op when nothing is parked or no fresh outcome is present. Fizzle/interrupt are 200
    /// (the cast RESOLVED — we know what happened), but `outcome` carries the truth so a 200 can never
    /// be misread as "the spell took hold". See `eqoxide_command::result`.
    pub fn fulfill_cast(&mut self, gs: &GameState) {
        // #492/#475: after a cast is TIME-reaped, cast echoes carry NO content key (`fulfill_cast`
        // keys only on `outcome.at`), so ANY cast outcome within the resend window could belong to the
        // reaped cast. Suppress ALL cast echo-fulfillment during the quarantine so a re-admitted cast
        // reaps to an honest `Unconfirmed` instead of absorbing an unrelated spell's outcome. Degraded
        // (a genuine cast in this rare window won't resolve on its echo) but never a wrong `Resolved`.
        if self.cast_quarantined() { return; }
        let Some(pc) = self.pending_cast.as_ref() else { return };
        // Correlate BEFORE taking: only a FRESH outcome (strictly after we parked) is this cast's.
        let Some(outcome) = gs.last_cast.as_ref().filter(|o| o.at > pc.sent_at) else { return };
        let result = match outcome.kind {
            "cast_completed" | "cast_fizzled" | "cast_interrupted" => {
                let verdict = match outcome.kind {
                    "cast_completed"   => "completed",
                    "cast_fizzled"     => "fizzled",
                    _                  => "interrupted",
                };
                eqoxide_command::CommandResult::Resolved(eqoxide_command::CastEnd {
                    outcome:    verdict.to_string(),
                    spell_id:   outcome.spell_id,
                    spell_name: eqoxide_core::spells::name_of(outcome.spell_id),
                    text:       outcome.text.clone(),
                })
            }
            // A real server refusal that reached us after we parked — the cast never happened → 409.
            "cast_failed" => eqoxide_command::CommandResult::Refused(outcome.text.clone()),
            // The server ended the cast without explaining it (buff-won't-stack, or an inferred end) —
            // genuinely unknown whether the spell had any effect → 202, never a claimed success.
            _ => eqoxide_command::CommandResult::Unconfirmed,
        };
        let pc = self.pending_cast.take().unwrap();
        let _ = pc.tx.send(result);
    }

    /// Reap a parked awaited-cast as `Unconfirmed` (A3 Migration 3, #448) — fired on a zone change so
    /// a crossing mid-cast can't strand the `Sender`. `begin_zone_in` already drops all in-flight cast
    /// tracking (the cast can't survive the crossing), so no `last_cast` transition will ever come for
    /// it now; reaping yields the honest "outcome UNKNOWN" 202 promptly. Mirrors `reap_pending_buy`.
    /// (Disconnect is covered for free: dropping `ActionLoop` drops the `Sender` → closed channel → 202.)
    pub fn reap_pending_cast(&mut self) {
        if let Some(pc) = self.pending_cast.take() {
            let _ = pc.tx.send(eqoxide_command::CommandResult::Unconfirmed);
        }
    }

    /// #492 — TIME-BASED reap of stranded A3 pending slots, swept once per `tick`. Buy/open/cast each
    /// park a `oneshot::Sender` that is otherwise cleared ONLY on (a) its resolving echo (`fulfill_*`)
    /// or (b) a zone change (`reap_pending_*`). But the exact Unconfirmed/202 case — the server sends
    /// NO resolving packet (a non-merchant/out-of-range open, an insufficient-funds buy, a cast the
    /// server silently drops) — hits NEITHER: the HTTP `tokio::time::timeout` fires and returns 202 to
    /// the caller, yet the parked `Sender` lingers on the net thread indefinitely. The in-flight
    /// singleton guard (`drain_merchant`/`drain_cast`) then honestly-but-WRONGLY 409s EVERY later
    /// command of that type ("another <X> already in flight") until the character zones.
    ///
    /// Here we drop any slot whose `sent_at` has exceeded its per-command reap deadline
    /// (`SHOP_PENDING_REAP` for buy/open, `CAST_PENDING_REAP` for cast — each `>=` the HTTP timeout the
    /// caller already gave up on, so a genuine late echo would have fulfilled the slot BEFORE we reap
    /// it). HONESTY: the parked receiver has already timed out and returned 202, so we simply DROP the
    /// `Sender` (dropping closes the channel; no late `Resolved`/`Unconfirmed` is fabricated) — the
    /// only effect is unblocking the next same-type command. Swept at the TOP of `tick`, before
    /// `drain_merchant`/`drain_cast`, so the same tick that reaps also admits the waiting command.
    ///
    /// The awaited GIVE is NOT swept here: it lives in `give_state` (not a `pending_*` slot) and its
    /// state machine already SELF-times-out every tick via `GIVE_ACK_TIMEOUT_TICKS` /
    /// `GIVE_FINISH_TIMEOUT_TICKS` in `tick_give`, resolving the awaited `Sender` to `Unconfirmed` and
    /// clearing `give_state` — so a stranded give cannot occur.
    ///
    /// QUARANTINE (#492/#475): dropping a `pending_*` slot on a NON-session-ending timeout re-opens the
    /// echo-misattribution class #475 closed — the reaped command's request is still alive on the wire
    /// and its delayed echo can land up to the transport's ~30s `resend_timeout` later, long after a
    /// same-key command has been re-admitted. So on each reap we record the reaped command's
    /// correlation key (buy: merchant_id+slot; open: merchant_id; cast: a key-less time gate) for
    /// `ECHO_QUARANTINE`; `fulfill_buy_ok`/`fulfill_open`/`fulfill_cast` then DROP any matching echo
    /// during the window rather than credit it — a re-admitted same-key command reaps to an honest
    /// `Unconfirmed` instead of stealing the reaped command's outcome (a silent wrong `Resolved`).
    fn reap_expired_pending(&mut self) {
        let now = Instant::now();
        if self.pending_buy.as_ref().is_some_and(|p| p.sent_at.elapsed() >= SHOP_PENDING_REAP) {
            let p = self.pending_buy.take().unwrap(); // drop the Sender — caller already got its 202
            self.reaped_buy_keys.push(((p.merchant_id, p.slot), now + ECHO_QUARANTINE));
            tracing::warn!("EQ: #492 reaped a stranded awaited BUY (merchant={} slot={}, no resolving packet within {SHOP_PENDING_REAP:?}) — later buys unblocked; key quarantined against a stale echo for {ECHO_QUARANTINE:?}", p.merchant_id, p.slot);
        }
        if self.pending_open.as_ref().is_some_and(|p| p.sent_at.elapsed() >= SHOP_PENDING_REAP) {
            let p = self.pending_open.take().unwrap();
            self.reaped_open_keys.push((p.merchant_id, now + ECHO_QUARANTINE));
            tracing::warn!("EQ: #492 reaped a stranded awaited OPEN (merchant={}, no resolving packet within {SHOP_PENDING_REAP:?}) — later opens unblocked; key quarantined for {ECHO_QUARANTINE:?}", p.merchant_id);
        }
        if self.pending_cast.as_ref().is_some_and(|p| p.sent_at.elapsed() >= CAST_PENDING_REAP) {
            self.pending_cast = None; // drop the Sender
            self.cast_quarantine_until = Some(now + ECHO_QUARANTINE);
            tracing::warn!("EQ: #492 reaped a stranded awaited CAST (no resolving packet within {CAST_PENDING_REAP:?}) — later casts unblocked; cast echoes suppressed for {ECHO_QUARANTINE:?} (no content key to correlate)");
        }
        // Prune expired quarantine entries so the sets stay small (they gate `fulfill_*` on `> now`
        // anyway, so an un-pruned stale entry is inert — this is just housekeeping).
        self.reaped_buy_keys.retain(|(_, exp)| *exp > now);
        self.reaped_open_keys.retain(|(_, exp)| *exp > now);
        if self.cast_quarantine_until.is_some_and(|t| t <= now) { self.cast_quarantine_until = None; }
    }

    /// #492/#475 — true while an OP_ShopPlayerBuy echo for `(merchant_id, slot)` might be the delayed
    /// echo of a recently TIME-reaped buy (within `ECHO_QUARANTINE`), so it must not be credited.
    fn buy_key_quarantined(&self, merchant_id: u32, slot: u32) -> bool {
        let now = Instant::now();
        self.reaped_buy_keys.iter().any(|((m, s), exp)| *m == merchant_id && *s == slot && *exp > now)
    }

    /// #492/#475 — same for an OP_ShopRequest echo's `merchant_id`.
    fn open_key_quarantined(&self, merchant_id: u32) -> bool {
        let now = Instant::now();
        self.reaped_open_keys.iter().any(|(m, exp)| *m == merchant_id && *exp > now)
    }

    /// #492/#475 — true while cast echoes are suppressed after a time-reaped cast (no content key).
    fn cast_quarantined(&self) -> bool {
        self.cast_quarantine_until.is_some_and(|t| t > Instant::now())
    }

    /// Publish the open-merchant session from `gs` into the shared slot (GET /trade/list + the HUD
    /// merchant window).
    pub fn sync_merchant(&self, gs: &GameState) {
        let mut m = self.merchant_slots.merchant.lock().unwrap();
        m.open = gs.merchant_open.is_some();
        m.merchant_id = gs.merchant_open;
        m.items.clear();
        m.items.extend(gs.merchant_items.iter().cloned());
    }

    /// Publish the in-game message log from `gs` into the shared slot (GET /messages), converting
    /// each LogEntry into a serializable MessageEntry, extracting `[bracketed]` quest keywords
    /// (the same splitter the HUD dialogue panel uses), and carrying along any item/say links the
    /// text contained (eqoxide#256) so an agent gets a resolvable `item_id` alongside the clean text.
    pub fn sync_messages(&self, gs: &GameState) {
        let mut out = self.chat.messages.lock().unwrap();
        out.clear();
        out.extend(gs.messages.iter().map(|m| {
            let keywords = eqoxide_core::game_state::split_keywords(&m.text).into_iter()
                .filter(|(_, is_kw)| *is_kw)
                .map(|(seg, _)| seg.trim_matches(|c| c == '[' || c == ']').trim().to_string())
                .filter(|k| !k.is_empty())
                .collect();
            eqoxide_ipc::MessageEntry {
                kind: m.kind.clone(),
                text: m.text.clone(),
                keywords,
                item_links: m.item_links.clone(),
            }
        }));
        drop(out);
        // Publish the current clickable NPC-dialogue choices (GET /v1/observe/dialogue, #120).
        *self.interact.dialogue.lock().unwrap() = gs.dialogue_choices.clone();
        // Publish async events (GET /v1/events/*), preserving their stable monotonic ids.
        let mut ev = self.chat.chat_events.lock().unwrap();
        ev.clear();
        ev.extend(gs.chat_events.iter().map(|e| eqoxide_ipc::Event {
            id: e.id, category: e.category.clone(), kind: e.kind.clone(),
            from: e.from.clone(), directed: e.directed, text: e.text.clone(),
        }));
    }

    /// Publish the current zone's doors from `gs` into the shared slot (GET /doors).
    pub fn sync_doors(&self, gs: &GameState) {
        let mut out = self.interact.doors_shared.lock().unwrap();
        out.clear();
        out.extend(gs.world.doors.values().map(|d| eqoxide_ipc::DoorView {
            door_id: d.door_id, name: d.name.clone(),
            x: d.x, y: d.y, z: d.z, heading: d.heading,
            opentype: d.opentype, is_open: d.is_open,
        }));
    }

    /// The controller slots this loop streams from, for the zone-entry handshake (#846 review B1).
    ///
    /// `gameplay::run_zone_entry_handshake` needs [`eqoxide_ipc::ControllerSlots::begin_zone_in`]
    /// rather than `GameState::begin_zone_in`: clearing the `GameState` disclosure fields without
    /// invalidating the `ControllerView` they are mirrored from leaves `stream_position` to restore
    /// the departed zone's hold on its very next tick. This is a read-only borrow — the handshake
    /// has no business driving the loop, it just needs the shared slots.
    pub fn controller_slots(&self) -> &eqoxide_ipc::ControllerSlots { &self.controller }

    /// Sync zone exit points from `gs` into the shared zone_points map.
    /// On zone change, also loads map-label exits from disk as fallback zone points.
    pub fn sync_zone_points(&mut self, gs: &GameState) {
        // On zone change, load map labels from disk as fallback zone points.
        if gs.world.zone_name != self.current_zone {
            self.current_zone = gs.world.zone_name.clone();

            // Reset the nav destination + route on a zone change (#248). The old goal/path are in the
            // PREVIOUS zone's coordinate space; kept across a crossing they aim the walker at an
            // arbitrary spot (usually a corner near the arrival point) and wedge it there. A completed
            // crossing IS the "walk to the zone line" goal reached, so the character should come to
            // rest in the new zone; a driver that wants to keep going re-issues /v1/move/* afterward.
            // (This is the zone-boundary sibling of the mid-zone stale-plan bug #246.)
            self.walker.reset_for_zone_change();

            // #683 round 3: the gated-refusal latch is keyed on a REGION INDEX, and region indices
            // are per-zone namespaces (index 0 is the universal unresolvable index). A server
            // teleport that lands the character from one gated null region directly onto ANOTHER
            // zone's gated null region would find the latch already set and silently suppress the
            // new zone's refusal message — the F1 silent wait, resurrected across a zone boundary.
            // The latch is only ever about the zone the character is standing in; re-arm it here.
            self.gated_cross_reported = None;

            // #448: reap any awaited merchant buy still parked across the crossing as `Unconfirmed`.
            // The merchant is in the OLD zone, so no echo can ever arrive now; leaving the Sender
            // parked would let a shop echo in the NEW zone mis-correlate it (or strand it until the
            // HTTP timeout). Firing Unconfirmed here yields the honest "outcome UNKNOWN" 202 promptly.
            self.reap_pending_buy();
            // eqoxide#479: same reasoning for an awaited merchant open still parked across the
            // crossing — the merchant is in the OLD zone, no OP_ShopRequest echo can ever arrive now.
            self.reap_pending_open();
            // #448 (Migration 2): same reasoning for an awaited give still parked across the crossing —
            // the NPC is in the OLD zone, no OP_FinishTrade can arrive, and a stray finish in the NEW
            // zone must not mis-resolve it. Reap it to a prompt Unconfirmed/202.
            self.reap_pending_give();
            // #448 (Migration 3): same for an awaited cast still parked across the crossing — the cast
            // cannot survive a zone change (`begin_zone_in` drops all cast tracking), so no `last_cast`
            // transition will ever resolve it now. Reap it to a prompt Unconfirmed/202.
            self.reap_pending_cast();

            let mut shared = self.world.zone_points.lock().unwrap();
            // Start fresh with server entries.
            shared.clear();
            shared.extend(gs.world.zone_points.iter().cloned());
            // Load map labels from disk.
            if let Some(zm) = eqoxide_core::zone_map::ZoneMap::load(&self.maps_dir, &gs.world.zone_name) {
                let before = shared.len();
                for label in &zm.labels {
                    let lower = label.text.to_lowercase();
                    if !lower.starts_with("to ") { continue; }
                    let dest_zone_id: u16 = if lower.contains("north qeynos") || lower.contains("qeynos2") {
                        2
                    } else if lower.contains("south qeynos") {
                        1 // qeynos south
                    } else {
                        0
                    };
                    if dest_zone_id == 0 { continue; }
                    let dup = shared.iter().any(|zp| {
                        zp.zone_id == dest_zone_id
                            && ((zp.server_x - label.east).powi(2) + (zp.server_y - label.north).powi(2)) < 2500.0
                    });
                    if dup { continue; }
                    shared.push(ZonePoint {
                        iterator: u32::MAX,
                        server_x: label.east,
                        server_y: label.north,
                        server_z: 0.0,
                        heading: 0.0,
                        zone_id: dest_zone_id,
                    });
                    tracing::info!("zone_map: added exit '{}' at ({:.1}, {:.1}) → zone_id={}",
                              label.text, label.east, label.north, dest_zone_id);
                }
                if shared.len() > before {
                    tracing::info!("zone_map: {} fallback exits added (total {})", shared.len() - before, shared.len());
                }
            }
        } else {
            // Same zone: update server entries but keep map labels.
            let mut shared = self.world.zone_points.lock().unwrap();
            let map_labels: Vec<_> = shared.drain(..)
                .filter(|zp| zp.iterator == u32::MAX)
                .collect();
            shared.extend(gs.world.zone_points.iter().cloned());
            shared.extend(map_labels);
        }
    }

    /// Publish the current `/move/goto` navigation state for GET /v1/observe/debug (#166, #337).
    /// The value set is an AGENT-FACING CONTRACT — every value is documented in `docs/http-api.md`:
    ///
    ///   pending | idle | planning | navigating | navigating_partial | following | arrived
    ///   | no_path | search_exhausted | blocked | zone_loading
    ///
    // `set_nav_state`/`stop_nav`/`apply_plan`/`apply_local_plan`/`is_player_dead`/`nav_halt_if_dead`/
    // `find_in_zone_portal`/`aggro_avoid` moved to `eqoxide_nav::walker::Walker` (M1 extraction).
    // `is_player_dead` itself moved further, to `GameState::is_player_dead` — both `Walker` and
    // `drain_zone_cross` (below) need it, and it depends only on `GameState`.

    /// Advance one navigation tick (no-op if fewer than 150 ms have elapsed).
    pub fn tick(
        &mut self,
        stream:  &mut EqStream,
        gs:      &mut GameState,
    ) {
        // #492: drop any A3 pending slot the server never resolved (buy/open/cast) BEFORE the drains
        // re-check the in-flight singleton guard, so a stranded slot from a silent Unconfirmed/202
        // command can't 409-block the next same-type command indefinitely. See `reap_expired_pending`.
        self.reap_expired_pending();

        self.drain_loot(gs);
        self.drain_doors(stream, gs);
        self.drain_quests(stream, gs);
        self.drain_group(stream, gs);
        self.drain_trainer(stream, gs);
        self.drain_zone_cross(stream, gs);
        self.drain_chat(stream, gs);
        self.drain_target(stream, gs);
        self.drain_who_friends(stream);
        self.drain_combat(stream, gs);
        self.drain_pet(stream, gs);
        self.drain_read_book(stream, gs);
        self.drain_guild(stream, gs);
        self.drain_cast(stream, gs);
        self.drain_mem_spell(stream, gs);
        self.drain_sit(stream, gs);
        self.drain_run_mode(stream, gs); // #625 — distinct fn, does not touch any other drain
        self.drain_consider(stream, gs);
        self.drain_merchant(stream, gs);
        self.drain_move_item(stream, gs);

        // Stream the controller's authoritative position to the server every tick at native cadence
        // (independent of the 150 ms planner gate below). This is the single position authority.
        self.stream_position(stream, gs);

        // Dead men don't walk (#238, eqoxide#61): the instant the player is slain, abandon any /goto
        // or /zone_cross and stop driving the controller, so a corpse doesn't keep walking its route
        // toward the goal. Placed BEFORE the fast-steering refresh AND the 150 ms walk gate so
        // movement halts within a tick, not up to a gate-period later. Position streaming above still
        // runs, keeping the stationary corpse in sync with the server.
        if self.walker.nav_halt_if_dead(gs) {
            return;
        }

        self.walker.apply_fast_steering(gs);

        if self.last_tick.elapsed().as_millis() < NAV_TICK_MS {
            return;
        }
        self.last_tick = Instant::now();

        // Quest turn-in (POST /give) trade-window state machine. Spans multiple ticks: we must
        // wait for the server's OP_TradeRequestAck (sets gs.trade_ack_ready) between sending the
        // trade request and moving the item into the NPC trade slot. Run on the throttled ~150ms
        // cadence so the per-tick ack timeout count matches the documented ~3s window.
        self.tick_give(stream, gs);

        self.drive_auto_target(stream, gs);

        self.drive_auto_pet_combat(stream, gs);

        if self.drive_auto_engage_melee(stream, gs) { return; }

        // (The dead-player guard now runs earlier — right after stream_position, before the fast-
        // steering refresh and the 150 ms gate — so a corpse stops within a tick. See #238.)

        self.walker.drive_chase();

        self.walker.drive_teleport_detect(gs);

        let goal = match self.walker.resolve_goal(gs) {
            Some(g) => g,
            None => return,
        };

        // `Walker::drive_walk` never touches position/`EqStream` itself (intent-only boundary — see
        // `eqoxide_nav::walker`'s module doc): it only writes the per-frame `nav_intent`. A big drop
        // is no longer a special handoff — the walker just keeps walking toward the goal and the
        // render controller's ONE collided gravity path descends off the edge (§442, #442); the
        // landing damage is applied driver-agnostically in `stream_position`.
        self.walker.drive_walk(gs, goal);
    }

    // TODO(MVC): this and the other `drain_*` methods below are slot CONSUMERS — they poll a
    // request slot that both UI click-handlers (src/ui/) and the HTTP agent API (src/http/) write
    // into independently today. Program Phase 2 should unify those two producers behind one shared
    // controller-verb call so "click Loot" and "POST /v1/interact/loot" both go through the same
    // code path instead of two independent writers racing into the same `Arc<Mutex<Option<T>>>`.
    fn drain_loot(&mut self, gs: &mut GameState) {
        // POST /loot: queue the requested corpse onto the existing auto-loot pipeline. The gameplay
        // loop drains pending_loot — sends OP_LootRequest, echoes each OP_LootItem to take it, then
        // OP_EndLootRequest. The 500ms delay (loot_queued_at) lets the server register the corpse.
        if let Some(corpse_id) = self.command.take_loot() {
            gs.pending_loot.push_back(corpse_id);
            if gs.loot_queued_at.is_none() {
                gs.loot_queued_at = Some(Instant::now());
            }
            tracing::info!("loot: queued corpse_id={} for looting (via POST /loot)", corpse_id);
        }
    }

    fn drain_doors(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /doors/click or a human door click: send OP_ClickDoor. The door opens
        // visually only when the server replies with OP_MoveDoor.
        if let Some(door_id) = self.command.take_door_click() {
            stream.send_app_packet(OP_CLICK_DOOR, &build_click_door(door_id, gs.player_id));
            tracing::info!("EQ: click door_id={}", door_id);
            gs.log_msg("door", &format!("Clicked door {}", door_id));
        }
    }

    fn drain_quests(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/quests/accept ({"task_id":N}) or /decline (task_id=0): send OP_AcceptNewTask.
        // For a real accept, look up the offering NPC's id from gs.task_offers (task_master_id is
        // required by the struct); a decline sends task_master_id=0 (irrelevant when task_id==0).
        // Either way, the selector window is done with — clear all pending offers.
        if let Some(task_id) = self.command.take_accept_task() {
            let task_master_id = if task_id == 0 {
                0
            } else {
                gs.task_offers.iter().find(|o| o.task_id == task_id).map(|o| o.npc_id).unwrap_or(0)
            };
            stream.send_app_packet(OP_ACCEPT_NEW_TASK, &build_accept_new_task(task_id, task_master_id));
            if task_id == 0 {
                tracing::info!("EQ: quests: declined all pending task offers");
                gs.log_msg("quest", "Declined task offer(s)");
            } else {
                tracing::info!("EQ: quests: accepted task_id={task_id} task_master_id={task_master_id}");
                gs.log_msg("quest", "Accepted task offer");
            }
            gs.task_offers.clear();
        }

        // POST /v1/quests/cancel ({"task_id":N}): abandon an active task. OP_CancelTask addresses
        // the task by its journal sequence_number, not task_id — see build_cancel_task.
        if let Some(task_id) = self.command.take_cancel_task() {
            if let Some(task) = gs.tasks.get(&task_id) {
                let seq = task.sequence_number;
                stream.send_app_packet(OP_CANCEL_TASK, &build_cancel_task(seq));
                tracing::info!("EQ: quests: cancelled task_id={task_id} sequence_number={seq}");
                gs.log_msg("quest", "Cancelled task");
            } else {
                tracing::warn!("EQ: quests: cancel requested for unknown task_id={task_id} — ignoring");
            }
        }
    }

    // #446: the HUD group window and POST /v1/group/* both write through the shared
    // `CommandState::request_group_*` verbs now, and this drain reads them back via
    // `take_group_*` — one typed surface over each slot instead of two call sites poking the raw
    // `Arc<Mutex<..>>`.
    fn drain_group(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/group/invite {"name":"X"}: send OP_GroupInvite.
        if let Some(target) = self.command.take_group_invite() {
            stream.send_app_packet(OP_GROUP_INVITE, &build_group_invite(&target, &gs.player_name));
            tracing::info!("EQ: group: invited {target}");
            gs.log_msg("group", &format!("Invited {target} to group"));
        }

        // POST /v1/group/accept: send OP_GroupFollow. Optimistically clear pending_invite now —
        // the real roster confirmation arrives via OP_GroupUpdateB/OP_GroupAcknowledge.
        if self.command.take_group_accept().is_some() {
            if let Some(inviter) = gs.pending_invite.take() {
                stream.send_app_packet(OP_GROUP_FOLLOW, &build_group_follow(&inviter, &gs.player_name));
                tracing::info!("EQ: group: accepted invite from {inviter}");
                gs.log_msg("group", &format!("Accepted group invite from {inviter}"));
            }
        }

        // POST /v1/group/decline: RoF2 has no working OP_GroupCancelInvite, so send a defensive
        // OP_GroupDisband(self, self) cleanup instead.
        if self.command.take_group_decline().is_some() {
            if let Some(inviter) = gs.pending_invite.take() {
                stream.send_app_packet(OP_GROUP_DISBAND, &build_group_disband(&gs.player_name, &gs.player_name));
                tracing::info!("EQ: group: declined invite from {inviter}");
                gs.log_msg("group", &format!("Declined group invite from {inviter}"));
            }
        }

        // POST /v1/group/leave: send OP_GroupDisband(self, self). If leader with < 3 members this
        // fully disbands the group server-side (no auto handoff — see Global Constraints).
        if self.command.take_group_leave().is_some() {
            stream.send_app_packet(OP_GROUP_DISBAND, &build_group_disband(&gs.player_name, &gs.player_name));
            tracing::info!("EQ: group: left group");
            gs.log_msg("group", "Left group");
        }

        // POST /v1/group/kick {"name":"X"}: send OP_GroupDisband(self, target). HTTP layer already
        // validated leadership + membership before queuing this.
        if let Some(target) = self.command.take_group_kick() {
            stream.send_app_packet(OP_GROUP_DISBAND, &build_group_disband(&gs.player_name, &target));
            tracing::info!("EQ: group: kicked {target}");
            gs.log_msg("group", &format!("Kicked {target} from group"));
        }

        // POST /v1/group/makeleader {"name":"X"}: send OP_GroupMakeLeader.
        if let Some(target) = self.command.take_group_make_leader() {
            stream.send_app_packet(OP_GROUP_MAKE_LEADER, &build_group_make_leader(&gs.group_leader, &target));
            tracing::info!("EQ: group: transferring leadership to {target}");
            gs.log_msg("group", &format!("Transferred group leadership to {target}"));
        }
    }

    fn drain_trainer(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/trainer/open {"trainer":"X"}: send OP_GMTraining for the resolved NPC spawn id.
        // The server replies OP_GMTraining with the offered caps → apply_gm_training sets gs.trainer_*.
        // Sentinel: Some(0) ENDS the open session (OP_GMEndTraining) — 0 is never a real spawn id;
        // reusing the slot avoids threading one more field through the positional chains (#162).
        if let Some(npc_id) = self.command.take_trainer_open() {
            if npc_id == 0 {
                if let Some(open_npc) = gs.trainer_open.take() {
                    let payload = build_gm_end_training(open_npc, gs.player_id);
                    stream.send_app_packet(OP_GM_END_TRAINING, &payload);
                    gs.trainer_skills.clear();
                    tracing::info!("EQ: trainer: ended training with npc {open_npc}");
                }
            } else {
                stream.send_app_packet(OP_GM_TRAINING, &build_gm_training(npc_id, gs.player_id));
                tracing::info!("EQ: trainer: opening training with npc {npc_id}");
            }
        }

        // POST /v1/trainer/train {"skill_id":N}: send OP_GMTrainSkill to the open trainer. The server
        // raises the skill and echoes OP_SkillUpdate → apply_skill_update reflects the new value.
        if let Some(skill_id) = self.command.take_train_skill() {
            if let Some(npc_id) = gs.trainer_open {
                stream.send_app_packet(OP_GM_TRAIN_SKILL, &build_gm_train_skill(npc_id, skill_id));
                tracing::info!("EQ: trainer: training skill {skill_id} at npc {npc_id}");
                gs.log_msg("trainer", &format!("Training {}", eqoxide_core::skills::skill_name(skill_id).unwrap_or("?")));
            } else {
                gs.log_msg("trainer", "Cannot train — no trainer window open");
            }
        }
    }

    fn drain_zone_cross(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Check zone-cross request — walk onto the target zone line so the auto-cross below fires.
        //
        // A zone line's real trigger is a `DRNTP` region baked into the zone geometry (native
        // mechanism), NOT the coords in OP_SendZonepoints — those are the DESTINATION of each line,
        // so walking to them lands the player nowhere near the trigger and the server safe-coords /
        // cheat-flags the crossing (the root cause of #174). Resolve the target zone to its
        // zone-point index (iterator), locate that DRNTP region in the zone BSP, and walk there.
        //
        // #725: the drain yields a `ZoneCrossTicket`, not a bare `u16`. Once the request leaves the
        // slot it exists nowhere else, so every path out of the resolution OWES the agent something
        // observable; the ticket's `Drop` publishes `idle`/`zone_cross_dropped_unhandled` if none of
        // them did. It is a BACKSTOP, not the mechanism — each arm publishes for itself — but it is
        // what makes "a branch that consumes the request and writes nothing" impossible to ship
        // silently, which is the general shape of #725 rather than its particular trigger.
        //
        // `resolve_zone_cross` takes the ticket BY VALUE, so the obligation is settled by the time
        // it returns — before the standing auto-cross below. The ticket is deliberately out of
        // scope for the remainder of this function; see that method for why the order matters.
        if let Some(ticket) = self.command.take_zone_cross() {
            self.resolve_zone_cross(ticket, gs);
        }

        // Auto zone-cross (native mechanism): when the player stands in a DRNTP zone-line region
        // baked into the zone BSP, resolve its zone-point index to a destination via the
        // OP_SendZonepoints list and send OP_ZONE_CHANGE. The server then matches our real position
        // against the DB trigger and places us at the correct arrival point. Cooldown prevents
        // re-firing while still inside the region right after a crossing.
        {
            // #725 review round 3, B3: the resolution's obligation must be SETTLED before this
            // block. Statically it is — `resolve_zone_cross` takes the ticket by value and has
            // returned — but the refactor that undoes that (parameter to `&ZoneCrossTicket`, so the
            // caller's local outlives this block) compiles, and the whole suite passed it. This
            // makes it fail instead: a ticket still alive here is one the auto-cross could pay off
            // (`perform_cross`'s same-zone branch publishes) or be stamped over by (the other two
            // arms publish nothing, so a late drop would write `zone_cross_dropped_unhandled` over a
            // live crossing).
            debug_assert_eq!(self.command.zone_cross_outstanding(), 0,
                "#725 B3: a drained zone_cross ticket is still alive at the standing auto-cross — \
                 resolve_zone_cross must OWN it so the obligation is settled before this point");
            const ZONE_CROSS_COOLDOWN_MS: u128 = 10000; // 10 seconds
            // Probe the STANDING CAPSULE SPAN, not the feet point. A DRNTP trigger volume whose
            // lower face floats just above the walkable floor (the qeynos2 KoT waterfall, #266:
            // lower face ≈0.4u over the flat vault floor) sits ABOVE a resting character's feet,
            // so a feet-only `zone_line_at([x,y,player_z])` never fired while standing on the
            // disclosed footprint — only a jump crossed. `zone_line_at_standing` sweeps
            // [feet, feet+height] so the body occupying the trigger fires the crossing, matching
            // the footprint validator (`teleport_pad_source`, which validates at feet+1). This is
            // purely the physical auto-cross from the character's real position — it does NOT
            // auto-route the walker onto the pad (that gate, TRUST_ADVERTISED_SAME_ZONE_CROSSINGS,
            // stays false); it only makes an agent-driven crossing actually fire (#266).
            //
            // **#713 review round 2, B2 — this probe runs EVERY TICK, deliberately OUTSIDE both the
            // cooldown and the dead guard below.** It used to sit inside them, which meant the
            // "have I left every zone-line region?" question — the one the client TELLS the agent to
            // answer by stepping off the line — was only asked once per 10 s cooldown, and the
            // bound-reaching attempt itself stamps that cooldown (`cross_unresolved`, below), so the
            // probe was dormant for the entire window right after the client said "step off and back
            // on to try again". A walk-off/walk-on shorter than the cooldown was sampled ON the line
            // at both ends and cleared NOTHING — the documented recovery did nothing at all, and the
            // agent had no way to tell "you did it wrong" from "it didn't work". That is the
            // agent-honesty failure mode (a confident false statement about the client's own
            // recoverability), so the fix is to make the sentence TRUE rather than to hedge it.
            //
            // It also un-gates the re-arm from `is_player_dead`: a corpse that is off every zone line
            // has ended its stand like anyone else, and leaving a dead character permanently blocked
            // was an accident of where the reset sat, not a decision.
            //
            // This does NOT re-open the refire storm the bound exists to close. Every path that
            // SENDS is still inside the cooldown guard AND still behind `blocks()`; the only thing
            // hoisted is an observation plus the two resets it licenses, and those fire whenever the
            // standing probe finds no zone-line region — which never happens during the continuous
            // stand the bound is stated over. That is not the same claim as "the character is
            // physically off every zone line": `zone_line_at_standing` also reads `None` when
            // `self.collision` is absent (no grid loaded yet) or the zone has no `.wtr` (a v1 map, no
            // water/region layer) — both indistinguishable from "off the line" to this probe. Benign
            // here (the hoisted statements only ever CLEAR), but not a physics guarantee. Cost: one
            // BSP leaf descent per ~1 u of body height per net tick (~100 Hz), against a `RwLock`
            // read — the same query the block already made, just no longer rate-limited by an
            // unrelated packet cooldown.
            //
            // The `eqoxide-core` property test (`auto_cross_attempts_are_bounded_per_stand_713`)
            // already modelled it this way — its `Tick::Off` arm clears the tally on EVERY off tick.
            // The model was right and the code was not; before this the two disagreed.
            let index = self.collision.read().unwrap().as_ref()
                .and_then(|c| c.zone_line_at_standing([gs.player_x, gs.player_y, gs.player_z]));
            if index.is_none() {
                // Off every zone-line region — re-arm the one-shot gated-refusal report so the
                // NEXT time the character stands on a gated line it is told again (#683 F1), and
                // re-arm the #713 attempt bound: the stand it counts has ended, so a character
                // that walks away and comes back gets its full allowance again. This is the
                // intended escape hatch from a bound that turned out to be too low, and it is
                // why a small bound is the safe direction (see `MAX_CROSS_ATTEMPTS`).
                self.gated_cross_reported = None;
                gs.zone_cross_attempts = None;
            }
            // A dead corpse standing in a zone-line region must NOT auto-zone (#238) — this fires purely
            // from physical position, so a character killed right at a boundary would cross while dead.
            if !gs.is_player_dead() && self.last_zone_cross.elapsed().as_millis() > ZONE_CROSS_COOLDOWN_MS {
                if let Some(index) = index {
                    // #713 item 1 (the #683 review's F3) — THE BOUND. A character left standing in a
                    // region the server refuses to zone used to re-request every cooldown for as long
                    // as it stood there, and every attempt is a server-side anomaly event. Stop after
                    // `MAX_CROSS_ATTEMPTS` attempts during one continuous stand on zone-line
                    // geometry. The allowance is per STAND and not per region index — a per-index
                    // tally leaks on an alternating stand between two adjacent DRNTP boxes, which the
                    // `eqoxide-core` property test found; see `CrossAttempts`.
                    //
                    // The tally counts ATTEMPTS, not denial echoes — see `CrossAttempts` for why
                    // (an OP_ZoneChange carries no correlation id, and a denial counter is blind to
                    // the server answering nothing at all, which refires identically). It is bumped
                    // only when a packet actually went out: `perform_cross`'s SAME-ZONE branch sends
                    // none (it is a pure in-zone teleport, #368), and bounding that would change
                    // intra-zone translocator behaviour, which #713 explicitly must not touch.
                    if gs.zone_cross_attempts.is_some_and(|t| t.blocks()) {
                        // Terminal. Deliberately SILENT here: the message log was written once at
                        // the moment the bound was reached (below), and `zone_cross_stopped` on
                        // GET /v1/observe/debug is the standing machine-readable answer — repeating
                        // the line every cooldown would be the same noise in a different pipe.
                        tracing::debug!("zone_cross: auto-cross at region index={index} is stopped \
                                         (attempt limit reached this stand)");
                    } else {
                        // Resolve the region's destination zone + arrival coords. `None` = no advertised
                        // zone point for this index (a bake/advertise data gap, #683) → fall back to the
                        // retail server-resolved crossing, gated by `classify_unresolved_cross`.
                        let sent = match self.resolve_cross_destination(index) {
                            // `perform_cross` returns true for the same-zone in-zone teleport, which
                            // sends NOTHING — so it is not an attempt for the bound's purposes.
                            Some((dest_zone, dest_pos)) =>
                                !self.perform_cross(stream, gs, index, dest_zone, dest_pos),
                            None => self.cross_unresolved(stream, gs, index),
                        };
                        if sent {
                            let tally = CrossAttempts::record(gs.zone_cross_attempts, index);
                            gs.zone_cross_attempts = Some(tally);
                            if tally.blocks() {
                                // Reached the bound on THIS attempt — fires exactly once per stand,
                                // because the guard above short-circuits every later tick and so
                                // nothing records again. An agent that only reads the message log
                                // must be told the retrying has ENDED; going quiet would replace an
                                // infinite retry with a silent nothing, which is worse (it would
                                // wait forever for a crossing that will never be attempted again).
                                gs.log_msg("zone", &format!(
                                    "Stopped trying to cross this zone line — {} attempts while \
                                     standing here did not cross (see zone_cross_stopped on \
                                     GET /v1/observe/debug). Step off every zone line and back on to \
                                     try again.",
                                    tally.count()));
                                tracing::info!("zone_cross: STOP — {} auto-cross attempts this stand \
                                                (last at region index={index}) did not cross; not \
                                                retrying until the character leaves every zone-line \
                                                region (#713)", tally.count());
                            }
                        }
                    }
                }
            }
        }

        // NOTE: server-initiated zone changes (GM #zone, portal doors, spell ports/gate/evac) are
        // answered by the gameplay.rs OP_REQUEST_CLIENT_ZONE_CHANGE handler, which echoes the
        // server's real zone_id via build_zone_change. This block USED to re-send via
        // send_zone_change_packet, but #199 changed that to always emit zoneID=0 (the resolve-from-
        // position sentinel, correct only for client-initiated WALK-IN crossings). That misrouted
        // every server-initiated teleport to a wrong zone (#235) — so it's removed; the wire
        // zoneID=0 path is now confined to /v1/move/zone_cross.
    }

    /// Resolve one drained `/v1/move/zone_cross` request (#725 review, B3).
    ///
    /// **The ticket is taken BY VALUE, and that is the point.** The obligation it carries is
    /// settled when this function returns — which is before the standing auto-cross in
    /// [`Self::drain_zone_cross`] runs, because the ticket is not in scope there and cannot be
    /// moved into it. The previous shape kept the ticket alive in `drain_zone_cross` and relied on
    /// a hand-placed `drop()` statement sitting above the auto-cross; moving that one statement to
    /// the end of the function was a mutation the ENTIRE SUITE passed, so the ordering was a
    /// convention held by a comment, not a fact. Now it is a fact about scope.
    ///
    /// Why the ordering matters at all, stated precisely — the comment this replaces overstated it.
    /// Of the three auto-cross outcomes, only `perform_cross`'s SAME-ZONE branch publishes anything
    /// (`request_stop()`), so it is the only one that could pay off a ticket the resolution had in
    /// fact dropped: an unrelated crossing firing from physical position, with no request behind
    /// it, silently discharging one. The cross-zone branch and `cross_unresolved` publish NOTHING,
    /// which is the hazard running the other way: a ticket dropped after either of them would see
    /// `moved == false` and stamp `idle`/`zone_cross_dropped_unhandled` over a crossing that is
    /// genuinely in flight — the backstop itself publishing the falsehood it exists to prevent.
    /// Both hazards disappear when the ticket simply does not exist down there.
    ///
    /// Takes no `stream`: resolution never sends a packet. It walks the character onto the line and
    /// the standing auto-cross does the sending, from physical position.
    ///
    /// **Do not change `ticket` to a reference.** The by-value parameter is the whole guarantee: it
    /// is what makes "settle the ticket after the auto-cross" fail to compile rather than merely be
    /// discouraged. Measured: as a one-line statement move it is now `E0308`.
    ///
    /// # AMENDED in review round 3 — the escape hatch is easier to reach than round 2 disclosed
    ///
    /// Round 2 disclosed the surviving mutation as *"a deliberate two-part refactor (signature
    /// changed to `&ZoneCrossTicket`, plus a late `drop`)"*. **That overstated how deliberate it has
    /// to be, and the round-3 reviewer corrected it in the unhelpful direction: no late `drop()` is
    /// needed at all.** The caller's binding is a local of `drain_zone_cross`, so it already outlives
    /// the auto-cross on its own. Two edits suffice, and both read as ordinary "avoid a move" tidying:
    ///
    /// ```ignore
    /// let held = self.command.take_zone_cross();
    /// if let Some(ref ticket) = held { self.resolve_zone_cross(ticket, gs); }
    /// ```
    ///
    /// Reproduced in round 3: that shape compiles and left the whole suite GREEN (39 / 175 / 372).
    /// It is now caught — not by a test that can observe the wrong outcome (round 2 was right that
    /// one cannot be written: it needs a resolution arm that publishes nothing, which is what the
    /// ticket exists to prevent, and both orderings leave the same final state) but by the
    /// `debug_assert_eq!` on `CommandState::zone_cross_outstanding()` at the top of the auto-cross
    /// block in [`Self::drain_zone_cross`]. Measured against exactly the two-edit shape above: RED
    /// in 6 existing `eqoxide-net` tests; GREEN with no false positives on the unmutated branch.
    /// That is a dynamic backstop for a static fact, and it is deliberately weaker than the type —
    /// keep the by-value parameter, which is what makes the accidental case impossible rather than
    /// merely detected.
    fn resolve_zone_cross(&mut self, ticket: eqoxide_command::ZoneCrossTicket, gs: &mut GameState) {
        let want_zone = ticket.zone_id();
        // #713 item 2 — the best-effort marker describes THE LAST RESOLUTION, so every resolution
        // starts by clearing it. Without this, a request that degrades to the server-resolved
        // fallback would leave `zone_cross_best_effort` set over a LATER request that resolved to a
        // properly advertised destination — an observable with no clear-on-event path, which is the
        // #343 `connected: true` failure mode (a field that latches on and never latches off).
        // The other clear is zone entry (`GameState::begin_zone_in`).
        gs.zone_cross_plan = None;
        // #600 — THE ONE DECISION FUNCTION, at the THIRD world-answering consumer. The resolution
        // below reads a collision grid AND publishes an agent-observable nav_state. While the
        // zone's assets are not usable — still loading (no grid yet, ~10s), a failed load, or the
        // previous zone's grid in the ~1-frame stale window — `located` would be `None` and the old
        // code published `no_path` ("DEFINITIVE: no route exists, do not retry"). That is a
        // confident falsehood: the cross may be perfectly reachable once the world finishes
        // loading, and telling the agent to give up permanently is WORSE than the walker case. Gate
        // it through `usable_collision` (which delegates the verdict to `usability`, so this is
        // still that one predicate) and answer the honest transient `zone_loading` with the
        // predicate's own verdict (the SAME
        // vocabulary the HTTP surface and the walker publish). RE-QUEUE the one-shot request so it
        // resolves for real once the correct zone's assets land — mirroring the `zone_loading`
        // contract the walker's kept goto already honours. `Ok` (usable) ⇒ the grid resolved below
        // is the right zone's, and a genuine `no_path` there is truthful.
        //
        // #827 — the verdict AND the grid it blesses come from ONE call. This used to ask
        // `usability` for permission here and then read `self.collision`, a SEPARATE shared slot,
        // at the lookup below. Nothing coupled them, and `begin_zone_load` empties the collision
        // slot BEFORE it publishes `Pending`, so a zone change landing between the two reads paired
        // a usable verdict with an emptied slot; the lookup was then `None` for want of a grid and
        // the `(None, None)` arm reported `zone_line_not_in_map` — a claim about the contents of a
        // map that was never read. #829's reviewer CONSTRUCTED that pairing (issue #827 comment 1):
        // a genuinely usable verdict over an emptied slot produced `no_path` /
        // `zone_line_not_in_map` on demand. (Whether a real session's interleaving reaches it is
        // still reasoned from the write order, not measured — the probe set the end state directly.)
        // `usable_collision` returns the `Arc<Collision>` that `Ready` OWNS, in the same call as
        // the verdict, so the resolution below cannot be reached without the grid the gate just
        // vouched for. The `Arc` is CLONED out so the zone-asset lock is released here, not held
        // across the lookup and publish.
        let usable = {
            let st = eqoxide_nav::zone_assets::lock_state(&self.zone_assets);
            eqoxide_nav::zone_assets::usable_collision(&st, &gs.world.zone_name)
                .map(std::sync::Arc::clone)
        };
        let collision = match usable {
            Err(why) => {
                // Re-queue the one-shot request FIRST so the cross resolves for real once the correct
                // zone's assets land (mirroring the kept-goto `zone_loading` contract). `request_zone_
                // cross` stamps a transient `pending` on the SHARED nav_state; publish the honest
                // `zone_loading` AFTER it so the load-window state is the accurate one, not `pending`'s
                // ≤150ms promise. The queued request also keeps `resolve_goal` from resetting this
                // `zone_loading` back to `idle` (see its zone-cross guard) — so the state is STABLE
                // across the whole load, not a one-tick flash.
                self.command.request_zone_cross(want_zone);
                self.walker.set_nav_state_because(
                    eqoxide_nav::walker::NAV_STATE_ZONE_LOADING, Some(why.as_str()));
                return;
            }
            Ok(collision) => collision,
        };
        // want_zone != 0 → resolve it to a zone-point index; want_zone == 0 → any nearest line.
        let want_index = if want_zone != 0 {
            match self.world.zone_points.lock().unwrap().iter()
                .find(|zp| zp.zone_id == want_zone).map(|zp| zp.iterator as i32)
            {
                Some(idx) => Some(idx),
                None => {
                    tracing::info!("zone_cross: no zone point advertised for zone_id={want_zone}");
                    gs.log_msg("zone", "No zone line found to cross");
                    // Make the failure observable instead of a silent no-op (#267): a caller that
                    // got 200 from POST /zone_cross can poll nav_state and see it didn't happen.
                    // With a REASON — a terminal state with `nav_reason: null` contradicts the
                    // contract this PR documents (#377 review, N2). Reached only when the zone IS
                    // usable (gated above), so this is a truthful definitive no, not a load artefact.
                    self.walker.set_nav_state_because("no_path", Some("no_zone_line_to_zone"));
                    None
                }
            }
        } else {
            None // any zone line
        };
        // Only proceed if we actually have a target (want_zone==0 always may; want_zone!=0 needs a match).
        if want_zone == 0 || want_index.is_some() {
            // Locate the NEAREST reachable zone-line region for the wanted zone (not the first
            // zone-point index that matches — a zone with several lines to the same target, or an
            // in-zone translocator with multiple advertised points, would otherwise pick one with
            // no nearby region and no-op, #266). want_index==None → any nearest line.
            //
            // #815: the lookup and "could this grid read region data AT ALL?" are taken from
            // the SAME grid. #827 makes that grid the one the gate above blessed — the `Arc`
            // out of `usable_collision`, not a fresh `self.collision.read()`. There is no
            // second slot left to straddle a zone change, and no `Option` left to be `None`
            // for want of a grid: both values below are derived from one non-optional
            // `Collision`, so the `(None, None)` arm can only mean "the region map was read
            // and has no matching region".
            let c = &collision;
            let region_absent = c.region_data_absent().cloned();
            let located = {
                let pos = [gs.player_x, gs.player_y, gs.player_z];
                match (want_zone, want_index) {
                    (0, _) => c.find_zone_line_near(None, pos),
                    (_, _) => {
                        // Every zone-point index advertised for `want_zone`, nearest region wins.
                        let idxs: Vec<i32> = self.world.zone_points.lock().unwrap().iter()
                            .filter(|zp| zp.zone_id == want_zone).map(|zp| zp.iterator as i32).collect();
                        idxs.iter()
                            .filter_map(|&idx| c.find_zone_line_near(Some(idx), pos))
                            .min_by(|a, b| {
                                let da = (a.1[0]-pos[0]).hypot(a.1[1]-pos[1]);
                                let db = (b.1[0]-pos[0]).hypot(b.1[1]-pos[1]);
                                da.total_cmp(&db)
                            })
                            // #683: `want_zone` IS advertised but no located region carries
                            // any of its indices (qrg: the only exit region is baked with
                            // index 0). Fall back to the nearest UNRESOLVABLE zone line —
                            // one whose index matches no advertised destination — and let
                            // the standing auto-cross + server echo determine the real
                            // destination. ONLY unresolvable lines are candidates: a region
                            // whose index resolves to an advertised zone point is KNOWN to
                            // lead to that OTHER zone, and walking onto it would cross the
                            // caller somewhere it never asked to go (qeynos: a zone_cross
                            // to qeynos2, whose gate lines are baked index 0, must not walk
                            // onto the nearer RESOLVABLE catacombs line and cross to qcat).
                            // Gated exactly like the auto-cross fallback (see
                            // `classify_unresolved_cross`): never in a zone that advertises
                            // a same-zone pad, and never before zone points arrive — so
                            // this can't route a character onto an intra-zone pad (#679).
                            .or_else(|| {
                                let (allowed, resolvable): (bool, Vec<i32>) = {
                                    let zps = self.world.zone_points.lock().unwrap();
                                    (classify_unresolved_cross(&zps, gs.world.zone_id)
                                         == UnresolvedCross::SendServerResolved,
                                     zps.iter().filter(|zp| zp.zone_id != 0)
                                        .map(|zp| zp.iterator as i32).collect())
                                };
                                if !allowed { return None; }
                                // #803/#815: `zone_line_indices` is a `Result`. This fallback
                                // only ever ASKS for candidates, so a grid with no region data
                                // has none to offer and this returns `None` — the same outcome
                                // the old `unwrap_or_default()` produced. The discard is spelled
                                // out as a `let … else` rather than laundered through
                                // `unwrap_or_default()`, which turns a failed read into an empty
                                // success and is exactly what made the reason below wrong. The
                                // reason itself is NOT dropped: it is sampled as `region_absent`
                                // beside this lookup, and the `None` arm reports it.
                                let Ok(indices) = c.zone_line_indices() else { return None; };
                                indices.into_iter()
                                    .filter(|idx| !resolvable.contains(idx))
                                    .filter_map(|idx| c.find_zone_line_near(Some(idx), pos))
                                    .min_by(|a, b| {
                                        let da = (a.1[0]-pos[0]).hypot(a.1[1]-pos[1]);
                                        let db = (b.1[0]-pos[0]).hypot(b.1[1]-pos[1]);
                                        da.total_cmp(&db)
                                    })
                            })
                    }
                }
            };
            // #815: matched as a PAIR, so every combination of "did we find a region" and
            // "could this grid have had one" has its own arm and the compiler checks the
            // enumeration. The alternative — a guard arm plus an `unwrap` — would put a panic
            // path in the net thread's drain loop to re-establish something already known.
            match (located, region_absent) {
                (Some((index, [tx, ty, tz])), _) => {
                    // Destination zone for logging — resolve the located region's index
                    // against the advertised list. `None` = an UNADVERTISED index (the #683
                    // fallback): the destination is genuinely unknown until the server
                    // resolves the crossing, so say that — never claim the requested zone,
                    // which the server may tie-break elsewhere (agent-honesty). The
                    // `zone_id != 0` filter mirrors `resolve_cross_destination`: a zone
                    // point advertising zone_id 0 is the "server resolves from position"
                    // SENTINEL (qrg ships one with iterator 0), not a destination — without
                    // the filter this claimed "walking to the zone_id=0 line" (seen live).
                    let known_dest = self.world.zone_points.lock().unwrap().iter()
                        .find(|zp| zp.iterator as i32 == index && zp.zone_id != 0).map(|zp| zp.zone_id);
                    // #713 item 2 (the #683 review's F4) — THE STRUCTURED MARKER. Everything
                    // below (the trace label, the message-log line, and the machine-readable
                    // `zone_cross_best_effort` disclosure) is derived from this ONE value, so
                    // the prose and the observable cannot drift apart: if the agent is told
                    // "resolved by server" in the log, the field says so too, by construction.
                    let resolution = match known_dest {
                        Some(z) => ZoneCrossResolution::Advertised { zone_id: z },
                        None => ZoneCrossResolution::ServerResolved,
                    };
                    gs.zone_cross_plan = Some(ZoneCrossPlan {
                        requested_zone_id: (want_zone != 0).then_some(want_zone),
                        index,
                        resolution,
                    });
                    let dest_label = match resolution {
                        ZoneCrossResolution::Advertised { zone_id } => format!("zone_id={zone_id}"),
                        ZoneCrossResolution::ServerResolved => "server-resolved-destination".to_string(),
                    };
                    // ALWAYS walk to the located line. There is deliberately no "close
                    // enough, skip it" shortcut here (#725).
                    //
                    // There used to be one: `if d2 <= 15.0*15.0 { tracing::info!(…) }`, whose
                    // entire body was that log line, on the belief that the standing
                    // auto-cross below would "fire this tick". It gates on
                    // `zone_line_at_standing` — the capsule must be physically INSIDE the
                    // DRNTP region — so the two conditions never met, and every request from
                    // between the region and 15.0u evaporated: no goto, no packet, nothing
                    // published, `nav_state` left at `pending` forever. Measured: 11.87u /
                    // 12.90u / 14.66u dropped 100%; 19.54u / 31.57u crossed in under a
                    // second. Worse, the server's walk-in ARRIVAL point sits inside that band
                    // for the RETURN line, so the natural agent loop (walk in, ask to go
                    // back) failed by construction.
                    //
                    // Re-tuning the threshold would only move the band. Testing
                    // `zone_line_at_standing` here would close it, but leaves two conditions
                    // that must agree forever. Walking is unconditionally correct instead:
                    // `find_zone_line_near` returns a point PROJECTED onto the walkable floor
                    // and re-verified to still be inside the region
                    // (`zone_line_floor_point`), so arriving there IS standing in the trigger
                    // volume; and if we are already inside it, the goal is metres away, the
                    // walker reports `arrived` at once, and the auto-cross below fires from
                    // physical position exactly as it would have anyway. One path, one
                    // publish (`request_goto` stamps a fresh goal id + `nav_goal`), no band.
                    let dist = (tx - gs.player_x).hypot(ty - gs.player_y);
                    tracing::info!("zone_cross: walking {dist:.1}u to the {dest_label} line at ({tx:.0},{ty:.0}) (index={index})");
                    match resolution {
                        ZoneCrossResolution::Advertised { zone_id } =>
                            gs.log_msg("zone", &format!("Walking to the zone {} line", zone_id)),
                        // Prose in a message log is not a machine-readable signal (#713 F4):
                        // this line is now the HUMAN half of a degradation whose machine half
                        // is `zone_cross_best_effort` on GET /v1/observe/debug.
                        ZoneCrossResolution::ServerResolved =>
                            gs.log_msg("zone", "Walking to a zone line (destination resolved by server — best effort, see zone_cross_best_effort on GET /v1/observe/debug)"),
                    }
                    self.command.request_goto((tx, ty, tz));
                }
                // #815: WHY there is no located region decides what we may say. The gate above
                // establishes that the grid is THIS zone's and (since #827) that there IS a grid
                // — not that the grid has any region data to have looked in. Both cases used to
                // answer `zone_line_not_in_map`, documented verbatim as "the locally loaded zone
                // geometry has no matching WLD zone-line (DRNTP) trigger region" — a claim
                // about the CONTENTS of a file that, in the second case, was never read.
                //
                // They call for opposite operator action: a genuine gap means route another
                // way / re-bake this exit, while an unread region map means every exit in this
                // zone is unknown and the assets need re-syncing. One string could not carry
                // both, and no other field recovered the difference.
                (None, Some(absent)) => {
                    tracing::info!(
                        "zone_cross: region map unavailable ({absent}) — whether zone_id={want_zone} \
                         has a zone-line region in this zone is UNKNOWN, not absent");
                    gs.log_msg("zone", &format!(
                        "Cannot read this zone's region map ({absent}) — whether a zone line to cross is here is UNKNOWN"));
                    // The SAME machine-readable cause `/v1/observe/zone_exits` refuses with
                    // (`RegionDataAbsent::as_str`), so one question has one answer across the
                    // two surfaces, and "the pack never delivered this file" stays distinct
                    // from "the file is truncated". Same shape as the `zone_loading` refusal
                    // above, which publishes `NotUsable::as_str()`.
                    //
                    // `no_path` is kept as the STATE: a region map that failed to load does
                    // not resolve itself by polling (#803's refusal says so explicitly), so
                    // this is terminal for this zone, not the transient `zone_loading`. What
                    // changes is the reason, which no longer names a cause the client did not
                    // observe.
                    self.walker.set_nav_state_because("no_path", Some(absent.as_str()));
                }
                (None, None) => {
                    tracing::info!("zone_cross: no zone-line region found for zone_id={want_zone}");
                    gs.log_msg("zone", "No zone line found to cross");
                    // Advertised in OP_SendZonepoints but no DRNTP region in the loaded map (a .wtr
                    // gap): report it so the caller isn't left thinking the 200 meant success (#267).
                    // Gated above, so the grid IS this zone's.
                    //
                    // #827: this arm used to be TWO states, and only the first was what the
                    // reason claims — (a) the grid's region map LOADED and genuinely has no
                    // matching region, versus (b) the shared collision slot held no grid at
                    // all, so nothing was read. (b) is gone by construction, not by a guard:
                    // the grid is the `Arc` `usable_collision` handed back with the verdict, so
                    // there is no `Option` here to be `None` for want of a grid, and no second
                    // slot for a concurrent `begin_zone_load` to empty between the two reads.
                    // Reaching this arm therefore means `region_data_absent()` said the map is
                    // present — data actually read — so the claim is about a file this client
                    // really did open.
                    self.walker.set_nav_state_because("no_path", Some("zone_line_not_in_map"));
                }
            }
        }
    }

    fn drain_chat(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Check hail request — say "Hail, <name>" so the NPC fires its hail script. The server only
        // runs an NPC's EVENT_SAY on the player's CURRENT TARGET (client.cpp: `Mob* t = GetTarget()`),
        // so we must target the NPC FIRST, in the same tick and before the say packet, or the hail is
        // silently ignored (#130). The target packet precedes the say on the ordered stream, so the
        // server has GetTarget()==the NPC when it processes the say.
        let hail_req = self.command.take_hail();
        if let Some((name, spawn_id)) = hail_req {
            // A hail starts a FRESH interaction — drop any saylink choices left over from a prior
            // NPC (or a system/command message). Otherwise `/observe/dialogue` leaks the last
            // choices indefinitely, since they're only ever overwritten when a new say-line carries
            // saylinks and never cleared (#274). The hailed NPC's own reply repopulates them.
            gs.dialogue_choices.clear();
            if let Some(id) = spawn_id {
                gs.set_target(id); // also clears stale con/attitude from any prior target (#323)
                stream.send_app_packet(OP_TARGET_MOUSE, &build_target_packet(id));
            }
            let msg = format!("Hail, {}", name);
            let pkt = build_say_packet(&gs.player_name, &name, &msg);
            tracing::info!("EQ: hailing '{}' (target={:?}, say): {}", name, spawn_id, msg);
            stream.send_app_packet(OP_CHANNEL_MESSAGE, &pkt);
            let line = format!("You say, '{}'", msg);
            gs.log_msg("chat", &line);
        }

        // Check say request — arbitrary Say text (HUD say box / quest keyword follow-up).
        let say_text = self.command.take_say();
        if let Some(text) = say_text {
            // The `/camp` chat keyword is a local command, not Say text: toggle a camp instead of
            // broadcasting it. The gameplay loop drains the camp slot and runs the camp/cancel.
            if text.trim().eq_ignore_ascii_case("/camp") {
                self.command.request_camp(CampCmd::Toggle);
                tracing::info!("EQ: /camp chat command — toggling camp");
            } else {
                let pkt = build_say_packet(&gs.player_name, "", &text);
                tracing::info!("EQ: say: {}", text);
                stream.send_app_packet(OP_CHANNEL_MESSAGE, &pkt);
                let line = format!("You say, '{}'", text);
                gs.log_msg("chat", &line);
            }
        }

        // Check dialogue-click request (POST /v1/interact/dialogue, or a GUI click): "click" a
        // parsed saylink by sending OP_ItemLinkClick with its ids. The server resolves the phrase
        // from its saylink table and processes it as if we said it to the NPC (#120).
        let click = self.command.take_dialogue_click();
        if let Some(c) = click {
            let pkt = build_item_link_click(c.item_id, &c.augments, c.link_hash, c.icon);
            tracing::info!("EQ: dialogue click: '{}' (sayid={})", c.text, c.augments[0]);
            stream.send_app_packet(OP_ITEM_LINK_CLICK, &pkt);
            let line = format!("You say, '{}'", c.text);
            gs.log_msg("chat", &line);
        }

        // Drain queued outgoing chat (POST /tell|/ooc|/shout|/group): build + send OP_ChannelMessage.
        // #446: both the Chat window and the POST handlers write through the shared
        // `CommandState::request_chat_send` verb now; this drains the whole FIFO queue at once via
        // `take_chat_send` (same `std::mem::take` behavior the raw slot drain had).
        let outgoing: Vec<eqoxide_ipc::ChatSend> = self.command.take_chat_send();
        for c in outgoing {
            let pkt = build_channel_message(&gs.player_name, &c.to, c.chan, &c.text);
            stream.send_app_packet(OP_CHANNEL_MESSAGE, &pkt);
            let label = match c.chan { 7 => format!("tell {}", c.to), 5 => "ooc".into(),
                                       3 => "shout".into(), 2 => "group".into(), 0 => "guild".into(),
                                       n => format!("chan{n}") };
            tracing::info!("EQ: {} -> {}", label, c.text);
            // Native-style local echo, logged under the channel's kind so the chat window
            // tab-filters and colors it like the matching incoming traffic.
            let (kind, line): (&str, String) = match c.chan {
                7 => ("tell",  format!("You told {}, '{}'", c.to, c.text)),
                5 => ("ooc",   format!("You say out of character, '{}'", c.text)),
                3 => ("shout", format!("You shout, '{}'", c.text)),
                2 => ("group", format!("You tell your party, '{}'", c.text)),
                0 => ("guild", format!("You say to your guild, '{}'", c.text)),
                _ => ("chat",  format!("You {}: {}", label, c.text)),
            };
            gs.log_msg(kind, &line);
        }
    }

    fn drain_target(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Check target request — set target + auto-consider it (con color comes back as
        // an OP_CONSIDER reply, handled in packet_handler). GameState::set_target seeds
        // target_name/target_hp_pct (name/HP — update_hp/update_hp_pct then keep target_hp_pct
        // live as combat HP updates arrive) AND clears target_con/target_con_name/
        // target_attitude so the PREVIOUS target's con can't survive a re-target (eqoxide#323).
        let target_id = self.command.take_target();
        if let Some(id) = target_id {
            // Never adopt a spawn that isn't in the zone. POST /v1/combat/target 404s on an unknown
            // id, but the entity could still despawn between the HTTP check and this drain — and the
            // server silently IGNORES an OP_TargetMouse for an unknown id, so calling set_target
            // anyway would leave the client believing in a target the server never set. Say so
            // instead of lying. The player's own spawn is legal and is absent from `entities`. (#348)
            if id != gs.player_id && !gs.world.entities.contains_key(&id) {
                let text = format!("Cannot target spawn {id}: it is not in this zone.");
                gs.log_msg("combat", &text);
                gs.push_event("combat", "target_failed", "", true, &text);
                tracing::info!("EQ: target spawn_id={} REFUSED — not in the entity list", id);
            } else {
                gs.set_target(id);
                stream.send_app_packet(OP_TARGET_MOUSE, &build_target_packet(id));
                stream.send_app_packet(OP_CONSIDER, &build_consider_packet(gs.player_id, id));
                tracing::info!("EQ: target spawn_id={} + consider", id);
            }
        }
    }

    // #446: GET /v1/observe/who and the /v1/social/friends presence poll now register their
    // oneshot senders through the shared `CommandState::request_who`/`request_friends_who` verbs,
    // and this drain reads them back via `take_who_req`/`take_friends_req`.
    fn drain_who_friends(&mut self, stream: &mut EqStream) {
        // Check /who all request (#300) — send OP_WhoAllRequest (server-wide, type=3); the oneshot
        // sender is held in `pending_who` until OP_WhoAllResponse arrives (see `fulfill_who`). A newer
        // request supersedes an in-flight one (its sender drops → that GET times out).
        if let Some(tx) = self.command.take_who_req() {
            stream.send_app_packet(OP_WHO_ALL_REQUEST, &build_who_all_request(3));
            self.pending_who = Some(tx);
            self.expecting_friends = false; // the next OP_WhoAllResponse is a /who all, not a friends poll
            tracing::info!("EQ: sent OP_WhoAllRequest (/who all)");
        }

        // Check friends-presence request (#301) — send OP_FriendsWho with the client-local friends
        // string; the reply arrives as OP_WhoAllResponse (online subset), routed to `fulfill_friends`
        // by the `expecting_friends` flag. Mirrors the /who all path above.
        if let Some(tx) = self.command.take_friends_req() {
            let names = self.social.friends_list.lock().unwrap().clone();
            stream.send_app_packet(OP_FRIENDS_WHO, &build_friends_who(&names));
            self.pending_friends = Some(tx);
            self.expecting_friends = true;
            tracing::info!("EQ: sent OP_FriendsWho ({} friend(s))", names.len());
        }
    }

    // #446: the HUD attack button and POST /v1/combat/attack now both write through the shared
    // `CommandState::request_attack` verb, and this drain reads it back via `take_attack` — one
    // typed surface over the slot instead of two call sites poking the raw `Arc<Mutex<..>>`.
    fn drain_combat(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Check attack request — send OP_AUTO_ATTACK(1) to start, OP_AUTO_ATTACK(0) to stop.
        // Server expects exactly 4 bytes; byte[0]=1 enables, byte[0]=0 disables.
        let attack_req = self.command.take_attack();
        if let Some(on) = attack_req {
            self.auto_attack = on;
            let payload = [if on { 1u8 } else { 0u8 }, 0, 0, 0];
            stream.send_app_packet(OP_AUTO_ATTACK, &payload);
            gs.auto_attack = on;
            tracing::info!("EQ: auto-attack {}", if on { "ON" } else { "OFF" });
        }
    }

    fn drain_pet(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/pet/command or a Pet-window button: send one OP_PetCommands for the player's
        // pet. PET_ATTACK aims at the current target (like the auto-pet path); every other command
        // (back off / follow / guard / sit) targets 0 — the server acts on the pet itself.
        let pet_cmd = self.command.take_pet_command();
        if let Some(cmd) = pet_cmd {
            let cmd = cmd as u32;
            if gs.pet_id.is_none() {
                gs.log_msg("pet", "You have no pet");
            } else if cmd == PET_ATTACK {
                match gs.target_id.filter(|&t| t != 0) {
                    Some(tid) => {
                        stream.send_app_packet(OP_PET_COMMANDS, &build_pet_command(PET_ATTACK, tid));
                        // Keep the auto-pet-combat dedupe in sync so it doesn't immediately
                        // re-issue (or back-off-cancel) the manual order.
                        self.last_pet_target = Some(tid);
                        tracing::info!("EQ: pet command attack → target {tid}");
                        gs.log_msg("pet", "Pet attack ordered");
                    }
                    None => gs.log_msg("pet", "Pet attack: no target"),
                }
            } else {
                stream.send_app_packet(OP_PET_COMMANDS, &build_pet_command(cmd, 0));
                if cmd == PET_BACKOFF { self.last_pet_target = None; }
                tracing::info!("EQ: pet command {cmd}");
                gs.log_msg("pet", &format!("Pet command sent ({})", match cmd {
                    PET_BACKOFF => "back off", PET_FOLLOWME => "follow",
                    PET_GUARDHERE => "guard here", PET_SIT => "sit", _ => "other",
                }));
            }
        }
    }

    fn drain_read_book(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/interact/read {"slot":N}: read a book/note. Look up the item at that wire slot;
        // if it carries a Filename it's readable, so send OP_ReadBook with that filename and the
        // server replies with the text (apply_read_book stores it → GET /v1/observe/item_text). (#288)
        let read_slot = self.command.take_read_book();
        if let Some(slot) = read_slot {
            match gs.inventory.iter().find(|i| i.slot == slot) {
                Some(item) if !item.filename.is_empty() => {
                    let pkt = build_read_book_packet(slot as i16, gs.player_id, &item.filename);
                    stream.send_app_packet(OP_READ_BOOK, &pkt);
                    tracing::info!("EQ: read book slot={} file='{}'", slot, item.filename);
                }
                Some(_) => gs.log_msg("book", &format!("Item in slot {slot} is not readable")),
                None    => gs.log_msg("book", &format!("No item in slot {slot} to read")),
            }
        }
    }

    // #446: POST /v1/guild/* now writes through the shared `CommandState::request_guild_action`
    // verb (which also preserves the original "one pending action at a time" CONFLICT check), and
    // this drain reads it back via `take_guild_action`.
    fn drain_guild(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/guild/{invite,accept,leave,remove}: one queued guild action → the matching RoF2
        // guild opcode. Invite/remove/leave share GuildCommand_Struct; accept replies to a captured
        // pending invite with GuildInviteAccept_Struct. (#295)
        let guild_action = self.command.take_guild_action();
        if let Some(action) = guild_action {
            const GUILD_RECRUIT: u32 = 8; // default rank for a fresh invite (RoF2 0-8 scale)
            match action {
                eqoxide_ipc::GuildAction::Invite(name) => {
                    let pkt = build_guild_command(&name, &gs.player_name, gs.player_guild_id, GUILD_RECRUIT);
                    stream.send_app_packet(OP_GUILD_INVITE, &pkt);
                    gs.log_msg("guild", &format!("Inviting {name} to the guild"));
                    tracing::info!("EQ: guild invite -> {name}");
                }
                eqoxide_ipc::GuildAction::Remove(name) => {
                    let pkt = build_guild_command(&name, &gs.player_name, gs.player_guild_id, 0);
                    stream.send_app_packet(OP_GUILD_REMOVE, &pkt);
                    gs.log_msg("guild", &format!("Removing {name} from the guild"));
                    tracing::info!("EQ: guild remove -> {name}");
                }
                eqoxide_ipc::GuildAction::Leave => {
                    // Self-leave: othername == myname.
                    let pkt = build_guild_command(&gs.player_name, &gs.player_name, gs.player_guild_id, 0);
                    stream.send_app_packet(OP_GUILD_REMOVE, &pkt);
                    gs.log_msg("guild", "Leaving guild");
                    tracing::info!("EQ: guild leave");
                }
                eqoxide_ipc::GuildAction::Accept => match gs.pending_guild_invite.take() {
                    Some((inviter, guild_id, rank)) => {
                        let pkt = build_guild_invite_accept(&inviter, &gs.player_name, rank, guild_id);
                        stream.send_app_packet(OP_GUILD_INVITE_ACCEPT, &pkt);
                        gs.log_msg("guild", &format!("Accepting guild invite from {inviter}"));
                        tracing::info!("EQ: guild accept from {inviter} (guild_id={guild_id})");
                    }
                    None => gs.log_msg("guild", "No pending guild invite to accept"),
                },
            }
        }
    }

    fn drain_cast(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Cast a memorized spell gem (FIRE-AND-FORGET UI path). Target priority, ST_SELF/beneficial
        // self-targeting, empty-gem / stale-clicky refusal, and the wire packet all live in
        // `send_cast` now (shared with the awaited path below so both emit byte-identical traffic).
        // The fire-and-forget path ignores the returned `CastSend`: a never-started refusal records
        // `finish_cast(cast_failed)` inside `send_cast` for the agent to read (#348) — but ONLY when no
        // awaited cast is parked. That write lands in `gs.last_cast`, and a parked awaited cast's
        // `fulfill_cast` correlates on `last_cast`, so a UI never-started write during a park would
        // resolve the awaited cast as a bogus `Refused` (#448 review). `pending_cast.is_none()`
        // gates the write off exactly when it could cross-talk; the wire packet is unaffected.
        if let Some(req) = self.command.take_cast() {
            let _ = send_cast(stream, gs, req, self.pending_cast.is_none());
        }

        // Cast (AWAITED, honest Command-with-result path — POST /v1/combat/cast, #448): emit the SAME
        // wire traffic as the fire-and-forget path above via `send_cast`, then act on whether the cast
        // STARTED. A never-started refusal (empty gem / stale clicky) is `Refused` IMMEDIATELY — the
        // cast definitively did not happen, so 409 is honest, not a 202 "unknown". A started cast PARKS
        // the HTTP-side `oneshot::Sender` in `pending_cast`; the resolving outcome is fired later by
        // `fulfill_cast` when `gs.last_cast` transitions (completed → Resolved(completed), fizzle/
        // interrupt → Resolved(that outcome), a server refusal after parking → Refused, an unexplained
        // end → Unconfirmed), or a zone change / HTTP timeout → Unconfirmed.
        //
        // SINGLETON-in-flight (reject-while-parked): casting is naturally serial (one cast bar), and a
        // cast's terminal outcome carries NO per-request token, so two awaited casts in flight at once
        // would be indistinguishable at the `last_cast` transition — the first's outcome could resolve
        // the second caller's Sender. So when a cast is already parked we do NOT supersede it and do
        // NOT send any wire packet: we immediately answer the NEW request `Refused`. Because no packet
        // went out, the cast DEFINITIVELY did not happen — 409, not a 202 "unknown". See
        // `eqoxide_command::result` for the discipline shared with buy/give. (Known residual: a UI
        // fire-and-forget cast concurrent with a parked awaited cast could still have its outcome
        // resolve the awaited cast — but the server serialises the cast bar, so a second cast can't
        // even start until the first frees it; very low likelihood, and it cannot fabricate success.)
        if let Some((req, tx)) = self.command.take_cast_await() {
            if self.pending_cast.is_some() {
                let _ = tx.send(eqoxide_command::CommandResult::Refused(
                    "a cast is already in flight; retry after it resolves".into()));
                tracing::info!("EQ: awaited cast REJECTED — one already in flight");
            } else {
                // `record_never_started = false`: the awaited path reports a never-started refusal via
                // its own `Sender` (below), so it does not need — and must not make — the `last_cast`
                // write, keeping the cast machinery's published outcome untouched by a queued refusal.
                match send_cast(stream, gs, req, false) {
                    CastSend::Started => {
                        self.pending_cast = Some(PendingCast { tx, sent_at: Instant::now() });
                        tracing::info!("EQ: awaited cast parked — awaiting outcome");
                    }
                    CastSend::NeverStarted(reason) => {
                        let _ = tx.send(eqoxide_command::CommandResult::Refused(reason));
                    }
                }
            }
        }
    }

    fn drain_mem_spell(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Scribe a scroll into the spellbook (scribing=0) or memorize a known spell into a gem
        // (scribing=1) — OP_MemorizeSpell. The server validates (you hold the scroll / know the
        // spell) and pushes OP_MemorizeSpell back, which updates gs.mem_spells for the gem case.
        let mem_req = self.command.take_mem_spell();
        if let Some((slot, spell_id, scribing, from)) = mem_req {
            // Scribing (0) only takes effect on the scroll sitting on the CURSOR: the RoF2 server
            // reads m_inv[slotCursor] and ignores the packet otherwise (silent fail, eqoxide#11).
            // So move the scroll from its inventory slot → cursor first (same tick; the server
            // processes packets in order, so the cursor holds the scroll when the scribe arrives).
            if scribing == 0 {
                if let Some(from_slot) = from {
                    if from_slot != SLOT_CURSOR {
                        stream.send_app_packet(OP_MOVE_ITEM, &build_move_item(from_slot, SLOT_CURSOR));
                        self.mirror_move_item(gs, from_slot as i32, SLOT_CURSOR as i32);
                        tracing::info!("EQ: scribe — moved scroll slot {} → cursor", from_slot);
                    }
                }
            }
            stream.send_app_packet(OP_MEMORIZE_SPELL, &build_memorize_packet(slot, spell_id, scribing));
            if scribing == 0 {
                // The RoF2 server CONSUMES the scribed scroll: OPMemorizeSpell's memSpellScribing
                // case runs ScribeSpell(...) then DeleteItemInInventory(slotCursor) (zone/
                // client_process.cpp). We already moved the scroll to the cursor above, so mirror
                // that deletion locally — otherwise the (now server-deleted) scroll stays stuck on
                // cursor slot 33 in our view, blocking looting and any later cursor move (#271). No
                // OP_DeleteItem is sent: the server already removed it, so that would double-delete.
                self.mirror_remove_item(gs, SLOT_CURSOR as i32);
            }
            let what = match scribing { 0 => "scribe", 1 => "memorize", _ => "unmem" };
            tracing::info!("EQ: {what} spell={spell_id} slot={slot}");
            gs.log_msg("spell", &format!("{what} spell {spell_id} (slot {slot})"));
        }
    }

    fn drain_sit(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Sit / stand (OP_SpawnAppearance type=14, param 110/100).
        let sit_req = self.command.take_sit();
        if let Some(sit) = sit_req {
            let param = if sit { 110u32 } else { 100u32 };
            let payload = build_spawn_appearance_packet(gs.player_id as u16, 14, param);
            stream.send_app_packet(OP_SPAWN_APPEARANCE, &payload);
            gs.sitting = sit;
            tracing::info!("EQ: {}", if sit { "sit" } else { "stand" });
        }
    }

    /// Run/walk toggle (#625). Sends `OP_SetRunMode` (0x009f) once per change and mirrors the
    /// requested state into `gs.run_mode`, which the nav walker (`eqoxide_nav::walker::nav_speed`)
    /// reads to pick `RUN_SPEED` vs `WALK_SPEED` for `/goto`'s LOCAL controller speed — this opcode
    /// carries no ack, so `gs.run_mode` is our own send-time intent, not a server confirmation (see
    /// the field's doc comment). Scoped to nav-driven movement only for this PR; manual WASD/melee
    /// auto-engage movement still always drives at `RUN_SPEED` (unchanged, out of scope — see the
    /// PR body). Kept as its own drain fn (not folded into `drain_sit`) so this file's #625 diff
    /// stays a clean, single-purpose addition alongside any concurrent nav/action_loop work in flight.
    fn drain_run_mode(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        if let Some(run) = self.command.take_run_mode() {
            stream.send_app_packet(OP_SET_RUN_MODE, &build_set_run_mode_packet(run));
            gs.run_mode = run;
            tracing::info!("EQ: run mode -> {}", if run { "run" } else { "walk" });
        }
    }

    fn drain_consider(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Standalone consider.
        let con_req = self.command.take_consider();
        if let Some(id) = con_req {
            stream.send_app_packet(OP_CONSIDER, &build_consider_packet(gs.player_id, id));
            tracing::info!("EQ: consider spawn_id={}", id);
        }
    }

    fn drain_merchant(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Merchant buy (FIRE-AND-FORGET UI path): open the merchant (OP_ShopRequest) then buy its
        // inventory slot (OP_ShopPlayerBuy). Sent in sequence — the server processes the open first
        // so the merchant is open by the time the buy arrives. Must be within ~200u of the merchant.
        // The packet-emit + in-flight bookkeeping is shared with the awaited path below via
        // `send_shop_buy`, so both emit byte-identical wire traffic (#448).
        //
        // No optimistic "Bought item" log and no local spend_coin at send time (#345, generalizing
        // the #269 sell fix): the server can refuse — out-of-range/bad merchant/qty, a stale slot,
        // or insufficient funds — with NO OP_ShopPlayerBuy echo at all, and the insufficient-funds
        // case sends nothing whatsoever, so a buy can fail silently server-side. Deducting coin or
        // logging success at send time would fabricate a purchase that never happened. (KOS is NOT a
        // refusal path — Handle_OP_ShopPlayerBuy has no faction check; faction only gates opening the
        // window. A buy from an already-open KOS merchant succeeds.) On success the server echoes
        // THIS SAME opcode back — apply_shop_player_buy (packet_handler.rs) is the only place that
        // may deduct coin or log "Bought item", because it's the only place that knows it succeeded.
        let buy_req = self.command.take_merchant_buy();
        if let Some((merchant_id, slot)) = buy_req {
            send_shop_buy(stream, gs, merchant_id, slot);
        }

        // Merchant buy (AWAITED, honest Command-with-result path — POST /v1/merchant/buy, #448):
        // emit the SAME wire traffic as the fire-and-forget path above, then PARK the HTTP-side
        // `oneshot::Sender` in `pending_buy` (with the merchant/slot for echo correlation + a
        // sent-at instant). We fire NOTHING here — the resolving packet does, after `apply_packet`:
        // the OP_ShopPlayerBuy echo → `fulfill_buy_ok` (Resolved), OP_ShopEndConfirm →
        // `fulfill_buy_refused` (Refused), or a zone change / HTTP timeout → Unconfirmed.
        //
        // SINGLETON-in-flight (reject-while-parked): awaited buys are serialized — at most ONE may be
        // parked. The server's OP_ShopPlayerBuy echo carries NO per-request token, so two identical
        // in-flight buys (same merchant+slot) are INDISTINGUISHABLE at the echo. If we superseded the
        // parked buy with a newer one, the first buy's echo would resolve the SECOND caller's Sender
        // using the FIRST buy's receipt — mis-attributing success (a failed second buy would report
        // 200 on the first's success). So when a buy is already parked we do NOT overwrite it and do
        // NOT send any wire packets: we immediately answer the NEW request `Refused` ("another buy
        // is in flight; retry after it resolves"). Because the packets were never sent, the buy
        // DEFINITIVELY did not happen — `Refused`/409 is honest here, not `Unconfirmed`/202 (which
        // means "outcome unknown" and would understate our certainty). The server then only ever
        // processes one awaited buy at a time, keeping the echo correlation unambiguous, and the busy
        // caller gets an honest 409 rather than a mis-attributed 200. See `eqoxide_command::result`
        // for the discipline
        // A3.2/A3.3 must copy. (Known residual: a UI fire-and-forget buy of the SAME slot concurrent
        // with a parked awaited buy could still have its echo resolve the awaited buy, because the
        // fire-and-forget path does not park — very low likelihood, and it cannot fabricate success.)
        if let Some((merchant_id, slot, tx)) = self.command.take_buy_await() {
            if self.pending_buy.is_some() {
                let _ = tx.send(eqoxide_command::CommandResult::Refused(
                    "another buy is already in flight; retry after it resolves".into()));
                tracing::info!("EQ: awaited shop buy REJECTED — one already in flight (merchant_id={merchant_id} slot={slot})");
            } else {
                send_shop_buy(stream, gs, merchant_id, slot);
                self.pending_buy = Some(PendingBuy { tx, merchant_id, slot, sent_at: Instant::now() });
                tracing::info!("EQ: awaited shop buy parked — merchant_id={merchant_id} slot={slot}");
            }
        }

        // Merchant sell: open the merchant (OP_ShopRequest) then sell a player inventory slot
        // (OP_ShopPlayerSell). Same sequencing as buy so the shop is open server-side first.
        // Must be within ~200u of the merchant; the server computes the price (we send 0).
        let sell_req = self.command.take_merchant_sell();
        if let Some((merchant_id, slot, quantity)) = sell_req {
            // #360: same staleness hazard as the buy path above — clear a DIFFERENT stale merchant
            // before sending, but don't flicker the one that's already open (#361 review FIX 2).
            gs.begin_shop_open_for(merchant_id);
            let open = merchant_click(merchant_id, gs.player_id, 1);
            stream.send_app_packet(OP_SHOP_REQUEST, &open);
            // RoF2 Merchant_Purchase_Struct is 20 bytes (rof2_structs.h): npcid(u32)@0,
            // inventory_slot(TypelessInventorySlot_Struct: Slot i16@4, SubIndex i16@6, AugIndex i16@8,
            // Unknown i16@10)@4, quantity(u32)@12, price(u32)@16. The old 16-byte body (plain u32
            // slot@4) failed the server's DECODE_LENGTH_EXACT, so EVERY sell was silently dropped
            // (#269). `slot` is the RoF2 wire slot /observe/inventory reports (general inv 23-32);
            // RoF2ToServerTypelessSlot passes it straight through for a top-level possession, so
            // SubIndex/AugIndex are the "none" sentinels (SLOT_INVALID / SOCKET_INVALID = -1).
            let mut sell = [0u8; 20];
            sell[0..4].copy_from_slice(&merchant_id.to_le_bytes());
            sell[4..6].copy_from_slice(&(slot as i16).to_le_bytes());   // Slot (RoF2 wire slot)
            sell[6..8].copy_from_slice(&(-1i16).to_le_bytes());          // SubIndex: not inside a bag
            sell[8..10].copy_from_slice(&(-1i16).to_le_bytes());         // AugIndex: no augment socket
            // Unknown01 @10 stays 0.
            sell[12..16].copy_from_slice(&quantity.to_le_bytes());
            // price @16 = 0: the server charges its own buy-back price.
            stream.send_app_packet(OP_SHOP_PLAYER_SELL, &sell);
            tracing::info!("EQ: shop sell — merchant_id={} slot={} qty={}", merchant_id, slot, quantity);
            // No optimistic "Sold" log: the server's OP_ShopPlayerSell echo (apply_shop_player_sell)
            // confirms the real payout + removes the item, so a premature success can't be printed
            // when the sale fails (#269).
        }

        // Open/close a merchant window (POST /trade/open, /trade/close). OP_ShopRequest with
        // command=1 (open) or 0 (close). The server replies with OP_ShopRequest (Open/Close) +
        // OP_ItemPacket(Merchant) items, decoded in packet_handler into gs.merchant_*.
        let trade_req = self.command.take_merchant_trade();
        if let Some(cmd) = trade_req {
            let (merchant_id, command) = match cmd {
                TradeCmd::Open(id) => (id, 1u32),
                TradeCmd::Close    => (gs.merchant_open.unwrap_or(0), 0u32),
            };
            if command == 1 {
                // #360: clear before sending — an Open request that never gets an echo (non-merchant
                // target / out-of-range) must not leave `merchant_open` reporting the merchant we had
                // open before this request. begin_shop_open_for keeps an already-open re-open from
                // flickering the window closed (#361 review FIX 2).
                gs.begin_shop_open_for(merchant_id);
            }
            let open = merchant_click(merchant_id, gs.player_id, command);
            stream.send_app_packet(OP_SHOP_REQUEST, &open);
            tracing::info!("EQ: shop {} — merchant_id={}", if command == 1 { "open" } else { "close" }, merchant_id);
            if command == 0 { gs.merchant_open = None; gs.merchant_items.clear(); }
        }

        // Merchant open (AWAITED, honest Command-with-result path — POST /v1/merchant/open,
        // eqoxide#479): emit the SAME OP_ShopRequest(command=1) the fire-and-forget `TradeCmd::Open`
        // path above sends, then PARK the HTTP-side `oneshot::Sender` in `pending_open` (with the
        // merchant_id for echo correlation + a sent-at instant). We fire NOTHING here — the
        // resolving packet does, after `apply_packet`: the OP_ShopRequest echo → `fulfill_open`
        // (Resolved on command=1, Refused on command=0), or a non-merchant/out-of-range target's
        // total silence → the HTTP timeout / a zone-change reaper → Unconfirmed.
        //
        // SINGLETON-in-flight (reject-while-parked), same discipline as awaited buy: the echo
        // carries no per-request token, so two identical in-flight opens are indistinguishable.
        // When an open is already parked we do NOT overwrite it and send NO wire packets — we
        // immediately answer the NEW request `Refused` (packets never sent, so the open
        // DEFINITIVELY did not happen — 409 is honest here, not 202).
        if let Some((merchant_id, tx)) = self.command.take_open_await() {
            if self.pending_open.is_some() {
                let _ = tx.send(eqoxide_command::CommandResult::Refused(
                    "another open is already in flight; retry after it resolves".into()));
                tracing::info!("EQ: awaited shop open REJECTED — one already in flight (merchant_id={merchant_id})");
            } else {
                gs.begin_shop_open_for(merchant_id);
                let open = merchant_click(merchant_id, gs.player_id, 1);
                stream.send_app_packet(OP_SHOP_REQUEST, &open);
                self.pending_open = Some(PendingOpen { tx, merchant_id, sent_at: Instant::now() });
                tracing::info!("EQ: awaited shop open parked — merchant_id={merchant_id}");
            }
        }
    }

    fn drain_move_item(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Move/equip/unequip an item between inventory slots (OP_MoveItem).
        // MoveItem_Struct (12b): from_slot(u32), to_slot(u32), number_in_stack(u32).
        // number_in_stack MUST be 0 for a whole-item move (equip/unequip/rearrange): EQEmu's
        // SwapItem rejects number_in_stack > 0 for any non-stackable item (inventory.cpp ~2025,
        // "not a stackable item" -> SwapItemResync = the "Inventory Desyncronization" we hit). 0
        // takes the direct-swap/equip path. (A count would only be for splitting a stack.)
        let move_req = self.command.take_inventory_move();
        if let Some((from_slot, to_slot)) = move_req {
            // build_move_item emits the structured 28-byte RoF2 MoveItem_Struct; a flat 12-byte
            // packet is silently dropped by the server (see build_move_item / eqoxide#11).
            stream.send_app_packet(OP_MOVE_ITEM, &build_move_item(from_slot, to_slot));
            // EQEmu applies the move silently (no echo), so mirror it into our snapshot or
            // /inventory goes stale and the next move corrupts it (phantom items).
            self.mirror_move_item(gs, from_slot as i32, to_slot as i32);
            tracing::info!("EQ: move item — from_slot={} to_slot={} qty=0(whole)", from_slot, to_slot);
            gs.log_msg("inventory", &format!("Moved item (slot {} -> {})", from_slot, to_slot));
        }
    }

    // `apply_fast_steering` moved to `eqoxide_nav::walker::Walker` (M1 extraction).

    fn drive_auto_target(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Auto-target: while auto-attacking, pick who to fight each tick. Priority (see
        // `pick_combat_target`): a mob that is actively attacking the player (engage adds instead of
        // tanking them unanswered) > a still-valid current target > the nearest reachable trash mob
        // (name starts "a_"/"an_", excluding named guards/merchants/citizens) within ~200u, so
        // grinding continues hands-free between kills.
        if self.auto_attack {
            // Drop attackers that haven't swung at us in a while so a long-dead aggressor or one
            // we've out-run doesn't keep pulling target priority.
            const ATTACKER_TTL: std::time::Duration = std::time::Duration::from_secs(6);
            gs.recent_attackers.retain(|_, t| t.elapsed() < ATTACKER_TTL);

            let col = self.collision.read().unwrap();
            // LINE of sight, not a walkable path: "is this NPC in the open in front of me", used only
            // to drop targets behind a wall. `line_clear` (a centre ray) is the right primitive —
            // `path_clear` now sweeps the player's whole collision volume (#358), which would also
            // reject a perfectly attackable NPC standing in a doorway.
            let clear_to = |e: &eqoxide_core::game_state::Entity| -> bool {
                col.as_ref().map_or(true, |c| {
                    c.line_clear([gs.player_x, gs.player_y, e.z + 3.0], [e.x, e.y, e.z + 3.0], 2.0)
                })
            };
            let alive_reachable = |id: u32| -> bool {
                gs.world.entities.get(&id).map(|e| !e.dead && e.is_npc && clear_to(e)).unwrap_or(false)
            };

            let current = gs.target_id;
            // The current target is valid only if alive AND still reachable in a straight line —
            // otherwise drop it so we retarget or roam (don't get stuck swinging "too far").
            let current_valid = current.map(|id| alive_reachable(id)).unwrap_or(false);
            let current_is_attacker = current.map(|id| gs.recent_attackers.contains_key(&id)).unwrap_or(false);

            // The add to engage: the most-recent attacker that is alive + reachable and isn't already
            // our current target. (If the current target is the attacker, `pick_combat_target` keeps it.)
            let attacker = gs.recent_attackers.iter()
                .filter(|(id, _)| Some(**id) != current && alive_reachable(**id))
                .max_by_key(|(_, t)| **t)
                .map(|(id, _)| *id);

            // Nearest reachable trash, only needed as the fallback (no attacker, no valid current).
            let nearest_trash = if attacker.is_none() && !current_valid {
                let mut best: Option<(f32, u32)> = None;
                for (id, e) in &gs.world.entities {
                    if e.dead || !e.is_npc { continue; }
                    let nl = e.name.to_ascii_lowercase();
                    if !(nl.starts_with("a_") || nl.starts_with("an_")) { continue; }
                    let dx = e.x - gs.player_x;
                    let dy = e.y - gs.player_y;
                    let d2 = dx * dx + dy * dy;
                    if d2 > 200.0 * 200.0 || !clear_to(e) { continue; }
                    if best.map(|(bd, _)| d2 < bd).unwrap_or(true) { best = Some((d2, *id)); }
                }
                best.map(|(_, id)| id)
            } else { None };
            drop(col);

            let desired = pick_combat_target(current, current_valid, current_is_attacker, attacker, nearest_trash);
            // Only send a target packet when the choice actually changes (avoid per-tick spam). If
            // `desired` is None we keep the current target and idle, matching the old behaviour of
            // waiting for a respawn rather than roaming out of a sealed pocket.
            if let Some(id) = desired {
                if Some(id) != current {
                    gs.set_target(id); // also clears stale con/attitude from the old target (#323)
                    stream.send_app_packet(OP_TARGET_MOUSE, &build_target_packet(id));
                }
            }
        }
    }

    fn drive_auto_pet_combat(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Auto-pet-combat: if the player has a pet (e.g. a summoned necro pet), send it to attack
        // the current target. Only (re)issue PET_ATTACK when the target changes, so we don't spam
        // OP_PetCommands every tick. The player's own melee auto-engage (below) still runs, which
        // keeps her walking into loot range while the pet does the damage.
        if let Some(pet) = gs.pet_id {
            // Engage only a reasonably-close LIVE target (<=200u) so the pet doesn't run across the
            // zone after a distant mob and lose itself. When there's no such target (idle, or the
            // mob died), recall the pet with PET_BACKOFF so it returns to the owner instead of
            // wandering off — the previous version left it chasing and it dropped out of view.
            let engage = if self.auto_attack {
                gs.target_id
                    .and_then(|tid| gs.world.entities.get(&tid).map(|e| (tid, e)))
                    .filter(|(_, e)| {
                        let dx = e.x - gs.player_x; let dy = e.y - gs.player_y;
                        !e.dead && dx * dx + dy * dy <= 200.0 * 200.0
                    })
                    .map(|(tid, _)| tid)
            } else { None };
            match engage {
                Some(tid) if self.last_pet_target != Some(tid) => {
                    stream.send_app_packet(OP_PET_COMMANDS, &build_pet_command(PET_ATTACK, tid));
                    self.last_pet_target = Some(tid);
                    tracing::info!("EQ: pet {pet} → attack target {tid}");
                }
                Some(_) => {} // already attacking this target
                None => {
                    if self.last_pet_target.is_some() {
                        stream.send_app_packet(OP_PET_COMMANDS, &build_pet_command(PET_BACKOFF, 0));
                        self.last_pet_target = None;
                        tracing::info!("EQ: pet {pet} → back off (no target)");
                    }
                }
            }
        } else {
            self.last_pet_target = None;
        }
    }

    /// Returns true if this handled the tick and the caller must stop (melee engage/hold fired).
    fn drive_auto_engage_melee(&mut self, stream: &mut EqStream, gs: &mut GameState) -> bool {
        // Auto-engage: while auto-attacking, walk into melee range of the target and face it so
        // the server registers swings. Closing the last few units via legit walking (not a held
        // far-away face) is what makes melee actually land. Runs regardless of any pending goto.
        if self.auto_attack {
            if let Some(tid) = gs.target_id {
                if let Some((ex, ey)) = gs.world.entities.get(&tid).map(|e| (e.x, e.y)) {
                    let dx = ex - gs.player_x;
                    let dy = ey - gs.player_y;
                    let dist = (dx * dx + dy * dy).sqrt();
                    if dist < 200.0 { // engage targets within ~200u (sparse spawns; walk to them)
                        const MELEE: f32 = 5.0;
                        const PET_STANDOFF: f32 = 25.0; // pet classes hang back and let the pet tank
                        // With a pet, DON'T walk into melee — the pet holds aggro (PET_ATTACK) and a
                        // squishy caster who closes to melee just gets killed (a level-1 necro died
                        // to a level-4 skeleton this way). Stand off ~25u: out of the mob's melee but
                        // close enough to loot the corpse after the pet kills it.
                        let engage = if gs.pet_id.is_some() { PET_STANDOFF } else { MELEE };
                        let hdg = if dist > 0.01 { eq_heading(dx, dy) } else { gs.player_heading };
                        gs.player_heading = hdg;
                        if dist > engage {
                            // Drive the controller toward the target (it owns collide-and-slide).
                            let swim = self.collision.read().unwrap().as_ref()
                                .is_some_and(|c| c.in_water([gs.player_x, gs.player_y, gs.player_z]));
                            *self.controller.nav_intent.lock().unwrap() = Some(MoveIntent {
                                wish_dir:    [dx / dist, dy / dist],
                                wish_vspeed: 0.0,
                                jump:        false,
                                want_swim:   swim,
                                speed:       RUN_SPEED,
                                climb:       0.0, // nav uses the native step-up now (#239); fences handled by hop
                                hop:         false,                      // melee approach: no auto-hop
                            });
                        } else {
                            // In melee range: stop the controller and face the target so swings land
                            // (IsFacingMob). The explicit send keeps the server's facing current.
                            *self.controller.nav_intent.lock().unwrap() = None;
                            // `from == to`: this is a stationary facing-only correction (we're
                            // already stopped, IsFacingMob is all that needs updating), so it must
                            // always report zero speed/anim regardless of the throttled cadence's
                            // baseline — and, deliberately, does not touch `last_sent_pos`/
                            // `last_pos_send` (see their doc comments), since it is an out-of-band
                            // send outside the normal 280ms/1300ms cadence.
                            let here = [gs.player_x, gs.player_y, gs.player_z];
                            self.send_position_update(stream, gs, here, gs.player_x, gs.player_y, gs.player_z, hdg);
                        }
                        self.command.request_cancel_goto(); // cancel any stale walk
                        return true;
                    }
                }
            }
        }
        false
    }

    // `drive_chase`/`drive_teleport_detect`/`resolve_goal`/`drive_walk` moved to
    // `eqoxide_nav::walker::Walker` (M1 extraction) — see `tick`'s `self.walker.*` calls above.

    /// Advance the quest turn-in (POST /give) trade-window flow. The full sequence is:
    ///   1. New give request: put the item on the cursor (OP_MoveItem from_slot→30, skip if it's
    ///      already on the cursor), send OP_TradeRequest, and enter the "waiting for ack" state.
    ///   2. The server replies OP_TradeRequestAck (→ gs.trade_ack_ready); only then may we move the
    ///      cursor item into the NPC trade slot — the server rejects cursor→trade moves before the
    ///      trade session exists.
    ///   3. Ack seen: OP_MoveItem cursor(30)→trade slot(3000), then OP_TradeAcceptClick.
    /// The server then sends OP_FinishTrade (handled in packet_handler). If no ack arrives within
    /// ~3s we abort and reset. Called every tick (not gated by the 150ms walk throttle).
    ///
    /// A3 Migration 2 (#448), SERIALIZED (#475 review): NEITHER path clears at step 3. Both hold
    /// `give_state` through a phase-2 wait for OP_FinishTrade, so at most one trade is ever in flight
    /// and the 0-byte finish (no trade id) is unambiguous. VERIFY-TRANSFER (#486): OP_FinishTrade only
    /// marks `finish_seen` (via `note_finish_trade`); the verdict is rendered HERE after a short settle,
    /// checking whether the item actually left inventory — a genuinely-gone item resolves an awaited
    /// give `Resolved(GiveOk)` (fire-and-forget clears silently), a still-held (returned) item resolves
    /// `Unconfirmed`/silent-clear (NEVER a fabricated 200). A phase-2 timeout with no finish at all
    /// yields `Unconfirmed`/silent-clear. A NEW give arriving mid-trade is refused (awaited) or dropped
    /// (fire-and-forget), never started. See `begin_give`/`note_finish_trade`.
    fn tick_give(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Begin a new give request if one is queued and we're not already mid-trade. Prefer the
        // fire-and-forget UI slot (existing behavior), then the awaited slot (#448). Only one trade
        // runs at a time, so with `give_state` clear at most one begins per tick.
        if self.give_state.is_none() {
            if let Some((npc_id, from_slot)) = self.command.take_give() {
                self.begin_give(stream, gs, npc_id, from_slot, None);
            } else if let Some((npc_id, from_slot, tx)) = self.command.take_give_await() {
                self.begin_give(stream, gs, npc_id, from_slot, Some(tx));
            }
            return;
        }

        // A trade IS already in flight. SINGLETON-IN-FLIGHT (#448, hardened by the #475 review): the
        // in-flight give owns the machine until its OP_FinishTrade (or timeout), so NO new give may
        // start — starting a second, racing trade is exactly what would make the next 0-byte
        // OP_FinishTrade ambiguous (it could resolve the wrong give → a fabricated 200).
        //   • A second AWAITED give is refused OUTRIGHT with `Refused` (409, "we KNOW it didn't happen"
        //     — the packets were never sent, so this is honest, not a 202 "unknown"). The in-flight
        //     give is untouched.
        //   • A second FIRE-AND-FORGET give has no caller to answer, so it is DROPPED with a log (never
        //     queued to start later, never started now). Dropping is the honest choice — silently
        //     starting a racing trade is the bug we are closing.
        if let Some((npc_id, from_slot, tx)) = self.command.take_give_await() {
            let _ = tx.send(eqoxide_command::CommandResult::Refused(
                "a give is already in flight; retry".into()));
            tracing::info!("EQ: give: awaited give REJECTED — one already in flight (npc_id={npc_id} from_slot={from_slot})");
        }
        if let Some((npc_id, from_slot)) = self.command.take_give() {
            tracing::warn!("EQ: give: dropping fire-and-forget give — one already in flight (npc_id={npc_id} from_slot={from_slot})");
            gs.log_msg("trade", "Ignored give — a turn-in is already in progress");
        }

        // Advance the state machine. `accepted` splits phase 1 (awaiting ack) from phase 2 (accept
        // sent, awaiting OP_FinishTrade — both paths). Read it without holding a borrow across sends.
        let accepted = self.give_state.as_ref().map(|g| g.accepted).unwrap_or(false);

        if !accepted {
            // Phase 1: either the ack arrived (advance the trade) or we keep waiting (with a timeout).
            if gs.trade_ack_ready {
                let npc_id = self.give_state.as_ref().map(|g| g.npc_id).unwrap_or(0);
                // Step 3: move the cursor item into the NPC's first trade slot, then accept. The trade
                // slot needs RoF2 typeTrade encoding (not possessions) — build_move_item_to_trade emits
                // the 28-byte structured MoveItem the server actually accepts (eqoxide#26).
                stream.send_app_packet(OP_MOVE_ITEM, &build_move_item_to_trade(SLOT_CURSOR, SLOT_TRADE_BEGIN));
                self.mirror_move_item(gs, SLOT_CURSOR as i32, SLOT_TRADE_BEGIN as i32);
                let mut accept = [0u8; 8];
                accept[0..4].copy_from_slice(&gs.player_id.to_le_bytes());
                // unknown4 = 0 (already zeroed).
                stream.send_app_packet(OP_TRADE_ACCEPT_CLICK, &accept);
                tracing::info!("EQ: give: cursor→trade slot + OP_TradeAcceptClick (npc_id={})", npc_id);
                gs.trade_ack_ready = false;
                // Enter phase 2 for BOTH paths (#475 review): keep `give_state` parked through the
                // confirming OP_FinishTrade so this give stays the ONLY trade in flight and the 0-byte
                // finish can't be mis-attributed. `note_finish_trade` marks the finish and the deferred
                // verify (#486) then resolves it (awaited → `Resolved(GiveOk)` if the item left, else
                // `Unconfirmed`; fire-and-forget → silent clear). Reset the tick counter to time the
                // finish wait. (Pre-review, the fire-and-forget path cleared here at accept — which
                // let a late finish resolve a DIFFERENT give that had reached phase 2 meanwhile.)
                if let Some(g) = self.give_state.as_mut() { g.accepted = true; g.ticks_waiting = 0; }
            } else if let Some(g) = self.give_state.as_mut() {
                g.ticks_waiting += 1;
                if g.ticks_waiting >= GIVE_ACK_TIMEOUT_TICKS {
                    // Abort: cancel the (possibly half-open) trade session and reset. The give was SENT
                    // but never acked — the awaited path reports `Unconfirmed` (202, outcome UNKNOWN),
                    // NEVER success.
                    let await_tx = g.await_tx.take();
                    // 8-byte CancelTrade_Struct — the server DROPS a 0-byte OP_CancelTrade on a size
                    // check (client_packet.cpp:4319), so a bare `&[]` never actually ended the session.
                    stream.send_app_packet(OP_CANCEL_TRADE, &build_cancel_trade(gs.player_id));
                    tracing::warn!("EQ: give: no trade ack (timed out)");
                    gs.log_msg("trade", "Trade timed out (no NPC ack)");
                    if let Some(tx) = await_tx {
                        let _ = tx.send(eqoxide_command::CommandResult::Unconfirmed);
                    }
                    self.give_state = None;
                    gs.trade_ack_ready = false;
                }
            }
        } else if self.give_state.as_ref().map(|g| g.finish_seen).unwrap_or(false) {
            // Phase 2, OP_FinishTrade SEEN (#486). WAIT the returned-item watch window first: EQEmu
            // sends any un-accepted item back to the cursor via a SEPARATE OP_ItemPacket queued STRICTLY
            // AFTER the 0-byte OP_FinishTrade (server source: zone/client_packet.cpp:15488), so it can
            // land in a later rx-drain than the finish. Settling for GIVE_FINISH_SETTLE_TICKS cadences
            // (each tick runs AFTER the full gameplay drain) guarantees any return-item packet is applied
            // before we judge — without it the verify would race and could still fabricate a 200.
            let settled = {
                let g = self.give_state.as_mut().unwrap();
                g.ticks_waiting += 1;
                g.ticks_waiting >= GIVE_FINISH_SETTLE_TICKS
            };
            if !settled { return; }
            // Watch window elapsed: every packet in the finish's batch — crucially any returned-item
            // OP_ItemPacket — is now applied to the inventory mirror. VERIFY the item actually
            // transferred instead of trusting the finish. OP_FinishTrade only means the trade SESSION
            // ended; a rejected / out-of-range NPC returns the item SPECIFICALLY TO THE CURSOR (slot 33,
            // via EQEmu PushItemOnCursor) and STILL sends OP_FinishTrade.
            //
            // The verdict keys on the captured `item_id` at the CURSOR — a POSITIVE, precise "was it
            // returned?" test (review of the first cut):
            //   • `item_id` is None → the item could not be identified at send time (mirror desync,
            //     #275). We can NEVER confidently claim success for an unidentifiable give → `Unconfirmed`
            //     (this closes a residual false-200: the old name-scan fell back to a synthetic
            //     "item in slot N" name that a real returned item never matches).
            //   • the captured `item_id` is on the CURSOR (slot 33) → the NPC RETURNED it → `Unconfirmed`.
            //   • otherwise (not on the cursor — gone) → the turn-in transferred → `Resolved(GiveOk)`.
            // Keying on cursor+item_id (not an all-slot name-scan) means a same-named/same-id DUPLICATE
            // sitting elsewhere in the pack (a spare stackable reagent, an equipped copy) does NOT
            // fabricate a false `Unconfirmed` on a REAL success — which an agent would treat as "retry"
            // and hand the item over twice. Never a false 200, and no spurious 202 from a duplicate.
            let g = self.give_state.as_mut().unwrap();
            let await_tx  = g.await_tx.take();
            let npc_id    = g.npc_id;
            let item_name = std::mem::take(&mut g.item_name);
            let item_id   = g.item_id;
            self.give_state = None;
            let confirmed = match item_id {
                // Identified: transferred iff the captured item is NOT sitting on the cursor.
                Some(id) => !gs.inventory.iter().any(|i| i.slot == SLOT_CURSOR as i32 && i.item_id == id),
                // Unidentifiable at send time (mirror desync) → never a confident success.
                None => false,
            };
            if confirmed {
                tracing::info!("EQ: give: turn-in confirmed — item {:?} (id={:?}) left inventory (npc_id={})", item_name, item_id, npc_id);
                if let Some(tx) = await_tx {
                    let _ = tx.send(eqoxide_command::CommandResult::Resolved(
                        eqoxide_command::GiveOk { npc_id, item_name }));
                }
            } else {
                tracing::warn!("EQ: give: OP_FinishTrade but item {:?} (id={:?}) NOT transferred (returned to cursor or unidentifiable) — honest Unconfirmed", item_name, item_id);
                gs.log_msg("trade", "Give not confirmed — the item was returned to you");
                if let Some(tx) = await_tx {
                    let _ = tx.send(eqoxide_command::CommandResult::Unconfirmed);
                }
            }
        } else if let Some(g) = self.give_state.as_mut() {
            // Phase 2 (both paths), NO finish yet: OP_TradeAcceptClick has been sent; we hold the
            // machine, awaiting OP_FinishTrade. If none arrives within the window, the turn-in was
            // either an item mismatch (the server returns the item on the cursor via OP_ItemPacket with
            // NO OP_FinishTrade) or the reply was lost. The awaited path reports the HONEST `Unconfirmed`
            // (202), NEVER success; a fire-and-forget give clears silently. Clearing here is also what
            // frees the machine for the next give (serialized — one trade at a time).
            g.ticks_waiting += 1;
            if g.ticks_waiting >= GIVE_FINISH_TIMEOUT_TICKS {
                let await_tx = g.await_tx.take();
                // #480: end the trade session server-side before we clear `give_state`. The verdict is
                // still genuinely UNKNOWN at this timeout, so the caller MUST get an honest `Unconfirmed`
                // — OP_CancelTrade does NOT retroactively make this a success or a clean refusal, it only
                // FORECLOSES a late OP_FinishTrade for THIS give from mis-attributing to a SUBSEQUENT
                // give/trade (`Client::FinishTrade`+`Trade::Reset`, client_packet.cpp:4337). If the trade
                // already completed server-side (the finish was merely delayed/lost) the cancel is a
                // harmless no-op (trade->With() is null; server just closes our UI). Either way the
                // outcome we report is unchanged — narrowed, not fabricated. (Residual: if the real
                // OP_FinishTrade is already IN FLIGHT when we cancel, the cancel can't recall it — see
                // #498; serialization keeps at most one give parked, but a next give could still be
                // reached by a finish that crossed the cancel on the wire. Narrowed, not eliminated.)
                stream.send_app_packet(OP_CANCEL_TRADE, &build_cancel_trade(gs.player_id));
                if let Some(tx) = await_tx {
                    let _ = tx.send(eqoxide_command::CommandResult::Unconfirmed);
                }
                tracing::warn!("EQ: give: no OP_FinishTrade after accept — outcome UNKNOWN (item mismatch or lost); sent OP_CancelTrade to end the session");
                gs.log_msg("trade", "Trade not confirmed (item may have been returned)");
                self.give_state = None;
            }
        }
    }

    /// Start a trade-window turn-in (shared by the fire-and-forget UI give and the awaited #448 give):
    /// capture the item name for the receipt, put the item on the cursor, send OP_TradeRequest, and
    /// enter phase 1 (`accepted: false`) awaiting OP_TradeRequestAck. `await_tx` is `None` for the UI
    /// path and `Some` for the awaited path (which is what makes the state machine keep the parked
    /// `Sender` through OP_FinishTrade). Byte-for-byte the same wire traffic in both cases.
    fn begin_give(
        &mut self,
        stream: &mut EqStream,
        gs: &mut GameState,
        npc_id: u32,
        from_slot: u32,
        await_tx: Option<tokio::sync::oneshot::Sender<eqoxide_command::CommandResult<eqoxide_command::GiveOk>>>,
    ) {
        // Capture the item name AND item_id BEFORE it leaves the slot — by the time the confirming
        // OP_FinishTrade is applied, `clear_trade_slots` has run, so neither can be read back then. The
        // `item_id` is the KEY the verify-transfer verdict uses (#486 review); the name is only the
        // human receipt. If the mirror is desynced and the slot holds no known item, `item_id` is None
        // (a nonzero id from a real item is required) → an unidentifiable give resolves `Unconfirmed`,
        // never a confident success. (One `find` for both so the id and name always describe the SAME
        // slotted item.)
        let found = gs.inventory.iter().find(|i| i.slot == from_slot as i32);
        let item_name = found.map(|i| i.name.clone())
            .unwrap_or_else(|| format!("item in slot {from_slot}"));
        let item_id = found.map(|i| i.item_id).filter(|&id| id != 0);
        // Step 1: put the item on the cursor (skip if it's already there). Use the 28-byte structured
        // MoveItem (possessions→cursor); the old flat 12-byte packet was silently dropped by the
        // server, so the item never reached the cursor (eqoxide#26).
        if from_slot != SLOT_CURSOR {
            stream.send_app_packet(OP_MOVE_ITEM, &build_move_item(from_slot, SLOT_CURSOR));
            self.mirror_move_item(gs, from_slot as i32, SLOT_CURSOR as i32);
        }
        // Send OP_TradeRequest { to_mob_id = npc, from_mob_id = player }.
        let mut req = [0u8; 8];
        req[0..4].copy_from_slice(&npc_id.to_le_bytes());
        req[4..8].copy_from_slice(&gs.player_id.to_le_bytes());
        stream.send_app_packet(OP_TRADE_REQUEST, &req);
        gs.trade_ack_ready = false;
        self.give_state = Some(GiveState { npc_id, ticks_waiting: 0, await_tx, item_name, item_id, accepted: false, finish_seen: false });
        tracing::info!("EQ: give: OP_TradeRequest to npc_id={} (item slot {})", npc_id, from_slot);
        gs.log_msg("trade", "Offering item to NPC...");
    }

    /// NOTE that OP_FinishTrade landed for the parked AWAITED/fire-and-forget give (A3 Migration 2,
    /// #448; verify-transfer hardened in #486). Called from the gameplay loop AFTER `apply_packet`
    /// (which cleared the trade slots AND applied any returned-item OP_ItemPacket), exactly as the buy
    /// fulfils are. OP_FinishTrade is a 0-byte packet with NO correlation data (no trade/npc id) — so it
    /// can only be matched to a give because gives are SERIALIZED (#475 review): at most one trade is in
    /// flight, so a finish while a give is parked in phase 2 unambiguously belongs to THAT give.
    ///
    /// #486 — OP_FinishTrade does NOT mean the NPC ACCEPTED the item. It means the trade SESSION ended.
    /// A rejected or OUT-OF-RANGE NPC turn-in ALSO fires OP_FinishTrade but RETURNS the item to the
    /// player (cursor slot 33). Resolving `Resolved(GiveOk)` on ANY finish was a fabricated 200 for an
    /// item that never transferred (live, twice: a give to an 850u NPC returned 200 "given" while the
    /// stack just shuffled slot 24→33). So this no longer resolves — it merely sets `finish_seen`, and
    /// the verdict is DEFERRED to `tick_give`, which runs AFTER the whole gameplay drain loop (so the
    /// inventory mirror is settled) and VERIFIES the item actually left before resolving. No-op unless a
    /// phase-2 give is parked (a finish with nothing in flight, or one still in phase 1, is ignored).
    /// Non-blocking; never `.await`s.
    pub fn note_finish_trade(&mut self) {
        if let Some(g) = self.give_state.as_mut() {
            if g.accepted && !g.finish_seen {
                g.finish_seen = true;
                g.ticks_waiting = 0; // restart the counter as the returned-item watch window
            }
        }
    }

    /// Reap a parked AWAITED give as `Unconfirmed` (A3 Migration 2, #448) — fired on a zone change so
    /// a crossing mid-give can't strand the `Sender` or let a stray OP_FinishTrade in the new zone
    /// mis-resolve it. Mirrors `reap_pending_buy`. Only touches an AWAITED give (a fire-and-forget give
    /// in flight is left to its own tick-abort, unchanged). No-op when nothing awaited is parked.
    pub fn reap_pending_give(&mut self) {
        let awaited = self.give_state.as_ref().map(|g| g.await_tx.is_some()).unwrap_or(false);
        if !awaited { return; }
        if let Some(mut g) = self.give_state.take() {
            if let Some(tx) = g.await_tx.take() {
                let _ = tx.send(eqoxide_command::CommandResult::Unconfirmed);
            }
        }
    }

    /// Stream the render controller's authoritative position to the server at native cadence
    /// (design §2/§3.4). Runs every tick (not gated by the 150 ms planner). Mirrors the controller's
    /// position into the network `gs` so combat/targeting see the live position, detects genuine
    /// server corrections (>12u jumps the server pushed) and forwards them to the controller, and
    /// sends OP_ClientUpdate at ≤280 ms while moving with a forced 1300 ms keepalive when idle.
    fn stream_position(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        let view = *self.controller.controller_view.lock().unwrap();
        // Don't stream/mirror until the render controller has spawned (else we'd push origin).
        if !view.initialized { return; }
        // #724 review B1: mirror the controller's "I have stopped the body and cannot resume" state
        // alongside its position, on the same tick and under the same init gate, so `player.hold` on
        // `GET /v1/observe` is exactly as fresh as the `pos` beside it. Level-triggered — the view is
        // republished every render frame, so this write is also the clear.
        // #776/#801: and the afloat stall beside it, in the SAME statement — `ControllerView` keeps
        // both disclosures private behind `disclosures()` precisely so this mirror cannot update one
        // and leave the other holding the previous frame's answer. Two GameState fields rather than
        // one, because they are two different claims — see `ControllerView::publish_disclosures`.
        (gs.player_hold, gs.player_afloat_stall) = view.disclosures();
        // Anti-MQGhost keepalive (#105): send a movement-history entry every 30s (< the server's 70s
        // window) whether or not we're moving, so the server's CheatManager never false-flags us.
        if self.last_movement_history_send.elapsed().as_millis() >= MOVEMENT_HISTORY_MS {
            stream.send_app_packet(OP_FLOAT_LIST_THING,
                &build_movement_history(view.pos[0], view.pos[1], view.pos[2]));
            self.last_movement_history_send = Instant::now();
        }
        // Driver-agnostic fall damage (§442, #442). The render controller runs the ONE collided
        // descent (for WASD AND nav) and latches the height of any airborne stretch it just LANDED
        // from — computed from its OWN tracked airborne start, never a nav waypoint z. We take-and-
        // clear that one-shot exactly once here and, if the fall was past the safe height, apply the
        // native (client-computed) fall damage + OP_ENV_DAMAGE — the same formula/threshold the old
        // `drive_controlled_fall` used. Any fall past the safe height damages, so WASD off a ledge
        // now damages too, matching the native RoF2 client. A teleport / server correction clears the
        // signal at the controller (see `CharacterController::teleport`), so a correction is never
        // misread as a fall (hazard 2b); a mid-fall depenetration/ground-snap recovery latches nothing
        // (hazard 2a). `SAFE_FALL_HEIGHT` is named so the threshold is easy to tune/revert.
        const SAFE_FALL_HEIGHT: f32 = 6.0; // below the fall_damage() zero-damage cutoff (~6.7u); the
                                           // formula's `dmg > 0` stays the final arbiter.
        if let Some(height) = self.controller.controller_view.lock().unwrap().landed_fall_height.take() {
            if height > SAFE_FALL_HEIGHT {
                let (dmg, _max) = fall_damage(height);
                if dmg > 0 {
                    stream.send_app_packet(OP_ENV_DAMAGE, &build_env_damage_packet(gs.player_id, dmg, DMGTYPE_FALLING));
                    gs.cur_hp = (gs.cur_hp - dmg as i32).max(0);
                    gs.log_msg("combat", &format!("Fell {:.0}u — {} fall damage", height, dmg));
                    tracing::info!("EQ: fall damage {dmg} (fell {height:.0}u)");
                }
            }
        }
        let gp = [gs.player_x, gs.player_y, gs.player_z];
        if !self.streamed_init {
            self.last_streamed = gp;
            self.last_pos_send = Instant::now();
            self.last_sent_pos = gp;
            self.streamed_init = true;
            return;
        }
        // Genuine server correction: the network gs player jumped (an incoming server packet moved
        // us) far from what we last mirrored. Hand it to the controller; adopt and re-stream it.
        // NOTE (#593): this branch does NOT set `player_pos_known` — it hands the position to the
        // controller and returns immediately, before the controller has actually adopted it. The
        // flag only flips once a LATER tick reaches the normal path below with `view.pos` mirroring
        // the adopted position. The one gap this leaves: if a new zone's spawn point lands within
        // `CORRECTION_SQ` (a squared distance equal to (12u)²) of the last-streamed old-zone
        // position, this branch is skipped entirely and the flag flips on that stale-but-close
        // controller position on the very next normal-path tick instead — bounded to ≤12u
        // horizontally — the check is 2D (`cd` omits `dz`), so a stale Z is not bounded by it; see
        // #593(c) for why the horizontal residual window isn't a meaningful falsehood.
        let cd = [gp[0] - self.last_streamed[0], gp[1] - self.last_streamed[1]];
        if cd[0] * cd[0] + cd[1] * cd[1] > CORRECTION_SQ {
            tracing::info!("NAV: server correction → handing controller new pos ({:.1},{:.1},{:.1})", gp[0], gp[1], gp[2]);
            *self.controller.pos_correction.lock().unwrap() = Some(gp);
            // #846 review B3: this branch returns EARLY, so `gs.player_x/y/z` keep the SERVER's new
            // coordinates while the two disclosures above still hold the frozen controller's answer
            // about the place we were just moved OUT of — a fresh position beside an old
            // predicament, with nothing in the payload marking the pair as coming from two different
            // instants. Measured to recur indefinitely, not for one tick, whenever the server
            // re-asserts the correction while the render loop idles (`D.D.D.D.D.D.`, diverged 6 of
            // 12 ticks at a 50% duty cycle).
            //
            // Withdraw both instead. This is NOT the net thread inventing an answer: `pos_correction`
            // has just been handed to `CharacterController::teleport`, whose FIRST act is to drop the
            // hold and reset the afloat window, so `None` is the value the render thread is about to
            // publish for this position — we are pairing the position with its own disclosure one
            // tick early, not overriding the controller. The direction matters too: withdrawing can
            // only lose a warning for a tick, whereas keeping the old `Some` asserts a wedge at
            // coordinates the body was just lifted away from, which is the #343 shape and the thing
            // #846 is about.
            (gs.player_hold, gs.player_afloat_stall) = (None, None);
            // `from = gp` (== the sent position): a server correction is a snap-adopt, not motion
            // WE performed, so it must report zero speed/anim, never a spike from whatever
            // `last_sent_pos` happened to be before the jump (#624 review — the reviewer confirmed
            // this path does not spike; keep it that way explicitly rather than by accident).
            self.send_position_update(stream, gs, gp, gp[0], gp[1], gp[2], gs.player_heading);
            self.last_streamed = gp;
            self.last_pos_send = Instant::now();
            self.last_sent_pos = gp;
            return;
        }
        // Normal: stream the controller's position at cadence, then mirror into gs for game logic.
        let pos = view.pos;
        let since = self.last_pos_send.elapsed().as_millis();
        let d = [pos[0] - self.last_streamed[0], pos[1] - self.last_streamed[1], pos[2] - self.last_streamed[2]];
        let moved = d[0] * d[0] + d[1] * d[1] + d[2] * d[2] > 0.01;
        if (moved && since >= POS_SEND_MOVING_MS) || since >= POS_SEND_KEEPALIVE_MS {
            // `from = self.last_sent_pos`: the position as of the last packet actually SENT, so the
            // distance covered here shares the exact same window as `dt_secs` inside
            // `send_position_update` (`self.last_pos_send.elapsed()`, read before we update it just
            // below). Using `gs.player_x/y/z` here (the #624-review bug) would instead measure only
            // the most recent ~10ms tick's movement against the full ~280-1300ms throttle interval,
            // flooring every sustained run's reported speed back down near the walking constant.
            self.send_position_update(stream, gs, self.last_sent_pos, pos[0], pos[1], pos[2], view.heading);
            self.last_pos_send = Instant::now();
            self.last_sent_pos = pos;
        }
        gs.player_x = pos[0];
        gs.player_y = pos[1];
        gs.player_z = pos[2];
        // #513: our position is now ESTABLISHED — this is the controller's real placement for the
        // current zone, the very value we stream to the server. Before this normal-path tick runs
        // for the new zone (i.e. between `begin_zone_in` and the controller being placed here),
        // `player_x/y/z` still hold whatever they held before: the OLD zone's last-known
        // coordinates on every zone change after the first, or the struct's construction-time
        // zeroes on the very first zone-in of the session. Anything derived from `player_x/y/z` in
        // that window — notably the `distance` a name-resolution endpoint reports — would be a
        // confident wrong number (a stale-but-plausible old position is the more dangerous case,
        // since it doesn't look absurd the way an origin-relative one would). Consumers gate on
        // `player_pos_known` via `HttpState::player_pos()` and report an honest "unknown".
        //
        // **#846 review / #871: the last two sentences of the paragraph above USED to end "either
        // way `player_pos_known` is false during that window", and that is not true.** The only gate
        // on this whole path is `view.initialized`, which `app.rs` sets once when the controller
        // first spawns and never re-clears on a zone change. So the first net tick after
        // `begin_zone_in` runs THIS line — writing the DEPARTED zone's controller position into
        // `player_x/y/z` and flipping the flag back to true on it — roughly 10 ms into a load that
        // takes seconds. `player_pos_known`'s own doc states the precondition ("once the controller
        // has actually been placed in this zone"); nothing here enforces it, because the mirror
        // cannot tell that placement from the one before it. Filed as #871 rather than fixed in
        // #846: same mechanism as the hold (one unconditional mirror, no zone identity on the
        // view), different field, and this one's blast radius is position streaming.
        gs.player_pos_known = true;
        gs.player_heading = view.heading;
        self.last_streamed = pos;
    }

    fn send_position_update(
        &mut self,
        stream:  &mut EqStream,
        gs:      &GameState,
        from: [f32; 3],
        x: f32, y: f32, z: f32,
        heading: f32,
    ) {
        let dx = x - from[0]; // east  delta (server_x)
        let dy = y - from[1]; // north delta (server_y)
        let dz = z - from[2];
        // Real speed, not a moving/idle flag (#624): the distance just covered divided by the wall
        // time since we last sent a position update. `from` is the EXPLICIT position as of the last
        // real send (`self.last_sent_pos` at the throttled call sites in `stream_position`, or the
        // destination itself — a deliberate zero — at the melee-facing call in
        // `drive_auto_engage_melee`), not `gs.player_x/y/z` (mirrored every ~10ms tick regardless of
        // whether a send fires — using that as `from` was the #624-review bug: it measured only the
        // most recent tick's movement while `dt_secs` below measured the full throttle interval, so
        // a sustained run's reported speed floored back down near the walking constant this issue
        // exists to remove). `self.last_pos_send` is still the PREVIOUS send's timestamp here —
        // every throttled caller updates it only AFTER this call returns — so `from` and `dt_secs`
        // share exactly the same window. Floored away from 0 so an accidental same-tick double send
        // can't divide by (near) zero.
        let dt_secs = self.last_pos_send.elapsed().as_secs_f32().max(0.001);
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        let anim: i32 = speed_to_wire_animation(dist / dt_secs);
        // Internal heading is CCW (0=north, 90=west). The EQ wire (and server) expects
        // CW (0=north, 90=east). The server decodes the wire heading via EQ12toFloat = wire/4,
        // and EQ headings run 0..512 (= 0..360deg), so wire = EQ_units * 4 = deg_cw * 512/360 * 4
        // = deg_cw * 2048/360. (Previously this used 4096/360 = 2x too large, so the server saw
        // the wrong facing and melee never landed — IsFacingMob failed.)
        // Internal heading is CCW (0=north, 90=west). EQ wire expects CW (0=north, 90=east).
        // EQEmu decodes wire heading via EQ12toFloat = wire/4; full circle = 512 EQ units.
        // So wire = cw_degrees * 512/360 * 4 = cw_degrees * 2048/360.
        let h_cw = crate::protocol::ccw_to_cw(heading);
        let eq_heading = crate::protocol::deg_cw_to_eq12_client(h_cw);

        let buf = encode_client_position_update(
            self.position_seq, gs.player_id as u16, [x, y, z], [dx, dy, dz], eq_heading, anim);
        self.position_seq = self.position_seq.wrapping_add(1);
        // Position is a transient firehose — send it UNRELIABLY (ack_req=false), exactly like the
        // native client and the server's own position broadcasts. Sending it on the reliable stream
        // (which we never retransmit) makes a single dropped datagram an unfillable sequence gap, so
        // long continuous runs — which send the most position packets — reliably linkdead (eqoxide#127).
        // DELIBERATE (#612): the position firehose is the one send with NO retransmit of any kind,
        // so a failure here is a genuinely lost update. It is counted in BOTH
        // `NetHealth::send_failures` and `send_failures_unretried` by `transmit` (pollable at
        // /v1/observe/debug), which is what an agent needs: a run of unretried failures means the
        // server's idea of where we are has stopped tracking ours. Not escalated further here — the
        // next update ~50ms later supersedes this one, and aborting the loop on a transient
        // WouldBlock would be worse than the drop.
        if let Err(e) = stream.send_app_packet_unreliable(OP_CLIENT_UPDATE, &buf) {
            tracing::debug!("NET: position update seq {} did not reach the wire ({:?}) — no \
                             retransmit; the next update supersedes it (#612)",
                            self.position_seq.wrapping_sub(1), e.kind());
        }
    }

    /// Resolve a DRNTP zone-line region's `region_index` to its crossing destination — the target
    /// `(zone_id, [x, y, z])` of the advertised zone point whose `iterator` matches. `None` only
    /// when NO advertised zone point matches the index (a WLD index the `OP_SendZonepoints` list
    /// never carried — a `.wtr`/data gap); such a region is left alone rather than crossed blindly.
    ///
    /// A destination whose `zone_id == current zone` is NOT suppressed here: same-zone DRNTP lines
    /// are legitimate retail content (intra-zone translocators — e.g. the 5 qeynos2 teleport pads,
    /// and 546 such rows DB-wide), and the player stepping on one must be teleported, not stranded.
    /// The self-zone case is instead handled at the call site: it still sends OP_ZoneChange (so the
    /// server repositions us in-zone via `DoZoneSuccess`), applies the resolved coords locally so we
    /// leave the region, and flags the echo to skip the world reconnect (#368). The wedge was never
    /// the crossing itself — it was the receive side reconnecting on the same-zone reposition echo.
    fn resolve_cross_destination(&self, region_index: i32) -> Option<(u16, [f32; 3])> {
        self.world.zone_points.lock().unwrap().iter()
            .find(|zp| zp.iterator as i32 == region_index && zp.zone_id != 0)
            .map(|zp| (zp.zone_id, [zp.server_x, zp.server_y, zp.server_z]))
    }

    /// Apply a SAME-ZONE translocator's in-zone reposition. The position is AUTHORITATIVE, not
    /// provisional: the same-zone branch of `perform_cross` sends NO OP_ZoneChange (see there for why
    /// zoneID=0 would misroute us cross-zone), so there is no server echo to wait for — the server
    /// accepts our streamed OP_ClientUpdate position unconditionally. We therefore mark the position
    /// KNOWN and must NOT set `position_provisional_since`: with no packet there is no echo, and none
    /// of that flag's server-position clearers would ever fire, so it would stick `true` forever — an
    /// observable that never clears, a worse lie than the field it was meant to caveat (#679 review,
    /// confirmed live: `crossing_pending_ms` grew unbounded and survived a full /goto).
    fn apply_in_zone_reposition(gs: &mut GameState, index: i32, dest_pos: [f32; 3]) {
        // 999999 / 999 sentinel from the zone point = "keep current position" (zoning.cpp:311): the
        // server keeps us put, so don't teleport to the sentinel — region-leave then relies on the
        // cooldown alone (rare; most same-zone points carry real target coords).
        if dest_pos.iter().all(|c| c.abs() < 900_000.0) {
            gs.player_x = dest_pos[0];
            gs.player_y = dest_pos[1];
            // Zone-point target coords are wire-datum (DB safe coords, model-origin z ~3.1u above the
            // floor) — convert to the internal foot datum (#522).
            gs.player_z = dest_pos[2] - eqoxide_core::coord::WIRE_Z_OFFSET;
            tracing::info!("zone_cross: index={index} → in-zone teleport to ({:.0},{:.0},{:.0})",
                dest_pos[0], dest_pos[1], dest_pos[2]);
        } else {
            tracing::info!("zone_cross: index={index} → in-zone teleport (sentinel keep-position)");
        }
        // The reposition is authoritative (the server accepts the streamed position). Mark it KNOWN;
        // do NOT set `position_provisional_since` — with no packet there is no echo to clear it.
        gs.player_pos_known = true;
        gs.log_msg("zone", "In-zone teleport");
    }

    /// The standing auto-cross hit a zone-line region whose index resolves to NO advertised zone
    /// point (#683 — qrg's only exit region is baked `index=0`, so its exit was a silent no-op and
    /// the character could never leave the zone). Send the same OP_ZoneChange(zoneID=0) a genuine
    /// resolved crossing sends and let the server derive the destination from our position, gated
    /// by [`classify_unresolved_cross`] — see there for the #679-drift defense and the exact
    /// scope of its guarantee (what is property-tested vs. what is a maintained writer invariant).
    ///
    /// HONESTY: nothing here predicts or reports a destination — the client cannot know where the
    /// server will place it. The arrival zone comes exclusively from the server's OP_ZoneChange
    /// echo (`classify_zone_change_echo` → `CrossZoneReconnect` → the normal zone-entry handshake),
    /// so the agent is told where it actually landed, never a guess.
    ///
    /// Returns **true if a packet actually went out** — the caller counts that against the #713
    /// attempt bound. The gated (`Ignore`) arm returns false: it sends nothing, costs the server
    /// nothing, and already has its own once-per-stand report, so bounding it would only make the
    /// gate's refusal go quiet after three ticks for no gain.
    fn cross_unresolved(&mut self, stream: &mut EqStream, gs: &mut GameState, index: i32) -> bool {
        let disposition = {
            let zps = self.world.zone_points.lock().unwrap();
            classify_unresolved_cross(&zps, gs.world.zone_id)
        };
        match disposition {
            UnresolvedCross::SendServerResolved => {
                // Stamp the cooldown exactly like `perform_cross` does: the server round-trip takes
                // tens of ms and we are still standing in the region the whole time — without this
                // the fallback would refire every nav tick.
                self.last_zone_cross = Instant::now();
                self.send_zone_change_packet(stream, gs, 0);
                tracing::info!("zone_cross: zone-line region index={index} has no advertised zone point — requesting a server-resolved crossing (zoneID=0)");
                gs.log_msg("zone", "Crossing zone line (destination resolved by server)");
                true
            }
            UnresolvedCross::Ignore => {
                // Report the refusal ONCE per stand (latch cleared when the probe reads no region).
                // A `tracing::debug!` alone left an agent standing on a `zone_id: null` exit in a
                // gated zone with zero feedback — "crossing in progress" and "refused" were
                // indistinguishable (#683 review F1, measured live in qeynos2).
                // The two stated causes mirror the classify gates EXACTLY (round-3 precision fix):
                // "no server zone points available" covers every way the list can lack a
                // server-originated advert — not yet received, all entries dropped client-side
                // (e.g. the 999999 keep-position sentinel filter), or only synthesized map-label
                // entries present — not just "not yet received".
                if self.gated_cross_reported != Some(index) {
                    self.gated_cross_reported = Some(index);
                    gs.log_msg("zone", "Standing on a zone line with unknown destination — auto-cross is disabled here (no server zone points are available for this zone, or this zone has teleport pads)");
                }
                tracing::debug!("zone_cross: zone-line region index={index} unresolved and the server-resolved fallback is gated (no server zone points available, or a same-zone pad is advertised here) — not crossing");
                false
            }
        }
    }

    fn perform_cross(&mut self, stream: &mut EqStream, gs: &mut GameState, index: i32, dest_zone: u16, dest_pos: [f32; 3]) -> bool {
        self.last_zone_cross = Instant::now();
        if dest_zone == gs.world.zone_id {
            // SAME-ZONE translocator (an intra-zone pad — e.g. the qeynos2 guild pads). The zone point
            // resolves to THIS zone, so it is a pure IN-ZONE teleport, and we must NOT send OP_ZoneChange.
            //
            // Sending OP_ZoneChange(zoneID=0) here was the bug: zoneID=0 makes the server resolve the
            // destination by nearest-XY (`GetClosestZonePointWithoutZone`). A same-zone zone point's DB
            // trigger is a 0,0,0 placeholder, so a real CROSS-zone trigger wins the nearest-XY race and
            // the server zones us to the WRONG zone (qeynos2 guild pad → South Qeynos) — on TOP of the
            // correct local in-zone reposition, i.e. "right location in the zone, then zoned away."
            // Verified against the native RoF2 client: its guild pads teleport within North Qeynos with
            // no zone change. An in-zone move needs no OP_ZoneChange — the OP_ClientUpdate position
            // stream tells the server where we moved (it applies absolute position unconditionally).
            // Only a genuine CROSS-zone line (the `else` branch) sends the packet and zones.
            self.same_zone_cross_at = Some(Instant::now());
            Self::apply_in_zone_reposition(gs, index, dest_pos);
            // STOP the walker (#508). The crossing we were asked to make already happened: the
            // translocator repositioned us in-zone. But the walker's `goto_target` still points at
            // the pre-cross goal (the zone-line coords `drain_zone_cross` walked us to, or a `/goto`
            // beyond it), and the reposition just TELEPORTED us elsewhere in the zone — so that path
            // is now stale. Resuming it walks us across a DIFFERENT zone's real DRNTP line and dumps
            // us in a zone we never requested (qeynos2 → drifts into qeynos). Clear the nav
            // destination so nav terminates honestly (idle) at the reposition; a caller that wants to
            // keep moving re-issues a fresh /goto. A genuine CROSS-zone crossing takes the `else`
            // branch below and is untouched — it zones and its post-zone nav is separate.
            self.command.request_stop();
            true
        } else {
            // Genuine CROSS-zone line: request the zone change (zoneID=0 → the server resolves the true
            // destination from our position, correct for a real walk-in crossing, #199) and let the
            // normal world reconnect / zone-entry handshake run.
            self.send_zone_change_packet(stream, gs, dest_zone);
            tracing::info!("zone_cross: in zone-line region index={index} → zone_id={dest_zone}");
            gs.log_msg("zone", &format!("Crossing to zone {}", dest_zone));
            false
        }
    }

    /// True if a SAME-ZONE walk-in cross fired recently enough that a `success=1` OP_ZoneChange
    /// echo is its in-zone reposition (skip the world reconnect, #368). **NON-consuming** (#554):
    /// a duplicate / retransmitted echo must classify the SAME way as the first, so this is a peek,
    /// not a take — consuming it on the first echo was exactly what let a duplicate fall through to
    /// a spurious reconnect (the bounce). Bounded to a short window so a stale flag can never
    /// suppress a later genuine cross-zone or death/bind reconnect: the reposition echo always
    /// returns within the round-trip (tens of ms), so 1.5s is a 20-50x safety margin over the
    /// observed echo latency while still shrinking the wrongly-suppressed-reconnect edge ~3x versus
    /// the original 5s window (#504, #503 follow-up). The window is the only clear; there is no
    /// consume.
    pub(crate) fn same_zone_reposition_pending(&self) -> bool {
        const WINDOW_MS: u128 = 1500;
        matches!(self.same_zone_cross_at, Some(t) if t.elapsed().as_millis() <= WINDOW_MS)
    }

    /// Classify an inbound OP_ZoneChange `success` echo, supplying this loop's live same-zone
    /// pending flag. The receive side (`gameplay.rs`) calls this ONCE per echo and dispatches on the
    /// result — a single classify→dispatch, so the #554 double-cross is unrepresentable. See
    /// [`classify_zone_change_echo`].
    pub(crate) fn classify_zone_change_echo(
        &self, success: i32, echo_zone_id: u16, current_zone_id: u16,
    ) -> ZoneChangeEcho {
        classify_zone_change_echo(success, echo_zone_id, current_zone_id, self.same_zone_reposition_pending())
    }

    /// Send OP_ZONE_CHANGE to request crossing a zone line to `target_zone_id`.
    /// ZoneChange_Struct (88 bytes): char_name[64] + zoneID(u16) + instance_id(u16)
    ///   + y(f32) + x(f32) + z(f32) + zone_reason(u32) + success(i32=0)
    /// NOTE: zoneID is sent as **0** (the "resolve from my position" sentinel), NOT the resolved
    /// destination. On zoneID==0 the server (`Handle_OP_ZoneChange`, `zone/zoning.cpp:49`) routes to
    /// `GetClosestZonePointWithoutZone` (`zone.cpp:2093`) — an XY-only, z-agnostic match with no
    /// water-map/OBB check — and derives the real destination from the matched zone point. Sending a
    /// nonzero destination instead routes to `GetClosestZonePoint`, whose water-map `InZoneLine` OBB
    /// test (z-bounded) rejects a valid walk-in with a stale tracked z and logs
    /// `MQZone … with Unknown Destination` (a false positive that could flag/kick on a strict server),
    /// and also hard-cancels if the matched point's target != the named zone. zoneID=0 avoids both.
    /// (`target_zone_id` is kept for logging/clarity; the server resolves the true target itself.)
    /// This is NOT the same as the old bug of sending our *current* zone (target==current → cancel):
    /// 0 is the documented resolve-from-position sentinel, not a zone id. (eqoxide#199)
    fn send_zone_change_packet(&self, stream: &mut EqStream, gs: &GameState, target_zone_id: u16) {
        // RoF2 ZoneChange_Struct is 100 bytes (rof2_structs.h): char_name[64], zoneID@64,
        // instanceID@66, Unknown068@68, Unknown072@72, y@76, x@80, z@84, zone_reason@88,
        // success@92, Unknown096@96. (Titanium put y/x/z at @68/@72/@76 — 8 bytes earlier — which
        // made the RoF2 server read garbage coords and silently ignore the zone-change request.)
        let mut buf = [0u8; 100];
        let name_bytes = gs.player_name.as_bytes();
        let name_len = name_bytes.len().min(64);
        buf[..name_len].copy_from_slice(&name_bytes[..name_len]);
        buf[64..66].copy_from_slice(&0u16.to_le_bytes());             // zoneID = 0 → server resolves from pos (avoids MQZone false positive; eqoxide#199)
        buf[66..68].copy_from_slice(&0u16.to_le_bytes());             // instanceID = 0 (server resolves from matched zone point)
        // @68..76 Unknown068/Unknown072 left zero.
        buf[76..80].copy_from_slice(&gs.player_y.to_le_bytes());      // y (north)
        buf[80..84].copy_from_slice(&gs.player_x.to_le_bytes());      // x (east)
        buf[84..88].copy_from_slice(&(gs.player_z + eqoxide_core::coord::WIRE_Z_OFFSET).to_le_bytes()); // z (wire datum, #522)
        buf[88..92].copy_from_slice(&0u32.to_le_bytes());             // zone_reason = 0
        buf[92..96].copy_from_slice(&0i32.to_le_bytes());             // success = 0 (request)
        tracing::info!("EQ: sending OP_ZONE_CHANGE target_zone={} from current_zone={} pos=({:.1},{:.1},{:.1})",
                  target_zone_id, gs.world.zone_id, gs.player_x, gs.player_y, gs.player_z);
        stream.send_app_packet(OP_ZONE_CHANGE, &buf);
    }
}

#[cfg(test)]
mod fine_tier_tests {
    use eqoxide_nav::steering::*;
    use eqoxide_nav::collision::{LocalOutcome, NoRoute, PlanLimit};

    /// A tiny deterministic LCG. No new dependency, and a seeded generator means a failure is
    /// reproducible — which a `rand`-seeded property test would not be.
    struct Lcg(u64);
    impl Lcg {
        fn next_u32(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn f32_in(&mut self, lo: f32, hi: f32) -> f32 {
            lo + (self.next_u32() as f32 / u32::MAX as f32) * (hi - lo)
        }
        fn usize_below(&mut self, n: usize) -> usize {
            if n == 0 { 0 } else { self.next_u32() as usize % n }
        }
    }

    fn random_path(rng: &mut Lcg, n: usize) -> Vec<[f32; 3]> {
        (0..n).map(|_| [rng.f32_in(-500.0, 500.0), rng.f32_in(-500.0, 500.0), rng.f32_in(-50.0, 50.0)])
            .collect()
    }

    /// # PROPERTY: **THE WALKER CAN NEVER STALL WAITING ON THE FINE PLAN.** (#382)
    ///
    /// The fine 2u plan now comes back from a worker thread, so on any given tick the fine tier may be
    /// in ANY of these states, and the walker must drive regardless:
    ///
    /// * never asked (`local` empty, first tick of a route)
    /// * still computing (`local` empty, or holding the PREVIOUS plan)
    /// * dead (`local` frozen at whatever it last held, forever)
    /// * answered with nothing usable (`local` empty or a 1-waypoint stub)
    /// * answered with a partial from a position the walker has since driven past
    ///
    /// **Every one of those is just "some `local` slice", and `steer_target` is TOTAL over all of
    /// them.** There is no input for which it has no aim, and therefore no state in which the walker
    /// waits. That is why the fine tier's absence degrades steering instead of blocking it.
    ///
    /// This is a UNIVERSAL claim ("cannot stall"), and a live run cannot discharge a universal — a race
    /// that usually wins is indistinguishable from one that cannot lose. In this very codebase a
    /// `/follow` deadlock passed live verification by luck and was caught only by a pure-function test.
    /// So it is pinned here, over 20k randomised states including every degenerate shape.
    #[test]
    fn the_walker_never_stalls_waiting_on_the_fine_plan() {
        let mut rng = Lcg(0xF382_0001);
        for case in 0..20_000u32 {
            // Every shape the fine tier can hand us, degenerate ones included.
            let local: Vec<[f32; 3]> = match case % 6 {
                0 => Vec::new(),                        // never asked / still computing / dead-empty
                1 => random_path(&mut rng, 1),          // a 1-waypoint stub: steers nowhere
                2 => random_path(&mut rng, 2),          // the minimum usable plan
                3 => { let n = rng.usize_below(30); random_path(&mut rng, 2 + n) } // an ordinary fine plan
                4 => vec![[7.0, 7.0, 0.0]; 4],          // fully degenerate: zero-length segments
                _ => { let n = rng.usize_below(4); random_path(&mut rng, 2 + n) }  // a stale partial
            };
            // ...against every shape of coarse route, since that is the fallback the aim rests on.
            let coarse: Vec<[f32; 3]> = match case % 4 {
                0 => { let n = rng.usize_below(40); random_path(&mut rng, 2 + n) }
                1 => random_path(&mut rng, 2),
                2 => vec![[3.0, 3.0, 0.0]; 3],          // degenerate coarse route
                _ => random_path(&mut rng, 8),
            };
            // ...from anywhere, with ANY cursor value, including ones far past the end of the path (a
            // cursor that outran a plan the worker then replaced with a shorter one).
            let from = [rng.f32_in(-600.0, 600.0), rng.f32_in(-600.0, 600.0), rng.f32_in(-600.0, 600.0)];
            let path_i = rng.usize_below(coarse.len() + 3);
            let mut local_i = rng.usize_below(local.len() + 3);
            let fallback = [rng.f32_in(-600.0, 600.0), rng.f32_in(-600.0, 600.0), 0.0];

            // Always-clear LOS: this property pins the fine-tier no-stall TOTALITY, which is orthogonal
            // to the #685 corner clamp — an all-clear predicate keeps steer_target's aim identical to
            // pre-#685 so the totality claim under all fine-tier shapes is what is under test here.
            let aim = steer_target(&coarse, path_i, &local, &mut local_i, from, 5.0, fallback, |_, _| true);

            // THE PROPERTY: an aim always exists, and it is a real point the walker can be driven at.
            // (`steer_target` returns `[f32;3]`, not `Option` — the no-stall guarantee is in the TYPE.
            // This pins the other half: that no input makes it produce a NaN the controller would
            // silently turn into a frozen wish_dir.)
            assert!(aim.iter().all(|c| c.is_finite()),
                "case {case}: the walker must ALWAYS have a finite aim — there is no fine-tier state in \
                 which it may wait. got {aim:?} (local={} wp, coarse={} wp)", local.len(), coarse.len());
            // And the cursor stays inside the path it indexes, however absurd its starting value.
            assert!(local.len() < 2 || local_i < local.len(),
                "case {case}: the fine cursor must stay in bounds (local_i={local_i}, len={})", local.len());
        }
    }

    /// # PROPERTY: **A LIMIT CAN NEVER BE REPORTED AS "NO WAY THROUGH".** (#382, the #337 disease)
    ///
    /// The proactive coarse re-plan (#246) is armed when the fine tier says the committed route cannot
    /// be threaded from here. Under the deleted 150 ms wall clock it was armed whenever the fine path
    /// merely fell short of the carrot — and a search that *ran out of clock* falls short of the carrot
    /// in exactly the same way a search that *proved the corridor impassable* does. So a TIMEOUT was
    /// laundered into "the route ahead is blocked": under CPU load, corridors that were perfectly
    /// threadable got torn up and re-planned, and (per #379) the coarse tier learned nothing from it and
    /// re-proposed the same corridor forever.
    ///
    /// The two answers are now different VALUES, and only the one that actually looked at the whole
    /// window may arm anything. This is universal over every limit and every partial, so it is a
    /// property, not an example.
    #[test]
    fn a_search_that_stopped_looking_can_never_arm_a_replan() {
        let mut rng = Lcg(0xF382_0002);
        for _ in 0..2_000 {
            let n = rng.usize_below(20);
            let steer = random_path(&mut rng, n);
            // "I stopped looking" (the node cap — the only limit that exists now, #394), and whatever
            // partial it dribbled out.
            {
                let o = LocalOutcome::Exhausted { limit: PlanLimit::NodeCap, steer: steer.clone() };
                assert!(!arms_coarse_replan(&o),
                    "an EXHAUSTED search did not look at the window — treating it as 'the corridor is \
                     blocked' is a limit laundered into a fact");
                assert_ne!(o.state(), "no_way_through",
                    "and it must never be PUBLISHED as 'no way through' either");
            }
            // "I looked at all of it; there is no way" — the only outcome that is evidence of anything.
            for why in [NoRoute::SearchClosed, NoRoute::StartIsolated, NoRoute::GoalNotWalkable, NoRoute::NoGeometry] {
                let o = LocalOutcome::NoWayThrough { steer: steer.clone(), why };
                assert!(arms_coarse_replan(&o),
                    "a CLOSED window IS evidence the coarse corridor is not threadable — it must still \
                     arm the #246 re-plan, or this change trades one bug for a worse one");
            }
            // And a threaded route obviously arms nothing.
            assert!(!arms_coarse_replan(&LocalOutcome::Threaded(steer.clone())));
        }
    }

    /// # PROPERTY: **THE FINE TIER CAN NEVER SAY `no_path`.** (#382)
    ///
    /// `no_path` is the client's DEFINITIVE, falsifiable "there is no route" — the one word an agent is
    /// entitled to act on by giving up on a goal. The fine tier searches a **40 u window**. A closed
    /// window proves nothing whatever about the goal, which is typically hundreds of units away. If the
    /// fine tier could reach `no_path`, a character standing in front of a tight doorway would tell its
    /// agent the destination is unreachable — a confident falsehood, and the single worst thing this
    /// planner can say.
    ///
    /// It cannot, and the reason is structural rather than a guard: `LocalOutcome` has **no variant
    /// that spells a definitive no**, so there is nothing to map. This pins the mapping anyway, because
    /// a future hand could add one.
    #[test]
    fn the_bounded_fine_tier_can_never_report_a_definitive_no_path() {
        let mut rng = Lcg(0xF382_0003);
        for _ in 0..500 {
            let n = rng.usize_below(10);
            let steer = random_path(&mut rng, n);
            let outcomes = [
                LocalOutcome::Threaded(steer.clone()),
                LocalOutcome::NoWayThrough { steer: steer.clone(), why: NoRoute::SearchClosed },
                LocalOutcome::NoWayThrough { steer: steer.clone(), why: NoRoute::StartIsolated },
                LocalOutcome::NoWayThrough { steer: steer.clone(), why: NoRoute::GoalNotWalkable },
                LocalOutcome::Exhausted { limit: PlanLimit::NodeCap, steer: steer.clone() },
            ];
            for o in &outcomes {
                assert_ne!(o.state(), "no_path",
                    "a 40u window can NEVER prove a goal unreachable — the fine tier must have no way to \
                     say `no_path`, or a tight doorway becomes 'your destination does not exist'");
                assert_ne!(o.state(), "search_exhausted",
                    "nor may it borrow the COARSE planner's terminal states: those stop the walker, and a \
                     local dead-end must not");
                // Every outcome carries a steer hint — the walker is never left with nothing to follow.
                assert_eq!(o.steer().len(), steer.len(),
                    "every fine outcome must carry its steering hint (the halas swimmer, #377 review N1)");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// **#679 — a same-zone in-zone reposition is AUTHORITATIVE, not provisional, and must not leave
    /// a stuck marker.**
    ///
    /// The same-zone branch of `perform_cross` sends NO OP_ZoneChange (zoneID=0 would make the server
    /// resolve nearest-XY and misroute us cross-zone), so there is no server echo. The previous code
    /// marked `position_provisional_since` here and relied on a server-position event to clear it —
    /// but with no packet none ever comes, so the flag stuck `true` forever (confirmed live in the
    /// #679 review: `crossing_pending_ms` grew unbounded and survived a full /goto). An observable
    /// that never clears is a worse lie than the field it was meant to caveat. The position is
    /// server-accepted and authoritative, so `apply_in_zone_reposition` applies the arrival, marks it
    /// KNOWN, and does NOT set the provisional flag.
    #[test]
    fn an_in_zone_reposition_is_authoritative_not_provisional_679() {
        let mut gs = GameState::new();
        gs.player_x = -615.0; gs.player_y = -83.0; gs.player_z = -14.0;
        gs.player_pos_known = false;
        assert!(gs.position_provisional_since.is_none(), "precondition: not provisional yet");

        ActionLoop::apply_in_zone_reposition(&mut gs, 2, [-153.0, -30.0, 9.0]);

        // 1. The arrival IS applied (this is what makes us leave the trigger region and not re-fire).
        assert_eq!(gs.player_x, -153.0);
        assert_eq!(gs.player_z, 9.0 - eqoxide_core::coord::WIRE_Z_OFFSET, "wire→foot datum (#522)");
        // 2. It is NOT marked provisional. No packet was sent, so no server echo would ever clear it;
        //    a stuck `position_provisional: true` is an observable that never clears. Mutation: set
        //    `position_provisional_since` in `apply_in_zone_reposition` and this assertion goes RED.
        assert!(gs.position_provisional_since.is_none(),
            "a same-zone in-zone reposition is authoritative (no echo to clear a provisional flag) — \
             it must NOT mark the position provisional");
        // 3. The position is marked KNOWN — it is where we are, server-accepted.
        assert!(gs.player_pos_known, "the in-zone reposition position is authoritative → known");
    }

    use super::*;
    use crate::packet_handler::apply_packet;
    use crate::transport::AppPacket;
    use eqoxide_nav::steering::{NAV_LOCAL_STUCK_TICKS, PROACTIVE_REPLAN_CAP};

    /// #522 datum round-trip: the internal FOOT z survives the wire hop unchanged.
    ///
    /// The bug was a datum split across ~8 sites (easy to double-apply or drop one). This pins the
    /// invariant end to end: `foot → OUTBOUND(+offset) → wire → INBOUND(−offset) → foot` is the
    /// identity, and the byte the client actually writes carries the model-origin (wire) datum.
    ///
    /// MUTATION CHECK (each independently turns this RED): drop the `+ WIRE_Z_OFFSET` in
    /// `encode_client_position_update` → the wire-datum assert fails; flip the inbound `− WIRE_Z_OFFSET`
    /// (packet_handler self-branch) to `+` or delete it → the identity assert fails; change the
    /// constant on only one side → both fail.
    #[test]
    fn wire_z_datum_round_trips_to_foot() {
        use eqoxide_core::coord::WIRE_Z_OFFSET;
        use crate::protocol::{decode_position_update, encode_position_update};

        // Grid-aligned foot z (EQ19 is value/8) so the fixed-point encode stays lossless and the
        // test isolates the DATUM conversion from wire quantization.
        let foot = 73.875_f32;

        // OUTBOUND — the client's 46-byte PlayerPositionUpdateClient_Struct. The z the client WRITES
        // must be the model-origin (wire) datum = foot + offset, or native observers render us sunk
        // into the floor (the #522 symptom).
        let buf = encode_client_position_update(7, 42, [10.0, 20.0, foot], [0.0; 3], 0, 0);
        let wire_z = f32::from_le_bytes([buf[26], buf[27], buf[28], buf[29]]);
        assert_eq!(wire_z, foot + WIRE_Z_OFFSET, "outbound wire z must carry the model-origin datum");

        // INBOUND — drive the REAL handler: the server rebroadcasts our position as the 24-byte
        // server struct, and apply_position_update's self-branch must subtract the offset to land
        // gs.player_z back on FOOT. Going through apply_packet (not a replicated formula) means a
        // mutation of the actual inbound conversion turns this RED.
        let server_pkt = encode_position_update(42, 10.0, 20.0, wire_z, 0.0);
        // (sanity: the decoder recovers the wire datum verbatim)
        let upd = decode_position_update(&server_pkt).expect("decode server position update");
        assert!((upd.z - wire_z).abs() < 1e-4, "decoder recovers the wire z");
        let mut gs = GameState::new();
        gs.player_id = 42;            // so the packet is treated as OUR OWN position
        gs.player_z = -999.0;         // poisoned: only the handler's conversion can fix it
        apply_packet(&mut gs, &AppPacket {
            opcode: crate::protocol::OP_CLIENT_UPDATE, payload: server_pkt });
        assert!((gs.player_z - foot).abs() < 1e-4,
            "foot→wire→foot must be the identity through the real handler: sent {foot}, gs.player_z {}",
            gs.player_z);
    }

    /// #624: the wire `animation` field is `speed * 40` (with EQEmu's player special-case 0.7→28,
    /// 0.3→12 — `mob.cpp:190-196`), NOT a moving/idle boolean. Broadcasting a constant `1` (=speed
    /// 0.025, ~2% of walking pace) makes every observer render us walking regardless of our real
    /// speed, and feeds the server's anti-cheat/endurance models a nonsense value.
    ///
    /// MUTATION CHECK: restore `speed_to_wire_animation` to `if speed_u_per_s > 0.0 { 1 } else { 0 }`
    /// (the pre-fix behavior folded into this helper) → `running_speed_encodes_as_28` and
    /// `walking_speed_encodes_as_12` both go RED (1 != 28, 1 != 12).
    #[test]
    fn stationary_speed_encodes_as_zero() {
        assert_eq!(speed_to_wire_animation(0.0), 0);
    }

    #[test]
    fn running_speed_encodes_as_28() {
        // RUN_SPEED (44 u/s) IS the player-special-cased eq_runspeed_float 0.7 → animation 28.
        assert_eq!(speed_to_wire_animation(RUN_SPEED), 28);
    }

    #[test]
    fn walking_speed_encodes_as_12() {
        // Native walk speed per #623: RUN_SPEED * (0.3/0.7) ≈ 18.857 u/s → eq_runspeed_float 0.3 → 12.
        let walk_speed = RUN_SPEED * (0.3 / 0.7);
        assert_eq!(speed_to_wire_animation(walk_speed), 12);
    }

    #[test]
    fn speed_proportional_between_walk_and_run() {
        // A snared/buffed speed at HALF the run speed (22 u/s → eq_runspeed_float 0.5*0.7 = 0.35 →
        // animation 14) must land at its own proportional encoding, not snap to one of the two named
        // speeds — this is what distinguishes a real computation from a hardcoded walk/run lookup.
        let mid_speed = RUN_SPEED * 0.5;
        assert_eq!(speed_to_wire_animation(mid_speed), 14);
    }

    #[test]
    fn wire_animation_clamps_to_the_signed_10bit_field_bound() {
        // Absurdly fast (a teleport/correction, not real controller motion) must clamp to +511, and
        // a hypothetical negative speed must clamp to -512 — never overflow the field or wrap.
        assert_eq!(speed_to_wire_animation(100_000.0), 511);
        assert_eq!(speed_to_wire_animation(-100_000.0), -512);
    }

    /// #776/#801 — `stream_position` mirrors the afloat stall into `GameState`, and CLEARS it.
    ///
    /// This is the one runtime writer that carries the signal from the render thread's
    /// `ControllerView` into the network `GameState` the HTTP API serialises. Without it the field
    /// added to `GameState` in #801 would be permanently `None` and `/v1/observe/debug` would report
    /// "not stalled" about every genuinely trapped swimmer — an observable with no live writer,
    /// which is the #343 `connected: true` shape in the silent direction.
    ///
    /// The second half matters at least as much: the mirror must also be the CLEAR. A stall that is
    /// written once and never withdrawn is a false alarm from the moment the body swims free, and
    /// this signal's false-alarm direction is the one #800 actually shipped a live bug in. Because
    /// the assignment is unconditional (a plain destructuring of `view.disclosures()`, not an
    /// `if let Some(..)`), a `None` view withdraws it on the very next tick.
    ///
    /// **Axes deliberately varied:** presence/absence of the stall, and the transition in BOTH
    /// directions (absent → present → absent). **Axis deliberately NOT varied:** the body's position
    /// is not driven here — whether the CLOCK is right is `afloat_unconstructible.rs`'s and
    /// `movement.rs`'s job; this test only pins the wire between the view and the game state.
    ///
    /// MUTATION CHECKS (#801, each run independently and recorded in the PR body): drop the stall
    /// half of the mirror in `stream_position` → RED at the first assertion; change the mirror to
    /// `if let Some(s) = .. { gs.player_afloat_stall = Some(s); }` so it writes but never withdraws
    /// → RED at the last assertion.
    #[tokio::test]
    async fn stream_position_mirrors_and_withdraws_the_afloat_stall_801() {
        use eqoxide_core::afloat::{AfloatFrame, AfloatStallClock, AFLOAT_STALL_SECS};

        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut nav = new_loop();
        let mut gs = GameState::new();

        // A real matured stall — the type has no public constructor, so this is the only way in.
        let anchor = [101.5_f32, -22.0, -8.25];
        let mut clock = AfloatStallClock::default();
        for _ in 0..((AFLOAT_STALL_SECS / 0.05).ceil() as usize + 3) {
            clock.observe(AfloatFrame::Wished, anchor, 0.05);
        }
        let stall = clock.stall().expect("fixture must actually reach the disclosure threshold");

        {
            let mut view = nav.controller.controller_view.lock().unwrap();
            view.initialized = true;
            view.pos = anchor;
            view.publish_disclosures((None, Some(stall)));
        }
        nav.stream_position(&mut stream, &mut gs);
        assert_eq!(gs.player_afloat_stall, Some(stall),
            "the render thread's afloat stall must reach the GameState the HTTP API serialises — \
             without this mirror the field has no live writer and always reads \"not stalled\"");
        assert_eq!(gs.player_afloat_stall.map(|s| s.anchor()), Some(anchor),
            "and it must carry the anchor intact — an agent routes off those coordinates");

        // The body swims free: the view republishes None, and the mirror must WITHDRAW it.
        nav.controller.controller_view.lock().unwrap().publish_disclosures((None, None));
        nav.stream_position(&mut stream, &mut gs);
        assert!(gs.player_afloat_stall.is_none(),
            "the mirror is level-triggered and must also be the clear — a stall that is written \
             once and never withdrawn keeps reporting a trapped swimmer who is already swimming");
    }

    /// A `ControllerHold` fixture. Public fields, so this is just a named literal — but naming it
    /// keeps the #846 tests below reading as "the render thread published THIS" rather than as
    /// struct-literal noise.
    fn held(reason: eqoxide_core::game_state::ControllerHoldReason, secs: f32)
        -> eqoxide_core::game_state::ControllerHold
    {
        eqoxide_core::game_state::ControllerHold { reason, secs }
    }

    /// **#846 — the net thread can never publish a hold the render thread did not publish,
    /// whatever the server does to our position and however long the render loop stays idle.**
    ///
    /// This is the universal `PlayerHoldView`'s rustdoc asserts, tested as a universal rather than
    /// left as prose. #846 attacked it with a GM `#summon`: the summon lands on the NET thread, and
    /// if the net thread could invent a predicament — or keep asserting a stale one — while the
    /// render loop idled, `player.hold` would be a well-formed field an agent has no way to tell
    /// from a fresh one. That is the #343 `connected: true` shape.
    ///
    /// The render loop is modelled by NEVER republishing the `ControllerView` for the whole run:
    /// the view is written once, before the first tick, and then frozen. Every tick after that is a
    /// net tick with an idle render thread, which is exactly the window under attack. (The
    /// complementary axis — the render thread publishing DIFFERENT values over time, including a
    /// withdrawal — is `the_hold_mirror_tracks_the_render_thread_over_time_846` below. Round 1 of
    /// this PR's review found that axis missing here and a latching mirror green across the whole
    /// workspace because of it.)
    ///
    /// **Axes deliberately varied:** the published hold (absent / embedded / underworld); the size
    /// and axis of the server reposition, spanning both sides of `CORRECTION_SQ` (no jump, a 3u
    /// drift below it, a 50u summon, a 500u cross-map summon, a negative-direction summon, and a
    /// Z-only 400u drop the 2D correction test cannot see); and the number of consecutive net ticks
    /// (1..=6), because a one-tick check cannot distinguish "never" from "not yet".
    ///
    /// **The expected value is exact, not "unchanged", and the difference is the #846 review's B3
    /// fix.** On a tick that reaches the correction branch the mirror is WITHDRAWN
    /// (`player_hold = None`), because that branch returns early with `gs.player_x/y/z` holding the
    /// SERVER's new coordinates: pairing them with the frozen controller's hold would assert a wedge
    /// at a position the body was just lifted away from. `None` there is not an invention — the
    /// `pos_correction` handed over on that same statement runs `CharacterController::teleport`,
    /// which drops the hold and the afloat window as its first act — it is that value, one tick
    /// early. Both the exact expectation and the weaker never-manufacture property are asserted, so
    /// a change that starts inventing holds fails even if it also changes the branch structure.
    ///
    /// **Axis deliberately NOT varied:** whether the hold is CORRECT. Whether the controller was
    /// right to raise one is `movement.rs`'s job. This pins the property #846 disputes — that
    /// nothing on the net side of the boundary can put a predicament into the answer.
    ///
    /// **What this does NOT establish** (say it, don't imply it away): it does not prove the render
    /// loop ever wakes. The bound on how long a frozen view can persist lives in `app.rs`'s
    /// `poll_external` — a pending `pos_correction`, and any `GameState` mutation at all, mark the
    /// loop active — and `app.rs`'s event loop needs a GPU and a window, so no test in this
    /// workspace reaches it. That wake is UNGUARDED: neutering it (`if false && ..` at
    /// `app.rs:1258`) leaves the whole workspace green, measured. This test covers the honesty claim
    /// (the net side cannot make the value wrong); the wake is the latency bound.
    ///
    /// MUTATION CHECKS (#846, each run independently, results in the PR body — all are WRAP
    /// mutations that leave the mirror statement in place and executing, per #799):
    /// 1. in the correction branch, replace the withdrawal with `gs.player_hold = Some(held(..))`
    ///    (a net-side invention) → RED;
    /// 2. in the correction branch, add
    ///    `self.controller.controller_view.lock().unwrap().pos = gp;` (the net thread adopting the
    ///    correction itself instead of asking the render thread) → RED;
    /// 3. keep the mirror statement but substitute a plausible wrong value for the hold half
    ///    (`Some(ControllerHold { EmbeddedNoRecovery, 1.0 })`) → RED;
    /// 4. drop the correction branch's `(gs.player_hold, gs.player_afloat_stall) = (None, None);`
    ///    (the B3 fix) → RED on every far row.
    #[tokio::test]
    async fn no_net_tick_can_free_or_manufacture_a_hold_846() {
        use eqoxide_core::game_state::ControllerHoldReason::*;

        let published: [Option<eqoxide_core::game_state::ControllerHold>; 3] = [
            None,
            Some(held(EmbeddedNoRecovery, 3.25)),
            Some(held(UnderworldNoRecovery, 0.5)),
        ];
        // Server repositions, relative to where the controller is frozen. Both sides of
        // `CORRECTION_SQ` (144.0 = 12u, 2D) are covered on purpose: only the far ones reach the
        // correction branch, and the near ones must be just as unable to touch the hold.
        let jumps: [[f32; 3]; 6] = [
            [   0.0,    0.0,    0.0], // no reposition at all
            [   3.0,   -1.0,    0.5], // ordinary sync drift, under the correction threshold
            [  50.0,    0.0,    0.0], // a GM #summon across the room
            [   0.0,  500.0,  -30.0], // a cross-map summon
            [-320.0, -180.0,   12.0], // ...in the other direction
            [   0.0,    0.0, -400.0], // Z only — the correction test is 2D and cannot see this one
        ];

        for hold in published {
            for jump in jumps {
                for ticks in 1..=6u32 {
                    let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
                    let mut nav = new_loop();
                    let mut gs = GameState::new();

                    // The render thread publishes ONCE and then goes idle. Nothing below ever
                    // republishes: every tick from here is a net tick against a frozen view.
                    const FROZEN_POS: [f32; 3] = [812.5, -44.0, 3.75];
                    const FROZEN_HEADING: f32 = 137.5;
                    {
                        let mut view = nav.controller.controller_view.lock().unwrap();
                        view.initialized = true;
                        view.pos = FROZEN_POS;
                        view.heading = FROZEN_HEADING;
                        view.publish_disclosures((hold, None));
                    }
                    // Baseline tick: anchors `last_streamed`/`last_pos_send` and returns. Seed `gs`
                    // at the frozen position first, so the anchor is the controller's placement and
                    // not the origin — otherwise EVERY case below would enter the correction branch
                    // on its first tick (measured: it did) and the sub-threshold rows would stop
                    // testing the normal path they were added for.
                    gs.player_x = FROZEN_POS[0];
                    gs.player_y = FROZEN_POS[1];
                    gs.player_z = FROZEN_POS[2];
                    nav.stream_position(&mut stream, &mut gs);

                    // Whether this row's jump can reach the correction branch at all.
                    let far = jump[0] * jump[0] + jump[1] * jump[1] > CORRECTION_SQ;
                    let mut corrections = 0u32;

                    for t in 0..ticks {
                        // The server keeps telling us we are somewhere else — the summon, re-sent.
                        gs.player_x = FROZEN_POS[0] + jump[0];
                        gs.player_y = FROZEN_POS[1] + jump[1];
                        gs.player_z = FROZEN_POS[2] + jump[2];

                        // Per-tick probe of which branch ran. Clearing the slot only affects what
                        // this test can observe — `stream_position` writes it and never reads it —
                        // and the branch taken ALTERNATES on a far row: the correction tick sets
                        // `last_streamed` to the server's position, so the next tick sees no jump,
                        // takes the normal path and writes the controller's position back, which
                        // makes the tick after that a jump again. Expecting `None` on every far
                        // tick would be wrong for exactly that reason.
                        *nav.controller.pos_correction.lock().unwrap() = None;
                        nav.stream_position(&mut stream, &mut gs);
                        let corrected = nav.controller.pos_correction.lock().unwrap().is_some();
                        if corrected { corrections += 1; }
                        let expected = if corrected { None } else { hold };

                        let ctx = format!(
                            "hold={hold:?} jump={jump:?} far={far} corrected={corrected} \
                             tick {}/{ticks}", t + 1);

                        // 1. THE NET THREAD NEVER MANUFACTURES A PREDICAMENT. The weak form of the
                        //    property, asserted on EVERY row so it survives any change to the branch
                        //    structure: whatever ends up in the field is either the render thread's
                        //    own answer or nothing at all — never a third value the net side made up.
                        assert!(gs.player_hold.is_none() || gs.player_hold == hold,
                            "the net thread published a hold the render thread never did: {ctx} \
                             got={:?}", gs.player_hold);

                        // 2. AND THE EXACT VALUE. Mirrored verbatim on the normal path; withdrawn on
                        //    the correction tick, where `pos` is the server's and the frozen hold
                        //    would describe the place we were lifted out of (#846 review B3).
                        assert_eq!(gs.player_hold, expected,
                            "on the normal path the net thread must MIRROR the render thread's hold \
                             verbatim, and on a correction tick it must WITHDRAW it rather than \
                             pair a fresh server position with the old predicament: {ctx}");

                        // 3. AND IT DID NOT REACH ACROSS AND MOVE THE BODY ITSELF. The controller is
                        //    the render thread's; the only thing the net side may do with a
                        //    correction is publish it into `pos_correction` and wait to be asked.
                        //    Note the withdrawal above is a write to `gs`, NOT to the view: the
                        //    render thread's published answer is left exactly as it published it.
                        let view = *nav.controller.controller_view.lock().unwrap();
                        assert_eq!(view.pos, FROZEN_POS,
                            "the net thread must not write the controller's position — freeing the \
                             body is the render thread's move to make: {ctx}");
                        assert_eq!(view.heading, FROZEN_HEADING,
                            "nor its heading: {ctx}");
                        assert_eq!(view.disclosures(), (hold, None),
                            "nor either disclosure — the view is the render thread's to publish, \
                             and the correction-tick withdrawal writes `gs`, not this: {ctx}");
                    }

                    // REACH CONTROL (#778's lesson: a guard that never reaches its target passes
                    // for the wrong reason). The rows whose 2D jump clears `CORRECTION_SQ` must
                    // actually have entered the correction branch; the rows below it must not.
                    // Without this, a threshold change could quietly move the whole matrix onto the
                    // normal path and every assertion above would still pass.
                    let asked = corrections > 0;
                    assert_eq!(asked, far,
                        "reach control: a jump past CORRECTION_SQ must reach the correction branch \
                         and one under it must not (note the test is 2D, so the Z-only row is an \
                         'under it' row — that blindness is `stream_position`'s, deliberately): \
                         jump={jump:?} far={far} asked={asked}");
                }
            }
        }
    }

    /// **#846 review B2 — the hold mirror must TRACK the render thread over time, withdrawal
    /// included.** The axis the round-1 property test was missing.
    ///
    /// The matrix above varies what the SERVER does across 108 rows and freezes what the render
    /// thread published; it therefore could not see a mirror that never withdraws. Measured in
    /// review: wrapping the mirror in a latch —
    ///
    /// ```text
    /// let __prev = gs.player_hold;
    /// (gs.player_hold, gs.player_afloat_stall) = view.disclosures();
    /// if gs.player_hold.is_none() { gs.player_hold = __prev; }   // never withdraw
    /// ```
    ///
    /// — left the whole workspace green apart from the two tests THIS PR adds: with this test and
    /// `a_re_asserted_summon_never_pairs_a_fresh_pos_with_a_stale_hold_846` skipped, the latch is
    /// invisible to every other target in the repo (42 headers / 42 `test result:` lines, 1856
    /// passed, 0 failed, 45 ignored, 2 filtered — the clean baseline is 1858 passed). The round-1
    /// property test beside this one passes under all three latch mutations below, which is what
    /// made B2 a real gap and not a stylistic one. A hold that is never
    /// withdrawn is the exact failure `ControllerView::hold`'s own doc names: a body that has been
    /// freed keeps reporting wedged, indefinitely, in a well-formed field, and nothing looks wrong.
    /// The afloat half of the same statement has had this axis since #801
    /// (`stream_position_mirrors_and_withdraws_the_afloat_stall_801`); this is its missing twin, so
    /// the "published on exactly the same terms" symmetry the docs assert is now true of the
    /// coverage as well as of the code.
    ///
    /// **Axes varied:** every ordered transition between four published values — absent, two
    /// DIFFERENT `Some`s with different reasons, and a third `Some` differing from one of them only
    /// in `secs`. The last one matters: a latch keyed on `is_none()` survives a `Some`→`Some` check,
    /// and a write-once latch survives a `None`→`Some` one, so the schedule walks all of them in
    /// both directions. The body is kept still, so every tick takes the normal (mirroring) path —
    /// the correction path's deliberate withdrawal is the matrix above's job, and mixing them here
    /// would let a mutation hide in the disjunction.
    ///
    /// MUTATION CHECKS (#846, run independently, results in the PR body):
    /// 1. the latch above (mirror statement left written and executing, per #799) → RED;
    /// 2. a write-once latch (`if gs.player_hold.is_none() { gs.player_hold = view.disclosures().0 }`)
    ///    → RED;
    /// 3. mirror only the reason and keep the previous `secs` → RED.
    #[tokio::test]
    async fn the_hold_mirror_tracks_the_render_thread_over_time_846() {
        use eqoxide_core::game_state::ControllerHoldReason::*;

        // Four published values, walked as EVERY ordered pair (16 transitions, self-transitions
        // included) so no single-direction latch survives.
        let values: [Option<eqoxide_core::game_state::ControllerHold>; 4] = [
            None,
            Some(held(EmbeddedNoRecovery, 3.25)),
            Some(held(UnderworldNoRecovery, 12.0)),
            Some(held(EmbeddedNoRecovery, 41.5)), // same reason as [1], different `secs`
        ];

        const STILL: [f32; 3] = [-64.0, 220.5, 18.25];

        for from in values {
            for to in values {
                let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
                let mut nav = new_loop();
                let mut gs = GameState::new();

                {
                    let mut view = nav.controller.controller_view.lock().unwrap();
                    view.initialized = true;
                    view.pos = STILL;
                    view.publish_disclosures((from, None));
                }
                gs.player_x = STILL[0];
                gs.player_y = STILL[1];
                gs.player_z = STILL[2];
                nav.stream_position(&mut stream, &mut gs); // baseline: anchors last_streamed
                nav.stream_position(&mut stream, &mut gs);
                assert_eq!(gs.player_hold, from,
                    "precondition: the render thread's first answer reached the GameState \
                     (from={from:?} to={to:?})");

                // The render thread renders another frame and publishes a DIFFERENT answer — the
                // body was freed, or wedged, or has been wedged for longer.
                nav.controller.controller_view.lock().unwrap().publish_disclosures((to, None));
                nav.stream_position(&mut stream, &mut gs);

                assert_eq!(gs.player_hold, to,
                    "the mirror is level-triggered and the republish IS the clear: a hold written \
                     once and never withdrawn keeps reporting a wedged body that is already free, \
                     and one whose `secs` never advances reports a fresh wedge as an old one \
                     (from={from:?} to={to:?})");

                // …and it stays tracked, not just on the transition tick.
                for _ in 0..3 {
                    nav.stream_position(&mut stream, &mut gs);
                    assert_eq!(gs.player_hold, to,
                        "and it keeps tracking on later ticks (from={from:?} to={to:?})");
                }

                // Reach control: the body never moved, so nothing here took the correction branch —
                // every assertion above is about the MIRROR, not about the withdrawal beside it.
                assert!(nav.controller.pos_correction.lock().unwrap().is_none(),
                    "reach control: this test must exercise the normal (mirroring) path only — a \
                     correction here would make the withdrawal, not the mirror, explain the result");
            }
        }
    }

    /// **#846 review B1 — a zone-in must clear the departed zone's hold, and it must STAY clear
    /// when the render loop publishes nothing across the whole zone load.**
    ///
    /// `GameState::begin_zone_in` sets `player_hold = None`, and its own doc says that clear is what
    /// covers "the render loop does not publish at all" (the ~10 s asset load) while `app.rs`'s
    /// `clear_hold` covers "it keeps rendering through the load". Measured in the round-1 review:
    /// the first half did not work. `stream_position` mirrors `ControllerView::disclosures()`
    /// unconditionally on every ~10 ms net tick, so the departed zone's `Some(EmbeddedNoRecovery,
    /// 7.5)` was back one tick after the clear —
    ///
    /// ```text
    /// after begin_zone_in: hold=None                            after ONE net tick: hold=Some(..)
    /// ```
    ///
    /// — a confident "you are wedged" about geometry in a zone the character has left, served on
    /// `GET /v1/observe/debug` with nothing to distinguish it from a live one. The mirror is not at
    /// fault: it faithfully republishes what the render thread last published, and what the render
    /// thread last published is exactly the stale thing. The clear therefore has to reach the VIEW,
    /// which is what `ControllerSlots::begin_zone_in` does and what the two net-thread zone-in call
    /// sites (`gameplay::run_zone_entry_handshake`, `login.rs`'s zone reconnect) now call.
    ///
    /// The render loop is modelled by never republishing after the wedge: not one frame across the
    /// entire load. That is the case under test, and the case the pre-existing pairing claimed to
    /// cover.
    ///
    /// MUTATION CHECKS (#846, run independently, results in the PR body):
    /// 1. drop `self.controller_view.lock().unwrap().invalidate_disclosures();` from
    ///    `ControllerSlots::begin_zone_in`, leaving the `gs.begin_zone_in()` call in place and
    ///    executing (the pre-fix behaviour, and a WRAP mutation per #799) → RED on the first
    ///    post-zone-in tick;
    /// 2. revert `run_zone_entry_handshake` to `gs.begin_zone_in()` → RED in
    ///    `a_zone_entry_handshake_clears_the_departed_zones_hold_at_the_view_846`
    ///    (`gameplay.rs`), which is the call-site half of this.
    #[tokio::test]
    async fn a_zone_in_clears_the_departed_zones_hold_for_good_846() {
        use eqoxide_core::game_state::ControllerHoldReason::EmbeddedNoRecovery;

        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut nav = new_loop();
        let mut gs = GameState::new();

        const WEDGED_IN_OLD_ZONE: [f32; 3] = [-812.5, 43.0, -119.75];
        let hold = held(EmbeddedNoRecovery, 7.5);

        {
            let mut view = nav.controller.controller_view.lock().unwrap();
            view.initialized = true;
            view.pos = WEDGED_IN_OLD_ZONE;
            view.publish_disclosures((Some(hold), None));
        }
        gs.player_x = WEDGED_IN_OLD_ZONE[0];
        gs.player_y = WEDGED_IN_OLD_ZONE[1];
        gs.player_z = WEDGED_IN_OLD_ZONE[2];
        nav.stream_position(&mut stream, &mut gs); // baseline
        nav.stream_position(&mut stream, &mut gs);
        assert_eq!(gs.player_hold, Some(hold),
            "precondition: the old zone's wedge is live in the GameState the API serialises");

        // The zone-entry handshake begins. From here the render thread publishes NOTHING — it is
        // tearing down the old scene and loading the new one, which takes seconds.
        nav.controller_slots().begin_zone_in(&mut gs);
        assert!(gs.player_hold.is_none(),
            "the zone-in clear itself (this half is `GameState::begin_zone_in` and predates #846)");

        for tick in 1..=12 {
            nav.stream_position(&mut stream, &mut gs);
            assert!(gs.player_hold.is_none(),
                "net tick {tick} after the zone-in restored the DEPARTED zone's hold — the mirror \
                 republishing a predicament about geometry that has been dropped, which is exactly \
                 what an agent cannot tell from a live one (#846 review B1)");
            assert!(gs.player_afloat_stall.is_none(),
                "…and the same for the afloat stall beside it, whose staleness is sharper still: \
                 it names an ANCHOR in the departed zone's coordinate frame (net tick {tick})");
        }

        // The source, not just the copy: the value the mirror reads is what was cleared.
        assert_eq!(nav.controller.controller_view.lock().unwrap().disclosures(), (None, None),
            "the clear must reach the view the mirror reads — clearing only the GameState copy is \
             what round 1 measured as surviving exactly one net tick");

        // And the render thread can still put a hold back: the first frame it publishes in the NEW
        // zone is mirrored normally. (That it is the ONLY thing that can is the separate universal
        // held by `no_net_tick_can_free_or_manufacture_a_hold_846`; this example only shows the
        // invalidation is not a latch.)
        let new_hold = held(EmbeddedNoRecovery, 0.25);
        nav.controller.controller_view.lock().unwrap().publish_disclosures((Some(new_hold), None));
        nav.stream_position(&mut stream, &mut gs);
        assert_eq!(gs.player_hold, Some(new_hold),
            "the invalidation must not latch the field OFF either — a zone-in that permanently \
             silenced the disclosure would trade a stale wedge alarm for a missing one");
    }

    /// **#846 — the one window where `pos` and `hold` came apart, and what closed it.**
    ///
    /// `GameState::player_hold`'s doc says the position beside a stale hold "is stale in the same
    /// breath and by the same amount". On the tick `stream_position` detects a server correction
    /// (a GM `#summon`, a knockback, an anti-cheat snap) that was not true: the branch hands the
    /// jump to the controller and returns EARLY, leaving `gs.player_x/y/z` holding the SERVER's new
    /// coordinates while `gs.player_hold` still held the controller's frozen answer — a fresh
    /// position next to an old predicament, with nothing in the payload marking the pair.
    ///
    /// Round 1 of this PR called that window "ONE net tick (~10 ms) wide". **That was measured
    /// against a server that asserts the correction once.** Re-measured against one that re-asserts
    /// it every tick while the render loop idles, the window reopened on every other tick,
    /// indefinitely (`D.D.D.D.D.D.` — diverged 6 of 12) — it was "until the render loop wakes"
    /// after all, at a 50% duty cycle. Whether a live EQEmu server re-asserts is UNDETERMINED here;
    /// no live run backs this test, and the repeat model is not exotic (`app.rs:1256`'s own #116
    /// comment describes the loop that produces it).
    ///
    /// So the branch now WITHDRAWS both disclosures instead of leaving them standing, and this test
    /// pins the fixed pairing under the re-asserting server: on every correction tick `pos` is the
    /// server's and `hold` is `None`, and once the render thread adopts the correction the two are
    /// the controller's again. The withdrawal is not an invention — see the branch's comment and
    /// `CharacterController::teleport`, which drops both as its first act.
    ///
    /// MUTATION CHECKS (#846, WRAP mutations, results in the PR body):
    /// 1. drop the correction branch's `(gs.player_hold, gs.player_afloat_stall) = (None, None);`
    ///    → RED (the round-1 behaviour: `pos` fresh, `hold` stale, every other tick, forever);
    /// 2. gate the `pos_correction` hand-off on `gs.player_hold.is_none()` → RED at the
    ///    `pos_correction` assertion. That is the mutation that would make the divergence
    ///    genuinely unrecoverable in the field: a held body would never ask the render loop to
    ///    adopt anything.
    #[tokio::test]
    async fn a_re_asserted_summon_never_pairs_a_fresh_pos_with_a_stale_hold_846() {
        use eqoxide_core::game_state::ControllerHoldReason::EmbeddedNoRecovery;

        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut nav = new_loop();
        let mut gs = GameState::new();

        const WEDGED: [f32; 3] = [100.0, 200.0, 10.0];
        const SUMMONED_TO: [f32; 3] = [-250.0, 640.0, -14.0];
        let hold = held(EmbeddedNoRecovery, 7.5);

        // The render thread publishes a wedged body, then the loop goes idle. It publishes again
        // only at the very end, when it finally adopts the correction.
        {
            let mut view = nav.controller.controller_view.lock().unwrap();
            view.initialized = true;
            view.pos = WEDGED;
            view.publish_disclosures((Some(hold), None));
        }
        gs.player_x = WEDGED[0];
        gs.player_y = WEDGED[1];
        gs.player_z = WEDGED[2];
        nav.stream_position(&mut stream, &mut gs); // baseline tick
        nav.stream_position(&mut stream, &mut gs);
        assert_eq!(gs.player_hold, Some(hold), "precondition: the wedge reached the GameState");
        assert_eq!([gs.player_x, gs.player_y, gs.player_z], WEDGED,
            "precondition: and so did the position it describes");
        assert!(nav.controller.pos_correction.lock().unwrap().is_none(),
            "precondition: nothing has asked the controller to move");

        // A GM #summon lands on the net thread and the server KEEPS asserting it: every tick, the
        // OP_ClientUpdate handler writes the server's coordinates back into `gs`. The render loop
        // is idle throughout, so the controller never adopts and the branch fires every time.
        let mut corrections = 0;
        for tick in 1..=12 {
            gs.player_x = SUMMONED_TO[0];
            gs.player_y = SUMMONED_TO[1];
            gs.player_z = SUMMONED_TO[2];
            nav.stream_position(&mut stream, &mut gs);

            // Which branch ran is readable from `gs` itself, so the `pos_correction` slot is left
            // untouched all the way through — the correction the net thread parked on tick 1 must
            // still be sitting there at the end, unconsumed. (The branch ALTERNATES: a correction
            // tick sets `last_streamed` to the server's position, so the next tick sees no jump and
            // the normal path writes the frozen controller's position back.)
            let corrected = [gs.player_x, gs.player_y, gs.player_z] == SUMMONED_TO;
            if corrected { corrections += 1; }
            assert!(nav.controller.pos_correction.lock().unwrap().is_some(),
                "tick {tick}: the correction must stay parked for the render thread — the net \
                 thread hands it over and waits, it never adopts it itself");

            if corrected {
                assert_eq!([gs.player_x, gs.player_y, gs.player_z], SUMMONED_TO,
                    "tick {tick}: on the correction tick `pos` is the SERVER's — the early return \
                     leaves it standing");
                assert!(gs.player_hold.is_none(),
                    "tick {tick}: …and `hold` must be WITHDRAWN, not left describing the place the \
                     body was just lifted out of. This pairing is what #846 was filed about, and \
                     under a re-asserting server it recurs indefinitely rather than for one tick");
            } else {
                assert_eq!([gs.player_x, gs.player_y, gs.player_z], WEDGED,
                    "tick {tick}: on a normal-path tick the controller's position is written back");
                assert_eq!(gs.player_hold, Some(hold),
                    "tick {tick}: …and it is paired with the controller's own hold, both describing \
                     the same frozen instant");
            }
        }
        // REACH CONTROL: the re-asserting server must actually keep re-entering the branch. If the
        // fixture stopped producing corrections the loop above would pass vacuously on its
        // normal-path arm. Round 1 measured 6 of 12 here (the alternating duty cycle); the count is
        // asserted as ">= 6" rather than "== 6" so the test pins the recurrence, not the phase.
        assert!(corrections >= 6,
            "reach control: the re-asserting server must keep reaching the correction branch — \
             got {corrections} of 12 ticks");

        // The render loop finally wakes, adopts the correction and publishes from the new position:
        // `teleport` has dropped the hold, so what it publishes is `None` at the summoned position —
        // the same pair the net thread has been serving, now from the controller itself.
        {
            let mut view = nav.controller.controller_view.lock().unwrap();
            view.pos = SUMMONED_TO;
            view.publish_disclosures((None, None));
        }
        *nav.controller.pos_correction.lock().unwrap() = None;
        nav.stream_position(&mut stream, &mut gs);
        assert_eq!([gs.player_x, gs.player_y, gs.player_z], SUMMONED_TO,
            "after adoption the controller's position IS the server's, so the correction branch is \
             done firing and the normal path writes it back");
        assert!(gs.player_hold.is_none(), "and the pair is the controller's own again");
        assert!(nav.controller.pos_correction.lock().unwrap().is_none(),
            "reach control: no correction is detected once the controller has adopted — otherwise \
             the assertions above would be the correction branch's again, not the normal path's");
    }

    /// #624 REVIEW FOLLOW-UP: exercises the REAL send cadence — `stream_position`/
    /// `send_position_update` via the `POS_SEND_MOVING_MS` (280ms) throttle — not just the pure
    /// `speed_to_wire_animation` formula. A prior version of this fix computed the right formula but
    /// fed it a MISMATCHED WINDOW: `gs.player_x/y/z` is mirrored on every ~10ms render tick
    /// regardless of whether a send fires, so at the moment a throttled send actually goes out, the
    /// distance measured against it was only the most recent tick's sliver of movement (~10ms
    /// worth), while `dt_secs` measured the full ~280ms since the last real send — silently flooring
    /// every sustained run's reported speed back down near the walking constant this issue exists to
    /// remove. The fix anchors both the distance AND the elapsed time to `last_sent_pos`/
    /// `last_pos_send`, which are updated ONLY together, at the moment a packet is actually put on
    /// the wire (see their doc comments on the `ActionLoop` struct).
    ///
    /// The HTTP API exposes no `animation`/speed field for any entity (`/v1/observe/entities` is
    /// position-only), so the only way to check what we actually TOLD the server is to read the raw
    /// wire bytes — exactly what the reviewer did with temporary `tracing::warn!` instrumentation.
    /// This test does the same thing permanently, via a real loopback `EqStream` peer socket
    /// (`test_stream_with_peer`): `send_app_packet_unreliable`'s datagran never enters the tracked
    /// resend window, so `sent_app_packets()` (which only sees `OP_Packet`-framed reliable sends)
    /// cannot see it — a real socket is the only way to observe it.
    ///
    /// MUTATION CHECK: change the normal-path call in `stream_position` from
    /// `self.send_position_update(stream, gs, self.last_sent_pos, pos[0], pos[1], pos[2], view.heading)`
    /// back to using `[gs.player_x, gs.player_y, gs.player_z]` as the `from` position (the windowing
    /// bug this test was added to catch) → the received `anim` collapses to ~1 (fails the `anim >=
    /// 20` assertion below), even though every pure-function unit test above still passes untouched.
    #[tokio::test]
    async fn sustained_running_over_the_real_throttle_reports_real_speed_not_a_tick_sliver() {
        let (mut stream, _rx, peer_sock, _addr) =
            crate::transport::test_stream_with_peer(Default::default()).await;
        let mut nav = new_loop();
        let mut gs = GameState::new();

        // First tick: the controller hasn't "spawned" yet (`view.initialized == false` by default),
        // so this is a no-op — it must NOT consume the `!streamed_init` baseline tick below.
        nav.stream_position(&mut stream, &mut gs);
        {
            let mut view = nav.controller.controller_view.lock().unwrap();
            view.initialized = true;
            view.pos = [0.0, 0.0, 0.0];
        }
        // This tick establishes the streamed baseline (`last_streamed`/`last_pos_send`/
        // `last_sent_pos` all anchor here) and returns without sending.
        nav.stream_position(&mut stream, &mut gs);

        // Drive the controller east at exactly RUN_SPEED (44 u/s), ticking every ~10ms — the real
        // gameplay loop's cadence (`gameplay.rs:704,708`) — for just over one throttle period
        // (`POS_SEND_MOVING_MS` = 280ms), so exactly one throttled OP_ClientUpdate send fires.
        const TICK_MS: u64 = 10;
        let ticks = 320 / TICK_MS;
        let mut x = 0.0f32;
        for _ in 0..ticks {
            tokio::time::sleep(Duration::from_millis(TICK_MS)).await;
            x += RUN_SPEED * (TICK_MS as f32 / 1000.0);
            nav.controller.controller_view.lock().unwrap().pos = [x, 0.0, 0.0];
            nav.stream_position(&mut stream, &mut gs);
        }

        // Read back whatever actually hit the wire. `test_stream_with_peer`'s `SessionInfo::default()`
        // (encode_pass1/pass2 = 0, key = 0, crc_bytes = 0) is the identity encode, so a raw datagram
        // is exactly `[opcode_lo, opcode_hi, payload...]` with no CRC suffix — decode it by hand
        // rather than reaching for `transport`'s private `decode_raw_app`.
        let mut buf = [0u8; 512];
        let anim = loop {
            let (n, _from) = tokio::time::timeout(
                Duration::from_millis(500), peer_sock.recv_from(&mut buf),
            )
                .await
                .expect("a throttled OP_ClientUpdate should have been sent within the window")
                .expect("recv_from should succeed on a live loopback socket");
            let d = &buf[..n];
            assert!(d[0] != 0x00, "a raw unreliable app packet must have a non-zero lead byte");
            let opcode = u16::from_le_bytes([d[0], d[1]]);
            if opcode == crate::protocol::OP_CLIENT_UPDATE {
                let payload = &d[2..];
                assert_eq!(payload.len(), 46, "PlayerPositionUpdateClient_Struct is 46 bytes");
                let anim_bits = u32::from_le_bytes(payload[34..38].try_into().unwrap());
                break anim_bits as i32;
            }
            // Anything else (e.g. a stray reliable-protocol byte pattern) isn't what we're after —
            // keep draining until the position update itself shows up or the timeout above fires.
        };

        assert!(anim >= 20 && anim <= 32,
            "running at RUN_SPEED (44 u/s) for a full ~280ms throttle window must report an \
             animation near 28 (running), not the ~1 a per-tick-sliver window (or the old hardcoded \
             boolean) would produce — got {anim}");
    }

    /// **A GOAL THE CLIENT CHANGED MUST NOT BE REPORTED AS THE GOAL THE AGENT ASKED FOR.**
    ///
    /// When the caller's `z` sits below every floor in the goal's column, the planner snaps the goal
    /// onto the real floor. That is a good accommodation — but performing it silently makes it a lie:
    /// an agent that asked for `z: 0` would be told `navigating`, then `arrived`, as though it got
    /// what it requested, having actually been walked to `z: 47`. An accommodation presented as
    /// compliance is exactly the class this PR exists to eliminate, so it is surfaced —
    /// `nav_reason: goal_z_snapped`, all the way through to ARRIVAL, plus the message log.
    #[test]
    fn a_snapped_goal_z_is_reported_not_silently_performed() {
        use eqoxide_nav::planner::PlanReply;
        let g: eqoxide_ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(eqoxide_ipc::GroupSnapshot::default()));
        let mut nav = test_action_loop(g);
        let mut gs = GameState::new();
        let goal = (100.0f32, 100.0f32, 0.0f32); // the agent asked for z = 0

        // The planner routed there — but only by moving the goal onto the floor at z = 47.
        nav.walker.apply_plan(PlanReply {
            gen: 1,
            outcome: eqoxide_nav::collision::PlanOutcome::Route(vec![[0.0, 0.0, 47.0], [100.0, 100.0, 47.0]]),
            start: [0.0; 3], goal: [0.0; 3], trace: Default::default(),
            plan_ms: 5,
            goal_snapped: Some(eqoxide_nav::collision::GoalSnap::ToColumnFloor { z: 47.0 }),
            tight: false,
        }, &mut gs, goal);

        let st = nav.nav.nav_state.lock().unwrap().clone();
        assert_eq!(st.state, "navigating");
        assert_eq!(st.reason.as_deref(), Some("goal_z_snapped"),
            "the agent asked for z=0 and is being walked to z=47 — it must be TOLD its goal was changed");
        assert!(gs.messages.iter().any(|m| m.text.contains("CHANGED your goal")),
            "and it must be said in the message log too, in words");

        // ...and it must survive to ARRIVAL. `arrived` with no reason would tell the agent it got
        // exactly what it asked for, which is the whole lie.
        assert!(nav.walker.goal_snapped, "the snap must be carried to arrival, not forgotten en route");

        // A goal whose z WAS honoured reports nothing — the accommodation must not be cried wolf.
        nav.walker.apply_plan(PlanReply {
            gen: 2,
            outcome: eqoxide_nav::collision::PlanOutcome::Route(vec![[0.0, 0.0, 0.0], [100.0, 100.0, 0.0]]),
            start: [0.0; 3], goal: [0.0; 3], trace: Default::default(),
            plan_ms: 5,
            goal_snapped: None,
            tight: false,
        }, &mut gs, goal);
        let st = nav.nav.nav_state.lock().unwrap().clone();
        assert_eq!(st.reason, None, "a goal that was honoured as given carries no snap reason");
        assert!(!nav.walker.goal_snapped);

        // The WATER variant (design §4d): a submerged goal the walker cannot dive to must carry
        // the same reason channel AND say the water part in words — "arrived" floating at the
        // surface with no qualifier would claim a depth never reached.
        nav.walker.apply_plan(PlanReply {
            gen: 3,
            outcome: eqoxide_nav::collision::PlanOutcome::Route(vec![[0.0, 0.0, -20.0], [100.0, 100.0, -20.0]]),
            start: [0.0; 3], goal: [0.0; 3], trace: Default::default(),
            plan_ms: 5,
            goal_snapped: Some(eqoxide_nav::collision::GoalSnap::ToWaterSurface { surface_z: 0.0 }),
            tight: false,
        }, &mut gs, (100.0, 100.0, -20.0));
        let st = nav.nav.nav_state.lock().unwrap().clone();
        assert_eq!(st.reason.as_deref(), Some("goal_z_snapped"),
            "a submerged goal rides the same goal_z_snapped channel");
        assert!(gs.messages.iter().any(|m| m.text.contains("WATER SURFACE")),
            "and the message log must carry the water qualifier, in words");
        assert!(nav.walker.goal_snapped, "carried to arrival: 'arrived' will bear the qualifier");
    }

    /// **`nav_tier` IS PER-ROUTE AND MUST NOT GO STALE (#378 Phase 2, #343 discipline).** The tier is
    /// the fact for the route being walked RIGHT NOW; it must never survive into a state whose route
    /// it does not describe. Repro the review's finding: journey A commits a `preferred` route →
    /// journey B is unreachable → the `no_path` state must NOT still read `preferred` from A. Also
    /// pins that an ARRIVED and an Exhausted `navigating_partial` state carry no stale tier.
    #[test]
    fn nav_tier_does_not_survive_into_a_later_no_path_or_arrived() {
        use eqoxide_nav::collision::{NoRoute, PlanLimit, PlanOutcome};
        use eqoxide_nav::planner::PlanReply;
        let group: eqoxide_ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(eqoxide_ipc::GroupSnapshot::default()));
        let mut nav = test_action_loop(group);
        let mut gs = GameState::new();
        let goal = (100.0f32, 100.0f32, 0.0f32);

        // Journey A: a committed route at the roomy tier → nav_tier = "preferred".
        nav.walker.apply_plan(PlanReply {
            gen: 1,
            outcome: PlanOutcome::Route(vec![[0.0, 0.0, 0.0], [100.0, 100.0, 0.0]]),
            start: [0.0; 3], goal: [0.0; 3], trace: Default::default(),
            plan_ms: 5, goal_snapped: None, tight: false,
        }, &mut gs, goal);
        assert_eq!(nav.nav.nav_state.lock().unwrap().tier, Some("preferred"),
            "a committed preferred route publishes nav_tier = preferred");

        // Journey B: a definitively unreachable goal → no_path. The tier from A must be GONE.
        nav.walker.apply_plan(PlanReply {
            gen: 2,
            outcome: PlanOutcome::Unreachable {
                reason: NoRoute::SearchClosed, goal_blocked_by: None, frontier_blocked_by: None },
            start: [0.0; 3], goal: [0.0; 3], trace: Default::default(),
            plan_ms: 5, goal_snapped: None, tight: false,
        }, &mut gs, goal);
        let st = nav.nav.nav_state.lock().unwrap().clone();
        assert_eq!(st.state, "no_path");
        assert_eq!(st.tier, None,
            "nav_tier must NOT survive from journey A into journey B's no_path (the #343 stale-field lie)");

        // A fresh minimum-tier route, then an Exhausted partial: the partial is not a confirmed route,
        // so it must carry no tier either.
        nav.walker.apply_plan(PlanReply {
            gen: 3,
            outcome: PlanOutcome::Route(vec![[0.0, 0.0, 0.0], [50.0, 50.0, 0.0]]),
            start: [0.0; 3], goal: [0.0; 3], trace: Default::default(),
            plan_ms: 5, goal_snapped: None, tight: true,
        }, &mut gs, goal);
        assert_eq!(nav.nav.nav_state.lock().unwrap().tier, Some("minimum"));
        nav.walker.apply_plan(PlanReply {
            gen: 4,
            outcome: PlanOutcome::Exhausted {
                limit: PlanLimit::NodeCap,
                progress: Some(vec![[0.0, 0.0, 0.0], [60.0, 60.0, 0.0], [90.0, 90.0, 0.0]]) },
            start: [0.0; 3], goal: [0.0; 3], trace: Default::default(),
            plan_ms: 5, goal_snapped: None, tight: false,
        }, &mut gs, goal);
        let st = nav.nav.nav_state.lock().unwrap().clone();
        assert_eq!(st.state, "navigating_partial");
        assert_eq!(st.tier, None, "an Exhausted partial walk is not a confirmed route — it carries no tier");

        // And an arrived state (reached via set_nav_state) after a committed route carries no stale tier.
        nav.walker.apply_plan(PlanReply {
            gen: 5,
            outcome: PlanOutcome::Route(vec![[0.0, 0.0, 0.0], [100.0, 100.0, 0.0]]),
            start: [0.0; 3], goal: [0.0; 3], trace: Default::default(),
            plan_ms: 5, goal_snapped: None, tight: false,
        }, &mut gs, goal);
        assert_eq!(nav.nav.nav_state.lock().unwrap().tier, Some("preferred"));
        nav.walker.set_nav_state("arrived");
        assert_eq!(nav.nav.nav_state.lock().unwrap().tier, None,
            "arrival ends the route — its tier must not linger");
    }

    /// Build a minimal ActionLoop for unit tests that only exercise a single `sync_*`/tick method —
    /// every other shared slot gets an empty/default placeholder.
    /// #643: `sync_entities` is the ONLY publisher of the agent-facing world tables, so if it does
    /// not carry pose/gait, `/v1/observe/entities?labeled=1` can only ever report nothing. This
    /// pins the publisher half of the net→HTTP path (the HTTP half is pinned by the app crate's
    /// `tests/entity_pose_643.rs`). It also pins the SIGN of the gait field: `animation` is
    /// `signed animation:10` on the wire, so a mob backing up carries a negative gait — reporting
    /// the raw unsigned bits would turn -12 into a confident, wrong 1012.
    #[test]
    fn sync_entities_publishes_pose_and_gait_643() {
        use eqoxide_core::game_state::{Gait, Pose};
        let mut gs = GameState::new();
        let mut sitter = eqoxide_core::game_state::make_entity(1, "a_sitter", 1.0, 2.0, 3.0, true);
        sitter.pose = Pose::Sitting;
        sitter.gait = Some(Gait::from_wire_10bit(12));
        gs.upsert_entity(sitter);
        let mut backer = eqoxide_core::game_state::make_entity(2, "a_backpedaller", 4.0, 5.0, 6.0, true);
        backer.gait = Some(Gait::from_wire_10bit(1012)); // 10-bit two's complement for -12
        gs.upsert_entity(backer);
        let mut weird = eqoxide_core::game_state::make_entity(3, "a_weirdo", 7.0, 8.0, 9.0, true);
        weird.pose = Pose::from_wire(199);
        gs.upsert_entity(weird);

        let group: eqoxide_ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(eqoxide_ipc::GroupSnapshot::default()));
        let nav = test_action_loop(group);
        nav.sync_entities(&gs);

        let poses = nav.world.entity_poses();
        assert_eq!(poses["a_sitter"].pose, "sitting");
        assert_eq!(poses["a_sitter"].gait, Some(12));
        assert_eq!(poses["a_backpedaller"].pose, "standing");
        assert_eq!(poses["a_backpedaller"].gait, Some(-12),
            "a backing-up mob's gait is NEGATIVE (signed 10-bit); the raw bits 1012 would be a lie");
        assert_eq!(poses["a_weirdo"].pose, "unknown(199)",
            "an unrecognised pose code reaches the agent as unknown, not as a plausible default");
        assert_eq!(poses["a_weirdo"].gait, None,
            "no position update yet => null ('not reported'), not 0 ('stationary')");
    }

    fn test_action_loop(group: eqoxide_ipc::GroupShared) -> ActionLoop {
        ActionLoop::new(
            eqoxide_ipc::NavSlots {
                nav_state: std::sync::Arc::new(std::sync::Mutex::new(eqoxide_ipc::NavStatus::default())),
                ..Default::default()
            },
            Default::default(), // world
            Default::default(), // quest
            eqoxide_ipc::GroupSlots { group, ..Default::default() },
            Default::default(), // command (CommandState)
            Default::default(), // social
            Default::default(), // merchant_slots
            Default::default(), // inventory_slots
            Default::default(), // interact
            Default::default(), // chat
            Default::default(), // controller
            Default::default(), // guild_slots
            Default::default(), // collision
            std::path::PathBuf::new(), // maps_dir
            Default::default(), // nav_debug (#608)
            std::sync::Arc::new(std::sync::Mutex::new(
                eqoxide_nav::zone_assets::ZoneAssetState::Idle)), // zone_assets (#600)
        )
    }

    /// **#600 (review round 2): `drain_zone_cross` is the THIRD world-answering consumer, and it must
    /// route its collision-derived refusal through the ONE decision function too.** It reads the SAME
    /// shared collision grid and publishes an agent-observable `nav_state`. Before this fix, while the
    /// zone's assets were not usable (`collision == None` for the whole ~10s load, a failed load, or
    /// the previous zone's grid in the ~1-frame stale window) it published `no_path`
    /// ("DEFINITIVE: no route exists, do not retry") — telling an agent to give up permanently on a
    /// crossing that is reachable the moment the world finishes loading. That is a worse lie than the
    /// walker case (which never claimed definitiveness).
    ///
    /// This drives the REAL `drain_zone_cross` (shared handles, real-driver style) across the not-ready
    /// states — loading (`Pending`), the stale window (`Ready` for the PREVIOUS zone), and a failed
    /// load — and asserts each answers the honest transient `zone_loading` (never `no_path`) and
    /// re-queues the one-shot request. It also pins that once the zone IS usable, a genuine no-region
    /// result is STILL the truthful `no_path` — the gate suppresses the lie, not legitimate refusals.
    ///
    /// **Mutation check:** revert the `drain_zone_cross` gate (delete the `usability` guard, run the
    /// resolution unconditionally) → every not-ready case publishes `no_path` and the
    /// `state == "zone_loading"` assertions go RED. A test that passes both ways pins nothing.
    #[tokio::test]
    async fn drain_zone_cross_routes_its_refusal_through_usability_never_a_definitive_no_path() {
        use eqoxide_nav::zone_assets::{self, ZoneAssetState};
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        const WANT: u16 = 30;
        // A real flat-floor `Ready` grid via the test fixture — used for the stale and usable cases
        // where a grid must be present.
        //
        // #815: the region map is an all-dry `flat_below(-1000.0)` — one that genuinely LOADED and
        // carries no zone-line region — NOT the reason-free `test_ready()` this used to call. That
        // fixture leaves the grid at `RegionDataAbsent::NotAttached`, so the `usable_no_region` case
        // below was asserting `zone_line_not_in_map` over a grid whose region map was never read:
        // it pinned the very substitution #815 is about. The case is meant to be "the map loaded
        // and the region isn't in it", and now is.
        let grid = ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::flat_below(-1000.0),
        ))).collision().unwrap().clone();

        // (label, player_zone, expect_zone_loading). Collision + zone_assets are set per label below.
        for &(label, player_zone, want_loading) in &[
            ("loading",          "freporte", true),  // Pending, collision None — the ~10s load window
            ("stale",            "qeynos",   true),  // Ready for the PREVIOUS zone (freporte) — ~1-frame window
            ("failed",           "freporte", true),  // the load failed
            ("usable_no_region", "freporte", false), // usable ⇒ a genuine, TRUTHFUL no_path is still allowed
        ] {
            let mut al = test_action_loop(Default::default());
            // Advertise a zone line for WANT so, ABSENT the gate, resolution reaches a COLLISION-derived
            // decision (region lookup) rather than short-circuiting on "no zone point advertised".
            *al.world.zone_points.lock().unwrap() = vec![zp_at(7, WANT, [10.0, 10.0, 0.0])];
            match label {
                "loading" => zone_assets::begin_zone_load(&al.collision, &al.zone_assets, "freporte", "loading…"),
                "failed"  => zone_assets::finish_zone_load(&al.collision, &al.zone_assets, "freporte", None, 0, Some("boom")),
                // stale + usable both need a present grid; only the player_zone differs.
                _ => zone_assets::finish_zone_load(&al.collision, &al.zone_assets, "freporte", Some(grid.clone()), 1, None),
            }

            let mut gs = eqoxide_core::game_state::GameState::new();
            gs.world.zone_name = player_zone.into();
            gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0;

            al.command.request_zone_cross(WANT);
            al.drain_zone_cross(&mut stream, &mut gs);

            let s = al.nav.nav_state.lock().unwrap().clone();
            // PEEK, don't drain (#725): draining here would take out a `ZoneCrossTicket` whose drop
            // publishes the unhandled-request fallback over the very state this test asserts on.
            let requeued = al.command.peek_zone_cross().is_some();
            if want_loading {
                assert_eq!(s.state, "zone_loading",
                    "{label}: a zone_cross while assets aren't usable must be the honest transient \
                     zone_loading, NEVER the definitive no_path (#600)");
                assert!(requeued,
                    "{label}: the one-shot request must be re-queued so it resolves once assets land");
            } else {
                // Usable + a LOADED region map with no matching region ⇒ a TRUTHFUL definitive no.
                assert_eq!(s.state, "no_path",
                    "{label}: once the zone IS usable, a genuine no-region result is a truthful no_path — \
                     the gate must suppress the load-window LIE, not legitimate refusals");
                assert_eq!(s.reason.as_deref(), Some("zone_line_not_in_map"));
                assert!(!requeued, "{label}: a usable, resolved attempt consumes the request");
            }
        }
    }

    /// **#815: `zone_line_not_in_map` is a claim about the CONTENTS of a file, so it may only be
    /// said about a file that was read.**
    ///
    /// `docs/http-api.md` documents that reason as "the locally **loaded** zone geometry has no
    /// matching WLD zone-line (DRNTP) trigger region for it — a client-side `.wtr` map-data gap".
    /// `POST /v1/move/zone_cross` also emitted it when the region map did not load AT ALL, where the
    /// client has read nothing and knows nothing about whether such a region exists. The two states
    /// call for opposite operator action — "this exit isn't baked, route around it" versus "every
    /// exit in this zone is unknown, re-sync the assets" — and no other field recovered the
    /// difference. Same substitution #803 killed on `/v1/observe/zone_exits`, one caller over.
    ///
    /// **Both directions, and the pair is the point.** A fix that made *everything* report the new
    /// reason would just move the lie, so the loaded-and-genuinely-absent row asserts the old reason
    /// is still emitted, and the final assertion pins that no two of the three reasons collide. The
    /// per-cause strings are `RegionDataAbsent::as_str()` — the same vocabulary `/zone_exits`
    /// refuses with, and the same shape as this function's own `zone_loading` refusal
    /// (`NotUsable::as_str()`), so one question has one answer across the surfaces.
    ///
    /// Every row is `Ready` for the player's own zone, so the #600 usability gate passes and the
    /// resolution really does reach the region lookup — otherwise this would be testing the gate.
    ///
    /// **Mutation checks, all at the call site rather than inside a callee's body** (a body-wrap
    /// cannot tell "this branch is dead" from "the condition is false"). Results in the PR:
    ///   * M1 — the emit arm publishes `Some("zone_line_not_in_map")` instead of
    ///     `Some(absent.as_str())`: the two failed-load rows go RED on their reason assertion.
    ///   * M2 — the whole pre-fix shape restored (`match located`, one `None` arm, the
    ///     `unwrap_or_default()` back): the same two rows go RED.
    ///   * M3 — the FIXTURE, not the production code: `loaded_lacks_region` is switched to a grid
    ///     with no region data attached. It goes RED, which is what proves that row is passing
    ///     because its region map genuinely loaded and not merely because nothing was found.
    ///     Without M3 the pair could both be satisfied by a test that never had a loaded map.
    ///
    /// **NOT verified by a live run**, and not claimed: constructing a truncated `.wtr` against a
    /// live server is not practical, so the failed-load rows are fixture-driven only.
    #[tokio::test]
    async fn zone_cross_reports_an_unread_region_map_as_such_never_as_a_map_data_gap() {
        use eqoxide_core::region_map::{RegionLoadError, RegionMap};
        use eqoxide_nav::zone_assets::{self, ZoneAssetState};
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        const WANT: u16 = 30;
        const ZONE: &str = "freporte";

        // (label, the grid's region-data state, the ONLY reason that is true of it)
        let cases: Vec<(&str, ZoneAssetState, &str)> = vec![
            // The map LOADED (an all-dry BSP, no zone-line leaves) and has no region for WANT.
            // This is the one state `zone_line_not_in_map` describes, and it must keep saying so.
            ("loaded_lacks_region",
             ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
                 RegionMap::flat_below(-1000.0)))),
             "zone_line_not_in_map"),
            // The `.wtr` was consulted and did not load. The client has NOT looked at any region
            // list, so it may not report the absence of a region.
            ("truncated",
             ZoneAssetState::test_ready_with_region_data(
                 Err(RegionLoadError::Truncated { declared_nodes: 4096, bytes: 40 })),
             "region_data_truncated"),
            // A second failed-load cause, to pin that the reason is per-CAUSE and not one
            // collapsed "something went wrong" string: re-syncing an asset pack and re-baking a
            // corrupt one are different operator actions.
            ("missing",
             ZoneAssetState::test_ready_with_region_data(Err(RegionLoadError::Missing)),
             "region_data_missing"),
        ];

        let mut seen: Vec<(&str, String)> = Vec::new();
        for (label, state, want_reason) in &cases {
            let mut al = test_action_loop(Default::default());
            // Advertise a zone line for WANT so resolution reaches the COLLISION-derived decision
            // (the region lookup) instead of short-circuiting on "no zone point advertised".
            *al.world.zone_points.lock().unwrap() = vec![zp_at(7, WANT, [10.0, 10.0, 0.0])];
            // `Ready` for the player's zone ⇒ the #600 gate passes. The grid is present in BOTH
            // slots, exactly as `finish_zone_load` writes them in production.
            zone_assets::finish_zone_load(
                &al.collision, &al.zone_assets, ZONE,
                Some(state.collision().expect("every row is a Ready fixture").clone()), 1, None);

            let mut gs = eqoxide_core::game_state::GameState::new();
            gs.world.zone_name = ZONE.into();
            gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0;

            al.command.request_zone_cross(WANT);
            al.drain_zone_cross(&mut stream, &mut gs);

            let s = al.nav.nav_state.lock().unwrap().clone();
            assert_eq!(s.state, "no_path",
                "{label}: the zone is usable and the request is resolved, so this is terminal for \
                 this zone either way — #815 changes the REASON, not the state");
            assert_eq!(s.reason.as_deref(), Some(*want_reason),
                "{label}: the reason must name the cause the client actually observed. \
                 `zone_line_not_in_map` is documented as a map-data GAP in a map that LOADED, so a \
                 failed load may not borrow it (#815).");
            seen.push((*label, s.reason.clone().expect("a terminal no_path always carries a reason")));
        }

        // The whole point of the fix: these are not the same string. Asserted over the observed
        // values rather than the expectations above, so a fix that collapsed two causes at the emit
        // site cannot pass by having also collapsed them in this table.
        for i in 0..seen.len() {
            for j in (i + 1)..seen.len() {
                assert_ne!(seen[i].1, seen[j].1,
                    "{} and {} must be distinguishable from `nav_reason` alone — an agent has no \
                     second channel to tell a map-data gap from an unread map (#815)",
                    seen[i].0, seen[j].0);
            }
        }
    }

    /// **#827: `zone_cross` answers out of the grid its OWN gate blessed, not out of a second slot
    /// that may have been emptied since.**
    ///
    /// The bug: the gate locked the zone-ASSET state (`zone_assets::usability`) and, if usable, fell
    /// through; the region lookup then read `self.collision`, a SEPARATE shared slot.
    /// `begin_zone_load` empties that slot BEFORE it publishes `Pending`, so a zone change landing
    /// between the two reads paired a usable verdict with an emptied slot. The lookup was then
    /// `None` for want of a grid, and the `(None, None)` arm published
    /// `no_path` / `zone_line_not_in_map` — documented as "the region map **loaded** and has no
    /// matching WLD zone-line (DRNTP) region", i.e. a claim about the contents of a map that was
    /// never opened. #829's reviewer built that end state directly and got exactly that string
    /// (issue #827, comment 1).
    ///
    /// The fix takes verdict AND grid from one `usable_collision` call — the `Arc<Collision>` the
    /// `Ready` state owns — as `/v1/observe/zone_exits` has since #803/#821. **The rows below are
    /// therefore constructions the SHAPE now rules out, not merely ones the code handles**: with the
    /// grid coming from the verdict, there is no optional grid left to be absent here.
    ///
    /// **Both halves of the acceptance bar, and the second is the one that keeps the fix honest:**
    /// an emptied slot must NOT produce `zone_line_not_in_map` (rows 1–2), *and* a genuinely loaded
    /// region map that really lacks the region must still produce it (rows 3–4) — otherwise a fix
    /// that simply stopped ever emitting the string would pass.
    ///
    /// **Which rows discriminate #827, and which deliberately do not.** Rows 1–2 are the
    /// discriminators, measured: under the M1 mutation below (grid re-read from `self.collision`
    /// while the verdict still comes from `usable_collision`) row 1 fails, and with row 1 removed
    /// row 2 fails with the acceptance sentence verbatim. Row 4 looks like the sharpest of the four
    /// — emptied slot, and still `zone_line_not_in_map` — but it is **blind to #827**, and saying so
    /// here is the point: the #840 review measured row 4 GREEN on its own under M1
    /// (`1 passed; 0 failed; 382 filtered out`). The pre-fix bug produced this row's expected answer
    /// too, because an emptied slot left `located` and `region_absent` both `None` and the
    /// `(None, None)` arm published that same string from no map at all. Row 4 therefore cannot
    /// attribute its outcome to the grid it was answered from, and no sentence here should.
    /// What rows 3–4 do pin is the opposite failure: they assert the reason is still *produced*, so
    /// a "fix" that suppressed `zone_line_not_in_map` rather than re-sourcing it fails here. (That
    /// is what the rows assert, read directly; no suppression mutation was run.) The fix removes
    /// the falsehood, not the reason.
    ///
    /// **Mutation checks — all at the CALL SITE in `resolve_zone_cross`, never inside a callee**
    /// (a body-wrap cannot tell "this branch is dead" from "the predicate is false"). Results are
    /// tabulated in the PR body; survivors are reported there, not omitted.
    ///
    /// **NOT verified by a live run, and not claimed:** an emptied collision slot under a usable
    /// verdict is a one-call window that this test reaches by construction, exactly as #829's
    /// reviewer did. Whether a real zone change interleaves that way was never measured — before
    /// this change or after it.
    #[tokio::test]
    async fn zone_cross_answers_from_the_grid_its_own_gate_blessed_827() {
        use eqoxide_core::region_map::{RegionLoadError, RegionMap};
        use eqoxide_nav::zone_assets::{self, ZoneAssetState};
        const WANT: u16 = 30;
        const IDX: i32 = 7;
        const ZONE: &str = "testfixture";

        /// What the caller must be told. `Walks` = a concrete zone line was resolved and a goto
        /// issued; `Refused` = a terminal `no_path` carrying exactly this reason.
        enum Want { Walks, Refused(&'static str) }

        // (label, the Ready state's grid, empty the collision slot after publishing?, expectation)
        let cases: Vec<(&str, ZoneAssetState, bool, Want)> = vec![
            // 1. The blessed grid HAS the requested zone line. Pre-fix the emptied slot hid it and
            //    the client claimed the map lacks it; post-fix the cross resolves and walks.
            ("emptied_slot_region_present",
             ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
                 RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, IDX)))),
             true, Want::Walks),
            // 2. The blessed grid's region data FAILED to load. The honest answer is #815's
            //    per-cause reason; pre-fix the emptied slot substituted the map-data-gap string.
            //    This row is the acceptance sentence verbatim.
            ("emptied_slot_region_data_failed",
             ZoneAssetState::test_ready_with_region_data(Err(RegionLoadError::Missing)),
             true, Want::Refused("region_data_missing")),
            // 3. THE PAIR'S SECOND HALF, in production wiring (both slots written, nothing
            //    emptied): an all-dry BSP that genuinely loaded and carries no zone-line region.
            //    `zone_line_not_in_map` is true of it and must still be said.
            ("loaded_lacks_region_both_slots",
             ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
                 RegionMap::flat_below(-1000.0)))),
             false, Want::Refused("zone_line_not_in_map")),
            // 4. The same loaded-and-genuinely-absent map, with the slot emptied underneath it.
            //    The emptied slot must now change NOTHING: the answer comes from the blessed grid,
            //    which was read, so the map-data-gap reason is truthful here. Pins that the fix
            //    routes the answer to the right grid rather than suppressing the reason.
            ("emptied_slot_loaded_lacks_region",
             ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
                 RegionMap::flat_below(-1000.0)))),
             true, Want::Refused("zone_line_not_in_map")),
        ];

        for (label, state, empty_slot, want) in &cases {
            let (mut al, nav, command, collision, za) = shared_nav_action_loop();
            let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
            // Advertise a zone point for WANT so resolution reaches the COLLISION-derived decision
            // (the region lookup) instead of short-circuiting on "no zone point advertised".
            *al.world.zone_points.lock().unwrap() = vec![zp_at(IDX as u32, WANT, [100.0, 200.0, 0.0])];
            // Publish BOTH slots exactly as production does, then — for the #827 rows — empty ONLY
            // the collision slot, which is the state `begin_zone_load` leaves behind between its two
            // writes. The zone-asset verdict is untouched and genuinely usable, so the gate passes
            // on its own terms: honour the premise, break the conclusion.
            let grid = state.collision().expect("every row is a Ready fixture").clone();
            zone_assets::finish_zone_load(&collision, &za, ZONE, Some(grid), 1, None);
            if *empty_slot { *collision.write().unwrap() = None; }
            // PREMISE, asserted rather than assumed: the gate really does say "usable" here, so a
            // row that refuses is refusing at the region lookup and not at the #600 gate.
            assert!(zone_assets::usability(&zone_assets::lock_state(&za), ZONE).is_none(),
                "{label}: premise — the zone-asset verdict must be USABLE, or this tests the gate");

            let mut gs = GameState::new();
            gs.world.zone_name = ZONE.into();
            gs.player_x = 30.0; gs.player_y = 0.0; gs.player_z = 0.0;

            command.request_zone_cross(WANT);
            al.drain_zone_cross(&mut stream, &mut gs);

            let s = nav.nav_state.lock().unwrap().clone();
            match want {
                Want::Walks => {
                    assert!(nav.goto_target.lock().unwrap().is_some(),
                        "{label}: the blessed grid carries the requested zone line, so the cross \
                         must WALK to it — an emptied second slot is not the client's answer (#827)");
                    assert_ne!(s.state, "no_path",
                        "{label}: publishing no_path here would be the #827 falsehood, reported \
                         over a map that does contain the line");
                }
                Want::Refused(reason) => {
                    assert_eq!(s.state, "no_path",
                        "{label}: the zone is usable and the request is resolved, so this is \
                         terminal for this zone");
                    assert_eq!(s.reason.as_deref(), Some(*reason),
                        "{label}: the reason must name the cause the client actually observed — \
                         `zone_line_not_in_map` is a claim about a map that was READ (#827/#815)");
                }
            }
        }
    }

    /// Build an `ActionLoop` whose `command` facade SHARES the walker's `NavSlots` — exactly as
    /// `main.rs` wires production (both are `.clone()`s of the one bundle). So a `request_zone_cross`/
    /// `request_stop` on the returned `CommandState` writes the SAME `nav.zone_cross` slot that
    /// `drain_zone_cross` drains and `Walker::resolve_goal` reads. (The default `test_action_loop`
    /// gives `command` its OWN nav, which is fine for slot round-trips but cannot exercise the
    /// cross-slot coupling this test needs.) Returns the loop + the shared handles the test drives.
    fn shared_nav_action_loop() -> (
        ActionLoop, eqoxide_ipc::NavSlots, eqoxide_command::CommandState,
        eqoxide_nav::collision::SharedCollision, eqoxide_nav::zone_assets::ZoneAssetStateShared,
    ) {
        let nav = eqoxide_ipc::NavSlots {
            nav_state: std::sync::Arc::new(std::sync::Mutex::new(eqoxide_ipc::NavStatus::default())),
            ..Default::default()
        };
        let command = eqoxide_command::CommandState::new(
            Default::default(), Default::default(), Default::default(), Default::default(),
            Default::default(), Default::default(), Default::default(), Default::default(),
            Default::default(), Default::default(), nav.clone(), Default::default(),
        );
        let collision: eqoxide_nav::collision::SharedCollision = Default::default();
        let zone_assets: eqoxide_nav::zone_assets::ZoneAssetStateShared =
            std::sync::Arc::new(std::sync::Mutex::new(eqoxide_nav::zone_assets::ZoneAssetState::Idle));
        let al = ActionLoop::new(
            nav.clone(), Default::default(), Default::default(), Default::default(),
            command.clone(), Default::default(), Default::default(), Default::default(),
            Default::default(), Default::default(), Default::default(), Default::default(),
            collision.clone(), std::path::PathBuf::new(), Default::default(), zone_assets.clone(),
        );
        (al, nav, command, collision, zone_assets)
    }

    /// **#600 review round 3: a `/zone_cross` queued during an asset load is CANCELLABLE by `/stop`,
    /// and never leaks.** Drives the REAL `request_stop` (not a manual slot null) through the whole
    /// chain: `drain_zone_cross` re-queues the cross during the load and publishes `zone_loading`;
    /// `resolve_goal` holds `zone_loading` while it is queued; `request_stop` clears the queued cross;
    /// `resolve_goal` then retires `zone_loading`→`idle`; and after the assets land the cross does NOT
    /// fire (no crossing attempt). A no-`/stop` positive control proves the cross WOULD otherwise still
    /// be live post-load — so `/stop` is genuinely the differentiator.
    ///
    /// **Mutation check:** delete `*self.nav.zone_cross.lock().unwrap() = None;` from
    /// `CommandState::request_stop` → after `/stop` the cross stays queued, so (a) `resolve_goal` keeps
    /// `zone_loading` instead of retiring to `idle` AND (b) the post-load drain fires the cross — both
    /// asserted here, so this goes RED.
    #[tokio::test]
    async fn zone_cross_queued_during_load_is_cancellable_by_stop_and_never_leaks() {
        use eqoxide_nav::zone_assets;
        const WANT: u16 = 30;

        // ── The fix: /stop cancels the queued-during-load cross ───────────────────────────────────
        let (mut al, nav, command, collision, za) = shared_nav_action_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        *al.world.zone_points.lock().unwrap() = vec![zp_at(7, WANT, [10.0, 10.0, 0.0])];
        zone_assets::begin_zone_load(&collision, &za, "freporte", "loading…"); // assets loading, collision None
        let mut gs = eqoxide_core::game_state::GameState::new();
        gs.world.zone_name = "freporte".into();

        command.request_zone_cross(WANT);
        al.drain_zone_cross(&mut stream, &mut gs);
        assert_eq!(nav.nav_state.lock().unwrap().state, "zone_loading", "loading: honest transient state");
        assert_eq!(*nav.zone_cross.lock().unwrap(), Some(WANT), "the cross was re-queued to resolve post-load");

        // resolve_goal (no concrete goto goal yet) HOLDS zone_loading while the cross is queued.
        assert!(al.walker.resolve_goal(&gs).is_none());
        assert_eq!(nav.nav_state.lock().unwrap().state, "zone_loading",
            "a queued cross holds zone_loading, not a misleading idle");

        // The REAL /stop: it must cancel the queued cross.
        command.request_stop();
        assert!(nav.zone_cross.lock().unwrap().is_none(), "/stop must clear the queued cross (#600 rd3)");

        // Now resolve_goal retires zone_loading → idle honestly (the cross is genuinely cancelled).
        assert!(al.walker.resolve_goal(&gs).is_none());
        assert_eq!(nav.nav_state.lock().unwrap().state, "idle",
            "after /stop the cross is gone, so zone_loading retires to idle");

        // Assets land: the cancelled cross must NOT fire. drain finds an empty slot and does nothing.
        zone_assets::finish_zone_load(&collision, &za,
            "freporte", Some(zone_assets::ZoneAssetState::test_ready().collision().unwrap().clone()), 1, None);
        al.drain_zone_cross(&mut stream, &mut gs);
        assert!(nav.goto_target.lock().unwrap().is_none(),
            "the cancelled cross must not fire a crossing attempt after the load completes");
        assert_eq!(nav.nav_state.lock().unwrap().state, "idle", "still idle — no post-/stop crossing");

        // ── Positive control: WITHOUT /stop the cross is still live post-load (so /stop mattered) ──
        let (mut al2, nav2, command2, collision2, za2) = shared_nav_action_loop();
        *al2.world.zone_points.lock().unwrap() = vec![zp_at(7, WANT, [10.0, 10.0, 0.0])];
        zone_assets::begin_zone_load(&collision2, &za2, "freporte", "loading…");
        let mut gs2 = eqoxide_core::game_state::GameState::new();
        gs2.world.zone_name = "freporte".into();
        command2.request_zone_cross(WANT);
        al2.drain_zone_cross(&mut stream, &mut gs2);
        assert_eq!(nav2.nav_state.lock().unwrap().state, "zone_loading");
        // No /stop this time. Assets land; the still-queued cross RESOLVES (flat grid ⇒ no region ⇒ a
        // truthful no_path — the point is it ATTEMPTED the cross, unlike the cancelled case above).
        zone_assets::finish_zone_load(&collision2, &za2,
            "freporte", Some(zone_assets::ZoneAssetState::test_ready().collision().unwrap().clone()), 1, None);
        al2.drain_zone_cross(&mut stream, &mut gs2);
        assert_eq!(nav2.nav_state.lock().unwrap().state, "no_path",
            "without /stop the re-queued cross is still live and resolves after load — /stop is the differentiator");
        assert!(nav2.zone_cross.lock().unwrap().is_none(), "a usable resolution consumes the cross");
    }

    /// **#725: `/v1/move/zone_cross` had a silent dead band, and this sweeps for one anywhere.**
    ///
    /// The shipped code took a shortcut when the located zone line was within 15.0 u —
    /// `if d2 <= 15.0*15.0 { tracing::info!(…) }`, a branch whose entire body was that log line —
    /// on the belief that the standing auto-cross would fire instead. The auto-cross gates on
    /// `zone_line_at_standing` (the capsule must be physically INSIDE the DRNTP region), so from
    /// outside the region but within 15 u the request was consumed and NOTHING happened: no goto, no
    /// packet, no state, no message. Live measurements: 11.87 u / 12.90 u / 14.66 u dropped every
    /// time; 19.54 u / 31.57 u crossed in under a second.
    ///
    /// This drives the REAL `drain_zone_cross` against a real collision grid carrying a real DRNTP
    /// region, and asserts the invariant that makes a band impossible rather than merely absent at
    /// the five distances that were measured: **from any position outside the trigger region, an
    /// accepted cross produces a walk to that region.** The sweep steps 0.25 u from just outside the
    /// footprint out to 40 u — 140 positions straddling the old threshold in both directions — plus
    /// the five live distances by name, so a re-tuned threshold cannot pass this either.
    ///
    /// **Mutation check:** restore the `d2 <= ZONE_LINE_DIST2` shortcut → every sample inside the
    /// band fails the `goto_target` assertion (RED, ~40 of the 140 sweep points and 3 of the 5
    /// named live cases). Change the threshold to any other positive value → still RED, just at
    /// different samples; only removing the distance test passes.
    #[tokio::test]
    async fn a_zone_cross_from_outside_the_line_region_always_walks_no_silent_band_725() {
        use eqoxide_nav::zone_assets;
        const DEST: u16 = 30;
        const IDX: i32 = 7;

        // The five distances measured live (the three that dropped, and the two that crossed), then
        // a fine sweep across and well past the old 15.0 u threshold.
        let mut cases: Vec<f32> = vec![11.87, 12.90, 14.66, 19.54, 31.57];
        let mut d = 6.0f32;
        while d <= 40.0 { cases.push(d); d += 0.25; }

        for east in cases {
            let (mut al, nav, command, collision, za) = shared_nav_action_loop();
            let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
            *al.world.zone_points.lock().unwrap() = vec![zp_at(IDX as u32, DEST, [100.0, 200.0, 0.0])];
            // A DRNTP trigger box on the flat test floor: north/east ∈ [-4, 4], z ∈ [-2, 2]. Its
            // floor-projected representative point is inside the region (`zone_line_floor_point`
            // re-verifies that), which is exactly why walking to it is unconditionally correct.
            let grid = zone_assets::ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
                eqoxide_core::region_map::RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, IDX),
            ))).collision().unwrap().clone();
            zone_assets::finish_zone_load(&collision, &za, "testfixture", Some(grid), 1, None);

            let mut gs = GameState::new();
            gs.world.zone_name = "testfixture".into();
            gs.player_x = east; gs.player_y = 0.0; gs.player_z = 0.0;

            // PREMISE, asserted rather than assumed: the character is genuinely OUTSIDE the trigger
            // volume at this position, so the standing auto-cross cannot fire and the request has
            // nothing to delegate to. (This is the assumption the shipped shortcut got wrong.)
            let (line_at, dist) = {
                let guard = collision.read().unwrap();
                let c = guard.as_ref().unwrap();
                assert!(c.zone_line_at_standing([gs.player_x, gs.player_y, gs.player_z]).is_none(),
                    "east={east}: premise — the sweep must stand OUTSIDE the region");
                let (_, p) = c.find_zone_line_near(Some(IDX), [gs.player_x, gs.player_y, gs.player_z])
                    .expect("the fixture bakes exactly one zone-line region");
                (p, (p[0] - gs.player_x).hypot(p[1] - gs.player_y))
            };

            let goal_id = command.request_zone_cross(DEST);
            al.drain_zone_cross(&mut stream, &mut gs);

            // The fix: a walk was ISSUED, to the region the cross resolved to.
            assert_eq!(*nav.goto_target.lock().unwrap(), Some((line_at[0], line_at[1], line_at[2])),
                "#725: standing {dist:.2}u from the zone line, an accepted cross must WALK there — \
                 not vanish because a distance gate assumed something else would handle it");

            // ...and it is OBSERVABLE on both channels an agent actually reads.
            let s = nav.nav_state.lock().unwrap().clone();
            assert!(s.goal_id > goal_id,
                "the resolved walk stamps its own fresh goal id (so a read can correlate it)");
            assert_eq!(s.goal, Some(line_at),
                "nav_goal must carry the concrete zone-line destination once it is resolved");
            assert!(gs.messages.iter().any(|m| m.text.contains("Walking to the zone")),
                "and the message log must say so — `tracing` output is invisible to an HTTP agent, \
                 which is what made the dropped request undebuggable");
        }
    }

    /// **#713 item 1 (the #683 review's F3): the standing auto-cross is BOUNDED, and its terminal
    /// state is OBSERVABLE — end to end, through the real `drain_zone_cross`.**
    ///
    /// The regression: a character left standing in a zone-line region the server will not zone
    /// re-sent `OP_ZoneChange` every 10 s cooldown, forever, and each attempt is a server-side
    /// anomaly event. Nothing ever said it was hopeless, so an agent polling the client saw a
    /// "crossing" that was neither progressing nor failing.
    ///
    /// The universal here — *the client cannot exceed `MAX_CROSS_ATTEMPTS` per continuous stand* —
    /// is discharged as a property in `eqoxide_core::zone_cross`
    /// (`auto_cross_attempts_are_bounded_per_stand_713`, over all 9841 tick sequences). THIS test is
    /// the specific-regression example: it drives the real auto-cross with a real collision grid and
    /// a real DRNTP region and pins the four things the property cannot see — that the packets stop,
    /// that the message log says they stopped, and that `GameState` carries the terminal fact for
    /// the HTTP layer to publish. Walking off the line to re-arm is pinned by
    /// `the_escape_hatch_works_on_the_tick_you_step_off_713` instead — see there for why it cannot
    /// live under this test's cooldown-rewinding fixture.
    ///
    /// **Mutation checks** (each reverts one part of the #713 change):
    /// * delete the `blocks()` guard in `drain_zone_cross` → the attempt-count assertion goes RED
    ///   (crossings keep firing every tick);
    /// * delete the `gs.log_msg("zone", "Stopped trying …")` → the "told once" assertion goes RED;
    /// * delete `gs.zone_cross_attempts = None` from the off-region branch → RED in
    ///   `the_escape_hatch_works_on_the_tick_you_step_off_713`, not here (a character that walks
    ///   away and comes back can never cross again);
    /// * count attempts in the `Ignore`/same-zone arms too (i.e. make `cross_unresolved` return
    ///   `true` unconditionally) → nothing here goes red, which is why the gated case is pinned by
    ///   `cross_unresolved`'s own return value rather than by this test alone.
    #[tokio::test]
    async fn the_standing_auto_cross_stops_after_the_bound_and_says_so_713() {
        use eqoxide_core::zone_cross::MAX_CROSS_ATTEMPTS;
        use eqoxide_nav::zone_assets;
        const DEST: u16 = 30;
        const IDX: i32 = 7;
        const HERE: u16 = 54;

        let (mut al, _nav, _command, collision, za) = shared_nav_action_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        // Advertised points exist and are CROSS-zone only, so `classify_unresolved_cross` opens the
        // server-resolved fallback — but none of them carries region index IDX, so the region the
        // character stands in is UNRESOLVED. That is the #683 shape, and the shape that refires.
        *al.world.zone_points.lock().unwrap() = vec![zp_at(99, DEST, [100.0, 200.0, 0.0])];
        let grid = zone_assets::ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, IDX),
        ))).collision().unwrap().clone();
        zone_assets::finish_zone_load(&collision, &za, "testfixture", Some(grid), 1, None);

        let mut gs = GameState::new();
        gs.world.zone_name = "testfixture".into();
        gs.world.zone_id = HERE;
        gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0;
        // PREMISE, asserted rather than assumed: the character really is standing in the region, so
        // the auto-cross has something to fire at every tick.
        assert_eq!(collision.read().unwrap().as_ref().unwrap()
                       .zone_line_at_standing([gs.player_x, gs.player_y, gs.player_z]), Some(IDX),
            "premise: the fixture must put the character INSIDE the DRNTP region");

        // Tick the auto-cross far more times than the bound, expiring the cooldown each time (the
        // cooldown is the only other thing that limits the refire, and it is what makes the
        // unbounded case one attempt per 10s rather than one per frame).
        let attempts = |gs: &GameState| gs.messages.iter()
            .filter(|m| m.text.contains("Crossing zone line (destination resolved by server)")).count();
        for _ in 0..(MAX_CROSS_ATTEMPTS as usize + 5) {
            al.last_zone_cross = Instant::now() - std::time::Duration::from_secs(60);
            al.drain_zone_cross(&mut stream, &mut gs);
        }
        assert_eq!(attempts(&gs), MAX_CROSS_ATTEMPTS as usize,
            "#713 F3: the auto-cross must STOP after {MAX_CROSS_ATTEMPTS} attempts in one stand — \
             an unbounded refire is a server anomaly event every cooldown for as long as we stand here");

        // The terminal state is OBSERVABLE and DISTINGUISHABLE, on both channels an agent reads.
        let stops: Vec<_> = gs.messages.iter()
            .filter(|m| m.text.contains("Stopped trying to cross this zone line")).collect();
        assert_eq!(stops.len(), 1,
            "the stop must be announced EXACTLY once — silence would leave an agent waiting forever \
             for a crossing that will never be attempted again, and repeating it every cooldown \
             would just move the retry storm into the message log");
        let tally = gs.zone_cross_attempts.expect("the terminal fact must reach GameState");
        assert!(tally.blocks(), "and it must read as BLOCKING — this is what /observe/debug \
                                    publishes as zone_cross_stopped");
        assert_eq!(tally.count(), MAX_CROSS_ATTEMPTS);
        assert_eq!(tally.last_index(), IDX);
        // The re-arm (walking off the line) moved to
        // `the_escape_hatch_works_on_the_tick_you_step_off_713`. It is not a tail of this test any
        // more because the fixture THIS test needs — a cooldown rewound before every tick, so the
        // bound rather than the cooldown is what stops the sends — is exactly the fixture that hid
        // the round-2 B2 defect: it handed the off-region probe a window the field never gives it.
    }

    /// **#713 review round 2, B2: the escape hatch the client TELLS the agent to use works on the
    /// tick the agent steps off — not one cooldown later, and not never.**
    ///
    /// The defect this pins is an agent-honesty defect, not a retry defect. On reaching the bound
    /// the client writes, unhedged, "Step off every zone line and back on to try again"
    /// (message log) and "walk OFF every zone-line region and back on (that clears the tally)"
    /// (`zone_cross_stopped.detail`). The reset that makes those sentences true used to sit inside
    /// the 10 s auto-cross cooldown guard — and the bound-reaching attempt STAMPS that cooldown on
    /// its way out (`cross_unresolved`), so the off-region probe was dormant for the whole window
    /// immediately after the instruction was issued. An agent that complied promptly saw nothing
    /// change, and `zone_cross_stopped` still read terminal, so it could not tell "I did it wrong"
    /// from "it doesn't work". A walk-off/walk-on excursion shorter than the cooldown was sampled
    /// ON the line at both ends and cleared nothing at all.
    ///
    /// **Why this is a separate test from the bound test above.** The bound test rewinds
    /// `last_zone_cross` before every tick, including before its walk-off drain — a fixture
    /// PRODUCING the precondition instead of the code REACHING it. It therefore graded the reset
    /// under a cooldown window the field does not supply, and passed throughout. Here the hostile
    /// precondition is produced by the client itself (the third attempt stamps the cooldown) and
    /// then ASSERTED as a premise, and nothing after that line touches `last_zone_cross` until the
    /// cooldown's own effect is what is being measured.
    ///
    /// **Mutation checks, measured (#713 review round 2), not reasoned:**
    /// * re-guard the off-region reset with the cooldown (`if index.is_none() &&
    ///   self.last_zone_cross.elapsed() > ZONE_CROSS_COOLDOWN_MS`) — i.e. restore the pre-fix
    ///   behaviour → RED at the "cleared on the very next tick" assertion. The bound test above
    ///   stays GREEN under exactly this mutation, which is the whole reason this test exists.
    /// * delete the cooldown from the SEND guard → RED, but at the "re-arm survives stepping back
    ///   on" assertion rather than the rate-limit one below it: with no cooldown the tick that steps
    ///   back onto the line already re-attempts and records a fresh tally. Either way this test
    ///   cannot be satisfied by deleting the cooldown.
    #[tokio::test]
    async fn the_escape_hatch_works_on_the_tick_you_step_off_713() {
        use eqoxide_core::zone_cross::MAX_CROSS_ATTEMPTS;
        use eqoxide_nav::zone_assets;
        const DEST: u16 = 30;
        const IDX: i32 = 7;
        const HERE: u16 = 54;

        let (mut al, _nav, _command, collision, za) = shared_nav_action_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        *al.world.zone_points.lock().unwrap() = vec![zp_at(99, DEST, [100.0, 200.0, 0.0])];
        let grid = zone_assets::ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, IDX),
        ))).collision().unwrap().clone();
        zone_assets::finish_zone_load(&collision, &za, "testfixture", Some(grid), 1, None);

        let mut gs = GameState::new();
        gs.world.zone_name = "testfixture".into();
        gs.world.zone_id = HERE;
        gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0;
        let attempts = |gs: &GameState| gs.messages.iter()
            .filter(|m| m.text.contains("Crossing zone line (destination resolved by server)")).count();

        // Drive EXACTLY the bound. Rewinding the cooldown before each tick is how a unit test spends
        // 10 s; the last of these ticks is the bound-reaching attempt, and it stamps
        // `last_zone_cross` through the real code path. Nothing below rewinds it until the cooldown
        // is itself the thing under measurement.
        for _ in 0..MAX_CROSS_ATTEMPTS {
            al.last_zone_cross = Instant::now() - std::time::Duration::from_secs(60);
            al.drain_zone_cross(&mut stream, &mut gs);
        }
        assert_eq!(attempts(&gs), MAX_CROSS_ATTEMPTS as usize, "premise: the bound was driven by real attempts");
        assert!(gs.zone_cross_attempts.is_some_and(|t| t.blocks()),
            "premise: the client must be in the terminal state that emits the 'step off' instruction");
        // PREMISE PRODUCED BY THE CODE, not by the fixture: the attempt that reached the bound
        // stamped the auto-cross cooldown, so the client is inside the ~10 s window in which it has
        // just told the agent to step off. This is precisely the window the bound test's fixture
        // skipped, and the window in which the escape hatch used to do nothing.
        assert!(al.last_zone_cross.elapsed() < std::time::Duration::from_millis(10_000),
            "premise: the bound-reaching attempt itself must have stamped the cooldown — if this \
             ever stops holding, this test is no longer measuring the field's hostile window");

        // The agent does exactly what it was told. One tick, no time travel.
        gs.player_x = 500.0;
        al.drain_zone_cross(&mut stream, &mut gs);
        assert!(gs.zone_cross_attempts.is_none(),
            "#713 B2: stepping off every zone-line region must clear the tally on the very next \
             tick. While the reset sat inside the cooldown guard this stayed blocking, so the \
             client's own unhedged instruction ('step off … that clears the tally') was false for \
             up to 10 s — and false FOREVER for any excursion shorter than one cooldown");

        // ...and back on, still inside the same cooldown. The excursion being shorter than a
        // cooldown is the point: the probe must have SEEN the off state, not merely have been
        // handed a chance to look.
        gs.player_x = 0.0;
        al.drain_zone_cross(&mut stream, &mut gs);
        assert!(gs.zone_cross_attempts.is_none(),
            "the re-arm survives stepping back on — a fresh stand starts with a fresh allowance");
        assert_eq!(attempts(&gs), MAX_CROSS_ATTEMPTS as usize,
            "but the SEND is still rate-limited by the 10 s cooldown: re-arming the bound must not \
             turn a walk-off/walk-on into an immediate extra OP_ZoneChange, which is the refire \
             storm #713 exists to close");

        // Once the cooldown is up, the crossing really is attempted again — the recovery is real,
        // not merely a cleared counter.
        al.last_zone_cross = Instant::now() - std::time::Duration::from_secs(60);
        al.drain_zone_cross(&mut stream, &mut gs);
        assert_eq!(attempts(&gs), MAX_CROSS_ATTEMPTS as usize + 1,
            "stepping back onto the line must be allowed to try again");
    }

    /// **#713 review round 2, B1: `zone_cross_best_effort` is NOT a "crossing currently in flight"
    /// signal — it can be non-null at the very moment `zone_cross_stopped` reads TERMINAL.**
    ///
    /// `docs/http-api.md` claimed the marker "always describes the request currently in flight and
    /// never a stale one". `zone_cross_plan` has three writers and **none of them is a nav-terminal
    /// path**: nothing retires it when the walk arrives, when it is blocked, or when the #713
    /// attempt bound fires. The claim was therefore false, and it contradicted the field's own
    /// `detail` string in the same PR. An agent that believed the doc row would read
    /// "a crossing is in flight" beside "this is a TERMINAL state, do not wait on it" and wait
    /// forever. The doc row now states the real clearers (next resolution, and zoning) and says the
    /// two fields can be non-null together; this test is what makes that statement graded.
    ///
    /// It reaches the state through the REAL code path rather than by hand-setting two `GameState`
    /// fields: one real `/zone_cross` that degrades to the #683 fallback, then real auto-cross ticks
    /// until the real bound fires.
    ///
    /// **If a later change DOES retire the plan on nav termination, this test goes RED — and that is
    /// the correct red.** It is not defending the current behaviour as desirable; it is defending
    /// the doc from silently drifting away from it again, in either direction.
    #[tokio::test]
    async fn a_best_effort_marker_outlives_the_terminal_stop_713() {
        use eqoxide_core::zone_cross::MAX_CROSS_ATTEMPTS;
        use eqoxide_nav::zone_assets;
        const DEST: u16 = 30;
        const IDX: i32 = 7;
        const HERE: u16 = 54;

        let (mut al, _nav, command, collision, za) = shared_nav_action_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        // DEST is advertised, but under an index no baked region carries → the #683 fallback.
        *al.world.zone_points.lock().unwrap() = vec![zp_at(99, DEST, [100.0, 200.0, 0.0])];
        let grid = zone_assets::ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, IDX),
        ))).collision().unwrap().clone();
        zone_assets::finish_zone_load(&collision, &za, "testfixture", Some(grid), 1, None);

        let mut gs = GameState::new();
        gs.world.zone_name = "testfixture".into();
        gs.world.zone_id = HERE;
        // Stand outside the region so the request WALKS (and publishes its plan) rather than being
        // consumed by the standing auto-cross on the same tick.
        gs.player_x = 30.0; gs.player_y = 0.0; gs.player_z = 0.0;
        command.request_zone_cross(DEST);
        al.drain_zone_cross(&mut stream, &mut gs);
        assert!(gs.zone_cross_plan.is_some_and(|p| p.resolution.is_best_effort()),
            "premise: the request must really have degraded to the #683 best-effort fallback");

        // The walk arrives — the character is now standing on that same line — and the auto-cross
        // runs out its allowance. Every state below is reached by the code, not written by hand.
        gs.player_x = 0.0;
        for _ in 0..(MAX_CROSS_ATTEMPTS + 2) {
            al.last_zone_cross = Instant::now() - std::time::Duration::from_secs(60);
            al.drain_zone_cross(&mut stream, &mut gs);
        }
        assert!(gs.zone_cross_attempts.is_some_and(|t| t.blocks()),
            "premise: the attempt bound must have fired, so zone_cross_stopped is non-null");

        // MEASURED: both disclosures are live on the SAME snapshot. This is the fact the doc row
        // denied.
        assert!(gs.zone_cross_plan.is_some_and(|p| p.resolution.is_best_effort()),
            "#713 B1: zone_cross_best_effort SURVIVES the terminal stop — nothing retires the plan \
             on a nav-terminal path. The docs must describe it as 'the most recent resolution in \
             this zone', never as 'the request currently in flight'");
    }

    /// **#713 item 2 (the #683 review's F4): the best-effort degradation is STRUCTURED, not prose.**
    ///
    /// When `POST /v1/move/zone_cross {"zone_id":N}` cannot find a region carrying an advertised
    /// index for N, it falls back to walking to an UNRESOLVABLE line and letting the server pick the
    /// destination (#683). Nothing false is asserted — the client never claims it will arrive in N —
    /// but the only signal was a sentence in the message log, so a caller could not DETECT the
    /// degradation. `GameState::zone_cross_plan` is the machine-readable half.
    ///
    /// Pins both arms (so the marker distinguishes rather than merely existing) and the
    /// clear-on-event path that keeps it from latching (#343's `connected: true` failure mode).
    ///
    /// **Mutation checks:** make the plan `ServerResolved` unconditionally → the advertised arm's
    /// assertion goes RED; delete the `gs.zone_cross_plan = None` at the top of `resolve_zone_cross`
    /// → the "a later advertised resolution clears it" assertion goes RED (the marker latches on).
    #[tokio::test]
    async fn a_server_resolved_zone_cross_is_marked_best_effort_and_clears_713() {
        use eqoxide_core::zone_cross::ZoneCrossResolution;
        use eqoxide_nav::zone_assets;
        const DEST: u16 = 30;
        const IDX: i32 = 7;
        const HERE: u16 = 54;

        // (label, advertised iterator for DEST, expected resolution)
        for &(label, advert_index, want) in &[
            // The advert's index matches the baked region → destination known before the walk.
            ("advertised", IDX as u32, ZoneCrossResolution::Advertised { zone_id: DEST }),
            // DEST is advertised but under an index no region carries (qrg's shape) → #683 fallback.
            ("fallback",   99u32,      ZoneCrossResolution::ServerResolved),
        ] {
            let (mut al, _nav, command, collision, za) = shared_nav_action_loop();
            let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
            *al.world.zone_points.lock().unwrap() = vec![zp_at(advert_index, DEST, [100.0, 200.0, 0.0])];
            let grid = zone_assets::ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
                eqoxide_core::region_map::RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, IDX),
            ))).collision().unwrap().clone();
            zone_assets::finish_zone_load(&collision, &za, "testfixture", Some(grid), 1, None);

            let mut gs = GameState::new();
            gs.world.zone_name = "testfixture".into();
            gs.world.zone_id = HERE;
            // Stand OUTSIDE the region so the resolution walks (and the standing auto-cross, which
            // would consume the plan's zone by crossing, cannot fire).
            gs.player_x = 30.0; gs.player_y = 0.0; gs.player_z = 0.0;

            command.request_zone_cross(DEST);
            al.drain_zone_cross(&mut stream, &mut gs);

            let plan = gs.zone_cross_plan.unwrap_or_else(|| panic!("{label}: a located cross must publish its plan"));
            assert_eq!(plan.resolution, want,
                "{label}: the marker must distinguish a known destination from one the server picks");
            assert_eq!(plan.requested_zone_id, Some(DEST), "{label}: and say WHICH request it answers");
            assert_eq!(plan.index, IDX, "{label}");
            assert_eq!(plan.resolution.is_best_effort(), label == "fallback",
                "{label}: only the #683 fallback is best effort");
            // The human half must agree with the machine half — they are derived from one value.
            assert_eq!(gs.messages.iter().any(|m| m.text.contains("best effort")), label == "fallback",
                "{label}: the message log and the structured marker must not disagree");

            // CLEAR-ON-EVENT: a later resolution that finds a properly advertised line must not
            // inherit this one's marker. (Re-advertise under IDX so the next resolution is exact.)
            *al.world.zone_points.lock().unwrap() = vec![zp_at(IDX as u32, DEST, [100.0, 200.0, 0.0])];
            command.request_zone_cross(DEST);
            al.drain_zone_cross(&mut stream, &mut gs);
            assert_eq!(gs.zone_cross_plan.map(|p| p.resolution),
                       Some(ZoneCrossResolution::Advertised { zone_id: DEST }),
                "{label}: the marker describes the LAST resolution — it must not latch on");

            // …and a resolution that locates NO line at all must not leave the previous plan
            // standing either. This is the arm that actually needs the clear at the TOP of
            // `resolve_zone_cross`: the located branch overwrites the plan on its own, so a marker
            // cleared only there would survive every FAILED request and describe a crossing the
            // client is no longer attempting — a stale observable with no clear-on-event path,
            // which is the #343 failure mode.
            gs.zone_cross_plan = Some(ZoneCrossPlan {
                requested_zone_id: Some(DEST), index: IDX,
                resolution: ZoneCrossResolution::ServerResolved,
            });
            command.request_zone_cross(9999); // advertised nowhere → the resolution locates nothing
            al.drain_zone_cross(&mut stream, &mut gs);
            assert!(gs.zone_cross_plan.is_none(),
                "{label}: a request that resolved to no line must clear the marker, not inherit it");
        }
    }

    /// **#725 review, B1: the `idle` row in `docs/http-api.md` is pinned to the code, in BOTH
    /// directions.** The finding that produced this test was not a wrong field value — it was a true
    /// sentence in the docs that stopped being true when a code path was added. Rewriting the prose
    /// fixes today; only a test keeps it fixed, and it has to fail both ways or it only guards the
    /// direction someone happened to think of:
    ///
    /// * add a new NAMED REASON for `idle` and forget the docs → the constant is absent from the
    ///   row → RED.
    /// * list a reason in the docs that nothing publishes → the token matches no constant → RED.
    ///   (Guidance that reads correct and sends an agent to wait for something that never arrives —
    ///   the shape of the B2 finding.)
    ///
    /// The set is the whole point of the row: **`nav_reason: null` on `idle` is only the boot
    /// state**, so every other route to `idle` must be named here and named in the docs.
    ///
    /// # AMENDED in review round 3 — this comment claimed a guarantee this test does not give
    ///
    /// The first bullet above used to read *"add a new way to reach `idle` and forget the docs …
    /// (This is #725's own defect class: a doc sentence the code quietly falsified.)"* **That was
    /// measured false, twice, by the round-3 reviewer, and the correction is kept here rather than
    /// deleted — a deleted wrong claim is how a wrong claim comes back.**
    ///
    /// * Reinstating B1's original defect verbatim (`Walker::reset_for_zone_change` back to
    ///   `set_nav_state("idle")`) leaves THIS TEST GREEN. Only the separate hand-written
    ///   `a_zone_change_publishes_idle_with_a_reason_not_a_bare_idle_725` goes red — a per-call-site
    ///   test, which by definition does not exist for a call site nobody has thought of yet.
    /// * Adding a brand-new reasonless `idle` on a path with no dedicated assertion —
    ///   `self.walker.set_nav_state("idle")` in `perform_cross`'s CROSS-ZONE branch, i.e. the
    ///   `/v1/move/zone_cross` success path, the same crossing B1's original defect lived on — left
    ///   the entire suite GREEN (39 / 175 / 372), reproduced in round 3 before the fix below.
    ///
    /// **The rule, not the example.** `published` is a hand-maintained array of constants, so this
    /// test binds *docs ↔ constants*. It cannot see any code path that publishes no constant: such
    /// a path adds nothing to the array, so both directions are vacuously satisfied. That is a
    /// structural ceiling, not a gap in the assertions — a second, third or hundredth reasonless
    /// `idle` is equally invisible to it, and so is any future `nav_reason: null` on any other
    /// state. **What covers that is the `debug_assert!` in `Walker::set_nav_state_because` and
    /// `CommandState::stamp_new_goal`** — the only two production writers that take a reason
    /// argument (the third, `ZoneCrossTicket::drop`, always supplies one), so the universal is
    /// enforced where the value is written rather than where it is documented. Measured in round 3:
    /// against the reasonless-`idle` mutation those asserts go RED in two existing tests, and
    /// against the unmutated branch the suite is GREEN with no false positives — the boot state is
    /// built by `NavStatus::default()`, which routes through neither writer.
    ///
    /// It lives in `eqoxide-net` because that is the crate that can see both halves — `zoned`,
    /// `goal_dropped` and `respawned` are the walker's, `stopped`, `goto_superseded` and
    /// `zone_cross_dropped_unhandled` are `CommandState`'s. Splitting the assertion by crate would
    /// let a reason exist in one half and be missing from the docs, which is the bug.
    #[test]
    fn the_documented_idle_reasons_are_exactly_the_ones_the_code_can_publish_725() {
        const DOC: &str = include_str!("../../../docs/http-api.md");
        // Every constant that can end up as `nav_reason` alongside `nav_state: "idle"`.
        let published = [
            eqoxide_nav::walker::NAV_REASON_ZONED,
            eqoxide_nav::walker::NAV_REASON_GOAL_DROPPED,
            eqoxide_nav::walker::NAV_REASON_RESPAWNED,
            eqoxide_command::NAV_REASON_STOPPED,
            eqoxide_command::NAV_REASON_GOTO_CANCELLED,
            eqoxide_command::NAV_REASON_ZONE_CROSS_UNHANDLED,
        ];

        // The `idle` row of the nav_state table; its last cell is the reason list.
        let row = DOC.lines().find(|l| l.starts_with("| `idle` |"))
            .expect("docs/http-api.md must document the `idle` nav_state");
        let cell = row.trim_end_matches('|').rsplit('|').next().unwrap();
        let documented: Vec<&str> = cell.split('`')
            .skip(1).step_by(2)                 // the odd-indexed pieces are the backticked tokens
            .filter(|t| *t != "null" && *t != "idle" && *t != "nav_reason" && *t != "nav_state")
            .collect();

        // Direction 1: code → docs.
        for reason in published {
            assert!(documented.contains(&reason),
                "#725 B1: `{reason}` can be published on `idle` but the `idle` row of \
                 docs/http-api.md does not list it. An agent reading that row would treat it as an \
                 unexplained idle. Documented: {documented:?}");
            assert!(DOC.contains(&format!("| `{reason}` |")),
                "`{reason}` is listed on the `idle` row but has no entry in the reason table — the \
                 row tells an agent the reason explains itself, so it has to be explained");
        }
        // Direction 2: docs → code. This is the half that catches guidance pointing at something
        // that never arrives; nothing else in the suite fails when the docs over-promise.
        for token in &documented {
            assert!(published.contains(token),
                "docs/http-api.md lists `{token}` as a reason `idle` can carry, but no code path \
                 publishes it. Either wire it up or stop telling agents to expect it.");
        }
    }

    /// **#725 review, N1 + B3: the case the sweep deliberately excluded — standing INSIDE the
    /// region.** The sweep above asserts its own premise that the character is outside the trigger
    /// volume, so nothing in the suite exercised the one call where BOTH halves of
    /// `drain_zone_cross` do work: the resolution issues a walk, and the standing auto-cross fires
    /// from physical position, in the same tick.
    ///
    /// That is also the call where the ticket's lifetime is load-bearing (B3). It asserts three
    /// things a reader of the resolution's doc comment should be able to rely on:
    ///
    /// 1. an inside-the-region cross still resolves to a walk (the removed shortcut's "we are
    ///    already close enough, do nothing" case — now the goal is metres away and honest);
    /// 2. the auto-cross still fires in the same call, i.e. removing the shortcut did not cost the
    ///    delegation it was wrongly assuming;
    /// 3. **nothing publishes `zone_cross_dropped_unhandled`.** The cross-zone auto-cross branch
    ///    writes no `nav_state` at all, so a ticket still alive at that point would be free to
    ///    stamp `idle`/`zone_cross_dropped_unhandled` over a crossing that is genuinely in flight.
    ///    It cannot, because `resolve_zone_cross` owns the ticket and has returned by then.
    ///
    /// **Mutation check, and the honest limit of it.** Moving the ticket's settlement past the
    /// auto-cross as a one-line statement move — the mutation the previous shape lost — no longer
    /// compiles (`E0308`), because the ticket is owned by `resolve_zone_cross` and is not in scope
    /// down there.
    ///
    /// # AMENDED in review round 3
    ///
    /// This paragraph used to end: *"**Measured, and not caught:** doing it as a deliberate two-part
    /// refactor (signature changed to `&ZoneCrossTicket`, plus a late `drop`) leaves this test and
    /// the whole `eqoxide-net` suite GREEN … so the accidental case is prevented by the type and the
    /// deliberate case rests on this comment."* Two corrections, both measured in round 3:
    ///
    /// 1. **It is easier to reach than that said** — the late `drop` is not needed, because the
    ///    caller's binding is a local of `drain_zone_cross` and already outlives the auto-cross.
    ///    Two edits, both looking like "avoid a move" tidying. Reproduced: suite GREEN.
    /// 2. **It is no longer uncaught.** `drain_zone_cross` now `debug_assert_eq!`s that no drained
    ///    ticket is alive when it reaches the auto-cross block. Against the two-edit shape that is
    ///    RED in 6 `eqoxide-net` tests including this one; on the unmutated branch it is GREEN.
    ///
    /// What round 2 got RIGHT and round 3 confirmed: no test can catch it by observing a wrong
    /// OUTCOME. That needs a resolution arm that publishes nothing, which is what the ticket exists
    /// to make impossible, and both orderings leave the same final state. The guard therefore checks
    /// the ticket's LIFETIME directly rather than its effect — which is also why it stays a backstop
    /// and not the mechanism: the by-value parameter is what makes the accidental case impossible.
    #[tokio::test]
    async fn a_cross_requested_from_inside_the_region_walks_and_crosses_in_one_tick_725() {
        use eqoxide_nav::zone_assets;
        const DEST: u16 = 30;
        const IDX: i32 = 7;

        let (mut al, nav, command, collision, za) = shared_nav_action_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        *al.world.zone_points.lock().unwrap() = vec![zp_at(IDX as u32, DEST, [100.0, 200.0, 0.0])];
        al.last_zone_cross = Instant::now() - Duration::from_secs(11); // past the auto-cross cooldown
        let grid = zone_assets::ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, IDX),
        ))).collision().unwrap().clone();
        zone_assets::finish_zone_load(&collision, &za, "testfixture", Some(grid), 1, None);

        let mut gs = GameState::new();
        gs.world.zone_name = "testfixture".into();
        gs.world.zone_id = 1; // != DEST, so the auto-cross takes the genuine CROSS-zone branch
        gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0;

        // PREMISE, asserted: the capsule really is inside the trigger volume here — the exact
        // condition the deleted shortcut assumed held from up to 15u away.
        assert!(collision.read().unwrap().as_ref().unwrap()
                .zone_line_at_standing([gs.player_x, gs.player_y, gs.player_z]).is_some(),
            "premise: standing inside the DRNTP region");

        command.request_zone_cross(DEST);
        al.drain_zone_cross(&mut stream, &mut gs);

        // (1) resolved to a walk rather than being swallowed
        assert!(nav.goto_target.lock().unwrap().is_some(),
            "#725: an inside-the-region cross resolves to a walk like any other — the goal is just \
             metres away, which is honest, unlike the shortcut that published nothing");
        assert!(gs.messages.iter().any(|m| m.text.contains("Walking to the zone")),
            "and says so on the channel an HTTP agent can read");
        // (2) the auto-cross fired in the SAME call
        assert!(gs.messages.iter().any(|m| m.text.contains("Crossing to zone")),
            "the standing auto-cross must still fire from physical position in this same tick");
        // (3) the backstop did not fire over a live crossing
        let s = nav.nav_state.lock().unwrap().clone();
        assert_ne!(s.reason.as_deref(), Some(eqoxide_command::NAV_REASON_ZONE_CROSS_UNHANDLED),
            "#725 B3: the cross-zone auto-cross branch publishes no nav_state, so a ticket still \
             alive here would stamp `dropped_unhandled` ON TOP of a crossing that is in flight — \
             the backstop telling the very kind of lie it exists to prevent");
    }

    /// **#725, the honesty half, through the real drain.** Even with the band closed, the thing that
    /// made it undebuggable must be structurally impossible: a request that leaves the one-shot slot
    /// and produces no observable outcome. `drain_zone_cross` now drains a `ZoneCrossTicket`, so the
    /// *default* behaviour of any path that forgets to publish is an honest terminal state.
    ///
    /// This pins the property at the drain boundary for every refusal the drain can reach — none may
    /// leave the accept's `pending` standing, because with the slot empty nothing downstream can
    /// retire it (the walker's own retirement is pinned separately in
    /// `no_in_progress_nav_state_survives_a_tick_with_no_goal_725`).
    ///
    /// **Mutation check:** delete `impl Drop for ZoneCrossTicket` AND make any single arm of the
    /// resolution silent (e.g. return early on `known_dest.is_none()`) → that case reads `pending`
    /// and goes RED. With the ticket in place the same edit is still caught, now as
    /// `zone_cross_dropped_unhandled`, which is the difference between a silent bug and a reported
    /// one.
    #[tokio::test]
    async fn no_drained_zone_cross_ever_leaves_pending_standing_725() {
        use eqoxide_nav::zone_assets;
        const DEST: u16 = 30;

        // (label, advertise the destination?, install a grid with a matching region?)
        for (label, advertised, region) in [
            ("no zone point advertised for the requested zone", false, false),
            ("advertised but no region in the loaded map",      true,  false),
            ("advertised with a real region to walk to",        true,  true),
        ] {
            let (mut al, nav, command, collision, za) = shared_nav_action_loop();
            let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
            if advertised {
                *al.world.zone_points.lock().unwrap() = vec![zp_at(7, DEST, [100.0, 200.0, 0.0])];
            }
            let water = region.then(|| std::sync::Arc::new(
                eqoxide_core::region_map::RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, 7)));
            let grid = zone_assets::ZoneAssetState::test_ready_with_water(water)
                .collision().unwrap().clone();
            zone_assets::finish_zone_load(&collision, &za, "testfixture", Some(grid), 1, None);

            let mut gs = GameState::new();
            gs.world.zone_name = "testfixture".into();
            gs.player_x = 20.0; gs.player_y = 0.0; gs.player_z = 0.0;

            command.request_zone_cross(DEST);
            assert_eq!(nav.nav_state.lock().unwrap().state, "pending", "{label}: the accept stamps pending");
            al.drain_zone_cross(&mut stream, &mut gs);

            let s = nav.nav_state.lock().unwrap().clone();
            if region {
                // Resolved into a walk: `pending` here belongs to the NEW goal `request_goto`
                // stamped, and a live `goto_target` backs it — the walker carries it forward.
                assert!(nav.goto_target.lock().unwrap().is_some(),
                    "{label}: a resolved cross leaves a real goal in flight");
            } else {
                assert_ne!(s.state, "pending",
                    "{label}: the request is gone from its slot, so `pending` can never be retired \
                     by anything downstream — a drained request must publish its outcome (#725)");
                assert!(s.reason.is_some(),
                    "{label}: and terminal states carry a machine-readable reason");
            }
        }
    }

    // ─────────────────────────────────────────────────────────────────────────────────────────
    // A3 Migration 1 (#448): the honest awaited merchant-buy path. These drive a buy through
    // `drain_merchant` (the loop's own `command` slot is shared with the drain, so a real
    // request→take round-trip runs), then feed the resolving packet exactly as `gameplay.rs` does.
    // ─────────────────────────────────────────────────────────────────────────────────────────

    fn zp_at(iterator: u32, zone_id: u16, pos: [f32; 3]) -> eqoxide_core::game_state::ZonePoint {
        eqoxide_core::game_state::ZonePoint {
            iterator, zone_id,
            server_x: pos[0], server_y: pos[1], server_z: pos[2], heading: 0.0,
        }
    }

    /// #368 — RESOLUTION IS NOT SUPPRESSED FOR SAME-ZONE. A same-zone DRNTP line (an intra-zone
    /// translocator — legitimate retail content, e.g. the qeynos2 teleport pads) must still resolve
    /// to a real destination + arrival coords, exactly like a cross-zone line; only an index with no
    /// advertised zone point resolves to `None`. A regression that re-added a blanket "dest==current
    /// → None" self-zone suppress (which would strand the player on a valid teleporter) flips the
    /// first assertion.
    #[test]
    fn same_zone_line_still_resolves_to_a_destination() {
        const QEYNOS2: u16 = 2;
        const QEYNOS:  u16 = 1;
        let nav = new_loop();
        {
            let mut zps = nav.world.zone_points.lock().unwrap();
            zps.push(zp_at(100, QEYNOS2, [111.0, 222.0, 33.0])); // same-zone translocator
            zps.push(zp_at(200, QEYNOS,  [ -5.0,  -6.0,  0.0])); // genuine cross-zone line
        }
        assert_eq!(nav.resolve_cross_destination(100), Some((QEYNOS2, [111.0, 222.0, 33.0])),
            "a same-zone translocator must still resolve to its arrival — never suppressed (#368)");
        assert_eq!(nav.resolve_cross_destination(200), Some((QEYNOS, [-5.0, -6.0, 0.0])),
            "a real cross-zone line resolves normally");
        assert_eq!(nav.resolve_cross_destination(999), None,
            "an index with no advertised zone point must not cross blindly");
    }

    /// #368 CORE. A SAME-ZONE crossing must (a) reposition the player to the resolved arrival so it
    /// LEAVES the DRNTP region (no re-fire), and (b) flag the echo so the receive side SKIPS the
    /// world reconnect — that reconnect against a still-live zone is the wedge. A genuine CROSS-ZONE
    /// crossing must do NEITHER (it repositions on zone-in, and it MUST reconnect). The same-zone
    /// cross sends NO OP_ZONE_CHANGE (it is a pure in-zone teleport — zoneID=0 would make the server
    /// resolve nearest-XY and zone us cross-zone); the cross-zone one sends exactly one.
    /// Mutation-sensitive: dropping the reposition, the flag-set, the same-zone branch condition, or
    /// the same-zone packet suppression each flips an assertion.
    #[tokio::test]
    async fn same_zone_cross_repositions_and_skips_reconnect_but_cross_zone_does_not() {
        const HERE: u16 = 2;   // current zone
        const OTHER: u16 = 1;  // a different zone
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;

        // ── Same-zone translocator ──────────────────────────────────────────────────────────────
        let mut nav = new_loop();
        let mut gs = GameState::new();
        gs.world.zone_id = HERE;
        gs.player_x = 0.0; gs.player_y = 0.0; gs.player_z = 0.0; // standing on the trigger region
        // The walker was walking to the zone-line goal (`drain_zone_cross` issues a /goto to it).
        nav.command.request_goto((-455.0, -174.0, 30.0));
        let same = nav.perform_cross(&mut stream, &mut gs, 100, HERE, [111.0, 222.0, 33.0]);
        assert!(same, "dest==current must be handled as a same-zone cross");
        // The arrival coords are WIRE datum (DB safe coords, model-origin z); the player's internal
        // z is FOOT, so z is converted 33.0 → 33.0 − WIRE_Z_OFFSET on the local apply (#522). x/y
        // are unaffected.
        assert_eq!([gs.player_x, gs.player_y, gs.player_z], [111.0, 222.0, 33.0 - eqoxide_core::coord::WIRE_Z_OFFSET],
            "same-zone cross repositions the player (foot datum) so it leaves the region (#368/#522)");
        // #508: the crossing is DONE — the translocator repositioned us in-zone. The stale zone-line
        // goal must be cleared so the walker does NOT resume toward it and drift across a DIFFERENT
        // zone's real line (qeynos2 → qeynos). Mutation check: delete `self.command.request_stop()`
        // in `perform_cross`'s same-zone branch and this assertion goes RED.
        assert_eq!(nav.command.goto_target(), None,
            "a same-zone reposition must STOP nav — leaving the pre-cross goal set drifts into an adjacent zone (#508)");
        // The echo (server keeps us in zone HERE) classifies as a same-zone reposition — no
        // reconnect. NON-consuming (#554): a duplicate/retransmitted echo classifies IDENTICALLY,
        // so both the peek and the classification stay stable across repeated calls. Mutation check:
        // making `same_zone_reposition_pending` consume (revert to the old `take`) flips the second
        // classify to CrossZoneReconnect and this assertion goes RED.
        assert!(nav.same_zone_reposition_pending(),
            "same-zone cross must flag the echo so the receive side skips the world reconnect (#368)");
        assert_eq!(nav.classify_zone_change_echo(1, HERE, HERE), ZoneChangeEcho::SameZoneReposition,
            "the server echo (current zone) + pending flag → same-zone reposition, no reconnect");
        assert_eq!(nav.classify_zone_change_echo(1, HERE, HERE), ZoneChangeEcho::SameZoneReposition,
            "a DUPLICATE echo must classify the SAME — the flag is peeked, never consumed (#554)");
        assert!(nav.same_zone_reposition_pending(), "the reposition flag is NOT consumed by classifying");
        // REGRESSION PIN (in-zone pad fix): a same-zone translocator must send NO OP_ZONE_CHANGE. The
        // zoneID=0 packet makes the server resolve nearest-XY and zone us CROSS-zone (qeynos2 guild pad
        // → South Qeynos) on top of the correct local reposition — the "right location, then zoned away"
        // bug. It is a pure in-zone teleport. Mutation: restore the unconditional `send_zone_change_packet`
        // at the top of `perform_cross` and this goes RED.
        assert!(!stream.sent_app_packets().iter().any(|(op, _)| *op == crate::protocol::OP_ZONE_CHANGE),
            "a same-zone translocator must NOT send OP_ZONE_CHANGE — it is a local in-zone reposition");

        // ── Genuine cross-zone line ─────────────────────────────────────────────────────────────
        let mut nav2 = new_loop();
        let mut gs2 = GameState::new();
        gs2.world.zone_id = HERE;
        gs2.player_x = 7.0; gs2.player_y = 8.0; gs2.player_z = 9.0;
        // A genuine cross-zone crossing must NOT be stopped by the #508 same-zone reset: it zones,
        // and its post-zone nav is a separate concern. Seed a goto and confirm it SURVIVES.
        nav2.command.request_goto((100.0, 200.0, 5.0));
        let same2 = nav2.perform_cross(&mut stream, &mut gs2, 200, OTHER, [111.0, 222.0, 33.0]);
        assert!(!same2, "dest!=current must be a cross-zone change");
        // A genuine cross-zone line DOES send OP_ZONE_CHANGE (the other half of the fix: only the
        // same-zone branch suppresses it). Mutation: drop `send_zone_change_packet` from the else
        // branch and this goes RED.
        assert!(stream.sent_app_packets().iter().any(|(op, _)| *op == crate::protocol::OP_ZONE_CHANGE),
            "a genuine cross-zone crossing must still send OP_ZONE_CHANGE");
        assert_eq!([gs2.player_x, gs2.player_y, gs2.player_z], [7.0, 8.0, 9.0],
            "a cross-zone cross must NOT locally reposition (the destination is in the other zone)");
        assert_eq!(nav2.command.goto_target(), Some((100.0, 200.0, 5.0)),
            "a genuine CROSS-zone crossing must NOT clear nav — only the same-zone reposition does (#508)");
        assert!(!nav2.same_zone_reposition_pending(),
            "a cross-zone cross must NOT flag a reposition — it MUST world-reconnect");
        assert_eq!(nav2.classify_zone_change_echo(1, OTHER, HERE), ZoneChangeEcho::CrossZoneReconnect,
            "a genuine cross-zone echo (echo != current) reconnects");
    }

    /// The reposition flag is bounded: a stale set (older than the ~1.5s echo window, #504) never
    /// suppresses a later genuine cross-zone / death reconnect, while a flag still inside the window
    /// (as the real reposition echo — tens of ms — always is) still suppresses correctly. Exercises
    /// both sides of the boundary so a mutation that widens or removes the window goes RED here.
    #[test]
    fn stale_same_zone_flag_does_not_suppress_a_later_reconnect() {
        let mut nav = new_loop();
        assert!(!nav.same_zone_reposition_pending(), "unset flag → no suppression");

        // Just under the window: still live, must suppress (this is where the real echo lands).
        nav.same_zone_cross_at = Instant::now().checked_sub(std::time::Duration::from_millis(1400));
        assert!(nav.same_zone_reposition_pending(),
            "a flag just under the ~1.5s window must still suppress the matching echo");

        // Just over the window: expired, must not suppress.
        nav.same_zone_cross_at = Instant::now().checked_sub(std::time::Duration::from_millis(1600));
        assert!(!nav.same_zone_reposition_pending(),
            "a flag older than the ~1.5s window must not suppress a reconnect");
    }

    /// #554 — the vault double-cross, at the classification boundary. The qeynos2 Knights-of-Truth
    /// waterfall translocator (index=2) fires: the client locally GUESSED same-zone (dest zone_id=2
    /// == current), so the same-zone pending flag IS set. But the server resolves the zone point to
    /// zone_id=1 (South Qeynos) and echoes `success=1 zone_id=1` — TWICE (a retransmit). The old
    /// code consumed the flag on the first echo (→ reposition, no reconnect) and let the DUPLICATE
    /// fall through to a world reconnect, so the char did BOTH → bounced to the wrong zone.
    ///
    /// Server-authoritative classification fixes it two ways at once:
    ///   1. the echoed zone_id (1) != current (2) → CrossZoneReconnect even though the client's
    ///      local guess (and the pending flag) said same-zone;
    ///   2. the flag is peeked, not consumed, so the duplicate echo classifies IDENTICALLY.
    /// So BOTH echoes → CrossZoneReconnect, and `world_reconnect_needed` (a set-once bool on the
    /// receive side) reconnects exactly once. Mutation checks: (a) drop the `echo == current` guard
    /// (classify same-zone purely on the pending flag) → the first assertion goes RED (it would
    /// SameZoneReposition and never zone to South Qeynos); (b) revert `same_zone_reposition_pending`
    /// to a consuming `take` → the duplicate assertion goes RED (duplicate would SameZoneReposition,
    /// the exact old split-interpretation bounce).
    #[test]
    fn vault_translocator_server_resolves_cross_zone_no_double_cross() {
        const CURRENT: u16 = 2; // qeynos2 (North Qeynos), where we stand
        const SERVER_DEST: u16 = 1; // qeynos (South Qeynos), what the server actually resolved

        let mut nav = new_loop();
        // Client locally guessed the translocator was same-zone, so the pending flag is set (as
        // `perform_cross`'s same-zone branch would).
        nav.same_zone_cross_at = Some(Instant::now());
        assert!(nav.same_zone_reposition_pending(), "precondition: the client guessed same-zone");

        // First echo: server says zone_id=1 (a real cross), NOT the current zone 2. Server truth wins.
        assert_eq!(nav.classify_zone_change_echo(1, SERVER_DEST, CURRENT), ZoneChangeEcho::CrossZoneReconnect,
            "#554: a translocator the server resolved to a DIFFERENT zone must reconnect, not reposition");
        // Duplicate/retransmitted echo: MUST classify identically — no split interpretation, no bounce.
        assert_eq!(nav.classify_zone_change_echo(1, SERVER_DEST, CURRENT), ZoneChangeEcho::CrossZoneReconnect,
            "#554: the duplicate echo classifies IDENTICALLY (peek, not consume) — no double-cross");

        // Pure-function corners, independent of any ActionLoop state:
        // A genuine same-zone reposition (server echoes the current zone) with the flag set → skip.
        assert_eq!(classify_zone_change_echo(1, CURRENT, CURRENT, true), ZoneChangeEcho::SameZoneReposition);
        // A death/bind respawn also echoes the current zone but sets NO pending flag → reconnect.
        assert_eq!(classify_zone_change_echo(1, CURRENT, CURRENT, false), ZoneChangeEcho::CrossZoneReconnect,
            "death/bind respawn echoes the current zone yet must reconnect (no pending flag)");
        // A failed request is ignored regardless of the flag.
        assert_eq!(classify_zone_change_echo(0, CURRENT, CURRENT, true), ZoneChangeEcho::Ignored);
    }

    // `classify_unresolved_cross_fires_only_without_same_zone_pads_683` and
    // `classify_unresolved_cross_property_683` MOVED with the function they test, to
    // `eqoxide_core::zone_cross` (#713). They are unchanged; run them with
    // `cargo test -p eqoxide-core zone_cross`.

    /// **#683 (qrg / Surefall Glade total exit lockout) — end-to-end through `drain_zone_cross`.**
    ///
    /// A character stands in a zone-line region baked with index 0 (the qrg bake). The index
    /// resolves to no advertised zone point, and the old code debug-no-op'd — with qrg's ONLY exit
    /// region shaped this way, the character could never leave the zone. Retail sends
    /// OP_ZoneChange(zoneID=0) on any zone-line hit and the server resolves the destination from
    /// position, so the client must do the same.
    ///
    /// Mutation checks: revert `cross_unresolved` to the debug no-op → the "exactly one
    /// OP_ZONE_CHANGE" assertion goes RED (this is also what unmodified main does, via the
    /// region_map index gate upstream); drop the `last_zone_cross` stamp → the no-refire assertion
    /// goes RED; send a nonzero wire zoneID → the zoneID==0 assertion goes RED.
    #[tokio::test]
    async fn an_unresolved_zone_line_hit_sends_a_server_resolved_cross_683() {
        use eqoxide_nav::zone_assets;
        const QRG: u16 = 54;
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut nav = new_loop();
        let mut gs = GameState::new();
        gs.world.zone_id = QRG;
        {
            let mut zps = nav.world.zone_points.lock().unwrap();
            zps.push(zp_at(1, 181, [1790.0, 1315.0, -13.0])); // → jaggedpine (cross-zone)
            zps.push(zp_at(2, 4,   [196.7, 5100.9, -1.0]));   // → qeytoqrg  (cross-zone)
        }
        // The player (origin) stands inside a zone-line region carrying index 0 — no advertised
        // point has iterator 0, so `resolve_cross_destination(0)` is None.
        let grid = zone_assets::ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_below(1000.0, 0),
        ))).collision().unwrap().clone();
        zone_assets::finish_zone_load(&nav.collision, &nav.zone_assets, "qrg", Some(grid), 1, None);
        nav.last_zone_cross = Instant::now() - Duration::from_secs(11); // past the 10s cooldown

        nav.drain_zone_cross(&mut stream, &mut gs);

        let zc = |s: &EqStream| s.sent_app_packets().into_iter()
            .filter(|(op, _)| *op == OP_ZONE_CHANGE).collect::<Vec<_>>();
        let sent = zc(&stream);
        assert_eq!(sent.len(), 1,
            "an unresolved zone-line hit must send a server-resolved OP_ZONE_CHANGE, not no-op (#683)");
        // The wire zoneID is the resolve-from-position sentinel 0 — the client never claims a
        // destination it cannot know.
        assert_eq!(u16::from_le_bytes([sent[0].1[64], sent[0].1[65]]), 0,
            "wire zoneID must be 0 (server resolves from position)");
        // HONESTY: the client reports the crossing WITHOUT predicting a destination — the arrival
        // zone comes only from the server echo. No "Crossing to zone N" claim may exist.
        assert!(gs.messages.iter().any(|m| m.text.contains("destination resolved by server")),
            "the crossing must be reported with an explicit unknown destination");
        assert!(!gs.messages.iter().any(|m| m.text.starts_with("Crossing to zone")),
            "the client must never fabricate a destination for an unresolved crossing");
        // The cooldown was stamped: still standing in the region while the echo is in flight, an
        // immediate second drain must NOT refire.
        nav.drain_zone_cross(&mut stream, &mut gs);
        assert_eq!(zc(&stream).len(), 1, "the fallback must stamp the cross cooldown — no refire");
    }

    /// **#683 gates, end-to-end: the fallback must NOT fire while a same-zone pad is advertised,
    /// before zone points arrive, or when only client-synthesized map-label points are present.**
    /// Identical setup to the firing test except for the advertised list — so the discriminator is
    /// the gate, not the plumbing. And the refusal must be OBSERVABLE, exactly once per stand
    /// (#683 review F1): a `tracing::debug!` alone left an agent standing on a `zone_id: null`
    /// exit waiting forever with zero feedback, "in progress" indistinguishable from "refused".
    ///
    /// Mutation checks: drop either classify gate, or revert gate 1 to plain `!is_empty()` → the
    /// corresponding case sends a packet, RED; drop the refusal `log_msg` → the message assertion
    /// goes RED; drop the one-shot latch → the exactly-once assertion goes RED; drop the
    /// latch re-arm (probe-None clear) → the re-entry assertion goes RED.
    #[tokio::test]
    async fn the_unresolved_cross_fallback_stays_shut_and_reports_the_refusal_once_683() {
        use eqoxide_nav::zone_assets;
        const HERE: u16 = 2;
        let refusals = |gs: &GameState| gs.messages.iter()
            .filter(|m| m.text.contains("auto-cross is disabled here")).count();
        for (label, points) in [
            ("same-zone pad advertised", vec![zp_at(1, 181, [0.0; 3]), zp_at(9, HERE, [111.0, 222.0, 33.0])]),
            ("no zone points yet", vec![]),
            // Client-synthesized map-label entries only (`sync_zone_points` marks them
            // iterator=u32::MAX; observed live in qeynos) — non-empty, yet NO server advert has
            // arrived, so the zone-in window may still be open (#683 review F2).
            ("only synthetic label points", vec![zp_at(u32::MAX, 181, [10.0, 20.0, 0.0])]),
        ] {
            let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
            let mut nav = new_loop();
            let mut gs = GameState::new();
            gs.world.zone_id = HERE;
            nav.world.zone_points.lock().unwrap().extend(points);
            let grid = zone_assets::ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
                eqoxide_core::region_map::RegionMap::zone_line_below(1000.0, 0),
            ))).collision().unwrap().clone();
            zone_assets::finish_zone_load(&nav.collision, &nav.zone_assets, "qeynos2", Some(grid), 1, None);
            nav.last_zone_cross = Instant::now() - Duration::from_secs(11);

            // Two ticks standing on the line: no packet ever, and the refusal reported ONCE.
            nav.drain_zone_cross(&mut stream, &mut gs);
            nav.drain_zone_cross(&mut stream, &mut gs);

            assert!(!stream.sent_app_packets().iter().any(|(op, _)| *op == OP_ZONE_CHANGE),
                "{label}: the unresolved-cross fallback must stay shut (#679 defense)");
            assert_eq!(refusals(&gs), 1,
                "{label}: the refusal must be reported to the message log exactly once per stand — \
                 not zero (silent forever-wait) and not per-tick (spam)");

            // Step off the line (above the region), tick — the latch re-arms — then step back on:
            // the agent is told again for the NEW stand.
            gs.player_z = 2000.0;
            nav.drain_zone_cross(&mut stream, &mut gs);
            gs.player_z = 0.0;
            nav.drain_zone_cross(&mut stream, &mut gs);
            assert_eq!(refusals(&gs), 2,
                "{label}: leaving and re-entering the region must re-report the refusal");
        }
    }

    /// **#683 round 3 — the gated-refusal latch must NOT survive a zone change.**
    ///
    /// Region indices are per-zone namespaces and index 0 is the universal unresolvable index, so
    /// a server teleport that lands the character from one gated null region directly onto
    /// ANOTHER zone's gated null region would find the latch still set for "index 0" and silently
    /// suppress the NEW zone's refusal message — the F1 silent wait this message exists to
    /// remove, resurrected across a zone boundary. The observable behaviour under test: after a
    /// zone change, standing on the new zone's gated line reports the refusal AGAIN.
    ///
    /// Mutation check: drop the `gated_cross_reported = None` from `sync_zone_points`' zone-change
    /// branch → the second-message assertion goes RED.
    #[tokio::test]
    async fn the_gated_refusal_latch_resets_on_zone_change_683() {
        use eqoxide_nav::zone_assets;
        let refusals = |gs: &GameState| gs.messages.iter()
            .filter(|m| m.text.contains("auto-cross is disabled here")).count();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut nav = new_loop();
        let mut gs = GameState::new();

        // Zone A: gated (no zone points at all), standing on an index-0 line → one refusal.
        gs.world.zone_id = 2;
        gs.world.zone_name = "qeynos2".into();
        nav.sync_zone_points(&gs);
        let grid = zone_assets::ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_below(1000.0, 0),
        ))).collision().unwrap().clone();
        zone_assets::finish_zone_load(&nav.collision, &nav.zone_assets, "qeynos2", Some(grid.clone()), 1, None);
        nav.last_zone_cross = Instant::now() - Duration::from_secs(11);
        nav.drain_zone_cross(&mut stream, &mut gs);
        assert_eq!(refusals(&gs), 1, "zone A: the gated stand is reported once");

        // Server teleport (e.g. a GM #zone) to zone B, landing DIRECTLY on another gated index-0
        // line — the character is never observed off-region in between.
        gs.begin_zone_in();
        gs.world.zone_id = 24;
        gs.world.zone_name = "erudnext".into();
        nav.sync_zone_points(&gs); // the zone-change tick — must re-arm the latch
        zone_assets::finish_zone_load(&nav.collision, &nav.zone_assets, "erudnext", Some(grid), 1, None);
        nav.last_zone_cross = Instant::now() - Duration::from_secs(11);
        nav.drain_zone_cross(&mut stream, &mut gs);

        assert_eq!(refusals(&gs), 2,
            "zone B's refusal must be reported — a latch left set across a zone change would leave \
             the agent in a silent wait in the NEW zone (the exact F1 defect, cross-zone)");
        assert!(!stream.sent_app_packets().iter().any(|(op, _)| *op == OP_ZONE_CHANGE),
            "no crossing fires in either gated zone");
    }

    /// **#683 — the `/v1/move/zone_cross {{zone_id:N}}` walk-to falls back to the nearest
    /// UNRESOLVABLE zone line when the advertised index has no located region.** qrg advertises
    /// zone 181/4 but its only exit region is baked index 0, so the per-index lookup finds
    /// nothing; the old code answered the definitive `no_path zone_line_not_in_map` and the agent
    /// could never even WALK to the exit.
    ///
    /// The scene is the qeynos MIXED shape (#672, observed live): a NEARER region that RESOLVES
    /// to a different advertised zone (the catacombs decoy) plus a FARTHER index-0 region. The
    /// fallback must pick the index-0 line — a resolvable region is KNOWN to lead to its own
    /// other zone, and walking onto it would cross the caller somewhere it never asked to go. And
    /// it must report the destination honestly as server-resolved — never as the requested zone
    /// (qrg's real triggers are 0,0,0 placeholders the server tie-breaks itself).
    ///
    /// Mutation checks: drop the `.or_else` fallback in `drain_zone_cross` → goto stays unset,
    /// RED; relax the fallback to `find_zone_line_near(None, ..)` (any nearest) → it walks to the
    /// nearer resolvable decoy, the footprint assertion goes RED; drop the `zone_id != 0`
    /// sentinel filter from the honest-messaging lookup → the message claims "zone 0" (seen
    /// live), the destination-resolved-by-server assertion goes RED. On unmodified main this test
    /// fails earlier still: the index-0 region is not even recognized by the region map.
    #[tokio::test]
    async fn zone_cross_walks_to_an_unadvertised_line_when_the_index_lookup_fails_683() {
        use eqoxide_nav::zone_assets;
        const QRG: u16 = 54;
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut nav = new_loop();
        let mut gs = GameState::new();
        gs.world.zone_id = QRG;
        gs.world.zone_name = "qrg".into();
        {
            let mut zps = nav.world.zone_points.lock().unwrap();
            zps.push(zp_at(1, 181, [1790.0, 1315.0, -13.0]));
            zps.push(zp_at(2, 4,   [196.7, 5100.9, -1.0]));
            zps.push(zp_at(33, 45, [50.0, 50.0, 0.0])); // the resolvable decoy's destination
            // The iterator-0 / zone_id-0 SENTINEL qrg really advertises. It matches the located
            // region's index (0) but is NOT a destination — the honest-messaging lookup must
            // filter it (seen live before the filter: "walking to the zone_id=0 line").
            zps.push(zp_at(0, 0, [0.0, 0.0, 0.0]));
        }
        // Two zone-line regions on the flat test floor (z-slab [-1, 5] so the floor-projected
        // point still classifies inside), player at the origin, both beyond the 15u "already on
        // the line" radius: a NEARER decoy (north/east 20..40, ~42u) whose index 33 RESOLVES to
        // zone 45, and a FARTHER index-0 region (north/east 60..80, ~99u) resolving to nothing.
        let grid = zone_assets::ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_boxes(&[
                ([20.0, 40.0, 20.0, 40.0, -1.0, 5.0], 33),
                ([60.0, 80.0, 60.0, 80.0, -1.0, 5.0], 0),
            ]),
        ))).collision().unwrap().clone();
        zone_assets::finish_zone_load(&nav.collision, &nav.zone_assets, "qrg", Some(grid), 1, None);

        nav.command.request_zone_cross(181); // the agent asks for a zone whose index has no region

        nav.drain_zone_cross(&mut stream, &mut gs);

        let goal = nav.command.goto_target()
            .expect("the fallback must walk toward the nearest unresolvable zone line, not report no_path (#683)");
        assert!((60.0..=80.0).contains(&goal.0) && (60.0..=80.0).contains(&goal.1),
            "the walk goal must be inside the INDEX-0 region footprint — never the nearer decoy that \
             resolves to a different zone (it would cross the caller to a zone it never asked for), got {goal:?}");
        // HONESTY: the client must not claim the walk leads to the REQUESTED zone — the server may
        // tie-break the crossing elsewhere.
        assert!(gs.messages.iter().any(|m| m.text.contains("destination resolved by server")),
            "the walk must be reported with an explicit server-resolved destination");
        assert!(!gs.messages.iter().any(|m| m.text.contains("Walking to the zone 181")),
            "the client must never claim the unlocated line leads to the requested zone");
    }

    fn new_loop() -> ActionLoop {
        let g: eqoxide_ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(eqoxide_ipc::GroupSnapshot::default()));
        test_action_loop(g)
    }

    /// A 32-byte RoF2 Merchant_Sell_Struct echo: npcid@0, itemslot@8, quantity@16, price@24.
    fn buy_echo(npcid: u32, slot: u32, price: u32) -> Vec<u8> {
        let mut e = vec![0u8; 32];
        e[0..4].copy_from_slice(&npcid.to_le_bytes());
        e[8..12].copy_from_slice(&slot.to_le_bytes());
        e[16..20].copy_from_slice(&1u32.to_le_bytes());
        e[24..28].copy_from_slice(&price.to_le_bytes());
        e
    }

    fn seed_buy_gs() -> GameState {
        let mut gs = GameState::new();
        gs.player_id = 42;
        gs.coin = [0, 0, 0, 100];
        gs.coin_confirmed = true; // a real coin reading had landed, so coin_verified() started true
        gs.merchant_open = Some(11);
        gs.merchant_items.push(eqoxide_core::game_state::MerchantItem {
            merchant_slot: 3, item_id: 1, name: "Rusty Dagger".into(), icon: 0, price: 5, quantity: 1,
        });
        gs
    }

    /// SUCCESS: an awaited buy, driven through the drain and confirmed by the OP_ShopPlayerBuy echo,
    /// resolves to `Resolved(BuyOk)` carrying the item name, the server price, and the coin AFTER the
    /// applied deduction. `pending_buy` is consumed exactly once.
    #[tokio::test]
    async fn awaited_buy_resolves_on_the_shop_echo_with_the_real_receipt() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_buy_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_buy_await(11, 3, tx);
        nav.drain_merchant(&mut stream, &mut gs);
        assert!(nav.pending_buy.is_some(), "the awaited buy must be parked after the drain");
        assert!(!gs.coin_verified(), "a buy in flight marks coin unverified until reconciled");

        // The confirming echo arrives — apply it (deducts coin, logs "Bought item") THEN fulfil,
        // exactly as the gameplay loop does after `apply_packet`.
        let echo = buy_echo(11, 3, 5);
        apply_packet(&mut gs, &AppPacket { opcode: OP_SHOP_PLAYER_BUY, payload: echo.clone() });
        nav.fulfill_buy_ok(&gs, &echo);

        assert_eq!(
            resp.await.unwrap(),
            eqoxide_command::CommandResult::Resolved(eqoxide_command::BuyOk {
                // 100c − 5c = 95c, which `spend_coin` normalises to 9 silver 5 copper.
                item_name: "Rusty Dagger".into(), price: 5, coin_after: [0, 0, 9, 5],
            }),
        );
        assert!(nav.pending_buy.is_none(), "pending_buy must be consumed exactly once");
    }

    /// CORRELATION: a shop echo for a DIFFERENT slot must NOT resolve this buy — it leaves the parked
    /// buy in place (so the right echo can still resolve it) rather than mis-reporting a stray buy.
    #[tokio::test]
    async fn awaited_buy_ignores_a_shop_echo_for_a_different_slot() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_buy_gs();

        let (tx, mut resp) = tokio::sync::oneshot::channel();
        nav.command.request_buy_await(11, 3, tx);
        nav.drain_merchant(&mut stream, &mut gs);

        nav.fulfill_buy_ok(&gs, &buy_echo(11, 99, 5)); // wrong slot
        assert!(nav.pending_buy.is_some(), "an uncorrelated echo must leave the buy parked");
        assert!(matches!(resp.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
            "no result must be sent for a buy that did not correlate");

        // The correct echo still resolves it.
        nav.fulfill_buy_ok(&gs, &buy_echo(11, 3, 5));
        assert!(matches!(resp.try_recv(), Ok(eqoxide_command::CommandResult::Resolved(_))));
    }

    /// REFUSAL: an OP_ShopEndConfirm while a buy is parked resolves it to `Refused` (a REAL negative
    /// ack → HTTP 409), and clears `pending_buy`.
    #[tokio::test]
    async fn awaited_buy_refused_on_shop_end_confirm() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_buy_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_buy_await(11, 3, tx);
        nav.drain_merchant(&mut stream, &mut gs);

        nav.fulfill_buy_refused();
        assert!(matches!(resp.await.unwrap(), eqoxide_command::CommandResult::Refused(_)));
        assert!(nav.pending_buy.is_none());
    }

    /// INSUFFICIENT-FUNDS SILENCE — THE HONESTY PROOF. The server sends NOTHING on this path, so no
    /// fulfil ever runs: the parked buy stays un-fired and `coin_verified` stays false. The client
    /// therefore CANNOT report success — the only resolution left is the HTTP timeout → `Unconfirmed`
    /// → 202 (asserted in the http::merchant tests). A version that resolved this to `Resolved`/200
    /// would have to have SENT on the Sender here; it provably has not.
    #[tokio::test]
    async fn silent_buy_never_resolves_and_leaves_coin_unverified() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_buy_gs();

        let (tx, mut resp) = tokio::sync::oneshot::channel();
        nav.command.request_buy_await(11, 3, tx);
        nav.drain_merchant(&mut stream, &mut gs);

        // No packet is fed — this is the insufficient-funds silence. The net side must NEVER fabricate
        // a resolution: the Sender stays un-fired and the buy stays parked (only the HTTP timeout / the
        // reaper may resolve it, both to a non-success outcome).
        assert!(nav.pending_buy.is_some(), "a silent buy must remain unresolved, never auto-succeed");
        assert!(matches!(resp.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
            "no CommandResult may be sent on silence — 200/Resolved on a silent buy is the lie A3 forbids");
        assert!(!gs.coin_verified(),
            "an unreconciled in-flight buy must keep coin unverified — the balance is not trustworthy");
    }

    /// **#732 (agent-honesty): `nav_goal` must not survive a zone change.**
    ///
    /// Measured live during the #725 investigation: standing in lfaydark, `GET /v1/observe/debug`
    /// reported `nav_state: "idle"` alongside `nav_goal: [2216.87, 579.17, -113.25]` — the goal from
    /// the zone just left. Each half is individually true and the pair is false: coordinates are a
    /// per-zone namespace and carry no zone tag, so a surviving goal is a confident, well-formed
    /// answer to "where was I going" about a different world.
    ///
    /// Driven through the REAL production hook, not `reset_for_zone_change` directly: the goal is
    /// set by `CommandState::request_goto` (the same call `drain_zone_cross` makes once it resolves
    /// a crossing into a concrete walk), and the zone change is a new `gs.world.zone_name` observed
    /// by `sync_zone_points`, which is what the packet path actually calls.
    ///
    /// The assertions read `nav.nav_state.lock().goal` — the *identical* expression
    /// `eqoxide_http::observe`'s `/debug` handler reads for its `"nav_goal"` field (it locks and
    /// clones this same `Arc`, then serializes `nav.goal` with no filter in between). The JSON end
    /// of that path is pinned separately by
    /// `debug_publishes_no_nav_goal_once_the_goal_is_retired_to_idle_732` in that crate, which
    /// cannot be called from here (eqoxide-http depends on this crate's siblings, not the reverse).
    ///
    /// Mutation checks (both run): delete `*goal = None;` from `NavStatus::retire_to_idle`
    /// (`crates/eqoxide-ipc/src/lib.rs`) → the post-zone `goal` assertion goes RED. Delete
    /// `*tier = None;` from the same function → the `tier` assertion goes RED, which is what pins
    /// the explicit `tier` clear this PR removed from `Walker::reset_for_zone_change` **through the
    /// real zone-change path** (the eqoxide-http test covers `retire_to_idle` called directly).
    #[tokio::test]
    async fn a_zone_change_retires_the_previous_zones_nav_goal_732() {
        // `shared_nav_action_loop` (not `new_loop`) because this test spans the command side and
        // the walker side of the SAME `nav_state`: `new_loop` hands the `CommandState` its own
        // `Default` slots, so a `request_goto` there would never reach `al.nav.nav_state` at all.
        let (mut al, nav, command, _collision, _za) = shared_nav_action_loop();
        let mut gs = GameState::new();
        gs.world.zone_name = "gfaydark".into();
        al.sync_zone_points(&gs); // settle `current_zone` so the next call is a real CHANGE

        // The walker is walking to a goal in gfaydark. `request_goto` is what publishes `nav_goal`
        // — and it is also the call `drain_zone_cross` makes once it resolves a crossing into a
        // concrete walk, which is the path #732 was measured on.
        let goal_id = command.request_goto((2216.0, 579.0, -113.0));
        al.walker.set_nav_state_because("navigating", None);
        // A committed route in the old zone has a clearance tier (#378 Phase 2). Planted AFTER the
        // state transition, because `set_nav_state_because` retires `tier` on a transition.
        // #732 review round 1, B2: without this the `tier` assertion below is satisfied by
        // `NavStatus::default()` before the code under test runs — and it is the ONLY thing pinning
        // the explicit `tier = None` this PR DELETED from `Walker::reset_for_zone_change`.
        nav.nav_state.lock().unwrap().tier = Some("preferred");
        {
            let s = nav.nav_state.lock().unwrap();
            assert_eq!(s.goal, Some([2216.0, 579.0, -113.0]),
                "PREMISE: the field an observer reads really is loaded with this zone's goal");
            assert_eq!(s.state, "navigating",
                "PREMISE: a NON-terminal, non-idle state — otherwise the retirement under test \
                 would not be the thing that clears the goal");
            assert_eq!(s.goal_id, goal_id, "PREMISE: that goal carries the id the accept returned");
            assert_eq!(s.tier, Some("preferred"),
                "PREMISE: a per-route tier really is loaded, so the post-zone `tier` assertion is \
                 load-bearing rather than satisfied by the struct's default");
        }

        // Cross into lfaydark. This is the production entry point: the packet path sets the new
        // zone name on the GameState and `sync_zone_points` notices on its next tick.
        gs.begin_zone_in();
        gs.world.zone_name = "lfaydark".into();
        al.sync_zone_points(&gs);

        let s = nav.nav_state.lock().unwrap().clone();
        assert_eq!(s.goal, None,
            "#732: the previous zone's goal must not be published in the new zone — coordinates \
             carry no zone tag, so a stale one is numerically indistinguishable from a valid one");
        assert_eq!(s.state, "idle", "a zone change ends navigation (#248)");
        assert_eq!(s.reason.as_deref(), Some(eqoxide_nav::walker::NAV_REASON_ZONED),
            "and says WHY, so a successful crossing is distinguishable from a dropped request (#725)");
        assert_eq!(s.tier, None, "the per-route clearance tier described the OLD zone's route");
        assert_eq!(s.goal_id, goal_id,
            "the monotonic identity stamp is deliberately KEPT (#349): it is what lets the caller \
             match this `idle` to the goal it issued. Only the per-goal facts are retired.");
    }

    /// **#766 (agent-honesty), the sibling of the test above: `nav_local` must not survive a zone
    /// change either.** #732 retired `nav_goal` and `nav_tier`; the fine tier's verdict (#382) was
    /// left standing, so the same crossing that produced #732's measured `idle` + stale `nav_goal`
    /// pair could also publish `nav_local: {"state":"no_way_through", …}` beside `nav_state: idle` /
    /// `nav_reason: zoned` — an assertion about a corridor in the zone we have left, computed
    /// against a collision grid that no longer exists.
    ///
    /// Driven through the REAL production hook for the same reason: `sync_zone_points` observing a
    /// new `gs.world.zone_name` is what the packet path calls, and it is the only thing that reaches
    /// `Walker::reset_for_zone_change` in production. `eqoxide-nav`'s
    /// `a_zone_change_retires_the_fine_tiers_verdict_and_its_in_flight_plan_766` pins the walker
    /// call directly (and the `LocalPlanner` cancel, which is not observable from here);
    /// `eqoxide-http`'s `debug_publishes_no_nav_local_once_the_goal_is_retired_to_idle_766` pins the
    /// JSON end. This one pins that the production path in between actually runs it.
    ///
    /// Mutation check: delete `*local = None;` from `NavStatus::retire_to_idle`
    /// (`crates/eqoxide-ipc/src/lib.rs`) → the post-zone `local` assertion goes RED.
    #[tokio::test]
    async fn a_zone_change_retires_the_previous_zones_nav_local_766() {
        let (mut al, nav, command, _collision, _za) = shared_nav_action_loop();
        let mut gs = GameState::new();
        gs.world.zone_name = "gfaydark".into();
        al.sync_zone_points(&gs); // settle `current_zone` so the next call is a real CHANGE

        let _goal_id = command.request_goto((2216.0, 579.0, -113.0));
        al.walker.set_nav_state_because("navigating", None);
        // The fine tier's last word in the OLD zone. Planted after the transition for the same
        // reason #732's `tier` is: otherwise the post-condition is satisfied by the default row.
        // `no_way_through` specifically, because `observe.rs` filters `threaded` out — the
        // unhealthy verdict is the only kind that reaches an agent at all.
        nav.nav_state.lock().unwrap().local = Some(eqoxide_ipc::NavLocal {
            state: "no_way_through".into(), reason: "search_closed".into(),
            stuck_ticks: 7, plan_us: 1234,
        });
        {
            let s = nav.nav_state.lock().unwrap();
            assert_eq!(s.local.as_ref().map(|l| l.state.clone()), Some("no_way_through".to_string()),
                "PREMISE: the field an observer reads really is loaded with the OLD zone's verdict");
            assert_eq!(s.state, "navigating",
                "PREMISE: a NON-terminal, non-idle state — so the retirement under test is what \
                 clears it, not some earlier transition");
        }

        gs.begin_zone_in();
        gs.world.zone_name = "lfaydark".into();
        al.sync_zone_points(&gs);

        let s = nav.nav_state.lock().unwrap().clone();
        assert_eq!(s.state, "idle", "a zone change ends navigation (#248)");
        assert_eq!(s.reason.as_deref(), Some(eqoxide_nav::walker::NAV_REASON_ZONED));
        assert_eq!(s.local, None,
            "#766: the fine tier's verdict is a per-goal fact by the same argument as `nav_goal` \
             and `nav_tier` — when the goal is retired at the zone line, the verdict about \
             threading toward it goes with it");
    }

    /// REAPER: a zone change while a buy is parked fires `Unconfirmed` for the stranded Sender and
    /// clears `pending_buy`, so a shop echo in the NEW zone can't mis-correlate it. Driven through the
    /// real `sync_zone_points` zone-change hook.
    #[tokio::test]
    async fn zone_change_reaps_a_parked_buy_as_unconfirmed() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_buy_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_buy_await(11, 3, tx);
        nav.drain_merchant(&mut stream, &mut gs);
        assert!(nav.pending_buy.is_some());

        // Cross a zone line: sync_zone_points sees a new zone name and runs its reaper.
        gs.world.zone_name = "newzone".into();
        nav.sync_zone_points(&gs);

        assert!(nav.pending_buy.is_none(), "the parked buy must be cleared on a zone change");
        assert_eq!(resp.await.unwrap(), eqoxide_command::CommandResult::Unconfirmed);
    }

    /// SINGLETON-IN-FLIGHT / NO MIS-ATTRIBUTION (#448 review). Two awaited buys of the SAME
    /// merchant+slot, back-to-back before any echo: the SECOND is rejected in-flight (`Refused`, no
    /// wire packets), the FIRST stays parked, and the single OP_ShopPlayerBuy echo resolves the FIRST
    /// — never the second. This is the honesty fix: without serialization the first buy's echo would
    /// resolve the second caller's Sender with the first's receipt (a failed second buy reporting
    /// success). The server-side echo carries no per-request token, so one-at-a-time is the only
    /// unambiguous correlation.
    #[tokio::test]
    async fn a_second_awaited_buy_is_rejected_in_flight_so_the_echo_cannot_be_mis_attributed() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_buy_gs();

        // Buy A parks.
        let (tx_a, resp_a) = tokio::sync::oneshot::channel();
        nav.command.request_buy_await(11, 3, tx_a);
        nav.drain_merchant(&mut stream, &mut gs);
        assert!(nav.pending_buy.is_some(), "buy A must be parked");

        // Buy B arrives while A is still in flight → rejected in-flight, A untouched.
        let (tx_b, resp_b) = tokio::sync::oneshot::channel();
        nav.command.request_buy_await(11, 3, tx_b);
        nav.drain_merchant(&mut stream, &mut gs);
        match resp_b.await.unwrap() {
            eqoxide_command::CommandResult::Refused(reason) =>
                assert!(reason.contains("already in flight"), "B must be told a buy is in flight, got: {reason}"),
            other => panic!("a second in-flight buy must be Refused, not {other:?} (mis-attribution risk)"),
        }
        assert!(nav.pending_buy.is_some(), "buy A must STILL be the parked buy after B is rejected");

        // The single echo resolves A (the parked buy) — proving no mis-attribution onto B.
        let echo = buy_echo(11, 3, 5);
        apply_packet(&mut gs, &AppPacket { opcode: OP_SHOP_PLAYER_BUY, payload: echo.clone() });
        nav.fulfill_buy_ok(&gs, &echo);
        assert!(matches!(resp_a.await.unwrap(), eqoxide_command::CommandResult::Resolved(_)),
            "the echo must resolve the FIRST (parked) buy, not the rejected second one");
        assert!(nav.pending_buy.is_none(), "the parked buy is consumed once");
    }

    // ─────────────────────────────────────────────────────────────────────────────────────────
    // eqoxide#479: the honest awaited merchant-open path. These drive an open through
    // `drain_merchant` (the loop's own `command` slot is shared with the drain, so a real
    // request→take round-trip runs), then feed the resolving echo exactly as `gameplay.rs` does.
    // ─────────────────────────────────────────────────────────────────────────────────────────

    /// A 12+-byte RoF2 MerchantClick_Struct echo: npc_id@0, command@8 (1=open confirmed, 0=refused).
    fn open_echo(npc_id: u32, command: u32) -> Vec<u8> {
        let mut e = vec![0u8; 24];
        e[0..4].copy_from_slice(&npc_id.to_le_bytes());
        e[8..12].copy_from_slice(&command.to_le_bytes());
        e
    }

    fn seed_open_gs() -> GameState {
        let mut gs = GameState::new();
        gs.player_id = 42;
        gs
    }

    /// SUCCESS: an awaited open, driven through the drain and confirmed by the OP_ShopRequest echo
    /// (command=1), resolves to `Resolved(OpenOk)`. `pending_open` is consumed exactly once.
    #[tokio::test]
    async fn awaited_open_resolves_on_the_shop_echo_command_1() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_open_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_open_await(11, tx);
        nav.drain_merchant(&mut stream, &mut gs);
        assert!(nav.pending_open.is_some(), "the awaited open must be parked after the drain");

        let echo = open_echo(11, 1);
        apply_packet(&mut gs, &AppPacket { opcode: OP_SHOP_REQUEST, payload: echo.clone() });
        nav.fulfill_open(&echo);

        assert_eq!(
            resp.await.unwrap(),
            eqoxide_command::CommandResult::Resolved(eqoxide_command::OpenOk { merchant_id: 11 }),
        );
        assert!(nav.pending_open.is_none(), "pending_open must be consumed exactly once");
    }

    /// CORRELATION: a shop-open echo for a DIFFERENT merchant must NOT resolve this open — it leaves
    /// the parked open in place rather than mis-reporting a stray echo.
    #[tokio::test]
    async fn awaited_open_ignores_a_shop_echo_for_a_different_merchant() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_open_gs();

        let (tx, mut resp) = tokio::sync::oneshot::channel();
        nav.command.request_open_await(11, tx);
        nav.drain_merchant(&mut stream, &mut gs);

        nav.fulfill_open(&open_echo(99, 1)); // wrong merchant
        assert!(nav.pending_open.is_some(), "an uncorrelated echo must leave the open parked");
        assert!(matches!(resp.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
            "no result must be sent for an open that did not correlate");

        // The correct echo still resolves it.
        nav.fulfill_open(&open_echo(11, 1));
        assert!(matches!(resp.try_recv(), Ok(eqoxide_command::CommandResult::Resolved(_))));
    }

    /// REFUSAL: an OP_ShopRequest echo with command=0 while an open is parked resolves it to
    /// `Refused` (a REAL negative ack → HTTP 409, covering faction/engaged/feigned/charmed/busy),
    /// and clears `pending_open`.
    #[tokio::test]
    async fn awaited_open_refused_on_shop_echo_command_0() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_open_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_open_await(11, tx);
        nav.drain_merchant(&mut stream, &mut gs);

        let echo = open_echo(11, 0);
        apply_packet(&mut gs, &AppPacket { opcode: OP_SHOP_REQUEST, payload: echo.clone() });
        nav.fulfill_open(&echo);
        assert!(matches!(resp.await.unwrap(), eqoxide_command::CommandResult::Refused(_)));
        assert!(nav.pending_open.is_none());
    }

    /// NON-MERCHANT/OUT-OF-RANGE SILENCE — THE #479 HONESTY PROOF. The server sends NO echo at all on
    /// this path (confirmed against the EQEmu RoF2 source), so no fulfil ever runs: the parked open
    /// stays un-fired. The client therefore CANNOT report success — the only resolution left is the
    /// HTTP timeout → `Unconfirmed` → 202 (asserted in the http::merchant tests). A version that
    /// resolved this to `Resolved`/200 would have to have SENT on the Sender here; it provably has
    /// not. This is the mutation-check boundary: neuter the fix (e.g. resolve on send, or default
    /// to `Resolved` on any non-correlating echo) and this test goes RED.
    #[tokio::test]
    async fn silent_open_never_resolves() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_open_gs();

        let (tx, mut resp) = tokio::sync::oneshot::channel();
        nav.command.request_open_await(11, tx);
        nav.drain_merchant(&mut stream, &mut gs);

        // No packet is fed — this is the non-merchant/out-of-range silence. The net side must NEVER
        // fabricate a resolution: the Sender stays un-fired and the open stays parked (only the HTTP
        // timeout / the reaper may resolve it, both to a non-success outcome).
        assert!(nav.pending_open.is_some(), "a silent open must remain unresolved, never auto-succeed");
        assert!(matches!(resp.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
            "no CommandResult may be sent on silence — 200/Resolved on a silent open is the lie #479 forbids");
    }

    /// REAPER: a zone change while an open is parked fires `Unconfirmed` for the stranded Sender and
    /// clears `pending_open`, so a shop echo in the NEW zone can't mis-correlate it. Driven through
    /// the real `sync_zone_points` zone-change hook.
    #[tokio::test]
    async fn zone_change_reaps_a_parked_open_as_unconfirmed() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_open_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_open_await(11, tx);
        nav.drain_merchant(&mut stream, &mut gs);
        assert!(nav.pending_open.is_some());

        gs.world.zone_name = "newzone".into();
        nav.sync_zone_points(&gs);

        assert!(nav.pending_open.is_none(), "the parked open must be cleared on a zone change");
        assert_eq!(resp.await.unwrap(), eqoxide_command::CommandResult::Unconfirmed);
    }

    /// SINGLETON-IN-FLIGHT / NO MIS-ATTRIBUTION. Two awaited opens of the SAME merchant, back-to-back
    /// before any echo: the SECOND is rejected in-flight (`Refused`, no wire packets), the FIRST
    /// stays parked, and the single OP_ShopRequest echo resolves the FIRST — never the second.
    #[tokio::test]
    async fn a_second_awaited_open_is_rejected_in_flight_so_the_echo_cannot_be_mis_attributed() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_open_gs();

        let (tx_a, resp_a) = tokio::sync::oneshot::channel();
        nav.command.request_open_await(11, tx_a);
        nav.drain_merchant(&mut stream, &mut gs);
        assert!(nav.pending_open.is_some(), "open A must be parked");

        let (tx_b, resp_b) = tokio::sync::oneshot::channel();
        nav.command.request_open_await(11, tx_b);
        nav.drain_merchant(&mut stream, &mut gs);
        match resp_b.await.unwrap() {
            eqoxide_command::CommandResult::Refused(reason) =>
                assert!(reason.contains("already in flight"), "B must be told an open is in flight, got: {reason}"),
            other => panic!("a second in-flight open must be Refused, not {other:?} (mis-attribution risk)"),
        }
        assert!(nav.pending_open.is_some(), "open A must STILL be the parked open after B is rejected");

        let echo = open_echo(11, 1);
        apply_packet(&mut gs, &AppPacket { opcode: OP_SHOP_REQUEST, payload: echo.clone() });
        nav.fulfill_open(&echo);
        assert!(matches!(resp_a.await.unwrap(), eqoxide_command::CommandResult::Resolved(_)),
            "the echo must resolve the FIRST (parked) open, not the rejected second one");
        assert!(nav.pending_open.is_none(), "the parked open is consumed once");
    }

    // ─────────────────────────────────────────────────────────────────────────────────────────
    // A3 Migration 3 (#448): the honest awaited self-cast path. These drive a cast through the real
    // `drain_cast` (the loop's own `command` slot is shared with the drain, so a real request→take
    // round-trip runs), then transition `gs.last_cast` exactly as the cast machinery does and fulfil
    // via `fulfill_cast` — the SAME call `gameplay.rs` makes after `apply_packet`.
    // ─────────────────────────────────────────────────────────────────────────────────────────

    fn seed_cast_gs() -> GameState {
        let mut gs = GameState::new();
        gs.player_id = 42;
        gs.mem_spells = [eqoxide_core::game_state::EMPTY_GEM; 9];
        gs.mem_spells[2] = 202; // gem 2 holds a real spell so the cast STARTS
        gs
    }

    use eqoxide_command::{CastEnd, CommandResult};

    /// SUCCESS: an awaited cast, parked by the drain, then RESOLVED as completed when `last_cast`
    /// transitions → `Resolved(CastEnd{outcome:"completed"})` carrying the real spell. `pending_cast`
    /// is consumed exactly once. The transition — not any single opcode — is what fulfils it.
    #[tokio::test]
    async fn awaited_cast_resolves_completed_on_the_last_cast_transition() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_cast_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx);
        nav.drain_cast(&mut stream, &mut gs);
        assert!(nav.pending_cast.is_some(), "the awaited cast must be parked after the drain");
        // Nothing has resolved yet — fulfil is a no-op while the cast is genuinely in flight.
        nav.fulfill_cast(&gs);
        assert!(nav.pending_cast.is_some(), "a cast in flight must not auto-resolve");

        // The cast completes — the machinery records the outcome into `last_cast` (as OP_MemorizeSpell
        // scribing=3 would), then the gameplay loop calls `fulfill_cast`.
        gs.finish_cast(202, "cast_completed", "You have finished casting Minor Healing.");
        nav.fulfill_cast(&gs);
        match resp.await.unwrap() {
            CommandResult::Resolved(CastEnd { outcome, spell_id, .. }) => {
                assert_eq!(outcome, "completed");
                assert_eq!(spell_id, 202);
            }
            other => panic!("a completed cast must Resolve(completed), got {other:?}"),
        }
        assert!(nav.pending_cast.is_none(), "pending_cast must be consumed exactly once");
    }

    /// A fizzle is STILL a resolved outcome (we know what happened) — `Resolved(CastEnd)` — but its
    /// `outcome` is "fizzled", never "completed". THE INVARIANT: a cast that did not complete-
    /// successfully can never present as completed.
    #[tokio::test]
    async fn awaited_cast_fizzle_resolves_fizzled_never_completed() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_cast_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx);
        nav.drain_cast(&mut stream, &mut gs);
        gs.finish_cast(202, "cast_fizzled", "Your spell fizzles!");
        nav.fulfill_cast(&gs);
        match resp.await.unwrap() {
            CommandResult::Resolved(CastEnd { outcome, .. }) => {
                assert_eq!(outcome, "fizzled", "a fizzle must be reported as fizzled");
                assert_ne!(outcome, "completed", "a fizzle must NEVER report completed");
            }
            other => panic!("a fizzle must Resolve(fizzled), got {other:?}"),
        }
    }

    /// An interrupt resolves as "interrupted", never "completed".
    #[tokio::test]
    async fn awaited_cast_interrupt_resolves_interrupted() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_cast_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx);
        nav.drain_cast(&mut stream, &mut gs);
        gs.finish_cast(202, "cast_interrupted", "Your spell is interrupted.");
        nav.fulfill_cast(&gs);
        assert!(matches!(resp.await.unwrap(),
            CommandResult::Resolved(CastEnd { ref outcome, .. }) if outcome == "interrupted"),
            "an interrupt must Resolve(interrupted)");
    }

    /// NEVER-STARTED: an awaited cast on an EMPTY gem is `Refused` IMMEDIATELY from the drain — the
    /// cast definitively did not happen, so it must not park and must not await. Mutation-check the
    /// honesty: a cast that never started can never 200-as-completed (it 409s at once).
    #[tokio::test]
    async fn awaited_cast_empty_gem_is_refused_immediately_and_never_parks() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_cast_gs(); // gem 5 is empty

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 5, target_id: None, item_slot: None }, tx);
        nav.drain_cast(&mut stream, &mut gs);
        assert!(nav.pending_cast.is_none(), "an empty-gem cast must NOT park — it never started");
        match resp.await.unwrap() {
            CommandResult::Refused(reason) => assert!(reason.contains("empty"), "reason: {reason}"),
            other => panic!("an empty-gem cast must be Refused, not {other:?}"),
        }
    }

    /// SILENT DROP: a parked cast whose server never resolves stays parked — `fulfill_cast` never
    /// invents a success. (The HTTP timeout then yields the honest 202; proven separately.)
    #[tokio::test]
    async fn awaited_cast_silent_stays_parked_never_auto_succeeds() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_cast_gs();

        let (tx, mut resp) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx);
        nav.drain_cast(&mut stream, &mut gs);
        // No outcome recorded. Many fulfil passes must not resolve anything.
        for _ in 0..5 { nav.fulfill_cast(&gs); }
        assert!(nav.pending_cast.is_some(), "a silent cast must remain parked, never auto-succeed");
        assert!(resp.try_recv().is_err(), "no outcome may have been sent for a silent cast");
    }

    /// FALSE-REFUSED GUARD (#448 review, DEFECT 2): a UI fire-and-forget cast that never starts
    /// (empty gem) writes `cast_failed` to `gs.last_cast` CLIENT-SIDE. While an awaited cast is parked,
    /// that stray write must NOT resolve the awaited cast as a bogus `Refused` — the fire-and-forget
    /// never-started `finish_cast` is suppressed while a park exists. The awaited cast stays parked and
    /// only its OWN real outcome resolves it.
    #[tokio::test]
    async fn a_ui_never_started_cast_does_not_resolve_a_parked_awaited_cast() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_cast_gs(); // gem 2 real, gem 5 empty

        // Awaited cast parks (gem 2 starts).
        let (tx, mut resp) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx);
        nav.drain_cast(&mut stream, &mut gs);
        assert!(nav.pending_cast.is_some(), "the awaited cast must be parked");

        // A UI fire-and-forget cast of an EMPTY gem arrives on the same tick's next drain. Its
        // client-side never-started write must be suppressed so it can't cross-talk into the park.
        nav.command.request_cast(eqoxide_ipc::CastRequest { gem: 5, target_id: None, item_slot: None });
        nav.drain_cast(&mut stream, &mut gs);
        nav.fulfill_cast(&gs);
        assert!(nav.pending_cast.is_some(),
            "a UI empty-gem cast must NOT resolve the parked awaited cast");
        assert!(resp.try_recv().is_err(),
            "no bogus Refused may be sent to the awaited cast by the UI never-started write");

        // The awaited cast still resolves normally on its OWN real completion.
        gs.finish_cast(202, "cast_completed", "You have finished casting Minor Healing.");
        nav.fulfill_cast(&gs);
        assert!(matches!(resp.try_recv(), Ok(CommandResult::Resolved(CastEnd { ref outcome, .. })) if outcome == "completed"),
            "the awaited cast must still resolve completed on its own outcome");
        assert!(nav.pending_cast.is_none());
    }

    /// SERVER REFUSAL AFTER PARK: a real cast-start refusal that reaches us after we parked
    /// (`cast_failed` — e.g. insufficient mana, detected server-side) resolves to `Refused` — the cast
    /// definitively did not happen.
    #[tokio::test]
    async fn awaited_cast_server_refusal_after_park_is_refused() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_cast_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx);
        nav.drain_cast(&mut stream, &mut gs);
        gs.finish_cast(0, "cast_failed", "Insufficient Mana to cast this spell!");
        nav.fulfill_cast(&gs);
        assert!(matches!(resp.await.unwrap(), CommandResult::Refused(_)),
            "a server cast_failed after parking must be Refused, never a completed 200");
    }

    /// UNEXPLAINED END: the server ended the cast and never said why (`cast_ended_unexplained`, the
    /// buff-won't-stack case) → `Unconfirmed`. Genuinely unknown whether the spell had an effect;
    /// never a claimed success.
    #[tokio::test]
    async fn awaited_cast_unexplained_end_is_unconfirmed() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_cast_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx);
        nav.drain_cast(&mut stream, &mut gs);
        gs.finish_cast(202, "cast_ended_unexplained", "The cast ended with no outcome reported.");
        nav.fulfill_cast(&gs);
        assert_eq!(resp.await.unwrap(), CommandResult::Unconfirmed,
            "an unexplained end must be Unconfirmed, never a completed 200");
    }

    /// SINGLETON: a second awaited cast while one is parked is `Refused` in-flight, and the first cast
    /// stays parked and resolves normally — its outcome cannot be mis-attributed to the second.
    #[tokio::test]
    async fn a_second_awaited_cast_while_parked_is_refused() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_cast_gs();

        let (tx_a, resp_a) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx_a);
        nav.drain_cast(&mut stream, &mut gs);
        assert!(nav.pending_cast.is_some(), "cast A must be parked");

        let (tx_b, resp_b) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx_b);
        nav.drain_cast(&mut stream, &mut gs);
        match resp_b.await.unwrap() {
            CommandResult::Refused(reason) => assert!(reason.contains("already in flight"), "reason: {reason}"),
            other => panic!("a second in-flight cast must be Refused, not {other:?}"),
        }
        assert!(nav.pending_cast.is_some(), "cast A must STILL be parked after B is rejected");

        gs.finish_cast(202, "cast_completed", "You have finished casting Minor Healing.");
        nav.fulfill_cast(&gs);
        assert!(matches!(resp_a.await.unwrap(), CommandResult::Resolved(_)),
            "the outcome must resolve the FIRST (parked) cast, not the rejected second");
        assert!(nav.pending_cast.is_none());
    }

    /// A STALE prior outcome (recorded BEFORE this cast parked) must never resolve it — the `at >
    /// sent_at` correlation is what keeps a previous cast's verdict from fabricating a result here.
    #[tokio::test]
    async fn awaited_cast_ignores_a_stale_prior_outcome() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_cast_gs();

        // A previous cast already left a completed outcome in `last_cast` BEFORE we park.
        gs.finish_cast(700, "cast_completed", "You have finished casting an earlier spell.");

        let (tx, mut resp) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx);
        nav.drain_cast(&mut stream, &mut gs);
        nav.fulfill_cast(&gs);
        assert!(nav.pending_cast.is_some(), "a stale prior outcome must not resolve the new cast");
        assert!(resp.try_recv().is_err(), "no result may be sent from a stale outcome");
    }

    /// ZONE CHANGE: a parked cast reaped as `Unconfirmed` — the crossing means no `last_cast`
    /// transition can ever come for it, so a prompt honest 202 is the only honest answer.
    #[tokio::test]
    async fn awaited_cast_reaped_unconfirmed_on_zone_change() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_cast_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx);
        nav.drain_cast(&mut stream, &mut gs);
        assert!(nav.pending_cast.is_some());
        nav.reap_pending_cast();
        assert_eq!(resp.await.unwrap(), CommandResult::Unconfirmed,
            "a cast parked across a zone change must be reaped Unconfirmed");
        assert!(nav.pending_cast.is_none());
    }

    // ─────────────────────────────────────────────────────────────────────────────────────────
    // #492: the time-based reap of stranded A3 pending slots (buy / open / cast). The exact
    // Unconfirmed/202 case (server sends NO resolving packet) leaves the parked Sender un-fired; the
    // HTTP timeout returns 202 to the caller but the slot lingers, 409-blocking every later same-type
    // command until a zone change. These drive the REAL reap-then-drain ordering `tick` uses (reap
    // first, then the singleton guard in `drain_*`), backdating `sent_at` to simulate elapsed time
    // (the reap reads wall-clock `Instant::elapsed`, not tokio virtual time). Each test proves BOTH:
    // before the deadline the singleton guard still holds (a second command is 409-rejected — the
    // reaper doesn't fire too early and break the guarantee); after the deadline the stranded slot is
    // reaped and a subsequent command is ADMITTED (parked), never 409. Mutation-check: emptying
    // `reap_expired_pending` (or dropping its `tick` call) turns the "admitted after timeout" halves RED.
    // ─────────────────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn buy_stranded_slot_is_reaped_after_the_deadline_and_a_later_buy_is_admitted() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_buy_gs();

        // Park buy A (the silent/insufficient-funds path: no echo will ever come).
        let (tx_a, _resp_a) = tokio::sync::oneshot::channel();
        nav.command.request_buy_await(11, 3, tx_a);
        nav.drain_merchant(&mut stream, &mut gs);
        assert!(nav.pending_buy.is_some(), "buy A must be parked");

        // BEFORE the deadline: reap is a no-op, so buy B is still correctly 409-rejected in-flight.
        nav.pending_buy.as_mut().unwrap().sent_at = Instant::now() - (SHOP_PENDING_REAP - Duration::from_secs(1));
        nav.reap_expired_pending();
        assert!(nav.pending_buy.is_some(), "an un-expired buy must NOT be reaped — the singleton guard still holds");
        let (tx_b, resp_b) = tokio::sync::oneshot::channel();
        nav.command.request_buy_await(11, 3, tx_b);
        nav.drain_merchant(&mut stream, &mut gs);
        match resp_b.await.unwrap() {
            CommandResult::Refused(reason) => assert!(reason.contains("already in flight"),
                "before the deadline B must be told a buy is in flight, got: {reason}"),
            other => panic!("before the deadline a second buy must be Refused, got {other:?}"),
        }
        assert!(nav.pending_buy.is_some(), "buy A must still be parked before the deadline");

        // AFTER the deadline: the stranded slot is reaped, so buy C is ADMITTED (parked), not 409.
        nav.pending_buy.as_mut().unwrap().sent_at = Instant::now() - (SHOP_PENDING_REAP + Duration::from_secs(1));
        nav.reap_expired_pending();
        assert!(nav.pending_buy.is_none(), "an expired stranded buy must be reaped");
        let (tx_c, mut resp_c) = tokio::sync::oneshot::channel();
        nav.command.request_buy_await(11, 3, tx_c);
        nav.drain_merchant(&mut stream, &mut gs);
        assert!(nav.pending_buy.is_some(), "buy C must be ADMITTED (parked) once the stranded slot is reaped");
        assert!(resp_c.try_recv().is_err(), "C must be in flight (awaiting its echo), not 409-refused");
    }

    #[tokio::test]
    async fn open_stranded_slot_is_reaped_after_the_deadline_and_a_later_open_is_admitted() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_open_gs();

        // Park open A (the non-merchant/out-of-range path: NO echo of any kind ever arrives).
        let (tx_a, _resp_a) = tokio::sync::oneshot::channel();
        nav.command.request_open_await(11, tx_a);
        nav.drain_merchant(&mut stream, &mut gs);
        assert!(nav.pending_open.is_some(), "open A must be parked");

        // BEFORE the deadline: open B is still correctly 409-rejected in-flight.
        nav.pending_open.as_mut().unwrap().sent_at = Instant::now() - (SHOP_PENDING_REAP - Duration::from_secs(1));
        nav.reap_expired_pending();
        assert!(nav.pending_open.is_some(), "an un-expired open must NOT be reaped");
        let (tx_b, resp_b) = tokio::sync::oneshot::channel();
        nav.command.request_open_await(11, tx_b);
        nav.drain_merchant(&mut stream, &mut gs);
        match resp_b.await.unwrap() {
            CommandResult::Refused(reason) => assert!(reason.contains("already in flight"),
                "before the deadline B must be told an open is in flight, got: {reason}"),
            other => panic!("before the deadline a second open must be Refused, got {other:?}"),
        }
        assert!(nav.pending_open.is_some(), "open A must still be parked before the deadline");

        // AFTER the deadline: reaped, so open C is ADMITTED (parked), not 409.
        nav.pending_open.as_mut().unwrap().sent_at = Instant::now() - (SHOP_PENDING_REAP + Duration::from_secs(1));
        nav.reap_expired_pending();
        assert!(nav.pending_open.is_none(), "an expired stranded open must be reaped");
        let (tx_c, mut resp_c) = tokio::sync::oneshot::channel();
        nav.command.request_open_await(11, tx_c);
        nav.drain_merchant(&mut stream, &mut gs);
        assert!(nav.pending_open.is_some(), "open C must be ADMITTED once the stranded slot is reaped");
        assert!(resp_c.try_recv().is_err(), "C must be in flight, not 409-refused");
    }

    #[tokio::test]
    async fn cast_stranded_slot_is_reaped_after_the_deadline_and_a_later_cast_is_admitted() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_cast_gs();

        // Park cast A (the silently-dropped path: no `last_cast` transition ever comes).
        let (tx_a, _resp_a) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx_a);
        nav.drain_cast(&mut stream, &mut gs);
        assert!(nav.pending_cast.is_some(), "cast A must be parked");

        // BEFORE the deadline (cast uses the longer CAST_PENDING_REAP): cast B is still 409-rejected.
        nav.pending_cast.as_mut().unwrap().sent_at = Instant::now() - (CAST_PENDING_REAP - Duration::from_secs(1));
        nav.reap_expired_pending();
        assert!(nav.pending_cast.is_some(), "an un-expired cast must NOT be reaped");
        let (tx_b, resp_b) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx_b);
        nav.drain_cast(&mut stream, &mut gs);
        match resp_b.await.unwrap() {
            CommandResult::Refused(reason) => assert!(reason.contains("already in flight"),
                "before the deadline B must be told a cast is in flight, got: {reason}"),
            other => panic!("before the deadline a second cast must be Refused, got {other:?}"),
        }
        assert!(nav.pending_cast.is_some(), "cast A must still be parked before the deadline");

        // AFTER the deadline: reaped, so cast C is ADMITTED (parked), not 409.
        nav.pending_cast.as_mut().unwrap().sent_at = Instant::now() - (CAST_PENDING_REAP + Duration::from_secs(1));
        nav.reap_expired_pending();
        assert!(nav.pending_cast.is_none(), "an expired stranded cast must be reaped");
        let (tx_c, mut resp_c) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx_c);
        nav.drain_cast(&mut stream, &mut gs);
        assert!(nav.pending_cast.is_some(), "cast C must be ADMITTED once the stranded slot is reaped");
        assert!(resp_c.try_recv().is_err(), "C must be in flight, not 409-refused");
    }

    // ─────────────────────────────────────────────────────────────────────────────────────────
    // #492/#475 QUARANTINE — the honesty half of the reap. The wire echoes carry no per-request
    // token, so once the time-based reap admits a same-key command, a DELAYED echo of the reaped
    // command (the transport delivers up to the ~30s resend_timeout) must NOT be mis-credited to the
    // re-admitted command — that would be a silent wrong `Resolved` (worse than the 409 it replaced).
    // These prove the stale echo is DROPPED and the re-admitted command is never falsely credited.
    // Mutation-check: deleting the `*_quarantined` guard in the matching `fulfill_*` turns each RED.
    // ─────────────────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_stale_buy_echo_after_reap_is_not_mis_credited_to_a_readmitted_same_key_buy() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_buy_gs();

        // Buy A (merchant 11, slot 3) parks, then is time-reaped past the deadline.
        let (tx_a, mut resp_a) = tokio::sync::oneshot::channel();
        nav.command.request_buy_await(11, 3, tx_a);
        nav.drain_merchant(&mut stream, &mut gs);
        nav.pending_buy.as_mut().unwrap().sent_at = Instant::now() - (SHOP_PENDING_REAP + Duration::from_secs(1));
        nav.reap_expired_pending();
        assert!(nav.pending_buy.is_none(), "A must be reaped");
        assert!(matches!(resp_a.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Closed)),
            "A's Sender is dropped on reap (its caller already got a 202)");

        // Buy B — SAME merchant+slot — is admitted (liveness fix) and parks.
        let (tx_b, mut resp_b) = tokio::sync::oneshot::channel();
        nav.command.request_buy_await(11, 3, tx_b);
        nav.drain_merchant(&mut stream, &mut gs);
        assert!(nav.pending_buy.is_some(), "B must be admitted, not 409-blocked by A");

        // A's DELAYED real echo now lands. It must be DROPPED (quarantined key), NOT credited to B.
        let echo = buy_echo(11, 3, 5);
        apply_packet(&mut gs, &AppPacket { opcode: OP_SHOP_PLAYER_BUY, payload: echo.clone() });
        nav.fulfill_buy_ok(&gs, &echo);
        assert!(matches!(resp_b.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
            "B must NOT be credited with A's stale echo — that would be a silent wrong Resolved");
        assert!(nav.pending_buy.is_some(), "the stale echo is dropped, leaving B still parked");

        // B (ambiguous same key during the window) can only reap to an honest Unconfirmed — never a
        // fabricated success from a stale echo.
        nav.pending_buy.as_mut().unwrap().sent_at = Instant::now() - (SHOP_PENDING_REAP + Duration::from_secs(1));
        nav.reap_expired_pending();
        assert!(nav.pending_buy.is_none(), "B reaps rather than resolving on an ambiguous echo");
        assert!(matches!(resp_b.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Closed)),
            "B ends Unconfirmed/202 (Sender dropped), never a wrong 200");
    }

    #[tokio::test]
    async fn a_stale_open_echo_after_reap_is_not_mis_credited_to_a_readmitted_same_merchant_open() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_open_gs();

        // Open A (merchant 11) parks, then is time-reaped.
        let (tx_a, _resp_a) = tokio::sync::oneshot::channel();
        nav.command.request_open_await(11, tx_a);
        nav.drain_merchant(&mut stream, &mut gs);
        nav.pending_open.as_mut().unwrap().sent_at = Instant::now() - (SHOP_PENDING_REAP + Duration::from_secs(1));
        nav.reap_expired_pending();
        assert!(nav.pending_open.is_none(), "A must be reaped");

        // Open B — SAME merchant — is admitted and parks.
        let (tx_b, mut resp_b) = tokio::sync::oneshot::channel();
        nav.command.request_open_await(11, tx_b);
        nav.drain_merchant(&mut stream, &mut gs);
        assert!(nav.pending_open.is_some(), "B must be admitted");

        // A's delayed OP_ShopRequest echo (command=1, a REAL "opened" ack) lands — must be DROPPED,
        // never credited to B as a Resolved(OpenOk).
        nav.fulfill_open(&open_echo(11, 1));
        assert!(matches!(resp_b.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
            "B must NOT be credited with A's stale open echo");
        assert!(nav.pending_open.is_some(), "the stale echo is dropped, leaving B still parked");
    }

    #[tokio::test]
    async fn a_cast_outcome_after_reap_is_not_mis_credited_to_a_readmitted_cast() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_cast_gs();

        // Cast A parks, then is time-reaped (cast has NO content key).
        let (tx_a, _resp_a) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx_a);
        nav.drain_cast(&mut stream, &mut gs);
        nav.pending_cast.as_mut().unwrap().sent_at = Instant::now() - (CAST_PENDING_REAP + Duration::from_secs(1));
        nav.reap_expired_pending();
        assert!(nav.pending_cast.is_none(), "A must be reaped");

        // Cast B is admitted and parks.
        let (tx_b, mut resp_b) = tokio::sync::oneshot::channel();
        nav.command.request_cast_await(eqoxide_ipc::CastRequest { gem: 2, target_id: None, item_slot: None }, tx_b);
        nav.drain_cast(&mut stream, &mut gs);
        assert!(nav.pending_cast.is_some(), "B must be admitted");

        // A cast outcome now transitions `last_cast` — during the cast quarantine this must NOT be
        // credited to B (it could be A's reaped cast finally landing). B stays parked; no wrong Resolved.
        gs.finish_cast(202, "cast_completed", "You have finished casting.");
        nav.fulfill_cast(&gs);
        assert!(matches!(resp_b.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
            "B must NOT be credited with a cast outcome during the post-reap quarantine");
        assert!(nav.pending_cast.is_some(), "B stays parked — the ambiguous outcome is suppressed");
    }

    // ─────────────────────────────────────────────────────────────────────────────────────────
    // A3 Migration 2 (#448): the honest awaited quest-turn-in (give) path. These drive a give
    // through the real `tick_give` state machine (the loop's own `command` slot is shared with the
    // drain, so a real request→take round-trip runs), advance it with `gs.trade_ack_ready`, and feed
    // OP_FinishTrade exactly as `gameplay.rs` does (via `note_finish_trade`), then tick through the
    // #486 settle window so the deferred verify-transfer verdict runs.
    // ─────────────────────────────────────────────────────────────────────────────────────────

    /// item_id of the seeded turn-in item. The verify-transfer verdict keys on item_id (#486 review),
    /// so the seed MUST carry a nonzero id — a zero id reads as "unidentifiable" (mirror desync) and
    /// would resolve `Unconfirmed`, which is exactly the Finding-1 case tested separately below.
    const BONE_CHIPS_ID: u32 = 13073;

    fn seed_give_gs() -> GameState {
        let mut gs = GameState::new();
        gs.player_id = 42;
        gs.inventory.push(eqoxide_core::game_state::InvItem {
            slot: 23, item_id: BONE_CHIPS_ID, name: "Bone Chips".into(), ..Default::default()
        });
        gs
    }

    /// SUCCESS: an awaited give, driven through `tick_give`, acked, then confirmed by OP_FinishTrade,
    /// resolves to `Resolved(GiveOk)` carrying the NPC id and the item name captured at send time.
    /// `give_state` is consumed exactly once.
    #[tokio::test]
    async fn awaited_give_resolves_on_finish_trade_with_the_receipt() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_give_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_give_await(11, 23, tx);
        // Tick 1: begin the trade (OP_TradeRequest sent, phase 1 parked awaiting the ack).
        nav.tick_give(&mut stream, &mut gs);
        assert!(nav.give_state.is_some(), "the awaited give must be parked after begin");

        // The server acks the trade session; tick 2 sends the accept and enters phase 2 (still parked).
        gs.trade_ack_ready = true;
        nav.tick_give(&mut stream, &mut gs);
        assert!(nav.give_state.is_some(), "an awaited give must stay parked through phase 2 (await finish)");

        // OP_FinishTrade arrives — apply it (clears trade slots) THEN note it, exactly as the gameplay
        // loop does after `apply_packet`. The NPC ACCEPTED the item, so nothing is returned: the item is
        // GONE from the mirror (the trade slot was cleared and no return-item packet follows).
        apply_packet(&mut gs, &AppPacket { opcode: OP_FINISH_TRADE, payload: vec![] });
        nav.note_finish_trade();
        // #486: the verdict is DEFERRED across the returned-item watch window. Tick through it; the item
        // never comes back, so the give resolves `Resolved(GiveOk)` — an honest confirmed transfer.
        for _ in 0..GIVE_FINISH_SETTLE_TICKS {
            assert!(nav.give_state.is_some(), "the give must stay parked through the settle window");
            nav.tick_give(&mut stream, &mut gs);
        }

        assert_eq!(
            resp.await.unwrap(),
            eqoxide_command::CommandResult::Resolved(eqoxide_command::GiveOk {
                npc_id: 11, item_name: "Bone Chips".into(),
            }),
        );
        assert!(nav.give_state.is_none(), "give_state must be consumed exactly once");
    }

    /// #486 — VERIFY-TRANSFER, THE HONESTY PROOF FOR A RETURNED ITEM. A give to a rejecting /
    /// out-of-range NPC: the server STILL sends OP_FinishTrade (it only means the trade SESSION ended)
    /// but then RETURNS the item to the CURSOR (slot 33, EQEmu PushItemOnCursor) via a SEPARATE
    /// OP_ItemPacket sent STRICTLY AFTER the finish. The give machine must NOT trust the finish — it
    /// checks whether the captured item_id came back on the cursor and, finding it there, resolves
    /// `Unconfirmed` (→ 202), NEVER `Resolved`/200.
    ///
    /// MUTATION CHECK: the pre-#486 code resolved `Resolved(GiveOk)` on ANY OP_FinishTrade. Under that
    /// behavior this test goes RED — the returned item would be reported as a successful "given".
    #[tokio::test]
    async fn awaited_give_finish_but_item_returned_is_unconfirmed_never_success() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_give_gs();

        let (tx, mut resp) = tokio::sync::oneshot::channel();
        nav.command.request_give_await(11, 23, tx);
        nav.tick_give(&mut stream, &mut gs);          // begin (phase 1): item 23 → cursor 33, OP_TradeRequest
        gs.trade_ack_ready = true;
        nav.tick_give(&mut stream, &mut gs);          // ack → cursor 33 → trade slot 3000, accept, phase 2
        assert!(nav.give_state.is_some());

        // Server sequence for a NON-accepted turn-in (EQEmu client_packet.cpp:15488 then
        // FinishTrade→PushItemOnCursor): OP_FinishTrade FIRST (clears the trade slot), then the item is
        // RETURNED to the cursor via OP_ItemPacket. Replay that exact ORDER.
        apply_packet(&mut gs, &AppPacket { opcode: OP_FINISH_TRADE, payload: vec![] });
        nav.note_finish_trade();
        // The returned item lands on the CURSOR (slot 33) under its REAL item_id, AFTER the finish — the
        // crux of the bug. The verdict keys on cursor+item_id, so this is the positive "returned" signal.
        gs.inventory.push(eqoxide_core::game_state::InvItem {
            slot: SLOT_CURSOR as i32, item_id: BONE_CHIPS_ID, name: "Bone Chips".into(), ..Default::default()
        });

        // Tick through the settle window; the verify sees the item on the cursor → Unconfirmed, not success.
        for _ in 0..GIVE_FINISH_SETTLE_TICKS {
            assert!(matches!(resp.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
                "the give must not resolve before the returned-item watch window elapses");
            nav.tick_give(&mut stream, &mut gs);
        }
        assert_eq!(resp.try_recv(), Ok(eqoxide_command::CommandResult::Unconfirmed),
            "a give whose item was RETURNED to the cursor is NOT a success — never a 200");
        assert!(nav.give_state.is_none(), "give_state must be consumed exactly once");
    }

    /// #486 (review, Finding 2) — NO FALSE-202 FROM A DUPLICATE ELSEWHERE. A SUCCESSFUL give while a
    /// DUPLICATE of the same item (same item_id, e.g. a spare stack of reagents) sits in a GENERAL slot:
    /// the NPC accepts the handed item (nothing returns to the cursor), so the give must resolve
    /// `Resolved`/200 even though a copy still sits in the pack. The old all-slot name-scan resolved
    /// `Unconfirmed` here — a false-202 an agent treats as "retry", handing the item over TWICE
    /// (double turn-in / item loss). Keying the verdict on cursor+item_id fixes it.
    ///
    /// MUTATION CHECK: revert the verdict to the all-slot name-scan (`any(slot < TRADE_BEGIN && name ==
    /// item_name)`) and this goes RED — the duplicate in the general slot forces a bogus `Unconfirmed`.
    #[tokio::test]
    async fn awaited_give_success_with_a_duplicate_in_a_bag_slot_still_resolves_200() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_give_gs();
        // A pre-existing DUPLICATE of the SAME item (same id + name) in another general slot. It is NOT
        // on the cursor, so it has nothing to do with whether THIS give transferred.
        gs.inventory.push(eqoxide_core::game_state::InvItem {
            slot: 24, item_id: BONE_CHIPS_ID, name: "Bone Chips".into(), ..Default::default()
        });

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_give_await(11, 23, tx);
        nav.tick_give(&mut stream, &mut gs);          // begin: slot 23 → cursor, OP_TradeRequest
        gs.trade_ack_ready = true;
        nav.tick_give(&mut stream, &mut gs);          // ack → cursor → trade slot 3000, accept, phase 2

        // NPC ACCEPTS: OP_FinishTrade clears the trade slot and NOTHING returns to the cursor. The
        // duplicate at slot 24 stays put. The cursor holds no Bone Chips → the give transferred.
        apply_packet(&mut gs, &AppPacket { opcode: OP_FINISH_TRADE, payload: vec![] });
        nav.note_finish_trade();
        for _ in 0..GIVE_FINISH_SETTLE_TICKS { nav.tick_give(&mut stream, &mut gs); }

        assert_eq!(
            resp.await.unwrap(),
            eqoxide_command::CommandResult::Resolved(eqoxide_command::GiveOk {
                npc_id: 11, item_name: "Bone Chips".into(),
            }),
            "a real success must be 200 even with a same-item duplicate elsewhere in the pack — no false 202",
        );
        // The duplicate is untouched.
        assert!(gs.inventory.iter().any(|i| i.slot == 24 && i.item_id == BONE_CHIPS_ID));
        assert!(nav.give_state.is_none());
    }

    /// #486 (review, Finding 1) — NO FALSE-200 WHEN THE ITEM CAN'T BE IDENTIFIED AT SEND TIME. If the
    /// inventory mirror is desynced (a documented #275 condition) the give slot holds no known item, so
    /// item_id can't be captured. The give still goes out on the wire (the server has the real item),
    /// and a rejecting NPC returns it to the cursor. With the OLD name-scan the captured name fell back
    /// to a synthetic "item in slot N" that the real returned item never matches → a fabricated 200.
    /// The fix: an unidentifiable give can NEVER be a confident success → `Unconfirmed`.
    ///
    /// MUTATION CHECK: map the `None` (unidentifiable) verdict arm to `true` (confirmed) and this goes
    /// RED — the unidentifiable, actually-returned give would be reported as a successful "given".
    #[tokio::test]
    async fn awaited_give_unidentifiable_at_send_is_unconfirmed_never_false_200() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_give_gs();

        // Give from slot 5, which is EMPTY in the mirror (desync): item_id cannot be captured → None.
        let (tx, mut resp) = tokio::sync::oneshot::channel();
        nav.command.request_give_await(11, 5, tx);
        nav.tick_give(&mut stream, &mut gs);          // begin (phase 1) — nothing at slot 5 in the mirror
        assert!(nav.give_state.is_some());
        gs.trade_ack_ready = true;
        nav.tick_give(&mut stream, &mut gs);          // ack → accept, phase 2

        // The NPC returns the item to the cursor under its REAL id/name (which the mirror only learns
        // now, from the server) — exactly the case the old name-fallback would have mis-read as success.
        apply_packet(&mut gs, &AppPacket { opcode: OP_FINISH_TRADE, payload: vec![] });
        nav.note_finish_trade();
        gs.inventory.push(eqoxide_core::game_state::InvItem {
            slot: SLOT_CURSOR as i32, item_id: BONE_CHIPS_ID, name: "Bone Chips".into(), ..Default::default()
        });
        for _ in 0..GIVE_FINISH_SETTLE_TICKS { nav.tick_give(&mut stream, &mut gs); }

        assert_eq!(resp.try_recv(), Ok(eqoxide_command::CommandResult::Unconfirmed),
            "an unidentifiable give (mirror desync at send) can never be a confident 200 — honest 202");
        assert!(nav.give_state.is_none());
    }

    /// NO-ACK SILENCE — THE HONESTY PROOF (net side). The NPC never acks: `tick_give`'s phase-1 abort
    /// fires after `GIVE_ACK_TIMEOUT_TICKS`, resolving the awaited give to `Unconfirmed` — NEVER
    /// success — and clears the state. Proves the NET verdict is delivered (which the HTTP side, whose
    /// timeout is strictly longer, maps to 202).
    #[tokio::test]
    async fn awaited_give_with_no_ack_times_out_to_unconfirmed_never_success() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_give_gs();

        let (tx, mut resp) = tokio::sync::oneshot::channel();
        nav.command.request_give_await(11, 23, tx);
        nav.tick_give(&mut stream, &mut gs); // begin
        // No ack ever arrives. Tick until the phase-1 abort fires.
        for _ in 0..GIVE_ACK_TIMEOUT_TICKS {
            assert!(matches!(resp.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
                "the give must not resolve before the timeout — never a fabricated success");
            nav.tick_give(&mut stream, &mut gs);
        }
        assert_eq!(resp.try_recv(), Ok(eqoxide_command::CommandResult::Unconfirmed),
            "a give that was never acked is honestly UNKNOWN, never success");
        assert!(nav.give_state.is_none(), "the aborted give must clear its state");
    }

    /// ITEM-MISMATCH: the NPC acks and we send the accept, but the item doesn't match the turn-in, so
    /// the server returns it on the cursor with NO OP_FinishTrade. `tick_give`'s phase-2 abort fires
    /// after `GIVE_FINISH_TIMEOUT_TICKS` → the honest `Unconfirmed` (never a claimed success or an
    /// unprovable `Refused`).
    #[tokio::test]
    async fn awaited_give_item_mismatch_no_finish_is_unconfirmed() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_give_gs();

        let (tx, mut resp) = tokio::sync::oneshot::channel();
        nav.command.request_give_await(11, 23, tx);
        nav.tick_give(&mut stream, &mut gs);          // begin (phase 1)
        gs.trade_ack_ready = true;
        nav.tick_give(&mut stream, &mut gs);          // ack → accept sent, enter phase 2
        assert!(nav.give_state.is_some());
        // No OP_FinishTrade (item mismatch). Tick until the phase-2 abort fires.
        for _ in 0..GIVE_FINISH_TIMEOUT_TICKS {
            assert!(matches!(resp.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
                "a give awaiting finish must not resolve early");
            nav.tick_give(&mut stream, &mut gs);
        }
        assert_eq!(resp.try_recv(), Ok(eqoxide_command::CommandResult::Unconfirmed),
            "an item-mismatch turn-in (no OP_FinishTrade) is honestly UNKNOWN, not success");
        assert!(nav.give_state.is_none());
    }

    /// #480: the PHASE-2 finish-timeout sends OP_CancelTrade to end the trade session server-side
    /// (shrinking the late-OP_FinishTrade misattribution window), WITHOUT fabricating an outcome — the
    /// give still resolves the honest `Unconfirmed`. Two independent asserts guard against opposite
    /// regressions: (1) an 8-byte OP_CancelTrade (0x354c) reaches the wire — mutation-check: deleting
    /// the `send_app_packet(OP_CANCEL_TRADE, ..)` fails this; a 0-byte send (the old phase-1 bug the
    /// server drops on a size check) fails the length assert; (2) the result is `Unconfirmed`, NOT a
    /// fabricated `Resolved`/`Refused` — the cancel doesn't retroactively decide the unknown outcome.
    #[tokio::test]
    async fn awaited_give_phase2_timeout_cancels_trade_and_is_unconfirmed() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_give_gs(); // player_id = 42

        let (tx, mut resp) = tokio::sync::oneshot::channel();
        nav.command.request_give_await(11, 23, tx);
        nav.tick_give(&mut stream, &mut gs);          // begin (phase 1)
        gs.trade_ack_ready = true;
        nav.tick_give(&mut stream, &mut gs);          // ack → accept sent, enter phase 2
        assert!(nav.give_state.is_some());

        // No OP_FinishTrade ever arrives. Tick until the phase-2 abort fires.
        for _ in 0..GIVE_FINISH_TIMEOUT_TICKS {
            assert!(matches!(resp.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
                "a give awaiting finish must not resolve before the timeout");
            nav.tick_give(&mut stream, &mut gs);
        }

        // (2) Honest outcome: still UNKNOWN — the cancel does not manufacture a success or a refusal.
        assert_eq!(resp.try_recv(), Ok(eqoxide_command::CommandResult::Unconfirmed),
            "a phase-2 finish-timeout is honestly UNKNOWN even though we cancel the trade");
        assert!(nav.give_state.is_none(), "the timed-out give must clear its state");

        // (1) The trade session was ended server-side: an 8-byte OP_CancelTrade reached the wire.
        let cancels: Vec<_> = stream.sent_app_packets().into_iter()
            .filter(|(op, _)| *op == OP_CANCEL_TRADE).collect();
        assert_eq!(cancels.len(), 1,
            "the phase-2 finish-timeout must send exactly one OP_CancelTrade to end the session (#480)");
        assert_eq!(cancels[0].1.len(), 8,
            "OP_CancelTrade must carry an 8-byte CancelTrade_Struct — the server DROPS any other size");
        assert_eq!(u32::from_le_bytes(cancels[0].1[0..4].try_into().unwrap()), gs.player_id,
            "fromid must be our player id");
    }

    /// SINGLETON-IN-FLIGHT: a second awaited give while one is in flight is rejected `Refused` with no
    /// new trade started, and the FIRST give is untouched (still parked, still resolvable). Mirrors the
    /// buy discipline — OP_FinishTrade carries no per-request token, so one-at-a-time is the only
    /// unambiguous correlation.
    #[tokio::test]
    async fn a_second_awaited_give_is_rejected_in_flight_and_leaves_the_first() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_give_gs();

        // Give A parks.
        let (tx_a, resp_a) = tokio::sync::oneshot::channel();
        nav.command.request_give_await(11, 23, tx_a);
        nav.tick_give(&mut stream, &mut gs);
        assert!(nav.give_state.is_some(), "give A must be parked");

        // Give B arrives while A is in flight → rejected in-flight, A untouched.
        let (tx_b, resp_b) = tokio::sync::oneshot::channel();
        nav.command.request_give_await(11, 23, tx_b);
        nav.tick_give(&mut stream, &mut gs);
        match resp_b.await.unwrap() {
            eqoxide_command::CommandResult::Refused(reason) =>
                assert!(reason.contains("already in flight"), "B must be told a give is in flight, got: {reason}"),
            other => panic!("a second in-flight give must be Refused, not {other:?}"),
        }
        assert!(nav.give_state.is_some(), "give A must STILL be parked after B is rejected");

        // A still resolves normally on its own OP_FinishTrade.
        gs.trade_ack_ready = true;
        nav.tick_give(&mut stream, &mut gs); // accept → phase 2
        apply_packet(&mut gs, &AppPacket { opcode: OP_FINISH_TRADE, payload: vec![] });
        nav.note_finish_trade();
        for _ in 0..GIVE_FINISH_SETTLE_TICKS { nav.tick_give(&mut stream, &mut gs); } // #486 settle
        assert!(matches!(resp_a.await.unwrap(), eqoxide_command::CommandResult::Resolved(_)),
            "the finish must resolve the FIRST (parked) give, not the rejected second one");
    }

    /// REAPER: a zone change while an awaited give is parked fires `Unconfirmed` for the stranded
    /// Sender and clears `give_state`, so a stray OP_FinishTrade in the NEW zone can't mis-resolve it.
    /// Driven through the real `sync_zone_points` zone-change hook.
    #[tokio::test]
    async fn zone_change_reaps_a_parked_give_as_unconfirmed() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_give_gs();

        let (tx, resp) = tokio::sync::oneshot::channel();
        nav.command.request_give_await(11, 23, tx);
        nav.tick_give(&mut stream, &mut gs);
        assert!(nav.give_state.is_some());

        gs.world.zone_name = "newzone".into();
        nav.sync_zone_points(&gs);

        assert!(nav.give_state.is_none(), "the parked give must be cleared on a zone change");
        assert_eq!(resp.await.unwrap(), eqoxide_command::CommandResult::Unconfirmed);
    }

    /// THE TWO-TIMEOUT ORDERING LANDMINE (#448): the net-side worst-case verdict time (phase 1 + phase
    /// 2 run in SEQUENCE) must be strictly LESS than the HTTP-side await budget, so the NET verdict
    /// (Resolved/Unconfirmed) is what the caller receives, never a vaguer HTTP-elapsed 202. This pins
    /// that relationship in the constants themselves.
    #[test]
    fn net_give_timeout_is_shorter_than_the_http_await_budget() {
        let net_ms = (GIVE_ACK_TIMEOUT_TICKS + GIVE_FINISH_TIMEOUT_TICKS) as u128 * NAV_TICK_MS;
        // Reference the real HTTP constant (not a magic literal) so a future edit to either side is
        // caught here (#475 review nit).
        let http_ms = eqoxide_http::interact::GIVE_HTTP_TIMEOUT_SECS as u128 * 1000;
        assert!(net_ms < http_ms,
            "net worst-case verdict ({net_ms}ms) must land before the HTTP timeout ({http_ms}ms) so the \
             net verdict wins — otherwise the caller gets a vague HTTP-elapsed 202 instead of the real outcome");
    }

    /// MISATTRIBUTION REPRO / THE #475 HONESTY FIX. OP_FinishTrade is a 0-byte packet with no trade id,
    /// so it can only be matched to a give if gives are SERIALIZED. Pre-fix, a fire-and-forget give
    /// cleared `give_state` at accept, so a later AWAITED give could reach phase 2 and a LATE finish
    /// from the first give would resolve the SECOND caller's Sender — a fabricated `Resolved`/200. The
    /// fix holds the machine through the finish for BOTH paths: a give in flight blocks any new give.
    /// This pins that a second give is REJECTED, never entering the machine, so the late finish resolves
    /// only the ORIGINAL give. Mutation-check: revert to clear-at-accept and D is no longer rejected
    /// (it begins instead) → the `try_recv` Refused assertion goes RED.
    #[tokio::test]
    async fn a_late_finish_cannot_misattribute_to_a_second_give() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_give_gs();

        // Fire-and-forget give to NPC C (11) begins and reaches phase 2 (accept sent) — HELD in flight.
        nav.command.request_give(11, 23);
        nav.tick_give(&mut stream, &mut gs);         // begin C (phase 1)
        gs.trade_ack_ready = true;
        nav.tick_give(&mut stream, &mut gs);         // ack → accept; C now phase 2, HELD (the fix)
        assert!(nav.give_state.is_some(),
            "a fire-and-forget give must stay in flight through the finish — serialized, not cleared at accept");

        // An AWAITED give to NPC D (22) arrives while C is still in flight → REJECTED synchronously; D
        // never enters the machine, so C's finish cannot land on D.
        let (tx_d, mut resp_d) = tokio::sync::oneshot::channel();
        nav.command.request_give_await(22, 23, tx_d);
        nav.tick_give(&mut stream, &mut gs);
        match resp_d.try_recv() {
            Ok(eqoxide_command::CommandResult::Refused(_)) => {}
            other => panic!("D must be synchronously Refused while C is in flight (else C's finish \
                             misattributes to D) — got {other:?}"),
        }
        assert!(nav.give_state.is_some(), "C must STILL be the one parked give after D is rejected");

        // C's late OP_FinishTrade lands → it consumes C (silently — no await_tx), and there is no D in
        // the machine to wrongly resolve. No fabricated 200 for D.
        apply_packet(&mut gs, &AppPacket { opcode: OP_FINISH_TRADE, payload: vec![] });
        nav.note_finish_trade();
        for _ in 0..GIVE_FINISH_SETTLE_TICKS { nav.tick_give(&mut stream, &mut gs); } // #486 settle
        assert!(nav.give_state.is_none(), "C's finish must consume C's held state");
    }

    /// SINGLE-GIVE HAPPY PATH UNCHANGED (fire-and-forget): one give at a time still completes exactly —
    /// begin, ack → accept (now HELD in phase 2 rather than cleared), then OP_FinishTrade consumes it
    /// silently. The only change vs. pre-review is the STATE lifetime (extended to the finish); the
    /// wire traffic and the eventual clear are identical.
    #[tokio::test]
    async fn fire_and_forget_single_give_completes_on_finish() {
        let mut nav = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = seed_give_gs();

        nav.command.request_give(11, 23);
        nav.tick_give(&mut stream, &mut gs);         // begin (phase 1)
        assert!(nav.give_state.is_some());
        gs.trade_ack_ready = true;
        nav.tick_give(&mut stream, &mut gs);         // ack → accept; phase 2, HELD
        assert!(nav.give_state.is_some(), "a fire-and-forget give is now held through the finish");

        apply_packet(&mut gs, &AppPacket { opcode: OP_FINISH_TRADE, payload: vec![] });
        nav.note_finish_trade();
        for _ in 0..GIVE_FINISH_SETTLE_TICKS { nav.tick_give(&mut stream, &mut gs); } // #486 settle
        assert!(nav.give_state.is_none(), "the finish clears the completed give — the machine is free again");
    }

    #[test]
    fn dead_player_halts_navigation() {
        // #238: a character that dies mid-goto must stop — the corpse must not keep walking the route.
        // Seed an in-progress nav, then assert nav_halt_if_dead() clears everything and reports dead.
        let seed_nav = |nav: &mut ActionLoop| {
            *nav.nav.goto_target.lock().unwrap() = Some((100.0, 200.0, 0.0));
            *nav.nav.goto_entity.lock().unwrap() = Some("a bat".into());
            *nav.controller.nav_intent.lock().unwrap() = Some(eqoxide_ipc::MoveIntent::default());
            nav.walker.path = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0]];
            nav.walker.local_path = vec![[0.0, 0.0, 0.0]];
            nav.walker.path_goal = Some((100.0, 200.0, 0.0));
            nav.walker.path_i = 1;
            nav.walker.local_i = 1;
            // #771: not `"navigating".into()` — `impl From<&str> for NavStatus` was deleted (it
            // was an unnamed fourth construction path bypassing every idle-row assert; see the
            // comment in `eqoxide-ipc/src/lib.rs` where the impl used to be). The real constructor
            // works everywhere, including across the crate boundary.
            *nav.nav.nav_state.lock().unwrap() =
                eqoxide_ipc::NavStatus { state: "navigating".to_string(), ..Default::default() };
        };
        let assert_halted = |nav: &ActionLoop| {
            assert!(nav.nav.goto_target.lock().unwrap().is_none(), "goto_target must clear on death");
            assert!(nav.nav.goto_entity.lock().unwrap().is_none(), "goto_entity must clear on death");
            assert!(nav.controller.nav_intent.lock().unwrap().is_none(), "nav_intent must clear so the controller stops");
            assert!(nav.walker.path.is_empty() && nav.walker.local_path.is_empty(), "route must clear on death");
            // The PUBLISHED snapshot (#608) must reflect the halt too — a consumer keeps whatever
            // was last published, so an unrefreshed route here would draw a dead player walking.
            let snap = nav.walker.debug_view().lock().unwrap().clone()
                .expect("halting must publish a snapshot");
            assert!(snap.committed_coarse.is_empty() && snap.committed_fine.is_empty(),
                "the published committed routes must clear on death");
            // The fast-steering cursor must reset with the path it indexes (#311) — a stale local_i
            // left over a cleared/rebuilt local_path aims the walker at the wrong segment.
            assert_eq!(nav.walker.local_i, 0, "local_i must reset with local_path on death");
            assert_eq!(nav.walker.path_goal, None);
            // #644: the halted state must be the HONEST TERMINAL `dead`, NOT the ambiguous `idle`
            // (which also means "ready for work"). An agent that issued a goto and then polled must
            // be able to tell "you died and went nowhere" from "arrived / ready".
            let ns = nav.nav.nav_state.lock().unwrap();
            assert_eq!(ns.state, "dead", "a slain character's nav_state must be the honest `dead`, not `idle`");
            assert_eq!(ns.reason.as_deref(), Some("player_dead"));
        };
        let new_nav = || {
            let g: eqoxide_ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(eqoxide_ipc::GroupSnapshot::default()));
            test_action_loop(g)
        };

        // (a) An HP-to-0 update that arrives BEFORE OP_Death (player_dead still false) — the exact
        //     window in which the corpse was seen walking. cur_hp<=0 with a known max must halt nav.
        let mut nav = new_nav();
        seed_nav(&mut nav);
        let mut gs = GameState::new();
        gs.player_dead = false;
        gs.cur_hp = 0;
        gs.max_hp = 1284;
        assert!(nav.walker.nav_halt_if_dead(&gs), "cur_hp<=0 (pre-OP_Death) must halt navigation");
        assert_halted(&nav);

        // (b) The OP_Death flag path (player_dead set, cur_hp already zeroed by apply_death).
        let mut nav = new_nav();
        seed_nav(&mut nav);
        let mut gs = GameState::new();
        gs.player_dead = true;
        gs.cur_hp = 0;
        gs.max_hp = 1284;
        assert!(nav.walker.nav_halt_if_dead(&gs));
        assert_halted(&nav);

        // (c) A LIVE player must NOT be halted (and cur_hp<=0 with max_hp==0 = "unknown", not dead —
        //     e.g. a fresh spawn before the first HP update — must not spuriously stop nav).
        let mut nav = new_nav();
        seed_nav(&mut nav);
        let mut gs = GameState::new();
        gs.player_dead = false;
        gs.cur_hp = 900;
        gs.max_hp = 1284;
        assert!(!nav.walker.nav_halt_if_dead(&gs), "a live player must keep navigating");
        assert!(nav.nav.goto_target.lock().unwrap().is_some(), "live nav must be untouched");
        gs.cur_hp = 0;
        gs.max_hp = 0; // unknown HP, not a death
        assert!(!nav.walker.nav_halt_if_dead(&gs), "cur_hp<=0 with max_hp==0 is unknown HP, not death");
        assert!(nav.nav.goto_target.lock().unwrap().is_some());
    }

    #[test]
    fn zone_change_resets_stale_destination_and_path() {
        // #248: a destination + route left over from the PREVIOUS zone must not survive a crossing —
        // in the new zone's coordinate space they aim the walker at a corner near the arrival point
        // and wedge it there. sync_zone_points must clear the goal, path, and recovery state.
        let group: eqoxide_ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(eqoxide_ipc::GroupSnapshot::default()));
        let mut nav = test_action_loop(group);

        // Simulate an in-progress nav in the OLD zone.
        nav.current_zone = "gfaydark".into();
        *nav.nav.goto_target.lock().unwrap() = Some((100.0, 200.0, 0.0));
        *nav.nav.goto_entity.lock().unwrap() = Some("a bat".into());
        *nav.controller.nav_intent.lock().unwrap() = Some(eqoxide_ipc::MoveIntent::default());
        nav.walker.path = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0]];
        nav.walker.local_path = vec![[0.0, 0.0, 0.0]];
        nav.walker.path_goal = Some((100.0, 200.0, 0.0));
        nav.walker.path_i = 1;
        nav.walker.local_i = 1;
        nav.walker.stuck_ticks = 5;
        nav.walker.nav_repaths = 3;
        nav.walker.backoff_ticks = 2;
        nav.walker.replan_coarse = true;
        // #771: real constructor, not `.into()` — see the comment on the same pattern above.
        *nav.nav.nav_state.lock().unwrap() =
            eqoxide_ipc::NavStatus { state: "blocked".to_string(), ..Default::default() };

        // Cross into a NEW zone.
        let mut gs = GameState::new();
        gs.world.zone_name = "crushbone".into();
        nav.sync_zone_points(&gs);

        // Destination + route + recovery state all cleared; walker comes to rest in the new zone.
        assert!(nav.nav.goto_target.lock().unwrap().is_none(), "goto_target must clear on zone change");
        assert!(nav.nav.goto_entity.lock().unwrap().is_none(), "goto_entity must clear on zone change");
        assert!(nav.controller.nav_intent.lock().unwrap().is_none(), "nav_intent must clear so the controller stops");
        // The PUBLISHED snapshot (#608): routes cleared AND the old zone's plan/pads dropped —
        // they describe the previous zone's geometry.
        let snap = nav.walker.debug_view().lock().unwrap().clone()
            .expect("the zone change must publish a cleared snapshot");
        assert!(snap.committed_coarse.is_empty() && snap.committed_fine.is_empty(),
            "published routes must clear on zone change");
        assert!(snap.plan.is_none(), "the old zone's plan trace must not survive the crossing");
        assert!(snap.pads.is_empty(), "the old zone's pad knowledge must not survive the crossing");
        assert!(nav.walker.path.is_empty() && nav.walker.local_path.is_empty(), "route must clear on zone change");
        assert_eq!(nav.walker.path_goal, None);
        assert_eq!(nav.walker.path_i, 0);
        // The fast-steering cursor must reset with the path it indexes (#311) — a stale local_i in
        // the NEW zone points at a segment of a route that no longer exists.
        assert_eq!(nav.walker.local_i, 0, "local_i must reset with local_path on zone change");
        assert_eq!(nav.walker.stuck_ticks, 0);
        assert_eq!(nav.walker.nav_repaths, 0);
        assert_eq!(nav.walker.proactive_replans, 0, "the oscillation budget must reset on zone change");
        assert_eq!(nav.walker.backoff_ticks, 0);
        assert!(!nav.walker.replan_coarse);
        assert_eq!(*nav.nav.nav_state.lock().unwrap(), "idle");
        assert_eq!(nav.current_zone, "crushbone");
    }

    /// **THE OSCILLATION GUARD counts proactive re-plans (#378 Phase 2).** A repeatedly-`NoWayThrough`
    /// fine tier must ARM the proactive coarse re-plan AND bump the oscillation budget, so a spot the
    /// fine tier cannot thread cannot loop `navigating` forever (the live qcat L-corner). A `Threaded`
    /// fine plan resets the local-stuck run (the wedge is over) but does NOT retroactively forgive the
    /// budget — only real journey progress (in `tick`) does that.
    #[test]
    fn proactive_replan_arms_and_counts_toward_the_oscillation_budget() {
        use eqoxide_nav::collision::{LocalOutcome, NoRoute};
        let group: eqoxide_ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(eqoxide_ipc::GroupSnapshot::default()));
        let mut nav = test_action_loop(group);

        let nwt = |start: [f32; 3]| eqoxide_nav::planner::LocalReply {
            gen: 1, start, goal: [start[0] + 40.0, start[1], start[2]],
            outcome: LocalOutcome::NoWayThrough { steer: vec![start], why: NoRoute::SearchClosed },
            plan_us: 100,
        };
        // Healthy walker, cooldown clear: NAV_LOCAL_STUCK_TICKS consecutive NoWayThrough plans arm the
        // proactive re-plan on the LAST one, and that arming bumps the oscillation budget exactly once.
        nav.walker.backoff_ticks = 0;
        nav.walker.stuck_ticks = 0;
        nav.walker.replan_cooldown = 0;
        for _ in 0..NAV_LOCAL_STUCK_TICKS {
            nav.walker.apply_local_plan(nwt([0.0, 0.0, 0.0]));
        }
        assert!(nav.walker.replan_coarse, "NoWayThrough × NAV_LOCAL_STUCK_TICKS must arm the proactive re-plan");
        assert_eq!(nav.walker.proactive_replans, 1, "arming the proactive re-plan bumps the oscillation budget");

        // A Threaded plan ends the local-stuck run but must not forgive the budget (only tick's
        // progress reset does): the fine tier finding one way through does not prove the wedge gone.
        nav.walker.apply_local_plan(eqoxide_nav::planner::LocalReply {
            gen: 2, start: [0.0, 0.0, 0.0], goal: [40.0, 0.0, 0.0],
            outcome: LocalOutcome::Threaded(vec![[0.0, 0.0, 0.0], [40.0, 0.0, 0.0]]), plan_us: 100,
        });
        assert_eq!(nav.walker.local_stuck_ticks, 0, "a threaded fine plan resets the local-stuck run");
        assert_eq!(nav.walker.proactive_replans, 1, "a threaded fine plan must not forgive the oscillation budget");

        // The cap is a real, small bound — the guard is not a no-op.
        assert!(PROACTIVE_REPLAN_CAP > 0 && PROACTIVE_REPLAN_CAP <= 16);
    }

    #[test]
    fn sync_group_publishes_own_and_other_member_hp_pct() {
        use eqoxide_core::game_state::{Entity, GroupMember};
        let mut gs = GameState::new();
        gs.player_name = "Aldric".into();
        gs.hp_pct = 88.0;
        gs.group_leader = "Aldric".into();
        gs.group_members = vec![
            GroupMember { name: "Aldric".into(), is_leader: true, level: 10, ..Default::default() },
            GroupMember { name: "Sariel".into(), level: 8, ..Default::default() },
        ];
        gs.upsert_entity(Entity {
            spawn_id: 99, name: "Sariel".into(), level: 8, is_npc: false,
            x: 0.0, y: 0.0, z: 0.0, hp_pct: 42.0, cur_hp: 42, max_hp: 100, race: "HUM".into(),
            heading: 0.0, dead: false, equipment: [0; 9], equipment_tint: [[0; 3]; 9],
            gender: 0, helm: 0, showhelm: 0, face: 0, hairstyle: 0, haircolor: 0, pose: eqoxide_core::game_state::Pose::Standing, gait: None, is_boat: false, flymode: 0,
        });

        let group: eqoxide_ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(eqoxide_ipc::GroupSnapshot::default()));
        let nav = test_action_loop(group.clone());
        nav.sync_group(&gs);

        let snap = group.lock().unwrap();
        assert_eq!(snap.leader, "Aldric");
        assert!(snap.you_are_leader);
        let aldric = snap.members.iter().find(|m| m.name == "Aldric").unwrap();
        assert_eq!(aldric.hp_pct, 88.0); // own HP comes from gs.hp_pct, not gs.world.entities
        let sariel = snap.members.iter().find(|m| m.name == "Sariel").unwrap();
        assert_eq!(sariel.hp_pct, 42.0); // other member's HP comes from the matching Entity
    }

    #[test]
    fn build_movement_history_layout() {
        // EQEmu UpdateMovementEntry is a packed 17-byte struct: Y@0, X@4, Z@8, type@12, ts@13.
        // Must be >= sizeof(UpdateMovementEntry) or the server debug-logs + ignores it (#105).
        let p = build_movement_history(10.0, -20.0, 3.5);
        assert_eq!(p.len(), 17, "UpdateMovementEntry is 17 packed bytes");
        assert_eq!(&p[0..4], &(-20.0f32).to_le_bytes(), "Y field @0 = server north");
        assert_eq!(&p[4..8], &(10.0f32).to_le_bytes(), "X field @4 = server east");
        // Z crosses the datum boundary: the caller passes FOOT (3.5), the wire carries the
        // model-origin datum = foot + WIRE_Z_OFFSET (#522).
        assert_eq!(&p[8..12], &(3.5f32 + eqoxide_core::coord::WIRE_Z_OFFSET).to_le_bytes(),
            "Z field @8 = foot + WIRE_Z_OFFSET (wire datum)");
        assert_eq!(p[12], 1, "type = Collision (benign; skips teleport/zoneline cheat checks)");
    }

    // ── #347 review round 1, finding B2: the client's own mirror must be PUBLISHED ──────────────
    //
    // `sync_inventory` is called from the packet loop (`gameplay.rs`), never from `tick()`. Every
    // local mirror edit in this file therefore used to leave the published inventory describing the
    // layout as it was BEFORE our own move, until the next inbound packet happened to arrive. #347
    // step 1 turned that published snapshot into a door check (`404 no item in slot N`), so the
    // staleness stopped being cosmetic and became a confident falsehood about a valid request.
    //
    // The seam: these tests assert on `inventory_slots.inventory`, which is *exactly* the slot the
    // HTTP door checks read (`s.inventory_slots.inventory` in `inventory.rs` / `combat.rs` /
    // `interact.rs` / `merchant.rs`). The other half of the link — that a request whose source slot
    // is absent from that slot is refused 404, and one whose source slot is present is accepted —
    // is pinned by `eqoxide-http`'s own `move_from_an_empty_slot_is_404_and_queues_nothing` and
    // `move_from_an_occupied_slot_is_200_and_queues_it`. Neither crate can host both halves:
    // `test_stream` is `pub(crate)` here, so the app crate's integration tests cannot drive a real
    // drain.

    fn inv_item(slot: i32, item_id: u32) -> eqoxide_core::game_state::InvItem {
        eqoxide_core::game_state::InvItem {
            slot, item_id, name: "Bone Chips".into(), charges: 1, ..Default::default()
        }
    }

    /// Slots present in the PUBLISHED inventory (the one `/v1/inventory` and every #347 door check
    /// read), sorted.
    fn published_slots(al: &ActionLoop) -> Vec<i32> {
        let mut v: Vec<i32> = al.inventory_slots.inventory.lock().unwrap().iter().map(|i| i.slot).collect();
        v.sort_unstable();
        v
    }

    /// **B2, the core.** Drive the REAL `/v1/inventory/move` path — `request_inventory_move` into the
    /// real command slot, then the real `drain_move_item` — and assert the PUBLISHED inventory agrees
    /// with `gs` the moment the drain returns, with no inbound packet and no `sync_inventory` call in
    /// between. Before the fix the published slot list was still `[23]` here while `gs` said `[30]`:
    /// a follow-up `/v1/combat/scribe from=30` would have been refused `404 no item in slot 30` for
    /// an item that is there, and `from=23` accepted for an item that is not.
    ///
    /// **Mutation check:** replace `self.mirror_move_item(gs, ..)` in `drain_move_item` with the old
    /// `gs.move_item(..)` → this goes RED on the published list (`[23]` vs `[30]`) while the `gs`
    /// assertion below still passes, which is precisely the divergence the reviewer described.
    #[tokio::test]
    async fn a_drained_move_republishes_the_inventory_before_any_inbound_packet() {
        let mut al = new_loop();
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let mut gs = GameState::new();
        gs.inventory.push(inv_item(23, 13073));
        al.sync_inventory(&gs); // the last publish an inbound packet would have done
        assert_eq!(published_slots(&al), vec![23]);

        assert!(al.command.request_inventory_move(23, 30), "the slot must start free");
        al.drain_move_item(&mut stream, &mut gs);

        assert_eq!(gs.inventory.iter().map(|i| i.slot).collect::<Vec<_>>(), vec![30],
            "the loop's own view moved the item");
        assert_eq!(published_slots(&al), vec![30],
            "the PUBLISHED inventory — the one every #347 door check reads — must agree with `gs` \
             the moment the drain returns, with no inbound packet to trigger sync_inventory");
    }

    /// The same promise for the deletion half: the RoF2 server consumes the scribed scroll off the
    /// cursor and sends nothing back, so the loop deletes it locally. An unpublished delete leaves a
    /// phantom item in the door check's view — the mirror-image lie, a `200` for a move of something
    /// that is gone.
    #[tokio::test]
    async fn a_mirrored_delete_republishes_the_inventory_too() {
        let al = new_loop();
        let mut gs = GameState::new();
        gs.inventory.push(inv_item(SLOT_CURSOR as i32, 13073));
        gs.inventory.push(inv_item(23, 1001));
        al.sync_inventory(&gs);
        assert_eq!(published_slots(&al), vec![23, SLOT_CURSOR as i32]);

        al.mirror_remove_item(&mut gs, SLOT_CURSOR as i32);

        assert_eq!(published_slots(&al), vec![23],
            "the consumed scroll must leave the PUBLISHED inventory immediately, not at the next \
             inbound packet");
    }

    /// Every raw local-mirror edit in `src`, as `(line, enclosing fn)`.
    ///
    /// **Round-2 review, R2-1 corollary.** The first cut matched a single PHYSICAL line, so
    /// `gs\n    .inventory\n    .retain(..)` evaded it — the identical hole the HTTP caller guard
    /// had, inherited from the same copy-a-grep-into-a-test habit. Continuation lines that begin
    /// with `.` are joined onto their statement and spaces touching a `.` are dropped, so a wrapped
    /// method chain is byte-identical to its one-line spelling. (`eqoxide-http`'s
    /// `no_silent_overwrite_guard` does the full normalising version — comments, string literals,
    /// the lot; this file only needs the method-chain case, and says so rather than implying it is
    /// the same strength.)
    fn raw_mirror_edits(src: &str) -> Vec<(usize, String)> {
        // Split so this scanner's own source is not itself a match (the first cut of this guard
        // flagged its own needle line).
        let move_needle   = concat!("gs.", "move_item(");
        let retain_needle = concat!("gs.", "inventory.retain(");

        // Blank string literals, then cut a trailing `//` comment, then drop spaces touching a `.`.
        // Blanking is not decoration: without it this scanner reads its OWN table-test snippets as
        // code and reports three offenders in the test that exists to pin it — the same
        // flagged-itself failure the first cut of this guard hit, one level up.
        let sanitize = |s: &str| -> String {
            let mut out = String::new();
            let mut chars = s.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '"' {
                    out.push('"');
                    while let Some(d) = chars.next() {
                        if d == '\\' { chars.next(); continue; }
                        if d == '"' { out.push('"'); break; }
                    }
                    continue;
                }
                if c == '/' && chars.peek() == Some(&'/') { break; }
                out.push(c);
            }
            let mut squeezed = String::new();
            let bytes: Vec<char> = out.chars().collect();
            for (i, &c) in bytes.iter().enumerate() {
                if c == ' ' {
                    let prev = squeezed.chars().last();
                    let next = bytes.get(i + 1).copied();
                    if prev == Some('.') || next == Some('.') { continue; }
                }
                squeezed.push(c);
            }
            squeezed
        };

        // Join `.`-continuations onto the statement they belong to, keeping its FIRST line number.
        let mut logical: Vec<(usize, String)> = Vec::new();
        for (n, line) in src.lines().enumerate() {
            let t = line.trim();
            if t.starts_with('.') && !t.starts_with("..") {
                if let Some(last) = logical.last_mut() { last.1.push_str(t); continue; }
            }
            logical.push((n + 1, line.to_string()));
        }

        let mut current_fn = String::new();
        let mut hits = Vec::new();
        for (n, line) in logical {
            let t = line.trim_start();
            if let Some(rest) = t.strip_prefix("fn ").or_else(|| t.strip_prefix("pub fn "))
                .or_else(|| t.strip_prefix("pub(crate) fn ")) {
                current_fn = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            }
            let squeezed = sanitize(&line);
            if !(squeezed.contains(move_needle) || squeezed.contains(retain_needle)) { continue; }
            hits.push((n, current_fn.clone()));
        }
        hits
    }

    /// R2-1 corollary, pinned: the wrapped spelling must be found. Without this, the joiner could be
    /// deleted and every mirror-edit guarantee below would silently weaken back to a line grep.
    #[test]
    fn the_mirror_edit_scanner_sees_a_wrapped_method_chain() {
        let one_line = "fn f() {\n    gs.inventory.retain(|i| i.slot != slot);\n}";
        let wrapped  = "fn f() {\n    gs\n        .inventory\n        .retain(|i| i.slot != slot);\n}";
        let spaced   = "fn f() {\n    gs . inventory . retain(|i| i.slot != slot);\n}";
        let prose    = "fn f() {\n    // gs.inventory.retain(..) is the shape\n}";
        for (label, src) in [("one line", one_line), ("wrapped", wrapped), ("spaced", spaced)] {
            let hits = raw_mirror_edits(src);
            assert_eq!(hits.len(), 1, "{label}: expected one mirror edit, got {hits:?}");
            assert_eq!(hits[0].1, "f", "{label}: wrong enclosing fn");
        }
        assert!(raw_mirror_edits(prose).is_empty(), "a comment is not a mirror edit");

        // The blanking half, pinned for the same reason: these are what this test's own snippets
        // look like to the scanner, and a regression here makes the guard flag itself rather than
        // the code it protects.
        let in_string = "fn f() {\n    let s = \"gs.inventory.retain(x)\";\n}";
        assert!(raw_mirror_edits(in_string).is_empty(), "a string literal is not a mirror edit");
        let trailing = "fn f() {\n    let x = 1; // gs.inventory.retain(x)\n}";
        assert!(raw_mirror_edits(trailing).is_empty(), "a trailing comment is not a mirror edit");
    }

    /// A guard, not a substitute for the two behaviour tests above: it catches the *sixth* mirror
    /// edit someone adds next year, which no existing test would cover. It is deliberately narrow —
    /// it only knows about this one file and these two mutating call shapes.
    #[test]
    fn every_local_inventory_mirror_edit_in_this_file_goes_through_a_republishing_helper() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/action_loop.rs");
        let text = std::fs::read_to_string(&path).unwrap();
        const ALLOWED: [&str; 2] = ["mirror_move_item", "mirror_remove_item"];

        let mut offenders: Vec<String> = Vec::new();
        let mut allowed_hits = 0usize;
        for (line, enclosing) in raw_mirror_edits(&text) {
            if ALLOWED.contains(&enclosing.as_str()) { allowed_hits += 1; continue; }
            offenders.push(format!("action_loop.rs:{line}: in `{enclosing}`"));
        }

        assert!(
            offenders.is_empty(),
            "these edit the local inventory mirror without republishing it, so every #347 door \
             check keeps answering from the pre-edit snapshot until the next inbound packet — use \
             `self.mirror_move_item(..)` / `self.mirror_remove_item(..)`:\n  {}",
            offenders.join("\n  ")
        );
        // Anti-vacuity: the scanner must actually be finding the two helpers' own edits. If a
        // rename made this zero, `offenders.is_empty()` would pass for the wrong reason.
        assert_eq!(allowed_hits, 2,
            "expected exactly the two republishing helpers' own mirror edits, found {allowed_hits}");
    }
}
