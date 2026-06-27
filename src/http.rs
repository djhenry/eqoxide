//! The agent-facing HTTP/REST API (axum, port 8765).
//!
//! Endpoints: camera control + `/frame` capture, navigation (`/goto`, `/warp`), `/entities`,
//! NPC/combat actions (`/hail`, `/say`, `/target`, `/target/name`, `/attack`, `/buy`, `/give`),
//! inventory (`/inventory/move`), zone crossing (`/zone_cross`, `/zone_points`), and `/debug`.
//! Most handlers just write a shared
//! `Arc<Mutex<…>>` request slot (the `*Req` type aliases below) that the navigation thread drains
//! each tick; reads come from snapshots the render/network threads publish. See `docs/http-api.md`.

use axum::{
    Router,
    body::Body,
    extract::{State, Query},
    http::{header, StatusCode},
    routing::{get, post},
    response::Response,
    Json,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use crate::camera_state::{CameraCmd, CameraSnapshot};

/// A pending frame capture: the render loop drains this, captures a PNG,
/// and sends the bytes back through the channel.
pub type FrameReq = Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>>;

/// Target position for the navigation system. Set by /goto, cleared on arrival.
pub type GotoTarget = Arc<Mutex<Option<(f32, f32, f32)>>>;

/// Live entity name → (x, y, z) map, updated by login.rs as packets arrive.
pub type EntityPositions = Arc<Mutex<HashMap<String, (f32, f32, f32)>>>;

/// Live entity name → spawn_id map (same keys as EntityPositions).
pub type EntityIds = Arc<Mutex<HashMap<String, u32>>>;

/// Zone exit points received in OP_SEND_ZONE_POINTS, exposed via GET /zone_points.
pub type ZonePoints = Arc<Mutex<Vec<crate::game_state::ZonePoint>>>;
/// Native Task-system quest log, published from GameState.tasks each tick (for GET /quests/log).
pub type TaskLog = Arc<Mutex<Vec<crate::game_state::ActiveTask>>>;

/// Zone-crossing request set by POST /zone_cross; gameplay thread reads it once,
/// warps to the matching zone line and sends OP_ZONE_CHANGE.
///   Some(0)  → cross the nearest zone line (any destination).
///   Some(id) → cross to a specific destination zone id.
pub type ZoneCrossReq = Arc<Mutex<Option<u16>>>;

/// Direct warp target set by POST /warp; the App reads it once and teleports
/// the player to the exact coordinates, bypassing collision.
pub type WarpReq = Arc<Mutex<Option<(f32, f32, f32)>>>;

/// NPC name to hail, set by POST /hail; the nav thread reads it once and sends a
/// "Hail, <name>" say packet so the NPC fires its hail/quest script.
pub type HailReq = Arc<Mutex<Option<String>>>;

/// Arbitrary Say-channel text, set by POST /say or a HUD button/keyword; the nav thread
/// reads it once and sends it on the Say channel (used for quest keyword follow-ups).
pub type SayReq = Arc<Mutex<Option<String>>>;

/// Spawn id to target, set by POST /target or the HUD "Target nearest" button; the nav
/// thread reads it once, sends OP_TargetCommand + OP_Consider.
pub type TargetReq = Arc<Mutex<Option<u32>>>;

/// Auto-attack toggle — set to true by POST /attack, false by DELETE /attack.
/// Nav thread reads it and sends OP_AUTO_ATTACK(1) or OP_AUTO_ATTACK(0).
pub type AttackReq = Arc<Mutex<Option<bool>>>;

/// Buy request — (merchant spawn id, merchant inventory slot), set by POST /buy.
/// Nav thread reads it and sends OP_ShopRequest (open) + OP_ShopPlayerBuy (buy that slot).
pub type BuyReq = Arc<Mutex<Option<(u32, u32)>>>;

/// Sell request — (merchant spawn id, player inventory slot, quantity), set by POST /trade/sell.
/// Nav thread reads it and sends OP_ShopRequest (open) + OP_ShopPlayerSell (sell that slot).
pub type SellReq = Arc<Mutex<Option<(u32, u32, u32)>>>;

/// Open/close a merchant window. `Open(merchant_id)` from POST /trade/open; `Close` from
/// POST /trade/close. The nav thread sends OP_ShopRequest (command 1/0).
#[derive(Clone, Copy)]
pub enum TradeCmd { Open(u32), Close }
pub type TradeReq = Arc<Mutex<Option<TradeCmd>>>;

/// Camp command, written by POST /exit, POST /camp, the HUD Camp button, and the `/camp` chat
/// keyword. The gameplay loop drains it: `Start` begins a camp if one isn't running (idempotent —
/// used by /exit so a double request doesn't cancel); `Toggle` starts a camp or cancels the one in
/// progress (used by the button / chat command). A completed camp shuts the client down cleanly
/// (no linkdead) once the server's ~29s camp timer has elapsed. See `gameplay::camp_apply`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CampCmd { Start, Toggle }
pub type CampReq = Arc<Mutex<Option<CampCmd>>>;

/// Published camp state: `Some(deadline)` while a camp is in progress (the instant the client will
/// disconnect), `None` otherwise. Set by the gameplay loop; read by the HUD for the countdown and
/// by handlers to know whether a camp is already running.
pub type CampUntil = Arc<Mutex<Option<std::time::Instant>>>;

/// Live merchant-session snapshot published each nav tick, read by GET /trade/list and used for
/// the HUD merchant window. `open` mirrors `GameState::merchant_open`.
#[derive(Default, Clone, serde::Serialize)]
pub struct MerchantSnapshot {
    pub open: bool,
    pub merchant_id: Option<u32>,
    pub items: Vec<crate::game_state::MerchantItem>,
}
pub type MerchantShared = Arc<Mutex<MerchantSnapshot>>;

/// Move-item request — (from_slot, to_slot), set by POST /inventory/move.
/// Nav thread reads it and sends OP_MoveItem (MoveItem_Struct, number_in_stack=1).
/// Used to equip/unequip/rearrange items (e.g. boots in bag slot 23 -> worn slot 19).
pub type MoveReq = Arc<Mutex<Option<(u32, u32)>>>;

/// Give request — (npc_spawn_id, item_from_slot), set by POST /give.
/// Nav thread runs the trade-window turn-in: puts the item on the cursor, sends OP_TradeRequest,
/// waits for OP_TradeRequestAck, then moves the item into the NPC trade slot + OP_TradeAcceptClick.
pub type GiveReq = Arc<Mutex<Option<(u32, u32)>>>;

/// Live snapshot of the player's inventory + equipment, published each tick by the nav thread
/// and read by GET /inventory. Slots are Titanium **wire** ids (the same numbers /give and
/// /inventory/move take — note these are one less than the EQEmu DB `inventory.slot_id` for
/// general slots: DB 23-30 → wire 22-29).
pub type InventoryShared = Arc<Mutex<Vec<crate::game_state::InvItem>>>;

/// Loot request — a corpse spawn id, set by POST /loot. The nav thread reads it once and pushes
/// the corpse onto the existing auto-loot queue (OP_LootRequest → OP_LootItem echoes → OP_EndLootRequest).
pub type LootReq = Arc<Mutex<Option<u32>>>;

/// One machine-readable line from the in-game message log (GET /messages). `kind` is the channel
/// ("npc" = NPC dialogue/emotes, "chat", "combat", "system", "exp", "loot", "trade", "zone", …);
/// `keywords` are the `[bracketed]` quest reply words extracted from the text (say them back via
/// POST /say to advance dialogue quests).
#[derive(Clone, serde::Serialize)]
pub struct MessageEntry {
    pub kind:     String,
    pub text:     String,
    pub keywords: Vec<String>,
}

/// Live snapshot of the in-game message log, published each tick by the nav thread and read by
/// GET /messages. Exposes NPC dialogue (kind "npc") as machine-readable text + keywords.
pub type MessagesShared = Arc<Mutex<Vec<MessageEntry>>>;

/// One inter-agent chat event exposed by GET /events (tell/ooc/shout/group/gmsay). `id` is a
/// monotonic cursor; `directed` = addressed specifically to us (a /tell to our name, or a GM
/// message). Agents poll `/events?since=<id>` (optionally long-poll with `wait=`) to be notified.
#[derive(Clone, serde::Serialize)]
pub struct ChatEvent {
    pub id:       u64,
    pub from:     String,
    pub channel:  String,
    pub directed: bool,
    pub text:     String,
}

/// Live snapshot of inter-agent chat events, published each tick by the nav thread, read by
/// GET /events. Ordered by ascending `id`.
pub type ChatEventsShared = Arc<Mutex<Vec<ChatEvent>>>;

/// One queued outgoing chat message, set by POST /tell|/ooc|/shout|/group and drained by the nav
/// thread, which builds + sends the `OP_ChannelMessage`. `to` is the recipient for /tell (chan 7),
/// empty for broadcasts. `chan` is the EQ ChatChannel number.
#[derive(Clone)]
pub struct ChatSend {
    pub chan: u32,
    pub to:   String,
    pub text: String,
}

/// Outgoing chat queue (FIFO), written by the /tell|/ooc|/shout|/group endpoints.
pub type ChatSendShared = Arc<Mutex<Vec<ChatSend>>>;

#[derive(Clone, Copy)]
pub struct CastRequest { pub gem: u8, pub target_id: Option<u32> }
/// Cast a memorized gem (0-8) on an explicit target, else current target, else self.
pub type CastReq = Arc<Mutex<Option<CastRequest>>>;
/// Scribe/memorize request — (slot, spell_id, scribing): scribing 0 = scribe a scroll into the
/// spellbook at book `slot`; 1 = memorize a known spell into gem `slot` (0-8). Set by POST
/// /scribe and POST /memorize; the nav thread sends OP_MemorizeSpell.
pub type MemSpellReq = Arc<Mutex<Option<(u32, u32, u32)>>>;
/// Posture: Some(true)=sit, Some(false)=stand.
pub type SitReq = Arc<Mutex<Option<bool>>>;
/// Standalone consider of a spawn id.
pub type ConsiderReq = Arc<Mutex<Option<u32>>>;

/// Door-click request — a door_id, set by POST /doors/click or a human click in the 3D
/// view. The nav thread reads it once and sends OP_ClickDoor. The door's visual state
/// changes only when the server replies with OP_MoveDoor (server-authoritative).
pub type DoorClickReq = Arc<Mutex<Option<u8>>>;

#[derive(Clone, serde::Serialize)]
pub struct DoorView {
    pub door_id:  u8,
    pub name:     String,
    pub x:        f32,
    pub y:        f32,
    pub z:        f32,
    pub heading:  f32,
    pub opentype: u8,
    pub is_open:  bool,
}
/// Snapshot of the current zone's doors, published each nav tick for GET /doors.
pub type DoorsShared = Arc<Mutex<Vec<DoorView>>>;

/// Current zone name and id, updated on every OP_NEW_ZONE.
#[allow(dead_code)]
pub type ZoneInfo = Arc<Mutex<(String, u16)>>;

/// Live player state for the /debug endpoint.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PlayerState {
    pub zone:         String,
    pub race:         String, // 3-letter race code, e.g. "ELF" (Wood Elf)
    pub class:        String, // class name, e.g. "Cleric"
    pub level:        u32,
    pub pos_east:     f32,
    pub pos_north:    f32,
    pub pos_up:       f32,
    pub heading_ccw:  f32, // 0=north CCW
    pub heading_cw:   f32, // 0=north CW (wire format)
    pub server_corrections: u32,
    pub mem_spells:   [u32; 9],
    pub target_id:    Option<u32>,
    /// Coin on hand: [platinum, gold, silver, copper], from the player profile.
    pub coin:         [u32; 4],
}
pub type PlayerInfo = Arc<Mutex<PlayerState>>;

/// Render coin `[platinum, gold, silver, copper]` as a JSON object for the API.
fn currency_json(coin: [u32; 4]) -> serde_json::Value {
    serde_json::json!({
        "platinum": coin[0],
        "gold":     coin[1],
        "silver":   coin[2],
        "copper":   coin[3],
    })
}

#[derive(Clone)]
struct HttpState {
    cmd_tx:           Arc<Mutex<Option<CameraCmd>>>,
    snapshot:         Arc<Mutex<CameraSnapshot>>,
    frame_req:        FrameReq,
    goto_target:      GotoTarget,
    entity_positions: EntityPositions,
    entity_ids:       EntityIds,
    zone_points:      ZonePoints,
    zone_cross:       ZoneCrossReq,
    warp:             WarpReq,
    hail:             HailReq,
    say:              SayReq,
    target:           TargetReq,
    attack:           AttackReq,
    cast:             CastReq,
    mem_spell:        MemSpellReq,
    sit:              SitReq,
    consider:         ConsiderReq,
    buy:              BuyReq,
    sell:             SellReq,
    trade:            TradeReq,
    merchant:         MerchantShared,
    move_req:         MoveReq,
    give:             GiveReq,
    inventory:        InventoryShared,
    loot:             LootReq,
    messages:         MessagesShared,
    chat_events:      ChatEventsShared,
    chat_send:        ChatSendShared,
    spells:           std::sync::Arc<crate::spells::SpellDb>,
    player_info:      PlayerInfo,
    task_log:         TaskLog,
    door_click:       DoorClickReq,
    doors_shared:     DoorsShared,
    camp:             CampReq,
    camp_until:       CampUntil,
}

#[derive(serde::Deserialize)]
struct CameraSetBody {
    azimuth:   Option<f32>,
    elevation: Option<f32>,
    radius:    Option<f32>,
    focus:     Option<[f32; 3]>,
}

#[derive(serde::Deserialize)]
struct GotoBody {
    name:     Option<String>,
    /// Map coordinates (map_x = server_y, map_y = server_x). Use these when
    /// navigating from a Qeynos map. Alternatively supply raw server x/y/z.
    map_x:    Option<f32>,
    map_y:    Option<f32>,
    /// Raw server coordinates (bypass map conversion)
    x:        Option<f32>,
    y:        Option<f32>,
    z:        Option<f32>,
}

pub fn spawn_camera_server(
    cmd_tx:           Arc<Mutex<Option<CameraCmd>>>,
    snapshot:         Arc<Mutex<CameraSnapshot>>,
    frame_req:        FrameReq,
    goto_target:      GotoTarget,
    entity_positions: EntityPositions,
    entity_ids:       EntityIds,
    zone_points:      ZonePoints,
    zone_cross:       ZoneCrossReq,
    warp:             WarpReq,
    hail:             HailReq,
    say:              SayReq,
    target:           TargetReq,
    attack:           AttackReq,
    cast:             CastReq,
    mem_spell:        MemSpellReq,
    sit:              SitReq,
    consider:         ConsiderReq,
    buy:              BuyReq,
    sell:             SellReq,
    trade:            TradeReq,
    merchant:         MerchantShared,
    move_req:         MoveReq,
    give:             GiveReq,
    inventory:        InventoryShared,
    loot:             LootReq,
    messages:         MessagesShared,
    chat_events:      ChatEventsShared,
    chat_send:        ChatSendShared,
    spells:           std::sync::Arc<crate::spells::SpellDb>,
    player_info:      PlayerInfo,
    task_log:         TaskLog,
    door_click:       DoorClickReq,
    doors_shared:     DoorsShared,
    camp:             CampReq,
    camp_until:       CampUntil,
    port:             u16,
    // When `Some`, an already-bound listener from `--api-port` (exact port, no scan).
    // When `None`, scan upward from `port` for the first free port.
    exact_listener:   Option<std::net::TcpListener>,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("http tokio runtime");
        rt.block_on(async move {
            let state = HttpState { cmd_tx, snapshot, frame_req, goto_target, entity_positions, entity_ids, zone_points, zone_cross, warp, hail, say, target, attack, cast, mem_spell, sit, consider, buy, sell, trade, merchant, move_req, give, inventory, loot, messages, chat_events, chat_send, spells, player_info, task_log, door_click, doors_shared, camp, camp_until };
            let app = Router::new()
                .route("/camera", get(get_camera).post(post_camera))
                .route("/camera/reset", post(post_camera_reset))
                .route("/frame", get(get_frame))
                .route("/goto", post(post_goto))
                .route("/entities", get(get_entities))
                .route("/quests", get(get_quests))
                .route("/quests/log", get(get_quest_log))
                .route("/zone_points", get(get_zone_points))
                .route("/zone_cross", post(post_zone_cross))
                .route("/warp", post(post_warp))
                .route("/hail", post(post_hail))
                .route("/say", post(post_say))
                .route("/target", post(post_target))
                .route("/target/name", post(post_target_name))
                .route("/attack", post(post_attack_on).delete(post_attack_off))
                .route("/cast", post(post_cast))
                .route("/scribe", post(post_scribe))
                .route("/memorize", post(post_memorize))
                .route("/spells", get(get_spells))
                .route("/sit", post(post_sit))
                .route("/stand", post(post_stand))
                .route("/consider", post(post_consider))
                // Vendor/trade API. /buy and /sell remain as back-compat aliases.
                .route("/trade/buy", post(post_buy))
                .route("/trade/sell", post(post_sell))
                .route("/trade/open", post(post_trade_open))
                .route("/trade/close", post(post_trade_close))
                .route("/trade/list", get(get_trade_list))
                .route("/buy", post(post_buy))
                .route("/sell", post(post_sell))
                .route("/inventory/move", post(post_move))
                .route("/give", post(post_give))
                .route("/inventory", get(get_inventory))
                .route("/loot", post(post_loot))
                .route("/messages", get(get_messages))
                .route("/events", get(get_events))
                .route("/tell", post(post_tell))
                .route("/ooc", post(post_ooc))
                .route("/shout", post(post_shout))
                .route("/group", post(post_group))
                .route("/doors", get(get_doors))
                .route("/doors/click", post(post_door_click))
                .route("/exit", post(post_exit))
                .route("/camp", post(post_camp))
                .route("/debug", get(get_debug))
                .with_state(state);
            let (listener, bound_port) = if let Some(std_l) = exact_listener {
                // --api-port: use the listener main already bound to the exact requested port.
                std_l.set_nonblocking(true).expect("set api-port listener non-blocking");
                let l = tokio::net::TcpListener::from_std(std_l).expect("adopt api-port listener");
                let p = l.local_addr().map(|a| a.port()).unwrap_or(port);
                (l, p)
            } else {
                // Scan upward from the configured base port so multiple client instances
                // (e.g. one per worktree) each grab the next free port instead of colliding.
                const MAX_TRIES: u16 = 50;
                let mut bound = None;
                for p in port..port.saturating_add(MAX_TRIES) {
                    if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", p)).await {
                        bound = Some((l, p));
                        break;
                    }
                }
                match bound {
                    Some(found) => found,
                    None => {
                        tracing::info!(
                            "camera HTTP: no free port in {}..{} — camera API disabled",
                            port,
                            port.saturating_add(MAX_TRIES)
                        );
                        return;
                    }
                }
            };
            // Machine-parseable line on stdout so a launching agent can discover the port.
            // Flush explicitly: the render loop may never return, leaving stdout buffered.
            use std::io::Write;
            tracing::info!("API_PORT={bound_port}");
            let _ = std::io::stdout().flush();
            tracing::info!("camera HTTP: http://127.0.0.1:{bound_port}");
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("camera HTTP: server error: {e}");
            }
        });
    });
}

async fn get_camera(State(s): State<HttpState>) -> Json<CameraSnapshot> {
    Json(s.snapshot.lock().unwrap().clone())
}

async fn post_camera(
    State(s): State<HttpState>,
    body: Result<Json<CameraSetBody>, axum::extract::rejection::JsonRejection>,
) -> StatusCode {
    match body {
        Ok(Json(b)) => {
            *s.cmd_tx.lock().unwrap() = Some(CameraCmd::Set {
                azimuth:   b.azimuth,
                elevation: b.elevation,
                radius:    b.radius,
                focus:     b.focus,
            });
            StatusCode::OK
        }
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

async fn post_camera_reset(State(s): State<HttpState>) -> StatusCode {
    *s.cmd_tx.lock().unwrap() = Some(CameraCmd::Reset);
    StatusCode::OK
}

/// POST /goto  {"name":"Lanhern Firepride"}  or  {"x":1.0,"y":2.0,"z":3.0}
async fn post_goto(
    State(s): State<HttpState>,
    body: Result<Json<GotoBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON".into()),
    };

    let target = if let Some(name) = &b.name {
        let positions = s.entity_positions.lock().unwrap();
        let nl = name.to_lowercase();
        // Exact key match first, then clean-name match, then substring fallback.
        let matched = positions.get(name.as_str()).map(|&p| p)
            .or_else(|| positions.iter()
                .find(|(k, _)| clean_entity_name(k).to_lowercase() == nl)
                .map(|(_, &p)| p))
            .or_else(|| positions.iter()
                .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl)
                    || k.to_lowercase().contains(&nl))
                .map(|(_, &p)| p));
        match matched {
            Some(pos) => pos,
            None => {
                let known: Vec<_> = positions.keys()
                    .filter(|k| k.to_lowercase().contains(&nl))
                    .take(5)
                    .cloned()
                    .collect();
                if known.is_empty() {
                    return (StatusCode::NOT_FOUND, format!("No entity named {:?}", name));
                }
                match positions.get(&known[0]).copied() {
                    Some(pos) => {
                        tracing::info!("goto: fuzzy match {:?} → {:?}", name, known[0]);
                        pos
                    }
                    None => return (StatusCode::NOT_FOUND, format!("No entity named {:?}", name)),
                }
            }
        }
    } else if let (Some(mx), Some(my)) = (b.map_x, b.map_y) {
        // Map coords match the eqmaps/Brewall .txt values, which are the negated
        // server position: map_x = -server_x, map_y = -server_y.
        let server_x = -mx;
        let server_y = -my;
        let server_z = b.z.unwrap_or(3.75);
        tracing::info!("goto: map ({:.1},{:.1}) → server ({:.1},{:.1})", mx, my, server_x, server_y);
        (server_x, server_y, server_z)
    } else if let (Some(x), Some(y), Some(z)) = (b.x, b.y, b.z) {
        (x, y, z)
    } else {
        return (StatusCode::BAD_REQUEST, "provide 'name', 'map_x'+'map_y', or 'x'+'y'+'z'".into());
    };

    *s.goto_target.lock().unwrap() = Some(target);
    tracing::info!("goto: target set to ({:.1},{:.1},{:.1})", target.0, target.1, target.2);
    (StatusCode::OK, format!("navigating to ({:.1},{:.1},{:.1})", target.0, target.1, target.2))
}

/// GET /entities — returns {name: [x,y,z], ...} for all known entities
async fn get_entities(State(s): State<HttpState>) -> Json<HashMap<String, [f32; 3]>> {
    let positions = s.entity_positions.lock().unwrap();
    let out: HashMap<_, _> = positions.iter()
        .map(|(k, &(x, y, z))| (k.clone(), [x, y, z]))
        .collect();
    Json(out)
}

/// GET /quests — the agent's "quests near me" view for the current zone. Lists quest givers
/// (data/quests.json) with their location, distance from the player, whether they're currently
/// loaded (in spawn range), what they want (turn-in items), and reward XP. NPCs here are the ones
/// shown with a golden "!" in the HUD. Use it like an MMO quest tracker; combine with /entities +
/// /goto to walk to a giver and (once implemented) /give to hand in. See docs/autonomous-play.md.
async fn get_quests(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let player = s.player_info.lock().unwrap().clone();
    let zone = player.zone.clone();
    let (px, py) = (player.pos_east, player.pos_north);
    // clean live names -> position, to flag loaded givers + use their live coords
    let live: HashMap<String, (f32, f32, f32)> = s.entity_positions.lock().unwrap().iter()
        .map(|(k, v)| (clean_entity_name(k), *v))
        .collect();
    let mut givers: Vec<serde_json::Value> = crate::quests::givers_in(&zone).into_iter()
        .map(|(name, g)| {
            let live_pos = live.get(&name).copied();
            let pos = live_pos.map(|(x, y, z)| [x, y, z]).unwrap_or([g.x, g.y, g.z]);
            let dist = ((pos[0] - px).powi(2) + (pos[1] - py).powi(2)).sqrt();
            serde_json::json!({
                "name": name,
                "npc_id": g.npc_id,
                "pos": pos,
                "loaded": live_pos.is_some(),
                "distance": dist.round(),
                "turn_in": g.turn_in,
                "wanted": g.wanted,
                "reward_xp": g.reward_xp,
                "hail": g.hail,
            })
        })
        .collect();
    givers.sort_by(|a, b| {
        let (da, db) = (a["distance"].as_f64().unwrap_or(1e9), b["distance"].as_f64().unwrap_or(1e9));
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });
    Json(serde_json::json!({ "zone": zone, "player": [px, py], "count": givers.len(), "quest_givers": givers }))
}

/// GET /quests/log — the player's NATIVE quest journal (EQ Task system), pushed by the server via
/// OP_TaskDescription/OP_TaskActivity. Each task has a title, description, reward, and objectives
/// with live progress (done_count/goal_count). Distinct from GET /quests (old-style Lua turn-in
/// quests derived from the server scripts) — together they cover both kinds of EQ quests.
async fn get_quest_log(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let tasks = s.task_log.lock().unwrap().clone();
    Json(serde_json::json!({ "active_count": tasks.len(), "tasks": tasks }))
}

/// GET /zone_points — returns all zone exit points received from the server.
async fn get_zone_points(State(s): State<HttpState>) -> Json<Vec<crate::game_state::ZonePoint>> {
    Json(s.zone_points.lock().unwrap().clone())
}

#[derive(serde::Deserialize, Default)]
struct ZoneCrossBody {
    /// Destination zone id to cross to. Omit (or 0) to take the nearest zone line.
    zone_id: Option<u16>,
}

/// POST /zone_cross — warp to a zone line and send OP_ZONE_CHANGE.
/// Body: {"zone_id": 1} to cross to a specific zone, or {} for the nearest line.
async fn post_zone_cross(
    State(s): State<HttpState>,
    body: Option<Json<ZoneCrossBody>>,
) -> (StatusCode, String) {
    let zone_id = body.and_then(|Json(b)| b.zone_id).unwrap_or(0);
    *s.zone_cross.lock().unwrap() = Some(zone_id);
    tracing::info!("zone_cross: flagged for OP_ZONE_CHANGE (target zone_id={zone_id})");
    (StatusCode::OK, format!("zone_cross request queued (zone_id={zone_id})"))
}

#[derive(serde::Deserialize)]
struct WarpBody {
    x: f32,
    y: f32,
    z: f32,
}

/// POST /warp — teleport directly to coordinates, bypassing collision.
async fn post_warp(
    State(s): State<HttpState>,
    Json(body): Json<WarpBody>,
) -> (StatusCode, String) {
    *s.warp.lock().unwrap() = Some((body.x, body.y, body.z));
    tracing::info!("warp: queued to ({:.1}, {:.1}, {:.1})", body.x, body.y, body.z);
    (StatusCode::OK, format!("warp queued to ({:.1}, {:.1}, {:.1})", body.x, body.y, body.z))
}

#[derive(serde::Deserialize)]
struct HailBody {
    /// NPC to hail (fuzzy-matched against /entities). Omit to hail the nearest NPC.
    name: Option<String>,
}

/// Turn an entity key like "Guard_Phaeton000" into a display name "Guard Phaeton".
pub fn clean_entity_name(raw: &str) -> String {
    raw.trim_end_matches(|c: char| c.is_ascii_digit())
        .replace('_', " ")
        .trim()
        .to_string()
}

/// POST /hail — say "Hail, <name>" so a nearby NPC fires its hail/quest script.
/// Body: {"name":"Guard Phaeton"} (fuzzy) or {} to hail the nearest NPC.
/// The NPC must be within ~200 units (server-enforced say range).
async fn post_hail(
    State(s): State<HttpState>,
    body: Option<Json<HailBody>>,
) -> (StatusCode, String) {
    let requested = body.and_then(|Json(b)| b.name);
    let positions = s.entity_positions.lock().unwrap();

    let resolved: Option<String> = if let Some(name) = &requested {
        // Exact (clean) match first, then fuzzy substring.
        let nl = name.to_lowercase();
        positions.keys()
            .find(|k| clean_entity_name(k).to_lowercase() == nl)
            .or_else(|| positions.keys().find(|k| k.to_lowercase().contains(&nl)))
            .cloned()
    } else {
        // Nearest NPC to the player. Camera focus = [east, north, height] =
        // [server_x, server_y, server_z]; entities stored as (server_x, server_y, z).
        let focus = s.snapshot.lock().unwrap().focus;
        positions.iter()
            .filter(|(k, _)| !k.contains("zone_controller"))
            .map(|(k, &(ex, ny, _))| {
                let de = ex - focus[0];
                let dn = ny - focus[1];
                (k.clone(), de * de + dn * dn)
            })
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(k, _)| k)
    };

    match resolved {
        Some(key) => {
            let display_name = clean_entity_name(&key);
            *s.hail.lock().unwrap() = Some(display_name.clone());
            tracing::info!("hail: queued hail to {:?}", display_name);
            (StatusCode::OK, format!("hailing {}", display_name))
        }
        None => {
            let msg = match &requested {
                Some(n) => format!("No NPC matching {:?}", n),
                None => "No NPCs known to hail".to_string(),
            };
            (StatusCode::NOT_FOUND, msg)
        }
    }
}

#[derive(serde::Deserialize)]
struct SayBody {
    text: String,
}

/// POST /say {"text":"..."} — say arbitrary text on the Say channel. Used for quest
/// keyword follow-ups (e.g. say "shipment" after an NPC mentions [shipment]).
async fn post_say(
    State(s): State<HttpState>,
    body: Result<Json<SayBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let text = match body {
        Ok(Json(b)) => b.text,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"text\":\"...\"}".into()),
    };
    if text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "empty text".into());
    }
    *s.say.lock().unwrap() = Some(text.clone());
    tracing::info!("say: queued {:?}", text);
    (StatusCode::OK, format!("saying {}", text))
}

#[derive(serde::Deserialize)]
struct TargetBody {
    id: u32,
}

/// POST /target {"id":<spawn_id>} — target the spawn and auto-consider it. The con
/// result comes back asynchronously as an OP_Consider reply (→ message log).
async fn post_target(
    State(s): State<HttpState>,
    body: Result<Json<TargetBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let id = match body {
        Ok(Json(b)) => b.id,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"id\":<spawn_id>}".into()),
    };
    *s.target.lock().unwrap() = Some(id);
    tracing::info!("target: queued spawn_id={}", id);
    (StatusCode::OK, format!("targeting spawn {}", id))
}

#[derive(serde::Deserialize)]
struct TargetNameBody {
    name: String,
}

/// POST /target/name {"name":"a rat"} — target a mob by (fuzzy) name. The nav thread
/// resolves the name to a spawn_id via gs.entities and sends OP_TargetCommand.
async fn post_target_name(
    State(s): State<HttpState>,
    body: Result<Json<TargetNameBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let name = match body {
        Ok(Json(b)) => b.name,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"name\":\"...\"}".into()),
    };
    let ids = s.entity_ids.lock().unwrap();
    let nl = name.to_lowercase();
    let found = ids.iter()
        .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl))
        .map(|(k, &id)| (k.clone(), id));
    match found {
        Some((key, id)) => {
            *s.target.lock().unwrap() = Some(id);
            tracing::info!("target_name: {:?} → spawn_id={}", key, id);
            (StatusCode::OK, format!("targeting {} (spawn_id={})", clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no entity matching {:?}", name)),
    }
}

/// POST /attack — enable auto-attack (sends OP_AUTO_ATTACK 1).
async fn post_attack_on(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.attack.lock().unwrap() = Some(true);
    tracing::info!("attack: queued auto-attack ON");
    (StatusCode::OK, "auto-attack ON".into())
}

/// DELETE /attack — disable auto-attack (sends OP_AUTO_ATTACK 0).
async fn post_attack_off(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.attack.lock().unwrap() = Some(false);
    tracing::info!("attack: queued auto-attack OFF");
    (StatusCode::OK, "auto-attack OFF".into())
}

#[derive(serde::Deserialize)]
struct CastBody { gem: Option<u8>, spell_id: Option<u32>, target_id: Option<u32> }

/// POST /cast {"gem":0-8} | {"spell_id":N,"target_id":M?}
async fn post_cast(State(s): State<HttpState>, body: Option<Json<CastBody>>) -> (StatusCode, String) {
    let b = body.map(|Json(b)| b).unwrap_or(CastBody { gem: None, spell_id: None, target_id: None });
    let mem = s.player_info.lock().unwrap().mem_spells;
    let gem = if let Some(g) = b.gem {
        g
    } else if let Some(sid) = b.spell_id {
        match mem.iter().position(|&x| x == sid) {
            Some(i) => i as u8,
            None => return (StatusCode::BAD_REQUEST, format!("spell {sid} is not memorized")),
        }
    } else {
        return (StatusCode::BAD_REQUEST, "provide {\"gem\":0-8} or {\"spell_id\":N}".into());
    };
    if gem > 8 { return (StatusCode::BAD_REQUEST, "gem must be 0-8".into()); }
    *s.cast.lock().unwrap() = Some(CastRequest { gem, target_id: b.target_id });
    (StatusCode::OK, format!("cast queued (gem {gem})"))
}

#[derive(serde::Deserialize)]
struct ScribeBody { spell_id: u32, slot: Option<u32> }

/// POST /scribe {"spell_id":N,"slot":B?} — scribe a spell scroll (in inventory) into the spellbook
/// at book slot B (default 0). Sends OP_MemorizeSpell with scribing=0. The server validates you
/// hold the scroll and consumes it.
async fn post_scribe(
    State(s): State<HttpState>,
    body: Result<Json<ScribeBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body { Ok(Json(b)) => b, Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"spell_id\":N,\"slot\":B?}".into()) };
    let slot = b.slot.unwrap_or(0);
    *s.mem_spell.lock().unwrap() = Some((slot, b.spell_id, 0));
    (StatusCode::OK, format!("scribing spell {} into book slot {}", b.spell_id, slot))
}

#[derive(serde::Deserialize)]
struct MemorizeBody { spell_id: u32, gem: u32 }

/// POST /memorize {"spell_id":N,"gem":0-8} — memorize a known (scribed) spell into a gem.
/// Sends OP_MemorizeSpell with scribing=1.
async fn post_memorize(
    State(s): State<HttpState>,
    body: Result<Json<MemorizeBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body { Ok(Json(b)) => b, Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"spell_id\":N,\"gem\":0-8}".into()) };
    if b.gem > 8 { return (StatusCode::BAD_REQUEST, "gem must be 0-8".into()); }
    *s.mem_spell.lock().unwrap() = Some((b.gem, b.spell_id, 1));
    (StatusCode::OK, format!("memorizing spell {} into gem {}", b.spell_id, b.gem))
}

/// GET /spells — the 9 memorized gems with names. Empty gem = spell id 0 or 0xFFFFFFFF.
async fn get_spells(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let mem = s.player_info.lock().unwrap().mem_spells;
    let gems: Vec<_> = mem.iter().enumerate().map(|(i, &id)| {
        if id == 0 || id == 0xFFFF_FFFF {
            serde_json::json!({ "gem": i, "spell_id": null, "name": null })
        } else {
            let name = s.spells.get(id).map(|x| x.name.clone());
            serde_json::json!({ "gem": i, "spell_id": id, "name": name })
        }
    }).collect();
    Json(serde_json::json!({ "gems": gems }))
}

async fn post_sit(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.sit.lock().unwrap() = Some(true);
    (StatusCode::OK, "sit queued".into())
}
async fn post_stand(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.sit.lock().unwrap() = Some(false);
    (StatusCode::OK, "stand queued".into())
}

#[derive(serde::Deserialize)]
struct ConsiderBody { id: Option<u32> }
async fn post_consider(State(s): State<HttpState>, body: Option<Json<ConsiderBody>>) -> (StatusCode, String) {
    let id = body.and_then(|Json(b)| b.id).or(s.player_info.lock().unwrap().target_id);
    match id {
        Some(id) => { *s.consider.lock().unwrap() = Some(id); (StatusCode::OK, format!("consider {id} queued")) }
        None => (StatusCode::BAD_REQUEST, "no target; provide {\"id\":N}".into()),
    }
}

#[derive(serde::Deserialize)]
struct BuyBody {
    /// Merchant NPC name (fuzzy-matched, like /target/name).
    merchant: String,
    /// Merchant inventory slot of the item to buy (from the merchantlist).
    slot: u32,
}

/// POST /buy {"merchant":"<name>","slot":N} — open the named merchant and buy item slot N.
/// Must be within ~200u of the merchant. The nav thread sends OP_ShopRequest then
/// OP_ShopPlayerBuy.
async fn post_buy(
    State(s): State<HttpState>,
    body: Result<Json<BuyBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"merchant\":\"...\",\"slot\":N}".into()),
    };
    let ids = s.entity_ids.lock().unwrap();
    let nl = b.merchant.to_lowercase();
    let found = ids.iter()
        .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl))
        .map(|(k, &id)| (k.clone(), id));
    match found {
        Some((key, id)) => {
            *s.buy.lock().unwrap() = Some((id, b.slot));
            tracing::info!("buy: queued merchant {:?} (spawn_id={}) slot={}", key, id, b.slot);
            (StatusCode::OK, format!("buying slot {} from {} (spawn_id={})", b.slot, clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no merchant matching {:?}", b.merchant)),
    }
}

#[derive(serde::Deserialize)]
struct SellBody {
    /// Merchant NPC name (fuzzy-matched, like /buy).
    merchant: String,
    /// Player inventory slot of the item to sell (Titanium: 22-29 general, 251+ bag contents).
    slot: u32,
    /// Number to sell from the slot (stack count). Defaults to 1.
    quantity: Option<u32>,
}

/// POST /sell {"merchant":"<name>","slot":N,"quantity":Q} — open the named merchant and sell the
/// item in player inventory slot N (quantity Q, default 1). Must be within ~200u of the merchant.
/// The nav thread sends OP_ShopRequest then OP_ShopPlayerSell (price computed server-side).
async fn post_sell(
    State(s): State<HttpState>,
    body: Result<Json<SellBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"merchant\":\"...\",\"slot\":N,\"quantity\":Q}".into()),
    };
    let qty = b.quantity.unwrap_or(1).max(1);
    let ids = s.entity_ids.lock().unwrap();
    let nl = b.merchant.to_lowercase();
    let found = ids.iter()
        .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl))
        .map(|(k, &id)| (k.clone(), id));
    match found {
        Some((key, id)) => {
            *s.sell.lock().unwrap() = Some((id, b.slot, qty));
            tracing::info!("sell: queued merchant {:?} (spawn_id={}) slot={} qty={}", key, id, b.slot, qty);
            (StatusCode::OK, format!("selling slot {} x{} to {} (spawn_id={})", b.slot, qty, clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no merchant matching {:?}", b.merchant)),
    }
}

#[derive(serde::Deserialize)]
struct TradeOpenBody {
    /// Merchant NPC name (fuzzy-matched, like /trade/buy).
    merchant: String,
}

/// POST /trade/open {"merchant":"<name>"} — open the named merchant's window (OP_ShopRequest).
/// Must be within ~200u. The server replies Open (window opens, items arrive) or Close (refused,
/// e.g. KOS faction); watch GET /trade/list `open` to see the result.
async fn post_trade_open(
    State(s): State<HttpState>,
    body: Result<Json<TradeOpenBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"merchant\":\"...\"}".into()),
    };
    let ids = s.entity_ids.lock().unwrap();
    let nl = b.merchant.to_lowercase();
    let found = ids.iter()
        .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl))
        .map(|(k, &id)| (k.clone(), id));
    match found {
        Some((key, id)) => {
            *s.trade.lock().unwrap() = Some(TradeCmd::Open(id));
            tracing::info!("trade: queued open merchant {:?} (spawn_id={})", key, id);
            (StatusCode::OK, format!("opening merchant {} (spawn_id={})", clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no merchant matching {:?}", b.merchant)),
    }
}

/// POST /trade/close — close the currently open merchant window (OP_ShopRequest command=Close).
async fn post_trade_close(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.trade.lock().unwrap() = Some(TradeCmd::Close);
    (StatusCode::OK, "closing merchant window".into())
}

/// GET /trade/list — the open merchant's offered items (for buying). Returns `{open, merchant_id,
/// count, items:[{merchant_slot,item_id,name,icon,price,quantity}]}`. `open:false` means no
/// merchant window is open (it was never opened, was closed, or the merchant refused, e.g. KOS).
async fn get_trade_list(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let m = s.merchant.lock().unwrap();
    Json(serde_json::json!({
        "open": m.open,
        "merchant_id": m.merchant_id,
        "count": m.items.len(),
        "items": m.items,
    }))
}

#[derive(serde::Deserialize)]
struct MoveBody {
    /// Source slot id (e.g. a general/bag slot like 23, or a worn slot to unequip).
    from: u32,
    /// Destination slot id (e.g. worn slot 19=Feet, 17=Chest; 30=cursor; 22-29 general).
    to: u32,
}

/// POST /inventory/move {"from":N,"to":M} — move/equip/unequip an item between inventory slots.
/// Nav thread sends OP_MoveItem (MoveItem_Struct, number_in_stack=1). Titanium slot ids:
/// 0-21 worn, 22-29 general inventory, 30 cursor, 251+ bag contents.
async fn post_move(
    State(s): State<HttpState>,
    body: Result<Json<MoveBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"from\":N,\"to\":M}".into()),
    };
    *s.move_req.lock().unwrap() = Some((b.from, b.to));
    tracing::info!("move: queued from_slot={} to_slot={}", b.from, b.to);
    (StatusCode::OK, format!("moving item from slot {} to slot {}", b.from, b.to))
}

#[derive(serde::Deserialize)]
struct GiveBody {
    /// NPC name to hand the item to (fuzzy-matched, like /buy and /target/name).
    npc: String,
    /// Inventory slot holding the item to give (e.g. 23 for a general/bag slot, or 30 if it's
    /// already on the cursor).
    from: u32,
}

/// POST /give {"npc":"<name>","from":N} — hand inventory item in slot N to the named NPC and
/// complete an EQ quest turn-in (trade-window flow). Must be within trade range of the NPC.
/// The nav thread runs a multi-tick state machine: it puts the item on the cursor + sends
/// OP_TradeRequest, waits for the server's OP_TradeRequestAck, then moves the item into the NPC
/// trade slot + sends OP_TradeAcceptClick. The server replies OP_FinishTrade on completion; if no
/// ack arrives within ~3s the give is aborted.
async fn post_give(
    State(s): State<HttpState>,
    body: Result<Json<GiveBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"npc\":\"...\",\"from\":N}".into()),
    };
    let ids = s.entity_ids.lock().unwrap();
    let nl = b.npc.to_lowercase();
    let found = ids.iter()
        .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl))
        .map(|(k, &id)| (k.clone(), id));
    match found {
        Some((key, id)) => {
            *s.give.lock().unwrap() = Some((id, b.from));
            tracing::info!("give: queued npc {:?} (spawn_id={}) from_slot={}", key, id, b.from);
            (StatusCode::OK, format!("giving slot {} to {} (spawn_id={})", b.from, clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no NPC matching {:?}", b.npc)),
    }
}

/// GET /inventory — the player's current inventory + equipment, published each tick by the nav
/// thread. Each item carries its Titanium **wire** slot (the number to pass to /give and
/// /inventory/move — note general slots are one less than the EQEmu DB `inventory.slot_id`: DB
/// 23-30 → wire 22-29), plus item_id, name, charges, icon, and idfile. Use this to discover which
/// slot holds an item before giving/equipping it.
async fn get_inventory(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let items = s.inventory.lock().unwrap().clone();
    let coin  = s.player_info.lock().unwrap().coin;
    Json(serde_json::json!({
        "count": items.len(),
        "items": items,
        "currency": currency_json(coin),
    }))
}

#[derive(serde::Deserialize)]
struct MessagesQuery {
    /// Filter to a single message channel, e.g. ?kind=npc for NPC dialogue only.
    kind: Option<String>,
}

/// GET /messages — the in-game message log as machine-readable text (oldest→newest, last ~50
/// lines), published each tick by the nav thread. This is how an agent reads **NPC dialogue**:
/// each line has a `kind` ("npc" = NPC say/emote, plus "chat", "combat", "system", "exp", "loot",
/// "trade", "zone"), the `text`, and any `[bracketed]` quest `keywords` to say back via POST /say.
/// Filter with `?kind=npc` for dialogue only. Replaces having to OCR the /frame HUD panel.
async fn get_messages(
    State(s): State<HttpState>,
    Query(q): Query<MessagesQuery>,
) -> Json<serde_json::Value> {
    let all = s.messages.lock().unwrap();
    let filtered: Vec<&MessageEntry> = match &q.kind {
        Some(k) => all.iter().filter(|m| m.kind == *k).collect(),
        None    => all.iter().collect(),
    };
    Json(serde_json::json!({ "count": filtered.len(), "messages": filtered }))
}

#[derive(serde::Deserialize, Default)]
struct EventsQuery {
    /// Return only events with id greater than this cursor (default 0 = all).
    since:    Option<u64>,
    /// Long-poll: block up to this many seconds (capped at 30) for a new event before returning.
    wait:     Option<u64>,
    /// 1 = only messages addressed specifically to you (a /tell to your name, or a GM message).
    directed: Option<u8>,
}

/// GET /events — the inter-agent chat feed (tells/ooc/shout/group/gmsay) as structured events.
///
/// This is how an agent becomes aware of a whisper meant for it. Pass `?since=<last_id>` to get
/// only new events; use the response's `last_id` as your next cursor. `?wait=<secs>` long-polls —
/// the request blocks (up to ~30s) until a new event arrives, so an agent can "listen" without
/// busy-polling (run it in a loop). `?directed=1` returns only messages addressed specifically to
/// you. Each event: `{id, from, channel, directed, text}`.
async fn get_events(
    State(s): State<HttpState>,
    Query(q): Query<EventsQuery>,
) -> Json<serde_json::Value> {
    let since         = q.since.unwrap_or(0);
    let directed_only = q.directed.unwrap_or(0) != 0;
    let wait          = q.wait.unwrap_or(0).min(30);
    let deadline      = std::time::Instant::now() + std::time::Duration::from_secs(wait);
    loop {
        let (events, last_id) = {
            let all = s.chat_events.lock().unwrap();
            let last_id = all.last().map(|e| e.id).unwrap_or(since).max(since);
            let evs: Vec<ChatEvent> = all.iter()
                .filter(|e| e.id > since && (!directed_only || e.directed))
                .cloned().collect();
            (evs, last_id)
        };
        if !events.is_empty() || std::time::Instant::now() >= deadline {
            return Json(serde_json::json!({
                "count": events.len(), "last_id": last_id, "events": events,
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

#[derive(serde::Deserialize)]
struct TellBody { to: String, text: String }

/// POST /tell {"to","text"} — send a directed whisper to one character (EQ /tell, chan 7).
/// The recipient's client receives it as a `directed` event on GET /events.
async fn post_tell(State(s): State<HttpState>, Json(b): Json<TellBody>) -> (StatusCode, String) {
    if b.to.trim().is_empty() || b.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "tell requires non-empty 'to' and 'text'".into());
    }
    s.chat_send.lock().unwrap().push(ChatSend { chan: 7, to: b.to.clone(), text: b.text });
    (StatusCode::OK, format!("tell queued to {}", b.to))
}

#[derive(serde::Deserialize)]
struct TextBody { text: String }

/// POST /ooc {"text"} — zone-wide out-of-character broadcast (chan 5).
async fn post_ooc(State(s): State<HttpState>, Json(b): Json<TextBody>) -> (StatusCode, String) {
    if b.text.trim().is_empty() { return (StatusCode::BAD_REQUEST, "ooc requires 'text'".into()); }
    s.chat_send.lock().unwrap().push(ChatSend { chan: 5, to: String::new(), text: b.text });
    (StatusCode::OK, "ooc queued".into())
}

/// POST /shout {"text"} — zone-wide shout (chan 3).
async fn post_shout(State(s): State<HttpState>, Json(b): Json<TextBody>) -> (StatusCode, String) {
    if b.text.trim().is_empty() { return (StatusCode::BAD_REQUEST, "shout requires 'text'".into()); }
    s.chat_send.lock().unwrap().push(ChatSend { chan: 3, to: String::new(), text: b.text });
    (StatusCode::OK, "shout queued".into())
}

/// POST /group {"text"} — group-channel message (chan 2; only seen by your group).
async fn post_group(State(s): State<HttpState>, Json(b): Json<TextBody>) -> (StatusCode, String) {
    if b.text.trim().is_empty() { return (StatusCode::BAD_REQUEST, "group requires 'text'".into()); }
    s.chat_send.lock().unwrap().push(ChatSend { chan: 2, to: String::new(), text: b.text });
    (StatusCode::OK, "group queued".into())
}

#[derive(serde::Deserialize, Default)]
struct LootBody {
    /// Corpse spawn id to loot directly.
    id:   Option<u32>,
    /// Corpse name to fuzzy-match (corpses are named like "a_rat000's corpse").
    name: Option<String>,
}

/// POST /loot — open a corpse and take all its items, reusing the auto-loot machinery
/// (OP_LootRequest → echo each OP_LootItem → OP_EndLootRequest). Must be near the corpse; looted
/// items land in inventory (see GET /inventory). Body: {"id":N} for a specific corpse spawn id,
/// {"name":"..."} to fuzzy-match a corpse name, or {} for the nearest corpse.
async fn post_loot(
    State(s): State<HttpState>,
    body: Option<Json<LootBody>>,
) -> (StatusCode, String) {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    // Resolve to a corpse spawn id: explicit id > fuzzy name > nearest corpse.
    let resolved: Option<(String, u32)> = if let Some(id) = b.id {
        let name = s.entity_ids.lock().unwrap().iter()
            .find(|(_, &v)| v == id).map(|(k, _)| k.clone())
            .unwrap_or_else(|| format!("spawn {}", id));
        Some((name, id))
    } else if let Some(name) = &b.name {
        let ids = s.entity_ids.lock().unwrap();
        let nl = name.to_lowercase();
        ids.iter()
            .find(|(k, _)| k.to_lowercase().contains(&nl) || clean_entity_name(k).to_lowercase().contains(&nl))
            .map(|(k, &id)| (k.clone(), id))
    } else {
        // Nearest corpse to the player (camera focus = player pos).
        let focus = s.snapshot.lock().unwrap().focus;
        let positions = s.entity_positions.lock().unwrap();
        let ids = s.entity_ids.lock().unwrap();
        positions.iter()
            .filter(|(k, _)| k.to_lowercase().contains("corpse"))
            .map(|(k, &(x, y, _))| {
                let (dx, dy) = (x - focus[0], y - focus[1]);
                (k.clone(), dx * dx + dy * dy)
            })
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .and_then(|(k, _)| ids.get(&k).map(|&id| (k, id)))
    };
    match resolved {
        Some((name, id)) => {
            *s.loot.lock().unwrap() = Some(id);
            tracing::info!("loot: queued corpse {:?} (spawn_id={})", name, id);
            (StatusCode::OK, format!("looting {} (spawn_id={})", clean_entity_name(&name), id))
        }
        None => (StatusCode::NOT_FOUND, "no corpse found to loot".into()),
    }
}

/// POST /exit — camp out, then cleanly shut down. Requests a camp (`CampCmd::Start`, idempotent):
/// the gameplay loop sends OP_Camp, stays connected ~30s for EQEmu's camp timer to set `instalog`,
/// then sets the shutdown flag so the disconnect leaves NO linkdead ghost (instant re-login). The
/// render loop's `about_to_wait` then exits the winit event loop on the MAIN thread and the process
/// exits via `main`.
///
/// The watchdog is a last resort if the gameplay/render loop is wedged. It must outlast the camp
/// (CAMP_DURATION ≈ 30s) so it never force-kills mid-camp (which WOULD linkdead); 45s gives margin.
async fn post_exit(State(s): State<HttpState>) -> (StatusCode, &'static str) {
    tracing::info!("exit: camp-and-shutdown requested via POST /exit");
    *s.camp.lock().unwrap() = Some(CampCmd::Start);
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(45)).await;
        tracing::warn!("exit: watchdog timeout — loop unresponsive, forcing process exit");
        std::process::exit(0);
    });
    (StatusCode::OK, "camping out, then shutting down (~30s)")
}

/// POST /camp — toggle a camp. Starts a camp if none is running, or cancels the one in progress
/// (same as the HUD Camp button and the `/camp` chat keyword). A completed camp shuts the client
/// down cleanly with no linkdead; a cancel keeps the client in-world.
async fn post_camp(State(s): State<HttpState>) -> (StatusCode, &'static str) {
    let camping = s.camp_until.lock().unwrap().is_some();
    *s.camp.lock().unwrap() = Some(CampCmd::Toggle);
    if camping {
        tracing::info!("camp: cancel requested via POST /camp");
        (StatusCode::OK, "cancelling camp")
    } else {
        tracing::info!("camp: start requested via POST /camp");
        (StatusCode::OK, "camping out (~30s), then shutting down")
    }
}

/// GET /doors — list the current zone's doors (id, name, position, opentype, open state).
async fn get_doors(State(s): State<HttpState>) -> Json<Vec<DoorView>> {
    Json(s.doors_shared.lock().unwrap().clone())
}

#[derive(serde::Deserialize)]
struct DoorClickBody { door_id: Option<u8>, name: Option<String> }

/// POST /doors/click {"door_id": N}  or  {"name": "DOOR1"} (exact case-insensitive name match).
async fn post_door_click(
    State(s): State<HttpState>,
    body: axum::extract::Json<DoorClickBody>,
) -> (StatusCode, String) {
    let id = if let Some(id) = body.door_id {
        Some(id)
    } else if let Some(name) = &body.name {
        let up = name.to_uppercase();
        s.doors_shared.lock().unwrap().iter()
            .find(|d| d.name.to_uppercase() == up)
            .map(|d| d.door_id)
    } else {
        None
    };
    match id {
        Some(id) => {
            *s.door_click.lock().unwrap() = Some(id);
            (StatusCode::OK, format!("clicking door {}", id))
        }
        None => (StatusCode::BAD_REQUEST,
                 "provide {\"door_id\":N} or {\"name\":\"...\"}".into()),
    }
}

async fn get_debug(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let cam   = s.snapshot.lock().unwrap().clone();
    let player = s.player_info.lock().unwrap().clone();
    Json(serde_json::json!({
        "player": {
            "zone":       player.zone,
            "race":       player.race,
            "class":      player.class,
            "level":      player.level,
            "pos":        [player.pos_east, player.pos_north, player.pos_up],
            "heading_ccw": player.heading_ccw,
            "heading_cw":  player.heading_cw,
            "server_corrections": player.server_corrections,
            "currency":    currency_json(player.coin),
        },
        "camera": {
            "azimuth_deg":   cam.azimuth.to_degrees(),
            "elevation_deg": cam.elevation.to_degrees(),
            "radius":        cam.radius,
            "focus":         cam.focus,
            "mode":          cam.mode,
        },
    }))
}

/// GET /frame — returns the current rendered frame as a PNG.
async fn get_frame(State(s): State<HttpState>) -> Response {
    let (tx, rx) = oneshot::channel::<Vec<u8>>();
    *s.frame_req.lock().unwrap() = Some(tx);

    match tokio::time::timeout(std::time::Duration::from_secs(2), rx).await {
        Ok(Ok(png_bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/png")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(png_bytes))
            .unwrap(),
        _ => Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("renderer not ready"))
            .unwrap(),
    }
}

#[cfg(test)]
mod currency_tests {
    use super::currency_json;

    #[test]
    fn currency_json_maps_coin_slots_to_named_fields() {
        let v = currency_json([12, 3, 45, 6]);
        assert_eq!(v["platinum"], 12);
        assert_eq!(v["gold"], 3);
        assert_eq!(v["silver"], 45);
        assert_eq!(v["copper"], 6);
    }

    #[test]
    fn currency_json_all_zero() {
        let v = currency_json([0, 0, 0, 0]);
        assert_eq!(v["platinum"], 0);
        assert_eq!(v["copper"], 0);
    }
}
