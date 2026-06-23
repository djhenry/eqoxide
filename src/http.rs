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

#[derive(Clone, Copy)]
pub struct CastRequest { pub gem: u8, pub target_id: Option<u32> }
/// Cast a memorized gem (0-8) on an explicit target, else current target, else self.
pub type CastReq = Arc<Mutex<Option<CastRequest>>>;
/// Posture: Some(true)=sit, Some(false)=stand.
pub type SitReq = Arc<Mutex<Option<bool>>>;
/// Standalone consider of a spawn id.
pub type ConsiderReq = Arc<Mutex<Option<u32>>>;

/// Current zone name and id, updated on every OP_NEW_ZONE.
#[allow(dead_code)]
pub type ZoneInfo = Arc<Mutex<(String, u16)>>;

/// Live player state for the /debug endpoint.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PlayerState {
    pub zone:         String,
    pub pos_east:     f32,
    pub pos_north:    f32,
    pub pos_up:       f32,
    pub heading_ccw:  f32, // 0=north CCW
    pub heading_cw:   f32, // 0=north CW (wire format)
    pub server_corrections: u32,
    pub mem_spells:   [u32; 9],
    pub target_id:    Option<u32>,
}
pub type PlayerInfo = Arc<Mutex<PlayerState>>;

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
    sit:              SitReq,
    consider:         ConsiderReq,
    buy:              BuyReq,
    move_req:         MoveReq,
    give:             GiveReq,
    inventory:        InventoryShared,
    loot:             LootReq,
    messages:         MessagesShared,
    spells:           std::sync::Arc<crate::spells::SpellDb>,
    player_info:      PlayerInfo,
    task_log:         TaskLog,
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
    sit:              SitReq,
    consider:         ConsiderReq,
    buy:              BuyReq,
    move_req:         MoveReq,
    give:             GiveReq,
    inventory:        InventoryShared,
    loot:             LootReq,
    messages:         MessagesShared,
    spells:           std::sync::Arc<crate::spells::SpellDb>,
    player_info:      PlayerInfo,
    task_log:         TaskLog,
    port:             u16,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("http tokio runtime");
        rt.block_on(async move {
            let state = HttpState { cmd_tx, snapshot, frame_req, goto_target, entity_positions, entity_ids, zone_points, zone_cross, warp, hail, say, target, attack, cast, sit, consider, buy, move_req, give, inventory, loot, messages, spells, player_info, task_log };
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
                .route("/spells", get(get_spells))
                .route("/sit", post(post_sit))
                .route("/stand", post(post_stand))
                .route("/consider", post(post_consider))
                .route("/buy", post(post_buy))
                .route("/inventory/move", post(post_move))
                .route("/give", post(post_give))
                .route("/inventory", get(get_inventory))
                .route("/loot", post(post_loot))
                .route("/messages", get(get_messages))
                .route("/exit", post(post_exit))
                .route("/debug", get(get_debug))
                .with_state(state);
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
            let (listener, bound_port) = match bound {
                Some(found) => found,
                None => {
                    eprintln!(
                        "camera HTTP: no free port in {}..{} — camera API disabled",
                        port,
                        port.saturating_add(MAX_TRIES)
                    );
                    return;
                }
            };
            // Machine-parseable line on stdout so a launching agent can discover the port.
            // Flush explicitly: the render loop may never return, leaving stdout buffered.
            use std::io::Write;
            println!("API_PORT={bound_port}");
            let _ = std::io::stdout().flush();
            eprintln!("camera HTTP: http://127.0.0.1:{bound_port}");
            if let Err(e) = axum::serve(listener, app).await {
                eprintln!("camera HTTP: server error: {e}");
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
                        eprintln!("goto: fuzzy match {:?} → {:?}", name, known[0]);
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
        eprintln!("goto: map ({:.1},{:.1}) → server ({:.1},{:.1})", mx, my, server_x, server_y);
        (server_x, server_y, server_z)
    } else if let (Some(x), Some(y), Some(z)) = (b.x, b.y, b.z) {
        (x, y, z)
    } else {
        return (StatusCode::BAD_REQUEST, "provide 'name', 'map_x'+'map_y', or 'x'+'y'+'z'".into());
    };

    *s.goto_target.lock().unwrap() = Some(target);
    eprintln!("goto: target set to ({:.1},{:.1},{:.1})", target.0, target.1, target.2);
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
    eprintln!("zone_cross: flagged for OP_ZONE_CHANGE (target zone_id={zone_id})");
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
    eprintln!("warp: queued to ({:.1}, {:.1}, {:.1})", body.x, body.y, body.z);
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
            let display = clean_entity_name(&key);
            *s.hail.lock().unwrap() = Some(display.clone());
            eprintln!("hail: queued hail to {:?}", display);
            (StatusCode::OK, format!("hailing {}", display))
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
    eprintln!("say: queued {:?}", text);
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
    eprintln!("target: queued spawn_id={}", id);
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
            eprintln!("target_name: {:?} → spawn_id={}", key, id);
            (StatusCode::OK, format!("targeting {} (spawn_id={})", clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no entity matching {:?}", name)),
    }
}

/// POST /attack — enable auto-attack (sends OP_AUTO_ATTACK 1).
async fn post_attack_on(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.attack.lock().unwrap() = Some(true);
    eprintln!("attack: queued auto-attack ON");
    (StatusCode::OK, "auto-attack ON".into())
}

/// DELETE /attack — disable auto-attack (sends OP_AUTO_ATTACK 0).
async fn post_attack_off(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.attack.lock().unwrap() = Some(false);
    eprintln!("attack: queued auto-attack OFF");
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
            eprintln!("buy: queued merchant {:?} (spawn_id={}) slot={}", key, id, b.slot);
            (StatusCode::OK, format!("buying slot {} from {} (spawn_id={})", b.slot, clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no merchant matching {:?}", b.merchant)),
    }
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
    eprintln!("move: queued from_slot={} to_slot={}", b.from, b.to);
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
            eprintln!("give: queued npc {:?} (spawn_id={}) from_slot={}", key, id, b.from);
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
    Json(serde_json::json!({ "count": items.len(), "items": items }))
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
            eprintln!("loot: queued corpse {:?} (spawn_id={})", name, id);
            (StatusCode::OK, format!("looting {} (spawn_id={})", clean_entity_name(&name), id))
        }
        None => (StatusCode::NOT_FOUND, "no corpse found to loot".into()),
    }
}

/// POST /exit — cleanly shut down THIS client instance. Lets an agent restart its own
/// client (to pick up changes) without `pkill`, which could kill another worktree's instance.
/// Responds 200 first, then a detached task exits the process after a short delay so the
/// HTTP response flushes. Uses the same `std::process::exit(0)` as the normal shutdown path.
async fn post_exit() -> (StatusCode, &'static str) {
    eprintln!("exit: shutdown requested via POST /exit");
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        std::process::exit(0);
    });
    (StatusCode::OK, "shutting down")
}

async fn get_debug(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let cam   = s.snapshot.lock().unwrap().clone();
    let player = s.player_info.lock().unwrap().clone();
    Json(serde_json::json!({
        "player": {
            "zone":       player.zone,
            "pos":        [player.pos_east, player.pos_north, player.pos_up],
            "heading_ccw": player.heading_ccw,
            "heading_cw":  player.heading_cw,
            "server_corrections": player.server_corrections,
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
