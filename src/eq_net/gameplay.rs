//! Phase 2 gameplay loop: receive packets, update game state, keepalive, navigate.
//!
//! Handles zone transitions inline: when OP_ZONE_SERVER_INFO arrives the current
//! zone stream is replaced with a new connection and the zone-entry handshake runs.

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::{Duration, sleep};

use crate::eq_net::login::WorldCredentials;
use crate::eq_net::navigation::{Navigator, make_position_packet};
use crate::eq_net::packet_handler::apply_packet;
use crate::eq_net::protocol::*;
use crate::eq_net::transport::{AppPacket, EqStream};
use crate::game_state::GameState;

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Consume the zone stream and run the gameplay loop indefinitely.
pub async fn run_gameplay_phase(
    stream_init:   EqStream,
    net_rx_init:   UnboundedReceiver<AppPacket>,
    app_tx:        UnboundedSender<AppPacket>,
    mut gs:        GameState,
    char_name:     String,
    mut navigator: Navigator,
    world_creds:   WorldCredentials,
) {
    // Wrap in Option so Rust allows reassignment after zone transitions.
    let mut stream: Option<EqStream>                      = Some(stream_init);
    let mut net_rx: Option<UnboundedReceiver<AppPacket>>  = Some(net_rx_init);
    let mut last_keepalive = std::time::Instant::now();

    loop {
        let s  = stream.as_mut().expect("stream always Some in loop");
        let rx = net_rx.as_mut().expect("net_rx always Some in loop");
        s.poll_recv();

        let mut zone_redirect: Option<(String, u16)> = None;
        let mut world_reconnect_needed = false;
        while let Ok(packet) = rx.try_recv() {
            apply_packet(&mut gs, &packet);
            navigator.sync_entities(&gs);
            navigator.sync_zone_points(&gs);
            navigator.sync_tasks(&gs);
            navigator.sync_inventory(&gs);
            let _ = app_tx.send(packet.clone());

            match packet.opcode {
                // Server listed a lootable item — echo back immediately to take it.
                OP_LOOT_ITEM => {
                    gs.loot_last_activity = Some(std::time::Instant::now());
                    gs.log_msg("loot", "Looting item...");
                    s.send_app_packet(OP_LOOT_ITEM, &packet.payload);
                    eprintln!("EQ: auto-loot: taking item (echoed OP_LootItem)");
                }
                OP_REQUEST_CLIENT_ZONE_CHANGE if packet.payload.len() >= 24 => {
                    // Server wants us to move — either a zone transition or a same-zone teleport.
                    // Parse RequestClientZoneChange_Struct (24 bytes):
                    //   u16 zone_id, u16 instance_id, float y, float x, float z,
                    //   float heading, u32 type
                    // Wire layout: y at offset 4, x at offset 8 (Titanium struct)
                    let zone_id     = u16::from_le_bytes([packet.payload[0], packet.payload[1]]);
                    let instance_id = u16::from_le_bytes([packet.payload[2], packet.payload[3]]);
                    let y = f32::from_le_bytes([packet.payload[4], packet.payload[5], packet.payload[6], packet.payload[7]]);
                    let x = f32::from_le_bytes([packet.payload[8], packet.payload[9], packet.payload[10], packet.payload[11]]);
                    let z = f32::from_le_bytes([packet.payload[12], packet.payload[13], packet.payload[14], packet.payload[15]]);

                    if zone_id == gs.zone_id {
                        // Same-zone teleport (e.g. #goto x y z, #zone 0).
                        // The server expects the client to just move — it clears zone_mode
                        // before we respond, so sending OP_ZONE_CHANGE would cause a cancel
                        // and a spurious world reconnect (see zoning.cpp:1056-1068).
                        // Wire y = server_y = east = gs.player_y; wire x = server_x = north = gs.player_x.
                        gs.player_x = x;
                        gs.player_y = y;
                        gs.player_z = z;
                        eprintln!("EQ: same-zone teleport → pos=({:.1},{:.1},{:.1})", x, y, z);
                        // Send position update so the server knows where we are.
                        let _ = app_tx.send(make_position_packet(gs.player_id, x, y, z));
                    } else {
                        // Cross-zone transition (#zone <name>): send OP_ZONE_CHANGE to
                        // trigger the full zone-change protocol (world reconnect, etc.).
                        let mut buf = [0u8; SIZE_ZONE_CHANGE];
                        let nb = gs.player_name.as_bytes();
                        buf[..nb.len().min(64)].copy_from_slice(&nb[..nb.len().min(64)]);
                        buf[64..66].copy_from_slice(&zone_id.to_le_bytes());
                        buf[66..68].copy_from_slice(&instance_id.to_le_bytes());
                        buf[68..72].copy_from_slice(&y.to_le_bytes());
                        buf[72..76].copy_from_slice(&x.to_le_bytes());
                        buf[76..80].copy_from_slice(&z.to_le_bytes());
                        s.send_app_packet(OP_ZONE_CHANGE, &buf);
                        eprintln!("EQ: cross-zone OP_REQUEST_CLIENT_ZONE_CHANGE zone_id={zone_id} → sent OP_ZONE_CHANGE");
                    }
                }
                OP_ZONE_CHANGE if packet.payload.len() >= 88 => {
                    let success = i32::from_le_bytes([
                        packet.payload[84], packet.payload[85],
                        packet.payload[86], packet.payload[87],
                    ]);
                    eprintln!("EQ: OP_ZONE_CHANGE server response success={success}");
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
                    eprintln!("EQ: auto-loot: sent OP_LootRequest for corpse_id={}", corpse_id);
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
                    eprintln!("EQ: auto-loot: sent OP_EndLootRequest (session complete)");
                    // Reset queued_at so the next corpse gets its own delay window.
                    gs.loot_queued_at = gs.pending_loot.front().map(|_| std::time::Instant::now());
                }
            }
        }

        if world_reconnect_needed {
            eprintln!("EQ: zone change approved — reconnecting to world for zone handoff");
            let ok = reconnect_via_world(
                &mut stream, &mut net_rx, &app_tx, &mut gs, &char_name, &world_creds,
            ).await;
            if ok {
                run_zone_entry_handshake(
                    stream.as_mut().unwrap(),
                    net_rx.as_mut().unwrap(),
                    &app_tx,
                    &mut gs,
                ).await;
                navigator.sync_zone_points(&gs);
                last_keepalive = std::time::Instant::now();
            } else {
                eprintln!("EQ: world reconnect failed — exiting gameplay");
                return;
            }
            continue;
        }

        if let Some((zone_ip, zone_port)) = zone_redirect {
            eprintln!("EQ: zone transition → {}:{}", zone_ip, zone_port);
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
                    eprintln!("EQ: sent zone entry for '{}'", char_name);
                    run_zone_entry_handshake(
                        stream.as_mut().unwrap(),
                        net_rx.as_mut().unwrap(),
                        &app_tx,
                        &mut gs,
                    ).await;
                    navigator.sync_zone_points(&gs);
                    last_keepalive = std::time::Instant::now();
                }
                Err(e) => {
                    eprintln!("EQ: zone transition connect failed: {e}");
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

        navigator.tick(s, &mut gs, &app_tx);

        sleep(Duration::from_millis(10)).await;
    }
}

/// After OP_ZONE_CHANGE success=1: reconnect to world, get OP_ZONE_SERVER_INFO, connect to new zone.
/// On success, `stream` and `net_rx` are replaced with the new zone connection.
async fn reconnect_via_world(
    stream:      &mut Option<EqStream>,
    net_rx:      &mut Option<UnboundedReceiver<AppPacket>>,
    app_tx:      &UnboundedSender<AppPacket>,
    _gs:         &mut GameState,
    char_name:   &str,
    creds:       &WorldCredentials,
) -> bool {
    drop(stream.take());
    drop(net_rx.take());
    sleep(Duration::from_millis(300)).await;

    let (world_tx, mut world_rx) = tokio::sync::mpsc::unbounded_channel::<AppPacket>();
    eprintln!("EQ: reconnecting to world {}:{}", creds.world_host, creds.world_port);
    let mut world_stream = match EqStream::connect(&creds.world_host, creds.world_port, world_tx).await {
        Ok(s) => s,
        Err(e) => { eprintln!("EQ: world reconnect failed: {e}"); return false; }
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
    eprintln!("EQ: sent OP_SEND_LOGIN_INFO to world (lsid={}, zoning=1)", creds.lsid);

    // Wait for OP_SEND_CHAR_INFO → send OP_ENTER_WORLD → wait for OP_ZONE_SERVER_INFO
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut char_info_sent = false;
    let mut zone_server: Option<(String, u16)> = None;

    while std::time::Instant::now() < deadline && zone_server.is_none() {
        world_stream.poll_recv();
        while let Ok(packet) = world_rx.try_recv() {
            let _ = app_tx.send(packet.clone());
            match packet.opcode {
                // In a fresh-login reconnect, world sends OP_SEND_CHAR_INFO as the trigger.
                // In a zoning=1 reconnect, world sends OP_APPROVE_WORLD (0x3c25) instead.
                OP_SEND_CHAR_INFO | OP_APPROVE_WORLD if !char_info_sent => {
                    char_info_sent = true;
                    let mut enter_buf = vec![0u8; SIZE_ENTER_WORLD];
                    let nb = char_name.as_bytes();
                    enter_buf[..nb.len().min(64)].copy_from_slice(&nb[..nb.len().min(64)]);
                    world_stream.send_app_packet(OP_ENTER_WORLD, &enter_buf);
                    world_stream.send_app_packet(OP_POST_ENTER_WORLD, &[]);
                    eprintln!("EQ: zone change: sent OP_ENTER_WORLD to world (trigger=0x{:04x})", packet.opcode);
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
                    eprintln!("EQ: zone change: world says new zone at {}:{}", ip, port);
                    zone_server = Some((ip, port));
                }
                _ => {
                    eprintln!("EQ: zone change world: opcode 0x{:04x} ({} bytes)", packet.opcode, packet.payload.len());
                }
            }
        }
        sleep(Duration::from_millis(10)).await;
    }

    drop(world_stream);

    let (zone_ip, zone_port) = match zone_server {
        Some(s) => s,
        None => {
            eprintln!("EQ: zone change: world did not send OP_ZONE_SERVER_INFO within 30s");
            return false;
        }
    };

    sleep(Duration::from_millis(800)).await;
    eprintln!("EQ: zone change: connecting to new zone {}:{}", zone_ip, zone_port);
    let (zone_tx, zone_rx) = tokio::sync::mpsc::unbounded_channel::<AppPacket>();
    let mut zone_stream = match EqStream::connect(&zone_ip, zone_port, zone_tx).await {
        Ok(s) => s,
        Err(e) => { eprintln!("EQ: zone change: zone connect failed: {e}"); return false; }
    };

    // Send zone entry
    let mut cze = vec![0u8; SIZE_CLIENT_ZONE_ENTRY];
    let nb = char_name.as_bytes();
    cze[4..4 + nb.len().min(64)].copy_from_slice(&nb[..nb.len().min(64)]);
    zone_stream.send_app_packet(OP_ZONE_ENTRY, &cze);
    eprintln!("EQ: zone change: sent OP_ZONE_ENTRY for '{}'", char_name);

    *stream = Some(zone_stream);
    *net_rx = Some(zone_rx);
    true
}

/// Handles the OP_NEW_ZONE → OP_WEATHER → OP_SEND_EXP_ZONE_IN handshake
/// that completes after connecting to a new zone server.
async fn run_zone_entry_handshake(
    stream:  &mut EqStream,
    net_rx:  &mut UnboundedReceiver<AppPacket>,
    app_tx:  &UnboundedSender<AppPacket>,
    gs:      &mut GameState,
) {
    // Clear stale entities now so OP_ZONE_SPAWNS can repopulate them.
    // (OP_NEW_ZONE arrives AFTER OP_ZONE_SPAWNS in the Titanium server sequence, so
    // we can't rely on apply_new_zone to do this reset.)
    gs.entities.clear();

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut done_new_zone     = false;
    let mut done_weather      = false;
    let mut done_client_ready = false;

    while std::time::Instant::now() < deadline && !done_client_ready {
        stream.poll_recv();
        while let Ok(packet) = net_rx.try_recv() {
            apply_packet(gs, &packet);
            let _ = app_tx.send(packet.clone());
            match packet.opcode {
                OP_NEW_ZONE if !done_new_zone => {
                    done_new_zone = true;
                    stream.send_app_packet(OP_REQ_CLIENT_SPAWN, &[]);
                    eprintln!("EQ: new zone '{}' — sent ReqClientSpawn", gs.zone_name);
                }
                OP_WEATHER if !done_weather => {
                    done_weather = true;
                    stream.send_app_packet(OP_REQ_NEW_ZONE, &[]);
                    eprintln!("EQ: zone weather — sent ReqNewZone");
                }
                OP_SEND_EXP_ZONE_IN if !done_client_ready => {
                    done_client_ready = true;
                    stream.send_app_packet(OP_SEND_EXP_ZONE_IN, &[]);
                    stream.send_app_packet(OP_CLIENT_READY, &[]);
                    eprintln!("EQ: zone transition complete — now in '{}'", gs.zone_name);
                }
                _ => {}
            }
        }
        sleep(Duration::from_millis(10)).await;
    }

    if !done_client_ready {
        eprintln!("EQ: zone entry handshake timed out (new_zone={done_new_zone} weather={done_weather})");
    }
}
