//! The agent-facing HTTP/REST API (axum, port 8765).
//!
//! Endpoints: camera control + `/frame` capture, navigation (`/goto`, `/warp`), `/entities`,
//! NPC/combat actions (`/hail`, `/say`, `/target`, `/target/name`, `/attack`, `/buy`), zone
//! crossing (`/zone_cross`, `/zone_points`), and `/debug`. Most handlers just write a shared
//! `Arc<Mutex<…>>` request slot (the `*Req` type aliases below) that the navigation thread drains
//! each tick; reads come from snapshots the render/network threads publish. See `docs/http-api.md`.

use axum::{
    Router,
    body::Body,
    extract::State,
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
    buy:              BuyReq,
    player_info:      PlayerInfo,
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
    buy:              BuyReq,
    player_info:      PlayerInfo,
    port:             u16,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("http tokio runtime");
        rt.block_on(async move {
            let state = HttpState { cmd_tx, snapshot, frame_req, goto_target, entity_positions, entity_ids, zone_points, zone_cross, warp, hail, say, target, attack, buy, player_info };
            let app = Router::new()
                .route("/camera", get(get_camera).post(post_camera))
                .route("/camera/reset", post(post_camera_reset))
                .route("/frame", get(get_frame))
                .route("/goto", post(post_goto))
                .route("/entities", get(get_entities))
                .route("/quests", get(get_quests))
                .route("/zone_points", get(get_zone_points))
                .route("/zone_cross", post(post_zone_cross))
                .route("/warp", post(post_warp))
                .route("/hail", post(post_hail))
                .route("/say", post(post_say))
                .route("/target", post(post_target))
                .route("/target/name", post(post_target_name))
                .route("/attack", post(post_attack_on).delete(post_attack_off))
                .route("/buy", post(post_buy))
                .route("/debug", get(get_debug))
                .with_state(state);
            let addr = format!("127.0.0.1:{port}");
            let listener = match tokio::net::TcpListener::bind(&addr).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("camera HTTP: failed to bind {addr}: {e} — camera API disabled");
                    return;
                }
            };
            eprintln!("camera HTTP: http://{addr}");
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
