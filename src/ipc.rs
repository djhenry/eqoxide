//! IPC channel "request-slot" types shared between the HTTP API thread, the network
//! (login/gameplay/navigation) thread, and the render/app loop.
//!
//! These are `Arc<Mutex<Option<T>>>`-style shared cells an HTTP handler writes a request into and
//! the network action loop (or, for a few render-owned values, the app loop) drains each tick, plus
//! the matching "published snapshot" direction (`Arc<Mutex<T>>` / `Arc<ArcSwap<T>>`) the network
//! thread writes and HTTP/render read. They are neither genuine HTTP types (route state, request/
//! response bodies — those stay in `crate::http`) nor genuine network-protocol types — this module
//! is the neutral third party both sides depend on, so the network loop no longer has to reach into
//! `crate::http` for its own inter-thread plumbing. Relocated out of `src/http/mod.rs` (cleanup; pure
//! code motion, no behavior change) — see that module's docs for the HTTP-side half of this split.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

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

/// The three clocks that answer "can I trust anything else in this payload?", owned and stamped by
/// the network thread and turned into `Health` **at HTTP read time** (`HttpState::health`), never
/// cached (#8, #343). They are deliberately separate signals, because they fail independently:
///
/// | clock           | bumped when                        | a stale value means                      |
/// |-----------------|------------------------------------|------------------------------------------|
/// | `last_datagram` | ANY inbound UDP datagram           | **the link is dead** → `connected: false` |
/// | `last_packet`   | an inbound APPLICATION packet       | the world is quiet (NOT necessarily dead) |
/// | `last_tick`     | every gameplay tick (~10ms)         | OUR network thread wedged/died            |
///
/// The `last_datagram` / `last_packet` split is load-bearing and was found by live-testing #343: a
/// genuinely idle EQ session (a character sitting alone in an empty zone) goes **40+ seconds**
/// without a single application packet, while the session layer keeps ACKing throughout. Deriving
/// `connected` from application traffic would therefore report a perfectly healthy idle session as
/// disconnected — trading the old false `true` for an equally dishonest false `false`.
///
/// #371 adds a FOURTH failure those three cannot fully see: a zone that is **still ticking but not
/// making application progress for us** — a stuck per-client dispatch, an infinite/blocking quest
/// script, or a tick so slow it never services our packets. Such a zone keeps ACKing
/// (`last_datagram` fresh → `connected: true`) while producing no application output for us
/// (`last_packet` climbing) — which is *pixel-identical* to a healthy-but-idle zone, because the
/// symptom is exactly "the world stopped speaking". No passive clock can separate them. The only
/// sound discriminator is an ACTIVE probe: periodically send a cheap request the zone MAIN LOOP
/// must service to answer, and time the reply. `last_probe_sent` / `last_probe_reply` are that
/// round-trip's clocks; `HttpState::health` turns them into `world_responsive` at read time.
///
/// SCOPE (do not oversell): this EQEmu build runs the zone as a single-threaded libuv loop, so a
/// *total* freeze stops the ACKs too and is ALREADY caught by `connected: false`. `world_responsive`
/// does NOT add total-freeze detection — it adds the still-ticking-but-unresponsive case above,
/// which `connected` cannot see. (The old Titanium `EQStreamFactory` split a hung main loop from a
/// still-ACKing reader thread; this server does not work that way — do not reason from that model.)
#[derive(Debug, Clone, Copy)]
pub struct NetHealth {
    /// Last inbound datagram of ANY kind, session-layer ACKs/keepalives included → link liveness.
    pub last_datagram: std::time::Instant,
    /// Last inbound APPLICATION packet (a decoded opcode that mutated `GameState`) → world activity.
    /// NOTE: the #371 liveness-probe reply is deliberately NOT stamped here — it is a solicited poke,
    /// not spontaneous world output, and counting it would cap `last_packet_age_ms` at the probe
    /// cadence and destroy its "the world has been quiet for 45s" meaning. It stamps `last_probe_reply`.
    pub last_packet:   std::time::Instant,
    /// Last network-thread gameplay tick → client liveness (is our own publisher still running?).
    pub last_tick:     std::time::Instant,
    /// When the network thread MOST RECENTLY (re)sent an active liveness probe (#371). Bumped on
    /// every 30s resend while a probe stays unanswered — this is a scheduling clock only. Do NOT feed
    /// this into `world_responsive`'s timeout check: resending an already-unanswered probe must not
    /// look like a *fresh* one, or a permanently wedged zone would flicker back to "responsive" every
    /// time the resend fires (the exact bug this comment is warning against — see
    /// `first_unanswered_probe_sent` below, which is what `world_responsive` actually reads).
    /// `None` until the first probe fires (e.g. before we are fully in-zone) — in which case there is
    /// simply no probe verdict yet and `world_responsive` defers to the passive signals.
    pub last_probe_sent:  Option<std::time::Instant>,
    /// When we last saw the probe's reply come back from the zone (#371). Compared against
    /// `first_unanswered_probe_sent` to tell an answered probe from an outstanding one.
    pub last_probe_reply: Option<std::time::Instant>,
    /// When the CURRENT unanswered-probe streak began (#371 wedge-flicker fix). Set the first time a
    /// probe is sent while none is already outstanding; deliberately left UNCHANGED by later resends
    /// of that same still-unanswered probe, so a zone that never answers cannot "earn" a fresh 10s
    /// in-flight grace window every time we poke it again. Reset to `None` the moment ANY proof of
    /// life arrives — a genuine probe reply (`record_probe_reply`) OR any spontaneous application
    /// packet (`record_app_packet`) — and on zone-change (`reset_probe_clocks`). Clearing on
    /// spontaneous traffic is load-bearing: it re-arms the clock so a SECOND wedge after a traffic
    /// recovery is timed freshly and still detected (without it, a stale streak-start would make the
    /// answered-clause permanently true → a confident false-alive). This — not `last_probe_sent` — is
    /// what `world_responsive` measures its timeout against, so once a wedge verdict is reached within
    /// one continuous silence it stays `false` until real proof of life, no matter how many resends
    /// happen in between.
    pub first_unanswered_probe_sent: Option<std::time::Instant>,
}

impl Default for NetHealth {
    fn default() -> Self {
        let now = std::time::Instant::now();
        NetHealth {
            last_datagram: now, last_packet: now, last_tick: now,
            last_probe_sent: None, last_probe_reply: None,
            first_unanswered_probe_sent: None,
        }
    }
}

/// #371: a probe left unanswered longer than this — while no spontaneous application packet has
/// arrived either — means the zone main loop is not processing (a wedged world), even though the
/// link keeps ACKing. Kept below `PROBE_INTERVAL` so a wedge is declared before the next probe is
/// even due; kept well above a normal round-trip so ordinary latency never false-alarms.
pub const PROBE_TIMEOUT_SECS: u64 = 10;

/// #371: decide, at HTTP read time, whether the WORLD (not just the link) is alive, from the
/// probe/app clocks expressed as ages (time since the event; `None` = it never happened). Pure so
/// the state machine can be exhaustively unit-tested without a socket. Returns `world_responsive`.
///
/// The rule: a probe is only damning once it is BOTH unanswered AND overdue.
/// - **No probe sent yet** → `true`. We are not asserting a probe result we do not have; the
///   passive signals (`connected` / `last_packet_age_ms`) stand until the first probe fires.
/// - **Answered** → `true`. "Answered" = proof the zone processed something at or after we sent the
///   probe: its own reply (`probe_reply_ago <= first_unanswered_sent_ago`) OR *any* spontaneous
///   application packet since (`last_packet_ago <= first_unanswered_sent_ago`). The second clause is
///   belt-and-suspenders: a busy zone is obviously alive even if a single probe reply was dropped,
///   and it is exactly what keeps a legitimately-quiet-but-answering idle session from ever
///   false-alarming.
/// - **Outstanding but not yet overdue** (`first_unanswered_sent_ago < timeout`) → `true`. Still in
///   flight; never mistake normal latency for a wedge.
/// - **Outstanding AND overdue** → `false`. The wedged-world signal — the whole point of #371.
///
/// CALLER CONTRACT (#371 wedge-flicker fix): `first_unanswered_sent_ago` MUST be the age of the
/// FIRST send of the current unanswered probe streak, not the most recent resend. `gameplay.rs`
/// resends an unanswered probe every `PROBE_INTERVAL` (30s) purely to keep detecting recovery; if
/// this function were fed the age of that most-recent resend instead, a permanently wedged zone
/// would re-enter the "still in flight" branch every 30s and flicker back to `true` forever even
/// though it never actually answers. `NetHealth::first_unanswered_probe_sent` is the clock that
/// holds still across resends and only clears on a genuine reply or a zone change — feed that one.
pub fn world_responsive(
    first_unanswered_sent_ago: Option<std::time::Duration>,
    probe_reply_ago:           Option<std::time::Duration>,
    last_packet_ago:           std::time::Duration,
    timeout:                   std::time::Duration,
) -> bool {
    match first_unanswered_sent_ago {
        None => true,
        Some(sent_ago) => {
            let answered = probe_reply_ago.is_some_and(|r| r <= sent_ago)
                        || last_packet_ago <= sent_ago;
            answered || sent_ago < timeout
        }
    }
}

pub type NetHealthShared = std::sync::Arc<std::sync::Mutex<NetHealth>>;

/// Smoothed per-frame phase timings, published by the **render** thread (the only agent-visible
/// value the renderer legitimately owns — see `PlayerState`'s note on the network/render split).
pub type FrameProfileShared = std::sync::Arc<std::sync::Mutex<crate::profiling::FrameProfile>>;

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
/// (#246). `.0` = coarse global route (`ActionLoop::path`), `.1` = fine local plan
/// (`ActionLoop::local_path`). Empty when idle. Draw-only; never steered from.
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
/// which sends the matching RoF2 guild opcode. Bundled into one slot to keep the ActionLoop plumbing
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// GET /v1/observe/debug. `state` is the agent-facing contract documented in `docs/http-api.md`:
///
/// `idle` | `planning` | `navigating` | `navigating_partial` | `following` | `arrived` |
/// `no_path` | `search_exhausted` | `blocked`
///
/// `reason` is the machine-readable WHY behind a terminal state (`goal_not_walkable`,
/// `search_closed`, `search_node_cap`, …). The whole point of the split (#337): a driver must be
/// able to tell "there is no route" (definitive) from "the planner gave up" (I don't know) from
/// "I am wedged" — three answers the old, overloaded `blocked` collapsed into one silent freeze.
#[derive(Clone, Debug, PartialEq)]
pub struct NavStatus {
    pub state:  String,
    pub reason: Option<String>,
    /// The agent-honesty payload behind a terminal `no_path` (#378 Phase 2): WHAT is blocking the
    /// goal, and WHERE. `blocked_goal` is the definitive "your goal itself cannot be stood at";
    /// `blocked_frontier` is "I got as close as here and this is the obstruction between me and the
    /// goal". Surfaced as `nav_blocked_by.goal` / `nav_blocked_by.frontier` on /v1/observe/debug.
    /// `None` when there is no blockage to report (not a terminal no_path, or the diagnosis could
    /// not be computed) — honest silence, never a fabricated hazard.
    pub blocked_goal: Option<NavBlockage>,
    pub blocked_frontier: Option<NavBlockage>,
    /// Which clearance tier answered the CURRENT route (#378 Phase 2 / design §4c): `preferred`
    /// (roomy) or `minimum` (threaded a tight gap with no margin to spare — a riskier path). `None`
    /// until a route is committed. This is the PER-ROUTE fact the zone-lifetime `nav_tight` counter
    /// could not give (it is `connected:true`'s shape — a field with no per-instance writer, #343).
    pub tier: Option<&'static str>,
    /// The FINE LOCAL (2 u) steering tier's last honest outcome (#382), published as the top-level
    /// `nav_local` on GET /v1/observe/debug. `None` = the tier has not answered for the current route
    /// (idle, or the first fine plan is still in flight).
    ///
    /// It is carried HERE, alongside `state`/`reason`, rather than in a second shared cell, because
    /// the two are read together and must not be able to drift: an agent that sees
    /// `nav_state: navigating` needs to know, in the same snapshot, whether the tier that is actually
    /// steering it can see a way through the next 40 u.
    pub local:  Option<NavLocal>,
}

/// A named obstruction with a position — the agent-facing form of `traversability::Blockage`
/// (#378 Phase 2). `hazard` is `floor` | `wall` | `water`.
#[derive(Clone, Debug, PartialEq)]
pub struct NavBlockage {
    pub hazard: &'static str,
    pub at: [f32; 3],
}

/// What the fine 2 u steering tier last said, verbatim. See `nav::collision::LocalOutcome`.
///
/// **`state` is never `no_path` and structurally cannot be.** The fine search closes only the frontier
/// inside a 40 u window, so it can never prove a goal unreachable; conflating its local dead-end with
/// a definitive "no route" would be #337 with a smaller radius.
#[derive(Clone, Debug, PartialEq)]
pub struct NavLocal {
    /// `threaded` (healthy: a complete fine route to the carrot) | `no_way_through` (the window's
    /// frontier CLOSED — the coarse corridor is not threadable here) | `exhausted` (the search was
    /// cut short: "I don't know") | `planner_dead` (the fine worker died; steering has degraded to
    /// the coarse route only — the walker keeps walking).
    pub state:       String,
    /// `threaded` | `search_closed` | `start_isolated` | `goal_not_walkable` | `no_geometry` |
    /// `search_node_cap` | `local_planner_dead`.
    pub reason:      String,
    /// Consecutive nav ticks the fine tier has failed to thread to its carrot. A nonzero value with
    /// `state: navigating` means the walker is being steered on the coarse route through a stretch the
    /// fine tier says it cannot fit — usually the prelude to a proactive coarse re-plan (#246).
    pub stuck_ticks: u32,
    /// How long the last fine plan took, in microseconds. This is the per-tick cost that used to be
    /// paid **on the network thread** (mean 15.3 ms, worst 358 ms, release/akanon) and is now paid on
    /// the fine worker.
    pub plan_us:     u64,
}

impl Default for NavStatus {
    fn default() -> Self {
        NavStatus { state: "idle".into(), reason: None, local: None,
            blocked_goal: None, blocked_frontier: None, tier: None }
    }
}

impl From<&str> for NavStatus {
    fn from(state: &str) -> Self {
        NavStatus { state: state.to_string(), reason: None, local: None,
            blocked_goal: None, blocked_frontier: None, tier: None }
    }
}

impl PartialEq<&str> for NavStatus {
    fn eq(&self, other: &&str) -> bool { self.state == *other }
}

pub type NavStateShared = Arc<Mutex<NavStatus>>;

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

// ── Domain slot bundles (M4) ────────────────────────────────────────────────────────────────
//
// Everything above this line is an individual slot alias/type. `ActionLoop` (the network/nav
// thread's per-tick state, `eq_net::action_loop`) and `HttpState` (the HTTP API's per-request
// state, `http::mod`) each used to hold ~50–60 of these as flat, individually-named fields —
// duplicated field lists in two structs, two constructors, and two hand-written test builders,
// with no structure connecting e.g. `attack`/`cast`/`target` as "the combat slots" beyond
// eyeballing the source.
//
// These bundles regroup the same fields BY DOMAIN, one struct per HTTP API group
// (`/v1/combat`, `/v1/merchant`, `/v1/group`, …) — the router nesting in `http::mod::
// spawn_camera_server` is the authoritative domain boundary these mirror, since that's already
// the seam a future shared "controller verb" (one call both a UI click-handler and an agent HTTP
// handler go through, instead of each independently poking a slot) would need to land on. This
// is PURE REGROUPING: every field keeps its original name and `Arc`-sharing semantics unchanged
// — only its home moved from `ActionLoop`/`HttpState` directly to one of these, embedded by
// whichever of the two structs actually reads it. See `ActionLoop::new` and
// `http::mod::spawn_camera_server`/`HttpState` for how a bundle is constructed exactly ONCE and
// then `.clone()`d (a shallow `Arc`-handle clone, not a fresh channel) into each consumer that
// needs it — never `Default`-constructed twice, which would silently sever the channel.
//
// A `TODO(MVC)` marker sits at a handful of representative drain sites in `action_loop.rs` for
// where that future controller-verb unification would land; these bundles are the plumbing for
// it, not the verbs themselves (out of scope here — this is a behavior-preserving refactor).

/// `/v1/combat/*`: targeting, auto-attack, consider, spell scribe/memorize/cast, and the one
/// `/v1/pet/command` slot (small enough on its own that a dedicated `PetSlots` would just be
/// noise — it rides along with the other "act on a target" verbs).
#[derive(Clone, Default)]
pub struct CombatSlots {
    pub attack:   AttackReq,
    pub cast:     CastReq,
    pub mem_spell: MemSpellReq,
    pub consider: ConsiderReq,
    pub target:   TargetReq,
    pub pet_cmd:  crate::ipc::PetCmdReq,
}

/// `/v1/merchant/*`: open/close a vendor window, list wares, buy, sell.
#[derive(Clone, Default)]
pub struct MerchantSlots {
    pub merchant: MerchantShared,
    pub buy:      BuyReq,
    pub sell:     SellReq,
    pub trade:    TradeReq,
}

/// `/v1/inventory/*`: the live snapshot plus the one move/equip/unequip request slot.
#[derive(Clone, Default)]
pub struct InventorySlots {
    pub inventory: InventoryShared,
    pub move_req:  MoveReq,
}

/// `/v1/interact/*`: NPC/world interaction — hail, say, loot, give (turn-in), doors, sit/stand,
/// dialogue clicks, and reading a book/note. Mirrors `http::interact`'s own module doc verbatim
/// ("NPC/world interaction: hail, say, loot, give (turn-in), doors, sit/stand") — that file is
/// the domain boundary this bundle reifies, including `doors_shared` (the read-side twin of
/// `door_click`, published for GET /v1/observe/doors but conceptually the same door verb).
#[derive(Clone, Default)]
pub struct InteractSlots {
    pub hail:           HailReq,
    pub say:            SayReq,
    pub loot:           LootReq,
    pub give:           GiveReq,
    pub door_click:     DoorClickReq,
    pub doors_shared:   DoorsShared,
    pub sit:            SitReq,
    pub dialogue:       DialogueShared,
    pub dialogue_click: DialogueClickReq,
    pub read_book:      ReadBookReq,
}

/// `/v1/quests/*`: the native Task-system log/offers/history plus accept/cancel requests.
#[derive(Clone, Default)]
pub struct QuestSlots {
    pub task_log:               TaskLog,
    pub task_offers_shared:     TaskOffersShared,
    pub completed_tasks_shared: CompletedTasksShared,
    pub accept_task:            AcceptTaskReq,
    pub cancel_task:             CancelTaskReq,
}

/// `/v1/group/*`: roster + invite/accept/decline/leave/kick/transfer-leadership.
#[derive(Clone, Default)]
pub struct GroupSlots {
    pub group:             GroupShared,
    pub group_invite:      GroupInviteReq,
    pub group_accept:      GroupAcceptReq,
    pub group_decline:     GroupDeclineReq,
    pub group_leave:       GroupLeaveReq,
    pub group_kick:        GroupKickReq,
    pub group_make_leader: GroupMakeLeaderReq,
}

/// `/v1/guild/*`: roster + identity snapshot plus the one queued guild action.
#[derive(Clone, Default)]
pub struct GuildSlots {
    pub guild:        GuildShared,
    pub guild_action: GuildActionReq,
}

/// `/v1/trainer/*`: open a trainer window, train a skill.
#[derive(Clone, Default)]
pub struct TrainerSlots {
    pub trainer_open_req:  TrainerOpenReq,
    pub trainer_train_req: TrainerTrainReq,
}

/// `/v1/social/*`: the client-local friends list plus the `/who` and friends-presence polls.
#[derive(Clone, Default)]
pub struct SocialSlots {
    pub who_req:      WhoReq,
    pub friends_list: FriendsListShared,
    pub friends_req:  FriendsReq,
}

/// The outgoing/async text feeds: `/v1/chat/*` (outgoing), `/v1/events/*` (async feed), and the
/// machine-readable NPC/system message log surfaced at `/v1/observe/messages`. Grouped together
/// (rather than splitting `messages` into its own bundle or into `InteractSlots`) because all
/// three are "a queue/log of text the nav thread produces or consumes", read by adjacent handlers
/// in practice (an agent polling `/events` is usually also reading `/observe/messages`).
#[derive(Clone, Default)]
pub struct ChatSlots {
    pub chat_events: ChatEventsShared,
    pub chat_send:   ChatSendShared,
    pub messages:    MessagesShared,
}

/// `/v1/move/*`: the `/goto` target (+ chase-entity), zone-crossing, aggro-avoidance knobs, live
/// nav status, and the walker's draw-only path mirror. Does NOT include the manual-move/jump
/// escape hatch (`ManualMoveReq`) — that slot is consumed by the RENDER thread, not the nav
/// thread/`ActionLoop` (see `CameraSlots`), so folding it in here would make `ActionLoop` carry a
/// field it can never read.
#[derive(Clone, Default)]
pub struct NavSlots {
    pub goto_target:   GotoTarget,
    pub goto_entity:   GotoEntity,
    pub zone_cross:    ZoneCrossReq,
    pub nav_avoid:     NavAvoidShared,
    pub nav_state:     NavStateShared,
    pub nav_path_view: NavPathView,
}

/// The live entity registry (`login.rs` writes it as spawn packets arrive): name → position/id,
/// plus the zone's exit points. Read by nearly every domain to resolve a name/target to a spawn
/// id (merchant buy/sell, combat target, trainer open, `/goto` by name, …) — it is genuinely a
/// shared world index, not particular to navigation, even though nav is its biggest reader.
#[derive(Clone, Default)]
pub struct WorldSlots {
    pub entity_positions: EntityPositions,
    pub entity_ids:       EntityIds,
    pub zone_points:      ZonePoints,
}

/// Single-authority controller integration (design §2): the render thread's authoritative
/// position snapshot streamed to the server, the `/goto` planner's per-frame movement intent, and
/// a server correction handed back to the controller. `ActionLoop`-only — `HttpState` has no
/// controller-facing endpoint today, so there is nothing for it to embed here.
#[derive(Clone, Default)]
pub struct ControllerSlots {
    pub controller_view: ControllerShared,
    pub nav_intent:      NavIntent,
    pub pos_correction:  PosCorrection,
}

/// `/v1/lifecycle/*`: camp (+ its published deadline) and respawn. `HttpState`-only: `ActionLoop`
/// only ever WRITES `camp` (never reads `camp_until`/`respawn`, which the separate gameplay-tick
/// gets directly — see `eq_net::gameplay::run_gameplay_phase`), so it keeps a lone `camp` field
/// rather than embedding this whole bundle for one field it partially uses.
#[derive(Clone, Default)]
pub struct LifecycleSlots {
    pub camp:       CampReq,
    pub camp_until: CampUntil,
    pub respawn:    RespawnReq,
}

/// What HTTP hands straight to the RENDER thread, bypassing the nav thread entirely:
/// `/v1/camera/*` (cmd + published snapshot), `GET /v1/observe/frame` (frame-capture request),
/// and the manual-move/jump escape hatch consumed by the controller alongside WASD. `HttpState`-
/// only; no `Default` (the camera snapshot's initial value is meaningful — see `App::new`/
/// `main.rs` — so callers construct this explicitly rather than risk a silently-wrong default).
#[derive(Clone)]
pub struct CameraSlots {
    pub cmd_tx:      Arc<Mutex<Option<crate::camera_state::CameraCmd>>>,
    pub snapshot:    Arc<Mutex<crate::camera_state::CameraSnapshot>>,
    pub frame_req:   FrameReq,
    pub manual_move: ManualMoveReq,
}

/// #371: the active-liveness-probe state machine, tested as a pure function. These are the exact
/// distinctions the issue turns on — a wedged-but-ACKing world vs a genuinely idle one — proved
/// without a socket. The `secs`/`ms` helpers keep the age arithmetic readable.
#[cfg(test)]
mod world_responsive_tests {
    use super::{world_responsive, PROBE_TIMEOUT_SECS};
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(PROBE_TIMEOUT_SECS);
    fn s(secs: u64) -> Duration { Duration::from_secs(secs) }

    /// THE bug (#371): a probe was sent, no reply has come, and the world has been silent longer than
    /// the bound — while the link is still ACKing. That is a wedged world, and it MUST read as such.
    #[test]
    fn unanswered_probe_past_the_bound_reports_the_world_wedged() {
        // The realistic wedge: the last spontaneous packet PREDATES the probe (world went quiet at
        // 30s ago, we probed 15s ago), the probe was never answered, and 15s > the 10s bound. The
        // probe is only ever sent AFTER a stretch of app-silence, so last_packet_ago > probe_sent_ago
        // always holds here — nothing has arrived since the probe to prove liveness.
        assert!(!world_responsive(Some(s(15)), None, s(30), TIMEOUT),
            "an unanswered probe past the timeout, on a still-ACKing link, is a wedged world");
    }

    /// The #343-trap-in-reverse: a legitimately IDLE session that has no spontaneous app traffic for
    /// 45s but whose probe IS answered must STILL read as live. This is the false-alarm we must not
    /// raise — the whole reason a passive `last_packet_age_ms` threshold cannot solve the problem.
    #[test]
    fn idle_but_answered_probe_is_still_live() {
        // last spontaneous packet 45s ago (a normal solo-idle gap), but the probe replied 2s ago.
        assert!(world_responsive(Some(s(30)), Some(s(2)), s(45), TIMEOUT),
            "an idle world that ANSWERS the probe is alive — do not false-alarm on app-silence alone");
    }

    /// A probe answered by its own reply is live even with zero spontaneous traffic.
    #[test]
    fn answered_probe_reports_live() {
        assert!(world_responsive(Some(s(30)), Some(s(1)), s(30), TIMEOUT));
    }

    /// A probe in flight but not yet overdue must NOT false-alarm — ordinary round-trip latency is
    /// not a wedge. Only crossing the bound flips it.
    #[test]
    fn outstanding_probe_within_the_bound_is_not_yet_a_wedge() {
        // Unanswered (last packet predates the probe → no proof of life since), but 3s < 10s bound.
        assert!(world_responsive(Some(s(3)), None, s(20), TIMEOUT),
            "a 3s-old unanswered probe (bound 10s) is still in flight, not a wedge");
        // ...and one whose prior reply predates the newest send is likewise still outstanding.
        assert!(world_responsive(Some(s(3)), Some(s(20)), s(20), TIMEOUT),
            "a reply OLDER than the latest probe does not answer it, but 3s < 10s is not yet overdue");
    }

    /// Spontaneous application traffic since the probe was sent proves the world is processing even
    /// if that one probe reply was dropped — a busy zone must never read as wedged. This is the
    /// belt-and-suspenders clause.
    #[test]
    fn spontaneous_traffic_since_the_probe_proves_liveness() {
        // probe sent 15s ago, no probe reply, BUT an app packet arrived 1s ago (world is busy).
        assert!(world_responsive(Some(s(15)), None, s(1), TIMEOUT),
            "any app packet since the probe proves liveness — a busy zone is never wedged");
    }

    /// Before the first probe fires (e.g. mid zone-in), there is no probe verdict; defer to the
    /// passive signals rather than assert a liveness we have not measured.
    #[test]
    fn no_probe_sent_yet_defers_to_passive_signals() {
        assert!(world_responsive(None, None, s(60), TIMEOUT),
            "no probe sent yet → no verdict → true (connected/last_packet_age_ms still stand)");
    }

    /// Exactly at the bound counts as overdue (the boundary is closed on the wedge side), so a probe
    /// sitting right at the timeout with no other proof of life reads as wedged.
    #[test]
    fn boundary_at_the_timeout_is_wedged() {
        assert!(!world_responsive(Some(TIMEOUT), None, s(60), TIMEOUT),
            "sent_ago == timeout is overdue (not `< timeout`), so it reports wedged");
    }
}
