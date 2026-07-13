//! The agent-facing HTTP/REST API (axum). Routes are versioned + grouped: `/v1/<group>/<action>`,
//! where `<group>` mirrors the MCP tool grouping — `observe`, `move`, `combat`, `interact`,
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
mod quests;
mod group;
mod guild;
mod move_api;
mod trainer;
mod pet;
mod combat;
mod interact;
mod merchant;
mod inventory;
mod chat;
mod events;
mod social;
mod camera;
mod lifecycle;

/// A pending frame capture: the render loop drains this, captures a PNG,
/// and sends the bytes back through the channel.
pub type FrameReq = Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>>;

/// A pending `/who all` request: GET /v1/observe/who registers a oneshot sender here; the nav thread
/// drains it, sends OP_WhoAllRequest, and fires it with the parsed roster when OP_WhoAllResponse
/// arrives. (#300)
pub type WhoReq = Arc<Mutex<Option<oneshot::Sender<Vec<crate::game_state::WhoEntry>>>>>;

/// The client-local friends list (names). Edited by POST /v1/social/friends {add|remove}; read by the
/// nav thread to build the OP_FriendsWho poll and by GET /v1/social/friends to annotate online. (#301)
pub type FriendsListShared = Arc<Mutex<Vec<String>>>;
/// A pending friends-presence poll: GET /v1/social/friends registers a oneshot here; the nav thread
/// drains it, sends OP_FriendsWho, and fires it with the online-friends roster (the OP_WhoAllResponse
/// the server sends back) — mirrors [`WhoReq`]. (#301)
pub type FriendsReq = Arc<Mutex<Option<oneshot::Sender<Vec<crate::game_state::WhoEntry>>>>>;

/// Target position for the navigation system. Set by /goto, cleared on arrival.
pub type GotoTarget = Arc<Mutex<Option<(f32, f32, f32)>>>;

/// When /goto targets a named ENTITY, this holds its `entity_positions` key so the nav walker can
/// re-resolve the entity's CURRENT position each tick and CHASE it — roaming mobs move (and their
/// client position is stale until they come within the server's update range), so pathing to a
/// one-time snapshot lands nowhere near them (eqoxide#88). `None` for coordinate gotos. Cleared
/// on arrival/stop alongside `goto_target`.
pub type GotoEntity = Arc<Mutex<Option<String>>>;

/// Authoritative controller snapshot published by the render thread each frame and read by the nav
/// thread to stream OP_ClientUpdate (design §2). Single source of position truth.
pub type ControllerShared = Arc<Mutex<crate::movement::ControllerView>>;

/// The `/goto` planner's per-frame movement intent. The nav planner writes `Some` while walking a
/// path and `None` when idle/arrived; the render controller consumes it when no WASD key is held.
pub type NavIntent = Arc<Mutex<Option<crate::movement::MoveIntent>>>;

/// A large (>12u) server position correction the nav thread hands to the render controller to apply
/// (teleport). Small deltas are ignored — the controller is authoritative (design §3.4).
pub type PosCorrection = Arc<Mutex<Option<[f32; 3]>>>;

/// Single-owner GameState publication (see
/// docs/superpowers/plans/2026-07-12-gamestate-single-owner-snapshot.md). The network thread is
/// the sole writer of `GameState`; it publishes an immutable clone here after every gameplay tick
/// via `eq_net::gameplay::publish_snapshot`. Render/HTTP consumers read it lock-free via `.load()`
/// (borrowed) or `.load_full()` (owned `Arc<GameState>`).
pub type GameStateSnapshot = std::sync::Arc<arc_swap::ArcSwap<crate::game_state::GameState>>;

/// Updated by the network thread every time it applies a real inbound packet (never on a mere
/// idle tick — `GameStateSnapshot` publishes unconditionally every ~10ms, which would make a
/// pointer-equality check meaningless as a connection-health signal). The render thread polls this
/// once per frame to derive `last_packet_age_ms`/`connected` for `/debug` (#8).
pub type LastInboundShared = std::sync::Arc<std::sync::Mutex<std::time::Instant>>;

/// Aggro-avoidance knobs the `/v1/move/*` handlers set and the nav walker reads (#242). `enabled`
/// gates the always-on NPC-camp avoidance (#67) — `false` routes straight through (e.g. to reach a
/// mob). `buffer` widens the soft-avoid radius so the route gives hostile pulls more berth. Default =
/// the historical behavior (avoidance on, no extra buffer). A `/goto`/`/zone_cross` request that omits
/// the fields leaves the current setting unchanged.
#[derive(Clone, Copy)]
pub struct AggroAvoidOpts {
    pub enabled: bool,
    pub buffer:  f32,
}
impl Default for AggroAvoidOpts {
    fn default() -> Self { Self { enabled: true, buffer: 0.0 } }
}
pub type NavAvoidShared = Arc<Mutex<AggroAvoidOpts>>;

/// The walker's ACTUAL committed plan, published each nav tick so the `--nav-debug` overlay can draw
/// exactly what the walker is following instead of an independent per-frame `find_path` recompute
/// (#246). `.0` = coarse global route (`Navigator::path`), `.1` = fine local plan
/// (`Navigator::local_path`). Empty when idle. Draw-only; never steered from.
pub type NavPathView = Arc<Mutex<(Vec<[f32; 3]>, Vec<[f32; 3]>)>>;

/// Live entity name → (x, y, z) map, updated by login.rs as packets arrive.
pub type EntityPositions = Arc<Mutex<HashMap<String, (f32, f32, f32)>>>;

/// Live entity name → spawn_id map (same keys as EntityPositions).
pub type EntityIds = Arc<Mutex<HashMap<String, u32>>>;

/// Zone exit points received in OP_SEND_ZONE_POINTS, exposed via GET /v1/observe/zone_points.
pub type ZonePoints = Arc<Mutex<Vec<crate::game_state::ZonePoint>>>;
/// Native Task-system quest log, published from GameState.tasks each tick (GET /v1/observe/quests/log).
pub type TaskLog = Arc<Mutex<Vec<crate::game_state::ActiveTask>>>;

/// Pending offers from an open task-selector window, published each tick (GET /v1/quests/offers).
pub type TaskOffersShared = Arc<Mutex<Vec<crate::game_state::TaskOffer>>>;
/// Completed-task history with titles, published each tick (GET /v1/quests/completed).
pub type CompletedTasksShared = Arc<Mutex<Vec<crate::game_state::CompletedTaskEntry>>>;
/// Accept/decline a pending task offer, set by POST /v1/quests/accept ({"task_id":N}) or
/// POST /v1/quests/decline (task_id=0). The nav thread reads it once and sends
/// OP_AcceptNewTask (AcceptNewTask_Struct), looking up the offering NPC's id from gs.task_offers.
pub type AcceptTaskReq = Arc<Mutex<Option<u32>>>;
/// Abandon an active task, set by POST /v1/quests/cancel ({"task_id":N}). The nav thread reads it
/// once, looks up the task's sequence_number in gs.tasks, and sends OP_CancelTask
/// (CancelTask_Struct).
pub type CancelTaskReq = Arc<Mutex<Option<u32>>>;

/// Read a book/note item, set by POST /v1/interact/read ({"slot":N}). Carries the inventory wire
/// slot of the item to read. The nav thread takes it, looks up the item's Filename in gs.inventory,
/// and sends OP_ReadBook; the server replies with the text (surfaced via /v1/observe/item_text). (#288)
pub type ReadBookReq = Arc<Mutex<Option<i32>>>;

/// One group member's live view for GET /v1/group/roster (role badges are read-only display
/// flags pushed by the server — not settable via this API in v1).
#[derive(Clone, serde::Serialize)]
pub struct GroupMemberView {
    pub name:     String,
    pub level:    u32,
    pub is_leader: bool,
    pub is_merc:  bool,
    pub tank:     bool,
    pub assist:   bool,
    pub puller:   bool,
    pub offline:  bool,
    pub hp_pct:   f32,
}

/// Published each nav tick from GameState.group_members/group_leader/pending_invite (GET
/// /v1/group/roster, and the UI roster panel). `you_are_leader` is precomputed at publish time
/// (gs.player_name == gs.group_leader) so handlers don't need the player's own name separately.
#[derive(Clone, Default)]
pub struct GroupSnapshot {
    pub members:         Vec<GroupMemberView>,
    pub leader:           String,
    pub pending_invite:   Option<String>,
    pub you_are_leader:   bool,
}
pub type GroupShared = Arc<Mutex<GroupSnapshot>>;

/// Published each nav tick from the player's guild identity + roster: the guild fields of
/// /v1/observe/debug and GET /v1/guild/roster. `guild_id == 0` / empty `guild_name` = not in a
/// guild. Mirrors GroupSnapshot. (#295)
#[derive(Clone, Default)]
pub struct GuildSnapshot {
    pub guild_id:   u32,
    pub guild_name: String,
    pub guild_rank: u32,
    pub members:    Vec<crate::game_state::GuildMember>,
    /// Name of whoever has a pending guild invite out to us (for GET /v1/guild/roster), or None.
    pub pending_invite: Option<String>,
}
pub type GuildShared = Arc<Mutex<GuildSnapshot>>;

/// One queued guild action from POST /v1/guild/{invite,accept,leave,remove}, drained by the nav tick
/// which sends the matching RoF2 guild opcode. Bundled into one slot to keep the Navigator plumbing
/// small. (#295)
#[derive(Clone, Debug, PartialEq)]
pub enum GuildAction {
    Invite(String),   // POST /v1/guild/invite {"name"} — invite a player to our guild
    Accept,           // POST /v1/guild/accept — accept a pending guild invite
    Leave,            // POST /v1/guild/leave — leave our guild
    Remove(String),   // POST /v1/guild/remove {"name"} — leader/GM removes a member
}
pub type GuildActionReq = Arc<Mutex<Option<GuildAction>>>;

/// POST /v1/group/invite target name. Drained by the nav tick loop, which sends OP_GroupInvite.
pub type GroupInviteReq = Arc<Mutex<Option<String>>>;
/// POST /v1/trainer/open sets this to the trainer NPC's spawn id → nav sends OP_GMTraining (#99).
pub type TrainerOpenReq = Arc<Mutex<Option<u32>>>;
/// POST /v1/trainer/train sets this to a skill id → nav sends OP_GMTrainSkill for the open trainer.
pub type TrainerTrainReq = Arc<Mutex<Option<u32>>>;
/// POST /v1/group/accept trigger — accepts gs.pending_invite. One-shot: `Some(())` then drained.
pub type GroupAcceptReq = Arc<Mutex<Option<()>>>;
/// POST /v1/group/decline trigger — declines gs.pending_invite via a defensive OP_GroupDisband.
pub type GroupDeclineReq = Arc<Mutex<Option<()>>>;
/// POST /v1/group/leave trigger — sends OP_GroupDisband(self, self).
pub type GroupLeaveReq = Arc<Mutex<Option<()>>>;
/// POST /v1/group/kick target member name. Sends OP_GroupDisband(self, target).
pub type GroupKickReq = Arc<Mutex<Option<String>>>;
/// POST /v1/group/makeleader target member name. Sends OP_GroupMakeLeader.
pub type GroupMakeLeaderReq = Arc<Mutex<Option<String>>>;

/// Zone-crossing request set by POST /v1/move/zone_cross; gameplay thread reads it once,
/// teleports to the matching zone line and sends OP_ZONE_CHANGE.
///   Some(0)  → cross the nearest zone line (any destination).
///   Some(id) → cross to a specific destination zone id.
pub type ZoneCrossReq = Arc<Mutex<Option<u16>>>;

/// Manual-movement escape hatch (#188), set by POST /v1/move/manual or /v1/move/jump. The render
/// loop drives the CharacterController with this — exactly like WASD — taking priority over the
/// `/goto` nav planner (but below real keyboard input) until `until`, so an agent can walk/hop out
/// of a spot where A* finds no path. `dir` is a world `(east, north)` direction (zero = stand in
/// place, e.g. a jump with no movement). `up` is the vertical axis for swimming (`-1..1`): while in
/// water it drives the character up/down through the water column (#207); it's ignored on land.
#[derive(Clone, Copy)]
pub struct ManualMove {
    pub dir:   [f32; 2],
    pub up:    f32,
    pub jump:  bool,
    pub until: std::time::Instant,
}
pub type ManualMoveReq = Arc<Mutex<Option<ManualMove>>>;

/// A hail request set by POST /v1/interact/hail: the NPC's display name (for the "Hail, <name>"
/// say text) plus its `spawn_id` when known. The nav thread targets the NPC (`spawn_id`) BEFORE
/// saying, because the server only fires an NPC's `EVENT_SAY` on the player's current target
/// (client.cpp: `Mob* t = GetTarget()`), so a hail without a target is silently ignored (#130).
pub type HailReq = Arc<Mutex<Option<(String, Option<u32>)>>>;

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

/// Manual pet command — one OP_PetCommands command byte (PET_ATTACK=2, PET_FOLLOWME=4,
/// PET_GUARDHERE=5, PET_SIT=6, PET_BACKOFF=28; EQEmu zone/common.h), set by POST /v1/pet/command
/// or a Pet-window button. The nav thread drains it and sends OP_PetCommands (attack uses the
/// current target as PetCommand_Struct.target; other commands send target 0).
pub type PetCmdReq = Arc<Mutex<Option<u8>>>;

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
/// Set true by POST /v1/lifecycle/respawn to release a held-dead character back to its bind point
/// (the client no longer auto-respawns — it holds the character slain until asked). (#284)
pub type RespawnReq = Arc<Mutex<bool>>;

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

/// Live snapshot of the current clickable NPC-dialogue choices (saylinks from the most recent NPC
/// message), published each tick by the nav thread and read by GET /v1/observe/dialogue. (#120)
pub type DialogueShared = Arc<Mutex<Vec<crate::game_state::DialogueChoice>>>;

/// Live navigation state for the active `/move/goto`, set by the nav thread and read by
/// GET /v1/observe/debug: `idle` | `navigating` | `arrived` | `no_path` | `blocked`. Lets a driver
/// distinguish "unreachable / no route" from "wedged" from "en route" (#166).
pub type NavStateShared = Arc<Mutex<String>>;

/// Pending "click a dialogue choice" request (POST /v1/interact/dialogue or a GUI click): the nav
/// thread drains it and sends an OP_ItemLinkClick for the chosen saylink. (#120)
pub type DialogueClickReq = Arc<Mutex<Option<crate::game_state::DialogueChoice>>>;

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
pub struct CastRequest {
    pub gem: u8,
    pub target_id: Option<u32>,
    /// When Some, this is an item "clicky" cast: the wire inventory slot of the item to activate.
    /// The gem field is then ignored and the click spell is resolved from the item. (eqoxide#193)
    pub item_slot: Option<u32>,
}
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
    /// The player's own character name — so `/v1/observe/debug` identifies which char it drives (#109).
    pub name:         String,
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
    /// Player skill values by skill id (0..77), for GET /v1/observe/skills (eqoxide#99).
    pub skills:       Vec<u32>,
    /// Whether a guildmaster training window is open, and the caps it offers per skill id (#99).
    pub trainer_open:   bool,
    pub trainer_skills: Vec<u32>,
    /// The player's own spawn id (for scripting a self-target). (eqoxide#95)
    pub player_id:    u32,
    pub target_id:    Option<u32>,
    /// Coin on hand: [platinum, gold, silver, copper], from the player profile.
    pub coin:         [u32; 4],
    /// Vitals — same values the HUD renders. Percentages are 0–100. Lets an API consumer make
    /// flee/heal/leveling decisions instead of scraping the message log. (eqoxide#9)
    pub hp_pct:        f32,
    pub cur_hp:        i32,
    pub max_hp:        i32,
    /// Death state for headless agents (#284). `dead` = currently slain (held until POST
    /// /v1/lifecycle/respawn). `killed_by` + `died_ago_secs` also persist for a window AFTER a
    /// respawn, so an infrequently-polling agent can still tell it died and what killed it.
    pub dead:          bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub killed_by:     Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub died_ago_secs: Option<u64>,
    pub mana_pct:      f32,
    pub cur_mana:      i32,
    pub max_mana:      i32,
    pub xp_pct:        f32,
    /// Current target's display name and HP percent (0–100), or None when nothing is targeted.
    pub target_name:   Option<String>,
    pub target_hp_pct: Option<f32>,
    /// #292: consider result for the current target (from POST /v1/combat/consider). `target_con`
    /// is the difficulty tier (gray/green/light_blue/blue/white/yellow/red), `target_attitude` the
    /// faction attitude enum (ally … scowls), `target_level` the target's actual level — so agents
    /// can gauge "how tough + how hostile" programmatically. Populated after a consider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_con:      Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_attitude: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_level:    Option<u32>,
    /// Milliseconds since the last inbound server packet (connection-health signal, #8).
    pub last_packet_age_ms: u64,
    /// False when no server packet has arrived for [`CONN_STALE_SECS`] — the session is
    /// dead/frozen rather than merely idle. Recomputed from `last_packet_age_ms` every frame (the
    /// derived Default is a transient `false` before the first snapshot).
    pub connected:          bool,
    /// Smoothed per-phase frame timings (ms); all zero unless the client runs with `--profile` /
    /// `EQ_PROFILE=1`. Exposed under `/v1/observe/debug` → `frame_profile` so perf work (#152)
    /// can read phase costs programmatically instead of screenshotting the HUD overlay.
    pub frame_profile:      crate::profiling::FrameProfile,
    /// Text of the most recently read book/note (OP_ReadBook reply), newline-decoded. None until a
    /// book is read. Surfaced via GET /v1/observe/item_text so an agent can read a quest note (#288).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book_text:          Option<String>,
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
    pub(crate) goto_entity:      GotoEntity,
    pub(crate) entity_positions: EntityPositions,
    pub(crate) entity_ids:       EntityIds,
    pub(crate) zone_points:      ZonePoints,
    /// Zone collision + region map (shared with the nav thread); read-only here, for zone_exits.
    pub(crate) shared_collision: crate::assets::SharedCollision,
    pub(crate) zone_cross:       ZoneCrossReq,
    /// Aggro-avoidance knobs set by /v1/move/goto|zone_cross and read by the nav walker (#242).
    pub(crate) nav_avoid:        NavAvoidShared,
    /// Manual-move / jump escape hatch (#188), consumed by the render loop.
    pub(crate) manual_move:      ManualMoveReq,
    pub(crate) hail:             HailReq,
    pub(crate) say:              SayReq,
    pub(crate) target:           TargetReq,
    pub(crate) who_req:          WhoReq,
    pub(crate) friends_list:     FriendsListShared,
    pub(crate) friends_req:      FriendsReq,
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
    pub(crate) dialogue:         DialogueShared,
    pub(crate) nav_state:        NavStateShared,
    pub(crate) dialogue_click:   DialogueClickReq,
    pub(crate) chat_events:      ChatEventsShared,
    pub(crate) chat_send:        ChatSendShared,
    pub(crate) spells:           std::sync::Arc<crate::spells::SpellDb>,
    pub(crate) player_info:      PlayerInfo,
    pub(crate) task_log:         TaskLog,
    pub(crate) task_offers_shared:    TaskOffersShared,
    pub(crate) completed_tasks_shared: CompletedTasksShared,
    pub(crate) accept_task:           AcceptTaskReq,
    pub(crate) cancel_task:           CancelTaskReq,
    pub(crate) group:             GroupShared,
    pub(crate) group_invite:      GroupInviteReq,
    pub(crate) trainer_open_req:  TrainerOpenReq,
    pub(crate) trainer_train_req: TrainerTrainReq,
    pub(crate) group_accept:      GroupAcceptReq,
    pub(crate) group_decline:     GroupDeclineReq,
    pub(crate) group_leave:       GroupLeaveReq,
    pub(crate) group_kick:        GroupKickReq,
    pub(crate) group_make_leader: GroupMakeLeaderReq,
    pub(crate) door_click:       DoorClickReq,
    pub(crate) doors_shared:     DoorsShared,
    pub(crate) camp:             CampReq,
    pub(crate) camp_until:       CampUntil,
    pub(crate) respawn:          RespawnReq,
    pub(crate) pet_cmd:          PetCmdReq,
    pub(crate) read_book:        ReadBookReq,
    pub(crate) guild:            GuildShared,
    pub(crate) guild_action:     GuildActionReq,
}

pub fn spawn_camera_server(
    cmd_tx:           Arc<Mutex<Option<CameraCmd>>>,
    snapshot:         Arc<Mutex<CameraSnapshot>>,
    frame_req:        FrameReq,
    goto_target:      GotoTarget,
    goto_entity:      GotoEntity,
    entity_positions: EntityPositions,
    entity_ids:       EntityIds,
    zone_points:      ZonePoints,
    shared_collision: crate::assets::SharedCollision,
    zone_cross:       ZoneCrossReq,
    manual_move:      ManualMoveReq,
    hail:             HailReq,
    say:              SayReq,
    target:           TargetReq,
    who_req:          WhoReq,
    friends_list:     FriendsListShared,
    friends_req:      FriendsReq,
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
    dialogue:         DialogueShared,
    nav_state:        NavStateShared,
    dialogue_click:   DialogueClickReq,
    chat_events:      ChatEventsShared,
    chat_send:        ChatSendShared,
    spells:           std::sync::Arc<crate::spells::SpellDb>,
    player_info:      PlayerInfo,
    task_log:         TaskLog,
    task_offers_shared:    TaskOffersShared,
    completed_tasks_shared: CompletedTasksShared,
    accept_task:           AcceptTaskReq,
    cancel_task:           CancelTaskReq,
    group:             GroupShared,
    group_invite:      GroupInviteReq,
    trainer_open_req:  TrainerOpenReq,
    trainer_train_req: TrainerTrainReq,
    group_accept:      GroupAcceptReq,
    group_decline:     GroupDeclineReq,
    group_leave:       GroupLeaveReq,
    group_kick:        GroupKickReq,
    group_make_leader: GroupMakeLeaderReq,
    door_click:       DoorClickReq,
    doors_shared:     DoorsShared,
    camp:             CampReq,
    camp_until:       CampUntil,
    respawn:          RespawnReq,
    pet_cmd:          PetCmdReq,
    nav_avoid:        NavAvoidShared,
    read_book:        ReadBookReq,
    guild:            GuildShared,
    guild_action:     GuildActionReq,
    port:             u16,
    // When `Some`, an already-bound listener from `--api-port` (exact port, no scan).
    // When `None`, scan upward from `port` for the first free port.
    exact_listener:   Option<std::net::TcpListener>,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("http tokio runtime");
        rt.block_on(async move {
            let state = HttpState { cmd_tx, snapshot, frame_req, goto_target, goto_entity, entity_positions, entity_ids, zone_points, shared_collision, zone_cross, manual_move, hail, say, target, who_req, friends_list, friends_req, attack, cast, mem_spell, sit, consider, buy, sell, trade, merchant, move_req, give, inventory, loot, messages, dialogue, nav_state, dialogue_click, chat_events, chat_send, spells, player_info, task_log, task_offers_shared, completed_tasks_shared, accept_task, cancel_task, group, group_invite, trainer_open_req, trainer_train_req, group_accept, group_decline, group_leave, group_kick, group_make_leader, door_click, doors_shared, camp, camp_until, respawn, pet_cmd, nav_avoid, read_book, guild, guild_action };
            // Versioned + grouped routes: /v1/<group>/<action>. Each group's `router()` defines
            // relative paths; nesting prefixes them. Shared state is applied once at the end.
            let app = Router::new()
                .nest("/v1/observe",   observe::router())
                .nest("/v1/quests",    quests::router())
                .nest("/v1/group",     group::router())
                .nest("/v1/guild",     guild::router())
                .nest("/v1/move",      move_api::router())
                .nest("/v1/trainer",   trainer::router())
                .nest("/v1/pet",       pet::router())
                .nest("/v1/combat",    combat::router())
                .nest("/v1/interact",  interact::router())
                .nest("/v1/merchant",  merchant::router())
                .nest("/v1/inventory", inventory::router())
                .nest("/v1/chat",      chat::router())
                .nest("/v1/events",    events::router())
                .nest("/v1/social",    social::router())
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
