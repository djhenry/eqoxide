//! Phase 2 gameplay loop: receive packets, update game state, keepalive, navigate.
//!
//! Handles zone transitions inline: when OP_ZONE_SERVER_INFO arrives the current
//! zone stream is replaced with a new connection and the zone-entry handshake runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Duration, sleep};

use crate::eq_net::login::WorldCredentials;
use crate::eq_net::action_loop::ActionLoop;
use crate::eq_net::packet_handler::apply_packet;
use crate::eq_net::protocol::*;
use crate::eq_net::transport::{AppPacket, EqStream};
use crate::game_state::GameState;

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// How often to re-send the bind-respawn request while an explicit respawn is pending (#284/#50).
const RESPAWN_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// #371 active liveness probe. `connected:true` only proves the SOCKET ACKs; a zone that is still
/// ticking but not servicing our packets (a stuck per-client dispatch / script, or a very slow tick)
/// keeps ACKing while making no application progress for us, which is indistinguishable from a quiet
/// zone by the passive clocks. The probe is a self-`OP_Consider`: purely zone-MAIN-LOOP-serviced (the
/// zone resolves it against its in-process entity list and replies — no WORLD hop, unlike OP_WhoAll),
/// benign (self is not an NPC, so faction is hardcoded 1 with no aggro/faction/hate-list evaluation),
/// and outside any anti-cheat path (MQGhost keys on movement cadence only). An unanswered probe past
/// [`crate::ipc::PROBE_TIMEOUT_SECS`] while the socket still ACKs is the unresponsive-world signal.
/// (Scope: this catches the still-ticking-but-unresponsive case. A TOTAL zone freeze stops the ACKs
/// too on this single-threaded-libuv server, so that is already covered by `connected: false`.)
///
/// Only sent once the world has been application-silent this long — during active play spontaneous
/// packets already prove the world is processing, so the probe adds no load there.
const PROBE_QUIET_THRESHOLD: Duration = Duration::from_secs(12);
/// Minimum gap between probes while the world stays quiet, so we poke at most ~twice a minute. Single
/// source of truth is `ipc::PROBE_INTERVAL_SECS`, because `ipc::PASSIVE_LIVENESS_STALE_SECS` is
/// derived from this cadence (an answered-idle session refreshes proof-of-life once per interval, so
/// the #470 passive bound must exceed it — see that constant's doc).
const PROBE_INTERVAL: Duration = Duration::from_secs(crate::ipc::PROBE_INTERVAL_SECS);

/// #335: how long `run_zone_entry_handshake` waits for the new zone to accept our OP_ZoneEntry
/// (OP_NewZone → OP_Weather → OP_SendExpZonein) before declaring the zone-in FAILED and surfacing it
/// honestly. This is a HARD deadline, not a resend cadence: we send OP_ZoneEntry exactly once (the
/// transport layer's `poll_resend` retransmits that ONE datagram verbatim while it is still unacked,
/// which recovers genuine wire loss). We deliberately do NOT app-level re-send a fresh OP_ZoneEntry
/// once the session has ACKed the first — see `run_zone_entry_handshake` and
/// `docs/eq-technical-knowledgebase/zone-entry-duplicate-on-admitted-client.md` for why a second
/// ClientZoneEntry on an admitted session self-disconnects the client via EQEmu's antighost check.
/// Any wedge past this deadline is surfaced as an honest failure, never a confident falsehood.
const ZONE_ENTRY_HANDSHAKE_DEADLINE: Duration = Duration::from_secs(30);

/// #371: does this OP_Consider reply describe our OWN spawn — i.e. is it the reply to our
/// self-consider liveness probe, rather than a user `/consider` of another mob? Reads targetid@4
/// of Consider_Struct.
fn consider_reply_is_self(payload: &[u8], player_id: u32) -> bool {
    player_id != 0
        && payload.len() >= 8
        && u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]) == player_id
}

/// #371: is a liveness probe currently awaiting its reply? True when we have sent a probe that has
/// not yet been answered (no reply, or the last reply predates the last send). Used to consume the
/// probe's own reply exactly once, so a genuine user self-consider is never swallowed.
fn probe_outstanding(h: &crate::ipc::NetHealth) -> bool {
    match (h.last_probe_sent, h.last_probe_reply) {
        (Some(sent), Some(reply)) => sent > reply,
        (Some(_), None)           => true,
        _                         => false,
    }
}

/// #371: on a zone change, discard the previous zone's probe verdict. `world_responsive` returns to
/// "no verdict yet" (true) until we re-probe in the new zone, so a transition never reads as a wedge.
fn reset_probe_clocks(net_health: &crate::ipc::NetHealthShared) {
    let mut h = net_health.lock().unwrap();
    h.last_probe_sent = None;
    h.last_probe_reply = None;
    h.first_unanswered_probe_sent = None;
}

/// #371 wedge-flicker fix: pure resend-policy predicate — should the network thread send a new probe
/// right now, given how long the world has been application-silent and how long ago (if ever) the
/// last one went out? Extracted out of the loop body so the 30s-resend cadence and the http.rs 10s
/// grace window can be driven TOGETHER over a simulated timeline in tests — the interaction a
/// single-snapshot test cannot see (see `wedge_timeline_tests` below, in this same module).
fn should_send_probe(last_packet_ago: Duration, last_probe_sent_ago: Option<Duration>) -> bool {
    let app_silent = last_packet_ago >= PROBE_QUIET_THRESHOLD;
    let probe_due  = last_probe_sent_ago.map_or(true, |a| a >= PROBE_INTERVAL);
    app_silent && probe_due
}

/// #371 wedge-flicker fix: apply a just-sent probe to `NetHealth`. Bumps the scheduling clock
/// (`last_probe_sent`) unconditionally — every resend needs that so `should_send_probe` knows when
/// the next one is due — but stamps the wedge-timeout clock (`first_unanswered_probe_sent`) ONLY on
/// the first send of a new unanswered streak (`is_none()`). THIS is the actual fix: a resend of a
/// still-unanswered probe must never look like a fresh one to `world_responsive`, or a permanently
/// wedged zone would re-earn the 10s in-flight grace window on every 30s resend forever instead of
/// staying flagged wedged. Extracted so tests can drive the exact state transition production code
/// performs, not a re-derivation of it.
fn record_probe_sent(h: &mut crate::ipc::NetHealth, now: std::time::Instant) {
    h.last_probe_sent = Some(now);
    if h.first_unanswered_probe_sent.is_none() {
        h.first_unanswered_probe_sent = Some(now);
    }
}

/// #371 wedge-flicker fix: apply a genuine probe reply to `NetHealth`. Stamps `last_probe_reply` and
/// ENDS the unanswered streak (`first_unanswered_probe_sent = None`), so `world_responsive` returns to
/// "no verdict yet" until the next probe starts a new streak. Extracted alongside `record_probe_sent`
/// for the same testability reason.
fn record_probe_reply(h: &mut crate::ipc::NetHealth, now: std::time::Instant) {
    h.last_probe_reply = Some(now);
    h.first_unanswered_probe_sent = None;
}

/// #371 false-alive-on-second-wedge fix: apply a spontaneous inbound APPLICATION packet to
/// `NetHealth`. Stamps `last_packet` AND ends any unanswered-probe streak — because real world
/// traffic is itself proof the zone is servicing us, exactly like a probe reply. Clearing (not
/// restamping) the streak-start clock re-arms it: within one CONTINUOUS silence nothing bumps
/// `last_packet`, so the anti-flicker guard from `record_probe_sent` still holds; but once traffic
/// RESUMES and then stops again, the NEXT probe of the new silence stamps a FRESH streak start,
/// so a second genuine wedge is still detected. Without this clear, a stale streak-start from an
/// earlier wedge would sit older than every later probe forever, making the answered-clause
/// `last_packet_ago <= first_unanswered_sent_ago` permanently true → a confident false-ALIVE that
/// hides the re-wedge (the worst honesty class).
///
/// #419 (defensive hygiene): make this the SOLE writer of `last_packet`. `login.rs`'s handshake
/// drain used to stamp `last_packet` directly, bypassing the streak-clear above. That is NOT an
/// active bug today — `first_unanswered_probe_sent` is only ever set in `record_probe_sent` during
/// gameplay, and login never re-runs after gameplay (`run_login_phase` precedes `run_gameplay_phase`,
/// which returns and ends the net thread — there is no relogin-without-restart path), so no stale
/// streak can reach the login drain to be masked. It is a LATENT seam: a *second* writer of
/// `last_packet` that skips the streak-clear could resurrect the #371 false-alive IF a
/// relogin-without-restart path is ever added. Routing the login stamp through here keeps a single
/// canonical writer so that can't happen silently. `pub(crate)` so `login.rs` calls it.
pub(crate) fn record_app_packet(h: &mut crate::ipc::NetHealth, now: std::time::Instant) {
    h.last_packet = now;
    h.first_unanswered_probe_sent = None;
}

/// Consume the zone stream and run the gameplay loop indefinitely.
pub async fn run_gameplay_phase(
    stream_init:   EqStream,
    net_rx_init:   UnboundedReceiver<AppPacket>,
    mut gs:        GameState,
    char_name:     String,
    mut action_loop: ActionLoop,
    world_creds:   WorldCredentials,
    shutdown:      Arc<AtomicBool>,
    camp:          crate::ipc::CampReq,
    camp_until:    crate::ipc::CampUntil,
    respawn:       crate::ipc::RespawnReq,
    game_state_snapshot: crate::ipc::GameStateSnapshot,
    net_health:          crate::ipc::NetHealthShared,
) {
    // Wrap in Option so Rust allows reassignment after zone transitions.
    let mut stream: Option<EqStream>                      = Some(stream_init);
    let mut net_rx: Option<UnboundedReceiver<AppPacket>>  = Some(net_rx_init);
    let mut last_keepalive = std::time::Instant::now();
    // Last time we (re)sent a bind-respawn request while an explicit respawn is pending (#284/#50).
    let mut last_respawn_retry: Option<std::time::Instant> = None;
    // The OP_RespawnWindow payload the server sent when we died, held (NOT auto-answered) until the
    // agent POSTs /v1/lifecycle/respawn (#284).
    let mut pending_respawn: Option<Vec<u8>> = None;

    loop {
        let s  = stream.as_mut().expect("stream always Some in loop");
        let rx = net_rx.as_mut().expect("net_rx always Some in loop");
        s.poll_recv();
        s.poll_resend(); // retransmit un-ACKed reliables so a lost packet doesn't linkdead us (#254)

        // Relaxed: the flag is a self-contained shutdown signal with no happens-before
        // dependency on other data published by the setter (/exit or window-close).
        if shutdown.load(Ordering::Relaxed) {
            perform_clean_shutdown(s, rx).await;
            // Logout sent. Idle until the main thread exits the process (it owns the winit/Wayland
            // teardown). Do NOT return — returning would let run_login_flow retry and reconnect.
            loop { sleep(Duration::from_millis(200)).await; }
        }

        // ── Camp ─────────────────────────────────────────────────────────────
        // Drain a camp command (from /exit, /camp, the HUD button, or the `/camp` chat keyword)
        // and start/cancel the camp. `OP_Camp` arms EQEmu's ~29s camp timer; we keep the session
        // alive (keepalives below still fire) until CAMP_DURATION elapses, then set `shutdown` so
        // the disconnect is clean (the server has set `instalog`, so no linkdead). Cancelling sends
        // a Standing `OP_SpawnAppearance`, which disables the server-side camp timer.
        if let Some(cmd) = camp.lock().unwrap().take() {
            let now      = std::time::Instant::now();
            let current  = *camp_until.lock().unwrap();
            let (next, action) = camp_apply(cmd, current, now, CAMP_DURATION);
            *camp_until.lock().unwrap() = next;
            use crate::eq_net::protocol::build_spawn_appearance_packet;
            match action {
                CampAction::Started => {
                    s.send_app_packet(OP_CAMP, &[]);
                    // Sit, like the real client — camping is a seated action and standing cancels it.
                    s.send_app_packet(OP_SPAWN_APPEARANCE,
                        &build_spawn_appearance_packet(gs.player_id as u16, 14, 110));
                    gs.sitting = true;
                    gs.log_msg("system", "Camping to desktop...");
                    tracing::info!("EQ: camp started — clean shutdown in {}s", CAMP_DURATION.as_secs());
                }
                CampAction::Cancelled => {
                    // Standing both cancels the server camp timer and stands the character back up.
                    s.send_app_packet(OP_SPAWN_APPEARANCE,
                        &build_spawn_appearance_packet(gs.player_id as u16, 14, 100));
                    gs.sitting = false;
                    gs.log_msg("system", "Camp cancelled.");
                    tracing::info!("EQ: camp cancelled");
                }
                CampAction::NoOp => {}
            }
        }
        // Camp deadline reached → request a clean shutdown (handled at the top of the next loop).
        if camp_expired(*camp_until.lock().unwrap(), std::time::Instant::now()) {
            tracing::info!("EQ: camp complete — disconnecting cleanly (no linkdead)");
            shutdown.store(true, Ordering::Relaxed);
        }

        let mut zone_redirect: Option<(String, u16)> = None;
        let mut world_reconnect_needed = false;
        while let Ok(packet) = rx.try_recv() {
            // #371: intercept the reply to OUR self-consider liveness probe BEFORE apply_packet.
            // It is an internal health poke, not world state — stamp the probe clock and DROP it, so
            // it does not spam the con log, overwrite target con, or bump `last_packet` (which would
            // reset last_packet_age_ms every probe cadence and destroy its "world quiet for 45s"
            // meaning). Only consume it while a probe is actually outstanding, so a genuine user
            // self-consider is never swallowed.
            if packet.opcode == OP_CONSIDER && consider_reply_is_self(&packet.payload, gs.player_id) {
                let mut h = net_health.lock().unwrap();
                if probe_outstanding(&h) {
                    record_probe_reply(&mut h, std::time::Instant::now());
                    continue;
                }
            }
            apply_packet(&mut gs, &packet);
            record_app_packet(&mut net_health.lock().unwrap(), std::time::Instant::now());
            action_loop.sync_entities(&gs);
            action_loop.sync_zone_points(&gs);
            action_loop.sync_tasks(&gs);
            action_loop.sync_group(&gs);
            action_loop.sync_guild(&gs);
            action_loop.sync_inventory(&gs);
            action_loop.sync_merchant(&gs);
            action_loop.sync_messages(&gs);
            action_loop.sync_doors(&gs);
            // Deliver a /who all roster to the pending GET /v1/observe/who as soon as it lands (#300).
            // A friends-presence poll (OP_FriendsWho) replies on this SAME opcode, so route it to the
            // pending GET /v1/social/friends instead when a friends poll is what we just sent (#301).
            if packet.opcode == OP_WHO_ALL_RESPONSE {
                if action_loop.expecting_friends() {
                    action_loop.fulfill_friends(&gs);
                } else {
                    action_loop.fulfill_who(&gs);
                }
            }

            // A3 Migration 1 (#448): resolve an awaited merchant buy on its RESOLVING packet, AFTER
            // `apply_packet` so `gs` already holds the receipt (coin deducted, item name in the ware
            // list). The OP_ShopPlayerBuy echo (correlated on merchant/slot) → `Resolved(BuyOk)`; the
            // OP_ShopEndConfirm refusal → `Refused`. Insufficient funds sends NEITHER — that buy stays
            // parked and resolves to `Unconfirmed` via the HTTP timeout (the honesty invariant). Both
            // fulfils are non-blocking sends; the net tick never `.await`s. See `command_state::result`.
            match packet.opcode {
                OP_SHOP_PLAYER_BUY  => action_loop.fulfill_buy_ok(&gs, &packet.payload),
                OP_SHOP_END_CONFIRM => action_loop.fulfill_buy_refused(),
                // eqoxide#479: resolve an awaited merchant open on its OP_ShopRequest echo. Both a
                // confirmed open (command=1) and a real refusal (command=0) share this one opcode —
                // `fulfill_open` reads the echoed `command` field to tell them apart. A non-merchant
                // / out-of-range target sends NO echo at all, so that case never reaches here — it
                // resolves via the HTTP timeout instead (the honesty invariant).
                OP_SHOP_REQUEST     => action_loop.fulfill_open(&packet.payload),
                // A3 Migration 2 (#448); verify-transfer (#486): OP_FinishTrade ends the trade SESSION
                // — it does NOT prove the NPC accepted the item (a rejected / out-of-range turn-in fires
                // it too, then RETURNS the item). So we only NOTE the finish here (applied above with the
                // trade slots cleared AND any returned-item packet), and the DEFERRED `tick_give` verdict
                // verifies the item actually left inventory before resolving. No-op unless a phase-2
                // awaited/fire-and-forget give is parked.
                OP_FINISH_TRADE     => action_loop.note_finish_trade(),
                _ => {}
            }

            // A3 Migration 3 (#448): resolve an awaited self-cast the moment its outcome is APPLIED.
            // Unlike buy/give (keyed on a specific resolving opcode), a cast ends via one of THREE
            // de-duped opcodes, so we fulfil on the `gs.last_cast` TRANSITION instead — checked here,
            // per packet, so the terminal is caught before any subsequent OP_BeginCast in this same
            // drain could clear `last_cast`. No-op unless an awaited cast is parked and a fresh outcome
            // is present. See `ActionLoop::fulfill_cast`.
            action_loop.fulfill_cast(&gs);

            match packet.opcode {
                // Another player is asking to trade with us: the server forwards their
                // OP_TradeRequest { to_mob_id = us, from_mob_id = initiator }. Our give/turn-in
                // flow only implemented the initiator side, so incoming PC trade requests were
                // never acked and the initiator timed out (eqoxide#38). Auto-accept by replying
                // OP_TradeRequestAck with the ids swapped (to = initiator, from = us), mirroring
                // the server's NPC auto-ack, which opens the trade session.
                OP_TRADE_REQUEST if packet.payload.len() >= 8 => {
                    let to_mob_id = u32::from_le_bytes(packet.payload[0..4].try_into().unwrap());
                    let from_mob_id = u32::from_le_bytes(packet.payload[4..8].try_into().unwrap());
                    if to_mob_id == gs.player_id {
                        s.send_app_packet(OP_TRADE_REQUEST_ACK,
                            &build_trade_request(from_mob_id, gs.player_id));
                        gs.log_msg("trade", "Accepting incoming trade request.");
                        tracing::info!("EQ: trade: acked incoming OP_TradeRequest from mob_id={}", from_mob_id);
                    }
                }
                // Server listed a lootable item — echo back immediately to take it.
                OP_LOOT_ITEM => {
                    gs.loot_last_activity = Some(std::time::Instant::now());
                    gs.log_msg("loot", "Looting item...");
                    s.send_app_packet(OP_LOOT_ITEM, &packet.payload);
                    tracing::info!("EQ: auto-loot: taking item (echoed OP_LootItem)");
                }
                // Player died: the server opened the respawn hover window and holds us as a corpse
                // until we pick an option. #284: DON'T auto-respawn — keep the character slain so a
                // headless agent can observe the death (killed_by / corpse) and decide. HOLD the
                // window; we answer it only when the agent POSTs /v1/lifecycle/respawn (handled by
                // the respawn-drive block below). The server keeps us as a corpse meanwhile.
                OP_RESPAWN_WINDOW => {
                    pending_respawn = Some(packet.payload.clone());
                    gs.strategy = "Dead — POST /v1/lifecycle/respawn to revive".into();
                    tracing::info!("EQ: respawn window received — holding dead until /lifecycle/respawn (#284)");
                }
                // Death respawn: EQEmu (with RespawnFromHover off) sends OP_ZonePlayerToBind and
                // then holds us in a ZoneToBindPoint "zoning" state, waiting for the client to
                // reply with OP_ZoneChange to finalize the respawn (Client::GoToDeath →
                // MovePC(ZoneToBindPoint), completed by Handle_OP_ZoneChange). Without that reply
                // the server leaves us half-zoned and silently drops all auto-attack/combat until
                // a full relog. apply_bind_respawn already moved us locally; here we complete the
                // handshake. bind_zone_id == 0 is the server's "same zone" marker → reply with the
                // current zone id. The server's OP_ZoneChange response (handled below) then drives
                // the reconnect/re-entry exactly like a normal zone change. (eqoxide#75)
                OP_ZONE_PLAYER_TO_BIND if packet.payload.len() >= 20 => {
                    let p = &packet.payload;
                    let bind_zone_id = u16::from_le_bytes([p[0], p[1]]);
                    let instance_id  = u16::from_le_bytes([p[2], p[3]]);
                    let bx = f32::from_le_bytes([p[4],  p[5],  p[6],  p[7]]);
                    let by = f32::from_le_bytes([p[8],  p[9],  p[10], p[11]]);
                    let bz = f32::from_le_bytes([p[12], p[13], p[14], p[15]]);
                    let target_zone = if bind_zone_id != 0 { bind_zone_id } else { gs.zone_id };
                    s.send_app_packet(OP_ZONE_CHANGE,
                        &build_zone_change(&gs.player_name, target_zone, instance_id, bx, by, bz));
                    tracing::info!("EQ: bind respawn — sent OP_ZoneChange to finalize respawn (zone_id={target_zone})");
                }
                // Server booted us (typically another client logged in this same character).
                // EQEmu's default is "second login wins"; the first client receives OP_GMKick.
                // We're already kicked, so just disconnect the session and exit cleanly.
                OP_GMKICK => {
                    tracing::info!("EQ: OP_GMKick — disconnected (character logged in elsewhere)");
                    gs.log_msg("system", "Disconnected: this character was logged in from another location.");
                    s.send_session_disconnect();
                    // We're already booted, so no OP_Logout. Request shutdown: the render loop's
                    // `about_to_wait` exits the winit event loop on the main thread, which tears
                    // down cleanly and exits the process. Idle here; do NOT return (avoids reconnect).
                    shutdown.store(true, Ordering::Relaxed);
                    loop { sleep(Duration::from_millis(200)).await; }
                }
                OP_REQUEST_CLIENT_ZONE_CHANGE if packet.payload.len() >= 24 => {
                    // Server wants us to move — either a zone transition or a same-zone teleport
                    // (GM #summon / #goto / #zone). RoF2 RequestClientZoneChange_Struct
                    // (common/patches/rof2_structs.h): u16 zone_id, u16 instance_id, u32 unknown004,
                    // float y, float x, float z, float heading, ... — i.e. an extra `unknown004`
                    // sits before the coords that the Titanium struct does NOT have, so y/x/z live at
                    // offsets 8/12/16, not 4/8/12. Reading the Titanium offsets grabbed `unknown004`
                    // as y (a garbage/NaN float), which corrupted the teleport target: a NaN position
                    // fails the streamer's `> CORRECTION_SQ` jump test so the controller never adopted
                    // it, leaving a GM #summon/#zone'd character stranded at its old coords / in the
                    // void (#167). (#116 family — same-zone summons are also on this path.)
                    let zone_id     = u16::from_le_bytes([packet.payload[0], packet.payload[1]]);
                    let instance_id = u16::from_le_bytes([packet.payload[2], packet.payload[3]]);
                    let y = f32::from_le_bytes([packet.payload[8],  packet.payload[9],  packet.payload[10], packet.payload[11]]);
                    let x = f32::from_le_bytes([packet.payload[12], packet.payload[13], packet.payload[14], packet.payload[15]]);
                    let z = f32::from_le_bytes([packet.payload[16], packet.payload[17], packet.payload[18], packet.payload[19]]);

                    // Defense-in-depth: never let a non-finite coordinate reach the position — a NaN
                    // silently breaks every downstream distance/adoption test (the bug above).
                    if !(x.is_finite() && y.is_finite() && z.is_finite()) {
                        tracing::warn!("EQ: OP_REQUEST_CLIENT_ZONE_CHANGE with non-finite coords ({x},{y},{z}) — ignoring");
                    } else if zone_id == gs.zone_id {
                        // Same-zone teleport (e.g. #goto x y z, #zone 0).
                        // The server expects the client to just move — it clears zone_mode
                        // before we respond, so sending OP_ZONE_CHANGE would cause a cancel
                        // and a spurious world reconnect (see zoning.cpp:1056-1068).
                        // Wire y = server_y = east = gs.player_y; wire x = server_x = north = gs.player_x.
                        gs.player_x = x;
                        gs.player_y = y;
                        gs.player_z = z;
                        tracing::info!("EQ: same-zone teleport → pos=({:.1},{:.1},{:.1})", x, y, z);
                    } else {
                        // Cross-zone transition (#zone <name>): send OP_ZONE_CHANGE to
                        // trigger the full zone-change protocol (world reconnect, etc.).
                        s.send_app_packet(OP_ZONE_CHANGE,
                            &build_zone_change(&gs.player_name, zone_id, instance_id, x, y, z));
                        tracing::info!("EQ: cross-zone OP_REQUEST_CLIENT_ZONE_CHANGE zone_id={zone_id} → sent OP_ZONE_CHANGE");
                    }
                }
                OP_TRANSLOCATE if packet.payload.len() >= 92 => {
                    // The server is offering a translocate (Priest of Discord, a Timorous Deep
                    // firepot, or a Translocate spell): a confirmation prompt carrying the
                    // destination (Translocate_Struct: ZoneID@0, SpellID@4, y@76, x@80, z@84,
                    // Complete@88). A headless agent that triggered it wants to travel, so auto-accept
                    // by echoing the struct back with Complete=1 — the server then moves/zones us via
                    // its normal path (OP_RequestClientZoneChange / a same-zone move handled above).
                    // Ignore a packet already marked Complete (nothing to accept). (#192)
                    use crate::eq_net::protocol::build_translocate_ack;
                    let complete = u32::from_le_bytes([packet.payload[88], packet.payload[89], packet.payload[90], packet.payload[91]]);
                    if complete != 1 {
                        let zone_id  = u32::from_le_bytes([packet.payload[0], packet.payload[1], packet.payload[2], packet.payload[3]]);
                        let spell_id = u32::from_le_bytes([packet.payload[4], packet.payload[5], packet.payload[6], packet.payload[7]]);
                        s.send_app_packet(OP_TRANSLOCATE, &build_translocate_ack(&packet.payload));
                        gs.log_msg("zone", &format!("Accepting translocate to zone {zone_id}"));
                        tracing::info!("EQ: OP_Translocate → auto-accepting (zone_id={zone_id}, spell_id={spell_id})");
                    }
                }
                OP_ZONE_CHANGE if packet.payload.len() >= 96 => {
                    // RoF2 ZoneChange_Struct: zoneID@64, success is an i32 at offset 92 (was 84 in
                    // Titanium).
                    let echo_zone_id = u16::from_le_bytes([packet.payload[64], packet.payload[65]]);
                    let success = i32::from_le_bytes([
                        packet.payload[92], packet.payload[93],
                        packet.payload[94], packet.payload[95],
                    ]);
                    tracing::info!("EQ: OP_ZONE_CHANGE server response success={success} zone_id={echo_zone_id}");
                    if success == 1 {
                        // #368: a SAME-ZONE walk-in cross (intra-zone translocator) gets a success=1
                        // echo naming the CURRENT zone, from the server's lightweight `DoZoneSuccess`
                        // in-zone reposition — the zone session was NOT torn down, so a world
                        // reconnect here reconnects against a live zone and wedges the connection.
                        // The action loop flags exactly that case (and already repositioned us);
                        // skip the reconnect for it. Every OTHER success=1 echo — a genuine
                        // cross-zone line, a GM #zone, a death/bind respawn — still reconnects, even
                        // if it happens to name the current zone (the death path echoes the current
                        // zone too, but never sets this flag).
                        if action_loop.take_same_zone_reposition() {
                            tracing::info!("EQ: same-zone in-zone reposition (zone_id={echo_zone_id}) — no world reconnect (#368)");
                        } else {
                            world_reconnect_needed = true;
                        }
                    }
                }
                OP_ZONE_SERVER_INFO
                    if packet.payload.len() >= SIZE_ZONE_SERVER_INFO
                        && zone_redirect.is_none() =>
                {
                    let info = unsafe { safe_read::<ZoneServerInfo_S>(&packet.payload) };
                    let ip   = String::from_utf8_lossy(&info.ip)
                        .trim_end_matches('\0')
                        .to_string();
                    zone_redirect = Some((ip, info.port));
                }
                _ => {}
            }
        }

        // ── Auto-loot ──────────────────────────────────────────────────────────
        // `loot_tick_action` is the pure decision (no I/O, no gs); this loop performs whatever it
        // decides and applies the resulting state. The one thing it NEVER decides is "Looting
        // complete" — that message may only come from the inbound OP_LootComplete handler
        // (apply_loot_complete in packet_handler.rs), never from this timer (#346).
        match loot_tick_action(&LootTickState {
            session_active:   gs.loot_session_active,
            confirmed:        gs.loot_confirmed,
            current_corpse:   gs.loot_current_corpse,
            queued_at:        gs.loot_queued_at,
            pending_front:    gs.pending_loot.front().copied(),
            last_activity:    gs.loot_last_activity,
            end_requested_at: gs.loot_end_requested_at,
            defensive_close_at: gs.loot_defensive_close_at,
        }, std::time::Instant::now()) {
            LootTickAction::None => {}
            LootTickAction::Open(corpse_id) => {
                gs.pending_loot.pop_front();
                s.send_app_packet(OP_LOOT_REQUEST, &corpse_id.to_le_bytes());
                gs.loot_session_active = true;
                gs.loot_confirmed = false;
                gs.loot_current_corpse = Some(corpse_id);
                gs.loot_last_activity = Some(std::time::Instant::now());
                gs.loot_end_requested_at = None;
                if gs.pending_loot.is_empty() {
                    gs.loot_queued_at = None;
                }
                tracing::info!("EQ: auto-loot: sent OP_LootRequest for corpse_id={}", corpse_id);
            }
            LootTickAction::Close(corpse_id) => {
                // Payload MUST be the corpse's spawn_id (4 bytes) — EQEmu's Handle_OP_EndLootRequest
                // drops any OP_EndLootRequest whose size != sizeof(uint32) without replying, so the
                // old empty-payload send could never have produced an OP_LootComplete at all
                // (client_packet.cpp:6266).
                s.send_app_packet(OP_END_LOOT_REQUEST, &corpse_id.to_le_bytes());
                gs.loot_end_requested_at = Some(std::time::Instant::now());
                tracing::info!(
                    "EQ: auto-loot: sent OP_EndLootRequest for corpse_id={} — awaiting OP_LootComplete",
                    corpse_id
                );
            }
            LootTickAction::OpenTimedOut(corpse_id) => {
                apply_loot_open_timeout(&mut gs, corpse_id);
                // #414: release the server-side loot lock speculatively even though this corpse
                // never confirmed open — `Corpse::EndLoot` doesn't check ownership (verified
                // against EQEmu zone/corpse.cpp:1787-1802 via eq-client-expert), so this is safe.
                // Waiting for it to resolve (loot_defensive_close_at, set inside
                // apply_loot_open_timeout) before opening the next corpse narrows the window in
                // which a late OP_MoneyOnCorpse for THIS corpse could land on a later session.
                s.send_app_packet(OP_END_LOOT_REQUEST, &corpse_id.to_le_bytes());
                tracing::info!(
                    "EQ: auto-loot: sent defensive OP_EndLootRequest for corpse_id={} after open-timeout",
                    corpse_id
                );
            }
            LootTickAction::TimedOut => {
                // OP_EndLootRequest got no OP_LootComplete ack in time. Say so honestly instead of
                // fabricating "Looting complete". #414: don't unwedge the queue immediately either
                // — a genuinely-late OP_LootComplete for THIS corpse is still possible, and letting
                // the next corpse open right away is exactly what would let that late reply get
                // misattributed as an "aborted" outcome for the NEXT corpse's still-legitimate
                // session (apply_loot_complete's branch-2 misfire). Enter the same defensive-close
                // quarantine `OpenTimedOut` uses instead; the queue resumes once it resolves.
                let corpse = gs.loot_current_corpse;
                gs.loot_confirmed = false;
                gs.loot_last_activity = None;
                gs.loot_end_requested_at = None;
                gs.loot_defensive_close_at = Some(std::time::Instant::now());
                let msg = format!(
                    "Loot failed — no close confirmation from the server (corpse_id={:?})",
                    corpse
                );
                gs.log_msg("loot", &msg);
                gs.push_event("loot", "failed", "system", true, &msg);
                tracing::warn!("EQ: auto-loot: {}", msg);
                // loot_session_active and loot_current_corpse are deliberately left as-is: the
                // quarantine (loot_tick_action's defensive_close_at branch) needs session_active
                // to stay true so it keeps withholding the next Open() until this resolves.
            }
            LootTickAction::DefensiveCloseTimedOut => {
                // #414: the defensive OP_EndLootRequest itself got no ack either — give up on this
                // corpse entirely. The failure was already reported when OpenTimedOut/TimedOut
                // fired, so there's nothing new to tell the agent; just free the slot.
                let corpse = gs.loot_current_corpse.take();
                gs.loot_session_active = false;
                gs.loot_confirmed = false;
                gs.loot_defensive_close_at = None;
                tracing::warn!(
                    "EQ: auto-loot: defensive OP_EndLootRequest for corpse_id={:?} never acked — giving up, resuming queue",
                    corpse
                );
                gs.loot_queued_at = gs.pending_loot.front().map(|_| std::time::Instant::now());
            }
        }

        if world_reconnect_needed {
            tracing::info!("EQ: zone change approved — reconnecting to world for zone handoff");
            let ok = reconnect_via_world(
                &mut stream, &mut net_rx, &mut gs, &char_name, &world_creds, &net_health,
                &game_state_snapshot,
            ).await;
            if ok {
                let zoned_in = run_zone_entry_handshake(
                    stream.as_mut().unwrap(),
                    net_rx.as_mut().unwrap(),
                    &mut gs,
                    &char_name,
                    &net_health,
                    &game_state_snapshot,
                    ZONE_ENTRY_HANDSHAKE_DEADLINE,
                ).await;
                if !zoned_in {
                    // #335/agent-honesty: the zone never accepted us. run_zone_entry_handshake has
                    // already flagged zone_in_failed + cleared the stale zone; end the phase (an honest
                    // teardown) rather than looping on a wedged, mislabelled connection.
                    tracing::warn!("EQ: zone-in never completed after world reconnect — exiting gameplay");
                    return;
                }
                action_loop.sync_zone_points(&gs);
                last_keepalive = std::time::Instant::now();
                reset_probe_clocks(&net_health);
            } else {
                tracing::warn!("EQ: world reconnect failed — exiting gameplay");
                return;
            }
            continue;
        }

        if let Some((zone_ip, zone_port)) = zone_redirect {
            tracing::info!("EQ: zone transition → {}:{}", zone_ip, zone_port);
            let (new_tx, new_rx) = tokio::sync::mpsc::unbounded_channel::<AppPacket>();
            // Drop old connections (Option::take returns the value, dropping it).
            drop(stream.take());
            drop(net_rx.take());
            // #335: no fixed pre-connect sleep. `EqStream::connect` blocks on OP_SESSION_RESPONSE
            // (re-sending OP_SESSION_REQUEST every SESSION_REQUEST_RETRY) — but that only brings up the
            // UDP SESSION. It does NOT wait for the zone to accept us at the app layer: the zone ACKs
            // our OP_ZoneEntry (sent once, below, in run_zone_entry_handshake) before its app handler
            // runs and may silently drop it if it beat AddAuth. `poll_resend` recovers a genuinely lost
            // (unacked) entry; a wedge past the honest 30s deadline is surfaced as a zone-in failure,
            // NOT papered over with a second OP_ZoneEntry (that would self-kick an admitted-but-slow
            // session). See docs/eq-technical-knowledgebase/zone-entry-{handshake-race,
            // duplicate-on-admitted-client}.md.
            match EqStream::connect(&zone_ip, zone_port, new_tx, net_health.clone()).await {
                Ok(new_stream) => {
                    stream = Some(new_stream);
                    net_rx = Some(new_rx);
                    // The single OP_ZoneEntry send is owned by run_zone_entry_handshake now.
                    let zoned_in = run_zone_entry_handshake(
                        stream.as_mut().unwrap(),
                        net_rx.as_mut().unwrap(),
                        &mut gs,
                        &char_name,
                        &net_health,
                        &game_state_snapshot,
                        ZONE_ENTRY_HANDSHAKE_DEADLINE,
                    ).await;
                    if !zoned_in {
                        // #335/agent-honesty: zone never accepted us; handshake already flagged
                        // zone_in_failed + cleared the stale zone. End the phase honestly.
                        tracing::warn!("EQ: zone transition never completed — exiting gameplay");
                        return;
                    }
                    action_loop.sync_zone_points(&gs);
                    last_keepalive = std::time::Instant::now();
                    reset_probe_clocks(&net_health);
                }
                Err(e) => {
                    tracing::warn!("EQ: zone transition connect failed: {e}");
                    // Can't recover without a stream; exit gameplay phase.
                    return;
                }
            }
            continue;
        }

        let s = stream.as_mut().unwrap();
        if last_keepalive.elapsed() > KEEPALIVE_INTERVAL {
            s.send_keepalive();
            last_keepalive = std::time::Instant::now();
        }

        // ── Active liveness probe (#371) ─────────────────────────────────────
        // Poke the zone MAIN LOOP with a cheap self-consider it must service to answer, so a zone
        // that is still ticking at the socket but not making application progress for us is
        // distinguishable from a merely quiet one. The reply is captured above and turned into
        // `world_responsive` at HTTP read time. Only fires once the world has been application-silent
        // for PROBE_QUIET_THRESHOLD (no load during active play) and at most once per PROBE_INTERVAL.
        // Gated on being in-zone (player_id set); a self-consider is benign and non-disruptive (see
        // the const doc, which also bounds what this signal does and does not catch).
        if gs.player_id != 0 {
            let (last_packet_ago, probe_sent_ago) = {
                let h = net_health.lock().unwrap();
                (h.last_packet.elapsed(), h.last_probe_sent.map(|t| t.elapsed()))
            };
            if should_send_probe(last_packet_ago, probe_sent_ago) {
                s.send_app_packet(OP_CONSIDER,
                    &crate::eq_net::protocol::build_consider_packet(gs.player_id, gs.player_id));
                let mut h = net_health.lock().unwrap();
                record_probe_sent(&mut h, std::time::Instant::now());
                tracing::debug!("EQ: liveness probe — sent self-consider (#371)");
            }
        }

        // Respawn drive (#284): we no longer auto-respawn. While dead, we recover ONLY once the
        // agent asks via POST /v1/lifecycle/respawn (which sets `respawn`). Then reply to the held
        // OP_RespawnWindow (option 0 = bind); if the window wasn't captured, fall back to a proactive
        // bind-select so a requested respawn always recovers (never permanently stuck-dead). Retry
        // every RESPAWN_RETRY_INTERVAL until HP returns. Once alive again, reset all death state.
        if gs.player_dead_since.is_some() {
            let want_respawn = *respawn.lock().unwrap();
            if want_respawn
                && last_respawn_retry.map_or(true, |t| t.elapsed() > RESPAWN_RETRY_INTERVAL)
            {
                let reply = pending_respawn.as_deref()
                    .and_then(respawn_window_reply)
                    .unwrap_or_else(|| build_respawn_select(0));
                s.send_app_packet(OP_RESPAWN_WINDOW, &reply);
                gs.strategy = "Respawning at bind...".into();
                gs.log_msg("combat", "Respawning at bind point.");
                tracing::info!("EQ: respawn requested — sent bind respawn (#284)");
                last_respawn_retry = Some(std::time::Instant::now());
            }
        } else {
            // Alive: clear the one-shot request + held window + retry timer for the next death.
            if last_respawn_retry.is_some() || pending_respawn.is_some() {
                last_respawn_retry = None;
                pending_respawn = None;
            }
            *respawn.lock().unwrap() = false;
        }

        // A cast the server ended (OP_ManaChange keepcasting=0) but never explained — the classic
        // case is a beneficial buff that won't stack, where SpellFinished returns false and
        // StopCasting sends that ManaChange and NOTHING else. Report the unexplained end once the
        // grace window lapses, instead of leaving the agent with no outcome at all. (#348)
        gs.resolve_pending_cast_end();
        // #448 (Migration 3): the unexplained-end above sets `gs.last_cast` from a TIMER, not a packet,
        // so catch that transition here too — an awaited cast whose server-end is never explained
        // resolves to `Unconfirmed` promptly instead of waiting for the next inbound packet.
        action_loop.fulfill_cast(&gs);

        action_loop.tick(s, &mut gs);

        publish_snapshot(&gs, &game_state_snapshot, &net_health);

        sleep(Duration::from_millis(10)).await;
    }
}

/// Clean logout: send OP_Logout, briefly wait for OP_LogoutReply, then send a session-layer
/// disconnect. Returns when done — it does NOT exit the process. The actual process exit happens
/// on the MAIN thread (the render loop's `about_to_wait` exits the winit event loop once the
/// shutdown flag is set, then `main` exits), so the GPU/Wayland teardown is not raced by a
/// background-thread `process::exit()`. EQEmu saves the character; the brief linkdead window is
/// harmless because the next login DropClient-kicks the ghost.
async fn perform_clean_shutdown(
    s:  &mut EqStream,
    rx: &mut UnboundedReceiver<AppPacket>,
) {
    tracing::info!("EQ: clean shutdown requested — sending OP_Logout");
    s.send_app_packet(OP_LOGOUT, &[]);
    // RoF2 has no wire OP_LogoutReply (OP_LogoutReply=0x0000/unused in patch_RoF2.conf), so there is
    // nothing to wait for — the old code always timed out. Just give OP_Logout a brief window to
    // flush to the socket and be processed server-side (character save) before we disconnect.
    let deadline = std::time::Instant::now() + Duration::from_millis(150);
    while std::time::Instant::now() < deadline {
        s.poll_recv();
        s.poll_resend(); // (#254)
        while rx.try_recv().is_ok() {}
        sleep(Duration::from_millis(10)).await;
    }
    s.send_session_disconnect();
    tracing::info!("EQ: sent OP_Logout + OP_SessionDisconnect (process exits on the main thread)");
}

/// After OP_ZONE_CHANGE success=1: reconnect to world, get OP_ZONE_SERVER_INFO, connect to new zone.
/// On success, `stream` and `net_rx` are replaced with the new zone connection.
///
/// `net_health.last_packet` is bumped as real inbound packets are drained here (the world-reconnect leg of a
/// zone handoff), the same way the gameplay loop's drain does — this handoff can take multiple
/// seconds and, with CONN_STALE_SECS=15, would otherwise falsely report the connection as lost
/// while it's mid-transition.
///
/// `game_state_snapshot` is published once per drain pass (#324) so the renderer's view stays live
/// through this leg of the handoff too. This is NOT dead work, even though this loop never calls
/// `apply_packet` itself: the gameplay loop drains and applies packets into `gs` *before* it checks
/// `world_reconnect_needed`, and its own publish sits at the loop bottom — *after* that branch. So
/// `gs` reaches us carrying mutations the renderer has not seen yet, and publishing here flushes
/// them immediately instead of stranding them for the multiple seconds this handoff takes. Any
/// future packet handling added to this loop is then covered for free.
async fn reconnect_via_world(
    stream:              &mut Option<EqStream>,
    net_rx:               &mut Option<UnboundedReceiver<AppPacket>>,
    gs:                   &mut GameState,
    char_name:            &str,
    creds:                &WorldCredentials,
    net_health:           &crate::ipc::NetHealthShared,
    game_state_snapshot:  &crate::ipc::GameStateSnapshot,
) -> bool {
    drop(stream.take());
    drop(net_rx.take());
    // #335: no fixed sleep here. Dropping the old zone socket is local (no graceful disconnect is
    // sent, nothing server-side has to "settle"), and the world server is a separate, always-running
    // process — connecting to it does not reuse anything from the old zone session. The real wait is
    // the event-driven one below: `EqStream::connect` blocks on OP_SESSION_RESPONSE, and the
    // OP_SEND_LOGIN_INFO we then send is a reliable packet retransmitted by `poll_resend` until acked.
    let (world_tx, mut world_rx) = tokio::sync::mpsc::unbounded_channel::<AppPacket>();
    tracing::info!("EQ: reconnecting to world {}:{}", creds.world_host, creds.world_port);
    let mut world_stream = match EqStream::connect(&creds.world_host, creds.world_port, world_tx, net_health.clone()).await {
        Ok(s) => s,
        Err(e) => { tracing::warn!("EQ: world reconnect failed: {e}"); return false; }
    };

    // Send OP_SEND_LOGIN_INFO: lsid\0ls_key\0 padded to SIZE_LOGIN_INFO bytes.
    // zoning=1 at byte 188 tells the world we're mid-zone-transfer, not a fresh login.
    // Without this the world treats us as a fresh session and never sends OP_ZONE_SERVER_INFO.
    let lsid_str = format!("{}\0{}\0", creds.lsid, creds.ls_key);
    let mut login_info = vec![0u8; SIZE_LOGIN_INFO];
    let lb = lsid_str.as_bytes();
    login_info[..lb.len().min(64)].copy_from_slice(&lb[..lb.len().min(64)]);
    login_info[188] = 1; // LoginInfo_S::zoning — signals zone transition reconnect
    world_stream.send_app_packet(OP_SEND_LOGIN_INFO, &login_info);
    tracing::info!("EQ: sent OP_SEND_LOGIN_INFO to world (lsid={}, zoning=1)", creds.lsid);

    // Wait for OP_SEND_CHAR_INFO → send OP_ENTER_WORLD → wait for OP_ZONE_SERVER_INFO
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut char_info_sent = false;
    let mut zone_server: Option<(String, u16)> = None;

    while std::time::Instant::now() < deadline && zone_server.is_none() {
        world_stream.poll_recv();
        world_stream.poll_resend(); // retransmit OP_SEND_LOGIN_INFO/ENTER_WORLD across the handoff (#254)
        while let Ok(packet) = world_rx.try_recv() {
            record_app_packet(&mut net_health.lock().unwrap(), std::time::Instant::now());
            match packet.opcode {
                // In a fresh-login reconnect, world sends OP_SEND_CHAR_INFO as the trigger.
                // In a zoning=1 reconnect, world sends OP_APPROVE_WORLD (RoF2: 0x7499) instead.
                OP_SEND_CHAR_INFO | OP_APPROVE_WORLD if !char_info_sent => {
                    char_info_sent = true;
                    let mut enter_buf = vec![0u8; SIZE_ENTER_WORLD];
                    let nb = char_name.as_bytes();
                    enter_buf[..nb.len().min(64)].copy_from_slice(&nb[..nb.len().min(64)]);
                    world_stream.send_app_packet(OP_ENTER_WORLD, &enter_buf);
                    world_stream.send_app_packet(OP_POST_ENTER_WORLD, &[]);
                    tracing::info!("EQ: zone change: sent OP_ENTER_WORLD to world (trigger=0x{:04x})", packet.opcode);
                }
                OP_ZONE_SERVER_INFO if packet.payload.len() >= SIZE_ZONE_SERVER_INFO => {

                    let info = unsafe { safe_read::<ZoneServerInfo_S>(&packet.payload) };
                    let port = info.port;
                    let ip   = String::from_utf8_lossy(&info.ip)
                        .trim_end_matches('\0')
                        .to_string();
                    let ip = if ip.is_empty() || ip == "0.0.0.0" {
                        creds.world_host.clone()
                    } else { ip };
                    tracing::info!("EQ: zone change: world says new zone at {}:{}", ip, port);
                    zone_server = Some((ip, port));
                }
                _ => {
                    tracing::info!("EQ: zone change world: opcode 0x{:04x} ({} bytes)", packet.opcode, packet.payload.len());
                }
            }
        }
        publish_snapshot(gs, game_state_snapshot, net_health);
        sleep(Duration::from_millis(10)).await;
    }

    drop(world_stream);

    let (zone_ip, zone_port) = match zone_server {
        Some(s) => s,
        None => {
            tracing::info!("EQ: zone change: world did not send OP_ZONE_SERVER_INFO within 30s");
            return false;
        }
    };

    // #335: no fixed sleep before connecting to the new zone. `EqStream::connect` re-sends
    // OP_SESSION_REQUEST every SESSION_REQUEST_RETRY so a cold on-demand zone that has not finished
    // booting its listener is retried quickly rather than padded for — but note that a FAST session
    // handshake here actually pushes OP_ZoneEntry out SOONER and can WIDEN the app-layer AddAuth race
    // (see SESSION_REQUEST_RETRY's doc and `zone-entry-handshake-race.md`). When that race is lost the
    // zone-in is not rescued by re-sending OP_ZoneEntry (that self-kicks an admitted session — see
    // zone-entry-duplicate-on-admitted-client.md); it falls through to the honest 30s failure that the
    // caller's run_zone_entry_handshake raises. This function no longer sends OP_ZoneEntry itself; the
    // single send lives in run_zone_entry_handshake, on the stream we return here.
    tracing::info!("EQ: zone change: connecting to new zone {}:{}", zone_ip, zone_port);
    let (zone_tx, zone_rx) = tokio::sync::mpsc::unbounded_channel::<AppPacket>();
    let zone_stream = match EqStream::connect(&zone_ip, zone_port, zone_tx, net_health.clone()).await {
        Ok(s) => s,
        Err(e) => { tracing::warn!("EQ: zone change: zone connect failed: {e}"); return false; }
    };

    *stream = Some(zone_stream);
    *net_rx = Some(zone_rx);
    true
}

/// Build and send an OP_ZoneEntry (ClientZoneEntry) for `char_name` on `stream`, as a reliable app
/// packet. Called EXACTLY ONCE per zone-server session by `run_zone_entry_handshake` — see that
/// function's doc and `docs/eq-technical-knowledgebase/zone-entry-duplicate-on-admitted-client.md` for
/// why a second app-level ClientZoneEntry on the same session must never be issued (it self-kicks the
/// admitted client via EQEmu's antighost check).
fn send_zone_entry(stream: &mut EqStream, char_name: &str) {
    let mut cze = vec![0u8; SIZE_CLIENT_ZONE_ENTRY];
    let nb = char_name.as_bytes();
    let n = nb.len().min(64);
    cze[4..4 + n].copy_from_slice(&nb[..n]);
    stream.send_app_packet(OP_ZONE_ENTRY, &cze);
}

/// Drives the OP_ZoneEntry → OP_NewZone → OP_Weather → OP_SendExpZonein handshake after connecting to
/// a new zone server. Returns `true` once the zone accepts us (OP_SendExpZonein seen and OP_ClientReady
/// sent); `false` if the deadline elapses first — an HONEST failure the caller must surface, not ignore.
///
/// This function OWNS the OP_ZoneEntry send (both call sites used to send it themselves) and sends it
/// EXACTLY ONCE. It deliberately does NOT app-level re-send a fresh OP_ZoneEntry while waiting for
/// OP_NewZone. That is a hard invariant, not an oversight — see
/// `docs/eq-technical-knowledgebase/zone-entry-duplicate-on-admitted-client.md`: a second
/// ClientZoneEntry delivered on an ALREADY-ADMITTED session (the common case once the first entry was
/// accepted and OP_NewZone is merely slow — e.g. a cold/heavy zone whose bulk spawn+DB load exceeds a
/// couple of seconds) is re-dispatched into `Handle_Connect_OP_ZoneEntry`, whose antighost check
/// `entity_list.GetClientByName` then MATCHES ITSELF (no `client != this` guard) and calls
/// `client->Disconnect()` on the live session — silently kicking a zone-in that had already succeeded.
/// From client-side session state alone we cannot distinguish "first entry app-dropped, needs a nudge"
/// from "first entry admitted, OP_NewZone just slow", and the two demand opposite actions, so the safe
/// rule is: send once, never a second time on this session.
///
/// Genuine WIRE loss of that one OP_ZoneEntry (it never reached the zone, so it is still unacked at the
/// transport layer and no admission happened) is recovered for free by `poll_resend`, which
/// retransmits the SAME datagram verbatim until the session ACKs it — that cannot trigger the antighost
/// self-kick because a never-delivered entry never set `this->name` server-side. The only case left
/// unrecovered is "entry delivered + session-ACKed but app-dropped because AddAuth had not yet landed,
/// and it then never lands" — per `zone-entry-handshake-race.md` that session may simply never auth, so
/// it correctly falls through to the honest [`ZONE_ENTRY_HANDSHAKE_DEADLINE`] failure below rather than
/// being papered over by an unsafe resend.
///
/// `net_health.last_packet` is bumped as real inbound packets are drained here, exactly like the
/// gameplay loop's own drain (`run_gameplay_phase`) does — a slow zone-in (up to `deadline_dur`) must
/// not falsely report the connection as lost while it's healthy and still zoning in.
///
/// `game_state_snapshot` is published once per drain pass (#324), same cadence as the steady-state
/// gameplay loop — without this the renderer never observes `OP_NEW_ZONE` / spawns / etc. as they
/// land here, and stays frozen on the OLD zone's last frame for this entire handshake instead of
/// starting the fade/loading screen the moment `OP_NEW_ZONE` arrives.
///
/// `deadline_dur` is a parameter (not the module const inlined) so tests can drive the timeout path in
/// milliseconds instead of the production 30s.
async fn run_zone_entry_handshake(
    stream:              &mut EqStream,
    net_rx:               &mut UnboundedReceiver<AppPacket>,
    gs:                   &mut GameState,
    char_name:            &str,
    net_health:           &crate::ipc::NetHealthShared,
    game_state_snapshot:  &crate::ipc::GameStateSnapshot,
    deadline_dur:         Duration,
) -> bool {
    // Purge the previous zone's spawns/doors now, before OP_ReqClientSpawn asks for the new zone's
    // stream, and re-arm the once-per-zone-in OP_NewZone apply so the repeat OP_NewZone this
    // handshake provokes can't clear again mid-stream (#322). Also clears any prior zone_in_failed.
    gs.begin_zone_in();

    // The one and ONLY OP_ZoneEntry for this session (see the fn doc — a second one self-kicks).
    // `poll_resend` retransmits this same datagram if it is lost in flight; nothing here ever issues a
    // fresh one.
    send_zone_entry(stream, char_name);
    tracing::info!("EQ: sent zone entry for '{}'", char_name);

    let deadline = std::time::Instant::now() + deadline_dur;
    let mut done_new_zone     = false;
    let mut done_weather      = false;
    let mut done_client_ready = false;

    while std::time::Instant::now() < deadline && !done_client_ready {
        stream.poll_recv();
        stream.poll_resend(); // retransmit the unacked OP_ZoneEntry / ReqClientSpawn during zone-in (#254)
        while let Ok(packet) = net_rx.try_recv() {
            apply_packet(gs, &packet);
            record_app_packet(&mut net_health.lock().unwrap(), std::time::Instant::now());
            match packet.opcode {
                OP_NEW_ZONE if !done_new_zone => {
                    done_new_zone = true;
                    stream.send_app_packet(OP_REQ_CLIENT_SPAWN, &[]);
                    tracing::info!("EQ: new zone '{}' — sent ReqClientSpawn", gs.zone_name);
                }
                OP_WEATHER if !done_weather => {
                    done_weather = true;
                    stream.send_app_packet(OP_REQ_NEW_ZONE, &[]);
                    tracing::info!("EQ: zone weather — sent ReqNewZone");
                }
                OP_SEND_EXP_ZONE_IN if !done_client_ready => {
                    done_client_ready = true;
                    stream.send_app_packet(OP_SEND_EXP_ZONE_IN, &[]);
                    stream.send_app_packet(OP_CLIENT_READY, &[]);
                    tracing::info!("EQ: zone transition complete — now in '{}'", gs.zone_name);
                }
                _ => {}
            }
        }

        publish_snapshot(gs, game_state_snapshot, net_health);
        sleep(Duration::from_millis(10)).await;
    }

    if !done_client_ready {
        // HONEST failure (#335/agent-honesty). Do NOT return silently leaving `connected: true` + the
        // OLD `zone_name` reported as current — that is the #343/#470 confident-falsehood anti-pattern.
        // Raise an explicit, distinguishable flag AND clear the stale zone so no agent reads the zone
        // we came from as where we are. The caller tears the session down after this (an honest end).
        // We do NOT try to "rescue" this with a second OP_ZoneEntry — per the fn doc that would risk
        // self-disconnecting an admitted-but-slow session; an honest failure is the correct backstop.
        gs.zone_in_failed = true;
        gs.zone_name.clear();
        publish_snapshot(gs, game_state_snapshot, net_health);
        tracing::warn!(
            "EQ: zone entry handshake TIMED OUT (new_zone={done_new_zone} weather={done_weather}) — \
             flagged zone_in_failed, cleared stale zone_name",
        );
        return false;
    }
    true
}

// ── Camp ────────────────────────────────────────────────────────────────────────
//
// Camping is the only clean way off the server: `OP_Camp` arms a ~29s timer in EQEmu
// (`Handle_OP_Camp`), after which the character is saved + removed with NO linkdead. The client
// must stay connected (keepalives flowing) until that timer fires, then disconnect. We wait
// `CAMP_DURATION` (just over the server's 29s) before triggering shutdown. A camp can be cancelled
// before then by toggling it — server-side a Standing `OP_SpawnAppearance` disables the camp timer.

/// How long the client stays connected after `OP_Camp` before disconnecting. Must exceed EQEmu's
/// 29s server-side camp timer so `instalog` is set first (otherwise the disconnect is linkdead).
pub const CAMP_DURATION: Duration = Duration::from_secs(30);

/// The action a camp command resolves to, given the current camp state.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum CampAction {
    /// A camp was not running and now begins — send `OP_Camp`.
    Started,
    /// A camp was running and is now cancelled — send a Standing `OP_SpawnAppearance`.
    Cancelled,
    /// `Start` while already camping — do nothing (idempotent, used by /exit).
    NoOp,
}

/// Pure decision for a camp command. `current` is the live camp deadline (`None` = not camping).
/// Returns the new deadline state and the action to take. `Toggle` starts or cancels; `Start` only
/// ever starts (never cancels an in-progress camp), so repeated /exit calls don't abort the camp.
pub fn camp_apply(
    cmd: crate::ipc::CampCmd,
    current: Option<std::time::Instant>,
    now: std::time::Instant,
    dur: Duration,
) -> (Option<std::time::Instant>, CampAction) {
    use crate::ipc::CampCmd;
    match (cmd, current) {
        (_, None)                  => (Some(now + dur), CampAction::Started),
        (CampCmd::Toggle, Some(_)) => (None, CampAction::Cancelled),
        (CampCmd::Start, Some(d))  => (Some(d), CampAction::NoOp),
    }
}

/// Whether an in-progress camp has reached its disconnect deadline.
pub fn camp_expired(current: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    matches!(current, Some(d) if now >= d)
}

// ── Auto-loot state machine (#346) ────────────────────────────────────────────
// The invariant this exists to enforce: a success message ("Looting complete") may ONLY be
// emitted from an inbound-packet handler (OP_LootComplete — see apply_loot_complete in
// packet_handler.rs), never from a timer or at send time. `loot_tick_action` therefore has no
// access to `GameState` at all — it can't call `log_msg`/`push_event` even by accident — it only
// ever decides what packet (if any) the gameplay loop should send next.

/// Delay after a corpse is queued before sending OP_LootRequest (lets the server register the
/// corpse as lootable).
const LOOT_OPEN_DELAY_MS: u128 = 500;
/// How long to wait for item echoes to go quiet before asking the server to close a confirmed
/// session (send OP_EndLootRequest).
const LOOT_INACTIVITY_SECS: f32 = 2.0;
/// How long to wait for OP_LootComplete after sending OP_EndLootRequest before giving up and
/// reporting a failure instead of wedging the queue forever.
const LOOT_END_TIMEOUT_SECS: f32 = 5.0;
/// How long to wait for the server's OP_MoneyOnCorpse accept/refuse ack after sending
/// OP_LootRequest before giving up (#370). Every server code path replies immediately (see
/// #370's issue body — no known silent-drop path), so this only fires on genuine packet loss on
/// the unreliable channel; it exists purely so that loss can never wedge `pending_loot` forever.
/// Sized the same as `LOOT_END_TIMEOUT_SECS` (its mirror-image timeout on the other side of a
/// confirmed session) — both bound "waiting on one specific server ack" and both fire well after
/// any real round trip but long before an agent would give up waiting on its own.
const LOOT_OPEN_TIMEOUT_SECS: f32 = 5.0;
/// #414: how long to wait for the server to ack our defensive `OP_EndLootRequest` (sent after
/// giving up on a corpse via `OpenTimedOut` or `TimedOut`) before giving up on THAT too and
/// letting the queue advance regardless. Bounds the window a stray, still-outstanding ack for the
/// abandoned corpse can be safely absorbed rather than risking it land on the next corpse instead.
const LOOT_DEFENSIVE_CLOSE_TIMEOUT_SECS: f32 = 5.0;

/// What the gameplay loop should do this tick for the auto-loot pipeline.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LootTickAction {
    /// Nothing to do.
    None,
    /// Send `OP_LootRequest` for this queued corpse.
    Open(u32),
    /// Item echoes have gone quiet on a CONFIRMED session — send `OP_EndLootRequest` for this
    /// corpse (payload must be its spawn_id, not empty — see `OP_END_LOOT_REQUEST`'s doc).
    Close(u32),
    /// `OP_LootRequest` was sent but no `OP_MoneyOnCorpse` accept/refuse ack arrived within the
    /// timeout (#370) — report a failure and unwedge the queue. Distinct from `TimedOut`: this
    /// session was never even confirmed as open, so treating it as a `Close` would falsely read
    /// as "corpse opened and was empty" (the exact #346 lie this state machine exists to forbid).
    OpenTimedOut(u32),
    /// `OP_EndLootRequest` was sent but no `OP_LootComplete` arrived within the timeout — report a
    /// failure and unwedge the queue; never silently claim "complete".
    TimedOut,
    /// #414: the defensive `OP_EndLootRequest` we sent after giving up on a corpse (via
    /// `OpenTimedOut` or `TimedOut`) itself got no `OP_LootComplete` ack within
    /// `LOOT_DEFENSIVE_CLOSE_TIMEOUT_SECS`. Give up entirely and let the queue advance — the
    /// failure was already reported when the original timeout fired, so this carries no new
    /// user-facing message.
    DefensiveCloseTimedOut,
}

/// The bits of loot state `loot_tick_action` needs, gathered by value so the decision is pure and
/// unit-testable without a live `GameState`/stream (mirrors `camp_apply`).
#[derive(Debug, Clone, Copy, Default)]
pub struct LootTickState {
    /// `gs.loot_session_active` — true from the moment OP_LootRequest is sent.
    pub session_active: bool,
    /// `gs.loot_confirmed` — true only once the server accepted the request.
    pub confirmed: bool,
    /// `gs.loot_current_corpse` — the corpse the open/confirmed session is against, if any.
    pub current_corpse: Option<u32>,
    /// `gs.loot_queued_at` — when the current queue head was first queued.
    pub queued_at: Option<std::time::Instant>,
    /// `gs.pending_loot.front()` — the next corpse waiting to be opened, if any.
    pub pending_front: Option<u32>,
    /// `gs.loot_last_activity` — last time a loot-related packet arrived for this session.
    pub last_activity: Option<std::time::Instant>,
    /// `gs.loot_end_requested_at` — when OP_EndLootRequest was sent, awaiting OP_LootComplete.
    pub end_requested_at: Option<std::time::Instant>,
    /// `gs.loot_defensive_close_at` — #414: when a defensive/give-up `OP_EndLootRequest` was sent
    /// for a corpse we're abandoning (via `OpenTimedOut` or `TimedOut`); withholds the next
    /// corpse's `Open` until this resolves or itself times out.
    pub defensive_close_at: Option<std::time::Instant>,
}

/// Pure decision for one gameplay tick of the auto-loot pipeline. See `LootTickAction` for what
/// each outcome means; see the module-level comment above for why this function must never touch
/// `GameState` or emit a message itself.
pub fn loot_tick_action(state: &LootTickState, now: std::time::Instant) -> LootTickAction {
    if !state.session_active {
        let ready = state.queued_at
            .map(|t| now.duration_since(t).as_millis() >= LOOT_OPEN_DELAY_MS)
            .unwrap_or(false);
        if ready {
            if let Some(id) = state.pending_front {
                return LootTickAction::Open(id);
            }
        }
        return LootTickAction::None;
    }
    if !state.confirmed {
        // #414: we've given up on this corpse (OpenTimedOut, or TimedOut on the confirmed-close
        // side re-entering here after clearing `confirmed`) and sent a defensive/idempotent
        // OP_EndLootRequest to release its server-side lock. Wait for that to resolve — bounded,
        // so a lost defensive ack can't wedge the queue either — before letting the next corpse
        // open. This withholding is what keeps a still-outstanding stray ack for THIS corpse from
        // ever landing on a session for a different one.
        if let Some(sent) = state.defensive_close_at {
            if now.duration_since(sent).as_secs_f32() > LOOT_DEFENSIVE_CLOSE_TIMEOUT_SECS {
                return LootTickAction::DefensiveCloseTimedOut;
            }
            return LootTickAction::None;
        }
        // Sent but not yet accepted/refused by the server — normally wait. A refusal closes
        // `session_active` immediately (see apply_money_on_corpse), so reaching here just means
        // the accept/refuse ack hasn't landed *yet*. But if OP_MoneyOnCorpse is lost entirely on
        // the wire, "yet" never arrives — bound the wait so a lost ack can't wedge every later
        // corpse behind this one forever (#370).
        if let Some(t) = state.last_activity {
            if now.duration_since(t).as_secs_f32() > LOOT_OPEN_TIMEOUT_SECS {
                if let Some(id) = state.current_corpse {
                    return LootTickAction::OpenTimedOut(id);
                }
            }
        }
        return LootTickAction::None;
    }
    if let Some(sent) = state.end_requested_at {
        if now.duration_since(sent).as_secs_f32() > LOOT_END_TIMEOUT_SECS {
            return LootTickAction::TimedOut;
        }
        return LootTickAction::None;
    }
    if let Some(t) = state.last_activity {
        if now.duration_since(t).as_secs_f32() > LOOT_INACTIVITY_SECS {
            if let Some(id) = state.current_corpse {
                return LootTickAction::Close(id);
            }
        }
    }
    LootTickAction::None
}

/// Apply a `LootTickAction::OpenTimedOut` outcome to `GameState`: the server never acked our
/// OP_LootRequest with an OP_MoneyOnCorpse accept/refuse (lost on the wire, #370). Report it
/// honestly — never a silent forever-wait, never a fabricated success.
///
/// The event's `kind` is **`loot_open_timeout`**, deliberately DISTINCT from the confirmed-side
/// `TimedOut` arm's `failed` kind (and from success/abort/normal-close). `/v1/events/loot` exposes
/// `kind` as the field agents dispatch on, so "corpse never opened" (here) must be machine-
/// separable from "corpse opened but never closed" — not merely different free text (#370 review).
///
/// #414: this does NOT fully clear the session — `loot_current_corpse` and `loot_session_active`
/// stay as they were, and `loot_defensive_close_at` is set instead. `OP_MoneyOnCorpse` carries no
/// corpse id (verified — see docs/eq-technical-knowledgebase/loot-protocol.md), so a "lost" ack
/// might really just be LATE rather than lost; if it turns up right after we naively opened the
/// next queued corpse, it would land on that corpse's session instead (the exact bug #414 reports).
/// Leaving `loot_current_corpse` pointing at THIS corpse until the caller's defensive
/// `OP_EndLootRequest` resolves (see `loot_tick_action`'s `defensive_close_at` branch) keeps
/// `apply_money_on_corpse`'s stale-ack gate correctly closed until then.
fn apply_loot_open_timeout(gs: &mut GameState, corpse_id: u32) {
    gs.loot_confirmed = false;
    gs.loot_last_activity = None;
    gs.loot_end_requested_at = None;
    gs.loot_defensive_close_at = Some(std::time::Instant::now());
    let msg = format!(
        "Loot failed — no response from the server opening corpse_id={}",
        corpse_id
    );
    gs.log_msg("loot", &msg);
    gs.push_event("loot", "loot_open_timeout", "system", true, &msg);
    tracing::warn!("EQ: auto-loot: {}", msg);
    // Do NOT reset queued_at / advance the queue here — the defensive-close quarantine
    // (loot_defensive_close_at) withholds the next Open() until it resolves.
}

/// Publish the network thread's `GameState` for lock-free reads by the render/HTTP threads. Called
/// once per gameplay tick, after every mutation for that tick (packet-applied and `ActionLoop::tick`'s
/// own writes) has landed — see the call site in `run_gameplay_phase`.
///
/// Store only on a real change so the published Arc's identity is a complete activity signal: the
/// render thread wakes on ANY network-thread mutation (inbound packet OR client-initiated request
/// handled by `ActionLoop::tick`), and a genuinely idle world lets the loop sleep (see
/// `App::poll_external` in app.rs, which drives its wake decision off `Arc::ptr_eq` against this
/// snapshot).
///
/// `net_health.last_tick` is bumped **unconditionally**, even when the state is byte-identical: it is not a
/// change signal but a *liveness* signal ("the thread that owns GameState is still running"). HTTP
/// reads turn it into `snapshot_age_ms`, so a wedged or dead network thread self-identifies instead
/// of letting a frozen snapshot be served as though it were live (#343). It must therefore stay
/// outside the `!=` guard.
pub fn publish_snapshot(
    gs:         &GameState,
    snapshot:   &crate::ipc::GameStateSnapshot,
    net_health: &crate::ipc::NetHealthShared,
) {
    if **snapshot.load() != *gs {
        snapshot.store(Arc::new(gs.clone()));
    }
    net_health.lock().unwrap().last_tick = std::time::Instant::now();
}

#[cfg(test)]
mod camp_tests {
    use super::*;
    use crate::ipc::CampCmd;
    use std::time::{Duration as Dur, Instant};

    #[test]
    fn start_when_idle_begins_camp() {
        let now = Instant::now();
        let (next, action) = camp_apply(CampCmd::Start, None, now, Dur::from_secs(30));
        assert_eq!(action, CampAction::Started);
        assert_eq!(next, Some(now + Dur::from_secs(30)));
    }

    #[test]
    fn toggle_when_idle_begins_camp() {
        let now = Instant::now();
        let (next, action) = camp_apply(CampCmd::Toggle, None, now, Dur::from_secs(30));
        assert_eq!(action, CampAction::Started);
        assert_eq!(next, Some(now + Dur::from_secs(30)));
    }

    #[test]
    fn toggle_while_camping_cancels() {
        let now = Instant::now();
        let deadline = now + Dur::from_secs(10);
        let (next, action) = camp_apply(CampCmd::Toggle, Some(deadline), now, Dur::from_secs(30));
        assert_eq!(action, CampAction::Cancelled);
        assert_eq!(next, None);
    }

    #[test]
    fn start_while_camping_is_noop_and_keeps_deadline() {
        // /exit calling Start twice must not cancel or extend the running camp.
        let now = Instant::now();
        let deadline = now + Dur::from_secs(10);
        let (next, action) = camp_apply(CampCmd::Start, Some(deadline), now, Dur::from_secs(30));
        assert_eq!(action, CampAction::NoOp);
        assert_eq!(next, Some(deadline));
    }

    #[test]
    fn not_expired_before_deadline() {
        let now = Instant::now();
        assert!(!camp_expired(Some(now + Dur::from_secs(5)), now));
    }

    #[test]
    fn expired_at_or_after_deadline() {
        let now = Instant::now();
        assert!(camp_expired(Some(now - Dur::from_millis(1)), now));
        assert!(camp_expired(Some(now), now));
    }

    #[test]
    fn idle_never_expires() {
        assert!(!camp_expired(None, Instant::now()));
    }
}

/// Regression tests for #346 ("Looting complete" was a 2s timer, not an outcome). These pin down
/// `loot_tick_action`'s decisions; the actual "Looting complete"/"loot refused" MESSAGES are
/// covered separately in packet_handler.rs (apply_loot_complete / apply_money_on_corpse), since
/// by design this function cannot emit them at all.
#[cfg(test)]
mod loot_tick_tests {
    use super::*;
    use std::time::{Duration as Dur, Instant};

    #[test]
    fn idle_with_no_queue_does_nothing() {
        let now = Instant::now();
        let st = LootTickState::default();
        assert_eq!(loot_tick_action(&st, now), LootTickAction::None);
    }

    #[test]
    fn queued_but_not_yet_500ms_does_nothing() {
        let now = Instant::now();
        let st = LootTickState {
            queued_at: Some(now - Dur::from_millis(100)),
            pending_front: Some(9),
            ..Default::default()
        };
        assert_eq!(loot_tick_action(&st, now), LootTickAction::None);
    }

    #[test]
    fn queued_past_500ms_opens_the_front_corpse() {
        let now = Instant::now();
        let st = LootTickState {
            queued_at: Some(now - Dur::from_millis(600)),
            pending_front: Some(9),
            ..Default::default()
        };
        assert_eq!(loot_tick_action(&st, now), LootTickAction::Open(9));
    }

    /// THE #346 REGRESSION. The old code closed the session — and logged "Looting complete" —
    /// after 2s of silence following the SEND of OP_LootRequest, whether or not the server ever
    /// accepted it. A corpse that never opened must not look like a corpse that opened and was
    /// empty, so an unconfirmed session must never time its way into a `Close` — it may only ever
    /// resolve via `OpenTimedOut` (see below), a distinct, explicit failure.
    #[test]
    fn active_but_unconfirmed_session_waits_within_open_timeout() {
        let now = Instant::now();
        let st = LootTickState {
            session_active: true,
            confirmed: false, // server never sent an accepting OP_MoneyOnCorpse — yet
            current_corpse: Some(9),
            last_activity: Some(now - Dur::from_secs(1)),
            ..Default::default()
        };
        assert_eq!(loot_tick_action(&st, now), LootTickAction::None,
            "an unconfirmed session must wait for the server's accept/refuse ack, not close early");
    }

    /// THE #370 FIX. If `OP_MoneyOnCorpse` never arrives at all (lost on the wire), the old code
    /// returned `None` unconditionally for an unconfirmed session — forever. That silently wedges
    /// `pending_loot`: every corpse queued after this one waits behind a session that can never
    /// resolve, and the agent is never told why. Past `LOOT_OPEN_TIMEOUT_SECS` it must instead
    /// resolve to an explicit, distinguishable failure — NOT `None` (permanently pending) and NOT
    /// `Close` (which would falsely read as "corpse opened and was empty", the #346 lie).
    #[test]
    fn active_but_unconfirmed_session_past_open_timeout_reports_explicit_failure() {
        let now = Instant::now();
        let st = LootTickState {
            session_active: true,
            confirmed: false, // OP_MoneyOnCorpse never arrived — ack lost
            current_corpse: Some(9),
            last_activity: Some(now - Dur::from_secs(3600)),
            ..Default::default()
        };
        assert_eq!(loot_tick_action(&st, now), LootTickAction::OpenTimedOut(9),
            "a lost accept/refuse ack must unwedge into an explicit failure, never stay pending forever");
    }

    /// #370 REVIEW. The `OpenTimedOut` outcome must be MACHINE-distinguishable from the confirmed-
    /// side `TimedOut` (which pushes kind `failed`) — agents dispatch on the event's `kind`, not its
    /// free text, so "corpse never opened" and "corpse opened but never closed" must not collide on
    /// the same kind. This pins the distinct kind AND the session teardown in one place.
    #[test]
    fn open_timeout_pushes_a_distinct_event_kind_and_clears_the_session() {
        let mut gs = GameState::new();
        gs.loot_session_active = true;
        gs.loot_confirmed = false;
        gs.loot_current_corpse = Some(9);
        gs.loot_last_activity = Some(Instant::now());

        apply_loot_open_timeout(&mut gs, 9);

        let ev = gs.chat_events.back().expect("an open-timeout must push an agent-visible event");
        assert_eq!(ev.category, "loot");
        assert_eq!(ev.kind, "loot_open_timeout",
            "an unconfirmed-open timeout must carry its OWN kind so an agent filtering on kind can \
             separate it from the confirmed-side `failed`/`complete`/`aborted` outcomes");
        assert_ne!(ev.kind, "failed", "must not collide with the confirmed-side TimedOut kind");

        assert!(!gs.loot_confirmed);
        assert_eq!(gs.loot_last_activity, None);
        assert_eq!(gs.loot_end_requested_at, None);
        // #414: the session is NOT fully torn down yet — `loot_current_corpse`/`loot_session_active`
        // stay put and a defensive-close quarantine begins, so a still-outstanding late
        // OP_MoneyOnCorpse for corpse 9 can't be misattributed to whatever corpse opens next.
        assert!(gs.loot_session_active, "stays active to withhold the next corpse's Open (#414)");
        assert_eq!(gs.loot_current_corpse, Some(9), "retained until the defensive close resolves (#414)");
        assert!(gs.loot_defensive_close_at.is_some(), "a defensive close-and-wait must begin (#414)");
    }

    /// #414. Once a corpse's open-ack has timed out and a defensive close is pending, the queue
    /// must NOT open the next corpse — otherwise a still-outstanding late ack for the abandoned
    /// corpse could land on the new one's session instead.
    #[test]
    fn defensive_close_pending_withholds_the_next_open() {
        let now = Instant::now();
        let st = LootTickState {
            session_active: true,
            confirmed: false,
            current_corpse: Some(9),
            defensive_close_at: Some(now - Dur::from_secs(1)),
            pending_front: Some(11), // corpse B queued right behind corpse A (9)
            ..Default::default()
        };
        assert_eq!(loot_tick_action(&st, now), LootTickAction::None,
            "must not open corpse 11 while corpse 9's defensive close is still outstanding");
    }

    /// #414. If the defensive close itself never gets acked either, give up rather than wedge the
    /// queue forever.
    #[test]
    fn defensive_close_past_timeout_gives_up() {
        let now = Instant::now();
        let st = LootTickState {
            session_active: true,
            confirmed: false,
            current_corpse: Some(9),
            defensive_close_at: Some(now - Dur::from_secs(6)),
            pending_front: Some(11),
            ..Default::default()
        };
        assert_eq!(loot_tick_action(&st, now), LootTickAction::DefensiveCloseTimedOut);
    }

    #[test]
    fn confirmed_session_idle_past_2s_asks_to_close_the_current_corpse() {
        let now = Instant::now();
        let st = LootTickState {
            session_active: true,
            confirmed: true,
            current_corpse: Some(9),
            last_activity: Some(now - Dur::from_secs(3)),
            ..Default::default()
        };
        assert_eq!(loot_tick_action(&st, now), LootTickAction::Close(9));
    }

    #[test]
    fn confirmed_session_within_2s_of_activity_does_nothing() {
        let now = Instant::now();
        let st = LootTickState {
            session_active: true,
            confirmed: true,
            current_corpse: Some(9),
            last_activity: Some(now - Dur::from_millis(500)),
            ..Default::default()
        };
        assert_eq!(loot_tick_action(&st, now), LootTickAction::None);
    }

    #[test]
    fn end_requested_and_within_timeout_waits_for_loot_complete() {
        let now = Instant::now();
        let st = LootTickState {
            session_active: true,
            confirmed: true,
            current_corpse: Some(9),
            last_activity: Some(now - Dur::from_secs(3)),
            end_requested_at: Some(now - Dur::from_secs(1)),
            ..Default::default()
        };
        assert_eq!(loot_tick_action(&st, now), LootTickAction::None);
    }

    /// If the server's OP_LootComplete never shows up, the loop must say so (TimedOut) instead of
    /// quietly declaring success or wedging the queue forever.
    #[test]
    fn end_requested_past_timeout_reports_timed_out() {
        let now = Instant::now();
        let st = LootTickState {
            session_active: true,
            confirmed: true,
            current_corpse: Some(9),
            last_activity: Some(now - Dur::from_secs(10)),
            end_requested_at: Some(now - Dur::from_secs(6)),
            ..Default::default()
        };
        assert_eq!(loot_tick_action(&st, now), LootTickAction::TimedOut);
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    fn health() -> crate::ipc::NetHealthShared {
        Arc::new(std::sync::Mutex::new(crate::ipc::NetHealth::default()))
    }

    /// #343: `net_tick` is a LIVENESS clock, not a change signal. It must advance on every publish
    /// — including a quiet tick where the state is byte-identical and the Arc is deliberately not
    /// republished — because `snapshot_age_ms` is how an agent learns that OUR network thread is
    /// still running. If it only ticked on change, an idle client would look wedged.
    #[test]
    fn publish_snapshot_bumps_net_tick_even_when_state_is_unchanged() {
        let gs = GameState::new();
        let snapshot: crate::ipc::GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(gs.clone()));
        let nh = health();
        nh.lock().unwrap().last_tick =
            std::time::Instant::now() - std::time::Duration::from_secs(60);

        publish_snapshot(&gs, &snapshot, &nh);

        let age = nh.lock().unwrap().last_tick.elapsed();
        assert!(age < std::time::Duration::from_secs(1),
            "an idle-but-alive network tick must refresh net_tick (age was {age:?}) — otherwise \
             `snapshot_age_ms` would report a healthy idle client as a dead one (#343)");
    }

    #[test]
    fn publish_snapshot_reflects_latest_state_independent_of_later_mutation() {
        let snapshot: crate::ipc::GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(GameState::new()));

        let mut gs = GameState::new();
        gs.player_name = "Aldric".to_string();
        publish_snapshot(&gs, &snapshot, &health());
        assert_eq!(snapshot.load().player_name, "Aldric");

        // Mutating the source after publishing must not retroactively change the already-published
        // snapshot — each publish is an independent, immutable clone.
        gs.player_name = "Mutated".to_string();
        assert_eq!(snapshot.load().player_name, "Aldric");

        // A second publish replaces the snapshot wholesale.
        publish_snapshot(&gs, &snapshot, &health());
        assert_eq!(snapshot.load().player_name, "Mutated");
    }

    /// Fix (single-owner GameState wake signal): a no-op publish (state genuinely unchanged since
    /// the last publish) must NOT replace the Arc — the render loop's `poll_external` treats a new
    /// Arc identity as "something happened" and would otherwise spuriously wake every tick even in
    /// an idle world.
    #[test]
    fn publish_snapshot_keeps_same_arc_when_state_unchanged() {
        let gs = GameState::new();
        let snapshot: crate::ipc::GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(gs.clone()));

        publish_snapshot(&gs, &snapshot, &health());
        let before = snapshot.load_full();

        // Same state, re-published (e.g. a quiet tick with no inbound packet and no client request).
        publish_snapshot(&gs, &snapshot, &health());
        let after = snapshot.load_full();

        assert!(Arc::ptr_eq(&before, &after), "unchanged state must not republish a new Arc");
    }

    /// Counterpart: a real mutation (standing in for either an inbound packet or a client-initiated
    /// change made by `ActionLoop::tick`, e.g. `gs.sitting`) DOES publish a new Arc, and the new
    /// snapshot reflects it.
    #[test]
    fn publish_snapshot_publishes_new_arc_when_state_changed() {
        let mut gs = GameState::new();
        let snapshot: crate::ipc::GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(gs.clone()));

        publish_snapshot(&gs, &snapshot, &health());
        let before = snapshot.load_full();

        // Simulate a client-initiated mutation with no inbound packet (e.g. POST /v1/interact/sit).
        gs.sitting = true;
        publish_snapshot(&gs, &snapshot, &health());
        let after = snapshot.load_full();

        assert!(!Arc::ptr_eq(&before, &after), "a real state change must publish a new Arc");
        assert!(after.sitting, "the new snapshot must reflect the mutation");
    }
}

#[cfg(test)]
mod zone_entry_handshake_publish_tests {
    use super::*;
    use crate::eq_net::transport::test_stream;

    /// Minimal RoF2 NewZone_Struct payload: everything zeroed except `zone_short_name` at offset 64
    /// (see `apply_new_zone` in packet_handler.rs) — enough for `apply_packet` to set `gs.zone_name`.
    fn new_zone_payload(name: &str) -> Vec<u8> {
        let mut p = vec![0u8; SIZE_NEW_ZONE];
        let nb = name.as_bytes();
        p[64..64 + nb.len()].copy_from_slice(nb);
        p
    }

    /// Regression test for #324: before this fix, `run_zone_entry_handshake` mutated `gs` on every
    /// inbound packet but never called `publish_snapshot` — the renderer only saw the result once the
    /// WHOLE handshake (NEW_ZONE → WEATHER → SEND_EXP_ZONE_IN) returned control to the caller, so it
    /// stayed frozen on the old zone for the entire handoff. Drive the handshake with only OP_NEW_ZONE
    /// (deliberately withholding OP_WEATHER/OP_SEND_EXP_ZONE_IN so the handshake never completes) and
    /// assert the published snapshot picks up the new zone name anyway — proving the publish happens
    /// per drain pass, not only at the end.
    ///
    /// This needs a real `EqStream` (its `poll_recv`/`poll_resend`/`send_app_packet` calls aren't
    /// mockable at a lower level), so it uses `transport::test_stream` — a dummy stream wired to a
    /// closed local UDP peer (outbound sends are harmless no-ops) — rather than a live session
    /// handshake. `net_rx` is a separate, test-owned channel (the function takes it independently of
    /// `stream`), so packets are injected directly with no wire encoding needed.
    #[tokio::test]
    async fn publishes_zone_name_as_op_new_zone_lands_not_only_at_handshake_end() {
        let (mut stream, _unused_rx) = test_stream(0, 0).await;
        let (tx, mut net_rx) = tokio::sync::mpsc::unbounded_channel::<AppPacket>();

        let mut gs = GameState::new();
        gs.zone_name = "oldzone".to_string();
        let snapshot: crate::ipc::GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(gs.clone()));
        let last_inbound: crate::ipc::NetHealthShared =
            Arc::new(std::sync::Mutex::new(crate::ipc::NetHealth::default()));

        let snapshot_bg     = snapshot.clone();
        let last_inbound_bg = last_inbound.clone();
        let handle = tokio::spawn(async move {
            // Long deadline: this test is about the per-pass publish, not the timeout path, so the 30s
            // deadline is never reached (WEATHER/EXP_ZONE_IN withheld).
            run_zone_entry_handshake(
                &mut stream, &mut net_rx, &mut gs, "Tester", &last_inbound_bg, &snapshot_bg,
                Duration::from_secs(30),
            ).await;
        });

        tx.send(AppPacket { opcode: OP_NEW_ZONE, payload: new_zone_payload("newzone") }).unwrap();

        // Give the 10ms-cadence drain loop a handful of ticks to pick up the packet and publish —
        // well short of the 30s handshake deadline, which is never reached (OP_WEATHER/
        // OP_SEND_EXP_ZONE_IN are withheld on purpose).
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if snapshot.load().zone_name == "newzone" { break; }
            assert!(std::time::Instant::now() < deadline,
                "snapshot never picked up OP_NEW_ZONE's zone_name — publish_snapshot isn't being \
                 called inside the handshake's drain loop (#324)");
            sleep(Duration::from_millis(10)).await;
        }

        handle.abort();
    }

    fn fresh_gs_snapshot() -> (GameState, crate::ipc::GameStateSnapshot, crate::ipc::NetHealthShared) {
        let mut gs = GameState::new();
        gs.zone_name = "oldzone".to_string();
        let snapshot: crate::ipc::GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(gs.clone()));
        let health: crate::ipc::NetHealthShared =
            Arc::new(std::sync::Mutex::new(crate::ipc::NetHealth::default()));
        (gs, snapshot, health)
    }

    /// #335 (the blocker fix): the antighost self-disconnect invariant. A SECOND ClientZoneEntry on an
    /// already-admitted session is re-dispatched into `Handle_Connect_OP_ZoneEntry`, whose antighost
    /// lookup `entity_list.GetClientByName` then matches ITSELF (no `client != this` guard) and calls
    /// `client->Disconnect()` on the live stream — silently kicking a zone-in that had already
    /// succeeded (see `docs/eq-technical-knowledgebase/zone-entry-duplicate-on-admitted-client.md`).
    /// The hard client-side rule is therefore: send OP_ZoneEntry EXACTLY ONCE per session, never a
    /// second app-level copy while waiting for OP_NewZone — because from session state we cannot tell
    /// "first was app-dropped" from "first admitted, OP_NewZone just slow (cold/heavy zone)".
    ///
    /// Drive the handshake against a peer that never sends OP_NewZone, over a window comfortably longer
    /// than any plausible resend cadence (the KB's cited 2.5s), and assert EXACTLY ONE OP_ZoneEntry
    /// ever reached the wire. Mutation check: re-introduce ANY app-level `send_zone_entry` retry in the
    /// loop and the count climbs above 1 → this goes RED. (A retransmit of the SAME unacked datagram by
    /// `poll_resend` is fine and is not counted here — `sent_app_packets` reports distinct tracked
    /// sends; a genuine app-level re-send uses a NEW sequence and would appear as a 2nd entry.)
    #[tokio::test]
    async fn never_sends_a_second_zone_entry_on_a_single_session() {
        let (mut stream, _unused_rx) = test_stream(0, 0).await;
        // A test-owned channel with NO sender activity: OP_NewZone never arrives, modelling BOTH the
        // app-dropped case AND the admitted-but-slow case (indistinguishable from here — which is the
        // whole point: the client must behave safely without knowing which it is).
        let (_tx, mut net_rx) = tokio::sync::mpsc::unbounded_channel::<AppPacket>();
        let (mut gs, snapshot, health) = fresh_gs_snapshot();

        let ok = run_zone_entry_handshake(
            &mut stream, &mut net_rx, &mut gs, "Tester", &health, &snapshot,
            Duration::from_millis(3200), // > the 2.5s the KB warns a blind resend could fire at
        ).await;

        assert!(!ok, "a zone-in that never gets OP_NewZone must report failure, not success");
        let zone_entries = stream
            .sent_app_packets()
            .into_iter()
            .filter(|(op, _)| *op == OP_ZONE_ENTRY)
            .count();
        assert_eq!(
            zone_entries, 1,
            "run_zone_entry_handshake must send OP_ZoneEntry EXACTLY ONCE per session — a second \
             app-level copy self-disconnects an admitted client via EQEmu's antighost check \
             (zone-entry-duplicate-on-admitted-client.md); saw {zone_entries} in a 3.2s window",
        );
    }

    /// #335/agent-honesty: when the handshake times out with no OP_NewZone at all, it must NOT return
    /// silently leaving `connected: true` + the OLD `zone_name` (the #343/#470 confident-falsehood
    /// anti-pattern). It must raise the explicit `zone_in_failed` flag AND clear the stale zone, and
    /// PUBLISH that so an agent reads an honest failure. Mutation check: delete the honest-failure
    /// block → `zone_in_failed` stays false / `zone_name` stays "oldzone" → RED.
    #[tokio::test]
    async fn handshake_timeout_surfaces_honest_failure_not_stale_zone() {
        let (mut stream, _unused_rx) = test_stream(0, 0).await;
        let (_tx, mut net_rx) = tokio::sync::mpsc::unbounded_channel::<AppPacket>();
        let (mut gs, snapshot, health) = fresh_gs_snapshot();

        let ok = run_zone_entry_handshake(
            &mut stream, &mut net_rx, &mut gs, "Tester", &health, &snapshot,
            Duration::from_millis(200),  // deadline — never completes
        ).await;

        assert!(!ok, "a never-completed zone-in must report failure");
        assert!(gs.zone_in_failed, "timeout must raise the honest zone_in_failed flag");
        assert!(gs.zone_name.is_empty(),
            "timeout must CLEAR the stale OLD zone_name so no agent reads it as current (#343/#470)");
        let published = snapshot.load();
        assert!(published.zone_in_failed && published.zone_name.is_empty(),
            "the honest failure state must be PUBLISHED to the snapshot, not just held locally");
    }
}

#[cfg(test)]
mod liveness_probe_tests {
    use super::{consider_reply_is_self, probe_outstanding};
    use crate::ipc::NetHealth;
    use std::time::{Duration, Instant};

    fn ago(secs: u64) -> Instant { Instant::now() - Duration::from_secs(secs) }

    /// The self-consider probe reply is discriminated from a user `/consider` of another mob purely
    /// by `targetid == our own spawn id` (Consider_Struct targetid@4). This is what lets us consume
    /// the probe's reply and drop it, without swallowing a real consider of some other spawn.
    #[test]
    fn consider_reply_is_self_matches_only_our_own_spawn() {
        let mut reply = vec![0u8; 20];
        reply[4..8].copy_from_slice(&42u32.to_le_bytes()); // targetid = 42
        assert!(consider_reply_is_self(&reply, 42),  "targetid == player_id → our probe reply");
        assert!(!consider_reply_is_self(&reply, 99), "targetid != player_id → a real consider of another mob");
    }

    /// player_id 0 (not yet in-zone) never matches — we never send a probe then, and a stray
    /// targetid-0 packet must not be mistaken for a probe reply.
    #[test]
    fn consider_reply_is_self_never_matches_before_zone_in() {
        let reply = vec![0u8; 20]; // targetid = 0
        assert!(!consider_reply_is_self(&reply, 0));
    }

    /// A short/truncated payload can't be a valid self-consider reply — don't index out of bounds.
    #[test]
    fn consider_reply_is_self_rejects_short_payload() {
        assert!(!consider_reply_is_self(&[0u8; 4], 42));
    }

    /// `probe_outstanding` gates the reply-consume so it fires exactly once per probe: true only
    /// while a sent probe is still awaiting its answer.
    #[test]
    fn probe_outstanding_reflects_the_send_reply_race() {
        let mut h = NetHealth::default();
        // Never probed → nothing outstanding.
        h.last_probe_sent = None; h.last_probe_reply = None;
        assert!(!probe_outstanding(&h), "no probe ever sent → nothing outstanding");

        // Sent, no reply yet → outstanding.
        h.last_probe_sent = Some(ago(2)); h.last_probe_reply = None;
        assert!(probe_outstanding(&h), "sent but unanswered → outstanding");

        // Reply came AFTER the send → answered, not outstanding (a later user self-consider is safe).
        h.last_probe_sent = Some(ago(5)); h.last_probe_reply = Some(ago(4));
        assert!(!probe_outstanding(&h), "reply newer than send → answered");

        // A new probe sent after the last reply → outstanding again.
        h.last_probe_sent = Some(ago(1)); h.last_probe_reply = Some(ago(10));
        assert!(probe_outstanding(&h), "newest send postdates the last reply → outstanding again");
    }
}

#[cfg(test)]
mod wedge_timeline_tests {
    //! #371 wedge-flicker regression (independent-reviewer finding). Every liveness test above this
    //! module — and every test in `http::world_responsive_tests` — feeds `world_responsive` a single
    //! frozen `(sent_ago, reply_ago, last_packet_ago)` snapshot. None of them exercise the seam the
    //! bug actually lived in: the gameplay.rs 30s resend cadence (`should_send_probe` +
    //! `record_probe_sent`) mutating a REAL `NetHealth` across elapsed wall-clock, then read back
    //! through the http.rs 10s in-flight grace window (`world_responsive`).
    //!
    //! This drives the actual production functions — `should_send_probe`, `record_probe_sent`,
    //! `record_probe_reply` — against a real `NetHealth`, over a synthetic timeline (a fixed `base`
    //! `Instant` plus `Duration::from_secs(t)`, since `Instant` itself can't be faked/advanced). At
    //! each virtual second the ages fed to `world_responsive` are computed exactly the way
    //! `HttpState::health()` computes them (`now_t.duration_since(field)`), just parameterized on the
    //! synthetic `now_t` instead of a real `Instant::now()`.
    //!
    //! Asserts the fixed behavior: for a zone that never answers a single probe, once
    //! `world_responsive` first reports the wedge (`false`), it must NEVER read `true` again — no
    //! matter how many more 30s resend cycles elapse. Before the fix this oscillated forever
    //! (true@0, false@22, true@42, false@52, true@72, ... — the exact timeline from the bug report),
    //! because every resend restamped the SAME clock `world_responsive` used for its 10s grace check.
    use super::{record_app_packet, record_probe_reply, record_probe_sent, should_send_probe, PROBE_INTERVAL};
    use crate::ipc::{world_responsive, NetHealth, PASSIVE_LIVENESS_STALE_SECS, PROBE_TIMEOUT_SECS};
    use std::time::{Duration, Instant};

    /// Drives a timeline second-by-second through the REAL `NetHealth` and the real production state
    /// transitions (`record_app_packet` for spontaneous traffic, `should_send_probe` +
    /// `record_probe_sent` for the resend policy), and returns the `world_responsive` verdict recorded
    /// at every virtual second from 0 to `run_secs`. `traffic(t)` says whether a spontaneous
    /// application packet arrives at second `t` (t=0 always counts as the initial packet). No probe is
    /// EVER answered in these scenarios — the point is to prove liveness is tracked from spontaneous
    /// traffic and probe-silence alone, without ever relying on a probe reply.
    ///
    /// Per-second order mirrors the gameplay loop: drain inbound packets (→ `record_app_packet`)
    /// first, then run the probe-send policy, then an HTTP read would compute `world_responsive` from
    /// the resulting clocks.
    fn simulate(run_secs: u64, traffic: impl Fn(u64) -> bool) -> Vec<(u64, bool)> {
        let timeout = Duration::from_secs(PROBE_TIMEOUT_SECS);
        let base = Instant::now();
        let mut h = NetHealth {
            last_datagram: base, last_packet: base, last_tick: base,
            last_probe_sent: None, last_probe_reply: None, first_unanswered_probe_sent: None,
        };
        let mut verdicts = Vec::with_capacity(run_secs as usize + 1);

        for t in 0..=run_secs {
            let now_t = base + Duration::from_secs(t);

            // 1. Spontaneous inbound traffic (t=0 is the initial packet that seeds `last_packet`).
            if t == 0 || traffic(t) {
                record_app_packet(&mut h, now_t); // stamps last_packet AND clears the streak
            }

            // 2. Resend policy (never answered: record_probe_reply is deliberately never called).
            let last_packet_ago     = now_t.duration_since(h.last_packet);
            let last_probe_sent_ago = h.last_probe_sent.map(|s| now_t.duration_since(s));
            if should_send_probe(last_packet_ago, last_probe_sent_ago) {
                record_probe_sent(&mut h, now_t);
            }

            // 3. HTTP-read verdict, computed exactly as `HttpState::health()` does.
            let first_unanswered_ago = h.first_unanswered_probe_sent.map(|s| now_t.duration_since(s));
            let probe_reply_ago      = h.last_probe_reply.map(|s| now_t.duration_since(s));
            let last_packet_ago      = now_t.duration_since(h.last_packet);
            // These timelines model a WEDGED-BUT-ACKing zone (#371), so the link is alive throughout
            // (`connected == true`); the #470 passive staleness bound is passed but only governs the
            // no-probe branch, which these scenarios exit at ~12s of app-silence when the probe fires.
            let responsive = world_responsive(
                true, first_unanswered_ago, probe_reply_ago, last_packet_ago, timeout,
                Duration::from_secs(PASSIVE_LIVENESS_STALE_SECS));
            verdicts.push((t, responsive));
        }
        verdicts
    }

    /// THE original regression, fixed: a permanently silent zone (traffic only at t=0) is flagged
    /// wedged once, and — across several full 30s resend cycles — never flickers back to `true`.
    #[test]
    fn never_answering_zone_stays_wedged_once_flagged() {
        let run_secs = PROBE_INTERVAL.as_secs() * 6 + 60; // several resend cycles past the first wedge
        let verdicts = simulate(run_secs, |_| false); // no traffic after t=0

        let first_wedge = verdicts.iter().find(|(_, r)| !r).map(|(t, _)| *t)
            .expect("a permanently silent zone must eventually be flagged wedged");
        // Sanity: the timeline must actually span multiple resend cycles past the first wedge,
        // or this test would pass vacuously without ever exercising a resend.
        assert!(run_secs - first_wedge > PROBE_INTERVAL.as_secs() * 3,
            "test timeline too short to cover several resend cycles past the first wedge verdict");

        for &(t, responsive) in verdicts.iter().filter(|(t, _)| *t > first_wedge) {
            assert!(!responsive,
                "world_responsive flipped back to true at t={t}s after first wedging at t={first_wedge}s \
                 — a never-answering zone must stay flagged wedged, not oscillate on every resend");
        }
    }

    /// THE second-wedge false-alive (reviewer follow-up): wedge, then RECOVER via resumed spontaneous
    /// traffic (never a probe reply), then WEDGE AGAIN. The second wedge must be detected —
    /// `world_responsive` must go `false` again. Before the traffic-clear fix, the stale streak-start
    /// from the first wedge sat older than every later probe forever, so the answered-clause stayed
    /// permanently true and the re-wedge read as a confident false-ALIVE.
    #[test]
    fn re_wedge_after_traffic_recovery_is_detected() {
        // Recovery = a sustained burst of spontaneous packets during [recover_from, recover_to];
        // silence (real wedge) before and after.
        let recover_from = 100;
        let recover_to   = 130;
        let run_secs     = 200;
        let verdicts = simulate(run_secs, |t| (recover_from..=recover_to).contains(&t));

        // First wedge: silence from t=0 → flagged false well before recovery begins.
        let first_wedge = verdicts.iter().find(|(_, r)| !r).map(|(t, _)| *t)
            .expect("first silence must be flagged wedged");
        assert!(first_wedge < recover_from,
            "the first wedge must be declared before recovery traffic starts (got t={first_wedge}s)");

        // During recovery, resumed traffic proves the world is alive again → responsive.
        assert!(verdicts.iter().any(|&(t, r)| (recover_from..=recover_to).contains(&t) && r),
            "resumed spontaneous traffic must read as responsive again (recovery not detected)");

        // Second wedge: after traffic stops at recover_to, silence resumes and a FRESH probe streak
        // must time out → world_responsive false again. This is the assertion the fix is about.
        let re_wedge = verdicts.iter()
            .find(|&&(t, r)| t > recover_to && !r).map(|&(t, _)| t);
        assert!(re_wedge.is_some(),
            "the SECOND wedge (silence after traffic recovery) was NOT detected — world_responsive \
             stayed true forever = a confident false-alive. Recovery traffic must re-arm the streak \
             clock so the next silence is timed freshly.");
        // And the timeline must run well past that re-wedge so this isn't a boundary fluke.
        assert!(run_secs - re_wedge.unwrap() > PROBE_TIMEOUT_SECS,
            "timeline too short to confirm the re-wedge verdict holds");
    }

    /// #470 REGRESSION (reviewer-found): the textbook #343 shape — a healthy idle session with NO
    /// spontaneous app traffic after t=0 whose every probe is answered INSTANTLY — must read
    /// `world_responsive: true` at EVERY step across many probe cycles. The subtlety this guards: an
    /// answered probe clears the unanswered streak (`record_probe_reply` → `first_unanswered = None`),
    /// so the session sits in `world_responsive`'s passive (`None`) branch, and the NEXT probe is not
    /// due until `PROBE_INTERVAL` (30s) after the previous SEND — so `probe_reply_ago` climbs to nearly
    /// a full interval before it is refreshed. The passive bound MUST survive that whole cycle. With
    /// the first-cut 22s bound this test goes RED in the t=[35..41], [65..71], … windows (proof-of-life
    /// aged 23–29s > 22); with the cadence-derived 40s bound (`PROBE_INTERVAL + PROBE_TIMEOUT`) it is
    /// green throughout. Drives the REAL production transitions, exactly as `HttpState::health` reads.
    #[test]
    fn answered_idle_session_stays_responsive_across_probe_cycles() {
        let base = Instant::now();
        let mut h = NetHealth {
            last_datagram: base, last_packet: base, last_tick: base,
            last_probe_sent: None, last_probe_reply: None, first_unanswered_probe_sent: None,
        };
        let timeout       = Duration::from_secs(PROBE_TIMEOUT_SECS);
        let passive_stale = Duration::from_secs(PASSIVE_LIVENESS_STALE_SECS);
        let run_secs = PROBE_INTERVAL.as_secs() * 4 + 10; // several full 30s answered cycles

        // Sanity: the run must actually cross the danger window a too-small bound fails in, or the
        // test would pass vacuously without exercising the near-full-interval proof-of-life age.
        assert!(run_secs > PROBE_INTERVAL.as_secs() + PROBE_TIMEOUT_SECS,
            "timeline too short to reach the late-cycle proof-of-life ages this test is about");

        let mut probe_cycles = 0;
        for t in 0..=run_secs {
            let now_t = base + Duration::from_secs(t);

            // 1. Spontaneous traffic ONLY at t=0 (the textbook idle shape: silent thereafter).
            if t == 0 {
                record_app_packet(&mut h, now_t);
            }

            // 2. Resend policy, and the zone ANSWERS every probe the same tick (instant reply).
            let last_packet_ago     = now_t.duration_since(h.last_packet);
            let last_probe_sent_ago = h.last_probe_sent.map(|s| now_t.duration_since(s));
            if should_send_probe(last_packet_ago, last_probe_sent_ago) {
                record_probe_sent(&mut h, now_t);
                record_probe_reply(&mut h, now_t); // answered → clears the streak back to None
                probe_cycles += 1;
            }

            // 3. HTTP-read verdict, computed exactly as `HttpState::health()` does (link ALIVE).
            let first_unanswered_ago = h.first_unanswered_probe_sent.map(|s| now_t.duration_since(s));
            let probe_reply_ago      = h.last_probe_reply.map(|s| now_t.duration_since(s));
            let last_packet_ago      = now_t.duration_since(h.last_packet);
            let responsive = world_responsive(
                true, first_unanswered_ago, probe_reply_ago, last_packet_ago, timeout, passive_stale);

            assert!(responsive,
                "a healthy every-probe-answering idle session read world_responsive=FALSE at t={t}s \
                 (proof-of-life aged {}s, bound {}s) — the #343 regression the reviewer caught",
                probe_reply_ago.map_or(last_packet_ago, |r| r.min(last_packet_ago)).as_secs(),
                PASSIVE_LIVENESS_STALE_SECS);
        }

        // Guard the guard: we must have actually gone through multiple answered probe cycles, or the
        // near-full-interval proof-of-life age was never reached and the test proved nothing.
        assert!(probe_cycles >= 3,
            "expected several answered probe cycles over the run, got {probe_cycles}");
    }
}

