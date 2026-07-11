//! EQ login protocol: login server → world server → zone server handshake.
//!
//! Handles only the connection/authentication state machine.  All gameplay
//! packet effects (spawn registration, HP updates, etc.) are delegated to
//! `packet_handler::apply_packet` so there is no duplication with the render loop.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::time::{Duration, sleep};

use cbc::{Decryptor, Encryptor};
use des::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::NoPadding};
use des::Des;

use crate::config::{CharacterCreate, LoginConfig};
use crate::eq_net::gameplay::{run_gameplay_phase};
use crate::eq_net::navigation::Navigator;
use crate::eq_net::packet_handler::apply_packet;
use crate::eq_net::protocol::*;
use crate::eq_net::transport::{AppPacket, EqStream};
use crate::game_state::GameState;
use crate::http::{AttackReq, BuyReq, SellReq, TradeReq, MerchantShared, DoorClickReq, DoorsShared, MoveReq, GiveReq, InventoryShared, LootReq, MessagesShared, DialogueShared, DialogueClickReq, NavStateShared, ChatEventsShared, ChatSendShared, CastReq, MemSpellReq, SitReq, ConsiderReq, CampReq, CampUntil, EntityIds, EntityPositions, GotoTarget, HailReq, SayReq, TargetReq, TaskLog, ZoneCrossReq, ZonePoints, TaskOffersShared, CompletedTasksShared, AcceptTaskReq, CancelTaskReq};

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
    nav_state:        NavStateShared,
    goto_entity:      crate::http::GotoEntity,
    entity_positions: EntityPositions,
    entity_ids:       EntityIds,
    zone_points:      ZonePoints,
    task_log:         TaskLog,
    task_offers_shared:    TaskOffersShared,
    completed_tasks_shared: CompletedTasksShared,
    accept_task:           AcceptTaskReq,
    cancel_task:           CancelTaskReq,
    group:             crate::http::GroupShared,
    group_invite:      crate::http::GroupInviteReq,
    trainer_open_req:  crate::http::TrainerOpenReq,
    trainer_train_req: crate::http::TrainerTrainReq,
    group_accept:      crate::http::GroupAcceptReq,
    group_decline:     crate::http::GroupDeclineReq,
    group_leave:       crate::http::GroupLeaveReq,
    group_kick:        crate::http::GroupKickReq,
    group_make_leader: crate::http::GroupMakeLeaderReq,
    zone_cross:       ZoneCrossReq,
    hail:             HailReq,
    say:              SayReq,
    target:           TargetReq,
    attack:           AttackReq,
    buy:              BuyReq,
    sell:             SellReq,
    trade:            TradeReq,
    merchant:         MerchantShared,
    move_req:         MoveReq,
    give:             GiveReq,
    inventory:        InventoryShared,
    loot:             LootReq,
    door_click:       DoorClickReq,
    doors_shared:     DoorsShared,
    messages:         MessagesShared,
    dialogue:         DialogueShared,
    dialogue_click:   DialogueClickReq,
    chat_events:      ChatEventsShared,
    chat_send:        ChatSendShared,
    cast:             CastReq,
    mem_spell:        MemSpellReq,
    sit:              SitReq,
    consider:         ConsiderReq,
    pet_cmd:          crate::http::PetCmdReq,
    collision:        crate::assets::SharedCollision,
    maps_dir:         std::path::PathBuf,
    shutdown:         Arc<AtomicBool>,
    camp:             CampReq,
    camp_until:       CampUntil,
    controller_view:  crate::http::ControllerShared,
    nav_intent:       crate::http::NavIntent,
    pos_correction:   crate::http::PosCorrection,
    nav_path_view:    crate::http::NavPathView,
    nav_avoid:        crate::http::NavAvoidShared,
) -> Result<(), String> {
    for attempt in 1..=max_retries {
        if attempt > 1 {
            tracing::warn!("EQ: retry {}/{}", attempt, max_retries);
            sleep(Duration::from_secs(3)).await;
        }
        match run_login_phase(&config, &app_tx).await {
            // A server-rejected create can't succeed on retry — surface it and stop now so the
            // user sees the real reason instead of an endless "Login timed out" loop. (#6)
            Err(LoginError::Fatal(e)) => return Err(e),
            Err(LoginError::Retryable(e)) => tracing::warn!("EQ: login failed (attempt {}): {}", attempt, e),
            Ok((stream, net_rx, gs, world_creds)) => {
                // Seed /entities map with everything discovered during login.
                {
                    let mut map = entity_positions.lock().unwrap();
                    let mut ids = entity_ids.lock().unwrap();
                    for (&id, e) in &gs.entities {
                        map.insert(e.name.clone(), (e.x, e.y, e.z));
                        ids.insert(e.name.clone(), id);
                    }
                    tracing::info!("NAV: entity map seeded with {} entities", map.len());
                }
                // Seed zone points (in case OP_SEND_ZONE_POINTS arrived during login phase).
                if !gs.zone_points.is_empty() {
                    *zone_points.lock().unwrap() = gs.zone_points.clone();
                    tracing::info!("NAV: {} zone points seeded", gs.zone_points.len());
                }
                let char_name = config.character_name.clone();
                let navigator = Navigator::new(goto_target, nav_state, goto_entity, entity_positions, entity_ids, zone_points, task_log, task_offers_shared, completed_tasks_shared, accept_task, cancel_task, group, group_invite, trainer_open_req, trainer_train_req, group_accept, group_decline, group_leave, group_kick, group_make_leader, zone_cross, hail, say, target, attack, buy, sell, trade, merchant, move_req, give, inventory, loot, door_click, doors_shared, messages, dialogue, dialogue_click, chat_events, chat_send, cast, mem_spell, sit, consider, pet_cmd, collision, maps_dir, camp.clone(), controller_view, nav_intent, pos_correction, nav_path_view, nav_avoid);
                run_gameplay_phase(stream, net_rx, app_tx, gs, char_name, navigator, world_creds, shutdown.clone(), camp.clone(), camp_until.clone()).await;
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
) -> Result<(EqStream, UnboundedReceiver<AppPacket>, GameState, WorldCredentials), LoginError> {
    let (net_tx, mut net_rx) = mpsc::unbounded_channel::<AppPacket>();

    tracing::info!("EQ: connecting to login server {}:{}", config.login_host, config.login_port);
    let mut stream = EqStream::connect(&config.login_host, config.login_port, net_tx.clone())
        .await
        .map_err(|e| format!("Login server connection failed: {e}"))?;

    tracing::info!("EQ: login session established — waiting for handshake");
    stream.send_app_packet(OP_SESSION_READY, &2u32.to_le_bytes());

    let mut proto = LoginProtocol::new(config);
    let mut gs    = GameState::new();
    gs.player_name = config.character_name.clone();

    'login: loop {
        stream.poll_recv();
        stream.poll_resend(); // retransmit un-ACKed login reliables (#254)

        while let Ok(packet) = net_rx.try_recv() {
            // Forward to render loop so it gets zone/spawn data during login.
            let _ = app_tx.send(AppPacket { opcode: packet.opcode, payload: packet.payload.clone() });

            // Apply gameplay side effects.
            apply_packet(&mut gs, &packet);

            // Handle login-protocol state transitions.
            match proto.handle(&packet, &mut stream, &gs) {
                PhaseResult::Continue                => {}
                PhaseResult::Done                    => break 'login,
                PhaseResult::Error(e)                => return Err(LoginError::Retryable(e)),
                PhaseResult::Fatal(e)                => return Err(LoginError::Fatal(e)),
                PhaseResult::ReconnectWorld { host, port } => {
                    drop(stream);
                    sleep(Duration::from_millis(100)).await;
                    tracing::info!("EQ: connecting to world {}:{}", host, port);
                    stream = EqStream::connect(&host, port, net_tx.clone())
                        .await
                        .map_err(|e| format!("World connection failed: {e}"))?;
                    proto.on_world_connected(&mut stream);
                }
                PhaseResult::ReconnectZone { host, port } => {
                    drop(stream);
                    sleep(Duration::from_millis(800)).await;
                    tracing::info!("EQ: connecting to zone {}:{}", host, port);
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
            return Err(LoginError::Retryable("Login timed out".to_string()));
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
    /// Unrecoverable handshake failure — retrying with the same config can't succeed (e.g. the
    /// server rejected character creation for an invalid combo). Stops the retry loop. (eqoxide#6)
    Fatal(String),
    ReconnectWorld { host: String, port: u16 },
    ReconnectZone  { host: String, port: u16 },
}

/// Outcome of one login attempt. `Retryable` errors (timeouts, transient connection drops) are
/// retried; `Fatal` errors (a server-rejected create) break the retry loop immediately so the user
/// sees the real reason instead of an endless "Login timed out". `?` on a `String` maps to
/// `Retryable` via the `From` impl, so existing connection error sites are unchanged.
enum LoginError {
    Retryable(String),
    Fatal(String),
}

impl From<String> for LoginError {
    fn from(s: String) -> Self { LoginError::Retryable(s) }
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { LoginError::Retryable(s) | LoginError::Fatal(s) => f.write_str(s) }
    }
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
    // Character-creation handshake (only used when `config.create` is set and the
    // configured character is missing from the OP_SendCharInfo char-select list).
    awaiting_name_approval: bool,
    create_attempted:       bool,
    /// True after OP_CharacterCreate is sent, while we await the server's verdict. Success arrives
    /// as a fresh OP_SendCharInfo; FAILURE arrives as OP_ApproveName{0} (EQEmu world/client.cpp),
    /// which — since `awaiting_name_approval` is already false by then — would otherwise be ignored
    /// and the client would hang/timeout. This flag routes that reply to a Fatal error. (eqoxide#6)
    awaiting_create_result: bool,
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
            awaiting_name_approval: false,
            awaiting_create_result: false,
            create_attempted:       false,
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
        tracing::info!("EQ: sent login info to world (lsid={} key={})", self.lsid, self.ls_key);
    }

    fn on_zone_connected(&self, stream: &mut EqStream) {
        let mut cze = vec![0u8; SIZE_CLIENT_ZONE_ENTRY];
        let name_bytes = self.config.character_name.as_bytes();
        cze[4..4 + name_bytes.len().min(64)].copy_from_slice(&name_bytes[..name_bytes.len().min(64)]);
        stream.send_app_packet(OP_ZONE_ENTRY, &cze);
        tracing::info!("EQ: sent zone entry for '{}'", self.config.character_name);
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
                let name = &self.config.character_name;
                if char_in_list(&packet.payload, name) {
                    // Character exists on the account — enter world as normal.
                    self.done_char_info = true;
                    self.awaiting_create_result = false; // create succeeded (char now in the list)
                    self.send_enter_world(stream);
                    PhaseResult::Continue
                } else if let Some(cc) = &self.config.create {
                    // Not on the account: run the create handshake, unless we
                    // already tried (and the fresh list still lacks the char).
                    if self.create_attempted {
                        return PhaseResult::Fatal(format!(
                            "character '{}' still missing after a create attempt — the server \
                             rejected creation. Check the character_create block in your config \
                             (valid race/class/deity combo, stats total, and start_zone = a zone_id).",
                            name));
                    }
                    self.create_attempted = true;
                    self.awaiting_name_approval = true;
                    let pkt = build_approve_name(name, cc.race, cc.class);
                    stream.send_app_packet(OP_APPROVE_NAME, &pkt);
                    tracing::info!("EQ: char '{}' not found — requesting name approval (race={} class={})",
                        name, cc.race, cc.class);
                    PhaseResult::Continue
                } else {
                    PhaseResult::Error(format!(
                        "character '{}' not on account and no character_create config", name))
                }
            }
            OP_APPROVE_NAME if self.awaiting_name_approval => {
                self.awaiting_name_approval = false;
                let approved = packet.payload.first().copied().unwrap_or(0) == 1;
                if !approved {
                    return PhaseResult::Error(format!(
                        "server rejected character name '{}'", self.config.character_name));
                }
                let cc = self.config.create.as_ref().expect("create set while awaiting approval");
                let pkt = build_char_create(cc);
                stream.send_app_packet(OP_CHARACTER_CREATE, &pkt);
                self.awaiting_create_result = true;
                tracing::info!("EQ: name approved — sent OP_CharacterCreate for '{}' (race={} class={} deity={} zone={})",
                    self.config.character_name, cc.race, cc.class, cc.deity, cc.start_zone);
                // Server replies with a fresh OP_SendCharInfo (success) or OP_ApproveName{0}
                // (failure) — the latter is caught by the awaiting_create_result arm below.
                PhaseResult::Continue
            }
            // Create verdict: after OP_CharacterCreate, an OP_ApproveName reply means FAILURE
            // (success comes as OP_SendCharInfo). EQEmu sends OP_ApproveName{0} when
            // CheckCharCreateInfoSoF rejects the combo. Surface a clear, actionable Fatal error
            // and stop retrying — the same config will be rejected every time. (eqoxide#6)
            OP_APPROVE_NAME if self.awaiting_create_result => {
                self.awaiting_create_result = false;
                PhaseResult::Fatal(format!(
                    "server rejected character creation for '{}' (race={} class={} deity={} \
                     start_zone={}). The combo is invalid, the stats don't total, or start_zone \
                     isn't a valid zone_id for it. Fix the character_create block in your config.",
                    self.config.character_name,
                    self.config.create.as_ref().map(|c| c.race).unwrap_or(0),
                    self.config.create.as_ref().map(|c| c.class).unwrap_or(0),
                    self.config.create.as_ref().map(|c| c.deity).unwrap_or(0),
                    self.config.create.as_ref().map(|c| c.start_zone).unwrap_or(0)))
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
                // Server echoes back the player's own spawn in RoF2 variable-length format.
                // apply_packet already handled OP_ZONE_ENTRY; just log the spawn for debugging.
                if let Some((info, _)) = parse_rof2_spawn(&packet.payload) {
                    if !info.name.is_empty() {
                        tracing::info!("EQ: server zone entry: spawn_id={} name={:?}",
                            info.spawn_id, info.name);
                    }
                }
                PhaseResult::Continue
            }
            OP_NEW_ZONE => {
                // apply_packet already updated gs.zone_name; just send protocol response.
                stream.send_app_packet(OP_REQ_CLIENT_SPAWN, &[]);
                tracing::info!("EQ: zone: {} — sent ReqClientSpawn", gs.zone_name);
                PhaseResult::Continue
            }
            OP_WEATHER if !self.done_zone_weather => {
                self.done_zone_weather = true;
                stream.send_app_packet(OP_REQ_NEW_ZONE, &[]);
                tracing::info!("EQ: zone weather received — sent ReqNewZone");
                PhaseResult::Continue
            }
            OP_SEND_EXP_ZONE_IN if !self.done_client_ready => {
                self.done_client_ready = true;
                stream.send_app_packet(OP_SEND_EXP_ZONE_IN, &[]);
                stream.send_app_packet(OP_CLIENT_READY, &[]);
                tracing::info!("EQ: zone entry complete — gameplay starts");
                PhaseResult::Done
            }
            op => {
                // Suppress known noise: keepalive, ground spawns, zone points
                // (handled by apply_packet), world-login flow opcodes.
                // RoF2 opcode values from patch_RoF2.conf.
                const SILENT: &[u16] = &[
                    // Char-select world chatter — informational packets sent to every client;
                    // this headless client renders no char-select/guild/membership UI, so
                    // dropping them is correct (eqoxide#20). Values from patch_RoF2.conf.
                    0x507a, // OP_GuildsList — guild dropdown list
                    0x5475, // OP_SendMaxCharacters — character-slot count
                    0x7acc, // OP_SendMembership — gold/silver account status
                    0x057b, // OP_SendMembershipDetails — membership feature matrix
                    0x6fca, // OP_GroundSpawn (RoF2) — ground item drops, no action needed
                    OP_SEND_ZONE_POINTS, // handled by apply_packet
                    OP_SPECIAL_MESG,     // NPC dialogue, handled by apply_packet
                    OP_FORMATTED_MESSAGE, // eqstr text, handled by apply_packet
                    OP_SIMPLE_MESSAGE,   // eqstr text, handled by apply_packet
                    OP_EMOTE,            // world/NPC emote, handled by apply_packet
                    OP_ENTER_WORLD,      // world login flow
                    OP_POST_ENTER_WORLD,
                    OP_EXPANSION_INFO,
                    OP_LOG_SERVER,
                    OP_APPROVE_WORLD,
                ];
                if !SILENT.contains(&op) {
                    tracing::info!("EQ: unhandled opcode 0x{:04x} ({} bytes)", op, packet.payload.len());
                }
                PhaseResult::Continue
            }
        }
    }

    // ── Packet builders ───────────────────────────────────────────────────────

    fn send_credentials(&self, stream: &mut EqStream) {
        tracing::info!("EQ: sending credentials for '{}'", self.config.username);
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
        tracing::info!("EQ: requested server list");
    }

    fn send_play_everquest(&self, stream: &mut EqStream) {
        // Header: struct.pack('<ibbi', 5, 0, 0, 0)
        let mut payload = vec![5u8, 0, 0, 0,  0,  0,  0, 0, 0, 0];
        payload.extend_from_slice(&self.world_server_id.to_le_bytes());
        stream.send_app_packet(OP_PLAY_EVERQUEST_REQ, &payload);
        tracing::info!("EQ: sent play everquest request (server_id={})", self.world_server_id);
    }

    fn send_enter_world(&self, stream: &mut EqStream) {
        let mut buf = vec![0u8; SIZE_ENTER_WORLD];
        let name_bytes = self.config.character_name.as_bytes();
        buf[..name_bytes.len().min(64)].copy_from_slice(&name_bytes[..name_bytes.len().min(64)]);
        stream.send_app_packet(OP_ENTER_WORLD, &buf);
        stream.send_app_packet(OP_POST_ENTER_WORLD, &[]);
        tracing::info!("EQ: entering world as '{}'", self.config.character_name);
    }

    // ── Packet parsers ────────────────────────────────────────────────────────

    /// Returns false if the server rejected the credentials.
    fn parse_login_accepted(&mut self, payload: &[u8]) -> bool {
        if payload.len() < 10 {
            tracing::info!("EQ: LoginAccepted too short ({} bytes) — assuming success", payload.len());
            self.lsid   = 1;
            self.ls_key = "0".to_string();
            return true;
        }
        let encrypted = &payload[10..];
        if encrypted.is_empty() {
            tracing::info!("EQ: LoginAccepted has no encrypted block — assuming success");
            self.lsid   = 1;
            self.ls_key = "0".to_string();
            return true;
        }
        let dec = des_decrypt(encrypted);
        if dec.len() < 12 {
            tracing::info!("EQ: decrypted LoginReply too short — assuming success");
            self.lsid   = 1;
            self.ls_key = "0".to_string();
            return true;
        }
        if dec[0] == 0 {
            let err_id = u32::from_le_bytes([dec[1], dec[2], dec[3], dec[4]]);
            tracing::error!("EQ: login rejected (success=0, error_id={})", err_id);
            return false;
        }
        self.lsid = i32::from_le_bytes([dec[8], dec[9], dec[10], dec[11]]);
        let key_end = dec[12..].iter().position(|&b| b == 0)
            .map(|p| p + 12).unwrap_or(dec.len());
        let key = String::from_utf8_lossy(&dec[12..key_end]).to_string();
        self.ls_key = if key.is_empty() { "0".to_string() } else { key };
        tracing::info!("EQ: login accepted: lsid={} key={}", self.lsid, self.ls_key);
        true
    }

    fn parse_server_list(&mut self, payload: &[u8]) {
        // ServerListReply: LoginBaseMessage prefix (15 or 16 bytes) + count(i32) + entries
        let mut offset = 16usize;
        if payload.len() < offset + 4 { offset = 15; }
        if payload.len() < offset + 4 {
            tracing::info!("EQ: server list too short");
            self.world_server_id = 1;
            self.world_host = self.config.login_host.clone();
            return;
        }
        let count = i32::from_le_bytes([payload[offset], payload[offset+1], payload[offset+2], payload[offset+3]]);
        offset += 4;
        tracing::info!("EQ: server list: {} server(s)", count);
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
                tracing::info!("EQ: world server: id={} name={} host={}", server_id, name, world_host);
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

// ── Character-creation helpers ─────────────────────────────────────────────────

/// Normalize a character name to Titanium's convention: first letter uppercase,
/// the rest lowercase (the native client enforces this on the create screen).
fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, c) in name.chars().enumerate() {
        if i == 0 { out.extend(c.to_uppercase()); }
        else      { out.extend(c.to_lowercase()); }
    }
    out
}

/// True if `name` appears as a null-terminated entry in an OP_SendCharInfo
/// payload (CharacterSelect_Struct holds up to 10 fixed-width name buffers).
/// Case-insensitive; the trailing NUL guards against prefix collisions.
fn char_in_list(payload: &[u8], name: &str) -> bool {
    let needle = name.as_bytes();
    if needle.is_empty() { return false; }
    payload.windows(needle.len() + 1)
        .any(|w| w[needle.len()] == 0 && w[..needle.len()].eq_ignore_ascii_case(needle))
}

/// Build the 72-byte NameApproval_Struct (OP_ApproveName): name[64] + race u32 + class u32.
fn build_approve_name(name: &str, race: u32, class: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 72];
    let nm = normalize_name(name);
    let b  = nm.as_bytes();
    let n  = b.len().min(63); // leave room for the NUL terminator
    buf[..n].copy_from_slice(&b[..n]);
    buf[64..68].copy_from_slice(&race.to_le_bytes());
    buf[68..72].copy_from_slice(&class.to_le_bytes());
    buf
}

/// Build the 96-byte RoF2 CharCreate_Struct (OP_CharacterCreate): 24 little-endian
/// u32 fields in wire order. No name field — the name was sent via OP_ApproveName.
fn build_char_create(cc: &CharacterCreate) -> Vec<u8> {
    // RoF2 CharCreate_Struct = 96 bytes (24 x u32). The server negotiates RoF2 (we parse RoF2
    // spawns/items) and rejects the old Titanium 80-byte layout for wrong size. Field order is
    // from EQEmu common/patches/rof2_structs.h CharCreate_Struct — DIFFERENT from Titanium:
    // drakkin_* and unknown0092 are new, and the stats move after the appearance block.
    let fields: [u32; 24] = [
        cc.gender,     // 0x00 gender
        cc.race,       // 0x04 race
        cc.class,      // 0x08 class_
        cc.deity,      // 0x0C deity
        cc.start_zone, // 0x10 start_zone — a ZONE_ID (not a Titanium StartZoneIndex). RoF2's
                       // CheckCharCreateInfoSoF matches this against char_create_combinations.start_zone
                       // (zone_ids); sending a 0..13 index is rejected → silent create loop. See eqoxide#5.
        cc.haircolor,  // 0x14 haircolor
        cc.beard,      // 0x18 beard
        cc.beardcolor, // 0x1C beardcolor
        cc.hairstyle,  // 0x20 hairstyle
        cc.face,       // 0x24 face
        cc.eyecolor1,  // 0x28 eyecolor1
        cc.eyecolor2,  // 0x2C eyecolor2
        0,             // 0x30 drakkin_heritage (non-Drakkin: 0)
        0,             // 0x34 drakkin_tattoo
        0,             // 0x38 drakkin_details
        cc.str_,       // 0x3C STR
        cc.sta,        // 0x40 STA
        cc.agi,        // 0x44 AGI
        cc.dex,        // 0x48 DEX
        cc.wis,        // 0x4C WIS
        cc.int_,       // 0x50 INT
        cc.cha,        // 0x54 CHA
        0,             // 0x58 tutorial (0 = normal)
        0,             // 0x5C unknown0092
    ];
    let mut buf = Vec::with_capacity(96);
    for f in fields { buf.extend_from_slice(&f.to_le_bytes()); }
    buf
}

#[cfg(test)]
mod charcreate_tests {
    use super::*;

    fn de_sk() -> CharacterCreate {
        // Dark Elf (6) Shadow Knight (5), validated stats summing to 582.
        // start_zone is a ZONE_ID: 42 = neriakc (a valid Dark Elf start city); a StartZoneIndex
        // such as 5 would be rejected by RoF2's CheckCharCreateInfoSoF. See eqoxide#5.
        CharacterCreate {
            race: 6, class: 5, gender: 0, deity: 206, start_zone: 42,
            str_: 70, sta: 70, agi: 90, dex: 75, wis: 83, int_: 129, cha: 65,
            face: 0, hairstyle: 0, haircolor: 0, beard: 0, beardcolor: 0,
            eyecolor1: 0, eyecolor2: 0,
        }
    }

    #[test]
    fn approve_name_layout() {
        let pkt = build_approve_name("mordeth", 6, 5);
        assert_eq!(pkt.len(), 72);
        assert_eq!(&pkt[..7], b"Mordeth");      // normalized capitalization
        assert_eq!(pkt[7], 0);                  // NUL-terminated
        assert_eq!(u32::from_le_bytes(pkt[64..68].try_into().unwrap()), 6);
        assert_eq!(u32::from_le_bytes(pkt[68..72].try_into().unwrap()), 5);
    }

    #[test]
    fn char_create_layout_and_stat_total() {
        // RoF2 layout: 96 bytes (24 x u32), order gender,race,class,deity,start_zone, hair/face
        // block, drakkin_*, then STR..CHA, tutorial, unknown0092.
        let cc = de_sk();
        let pkt = build_char_create(&cc);
        assert_eq!(pkt.len(), 96, "RoF2 CharCreate_Struct must be 96 bytes");
        let f = |i: usize| u32::from_le_bytes(pkt[i*4..i*4+4].try_into().unwrap());
        assert_eq!(f(0), 0);    // gender
        assert_eq!(f(1), 6);    // race (Dark Elf)
        assert_eq!(f(2), 5);    // class (Shadow Knight)
        assert_eq!(f(3), 206);  // deity (Innoruuk)
        assert_eq!(f(4), 42);   // start_zone = zone_id (42 = neriakc), NOT a StartZoneIndex
        assert_eq!(f(12), 0);   // drakkin_heritage (non-Drakkin)
        assert_eq!(f(22), 0);   // tutorial
        assert_eq!(f(23), 0);   // unknown0092
        // Stats occupy fields 15..=21 and must total exactly 582 for DE SK.
        let total: u32 = (15..=21).map(f).sum();
        assert_eq!(total, 582);
    }

    #[test]
    fn char_in_list_matches_exact_only() {
        // Two fixed-width name buffers: "Durgan\0..." then "Mordeth\0...".
        let mut payload = vec![0u8; 128];
        payload[..6].copy_from_slice(b"Durgan");
        payload[64..71].copy_from_slice(b"Mordeth");
        assert!(char_in_list(&payload, "Mordeth"));
        assert!(char_in_list(&payload, "mordeth"));   // case-insensitive
        assert!(char_in_list(&payload, "Durgan"));
        assert!(!char_in_list(&payload, "Mord"));     // prefix must not match
        assert!(!char_in_list(&payload, "Katie"));
    }

    #[test]
    fn login_error_from_string_is_retryable() {
        // The `?` operator on connection/timeout error sites yields String -> LoginError via this
        // From impl; it MUST map to Retryable so transient failures still retry. Only an explicit
        // PhaseResult::Fatal (server-rejected create) stops the loop. (eqoxide#6)
        let e: super::LoginError = "World connection failed".to_string().into();
        assert!(matches!(e, super::LoginError::Retryable(_)));
        assert_eq!(e.to_string(), "World connection failed");
        assert_eq!(super::LoginError::Fatal("rejected".into()).to_string(), "rejected");
    }
}
