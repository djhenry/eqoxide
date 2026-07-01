//! The agent-facing HTTP/REST API (axum). Routes are versioned + grouped: `/v1/<group>/<action>`,
//! where `<group>` mirrors the MCP tool grouping — `observe`, `navigate`, `combat`, `interact`,
//! `merchant`, `inventory`, `chat`, `camera`, `lifecycle`. The `/v1` prefix lets a future breaking
//! revision ship as `/v2` while old clients keep working.
//!
//! Each group lives in its own submodule (e.g. `combat.rs`) exposing a `router()` of relative
//! paths; `spawn_camera_server` nests them under `/v1/<group>`. This module holds the cross-cutting
//! pieces: the shared `Arc<Mutex<…>>` request/snapshot types, `HttpState`, and the server task.
//! Most handlers just write a shared request slot (the `*Req` aliases) that the navigation thread
//! drains each tick; reads come from snapshots the render/network threads publish. See
//! `docs/http-api.md`.

use axum::Router;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use crate::camera_state::{CameraCmd, CameraSnapshot};

mod observe;
mod navigate;
mod combat;
mod interact;
mod merchant;
mod inventory;
mod chat;
mod events;
mod camera;
mod lifecycle;

/// A pending frame capture: the render loop drains this, captures a PNG,
/// and sends the bytes back through the channel.
pub type FrameReq = Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>>;

/// Target position for the navigation system. Set by /goto, cleared on arrival.
pub type GotoTarget = Arc<Mutex<Option<(f32, f32, f32)>>>;

/// Authoritative controller snapshot published by the render thread each frame and read by the nav
/// thread to stream OP_ClientUpdate (design §2). Single source of position truth.
pub type ControllerShared = Arc<Mutex<crate::movement::ControllerView>>;

/// The `/goto` planner's per-frame movement intent. The nav planner writes `Some` while walking a
/// path and `None` when idle/arrived; the render controller consumes it when no WASD key is held.
pub type NavIntent = Arc<Mutex<Option<crate::movement::MoveIntent>>>;

/// A large (>12u) server position correction the nav thread hands to the render controller to apply
/// (teleport). Small deltas are ignored — the controller is authoritative (design §3.4).
pub type PosCorrection = Arc<Mutex<Option<[f32; 3]>>>;

/// Live entity name → (x, y, z) map, updated by login.rs as packets arrive.
pub type EntityPositions = Arc<Mutex<HashMap<String, (f32, f32, f32)>>>;

/// Live entity name → spawn_id map (same keys as EntityPositions).
pub type EntityIds = Arc<Mutex<HashMap<String, u32>>>;

/// Zone exit points received in OP_SEND_ZONE_POINTS, exposed via GET /v1/observe/zone_points.
pub type ZonePoints = Arc<Mutex<Vec<crate::game_state::ZonePoint>>>;
/// Native Task-system quest log, published from GameState.tasks each tick (GET /v1/observe/quests/log).
pub type TaskLog = Arc<Mutex<Vec<crate::game_state::ActiveTask>>>;

/// Zone-crossing request set by POST /v1/navigate/zone_cross; gameplay thread reads it once,
/// warps to the matching zone line and sends OP_ZONE_CHANGE.
///   Some(0)  → cross the nearest zone line (any destination).
///   Some(id) → cross to a specific destination zone id.
pub type ZoneCrossReq = Arc<Mutex<Option<u16>>>;

/// Direct warp target set by POST /v1/navigate/warp; the App reads it once and teleports
/// the player to the exact coordinates, bypassing collision.
pub type WarpReq = Arc<Mutex<Option<(f32, f32, f32)>>>;

/// NPC name to hail, set by POST /v1/interact/hail; the nav thread reads it once and sends a
/// "Hail, <name>" say packet so the NPC fires its hail/quest script.
pub type HailReq = Arc<Mutex<Option<String>>>;

/// Arbitrary Say-channel text, set by POST /v1/interact/say or a HUD button/keyword; the nav thread
/// reads it once and sends it on the Say channel (used for quest keyword follow-ups).
pub type SayReq = Arc<Mutex<Option<String>>>;

/// Spawn id to target, set by POST /v1/combat/target or the HUD "Target nearest" button; the nav
/// thread reads it once, sends OP_TargetCommand + OP_Consider.
pub type TargetReq = Arc<Mutex<Option<u32>>>;

/// Auto-attack toggle — set to true by POST /v1/combat/attack, false by DELETE /v1/combat/attack.
/// Nav thread reads it and sends OP_AUTO_ATTACK(1) or OP_AUTO_ATTACK(0).
pub type AttackReq = Arc<Mutex<Option<bool>>>;

/// Buy request — (merchant spawn id, merchant inventory slot), set by POST /v1/merchant/buy.
/// Nav thread reads it and sends OP_ShopRequest (open) + OP_ShopPlayerBuy (buy that slot).
pub type BuyReq = Arc<Mutex<Option<(u32, u32)>>>;

/// Sell request — (merchant spawn id, player inventory slot, quantity), set by POST /v1/merchant/sell.
/// Nav thread reads it and sends OP_ShopRequest (open) + OP_ShopPlayerSell (sell that slot).
pub type SellReq = Arc<Mutex<Option<(u32, u32, u32)>>>;

/// Open/close a merchant window. `Open(merchant_id)` from POST /v1/merchant/open; `Close` from
/// POST /v1/merchant/close. The nav thread sends OP_ShopRequest (command 1/0).
#[derive(Clone, Copy)]
pub enum TradeCmd { Open(u32), Close }
pub type TradeReq = Arc<Mutex<Option<TradeCmd>>>;

/// Camp command, written by POST /v1/lifecycle/exit, POST /v1/lifecycle/camp, the HUD Camp button,
/// and the `/camp` chat keyword. The gameplay loop drains it: `Start` begins a camp if one isn't
/// running (idempotent — used by /exit so a double request doesn't cancel); `Toggle` starts a camp
/// or cancels the one in progress (used by the button / chat command). A completed camp shuts the
/// client down cleanly (no linkdead) once the server's ~29s camp timer has elapsed. See
/// `gameplay::camp_apply`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CampCmd { Start, Toggle }
pub type CampReq = Arc<Mutex<Option<CampCmd>>>;

/// Published camp state: `Some(deadline)` while a camp is in progress (the instant the client will
/// disconnect), `None` otherwise. Set by the gameplay loop; read by the HUD for the countdown and
/// by handlers to know whether a camp is already running.
pub type CampUntil = Arc<Mutex<Option<std::time::Instant>>>;

/// Live merchant-session snapshot published each nav tick, read by GET /v1/merchant/list and used
/// for the HUD merchant window. `open` mirrors `GameState::merchant_open`.
#[derive(Default, Clone, serde::Serialize)]
pub struct MerchantSnapshot {
    pub open: bool,
    pub merchant_id: Option<u32>,
    pub items: Vec<crate::game_state::MerchantItem>,
}
pub type MerchantShared = Arc<Mutex<MerchantSnapshot>>;

/// Move-item request — (from_slot, to_slot), set by POST /v1/inventory/move.
/// Nav thread reads it and sends OP_MoveItem (MoveItem_Struct, number_in_stack=1).
/// Used to equip/unequip/rearrange items (e.g. boots in bag slot 23 -> worn slot 19).
pub type MoveReq = Arc<Mutex<Option<(u32, u32)>>>;

/// Give request — (npc_spawn_id, item_from_slot), set by POST /v1/interact/give.
/// Nav thread runs the trade-window turn-in: puts the item on the cursor, sends OP_TradeRequest,
/// waits for OP_TradeRequestAck, then moves the item into the NPC trade slot + OP_TradeAcceptClick.
pub type GiveReq = Arc<Mutex<Option<(u32, u32)>>>;

/// Live snapshot of the player's inventory + equipment, published each tick by the nav thread
/// and read by GET /v1/observe/inventory. Slots are Titanium **wire** ids (the same numbers /give
/// and /inventory/move take — note these are one less than the EQEmu DB `inventory.slot_id` for
/// general slots: DB 23-30 → wire 22-29).
pub type InventoryShared = Arc<Mutex<Vec<crate::game_state::InvItem>>>;

/// Loot request — a corpse spawn id, set by POST /v1/interact/loot. The nav thread reads it once and
/// pushes the corpse onto the auto-loot queue (OP_LootRequest → OP_LootItem echoes → OP_EndLootRequest).
pub type LootReq = Arc<Mutex<Option<u32>>>;

/// One machine-readable line from the in-game message log (GET /v1/observe/messages). `kind` is the
/// channel ("npc" = NPC dialogue/emotes, "chat", "combat", "system", "exp", "loot", "trade",
/// "zone", …); `keywords` are the `[bracketed]` quest reply words extracted from the text (say them
/// back via POST /v1/interact/say to advance dialogue quests).
#[derive(Clone, serde::Serialize)]
pub struct MessageEntry {
    pub kind:     String,
    pub text:     String,
    pub keywords: Vec<String>,
}

/// Live snapshot of the in-game message log, published each tick by the nav thread and read by
/// GET /v1/observe/messages. Exposes NPC dialogue (kind "npc") as machine-readable text + keywords.
pub type MessagesShared = Arc<Mutex<Vec<MessageEntry>>>;

/// One async game event exposed by the `GET /v1/events/*` feed. `category` is the top-level bucket
/// the events API filters on (chat/combat/navigate/system); `kind` is the sub-type
/// (tell/ooc/shout/group/gmsay/zone/slain/attacked/…). `id` is a 1-based monotonic cursor;
/// `directed` = concerns us specifically (a /tell to our name, a GM message, a zone change, our own
/// death). Agents poll `/v1/events/{all,<category>}?since=<id>` (optionally long-poll with `wait=`).
#[derive(Clone, serde::Serialize)]
pub struct Event {
    pub id:       u64,
    pub category: String,
    pub kind:     String,
    pub from:     String,
    pub directed: bool,
    pub text:     String,
}

/// Live snapshot of async events, published each tick by the nav thread, read by the
/// `GET /v1/events/*` endpoints. Ordered by ascending `id`.
pub type ChatEventsShared = Arc<Mutex<Vec<Event>>>;

/// One queued outgoing chat message, set by POST /v1/chat/{tell,ooc,shout,group} and drained by the
/// nav thread, which builds + sends the `OP_ChannelMessage`. `to` is the recipient for /tell (chan
/// 7), empty for broadcasts. `chan` is the EQ ChatChannel number.
#[derive(Clone)]
pub struct ChatSend {
    pub chan: u32,
    pub to:   String,
    pub text: String,
}

/// Outgoing chat queue (FIFO), written by the /v1/chat/{tell,ooc,shout,group} endpoints.
pub type ChatSendShared = Arc<Mutex<Vec<ChatSend>>>;

#[derive(Clone, Copy)]
pub struct CastRequest { pub gem: u8, pub target_id: Option<u32> }
/// Cast a memorized gem (0-8) on an explicit target, else current target, else self.
pub type CastReq = Arc<Mutex<Option<CastRequest>>>;
/// Scribe/memorize request — (slot, spell_id, scribing): scribing 0 = scribe a scroll into the
/// spellbook at book `slot`; 1 = memorize a known spell into gem `slot` (0-8). Set by POST
/// /v1/combat/scribe and POST /v1/combat/memorize; the nav thread sends OP_MemorizeSpell.
/// Tuple = `(slot, spell_id, scribing, from_slot)`. `from_slot` is only used for scribing (0): the
/// RoF2 server scribes only the scroll on the CURSOR, so the nav thread first moves the scroll from
/// `from_slot` → cursor (OP_MoveItem) before the scribe packet. `None` = scroll already on cursor
/// (or memorize/un-mem, which need no move). See eqoxide#11.
pub type MemSpellReq = Arc<Mutex<Option<(u32, u32, u32, Option<u32>)>>>;
/// Posture: Some(true)=sit, Some(false)=stand.
pub type SitReq = Arc<Mutex<Option<bool>>>;
/// Standalone consider of a spawn id.
pub type ConsiderReq = Arc<Mutex<Option<u32>>>;

/// Door-click request — a door_id, set by POST /v1/interact/click_door or a human click in the 3D
/// view. The nav thread reads it once and sends OP_ClickDoor. The door's visual state changes only
/// when the server replies with OP_MoveDoor (server-authoritative).
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
/// Snapshot of the current zone's doors, published each nav tick for GET /v1/observe/doors.
pub type DoorsShared = Arc<Mutex<Vec<DoorView>>>;

/// Current zone name and id, updated on every OP_NEW_ZONE.
#[allow(dead_code)]
pub type ZoneInfo = Arc<Mutex<(String, u16)>>;

/// Seconds without any inbound server packet after which the session is reported disconnected
/// (`connected: false`). Generous enough to ride out normal quiet spells; short enough that a
/// dead/frozen server is caught within a few seconds (eqoxide#8).
pub const CONN_STALE_SECS: u64 = 15;

/// Live player state for the /v1/observe/debug endpoint.
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
    /// Vitals — same values the HUD renders. Percentages are 0–100. Lets an API consumer make
    /// flee/heal/leveling decisions instead of scraping the message log. (eqoxide#9)
    pub hp_pct:        f32,
    pub cur_hp:        i32,
    pub max_hp:        i32,
    pub mana_pct:      f32,
    pub cur_mana:      i32,
    pub max_mana:      i32,
    pub xp_pct:        f32,
    /// Current target's display name and HP percent (0–100), or None when nothing is targeted.
    pub target_name:   Option<String>,
    pub target_hp_pct: Option<f32>,
    /// Milliseconds since the last inbound server packet (connection-health signal, #8).
    pub last_packet_age_ms: u64,
    /// False when no server packet has arrived for [`CONN_STALE_SECS`] — the session is
    /// dead/frozen rather than merely idle. Recomputed from `last_packet_age_ms` every frame (the
    /// derived Default is a transient `false` before the first snapshot).
    pub connected:          bool,
}
pub type PlayerInfo = Arc<Mutex<PlayerState>>;

/// Turn an entity key like "Guard_Phaeton000" into a display name "Guard Phaeton".
pub fn clean_entity_name(raw: &str) -> String {
    raw.trim_end_matches(|c: char| c.is_ascii_digit())
        .replace('_', " ")
        .trim()
        .to_string()
}

/// Render coin `[platinum, gold, silver, copper]` as a JSON object for the API.
pub(crate) fn currency_json(coin: [u32; 4]) -> serde_json::Value {
    serde_json::json!({
        "platinum": coin[0],
        "gold":     coin[1],
        "silver":   coin[2],
        "copper":   coin[3],
    })
}

#[derive(Clone)]
pub(crate) struct HttpState {
    pub(crate) cmd_tx:           Arc<Mutex<Option<CameraCmd>>>,
    pub(crate) snapshot:         Arc<Mutex<CameraSnapshot>>,
    pub(crate) frame_req:        FrameReq,
    pub(crate) goto_target:      GotoTarget,
    pub(crate) entity_positions: EntityPositions,
    pub(crate) entity_ids:       EntityIds,
    pub(crate) zone_points:      ZonePoints,
    pub(crate) zone_cross:       ZoneCrossReq,
    pub(crate) warp:             WarpReq,
    pub(crate) hail:             HailReq,
    pub(crate) say:              SayReq,
    pub(crate) target:           TargetReq,
    pub(crate) attack:           AttackReq,
    pub(crate) cast:             CastReq,
    pub(crate) mem_spell:        MemSpellReq,
    pub(crate) sit:              SitReq,
    pub(crate) consider:         ConsiderReq,
    pub(crate) buy:              BuyReq,
    pub(crate) sell:             SellReq,
    pub(crate) trade:            TradeReq,
    pub(crate) merchant:         MerchantShared,
    pub(crate) move_req:         MoveReq,
    pub(crate) give:             GiveReq,
    pub(crate) inventory:        InventoryShared,
    pub(crate) loot:             LootReq,
    pub(crate) messages:         MessagesShared,
    pub(crate) chat_events:      ChatEventsShared,
    pub(crate) chat_send:        ChatSendShared,
    pub(crate) spells:           std::sync::Arc<crate::spells::SpellDb>,
    pub(crate) player_info:      PlayerInfo,
    pub(crate) task_log:         TaskLog,
    pub(crate) door_click:       DoorClickReq,
    pub(crate) doors_shared:     DoorsShared,
    pub(crate) camp:             CampReq,
    pub(crate) camp_until:       CampUntil,
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
            // Versioned + grouped routes: /v1/<group>/<action>. Each group's `router()` defines
            // relative paths; nesting prefixes them. Shared state is applied once at the end.
            let app = Router::new()
                .nest("/v1/observe",   observe::router())
                .nest("/v1/navigate",  navigate::router())
                .nest("/v1/combat",    combat::router())
                .nest("/v1/interact",  interact::router())
                .nest("/v1/merchant",  merchant::router())
                .nest("/v1/inventory", inventory::router())
                .nest("/v1/chat",      chat::router())
                .nest("/v1/events",    events::router())
                .nest("/v1/camera",    camera::router())
                .nest("/v1/lifecycle", lifecycle::router())
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
