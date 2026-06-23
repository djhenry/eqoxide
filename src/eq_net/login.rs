//! EQ login protocol: login server → world server → zone server handshake.
//!
//! Handles only the connection/authentication state machine.  All gameplay
//! packet effects (spawn registration, HP updates, etc.) are delegated to
//! `packet_handler::apply_packet` so there is no duplication with the render loop.

use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::time::{Duration, sleep};

use cbc::{Decryptor, Encryptor};
use des::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::NoPadding};
use des::Des;

use crate::config::LoginConfig;
use crate::eq_net::gameplay::{run_gameplay_phase};
use crate::eq_net::navigation::Navigator;
use crate::eq_net::packet_handler::apply_packet;
use crate::eq_net::protocol::*;
use crate::eq_net::transport::{AppPacket, EqStream};
use crate::game_state::GameState;
use crate::http::{AttackReq, BuyReq, MoveReq, GiveReq, CastReq, SitReq, ConsiderReq, EntityIds, EntityPositions, GotoTarget, HailReq, SayReq, TargetReq, TaskLog, ZoneCrossReq, ZonePoints};

type DesCbcEnc = Encryptor<Des>;
type DesCbcDec = Decryptor<Des>;

// ── DES helpers (Titanium login uses all-zero key+IV) ────────────────────────

fn des_encrypt(data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    DesCbcEnc::new_from_slices(&[0u8; 8], &[0u8; 8])
        .expect("DES key/iv")
        .encrypt_padded_mut::<NoPadding>(&mut out, data.len())
        .expect("DES encrypt")
        .to_vec()
}

fn des_decrypt(data: &[u8]) -> Vec<u8> {
    let enc_len = (data.len() / 8) * 8;
    if enc_len == 0 { return Vec::new(); }
    let mut buf = data[..enc_len].to_vec();
    match DesCbcDec::new_from_slices(&[0u8; 8], &[0u8; 8])
        .expect("DES key/iv")
        .decrypt_padded_mut::<NoPadding>(&mut buf)
    {
        Ok(out) => out.to_vec(),
        Err(_)  => buf,
    }
}

// ── Public types ─────────────────────────────────────────────────────────────

/// World-server credentials needed to reconnect after a zone change.
pub struct WorldCredentials {
    pub lsid:       i32,
    pub ls_key:     String,
    pub world_host: String,
    pub world_port: u16,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Connect, authenticate, enter zone, then run the gameplay loop.
/// Retries up to `max_retries` times on transient failures.
pub async fn run_login_flow(
    config:           LoginConfig,
    app_tx:           mpsc::UnboundedSender<AppPacket>,
    max_retries:      u32,
    goto_target:      GotoTarget,
    entity_positions: EntityPositions,
    entity_ids:       EntityIds,
    zone_points:      ZonePoints,
    task_log:         TaskLog,
    zone_cross:       ZoneCrossReq,
    hail:             HailReq,
    say:              SayReq,
    target:           TargetReq,
    attack:           AttackReq,
    buy:              BuyReq,
    move_req:         MoveReq,
    give:             GiveReq,
    cast:             CastReq,
    sit:              SitReq,
    consider:         ConsiderReq,
    collision:        crate::assets::SharedCollision,
    maps_dir:         std::path::PathBuf,
) -> Result<(), String> {
    for attempt in 1..=max_retries {
        if attempt > 1 {
            eprintln!("EQ: retry {}/{}", attempt, max_retries);
            sleep(Duration::from_secs(3)).await;
        }
        match run_login_phase(&config, &app_tx).await {
            Err(e) => eprintln!("EQ: login failed (attempt {}): {}", attempt, e),
            Ok((stream, net_rx, gs, world_creds)) => {
                // Seed /entities map with everything discovered during login.
                {
                    let mut map = entity_positions.lock().unwrap();
                    let mut ids = entity_ids.lock().unwrap();
                    for (&id, e) in &gs.entities {
                        map.insert(e.name.clone(), (e.x, e.y, e.z));
                        ids.insert(e.name.clone(), id);
                    }
                    eprintln!("NAV: entity map seeded with {} entities", map.len());
                }
                // Seed zone points (in case OP_SEND_ZONE_POINTS arrived during login phase).
                if !gs.zone_points.is_empty() {
                    *zone_points.lock().unwrap() = gs.zone_points.clone();
                    eprintln!("NAV: {} zone points seeded", gs.zone_points.len());
                }
                let char_name = config.character_name.clone();
                let navigator = Navigator::new(goto_target, entity_positions, entity_ids, zone_points, task_log, zone_cross, hail, say, target, attack, buy, move_req, give, cast, sit, consider, collision, maps_dir);
                run_gameplay_phase(stream, net_rx, app_tx, gs, char_name, navigator, world_creds).await;
                return Ok(());
            }
        }
    }
    Err(format!("Login failed after {} attempts", max_retries))
}

// ── Login phase ───────────────────────────────────────────────────────────────

/// Run the full handshake (login → world → zone entry).
/// Returns the live zone stream, its packet receiver, accumulated GameState, and world credentials.
async fn run_login_phase(
    config: &LoginConfig,
    app_tx: &mpsc::UnboundedSender<AppPacket>,
) -> Result<(EqStream, UnboundedReceiver<AppPacket>, GameState, WorldCredentials), String> {
    let (net_tx, mut net_rx) = mpsc::unbounded_channel::<AppPacket>();

    eprintln!("EQ: connecting to login server {}:{}", config.login_host, config.login_port);
    let mut stream = EqStream::connect(&config.login_host, config.login_port, net_tx.clone())
        .await
        .map_err(|e| format!("Login server connection failed: {e}"))?;

    eprintln!("EQ: login session established — waiting for handshake");
    stream.send_app_packet(OP_SESSION_READY, &2u32.to_le_bytes());

    let mut proto = LoginProtocol::new(config);
    let mut gs    = GameState::new();
    gs.player_name = config.character_name.clone();

    'login: loop {
        stream.poll_recv();

        while let Ok(packet) = net_rx.try_recv() {
            // Forward to render loop so it gets zone/spawn data during login.
            let _ = app_tx.send(AppPacket { opcode: packet.opcode, payload: packet.payload.clone() });

            // Apply gameplay side effects.
            apply_packet(&mut gs, &packet);

            // Handle login-protocol state transitions.
            match proto.handle(&packet, &mut stream, &gs) {
                PhaseResult::Continue                => {}
                PhaseResult::Done                    => break 'login,
                PhaseResult::Error(e)                => return Err(e),
                PhaseResult::ReconnectWorld { host, port } => {
                    drop(stream);
                    sleep(Duration::from_millis(100)).await;
                    eprintln!("EQ: connecting to world {}:{}", host, port);
                    stream = EqStream::connect(&host, port, net_tx.clone())
                        .await
                        .map_err(|e| format!("World connection failed: {e}"))?;
                    proto.on_world_connected(&mut stream);
                }
                PhaseResult::ReconnectZone { host, port } => {
                    drop(stream);
                    sleep(Duration::from_millis(800)).await;
                    eprintln!("EQ: connecting to zone {}:{}", host, port);
                    stream = EqStream::connect(&host, port, net_tx.clone())
                        .await
                        .map_err(|e| format!("Zone connection failed: {e}"))?;
                    // Clear stale entities before zone entry so OP_ZONE_SPAWNS repopulates fresh.
                    gs.entities.clear();
                    proto.on_zone_connected(&mut stream);
                }
            }
        }

        sleep(Duration::from_millis(10)).await;

        if proto.is_timed_out() {
            return Err("Login timed out".to_string());
        }
    }

    let world_creds = WorldCredentials {
        lsid:       proto.lsid,
        ls_key:     proto.ls_key.clone(),
        world_host: proto.world_host.clone(),
        world_port: config.world_port,
    };
    Ok((stream, net_rx, gs, world_creds))
}

// ── Login protocol state machine ──────────────────────────────────────────────

enum PhaseResult {
    Continue,
    Done,
    Error(String),
    ReconnectWorld { host: String, port: u16 },
    ReconnectZone  { host: String, port: u16 },
}

/// Tracks only the login-protocol handshake state.
/// Game state (entities, positions) is maintained separately in GameState.
struct LoginProtocol<'a> {
    config:       &'a LoginConfig,
    // Credentials returned by the login server
    lsid:         i32,
    ls_key:       String,
    // World server info parsed from the server list
    world_host:   String,
    world_server_id: u32,
    // Handshake progress flags
    done_session_ready:    bool,
    done_login_accepted:   bool,
    done_server_list:      bool,
    done_play_everquest:   bool,
    done_char_info:        bool,
    done_zone_server_info: bool,
    done_zone_entry:       bool,
    done_client_ready:     bool,
    done_zone_weather:     bool,
    start_time: std::time::Instant,
}

impl<'a> LoginProtocol<'a> {
    fn new(config: &'a LoginConfig) -> Self {
        LoginProtocol {
            config,
            lsid: 0, ls_key: String::new(),
            world_host: String::new(), world_server_id: 0,
            done_session_ready:    false,
            done_login_accepted:   false,
            done_server_list:      false,
            done_play_everquest:   false,
            done_char_info:        false,
            done_zone_server_info: false,
            done_zone_entry:       false,
            done_client_ready:     false,
            done_zone_weather:     false,
            start_time: std::time::Instant::now(),
        }
    }

    fn is_timed_out(&self) -> bool {
        self.start_time.elapsed() > Duration::from_secs(120)
    }

    fn on_world_connected(&self, stream: &mut EqStream) {
        let lsid_str  = format!("{}\0{}\0", self.lsid, self.ls_key);
        let mut login_info = vec![0u8; SIZE_LOGIN_INFO];
        let lsid_bytes = lsid_str.as_bytes();
        login_info[..lsid_bytes.len().min(64)].copy_from_slice(&lsid_bytes[..lsid_bytes.len().min(64)]);
        stream.send_app_packet(OP_SEND_LOGIN_INFO, &login_info);
        eprintln!("EQ: sent login info to world (lsid={} key={})", self.lsid, self.ls_key);
    }

    fn on_zone_connected(&self, stream: &mut EqStream) {
        let mut cze = vec![0u8; SIZE_CLIENT_ZONE_ENTRY];
        let name_bytes = self.config.character_name.as_bytes();
        cze[4..4 + name_bytes.len().min(64)].copy_from_slice(&name_bytes[..name_bytes.len().min(64)]);
        stream.send_app_packet(OP_ZONE_ENTRY, &cze);
        eprintln!("EQ: sent zone entry for '{}'", self.config.character_name);
    }

    /// Process one packet for login-protocol transitions.
    /// `gs` is passed read-only so handlers can read player_name / spawn state.
    fn handle(&mut self, packet: &AppPacket, stream: &mut EqStream, gs: &GameState) -> PhaseResult {
        match packet.opcode {
            OP_CHAT_MESSAGE if !self.done_session_ready => {
                self.done_session_ready = true;
                self.send_credentials(stream);
                PhaseResult::Continue
            }
            OP_LOGIN_ACCEPTED if !self.done_login_accepted => {
                self.done_login_accepted = true;
                if !self.parse_login_accepted(&packet.payload) {
                    return PhaseResult::Error("Login credentials rejected".to_string());
                }
                self.send_server_list_request(stream);
                PhaseResult::Continue
            }
            OP_SERVER_LIST_RESPONSE if !self.done_server_list => {
                self.done_server_list = true;
                self.parse_server_list(&packet.payload);
                self.send_play_everquest(stream);
                PhaseResult::Continue
            }
            OP_PLAY_EVERQUEST_RESP if !self.done_play_everquest => {
                self.done_play_everquest = true;
                let host = if self.world_host.is_empty() {
                    self.config.login_host.clone()
                } else {
                    self.world_host.clone()
                };
                PhaseResult::ReconnectWorld { host, port: self.config.world_port }
            }
            OP_SEND_CHAR_INFO if !self.done_char_info => {
                self.done_char_info = true;
                self.send_enter_world(stream);
                PhaseResult::Continue
            }
            OP_ZONE_SERVER_INFO if !self.done_zone_server_info => {
                self.done_zone_server_info = true;
                if packet.payload.len() >= SIZE_ZONE_SERVER_INFO {
                    let info     = unsafe { safe_read::<ZoneServerInfo_S>(&packet.payload) };
                    let zone_ip  = String::from_utf8_lossy(&info.ip).trim_end_matches('\0').to_string();
                    let zone_ip  = if zone_ip.is_empty() || zone_ip == "0.0.0.0" {
                        self.config.login_host.clone()
                    } else { zone_ip };
                    return PhaseResult::ReconnectZone { host: zone_ip, port: info.port };
                }
                PhaseResult::Continue
            }
            OP_ZONE_ENTRY if !self.done_zone_entry => {
                self.done_zone_entry = true;
                // Server echoes back the player's own Spawn_S; register it.
                for offset in [0usize, 2, 4] {
                    if packet.payload.len() < offset + SIZE_SPAWN { continue; }
                    let spawn    = unsafe { safe_read::<Spawn_S>(&packet.payload[offset..]) };
                    let spawn_id = spawn.spawnId;
                    let name     = spawn.name_str();
                    if !name.is_empty() && name.chars().all(|c| c.is_ascii() && (c.is_alphanumeric() || c == '_')) {
                        eprintln!("EQ: server zone entry: spawn_id={} name={:?}", spawn_id, name);
                        // Use a local copy of gs for player_name; register_spawn needs &mut gs
                        // but we can't mutate here — the caller already called apply_packet
                        // which handled OP_ZONE_ENTRY.  Nothing extra to do.
                        break;
                    }
                }
                PhaseResult::Continue
            }
            OP_NEW_ZONE => {
                // apply_packet already updated gs.zone_name; just send protocol response.
                stream.send_app_packet(OP_REQ_CLIENT_SPAWN, &[]);
                eprintln!("EQ: zone: {} — sent ReqClientSpawn", gs.zone_name);
                PhaseResult::Continue
            }
            OP_WEATHER if !self.done_zone_weather => {
                self.done_zone_weather = true;
                stream.send_app_packet(OP_REQ_NEW_ZONE, &[]);
                eprintln!("EQ: zone weather received — sent ReqNewZone");
                PhaseResult::Continue
            }
            OP_SEND_EXP_ZONE_IN if !self.done_client_ready => {
                self.done_client_ready = true;
                stream.send_app_packet(OP_SEND_EXP_ZONE_IN, &[]);
                stream.send_app_packet(OP_CLIENT_READY, &[]);
                eprintln!("EQ: zone entry complete — gameplay starts");
                PhaseResult::Done
            }
            op => {
                // Suppress known noise: keepalive, ground spawns, doors, zone points
                // (handled by apply_packet), world-login flow opcodes.
                const SILENT: &[u16] = &[
                    0x700d, // server keepalive/time-sync
                    0x0f47, // OP_GroundSpawn — ground item drops, no action needed
                    0x4c24, // OP_SPAWN_DOOR — door positions, handled visually elsewhere
                    0x5996, // unknown empty packet, keepalive variant
                    0x3eba, // OP_SEND_ZONE_POINTS — handled by apply_packet
                    0x2372, // OP_SpecialMesg — NPC dialogue, handled by apply_packet
                    0x5a48, // OP_FormattedMessage — eqstr text, handled by apply_packet
                    0x673c, // OP_SimpleMessage — eqstr text, handled by apply_packet
                    0x547a, // OP_Emote — world/NPC emote, handled by apply_packet
                    0x7cba, // OP_ENTER_WORLD — world login flow
                    0x52a4, // OP_POST_ENTER_WORLD
                    0x04ec, // OP_EXPANSION_INFO
                    0x0fa6, // OP_LOG_SERVER
                    0x3c25, // OP_APPROVE_WORLD
                ];
                if !SILENT.contains(&op) {
                    eprintln!("EQ: unhandled opcode 0x{:04x} ({} bytes)", op, packet.payload.len());
                }
                PhaseResult::Continue
            }
        }
    }

    // ── Packet builders ───────────────────────────────────────────────────────

    fn send_credentials(&self, stream: &mut EqStream) {
        eprintln!("EQ: sending credentials for '{}'", self.config.username);
        let creds = format!("{}\0{}\0", self.config.username, self.config.password);
        let padded_len = ((creds.len() + 7) / 8) * 8;
        let mut creds_bytes = creds.into_bytes();
        creds_bytes.resize(padded_len, 0);
        let encrypted = des_encrypt(&creds_bytes);
        // Header: struct.pack('<ibbi', 3, 0, 2, 0)
        let mut payload = vec![3u8, 0, 0, 0,  0,  2,  0, 0, 0, 0];
        payload.extend_from_slice(&encrypted);
        stream.send_app_packet(OP_LOGIN, &payload);
    }

    fn send_server_list_request(&self, stream: &mut EqStream) {
        stream.send_app_packet(OP_SERVER_LIST_REQUEST, &4u32.to_le_bytes());
        eprintln!("EQ: requested server list");
    }

    fn send_play_everquest(&self, stream: &mut EqStream) {
        // Header: struct.pack('<ibbi', 5, 0, 0, 0)
        let mut payload = vec![5u8, 0, 0, 0,  0,  0,  0, 0, 0, 0];
        payload.extend_from_slice(&self.world_server_id.to_le_bytes());
        stream.send_app_packet(OP_PLAY_EVERQUEST_REQ, &payload);
        eprintln!("EQ: sent play everquest request (server_id={})", self.world_server_id);
    }

    fn send_enter_world(&self, stream: &mut EqStream) {
        let mut buf = vec![0u8; SIZE_ENTER_WORLD];
        let name_bytes = self.config.character_name.as_bytes();
        buf[..name_bytes.len().min(64)].copy_from_slice(&name_bytes[..name_bytes.len().min(64)]);
        stream.send_app_packet(OP_ENTER_WORLD, &buf);
        stream.send_app_packet(OP_POST_ENTER_WORLD, &[]);
        eprintln!("EQ: entering world as '{}'", self.config.character_name);
    }

    // ── Packet parsers ────────────────────────────────────────────────────────

    /// Returns false if the server rejected the credentials.
    fn parse_login_accepted(&mut self, payload: &[u8]) -> bool {
        if payload.len() < 10 {
            eprintln!("EQ: LoginAccepted too short ({} bytes) — assuming success", payload.len());
            self.lsid   = 1;
            self.ls_key = "0".to_string();
            return true;
        }
        let encrypted = &payload[10..];
        if encrypted.is_empty() {
            eprintln!("EQ: LoginAccepted has no encrypted block — assuming success");
            self.lsid   = 1;
            self.ls_key = "0".to_string();
            return true;
        }
        let dec = des_decrypt(encrypted);
        if dec.len() < 12 {
            eprintln!("EQ: decrypted LoginReply too short — assuming success");
            self.lsid   = 1;
            self.ls_key = "0".to_string();
            return true;
        }
        if dec[0] == 0 {
            let err_id = u32::from_le_bytes([dec[1], dec[2], dec[3], dec[4]]);
            eprintln!("EQ: login rejected (success=0, error_id={})", err_id);
            return false;
        }
        self.lsid = i32::from_le_bytes([dec[8], dec[9], dec[10], dec[11]]);
        let key_end = dec[12..].iter().position(|&b| b == 0)
            .map(|p| p + 12).unwrap_or(dec.len());
        let key = String::from_utf8_lossy(&dec[12..key_end]).to_string();
        self.ls_key = if key.is_empty() { "0".to_string() } else { key };
        eprintln!("EQ: login accepted: lsid={} key={}", self.lsid, self.ls_key);
        true
    }

    fn parse_server_list(&mut self, payload: &[u8]) {
        // ServerListReply: LoginBaseMessage prefix (15 or 16 bytes) + count(i32) + entries
        let mut offset = 16usize;
        if payload.len() < offset + 4 { offset = 15; }
        if payload.len() < offset + 4 {
            eprintln!("EQ: server list too short");
            self.world_server_id = 1;
            self.world_host = self.config.login_host.clone();
            return;
        }
        let count = i32::from_le_bytes([payload[offset], payload[offset+1], payload[offset+2], payload[offset+3]]);
        offset += 4;
        eprintln!("EQ: server list: {} server(s)", count);
        if count <= 0 || offset >= payload.len() { return; }

        // First entry: ip_str\0 + server_type(4) + server_id(4) + name_str\0 + ...
        if let Some(ip_end) = payload[offset..].iter().position(|&b| b == 0) {
            let world_host = String::from_utf8_lossy(&payload[offset..offset + ip_end]).to_string();
            offset += ip_end + 1;
            if offset + 8 <= payload.len() {
                let server_id = u32::from_le_bytes([
                    payload[offset+4], payload[offset+5], payload[offset+6], payload[offset+7],
                ]);
                offset += 8;
                let name_end = payload[offset..].iter().position(|&b| b == 0)
                    .unwrap_or(payload.len() - offset);
                let name = String::from_utf8_lossy(&payload[offset..offset + name_end]).to_string();
                eprintln!("EQ: world server: id={} name={} host={}", server_id, name, world_host);
                self.world_server_id = server_id;
                self.world_host = if world_host.is_empty() || world_host == "0.0.0.0" {
                    self.config.login_host.clone()
                } else {
                    world_host
                };
            }
        }
        if self.world_server_id == 0 { self.world_server_id = 1; }
        if self.world_host.is_empty() { self.world_host = self.config.login_host.clone(); }
    }
}
