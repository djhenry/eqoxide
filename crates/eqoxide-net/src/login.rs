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

use eqoxide_core::config::{CharacterCreate, LoginConfig};
use crate::gameplay::{record_app_packet, run_gameplay_phase};
use crate::action_loop::ActionLoop;
use crate::packet_handler::apply_packet;
use crate::protocol::*;
use crate::transport::{AppPacket, EqStream};
use eqoxide_core::game_state::GameState;
use eqoxide_ipc::{CampReq, CampUntil, RespawnReq};

type DesCbcEnc = Encryptor<Des>;
type DesCbcDec = Decryptor<Des>;

// ── DES helpers ──────────────────────────────────────────────────────────────
// The EQEmu loginserver encrypts the OP_Login credential block and the OP_LoginAccepted
// reply with DES-CBC using an all-zero key AND all-zero IV (loginserver/encryption.cpp
// `eqcrypt_block`). This is shared by BOTH the Titanium and the SoD/RoF2 login listeners —
// SoD did NOT change the login encryption, only the response opcode numbers (#404). So these
// helpers are correct for the SoD handshake unchanged.

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
#[allow(clippy::too_many_arguments)]
pub async fn run_login_flow(
    config:          LoginConfig,
    max_retries:     u32,
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
    nav_debug:       eqoxide_nav::diagnostics::NavDebugView,
    shutdown:        Arc<AtomicBool>,
    camp:            CampReq,
    camp_until:      CampUntil,
    respawn:         RespawnReq,
    game_state_snapshot: eqoxide_ipc::GameStateSnapshot,
    net_health:          eqoxide_ipc::NetHealthShared,
) -> Result<(), String> {
    for attempt in 1..=max_retries {
        if attempt > 1 {
            tracing::warn!("EQ: retry {}/{}", attempt, max_retries);
            sleep(Duration::from_secs(3)).await;
        }
        match run_login_phase(&config, &net_health).await {
            // A server-rejected create can't succeed on retry — surface it and stop now so the
            // user sees the real reason instead of an endless "Login timed out" loop. (#6)
            Err(LoginError::Fatal(e)) => return Err(e),
            Err(LoginError::Retryable(e)) => tracing::warn!("EQ: login failed (attempt {}): {}", attempt, e),
            Ok((stream, net_rx, gs, world_creds)) => {
                // Seed /entities map with everything discovered during login.
                {
                    let mut map = world.entity_positions.lock().unwrap();
                    let mut ids = world.entity_ids.lock().unwrap();
                    // #643: pose/gait is seeded HERE TOO, not only by `ActionLoop::sync_entities`.
                    // This is the second publisher of `entity_positions`; if it seeded positions
                    // without poses, `/v1/observe/entities?labeled=1` would report entities whose
                    // `poses` key is missing for the whole window between login and the first nav
                    // tick — the exact KeyError-on-a-race the handler now promises cannot happen.
                    // Same lock order as `sync_entities`: positions → ids → poses.
                    let mut poses = world.entity_poses.lock().unwrap();
                    for (&id, e) in &gs.world.entities {
                        map.insert(e.name.clone(), (e.x, e.y, e.z));
                        ids.insert(e.name.clone(), id);
                        poses.insert(e.name.clone(), eqoxide_ipc::EntityPoseView {
                            pose: e.pose.label(),
                            gait: e.gait.map(|g| g.raw()),
                        });
                    }
                    tracing::info!("NAV: entity map seeded with {} entities", map.len());
                }
                // Seed zone points (in case OP_SEND_ZONE_POINTS arrived during login phase).
                if !gs.world.zone_points.is_empty() {
                    *world.zone_points.lock().unwrap() = gs.world.zone_points.clone();
                    tracing::info!("NAV: {} zone points seeded", gs.world.zone_points.len());
                }
                let char_name = config.character_name.clone();
                let action_loop = ActionLoop::new(
                    nav, world, quest, group_slots, command, social,
                    merchant_slots, inventory_slots, interact, chat, controller, guild_slots,
                    collision, maps_dir, nav_debug,
                );
                run_gameplay_phase(stream, net_rx, gs, char_name, action_loop, world_creds, shutdown.clone(), camp.clone(), camp_until.clone(), respawn.clone(), game_state_snapshot, net_health).await;
                return Ok(());
            }
        }
    }
    Err(format!("Login failed after {} attempts", max_retries))
}

// ── Login phase ───────────────────────────────────────────────────────────────

/// #419: the login handshake drain's per-packet liveness stamp. It MUST go through the canonical
/// `record_app_packet` recorder rather than writing `NetHealth::last_packet` directly, so there is a
/// single writer of that liveness field — the one that also clears the wedge-streak clock
/// (`first_unanswered_probe_sent`). This is defensive/latent hygiene, not an active-bug fix: no code
/// path re-enters login after gameplay today (so no stale streak can reach this drain), but a
/// bypassing raw write here would resurrect the #371 false-alive if a relogin-without-restart path is
/// ever added. Extracted into a named helper so `login_liveness_stamp_goes_through_canonical_recorder`
/// can pin it: reverting this body to a raw `last_packet = now` write makes that test RED.
fn record_login_liveness(net_health: &eqoxide_ipc::NetHealthShared, now: std::time::Instant) {
    record_app_packet(&mut net_health.lock().unwrap(), now);
}

/// Run the full handshake (login → world → zone entry).
/// Returns the live zone stream, its packet receiver, accumulated GameState, and world credentials.
///
/// `last_inbound` is bumped as real inbound packets are drained here, exactly like the gameplay
/// loop's drain does (gameplay.rs) — this handshake can legitimately run past CONN_STALE_SECS
/// (multiple server hops + a fresh char list), and without this the connection-health check
/// (`connected` in `/v1/observe/debug`) would falsely go stale/disconnected while login is healthy
/// and simply still in progress.
async fn run_login_phase(
    config: &LoginConfig,
    net_health: &eqoxide_ipc::NetHealthShared,
) -> Result<(EqStream, UnboundedReceiver<AppPacket>, GameState, WorldCredentials), LoginError> {
    let (net_tx, mut net_rx) = mpsc::unbounded_channel::<AppPacket>();

    tracing::info!("EQ: connecting to login server {}:{}", config.login_host, config.login_port);
    let mut stream = EqStream::connect(&config.login_host, config.login_port, net_tx.clone(), net_health.clone())
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
            // Apply gameplay side effects.
            apply_packet(&mut gs, &packet);
            record_login_liveness(&net_health, std::time::Instant::now());

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
                    stream = EqStream::connect(&host, port, net_tx.clone(), net_health.clone())
                        .await
                        .map_err(|e| format!("World connection failed: {e}"))?;
                    proto.on_world_connected(&mut stream);
                }
                PhaseResult::ReconnectZone { host, port } => {
                    drop(stream);
                    sleep(Duration::from_millis(800)).await;
                    tracing::info!("EQ: connecting to zone {}:{}", host, port);
                    stream = EqStream::connect(&host, port, net_tx.clone(), net_health.clone())
                        .await
                        .map_err(|e| format!("Zone connection failed: {e}"))?;
                    // Purge stale spawns/doors before zone entry so the OP_ZoneSpawns/OP_SpawnDoor
                    // stream repopulates fresh, and re-arm the once-per-zone-in OP_NewZone apply (#322).
                    gs.begin_zone_in();
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
    done_new_zone:         bool,
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
            done_new_zone:         false,
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
            // Only the FIRST OP_NewZone drives the handshake: the server sends a second copy in reply
            // to the OP_ReqNewZone below, and re-sending OP_ReqClientSpawn on it would make the
            // server re-issue the whole door/object/zone-point stream (#322).
            OP_NEW_ZONE if !self.done_new_zone => {
                self.done_new_zone = true;
                // apply_packet already updated gs.world.zone_name; just send protocol response.
                stream.send_app_packet(OP_REQ_CLIENT_SPAWN, &[]);
                tracing::info!("EQ: zone: {} — sent ReqClientSpawn", gs.world.zone_name);
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
                    // this headless client renders no char-select/membership UI, so
                    // dropping them is correct (eqoxide#20). Values from patch_RoF2.conf.
                    // (OP_GuildsList 0x507a is NOT dropped — apply_packet parses it into the guild
                    // directory for /v1/guild/* and /observe/debug guild identity, #295.)
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
                    // SoD login listener sends this just before OP_LoginAccepted; it carries
                    // only expansion-offer data this headless client doesn't use (#404).
                    OP_LOGIN_EXPANSION_PACKET_DATA,
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
        stream.send_app_packet(OP_LOGIN, &build_login_request(&self.config.username, &self.config.password));
    }

    fn send_server_list_request(&self, stream: &mut EqStream) {
        stream.send_app_packet(OP_SERVER_LIST_REQUEST, &4u32.to_le_bytes());
        tracing::info!("EQ: requested server list");
    }

    fn send_play_everquest(&self, stream: &mut EqStream) {
        stream.send_app_packet(OP_PLAY_EVERQUEST_REQ, &build_play_everquest(self.world_server_id));
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
        match parse_login_accepted_payload(payload) {
            Some((lsid, key)) => {
                self.lsid   = lsid;
                self.ls_key = key;
                tracing::info!("EQ: login accepted: lsid={} key={}", self.lsid, self.ls_key);
                true
            }
            None => {
                tracing::error!("EQ: login rejected by server (success=0)");
                false
            }
        }
    }

    fn parse_server_list(&mut self, payload: &[u8]) {
        let (server_id, host, name) = parse_server_list_payload(payload, &self.config.login_host);
        tracing::info!("EQ: world server: id={} name={} host={}", server_id, name, host);
        self.world_server_id = server_id;
        self.world_host = host;
    }
}

// ── SoD login packet builders / parsers (pure, unit-tested) ────────────────────
//
// The SoD (RoF2) login uses the SAME packet layouts and DES-CBC zero-key encryption as the
// legacy Titanium login; only the response opcode numbers differ (see protocol.rs). Ground
// truth for these layouts: EQEmu loginserver/login_types.h (LoginBaseMessage,
// LoginBaseReplyMessage, PlayerLoginReply, ServerListReply) and loginserver/client.cpp +
// world_server_manager.cpp (byte-for-byte serialization). #404.

/// Build the OP_Login credential packet: a 10-byte unencrypted `LoginBaseMessage`
/// (sequence=3 "login", encrypt_type=2 "DES") followed by the DES-CBC(zero-key) encryption
/// of `"user\0pass\0"`, zero-padded to an 8-byte boundary (server rejects non-multiples of 8).
fn build_login_request(user: &str, pass: &str) -> Vec<u8> {
    let creds = format!("{user}\0{pass}\0");
    let padded_len = creds.len().div_ceil(8) * 8;
    let mut creds_bytes = creds.into_bytes();
    creds_bytes.resize(padded_len, 0);
    let encrypted = des_encrypt(&creds_bytes);
    // LoginBaseMessage: sequence=3 (i32 LE), compressed=0, encrypt_type=2, unk3=0 (i32 LE).
    let mut payload = vec![3u8, 0, 0, 0,  0,  2,  0, 0, 0, 0];
    payload.extend_from_slice(&encrypted);
    payload
}

/// Build the OP_PlayEverquestRequest packet: a 10-byte `LoginBaseMessage` (sequence=5)
/// followed by the u32 world server id to join.
fn build_play_everquest(server_id: u32) -> Vec<u8> {
    let mut payload = vec![5u8, 0, 0, 0,  0,  0,  0, 0, 0, 0];
    payload.extend_from_slice(&server_id.to_le_bytes());
    payload
}

/// Parse an OP_LoginAccepted payload. Returns `Some((lsid, session_key))` on success, or
/// `None` if the server explicitly rejected the credentials (decrypted `success` byte == 0).
///
/// Layout: `[LoginBaseMessage: 10 bytes][DES-CBC(zero-key) PlayerLoginReply]`. The decrypted
/// PlayerLoginReply (login_types.h) is packed: success@0, error_str_id@1..5, str[1]@5,
/// unk1@6, unk2@7, lsid@8..12, key[11]@12 (client reads to the NUL).
///
/// Malformed-but-not-rejected cases (too short to hold a reply) preserve the historical
/// permissive behavior: assume success with a placeholder (lsid=1, key="0"). Only an explicit
/// success=0 is treated as a rejection, so a real failure is never silently reported as success.
fn parse_login_accepted_payload(payload: &[u8]) -> Option<(i32, String)> {
    const PLACEHOLDER: (i32, &str) = (1, "0");
    if payload.len() < 10 { return Some((PLACEHOLDER.0, PLACEHOLDER.1.to_string())); }
    let encrypted = &payload[10..];
    if encrypted.is_empty() { return Some((PLACEHOLDER.0, PLACEHOLDER.1.to_string())); }
    let dec = des_decrypt(encrypted);
    if dec.len() < 12 { return Some((PLACEHOLDER.0, PLACEHOLDER.1.to_string())); }
    if dec[0] == 0 { return None; } // explicit rejection (success=0)
    let lsid = i32::from_le_bytes([dec[8], dec[9], dec[10], dec[11]]);
    let key_end = dec[12..].iter().position(|&b| b == 0)
        .map(|p| p + 12).unwrap_or(dec.len());
    let key = String::from_utf8_lossy(&dec[12..key_end]).to_string();
    let key = if key.is_empty() { "0".to_string() } else { key };
    Some((lsid, key))
}

/// Parse an OP_ServerListResponse payload. Returns `(world_server_id, world_host, world_name)`,
/// falling back to `fallback_host` when the advertised host is empty/`0.0.0.0`.
///
/// Layout (world_server_manager.cpp CreateServerListPacket): `LoginBaseMessage` (10) +
/// `LoginBaseReplyMessage` (success@10, error_str_id@11..15, empty str NUL@15) + server_count
/// (i32)@16 + entries. Each entry: `ip\0` + server_type(i32) + server_id(u32) + `name\0` + ...
/// (Titanium and SoD entries are identical — only cv_larion differs in world_server.cpp.)
fn parse_server_list_payload(payload: &[u8], fallback_host: &str) -> (u32, String, String) {
    let fb = || fallback_host.to_string();
    let mut offset = 16usize;
    if payload.len() < offset + 4 { offset = 15; } // defensive fallback for a shorter prefix
    if payload.len() < offset + 4 {
        return (1, fb(), String::new());
    }
    let count = i32::from_le_bytes([payload[offset], payload[offset+1], payload[offset+2], payload[offset+3]]);
    offset += 4;
    if count <= 0 || offset >= payload.len() {
        return (1, fb(), String::new());
    }

    let mut server_id = 0u32;
    let mut host = String::new();
    let mut name = String::new();
    // First entry: ip_str\0 + server_type(4) + server_id(4) + name_str\0 + ...
    if let Some(ip_end) = payload[offset..].iter().position(|&b| b == 0) {
        let world_host = String::from_utf8_lossy(&payload[offset..offset + ip_end]).to_string();
        offset += ip_end + 1;
        if offset + 8 <= payload.len() {
            server_id = u32::from_le_bytes([
                payload[offset+4], payload[offset+5], payload[offset+6], payload[offset+7],
            ]);
            offset += 8;
            let name_end = payload[offset..].iter().position(|&b| b == 0)
                .unwrap_or(payload.len() - offset);
            name = String::from_utf8_lossy(&payload[offset..offset + name_end]).to_string();
            host = if world_host.is_empty() || world_host == "0.0.0.0" { fb() } else { world_host };
        }
    }
    if server_id == 0 { server_id = 1; }
    if host.is_empty() { host = fb(); }
    (server_id, host, name)
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

// ── SoD login handshake tests (#404) ──────────────────────────────────────────
// Exercise the SoD login packet builders/parsers against the exact byte layouts from EQEmu's
// loginserver source (login_opcodes_sod.conf, login_types.h, client.cpp,
// world_server_manager.cpp). Each test is written to go RED if the SoD format is reverted to
// Titanium or the field offsets drift. Live round-trip (server accepts the SoD handshake and
// hands off to world) is the orchestrator's build-and-verify gate.
#[cfg(test)]
mod liveness_stamp_tests {
    use super::record_login_liveness;
    use std::time::{Duration, Instant};

    /// Pins the #419 fix at the login call site: the handshake drain's liveness stamp must route
    /// through `record_app_packet` (the single canonical writer), which clears the wedge-streak clock
    /// `first_unanswered_probe_sent` in addition to bumping `last_packet`. We seed an outstanding
    /// streak (as if a probe were in flight), call the exact helper `run_login_phase` calls, and
    /// assert BOTH effects. Reverting `record_login_liveness`'s body to a raw
    /// `net_health.lock().unwrap().last_packet = now` write leaves the streak set → this goes RED,
    /// so a future revert of the fix cannot pass unnoticed (the defect the reviewer flagged).
    #[test]
    fn login_liveness_stamp_goes_through_canonical_recorder() {
        let base = Instant::now();
        let health = std::sync::Arc::new(std::sync::Mutex::new(eqoxide_ipc::NetHealth {
            last_datagram: base, last_packet: base, last_tick: base,
            last_probe_sent: Some(base), last_probe_reply: None,
            first_unanswered_probe_sent: Some(base),
            ..eqoxide_ipc::NetHealth::default()
        }));

        let stamp_at = base + Duration::from_secs(65);
        record_login_liveness(&health, stamp_at);

        let h = health.lock().unwrap();
        assert_eq!(h.last_packet, stamp_at, "the stamp must update last_packet");
        assert!(h.first_unanswered_probe_sent.is_none(),
            "the login liveness stamp must go through record_app_packet, which clears the wedge-streak \
             clock — a raw `last_packet = now` write would leave it set (the #419 seam)");
    }
}

#[cfg(test)]
mod sod_login_tests {
    use super::*;

    /// The migration itself: eqoxide must use the SoD (RoF2) login opcodes, NOT Titanium's.
    /// Mutation-check: revert any SoD value to its Titanium value (0x0017→0x0016, 0x0018→0x0017,
    /// 0x0019→0x0018, 0x0022→0x0021) and this goes RED.
    #[test]
    fn login_opcodes_are_sod_not_titanium() {
        // C→S request opcodes — identical in both listeners.
        assert_eq!(OP_SESSION_READY, 0x0001);
        assert_eq!(OP_LOGIN, 0x0002);
        assert_eq!(OP_SERVER_LIST_REQUEST, 0x0004);
        assert_eq!(OP_PLAY_EVERQUEST_REQ, 0x000d);
        // S→C response opcodes — SoD shifts each up from the Titanium value.
        assert_eq!(OP_CHAT_MESSAGE, 0x0017);          // Titanium 0x0016
        assert_eq!(OP_LOGIN_ACCEPTED, 0x0018);        // Titanium 0x0017
        assert_eq!(OP_SERVER_LIST_RESPONSE, 0x0019);  // Titanium 0x0018
        assert_eq!(OP_PLAY_EVERQUEST_RESP, 0x0022);   // Titanium 0x0021
        assert_eq!(OP_LOGIN_EXPANSION_PACKET_DATA, 0x0031); // SoD-only
    }

    /// OP_Login: 10-byte LoginBaseMessage (seq=3, encrypt_type=2 DES) + DES(zero-key) of
    /// "user\0pass\0" zero-padded to an 8-byte boundary. Verified by DES round-trip.
    #[test]
    fn login_request_header_and_credential_roundtrip() {
        let pkt = build_login_request("bob", "secret");
        // LoginBaseMessage: sequence=3, compressed=0, encrypt_type=2, unk3=0.
        assert_eq!(&pkt[..10], &[3u8, 0, 0, 0, 0, 2, 0, 0, 0, 0]);
        let enc = &pkt[10..];
        // "bob\0secret\0" = 11 bytes → padded to 16; server rejects non-multiples of 8.
        assert_eq!(enc.len(), 16);
        assert_eq!(enc.len() % 8, 0);
        let dec = des_decrypt(enc);
        assert_eq!(&dec[..11], b"bob\0secret\0");
    }

    /// OP_PlayEverquestRequest: 10-byte LoginBaseMessage (seq=5) + u32 world server id.
    #[test]
    fn play_everquest_request_layout() {
        let pkt = build_play_everquest(0x1234_5678);
        assert_eq!(pkt.len(), 14);
        assert_eq!(&pkt[..10], &[5u8, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(u32::from_le_bytes(pkt[10..14].try_into().unwrap()), 0x1234_5678);
    }

    // Build a wire-accurate OP_LoginAccepted payload: 10-byte LoginBaseMessage + DES(PlayerLoginReply).
    fn make_login_accepted(success: u8, lsid: i32, key: &[u8]) -> Vec<u8> {
        let mut plain = vec![0u8; 32]; // multiple of 8; big enough for lsid@8 + key@12
        plain[0] = success;                                  // base_reply.success
        plain[1..5].copy_from_slice(&101i32.to_le_bytes());  // base_reply.error_str_id
        // str[1]@5, unk1@6, unk2@7 stay 0
        plain[8..12].copy_from_slice(&lsid.to_le_bytes());   // lsid
        plain[12..12 + key.len()].copy_from_slice(key);      // key[11], NUL-terminated by zero fill
        let enc = des_encrypt(&plain);
        let mut payload = vec![0u8; 10];                     // LoginBaseMessage header
        payload.extend_from_slice(&enc);
        payload
    }

    #[test]
    fn login_accepted_parses_lsid_and_session_key() {
        let payload = make_login_accepted(1, 4242, b"ABCDE12345");
        let (lsid, key) = parse_login_accepted_payload(&payload).expect("valid reply accepted");
        assert_eq!(lsid, 4242);
        assert_eq!(key, "ABCDE12345");
    }

    #[test]
    fn login_accepted_rejects_on_success_zero() {
        // success byte == 0 is an explicit credential rejection; must NOT be reported as success
        // (agent-honesty: never turn a real failure into a confident false OK).
        let payload = make_login_accepted(0, 0, b"");
        assert!(parse_login_accepted_payload(&payload).is_none());
    }

    // Build a wire-accurate OP_ServerListResponse per world_server_manager.cpp CreateServerListPacket.
    fn make_server_list(ip: &[u8], server_type: i32, server_id: u32, name: &[u8], count: i32) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&4i32.to_le_bytes()); // LoginBaseMessage.sequence
        p.push(0);                                 // compressed
        p.push(0);                                 // encrypt_type
        p.extend_from_slice(&0i32.to_le_bytes());  // unk3  → 10-byte header
        p.push(1);                                 // base_reply.success @10
        p.extend_from_slice(&0x65i32.to_le_bytes());// base_reply.error_str_id @11
        p.push(0);                                 // empty str NUL @15
        p.extend_from_slice(&count.to_le_bytes()); // server_count @16
        // first entry
        p.extend_from_slice(ip);
        p.push(0);
        p.extend_from_slice(&server_type.to_le_bytes());
        p.extend_from_slice(&server_id.to_le_bytes());
        p.extend_from_slice(name);
        p.push(0);
        p.extend_from_slice(b"us\0");
        p.extend_from_slice(b"en\0");
        p.extend_from_slice(&0i32.to_le_bytes()); // status
        p.extend_from_slice(&3u32.to_le_bytes()); // players_online
        p
    }

    #[test]
    fn server_list_parses_first_entry() {
        let p = make_server_list(b"192.168.1.5", 1, 7, b"MyServer", 1);
        let (id, host, name) = parse_server_list_payload(&p, "fallback");
        assert_eq!(id, 7);
        assert_eq!(host, "192.168.1.5");
        assert_eq!(name, "MyServer");
    }

    #[test]
    fn server_list_uses_fallback_host_for_zero_ip() {
        let p = make_server_list(b"0.0.0.0", 1, 9, b"S", 1);
        let (id, host, _name) = parse_server_list_payload(&p, "myfallback");
        assert_eq!(id, 9);
        assert_eq!(host, "myfallback");
    }
}
