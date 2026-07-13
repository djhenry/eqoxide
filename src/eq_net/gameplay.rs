//! Phase 2 gameplay loop: receive packets, update game state, keepalive, navigate.
//!
//! Handles zone transitions inline: when OP_ZONE_SERVER_INFO arrives the current
//! zone stream is replaced with a new connection and the zone-entry handshake runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Duration, sleep};

use crate::eq_net::login::WorldCredentials;
use crate::eq_net::navigation::Navigator;
use crate::eq_net::packet_handler::apply_packet;
use crate::eq_net::protocol::*;
use crate::eq_net::transport::{AppPacket, EqStream};
use crate::game_state::GameState;

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// How often to re-send the bind-respawn request while an explicit respawn is pending (#284/#50).
const RESPAWN_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Consume the zone stream and run the gameplay loop indefinitely.
pub async fn run_gameplay_phase(
    stream_init:   EqStream,
    net_rx_init:   UnboundedReceiver<AppPacket>,
    mut gs:        GameState,
    char_name:     String,
    mut navigator: Navigator,
    world_creds:   WorldCredentials,
    shutdown:      Arc<AtomicBool>,
    camp:          crate::http::CampReq,
    camp_until:    crate::http::CampUntil,
    respawn:       crate::http::RespawnReq,
    game_state_snapshot: crate::http::GameStateSnapshot,
    last_inbound:        crate::http::LastInboundShared,
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
            use crate::eq_net::navigation::build_spawn_appearance_packet;
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
            apply_packet(&mut gs, &packet);
            *last_inbound.lock().unwrap() = std::time::Instant::now();
            navigator.sync_entities(&gs);
            navigator.sync_zone_points(&gs);
            navigator.sync_tasks(&gs);
            navigator.sync_group(&gs);
            navigator.sync_guild(&gs);
            navigator.sync_inventory(&gs);
            navigator.sync_merchant(&gs);
            navigator.sync_messages(&gs);
            navigator.sync_doors(&gs);
            // Deliver a /who all roster to the pending GET /v1/observe/who as soon as it lands (#300).
            // A friends-presence poll (OP_FriendsWho) replies on this SAME opcode, so route it to the
            // pending GET /v1/social/friends instead when a friends poll is what we just sent (#301).
            if packet.opcode == OP_WHO_ALL_RESPONSE {
                if navigator.expecting_friends() {
                    navigator.fulfill_friends(&gs);
                } else {
                    navigator.fulfill_who(&gs);
                }
            }

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
                    use crate::eq_net::navigation::build_translocate_ack;
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
                    // RoF2 ZoneChange_Struct: success is an i32 at offset 92 (was 84 in Titanium).
                    let success = i32::from_le_bytes([
                        packet.payload[92], packet.payload[93],
                        packet.payload[94], packet.payload[95],
                    ]);
                    tracing::info!("EQ: OP_ZONE_CHANGE server response success={success}");
                    if success == 1 {
                        world_reconnect_needed = true;
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
        // Open next corpse 500ms after it was queued (delay lets server register the corpse).
        if !gs.loot_session_active {
            let ready = gs.loot_queued_at
                .map(|t| t.elapsed().as_millis() >= 500)
                .unwrap_or(false);
            if ready {
                if let Some(corpse_id) = gs.pending_loot.pop_front() {
                    s.send_app_packet(OP_LOOT_REQUEST, &corpse_id.to_le_bytes());
                    gs.loot_session_active = true;
                    gs.loot_last_activity = Some(std::time::Instant::now());
                    tracing::info!("EQ: auto-loot: sent OP_LootRequest for corpse_id={}", corpse_id);
                }
                if gs.pending_loot.is_empty() {
                    gs.loot_queued_at = None;
                }
            }
        }
        // Close session after 2 seconds of inactivity (all items have arrived)
        if gs.loot_session_active {
            if let Some(t) = gs.loot_last_activity {
                if t.elapsed().as_secs_f32() > 2.0 {
                    s.send_app_packet(OP_END_LOOT_REQUEST, &[]);
                    gs.loot_session_active = false;
                    gs.loot_last_activity = None;
                    gs.log_msg("loot", "Looting complete");
                    tracing::info!("EQ: auto-loot: sent OP_EndLootRequest (session complete)");
                    // Reset queued_at so the next corpse gets its own delay window.
                    gs.loot_queued_at = gs.pending_loot.front().map(|_| std::time::Instant::now());
                }
            }
        }

        if world_reconnect_needed {
            tracing::info!("EQ: zone change approved — reconnecting to world for zone handoff");
            let ok = reconnect_via_world(
                &mut stream, &mut net_rx, &mut gs, &char_name, &world_creds, &last_inbound,
                &game_state_snapshot,
            ).await;
            if ok {
                run_zone_entry_handshake(
                    stream.as_mut().unwrap(),
                    net_rx.as_mut().unwrap(),
                    &mut gs,
                    &last_inbound,
                    &game_state_snapshot,
                ).await;
                navigator.sync_zone_points(&gs);
                last_keepalive = std::time::Instant::now();
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
            sleep(Duration::from_millis(800)).await;
            match EqStream::connect(&zone_ip, zone_port, new_tx).await {
                Ok(new_stream) => {
                    stream = Some(new_stream);
                    net_rx = Some(new_rx);
                    let s2 = stream.as_mut().unwrap();
                    let mut cze = vec![0u8; SIZE_CLIENT_ZONE_ENTRY];
                    let nb = char_name.as_bytes();
                    cze[4..4 + nb.len().min(64)].copy_from_slice(&nb[..nb.len().min(64)]);
                    s2.send_app_packet(OP_ZONE_ENTRY, &cze);
                    tracing::info!("EQ: sent zone entry for '{}'", char_name);
                    run_zone_entry_handshake(
                        stream.as_mut().unwrap(),
                        net_rx.as_mut().unwrap(),
                        &mut gs,
                        &last_inbound,
                        &game_state_snapshot,
                    ).await;
                    navigator.sync_zone_points(&gs);
                    last_keepalive = std::time::Instant::now();
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

        navigator.tick(s, &mut gs);

        publish_snapshot(&gs, &game_state_snapshot);

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
/// `last_inbound` is bumped as real inbound packets are drained here (the world-reconnect leg of a
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
    last_inbound:         &crate::http::LastInboundShared,
    game_state_snapshot:  &crate::http::GameStateSnapshot,
) -> bool {
    drop(stream.take());
    drop(net_rx.take());
    sleep(Duration::from_millis(300)).await;

    let (world_tx, mut world_rx) = tokio::sync::mpsc::unbounded_channel::<AppPacket>();
    tracing::info!("EQ: reconnecting to world {}:{}", creds.world_host, creds.world_port);
    let mut world_stream = match EqStream::connect(&creds.world_host, creds.world_port, world_tx).await {
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
            *last_inbound.lock().unwrap() = std::time::Instant::now();
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
        publish_snapshot(gs, game_state_snapshot);
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

    sleep(Duration::from_millis(800)).await;
    tracing::info!("EQ: zone change: connecting to new zone {}:{}", zone_ip, zone_port);
    let (zone_tx, zone_rx) = tokio::sync::mpsc::unbounded_channel::<AppPacket>();
    let mut zone_stream = match EqStream::connect(&zone_ip, zone_port, zone_tx).await {
        Ok(s) => s,
        Err(e) => { tracing::warn!("EQ: zone change: zone connect failed: {e}"); return false; }
    };

    // Send zone entry
    let mut cze = vec![0u8; SIZE_CLIENT_ZONE_ENTRY];
    let nb = char_name.as_bytes();
    cze[4..4 + nb.len().min(64)].copy_from_slice(&nb[..nb.len().min(64)]);
    zone_stream.send_app_packet(OP_ZONE_ENTRY, &cze);
    tracing::info!("EQ: zone change: sent OP_ZONE_ENTRY for '{}'", char_name);

    *stream = Some(zone_stream);
    *net_rx = Some(zone_rx);
    true
}

/// Handles the OP_NEW_ZONE → OP_WEATHER → OP_SEND_EXP_ZONE_IN handshake
/// that completes after connecting to a new zone server.
///
/// `last_inbound` is bumped as real inbound packets are drained here, exactly like the gameplay
/// loop's own drain (`run_gameplay_phase`) does — a slow zone-in (up to the 30s deadline below)
/// must not falsely report the connection as lost while it's healthy and still zoning in.
///
/// `game_state_snapshot` is published once per drain pass (#324), same cadence as the steady-state
/// gameplay loop — without this the renderer never observes `OP_NEW_ZONE` / spawns / etc. as they
/// land here, and stays frozen on the OLD zone's last frame for this entire handshake (up to the
/// 30s deadline) instead of starting the fade/loading screen the moment `OP_NEW_ZONE` arrives.
async fn run_zone_entry_handshake(
    stream:              &mut EqStream,
    net_rx:               &mut UnboundedReceiver<AppPacket>,
    gs:                   &mut GameState,
    last_inbound:         &crate::http::LastInboundShared,
    game_state_snapshot:  &crate::http::GameStateSnapshot,
) {
    // Purge the previous zone's spawns/doors now, before OP_ReqClientSpawn asks for the new zone's
    // stream, and re-arm the once-per-zone-in OP_NewZone apply so the repeat OP_NewZone this
    // handshake provokes can't clear again mid-stream (#322).
    gs.begin_zone_in();

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut done_new_zone     = false;
    let mut done_weather      = false;
    let mut done_client_ready = false;

    while std::time::Instant::now() < deadline && !done_client_ready {
        stream.poll_recv();
        stream.poll_resend(); // retransmit OP_ZONE_ENTRY/ReqClientSpawn during zone-in (#254)
        while let Ok(packet) = net_rx.try_recv() {
            apply_packet(gs, &packet);
            *last_inbound.lock().unwrap() = std::time::Instant::now();
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
        publish_snapshot(gs, game_state_snapshot);
        sleep(Duration::from_millis(10)).await;
    }

    if !done_client_ready {
        tracing::warn!("EQ: zone entry handshake timed out (new_zone={done_new_zone} weather={done_weather})");
    }
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
    cmd: crate::http::CampCmd,
    current: Option<std::time::Instant>,
    now: std::time::Instant,
    dur: Duration,
) -> (Option<std::time::Instant>, CampAction) {
    use crate::http::CampCmd;
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

/// Publish the network thread's `GameState` for lock-free reads by the render/HTTP threads. Called
/// once per gameplay tick, after every mutation for that tick (packet-applied and `Navigator::tick`'s
/// own writes) has landed — see the call site in `run_gameplay_phase`.
///
/// Store only on a real change so the published Arc's identity is a complete activity signal: the
/// render thread wakes on ANY network-thread mutation (inbound packet OR client-initiated request
/// handled by `Navigator::tick`), and a genuinely idle world lets the loop sleep (see
/// `App::poll_external` in app.rs, which drives its wake decision off `Arc::ptr_eq` against this
/// snapshot).
pub fn publish_snapshot(gs: &GameState, snapshot: &crate::http::GameStateSnapshot) {
    if **snapshot.load() != *gs {
        snapshot.store(Arc::new(gs.clone()));
    }
}

#[cfg(test)]
mod camp_tests {
    use super::*;
    use crate::http::CampCmd;
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

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn publish_snapshot_reflects_latest_state_independent_of_later_mutation() {
        let snapshot: crate::http::GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(GameState::new()));

        let mut gs = GameState::new();
        gs.player_name = "Aldric".to_string();
        publish_snapshot(&gs, &snapshot);
        assert_eq!(snapshot.load().player_name, "Aldric");

        // Mutating the source after publishing must not retroactively change the already-published
        // snapshot — each publish is an independent, immutable clone.
        gs.player_name = "Mutated".to_string();
        assert_eq!(snapshot.load().player_name, "Aldric");

        // A second publish replaces the snapshot wholesale.
        publish_snapshot(&gs, &snapshot);
        assert_eq!(snapshot.load().player_name, "Mutated");
    }

    /// Fix (single-owner GameState wake signal): a no-op publish (state genuinely unchanged since
    /// the last publish) must NOT replace the Arc — the render loop's `poll_external` treats a new
    /// Arc identity as "something happened" and would otherwise spuriously wake every tick even in
    /// an idle world.
    #[test]
    fn publish_snapshot_keeps_same_arc_when_state_unchanged() {
        let gs = GameState::new();
        let snapshot: crate::http::GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(gs.clone()));

        publish_snapshot(&gs, &snapshot);
        let before = snapshot.load_full();

        // Same state, re-published (e.g. a quiet tick with no inbound packet and no client request).
        publish_snapshot(&gs, &snapshot);
        let after = snapshot.load_full();

        assert!(Arc::ptr_eq(&before, &after), "unchanged state must not republish a new Arc");
    }

    /// Counterpart: a real mutation (standing in for either an inbound packet or a client-initiated
    /// change made by `Navigator::tick`, e.g. `gs.sitting`) DOES publish a new Arc, and the new
    /// snapshot reflects it.
    #[test]
    fn publish_snapshot_publishes_new_arc_when_state_changed() {
        let mut gs = GameState::new();
        let snapshot: crate::http::GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(gs.clone()));

        publish_snapshot(&gs, &snapshot);
        let before = snapshot.load_full();

        // Simulate a client-initiated mutation with no inbound packet (e.g. POST /v1/interact/sit).
        gs.sitting = true;
        publish_snapshot(&gs, &snapshot);
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
        let snapshot: crate::http::GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(gs.clone()));
        let last_inbound: crate::http::LastInboundShared =
            Arc::new(std::sync::Mutex::new(std::time::Instant::now()));

        let snapshot_bg     = snapshot.clone();
        let last_inbound_bg = last_inbound.clone();
        let handle = tokio::spawn(async move {
            run_zone_entry_handshake(&mut stream, &mut net_rx, &mut gs, &last_inbound_bg, &snapshot_bg).await;
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
}
