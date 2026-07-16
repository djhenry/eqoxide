//! The net action loop: drains HTTP/IPC command slots into EQ wire packets each tick (loot,
//! doors, quests, group, trainer, zone-cross, chat, combat, merchant, …), and walks the player
//! toward a `/goto` target in capped steps at 150 ms intervals, sending movement packets and
//! notifying the render loop.

use std::time::Instant;

/// Nav tick interval (ms). Steps are gated to fire no more often than this.
const NAV_TICK_MS: u128 = 150;
/// Native Titanium base run speed in EQ units/second (runspeed 0.7 → 44 u/s; 10 Hz updates of
/// 4.4 u each). Per eq-client-expert, see docs/eq-technical-knowledgebase/player-movement-speed.md.
/// We must NOT move faster than this: even where THIS server tolerates it, others rubber-band or
/// reject motion the real client can't produce.
pub(crate) const RUN_SPEED: f32 = 44.0;
use crate::eq_net::protocol::*;
use crate::eq_net::transport::{AppPacket, EqStream};
use crate::game_state::{GameState, ZonePoint};
use crate::ipc::{AttackReq, BuyReq, SellReq, TradeReq, TradeCmd, MerchantShared, DoorClickReq, DoorsShared, MoveReq, GiveReq, InventoryShared, LootReq, MessagesShared, ChatEventsShared, ChatSendShared, CastReq, MemSpellReq, SitReq, ConsiderReq, CampReq, CampCmd, EntityIds, EntityPositions, GotoTarget, HailReq, SayReq, TargetReq, WhoReq, TaskLog, ZoneCrossReq, ZonePoints, ControllerShared, NavIntent, PosCorrection, DialogueShared, DialogueClickReq, NavStateShared};
use crate::movement::MoveIntent;

/// Min interval (ms) between OP_ClientUpdate sends while moving (native `0x118` = 280 ms).
const POS_SEND_MOVING_MS: u128 = 280;
/// Forced keepalive interval (ms) when idle (native `0x514` = 1300 ms).
const POS_SEND_KEEPALIVE_MS: u128 = 1300;
/// Interval (ms) between OP_FloatListThing (movement-history) sends. The server's MQGhost detector
/// (`cheat_manager.cpp`) trips ~70s after movement if this packet never arrives, then re-flags on
/// every movement check. Sending one benign entry every 30s keeps the 70s timer alive (eqoxide#105).
const MOVEMENT_HISTORY_MS: u128 = 30_000;

/// Build a RoF2 OP_FloatListThing payload: one `UpdateMovementEntry` (packed, 17 bytes) at the given
/// server position. `type = Collision` (1) is a normal move — it resets the server's movement-history
/// timer without tripping the TeleportA/ZoneLine special-cases in `ProcessMovementHistory`. Field
/// order matches EQEmu `UpdateMovementEntry`: Y(f32)@0, X(f32)@4, Z(f32)@8, type(u8)@12, ts(u32)@13.
pub fn build_movement_history(x: f32, y: f32, z: f32) -> Vec<u8> {
    const TYPE_COLLISION: u8 = 1; // UpdateMovementType::Collision — benign, skips teleport/zoneline checks
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0);
    let mut b = Vec::with_capacity(17);
    b.extend_from_slice(&y.to_le_bytes()); // Y @0 (server north)
    b.extend_from_slice(&x.to_le_bytes()); // X @4 (server east)
    b.extend_from_slice(&z.to_le_bytes()); // Z @8
    b.push(TYPE_COLLISION);                // type @12
    b.extend_from_slice(&ts.to_le_bytes()); // timestamp @13
    b
}
/// A >12u jump in the network gs player position between ticks that we did NOT stream is a genuine
/// server correction (anti-cheat snap / teleport), handed to the render controller to apply.
const CORRECTION_SQ: f32 = 144.0;

/// Pending state of a quest turn-in (POST /give). The trade window spans multiple nav ticks:
/// we send OP_TradeRequest, then must wait for the server's OP_TradeRequestAck before moving the
/// item into the NPC trade slot. `ticks_waiting` counts nav ticks (~150ms each) for the timeout.
struct GiveState {
    npc_id:        u32,
    ticks_waiting: u32,
}

/// ~3 second ack timeout, in nav ticks (tick gating is ~150ms → 20 ticks ≈ 3s).
const GIVE_ACK_TIMEOUT_TICKS: u32 = 20;

// Nav steering math (consts, replan/arrival decisions, pure-pursuit carrot, fast-steering cursor)
// moved to `crate::nav::steering` (cleanup step 2 — nav must not live inside net). `eq_heading`
// moved to `crate::coord` (shared by render/movement/nav alike). Everything below still needs
// them by their old bare names, so bring them in with a glob + a direct import.
use crate::nav::steering::*;
use crate::coord::eq_heading;


pub struct ActionLoop {
    goto_target:      GotoTarget,
    /// Live nav state for GET /v1/observe/debug (#166): idle|navigating|arrived|no_path|blocked.
    nav_state:        NavStateShared,
    goto_entity:      crate::ipc::GotoEntity,
    entity_positions: EntityPositions,
    entity_ids:       EntityIds,
    zone_points:      ZonePoints,
    task_log:         TaskLog,
    task_offers_shared:    crate::ipc::TaskOffersShared,
    completed_tasks_shared: crate::ipc::CompletedTasksShared,
    accept_task:           crate::ipc::AcceptTaskReq,
    cancel_task:           crate::ipc::CancelTaskReq,
    group:             crate::ipc::GroupShared,
    group_invite:      crate::ipc::GroupInviteReq,
    trainer_open_req:  crate::ipc::TrainerOpenReq,
    trainer_train_req: crate::ipc::TrainerTrainReq,
    group_accept:      crate::ipc::GroupAcceptReq,
    group_decline:     crate::ipc::GroupDeclineReq,
    group_leave:       crate::ipc::GroupLeaveReq,
    group_kick:        crate::ipc::GroupKickReq,
    group_make_leader: crate::ipc::GroupMakeLeaderReq,
    zone_cross:       ZoneCrossReq,
    hail:             HailReq,
    say:              SayReq,
    target:           TargetReq,
    /// GET /v1/observe/who registers a oneshot here; drained in `tick` to send OP_WhoAllRequest. (#300)
    who_req:          WhoReq,
    /// Held between sending the request and receiving OP_WhoAllResponse; fired by `fulfill_who`. (#300)
    pending_who:      Option<tokio::sync::oneshot::Sender<Vec<crate::game_state::WhoEntry>>>,
    /// Client-local friends list + a pending friends-presence poll, mirroring who_req/pending_who.
    /// The OP_FriendsWho reply arrives on the SAME opcode as /who all (OP_WhoAllResponse), so
    /// `expecting_friends` records that the next such reply is a friends poll, not a /who all. (#301)
    friends_list:     crate::ipc::FriendsListShared,
    friends_req:      crate::ipc::FriendsReq,
    pending_friends:  Option<tokio::sync::oneshot::Sender<Vec<crate::game_state::WhoEntry>>>,
    expecting_friends: bool,
    attack:           AttackReq,
    buy:              BuyReq,
    sell:             SellReq,
    trade:            TradeReq,
    merchant:         MerchantShared,
    move_req:         MoveReq,
    give:             GiveReq,
    cast:             CastReq,
    mem_spell:        MemSpellReq,
    sit:              SitReq,
    consider:         ConsiderReq,
    /// Manual pet command (POST /v1/pet/command or a Pet-window button): one OP_PetCommands
    /// command byte (PET_ATTACK/PET_BACKOFF/…), drained once per tick. Attack uses the current
    /// target; see the drain in `tick`.
    pet_cmd:          crate::ipc::PetCmdReq,
    /// Camp request slot, shared with the gameplay loop. The nav thread only WRITES it — when the
    /// `/camp` chat keyword is typed it pushes a `Toggle` here instead of sending the text as Say.
    camp:             CampReq,
    /// In-progress quest turn-in (POST /give), or None when idle. Drives the trade-window
    /// state machine across nav ticks (request → wait for ack → move item + accept).
    give_state:       Option<GiveState>,
    /// Shared inventory snapshot (published each tick for GET /inventory) and the pending
    /// POST /loot corpse request (drained into gs.pending_loot to reuse the auto-loot loop).
    inventory:        InventoryShared,
    loot:             LootReq,
    door_click:       DoorClickReq,
    /// Snapshot of the current zone's doors, published each tick for GET /doors.
    doors_shared:     DoorsShared,
    messages:         MessagesShared,
    /// Snapshot of the current NPC-dialogue choices (published each tick for GET
    /// /v1/observe/dialogue) and the pending POST /v1/interact/dialogue click request (drained
    /// into an OP_ItemLinkClick). (#120)
    dialogue:         DialogueShared,
    dialogue_click:   DialogueClickReq,
    chat_events:      ChatEventsShared,
    chat_send:        ChatSendShared,
    collision:        crate::nav::collision::SharedCollision,
    maps_dir:         std::path::PathBuf,
    current_zone:     String,
    last_zone_cross:  Instant,
    position_seq:     u16,
    last_tick:        Instant,
    /// Whether auto-attack is currently engaged (set by the /attack toggle). While true and a
    /// target is set, the nav thread keeps the player facing the target so melee swings land.
    auto_attack:      bool,
    /// Cached A* waypoints for the current goto goal (routes around walls). `path_i` is the
    /// current waypoint; `path_goal` is the goal these waypoints were computed for (recompute
    /// when the goal changes). Empty path = straight-line fallback.
    path:             Vec<[f32; 3]>,  // [east, north, floor_z] per waypoint
    path_i:           usize,
    path_goal:        Option<(f32, f32, f32)>,
    /// Fine LOCAL A* plan (2u grid, bounded) the walker actually steers along, re-run each tick from
    /// the player toward a carrot ~LOCAL_REACH ahead on the coarse `path`. It threads sub-8u detail
    /// (thin ramps, narrow openings) the coarse grid can't see. Empty = coarse aim (fine plan failed
    /// or no coarse path). Two-tier planner (#nav-multires).
    ///
    /// **This is now the LAST GOOD fine plan, not this tick's** (#382). It is computed on
    /// `local_planner`'s worker thread and lands a tick or two after it is posted; the walker keeps
    /// steering on the previous one meanwhile, exactly as #377 does with the coarse route. It is never
    /// waited on — see `steer_target`.
    local_path:       Vec<[f32; 3]>,
    /// Where `local_path` was planned FROM. The walker has moved since (~6.6u per tick at RUN_SPEED),
    /// which the fast-steering cursor absorbs — but a TELEPORT or a big server correction leaves the
    /// plan describing ground the character is no longer standing on, and steering along it would aim
    /// the walker at a line it isn't on. Beyond `LOCAL_BOUND` from here the plan is dropped.
    local_from:       [f32; 3],
    /// Fast-steering carrot cursor into `local_path` (#311). The fast-steering loop below re-aims
    /// every ~10ms, far more often than `local_path` is rebuilt (the 150ms gate), so — like the
    /// coarse `path_i` — it must advance as the projection passes each segment instead of staying
    /// pinned to segment 0 (where it saturates at t=1 within ~45ms at RUN_SPEED and starts measuring
    /// the carrot from a point BEHIND the walker). Reset to 0 everywhere `local_path` is rebuilt or
    /// cleared, since a stale cursor into a fresh path would just move the bug.
    local_i:          usize,
    /// The spawn id the pet was last ordered to attack (avoids re-spamming OP_PetCommands every
    /// tick). Reset when the target changes; see the auto-pet-combat block.
    last_pet_target:  Option<u32>,
    /// `Some(landing_z)` while a controlled fall is in progress (the walker descends at the native
    /// rate until reaching it, then applies fall damage); `fall_start_z` is where the fall began.
    falling:          Option<f32>,
    fall_start_z:     f32,
    /// No-progress detector for the path walker (see `nav_progress`). `stuck_best` is the
    /// closest distance reached toward the current aim, `stuck_ticks` the consecutive
    /// no-progress ticks, and `stuck_i` the `path_i` the detector is tracking (so it resets
    /// when the aim waypoint changes). Without this the walker can wedge into geometry and
    /// slide in place forever with no stop log (gfaydark/neriakc stalls, #4/#2).
    stuck_best:       f32,
    stuck_ticks:      u32,
    stuck_i:          usize,
    /// Stall-recovery re-paths WITHOUT forward progress; capped so a truly unreachable snag stops
    /// instead of re-pathing forever, but reset whenever the walker gets meaningfully closer to the
    /// goal — so a long cross-zone journey that clears several distinct wedges isn't killed by the
    /// cap while it's still making progress (#229).
    nav_repaths:      u32,
    /// Closest straight-line distance to the current goal reached so far; when it drops by
    /// `REPATH_RESET_DIST` the re-path budget resets (real progress → the last wedge is behind us).
    nav_best_gdist:   f32,
    /// Downhill back-off (eqoxide#212): when the walker wedges on a slope face, drive the reverse
    /// direction for this many ticks before re-pathing, so the re-plan starts from cleaner ground.
    /// 0 = not backing off.
    backoff_ticks:    u32,
    backoff_dir:      [f32; 2],
    /// Proactive coarse re-plan (#246). The coarse 8u route is committed at goal-change and, without
    /// this, only re-planned on a ~3s no-progress stall — so an obstacle the coarse grid skims but the
    /// fine 2u planner can't thread makes the walker press into it for seconds while the overlay (which
    /// re-plans continuously) already shows a clean detour. `local_stuck_ticks` counts consecutive fine
    /// plans that came back `NoWayThrough` (the 40u window CLOSED without a way to the carrot); after a
    /// few, `replan_coarse` is armed so the next tick re-plans the coarse route from the CURRENT
    /// position (routing around BEFORE the stall). `replan_cooldown` throttles those so they don't thrash.
    ///
    /// **Only `NoWayThrough` counts (#382, `arms_coarse_replan`).** It used to count any fine plan that
    /// fell short of the carrot — which, under the old 150 ms wall clock, INCLUDED every plan that
    /// merely ran out of time. A timeout was therefore laundered into "the coarse route ahead is
    /// blocked", and under CPU load that re-planned corridors that were perfectly threadable.
    local_stuck_ticks: u32,
    replan_coarse:     bool,
    replan_cooldown:   u32,
    /// How many PROACTIVE coarse re-plans (#246) have fired since the journey last made real
    /// progress. This is the oscillation guard (#378 Phase 2). Each proactive re-plan installs a
    /// fresh coarse route, which `apply_plan` resets `path_i`/`stuck_i` for — so the stall clock
    /// (`stuck_ticks`) never accumulates its 20-tick give-up and `nav_repaths` (bumped only by the
    /// stall path) never climbs. Without a counter of its OWN, a spot the fine tier cannot thread
    /// but the coarse tier keeps "re-routing" around loops `navigating` FOREVER — the live qcat
    /// L-corner. Capped at [`PROACTIVE_REPLAN_CAP`]; reset on real goal-ward progress (like
    /// `nav_repaths`). At the cap the walker stops honestly with `blocked / local_no_way_through`.
    proactive_replans: u32,
    /// Auto-escape a SEALED interior via an in-zone teleport (#266). When a /goto goal is walk-
    /// unreachable and the nearest zone-line region is a translocator that loops back to THIS zone (the
    /// Qeynos guild-vault waterfall), the goto is temporarily redirected to that region — the char
    /// walks in via the normal machinery, the auto-cross teleports it out, and the post-teleport jump
    /// restores the real goal. `escape_return` holds the real goal while escaping; `last_walk_pos`
    /// detects the teleport jump; `portal_cooldown` blocks an immediate re-escape so a still-unreachable
    /// goal can't ping-pong through the portal forever.
    escape_return:     Option<(f32, f32, f32)>,
    last_walk_pos:     [f32; 3],
    portal_cooldown:   u32,
    /// Single-authority controller integration (design §2). `controller_view` is the render
    /// thread's authoritative position snapshot we stream to the server; `nav_intent` is the
    /// `/goto` planner's per-frame wish written for the render controller; `pos_correction` hands a
    /// genuine server correction back to the controller.
    controller_view:  ControllerShared,
    nav_intent:       NavIntent,
    pos_correction:   PosCorrection,
    /// Draw-only mirror of the walker's committed `path`/`local_path`, published each tick for the
    /// nav-debug overlay so it shows what the walker actually follows, not a separate recompute (#246).
    nav_path_view:    crate::ipc::NavPathView,
    /// Aggro-avoidance knobs from /v1/move/* (#242): whether to route around NPC camps and how wide a
    /// buffer to give them. Read each time a route is (re)planned.
    nav_avoid:        crate::ipc::NavAvoidShared,
    /// POST /v1/interact/read request: the inventory wire slot of a book/note to read (#288). Drained
    /// each tick; the item's Filename is sent as OP_ReadBook and the server replies with the text.
    read_book:        crate::ipc::ReadBookReq,
    /// Guild roster + identity published each tick for GET /v1/guild/roster + /observe/debug (#295).
    guild:            crate::ipc::GuildShared,
    /// POST /v1/guild/{invite,accept,leave,remove} — one queued guild action, drained each tick (#295).
    guild_action:     crate::ipc::GuildActionReq,
    /// Last time we sent OP_FloatListThing (movement history) — the anti-MQGhost keepalive (#105).
    last_movement_history_send: Instant,
    /// Last position we streamed, and the last-send timestamp (for the 280 ms / 1300 ms cadence).
    last_streamed:    [f32; 3],
    last_pos_send:    Instant,
    streamed_init:    bool,
    /// The PATHFINDING WORKER (#340). Coarse A* plans are POSTED here and picked up on a later tick;
    /// the net thread never blocks on a search. See `crate::nav::planner`.
    planner:          crate::nav::planner::Planner,
    /// The FINE-TIER WORKER (#382). The 2u/40u steering plan is posted here EVERY nav tick and picked
    /// up a tick or two later. It was the last A* left on the network thread, and the last search in
    /// the client under a wall-clock budget.
    ///
    /// **Nothing ever waits on it.** There is deliberately no `awaiting_first_local_plan` mirroring
    /// `awaiting_first_plan`: the coarse tier may legitimately stand the character still (driving with
    /// no route at all would charge it through geometry), but the fine tier only ever REFINES an aim
    /// the coarse route already provides, so its absence degrades steering rather than blocking it.
    local_planner:    crate::nav::planner::LocalPlanner,
    /// The planner SNAPPED the current goal's z to a floor the caller never named (see
    /// `Collision::goal_z_was_snapped`). Carried to ARRIVAL, so the agent is not simply told
    /// `arrived` as though it got the goal it asked for — it did not.
    goal_snapped: bool,
    /// True while a plan is in flight for a goal we have NO route for yet — the walker must stand
    /// still rather than straight-line into geometry. (A re-plan of a goal we already have a route
    /// for keeps walking the old route while the new one computes.)
    awaiting_first_plan: bool,
}

impl ActionLoop {
    pub fn new(
        goto_target:      GotoTarget,
        nav_state:        NavStateShared,
        goto_entity:      crate::ipc::GotoEntity,
        entity_positions: EntityPositions,
        entity_ids:       EntityIds,
        zone_points:      ZonePoints,
        task_log:         TaskLog,
        task_offers_shared:    crate::ipc::TaskOffersShared,
        completed_tasks_shared: crate::ipc::CompletedTasksShared,
        accept_task:           crate::ipc::AcceptTaskReq,
        cancel_task:           crate::ipc::CancelTaskReq,
        group:             crate::ipc::GroupShared,
        group_invite:      crate::ipc::GroupInviteReq,
    trainer_open_req:  crate::ipc::TrainerOpenReq,
    trainer_train_req: crate::ipc::TrainerTrainReq,
        group_accept:      crate::ipc::GroupAcceptReq,
        group_decline:     crate::ipc::GroupDeclineReq,
        group_leave:       crate::ipc::GroupLeaveReq,
        group_kick:        crate::ipc::GroupKickReq,
        group_make_leader: crate::ipc::GroupMakeLeaderReq,
        zone_cross:       ZoneCrossReq,
        hail:             HailReq,
        say:              SayReq,
        target:           TargetReq,
        who_req:          WhoReq,
        friends_list:     crate::ipc::FriendsListShared,
        friends_req:      crate::ipc::FriendsReq,
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
        pet_cmd:          crate::ipc::PetCmdReq,
        collision:        crate::nav::collision::SharedCollision,
        maps_dir:         std::path::PathBuf,
        camp:             CampReq,
        controller_view:  ControllerShared,
        nav_intent:       NavIntent,
        pos_correction:   PosCorrection,
        nav_path_view:    crate::ipc::NavPathView,
        nav_avoid:        crate::ipc::NavAvoidShared,
        read_book:        crate::ipc::ReadBookReq,
        guild:            crate::ipc::GuildShared,
        guild_action:     crate::ipc::GuildActionReq,
    ) -> Self {
        ActionLoop {
            goto_target,
            nav_state,
            goto_entity,
            entity_positions,
            entity_ids,
            zone_points,
            task_log,
            task_offers_shared,
            completed_tasks_shared,
            accept_task,
            cancel_task,
            group,
            group_invite,
            trainer_open_req,
            trainer_train_req,
            group_accept,
            group_decline,
            group_leave,
            group_kick,
            group_make_leader,
            zone_cross,
            hail,
            say,
            target,
            who_req,
            pending_who: None,
            friends_list,
            friends_req,
            pending_friends: None,
            expecting_friends: false,
            attack,
            buy,
            sell,
            trade,
            merchant,
            move_req,
            give,
            cast,
            mem_spell,
            sit,
            consider,
            pet_cmd,
            camp,
            give_state: None,
            inventory,
            loot,
            door_click,
            doors_shared,
            messages,
            dialogue,
            dialogue_click,
            chat_events,
            chat_send,
            collision,
            maps_dir,
            current_zone: String::new(),
            last_zone_cross: Instant::now(),
            position_seq: 0,
            last_tick: Instant::now(),
            auto_attack: false,
            path: Vec::new(),
            path_i: 0,
            path_goal: None,
            local_path: Vec::new(),
            local_i: 0,
            local_from: [0.0, 0.0, 0.0],
            last_pet_target: None,
            falling: None,
            fall_start_z: 0.0,
            stuck_best: f32::MAX,
            stuck_ticks: 0,
            stuck_i: 0,
            nav_repaths: 0,
            proactive_replans: 0,
            nav_best_gdist: f32::MAX,
            backoff_ticks: 0,
            local_stuck_ticks: 0,
            replan_coarse: false,
            replan_cooldown: 0,
            escape_return: None,
            last_walk_pos: [0.0, 0.0, 0.0],
            portal_cooldown: 0,
            backoff_dir: [0.0, 0.0],
            controller_view,
            nav_intent,
            pos_correction,
            nav_path_view,
            nav_avoid,
            read_book,
            guild,
            guild_action,
            last_streamed: [0.0, 0.0, 0.0],
            last_pos_send: Instant::now(),
            last_movement_history_send: Instant::now(),
            streamed_init: false,
            planner: crate::nav::planner::Planner::spawn(),
            local_planner: crate::nav::planner::LocalPlanner::spawn(),
            goal_snapped: false,
            awaiting_first_plan: false,
        }
    }

    /// Drop the fine plan and forget the fine tier's last word. Called wherever the ground the plan
    /// describes stops being ground we are standing on — a new destination, a teleport, a stop.
    fn clear_local_plan(&mut self) {
        self.local_path.clear();
        self.local_i = 0;
        self.local_stuck_ticks = 0;
        self.local_planner.cancel();
        self.set_nav_local(None);
    }

    /// Did the FINE tier last say the corridor ahead is genuinely not threadable? Read from the
    /// published field rather than a shadow copy, so what steers the walker and what the agent is told
    /// cannot drift apart.
    fn local_says_no_way_through(&self) -> bool {
        self.nav_state.lock().unwrap().local.as_ref().is_some_and(|l| l.state == "no_way_through")
    }

    /// Copy all entity positions from `gs` into the shared entity map
    /// (used by the HTTP /entities endpoint and /goto by-name lookup).
    pub fn sync_entities(&self, gs: &GameState) {
        let mut map = self.entity_positions.lock().unwrap();
        let mut ids = self.entity_ids.lock().unwrap();
        // Full replace: clear stale entries so positions reflect the current zone only.
        map.clear();
        ids.clear();
        for (&id, e) in &gs.entities {
            map.insert(e.name.clone(), (e.x, e.y, e.z));
            ids.insert(e.name.clone(), id);
        }
    }

    /// Publish the native Task-system quest log from `gs` into the shared slot (GET /quests/log).
    pub fn sync_tasks(&self, gs: &GameState) {
        let mut log = self.task_log.lock().unwrap();
        log.clear();
        let mut tasks: Vec<_> = gs.tasks.values().cloned().collect();
        tasks.sort_by_key(|t| t.task_id);
        log.extend(tasks);
        drop(log);

        let mut offers = self.task_offers_shared.lock().unwrap();
        offers.clear();
        offers.extend(gs.task_offers.iter().cloned());
        drop(offers);

        let mut completed = self.completed_tasks_shared.lock().unwrap();
        completed.clear();
        completed.extend(gs.completed_task_history.iter().cloned());
    }

    /// Publish the group roster from `gs` into the shared slot (GET /v1/group/roster + the UI
    /// roster panel). Looks up each other member's HP% from `gs.entities` by name (group
    /// membership is what unlocks receiving another mob's OP_MobHealth percent, so this reuses
    /// existing Entity.hp_pct rather than needing a new opcode); the player's own HP% comes
    /// directly from `gs.hp_pct` since the player is never in `gs.entities`.
    pub fn sync_group(&self, gs: &GameState) {
        let mut g = self.group.lock().unwrap();
        g.leader = gs.group_leader.clone();
        g.pending_invite = gs.pending_invite.clone();
        g.you_are_leader = !gs.player_name.is_empty() && gs.group_leader == gs.player_name;
        g.members = gs.group_members.iter().map(|m| {
            let hp_pct = if m.name == gs.player_name {
                gs.hp_pct
            } else {
                gs.entities.values().find(|e| e.name == m.name).map(|e| e.hp_pct).unwrap_or(0.0)
            };
            crate::ipc::GroupMemberView {
                // m.level from OP_GroupUpdateB is a server placeholder (70/65); resolve the real
                // level from our profile / the member's spawn instead. (eqoxide#104)
                name: m.name.clone(), level: gs.group_member_level(&m.name),
                is_leader: m.is_leader, is_merc: m.is_merc,
                tank: m.tank, assist: m.assist, puller: m.puller, offline: m.offline, hp_pct,
            }
        }).collect();
    }

    /// Publish the player's guild identity + roster from `gs` into the shared slot (GET
    /// /v1/guild/roster and the guild fields of /observe/debug). Resolves guild_id → name via the
    /// OP_GuildsList table. (#295)
    pub fn sync_guild(&self, gs: &GameState) {
        let mut g = self.guild.lock().unwrap();
        // GUILD_NONE is 0xFFFFFFFF (and 0 also means none). Normalize both to 0 so the API cleanly
        // reports "no guild" as guild_id 0 / empty name / empty roster.
        let in_guild = gs.player_guild_id != 0 && gs.player_guild_id != 0xFFFF_FFFF;
        if in_guild {
            g.guild_id = gs.player_guild_id;
            g.guild_rank = gs.player_guild_rank;
            g.guild_name = gs.guild_names.get(&gs.player_guild_id).cloned().unwrap_or_default();
            g.members = gs.guild_members.clone();
        } else {
            g.guild_id = 0;
            g.guild_rank = 0;
            g.guild_name.clear();
            g.members.clear();
        }
        g.pending_invite = gs.pending_guild_invite.as_ref().map(|(inviter, _, _)| inviter.clone());
    }

    /// Publish the player's inventory + equipment from `gs` into the shared slot (GET /inventory).
    pub fn sync_inventory(&self, gs: &GameState) {
        let mut inv = self.inventory.lock().unwrap();
        inv.clear();
        inv.extend(gs.inventory.iter().cloned());
    }

    /// Deliver the freshly-parsed `/who all` roster to the pending GET /v1/observe/who (#300). Called
    /// from the gameplay drain loop right after an OP_WhoAllResponse updates `gs.who_roster`. No-op if
    /// no request is in flight (e.g. an unsolicited/duplicate response).
    pub fn fulfill_who(&mut self, gs: &GameState) {
        if let Some(tx) = self.pending_who.take() {
            let _ = tx.send(gs.who_roster.clone());
        }
    }

    /// True when the next OP_WhoAllResponse should be treated as an OP_FriendsWho reply (a friends
    /// poll) rather than a /who all — so the gameplay loop routes it to `fulfill_friends`. (#301)
    pub fn expecting_friends(&self) -> bool { self.expecting_friends }

    /// Deliver the friends-presence reply (the online subset, parsed into `gs.who_roster` by
    /// `apply_who_all`) to the pending GET /v1/social/friends. Mirrors `fulfill_who`. (#301)
    pub fn fulfill_friends(&mut self, gs: &GameState) {
        if let Some(tx) = self.pending_friends.take() {
            let _ = tx.send(gs.who_roster.clone());
        }
        self.expecting_friends = false;
    }

    /// Publish the open-merchant session from `gs` into the shared slot (GET /trade/list + the HUD
    /// merchant window).
    pub fn sync_merchant(&self, gs: &GameState) {
        let mut m = self.merchant.lock().unwrap();
        m.open = gs.merchant_open.is_some();
        m.merchant_id = gs.merchant_open;
        m.items.clear();
        m.items.extend(gs.merchant_items.iter().cloned());
    }

    /// Publish the in-game message log from `gs` into the shared slot (GET /messages), converting
    /// each LogEntry into a serializable MessageEntry and extracting `[bracketed]` quest keywords
    /// (the same splitter the HUD dialogue panel uses).
    pub fn sync_messages(&self, gs: &GameState) {
        let mut out = self.messages.lock().unwrap();
        out.clear();
        out.extend(gs.messages.iter().map(|m| {
            let keywords = crate::game_state::split_keywords(&m.text).into_iter()
                .filter(|(_, is_kw)| *is_kw)
                .map(|(seg, _)| seg.trim_matches(|c| c == '[' || c == ']').trim().to_string())
                .filter(|k| !k.is_empty())
                .collect();
            crate::ipc::MessageEntry { kind: m.kind.clone(), text: m.text.clone(), keywords }
        }));
        drop(out);
        // Publish the current clickable NPC-dialogue choices (GET /v1/observe/dialogue, #120).
        *self.dialogue.lock().unwrap() = gs.dialogue_choices.clone();
        // Publish async events (GET /v1/events/*), preserving their stable monotonic ids.
        let mut ev = self.chat_events.lock().unwrap();
        ev.clear();
        ev.extend(gs.chat_events.iter().map(|e| crate::ipc::Event {
            id: e.id, category: e.category.clone(), kind: e.kind.clone(),
            from: e.from.clone(), directed: e.directed, text: e.text.clone(),
        }));
    }

    /// Publish the current zone's doors from `gs` into the shared slot (GET /doors).
    pub fn sync_doors(&self, gs: &GameState) {
        let mut out = self.doors_shared.lock().unwrap();
        out.clear();
        out.extend(gs.doors.values().map(|d| crate::ipc::DoorView {
            door_id: d.door_id, name: d.name.clone(),
            x: d.x, y: d.y, z: d.z, heading: d.heading,
            opentype: d.opentype, is_open: d.is_open,
        }));
    }

    /// Sync zone exit points from `gs` into the shared zone_points map.
    /// On zone change, also loads map-label exits from disk as fallback zone points.
    pub fn sync_zone_points(&mut self, gs: &GameState) {
        // On zone change, load map labels from disk as fallback zone points.
        if gs.zone_name != self.current_zone {
            self.current_zone = gs.zone_name.clone();

            // Reset the nav destination + route on a zone change (#248). The old goal/path are in the
            // PREVIOUS zone's coordinate space; kept across a crossing they aim the walker at an
            // arbitrary spot (usually a corner near the arrival point) and wedge it there. A completed
            // crossing IS the "walk to the zone line" goal reached, so the character should come to
            // rest in the new zone; a driver that wants to keep going re-issues /v1/move/* afterward.
            // (This is the zone-boundary sibling of the mid-zone stale-plan bug #246.)
            *self.goto_target.lock().unwrap() = None;
            *self.goto_entity.lock().unwrap() = None;
            *self.nav_intent.lock().unwrap() = None; // stop driving the controller toward the stale aim
            *self.nav_path_view.lock().unwrap() = (Vec::new(), Vec::new()); // clear the overlay line
            self.path.clear();
            self.local_path.clear();
            self.local_i = 0;
            self.path_goal = None;
            self.path_i = 0;
            self.stuck_i = 0;
            self.stuck_best = f32::MAX;
            self.stuck_ticks = 0;
            self.nav_repaths = 0;
            self.proactive_replans = 0;
            self.nav_best_gdist = f32::MAX;
            self.backoff_ticks = 0;
            self.local_stuck_ticks = 0;
            self.replan_coarse = false;
            self.replan_cooldown = 0;
            self.falling = None;
            // A plan in flight was computed against the PREVIOUS zone's collision grid and its
            // coordinate space. Abandon it — applying it here would drive the character at a route
            // through a zone it is no longer in.
            self.planner.cancel();
            self.awaiting_first_plan = false;
            self.set_nav_state("idle");
            self.nav_state.lock().unwrap().tier = None; // no route committed → no per-route tier

            let mut shared = self.zone_points.lock().unwrap();
            // Start fresh with server entries.
            shared.clear();
            shared.extend(gs.zone_points.iter().cloned());
            // Load map labels from disk.
            if let Some(zm) = crate::zone_map::ZoneMap::load(&self.maps_dir, &gs.zone_name) {
                let before = shared.len();
                for label in &zm.labels {
                    let lower = label.text.to_lowercase();
                    if !lower.starts_with("to ") { continue; }
                    let dest_zone_id: u16 = if lower.contains("north qeynos") || lower.contains("qeynos2") {
                        2
                    } else if lower.contains("south qeynos") {
                        1 // qeynos south
                    } else {
                        0
                    };
                    if dest_zone_id == 0 { continue; }
                    let dup = shared.iter().any(|zp| {
                        zp.zone_id == dest_zone_id
                            && ((zp.server_x - label.east).powi(2) + (zp.server_y - label.north).powi(2)) < 2500.0
                    });
                    if dup { continue; }
                    shared.push(ZonePoint {
                        iterator: u32::MAX,
                        server_x: label.east,
                        server_y: label.north,
                        server_z: 0.0,
                        heading: 0.0,
                        zone_id: dest_zone_id,
                    });
                    tracing::info!("zone_map: added exit '{}' at ({:.1}, {:.1}) → zone_id={}",
                              label.text, label.east, label.north, dest_zone_id);
                }
                if shared.len() > before {
                    tracing::info!("zone_map: {} fallback exits added (total {})", shared.len() - before, shared.len());
                }
            }
        } else {
            // Same zone: update server entries but keep map labels.
            let mut shared = self.zone_points.lock().unwrap();
            let map_labels: Vec<_> = shared.drain(..)
                .filter(|zp| zp.iterator == u32::MAX)
                .collect();
            shared.extend(gs.zone_points.iter().cloned());
            shared.extend(map_labels);
        }
    }

    /// Publish the current `/move/goto` navigation state for GET /v1/observe/debug (#166, #337).
    /// The value set is an AGENT-FACING CONTRACT — every value is documented in `docs/http-api.md`:
    ///
    ///   idle | planning | navigating | navigating_partial | following | arrived
    ///   | no_path | search_exhausted | blocked
    ///
    /// `reason` is the machine-readable WHY behind a terminal state. A terminal state without one
    /// (the old bare `blocked`) tells the agent nothing, which is how an unreachable goal spent
    /// months masquerading as a mysterious wedge.
    fn set_nav_state(&self, state: &str) { self.set_nav_state_because(state, None); }

    /// Set the walker's state + reason. **Deliberately does NOT touch `local`** — the fine tier's
    /// last word is an independent fact about a different tier, and clobbering it here would mean
    /// every ordinary state transition silently erased the one field that says whether the tier
    /// steering the character can see a way through (#382).
    fn set_nav_state_because(&self, state: &str, reason: Option<&str>) {
        let mut s = self.nav_state.lock().unwrap();
        let reason = reason.map(str::to_string);
        if s.state != state || s.reason != reason {
            s.state = state.to_string();
            s.reason = reason;
            // A state transition retires the previous route's per-instance facts (#378 Phase 2,
            // #343 discipline): the `nav_blocked_by` payload belongs to ONE `no_path`, and `nav_tier`
            // belongs to ONE committed route. Leaving either set across a transition — e.g. a
            // "preferred" tier from an ARRIVED journey A surviving into journey B's `no_path`, or an
            // Exhausted `navigating_partial` reading a stale tier — is exactly the `connected:true`
            // stale-field lie. Cleared here on EVERY transition; the Route arm of `apply_plan`
            // re-sets `tier` immediately after (it writes it AFTER this call), and `stop_nav_blocked`
            // re-sets the blockage immediately after, so a genuinely-current fact is never lost.
            s.blocked_goal = None;
            s.blocked_frontier = None;
            s.tier = None;
        }
    }

    /// Publish the FINE tier's last honest outcome (#382). Never touches `state`/`reason`.
    fn set_nav_local(&self, local: Option<crate::ipc::NavLocal>) {
        let mut s = self.nav_state.lock().unwrap();
        if s.local != local { s.local = local; }
    }

    /// Read the current nav state word (without the reason).
    fn nav_state_is(&self, state: &str) -> bool {
        self.nav_state.lock().unwrap().state == state
    }

    /// Stop navigating and report WHY, loudly, in every channel an agent can see: the nav state, a
    /// machine-readable reason, the message log, and the trace. A nav failure that says nothing is
    /// the worst failure mode this client has — it cost the project months (#337).
    fn stop_nav(&mut self, gs: &mut GameState, state: &str, reason: &str, msg: &str) {
        self.stop_nav_blocked(gs, state, reason, None, None, msg);
    }

    /// [`stop_nav`], additionally publishing the agent-honesty blockage payload (#378 Phase 2). The
    /// two `Blockage`es come from the COLD diagnosis inside `PlanOutcome::Unreachable`; they are set
    /// on `NavStatus` (surfaced as `nav_blocked_by` on /v1/observe/debug) so an agent handed a
    /// terminal `no_path` learns WHAT stopped it and WHERE, not just that it stopped.
    fn stop_nav_blocked(&mut self, gs: &mut GameState, state: &str, reason: &str,
        goal_blk: Option<crate::traversability::Blockage>,
        frontier_blk: Option<crate::traversability::Blockage>, msg: &str)
    {
        tracing::warn!("NAV: {msg}");
        gs.log_msg("zone", msg);
        self.set_nav_state_because(state, Some(reason));
        // Publish the blockage AFTER the state (set_nav_state_because clears it on transition).
        let to_nav = |b: crate::traversability::Blockage| crate::ipc::NavBlockage {
            hazard: b.hazard.as_str(), at: b.at };
        {
            let mut s = self.nav_state.lock().unwrap();
            s.blocked_goal = goal_blk.map(to_nav);
            s.blocked_frontier = frontier_blk.map(to_nav);
        }
        self.path.clear();
        // Drop the fine PLAN, but deliberately KEEP the fine tier's last word (`nav_local`) — it is
        // the evidence behind a terminal `blocked`, and an agent reading the outcome of a failed goto
        // needs to see whether the corridor was proven unthreadable or whether the fine tier merely
        // stopped looking. It is cleared when a new route is committed (`clear_local_plan`).
        self.local_path.clear();
        self.local_i = 0;
        self.local_stuck_ticks = 0;
        self.local_planner.cancel();
        self.path_goal = None;
        self.planner.cancel();
        self.awaiting_first_plan = false;
        *self.goto_target.lock().unwrap() = None;
        *self.nav_intent.lock().unwrap() = None;
    }

    /// Apply a finished FINE plan from the local worker (#382).
    ///
    /// Three things happen here, and the second is the one #382 is about:
    ///
    /// 1. **Install the steer path.** Every outcome carries one — a complete fine route, or the best
    ///    partial toward the carrot. Even a `NoWayThrough` partial is installed: it is a steering hint,
    ///    not a route proposal, and wiping it is what stranded the halas swimmer at the shoreline
    ///    (#377 review, N1). The walker was steering on the PREVIOUS fine plan until this moment; it
    ///    never waited.
    ///
    /// 2. **Arm the proactive coarse re-plan ONLY on a CLOSED window** (`arms_coarse_replan`). The old
    ///    code armed it whenever the fine path merely fell short of the carrot — which, under the
    ///    150 ms wall clock this PR deletes, included every plan that simply ran out of time. A
    ///    timeout was therefore laundered into "the coarse route ahead is blocked", and under CPU load
    ///    it re-planned corridors that were perfectly threadable. Now `Exhausted` ("I stopped looking")
    ///    and `NoWayThrough` ("I looked at all of it; there is no way") are different values and only
    ///    the second is evidence of anything.
    ///
    /// 3. **Publish what the fine tier actually said** (`nav_local`), so an agent watching a character
    ///    grind against a doorway can tell "the corridor is genuinely not threadable" from "the
    ///    steering planner is not keeping up" — instead of reading a confident `nav_state: navigating`
    ///    and nothing else, which is what it got before.
    ///
    /// The carrot is judged against `reply.goal` — the carrot the plan was actually FOR — not against
    /// today's carrot, which has slid a few units since the request was posted.
    fn apply_local_plan(&mut self, reply: crate::nav::planner::LocalReply) {
        let outcome = reply.outcome;
        self.local_path = outcome.steer().to_vec();
        self.local_from = reply.start;
        // A fresh plan starts at `reply.start`, a point the walker has already driven past. Zero the
        // cursor and let `steer_target`'s projection advance it onto the segment the walker is really
        // on (#311) — a stale index into a new path would just move the bug.
        self.local_i = 0;

        // Only re-plan proactively while the walker is otherwise moving HEALTHILY: the point is to
        // detour BEFORE bonking. Once it's genuinely wedged (in a back-off, or the stall clock is
        // already climbing), the existing stall/back-off recovery owns it.
        let healthy = self.backoff_ticks == 0 && self.stuck_ticks < NAV_HOP_TICKS;
        if arms_coarse_replan(&outcome) && healthy && self.replan_cooldown == 0 {
            self.local_stuck_ticks += 1;
            if self.local_stuck_ticks >= NAV_LOCAL_STUCK_TICKS {
                self.replan_coarse = true;
                // Count each armed proactive re-plan toward the oscillation budget (#378 Phase 2).
                // `tick` resets this on real progress and terminates honestly at the cap, so a spot
                // the fine tier cannot thread can no longer loop `navigating` forever (qcat L-corner).
                self.proactive_replans += 1;
                tracing::debug!("NAV: fine plan CLOSED its window short of the carrot near ({:.0},{:.0}) \
                    ({}) — re-planning coarse (#246, proactive #{})", reply.start[0], reply.start[1],
                    outcome.reason(), self.proactive_replans);
            }
        } else if outcome.threaded() {
            self.local_stuck_ticks = 0;
        }
        // `Exhausted` deliberately does NEITHER: it is not evidence the corridor is blocked (so it must
        // not count toward a re-plan) and it is not evidence it is clear (so it must not reset the
        // count either). "I don't know" changes nothing.

        self.set_nav_local(Some(crate::ipc::NavLocal {
            state:       outcome.state().to_string(),
            reason:      outcome.reason().to_string(),
            stuck_ticks: self.local_stuck_ticks,
            plan_us:     reply.plan_us as u64,
        }));
    }

    /// Apply a finished plan from the worker thread. Returns `true` when the tick must STOP here —
    /// the plan was terminal (no route / gave up) or redirected the goto through a portal.
    ///
    /// This is where the three honest outcomes become three DISTINGUISHABLE agent-facing states. The
    /// old code had one: it walked a complete route and a timed-out partial route identically, and
    /// when the partial ran into a wall it froze at `blocked` and said nothing at all (#337).
    fn apply_plan(
        &mut self,
        reply: crate::nav::planner::PlanReply,
        gs: &mut GameState,
        goal: (f32, f32, f32),
    ) -> bool {
        use crate::nav::collision::PlanOutcome;
        self.awaiting_first_plan = false;
        // The in-flight goal lives INSIDE the Planner now and is cleared by `poll` the moment the
        // reply is handed over, so a consumed-but-dropped reply can no longer wedge the planner
        // permanently at `nav_state: planning`. That state is unrepresentable — see `Planner::pending`.
        // Did the planner CHANGE the goal we asked for? Say so — an agent that asked for z=0 and is
        // walked to z=47 must not simply be told `arrived` as if it got what it requested.
        let snapped = reply.goal_snapped_z;
        self.goal_snapped = snapped.is_some();
        if let Some(z) = snapped {
            gs.log_msg("zone", &format!(
                "Goal z={:.0} is not on any floor — routing to the floor at z={:.0} instead (the client \
                 CHANGED your goal; it is not the one you gave).", goal.2, z));
        }
        match reply.outcome {
            // A real, complete route to the goal. The only outcome the walker may treat as a plan.
            PlanOutcome::Route(path) => {
                tracing::info!("NAV: plan #{} → ROUTE to ({:.0},{:.0}) = {} waypoints ({}ms, off the net thread)",
                    reply.gen, goal.0, goal.1, path.len(), reply.plan_ms);
                self.path = path;
                self.path_i = 0;
                self.stuck_i = 0;
                // The fine plan is a REFINEMENT OF A SPECIFIC COARSE ROUTE — it threads a carrot on it.
                // Replace the route and the refinement is void: steering on it would follow the OLD
                // route's corridor. Inline planning hid this (the fine plan was rebuilt from the new
                // route the same tick); now it persists across ticks, so it must be dropped explicitly.
                // The walker steers on the coarse carrot for the ~1 tick until the next fine plan lands.
                self.clear_local_plan();
                match snapped {
                    // Navigating, but NOT to the goal as given — the agent can see that in nav_reason.
                    Some(_) => self.set_nav_state_because("navigating", Some("goal_z_snapped")),
                    None    => self.set_nav_state("navigating"),
                }
                // Publish the PER-ROUTE tier (#378 Phase 2 / design §4c): `minimum` = this route
                // only existed at the character's own collision radius (a tight door/bridge, no
                // margin — riskier), `preferred` = the roomy tier carried it. The agent sees the
                // risk of the route it is actually walking, not a zone-lifetime aggregate.
                self.nav_state.lock().unwrap().tier =
                    Some(if reply.tight { "minimum" } else { "preferred" });
                false
            }
            // The search was CUT SHORT — "I don't know", not "no route". It did close real ground
            // toward the goal (`PARTIAL_MIN_UNITS`), so walk that stage and re-plan from its end.
            // Reported as its OWN state: an agent must be able to tell "I have a route to your goal"
            // from "I am walking toward a frontier and hoping" — conflating those is the #337 lie.
            PlanOutcome::Exhausted { limit, progress: Some(path) } => {
                tracing::warn!("NAV: plan #{} → EXHAUSTED ({}) after {}ms — walking a PARTIAL route ({} wp) toward \
                    ({:.0},{:.0}) and re-planning from its end. This is NOT a route to the goal.",
                    reply.gen, limit.as_str(), reply.plan_ms, path.len(), goal.0, goal.1);
                gs.log_msg("zone", "Planner gave up before finding a full route — walking as far as it can, then re-planning");
                self.path = path;
                self.path_i = 0;
                self.stuck_i = 0;
                self.clear_local_plan(); // same: a new coarse route voids the fine plan that refined the old one
                self.set_nav_state_because("navigating_partial", Some(limit.as_str()));
                false
            }
            // Gave up with nothing usable. Honest "I DON'T KNOW" — deliberately NOT `no_path`, which
            // would be claiming a certainty we do not have.
            PlanOutcome::Exhausted { limit, progress: None } => {
                self.stop_nav(gs, "search_exhausted", limit.as_str(), &format!(
                    "Path search to ({:.0},{:.0}) GAVE UP ({}) after {}ms with no usable route. This is not \
                     'no route exists' — the search never finished. Try a nearer waypoint.",
                    goal.0, goal.1, limit.as_str(), reply.plan_ms));
                true
            }
            // DEFINITIVE: no route exists.
            PlanOutcome::Unreachable { reason: why, goal_blocked_by, frontier_blocked_by } => {
                // ...unless the only way out is an in-zone translocator (the Qeynos guild-vault
                // waterfall): REDIRECT the goto to it — the char walks in via the normal machinery,
                // the auto-cross teleports it out, and the post-teleport jump restores the real goal
                // (#266). Previously this hung off "the route came back partial"; an honest
                // Unreachable is a strictly better signal for it.
                //
                // But ONLY when a teleport could conceivably help: the goal is walkable and we are
                // walled off from it. A goal with NO FLOOR UNDER IT is not somewhere a portal can
                // take you, and redirecting there does real harm — the agent asked for goal X, got
                // silently re-aimed at a portal, and was then told `no_path: search_closed`, which is
                // the PORTAL's reason, not theirs. Their goal's TRUE reason (`goal_not_walkable`) never
                // reached them. Same family of lie as the rest of this PR, so: no escape, and the
                // reason the agent gets is the reason for the goal they actually asked about.
                if portal_escape_applies(why) && self.escape_return.is_none() && self.portal_cooldown == 0 {
                    if let Some(portal) = self.find_in_zone_portal(gs) {
                        tracing::info!("NAV: goal ({:.0},{:.0}) is UNREACHABLE by walking ({}) — escaping the sealed area \
                            via the in-zone teleport at ({:.0},{:.0}) (#266)",
                            goal.0, goal.1, why.as_str(), portal.0, portal.1);
                        self.escape_return = Some(goal);
                        *self.goto_target.lock().unwrap() = Some(portal);
                        self.portal_cooldown = PORTAL_COOLDOWN_TICKS;
                        self.path_goal = None; // re-plan to the portal next tick
                        *self.nav_intent.lock().unwrap() = None;
                        return true;
                    }
                }
                // The agent-honesty payload (#378 Phase 2): name WHAT is blocking and WHERE, so an
                // agent gets more than a bare `search_closed`. `goal_blocked_by` is the definitive
                // "your goal itself is impossible"; `frontier_blocked_by` is "I got as close as here
                // and THIS is the obstruction". Both are surfaced on /v1/observe/debug alongside the
                // reason; a missing diagnosis stays absent (honest silence, never invented).
                let blk = goal_blocked_by.or(frontier_blocked_by);
                let detail = blk.map(|b| format!(" — blocked by {} at ({:.0},{:.0},{:.0})",
                    b.hazard.as_str(), b.at[0], b.at[1], b.at[2])).unwrap_or_default();
                self.stop_nav_blocked(gs, "no_path", why.as_str(), goal_blocked_by, frontier_blocked_by,
                    &format!(
                    "No route to ({:.0},{:.0}): {} (searched to completion in {}ms — this is a definitive no, \
                     not a timeout){}.", goal.0, goal.1, why.as_str(), reply.plan_ms, detail));
                true
            }
        }
    }

    /// Is the player slain? Detected the SAME way the render/anim path picks the dead pose
    /// (`cur_hp <= 0` with a known `max_hp`) OR via the OP_Death `player_dead` flag. Using cur_hp —
    /// not just `player_dead` — catches an HP-to-0 update that lands before OP_Death arrives, which is
    /// the window in which a corpse was seen still walking (#238).
    fn is_player_dead(gs: &GameState) -> bool {
        gs.player_dead || (gs.cur_hp <= 0 && gs.max_hp > 0)
    }

    /// Stop all navigation the instant the player is slain (#238): abandon the destination + route +
    /// controller intent so a corpse doesn't keep walking toward the goal, and clear the overlay line.
    /// The route is wiped so a later respawn/relog starts fresh instead of resuming the dead man's
    /// path. Returns true when the player is dead (the caller returns early from the walk tick).
    fn nav_halt_if_dead(&mut self, gs: &GameState) -> bool {
        if !Self::is_player_dead(gs) {
            return false;
        }
        if self.goto_target.lock().unwrap().take().is_some() {
            tracing::info!("NAV: player is dead — abandoning /goto");
        }
        *self.goto_entity.lock().unwrap() = None;      // drop any entity chase
        *self.zone_cross.lock().unwrap() = None;        // drop a queued zone-cross
        *self.nav_intent.lock().unwrap() = None;        // stop driving the controller
        *self.nav_path_view.lock().unwrap() = (Vec::new(), Vec::new()); // clear the overlay line
        self.path.clear();
        self.local_path.clear();
        self.local_i = 0;
        self.path_goal = None;
        self.path_i = 0;
        // A corpse must not act on a plan that lands after it died (#238 + #340).
        self.planner.cancel();
        self.awaiting_first_plan = false;
        self.set_nav_state("idle");
        true
    }

    /// Live NPC-camp positions to route AROUND (aggro-avoidance, #67), excluding NPCs near the
    /// goal (you're walking TO the destination, often a target mob, so its own camp isn't avoided).
    /// The nearest FLOOR-REACHABLE in-zone translocator region (a zone-line region whose destination
    /// is THIS zone — the Qeynos guild-vault waterfall), as a goto target the char can walk INTO to
    /// teleport out (#266). None if no reachable in-zone portal exists.
    ///
    /// Two things this handles (both from find-issues-1's verified goto-Nerissa wedge at (-607,-71,z-14)):
    ///  1. Restrict to IN-ZONE indices, not the nearest zone-line overall — once a stranded char drifts
    ///     toward its goal a normal neighbour-zone exit can become the closest line, and "nearest line
    ///     then check in-zone" returned None from there, so the escape never fired.
    ///  2. Require the region's DRNTP footprint to reach the char's floor height, so walking to its XY
    ///     actually fires the z-EXACT auto-cross — skipping top-of-waterfall-column leaves whose point
    ///     sits high up (the char reaches the XY on the floor, stands below the leaf, never crosses).
    fn find_in_zone_portal(&self, gs: &GameState) -> Option<(f32, f32, f32)> {
        let guard = self.collision.read().unwrap();
        let c = guard.as_ref()?;
        let pos = [gs.player_x, gs.player_y, gs.player_z];
        let in_zone_idxs: Vec<i32> = self.zone_points.lock().unwrap().iter()
            .filter(|zp| zp.zone_id == gs.zone_id)
            .map(|zp| zp.iterator as i32)
            .collect();
        let portal = c.find_reachable_in_zone_line(&in_zone_idxs, pos).map(|(_, l)| (l[0], l[1], l[2]));
        if tracing::enabled!(tracing::Level::DEBUG) {
            let cands: Vec<_> = in_zone_idxs.iter()
                .filter_map(|&idx| c.find_zone_line_near(Some(idx), pos)
                    .map(|(_, l)| (idx, [l[0].round(), l[1].round(), l[2].round()])))
                .collect();
            tracing::debug!("find_in_zone_portal: pos_z={:.0} in_zone_idxs={in_zone_idxs:?} nearest_per_idx={cands:?} chose_reachable={portal:?}", pos[2]);
        }
        portal
    }

    fn aggro_avoid(gs: &GameState, goal: (f32, f32, f32), enabled: bool) -> Vec<[f32; 2]> {
        // `enabled == false` (from `avoid_aggro:false` on /v1/move/*) routes straight through — no
        // avoid points — for when the caller WANTS to path into a mob (#242). Default stays on (#67).
        if !enabled { return Vec::new(); }
        const NEAR_GOAL_SQ: f32 = 55.0 * 55.0;
        gs.entities.values()
            .filter(|e| e.is_npc && !e.dead)
            .filter(|e| { let (dx, dy) = (e.x - goal.0, e.y - goal.1); dx * dx + dy * dy > NEAR_GOAL_SQ })
            .map(|e| [e.x, e.y])
            .collect()
    }

    /// Advance one navigation tick (no-op if fewer than 150 ms have elapsed).
    pub fn tick(
        &mut self,
        stream:  &mut EqStream,
        gs:      &mut GameState,
    ) {
        self.drain_loot(gs);
        self.drain_doors(stream, gs);
        self.drain_quests(stream, gs);
        self.drain_group(stream, gs);
        self.drain_trainer(stream, gs);
        self.drain_zone_cross(stream, gs);
        self.drain_chat(stream, gs);
        self.drain_target(stream, gs);
        self.drain_who_friends(stream);
        self.drain_combat(stream, gs);
        self.drain_pet(stream, gs);
        self.drain_read_book(stream, gs);
        self.drain_guild(stream, gs);
        self.drain_cast(stream, gs);
        self.drain_mem_spell(stream, gs);
        self.drain_sit(stream, gs);
        self.drain_consider(stream, gs);
        self.drain_merchant(stream, gs);
        self.drain_move_item(stream, gs);

        // Stream the controller's authoritative position to the server every tick at native cadence
        // (independent of the 150 ms planner gate below). This is the single position authority.
        self.stream_position(stream, gs);

        // Dead men don't walk (#238, eqoxide#61): the instant the player is slain, abandon any /goto
        // or /zone_cross and stop driving the controller, so a corpse doesn't keep walking its route
        // toward the goal. Placed BEFORE the fast-steering refresh AND the 150 ms walk gate so
        // movement halts within a tick, not up to a gate-period later. Position streaming above still
        // runs, keeping the stationary corpse in sync with the server.
        if self.nav_halt_if_dead(gs) {
            return;
        }

        self.apply_fast_steering(gs);

        if self.last_tick.elapsed().as_millis() < NAV_TICK_MS {
            return;
        }
        self.last_tick = Instant::now();

        // Quest turn-in (POST /give) trade-window state machine. Spans multiple ticks: we must
        // wait for the server's OP_TradeRequestAck (sets gs.trade_ack_ready) between sending the
        // trade request and moving the item into the NPC trade slot. Run on the throttled ~150ms
        // cadence so the per-tick ack timeout count matches the documented ~3s window.
        self.tick_give(stream, gs);

        self.drive_auto_target(stream, gs);

        self.drive_auto_pet_combat(stream, gs);

        if self.drive_auto_engage_melee(stream, gs) { return; }
        if self.drive_controlled_fall(stream, gs) { return; }

        // (The dead-player guard now runs earlier — right after stream_position, before the fast-
        // steering refresh and the 150 ms gate — so a corpse stops within a tick. See #238.)

        self.drive_chase();

        self.drive_teleport_detect(gs);

        let goal = match self.resolve_goal() {
            Some(g) => g,
            None => return,
        };

        self.drive_walk(stream, gs, goal);
    }

    fn drain_loot(&mut self, gs: &mut GameState) {
        // POST /loot: queue the requested corpse onto the existing auto-loot pipeline. The gameplay
        // loop drains pending_loot — sends OP_LootRequest, echoes each OP_LootItem to take it, then
        // OP_EndLootRequest. The 500ms delay (loot_queued_at) lets the server register the corpse.
        if let Some(corpse_id) = self.loot.lock().unwrap().take() {
            gs.pending_loot.push_back(corpse_id);
            if gs.loot_queued_at.is_none() {
                gs.loot_queued_at = Some(Instant::now());
            }
            tracing::info!("loot: queued corpse_id={} for looting (via POST /loot)", corpse_id);
        }
    }

    fn drain_doors(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /doors/click or a human door click: send OP_ClickDoor. The door opens
        // visually only when the server replies with OP_MoveDoor.
        if let Some(door_id) = self.door_click.lock().unwrap().take() {
            stream.send_app_packet(OP_CLICK_DOOR, &build_click_door(door_id, gs.player_id));
            tracing::info!("EQ: click door_id={}", door_id);
            gs.log_msg("door", &format!("Clicked door {}", door_id));
        }
    }

    fn drain_quests(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/quests/accept ({"task_id":N}) or /decline (task_id=0): send OP_AcceptNewTask.
        // For a real accept, look up the offering NPC's id from gs.task_offers (task_master_id is
        // required by the struct); a decline sends task_master_id=0 (irrelevant when task_id==0).
        // Either way, the selector window is done with — clear all pending offers.
        if let Some(task_id) = self.accept_task.lock().unwrap().take() {
            let task_master_id = if task_id == 0 {
                0
            } else {
                gs.task_offers.iter().find(|o| o.task_id == task_id).map(|o| o.npc_id).unwrap_or(0)
            };
            stream.send_app_packet(OP_ACCEPT_NEW_TASK, &build_accept_new_task(task_id, task_master_id));
            if task_id == 0 {
                tracing::info!("EQ: quests: declined all pending task offers");
                gs.log_msg("quest", "Declined task offer(s)");
            } else {
                tracing::info!("EQ: quests: accepted task_id={task_id} task_master_id={task_master_id}");
                gs.log_msg("quest", "Accepted task offer");
            }
            gs.task_offers.clear();
        }

        // POST /v1/quests/cancel ({"task_id":N}): abandon an active task. OP_CancelTask addresses
        // the task by its journal sequence_number, not task_id — see build_cancel_task.
        if let Some(task_id) = self.cancel_task.lock().unwrap().take() {
            if let Some(task) = gs.tasks.get(&task_id) {
                let seq = task.sequence_number;
                stream.send_app_packet(OP_CANCEL_TASK, &build_cancel_task(seq));
                tracing::info!("EQ: quests: cancelled task_id={task_id} sequence_number={seq}");
                gs.log_msg("quest", "Cancelled task");
            } else {
                tracing::warn!("EQ: quests: cancel requested for unknown task_id={task_id} — ignoring");
            }
        }
    }

    fn drain_group(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/group/invite {"name":"X"}: send OP_GroupInvite.
        if let Some(target) = self.group_invite.lock().unwrap().take() {
            stream.send_app_packet(OP_GROUP_INVITE, &build_group_invite(&target, &gs.player_name));
            tracing::info!("EQ: group: invited {target}");
            gs.log_msg("group", &format!("Invited {target} to group"));
        }

        // POST /v1/group/accept: send OP_GroupFollow. Optimistically clear pending_invite now —
        // the real roster confirmation arrives via OP_GroupUpdateB/OP_GroupAcknowledge.
        if self.group_accept.lock().unwrap().take().is_some() {
            if let Some(inviter) = gs.pending_invite.take() {
                stream.send_app_packet(OP_GROUP_FOLLOW, &build_group_follow(&inviter, &gs.player_name));
                tracing::info!("EQ: group: accepted invite from {inviter}");
                gs.log_msg("group", &format!("Accepted group invite from {inviter}"));
            }
        }

        // POST /v1/group/decline: RoF2 has no working OP_GroupCancelInvite, so send a defensive
        // OP_GroupDisband(self, self) cleanup instead.
        if self.group_decline.lock().unwrap().take().is_some() {
            if let Some(inviter) = gs.pending_invite.take() {
                stream.send_app_packet(OP_GROUP_DISBAND, &build_group_disband(&gs.player_name, &gs.player_name));
                tracing::info!("EQ: group: declined invite from {inviter}");
                gs.log_msg("group", &format!("Declined group invite from {inviter}"));
            }
        }

        // POST /v1/group/leave: send OP_GroupDisband(self, self). If leader with < 3 members this
        // fully disbands the group server-side (no auto handoff — see Global Constraints).
        if self.group_leave.lock().unwrap().take().is_some() {
            stream.send_app_packet(OP_GROUP_DISBAND, &build_group_disband(&gs.player_name, &gs.player_name));
            tracing::info!("EQ: group: left group");
            gs.log_msg("group", "Left group");
        }

        // POST /v1/group/kick {"name":"X"}: send OP_GroupDisband(self, target). HTTP layer already
        // validated leadership + membership before queuing this.
        if let Some(target) = self.group_kick.lock().unwrap().take() {
            stream.send_app_packet(OP_GROUP_DISBAND, &build_group_disband(&gs.player_name, &target));
            tracing::info!("EQ: group: kicked {target}");
            gs.log_msg("group", &format!("Kicked {target} from group"));
        }

        // POST /v1/group/makeleader {"name":"X"}: send OP_GroupMakeLeader.
        if let Some(target) = self.group_make_leader.lock().unwrap().take() {
            stream.send_app_packet(OP_GROUP_MAKE_LEADER, &build_group_make_leader(&gs.group_leader, &target));
            tracing::info!("EQ: group: transferring leadership to {target}");
            gs.log_msg("group", &format!("Transferred group leadership to {target}"));
        }
    }

    fn drain_trainer(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/trainer/open {"trainer":"X"}: send OP_GMTraining for the resolved NPC spawn id.
        // The server replies OP_GMTraining with the offered caps → apply_gm_training sets gs.trainer_*.
        // Sentinel: Some(0) ENDS the open session (OP_GMEndTraining) — 0 is never a real spawn id;
        // reusing the slot avoids threading one more field through the positional chains (#162).
        if let Some(npc_id) = self.trainer_open_req.lock().unwrap().take() {
            if npc_id == 0 {
                if let Some(open_npc) = gs.trainer_open.take() {
                    let payload = build_gm_end_training(open_npc, gs.player_id);
                    stream.send_app_packet(OP_GM_END_TRAINING, &payload);
                    gs.trainer_skills.clear();
                    tracing::info!("EQ: trainer: ended training with npc {open_npc}");
                }
            } else {
                stream.send_app_packet(OP_GM_TRAINING, &build_gm_training(npc_id, gs.player_id));
                tracing::info!("EQ: trainer: opening training with npc {npc_id}");
            }
        }

        // POST /v1/trainer/train {"skill_id":N}: send OP_GMTrainSkill to the open trainer. The server
        // raises the skill and echoes OP_SkillUpdate → apply_skill_update reflects the new value.
        if let Some(skill_id) = self.trainer_train_req.lock().unwrap().take() {
            if let Some(npc_id) = gs.trainer_open {
                stream.send_app_packet(OP_GM_TRAIN_SKILL, &build_gm_train_skill(npc_id, skill_id));
                tracing::info!("EQ: trainer: training skill {skill_id} at npc {npc_id}");
                gs.log_msg("trainer", &format!("Training {}", crate::skills::skill_name(skill_id).unwrap_or("?")));
            } else {
                gs.log_msg("trainer", "Cannot train — no trainer window open");
            }
        }
    }

    fn drain_zone_cross(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Check zone-cross request — walk onto the target zone line so the auto-cross below fires.
        //
        // A zone line's real trigger is a `DRNTP` region baked into the zone geometry (native
        // mechanism), NOT the coords in OP_SendZonepoints — those are the DESTINATION of each line,
        // so walking to them lands the player nowhere near the trigger and the server safe-coords /
        // cheat-flags the crossing (the root cause of #174). Resolve the target zone to its
        // zone-point index (iterator), locate that DRNTP region in the zone BSP, and walk there.
        let cross_req = self.zone_cross.lock().unwrap().take();
        if let Some(want_zone) = cross_req {
            // want_zone != 0 → resolve it to a zone-point index; want_zone == 0 → any nearest line.
            let want_index = if want_zone != 0 {
                match self.zone_points.lock().unwrap().iter()
                    .find(|zp| zp.zone_id == want_zone).map(|zp| zp.iterator as i32)
                {
                    Some(idx) => Some(idx),
                    None => {
                        tracing::info!("zone_cross: no zone point advertised for zone_id={want_zone}");
                        gs.log_msg("zone", "No zone line found to cross");
                        // Make the failure observable instead of a silent no-op (#267): a caller that
                        // got 200 from POST /zone_cross can poll nav_state and see it didn't happen.
                        // With a REASON — a terminal state with `nav_reason: null` contradicts the
                        // contract this PR documents (#377 review, N2).
                        self.set_nav_state_because("no_path", Some("no_zone_line_to_zone"));
                        None
                    }
                }
            } else {
                None // any zone line
            };
            // Only proceed if we actually have a target (want_zone==0 always may; want_zone!=0 needs a match).
            if want_zone == 0 || want_index.is_some() {
                // Locate the NEAREST reachable zone-line region for the wanted zone (not the first
                // zone-point index that matches — a zone with several lines to the same target, or an
                // in-zone translocator with multiple advertised points, would otherwise pick one with
                // no nearby region and no-op, #266). want_index==None → any nearest line.
                let located = self.collision.read().unwrap().as_ref().and_then(|c| {
                    let pos = [gs.player_x, gs.player_y, gs.player_z];
                    match (want_zone, want_index) {
                        (0, _) => c.find_zone_line_near(None, pos),
                        (_, _) => {
                            // Every zone-point index advertised for `want_zone`, nearest region wins.
                            let idxs: Vec<i32> = self.zone_points.lock().unwrap().iter()
                                .filter(|zp| zp.zone_id == want_zone).map(|zp| zp.iterator as i32).collect();
                            idxs.iter()
                                .filter_map(|&idx| c.find_zone_line_near(Some(idx), pos))
                                .min_by(|a, b| {
                                    let da = (a.1[0]-pos[0]).hypot(a.1[1]-pos[1]);
                                    let db = (b.1[0]-pos[0]).hypot(b.1[1]-pos[1]);
                                    da.total_cmp(&db)
                                })
                        }
                    }
                });
                match located {
                    Some((index, [tx, ty, tz])) => {
                        // Destination zone for logging (resolve the located region's index).
                        let dest_zone = self.zone_points.lock().unwrap().iter()
                            .find(|zp| zp.iterator as i32 == index).map(|zp| zp.zone_id).unwrap_or(want_zone);
                        let d2 = (tx - gs.player_x).powi(2) + (ty - gs.player_y).powi(2);
                        const ZONE_LINE_DIST2: f32 = 15.0 * 15.0;
                        if d2 <= ZONE_LINE_DIST2 {
                            // Already standing on the line — the auto-cross below fires this tick.
                            tracing::info!("zone_cross: already on the zone_id={dest_zone} line (index={index})");
                        } else {
                            tracing::info!("zone_cross: walking {:.0}u to the zone_id={dest_zone} line at ({tx:.0},{ty:.0}) (index={index})", d2.sqrt());
                            gs.log_msg("zone", &format!("Walking to the zone {} line", dest_zone));
                            *self.goto_target.lock().unwrap() = Some((tx, ty, tz));
                            *self.goto_entity.lock().unwrap() = None;
                        }
                    }
                    None => {
                        tracing::info!("zone_cross: no zone-line region found for zone_id={want_zone}");
                        gs.log_msg("zone", "No zone line found to cross");
                        // Advertised in OP_SendZonepoints but no DRNTP region in the loaded map (a .wtr
                        // gap): report it so the caller isn't left thinking the 200 meant success (#267).
                        self.set_nav_state_because("no_path", Some("zone_line_not_in_map"));
                    }
                }
            }
        }

        // Auto zone-cross (native mechanism): when the player stands in a DRNTP zone-line region
        // baked into the zone BSP, resolve its zone-point index to a destination via the
        // OP_SendZonepoints list and send OP_ZONE_CHANGE. The server then matches our real position
        // against the DB trigger and places us at the correct arrival point. Cooldown prevents
        // re-firing while still inside the region right after a crossing.
        {
            const ZONE_CROSS_COOLDOWN_MS: u128 = 10000; // 10 seconds
            // A dead corpse standing in a zone-line region must NOT auto-zone (#238) — this fires purely
            // from physical position, so a character killed right at a boundary would cross while dead.
            if !Self::is_player_dead(gs) && self.last_zone_cross.elapsed().as_millis() > ZONE_CROSS_COOLDOWN_MS {
                let index = self.collision.read().unwrap().as_ref()
                    .and_then(|c| c.zone_line_at([gs.player_x, gs.player_y, gs.player_z]));
                if let Some(index) = index {
                    // Resolve destination: the advertised zone point whose iterator matches this
                    // region's index. A region with no matching zone point (e.g. a WLD index the DB
                    // doesn't advertise) is left alone rather than crossing blindly.
                    let dest = self.zone_points.lock().unwrap().iter()
                        .find(|zp| zp.iterator as i32 == index && zp.zone_id != 0)
                        .map(|zp| zp.zone_id);
                    match dest {
                        Some(dest_zone) => {
                            tracing::info!("zone_cross: in zone-line region index={index} → zone_id={dest_zone}");
                            gs.log_msg("zone", &format!("Crossing to zone {}", dest_zone));
                            self.send_zone_change_packet(stream, gs, dest_zone);
                            self.last_zone_cross = Instant::now();
                        }
                        None => {
                            tracing::debug!("zone_cross: zone-line region index={index} has no matching zone point — ignoring");
                        }
                    }
                }
            }
        }

        // NOTE: server-initiated zone changes (GM #zone, portal doors, spell ports/gate/evac) are
        // answered by the gameplay.rs OP_REQUEST_CLIENT_ZONE_CHANGE handler, which echoes the
        // server's real zone_id via build_zone_change. This block USED to re-send via
        // send_zone_change_packet, but #199 changed that to always emit zoneID=0 (the resolve-from-
        // position sentinel, correct only for client-initiated WALK-IN crossings). That misrouted
        // every server-initiated teleport to a wrong zone (#235) — so it's removed; the wire
        // zoneID=0 path is now confined to /v1/move/zone_cross.
    }

    fn drain_chat(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Check hail request — say "Hail, <name>" so the NPC fires its hail script. The server only
        // runs an NPC's EVENT_SAY on the player's CURRENT TARGET (client.cpp: `Mob* t = GetTarget()`),
        // so we must target the NPC FIRST, in the same tick and before the say packet, or the hail is
        // silently ignored (#130). The target packet precedes the say on the ordered stream, so the
        // server has GetTarget()==the NPC when it processes the say.
        let hail_req = self.hail.lock().unwrap().take();
        if let Some((name, spawn_id)) = hail_req {
            // A hail starts a FRESH interaction — drop any saylink choices left over from a prior
            // NPC (or a system/command message). Otherwise `/observe/dialogue` leaks the last
            // choices indefinitely, since they're only ever overwritten when a new say-line carries
            // saylinks and never cleared (#274). The hailed NPC's own reply repopulates them.
            gs.dialogue_choices.clear();
            if let Some(id) = spawn_id {
                gs.set_target(id); // also clears stale con/attitude from any prior target (#323)
                stream.send_app_packet(OP_TARGET_MOUSE, &build_target_packet(id));
            }
            let msg = format!("Hail, {}", name);
            let pkt = build_say_packet(&gs.player_name, &name, &msg);
            tracing::info!("EQ: hailing '{}' (target={:?}, say): {}", name, spawn_id, msg);
            stream.send_app_packet(OP_CHANNEL_MESSAGE, &pkt);
            let line = format!("You say, '{}'", msg);
            gs.log_msg("chat", &line);
        }

        // Check say request — arbitrary Say text (HUD say box / quest keyword follow-up).
        let say_text = self.say.lock().unwrap().take();
        if let Some(text) = say_text {
            // The `/camp` chat keyword is a local command, not Say text: toggle a camp instead of
            // broadcasting it. The gameplay loop drains the camp slot and runs the camp/cancel.
            if text.trim().eq_ignore_ascii_case("/camp") {
                *self.camp.lock().unwrap() = Some(CampCmd::Toggle);
                tracing::info!("EQ: /camp chat command — toggling camp");
            } else {
                let pkt = build_say_packet(&gs.player_name, "", &text);
                tracing::info!("EQ: say: {}", text);
                stream.send_app_packet(OP_CHANNEL_MESSAGE, &pkt);
                let line = format!("You say, '{}'", text);
                gs.log_msg("chat", &line);
            }
        }

        // Check dialogue-click request (POST /v1/interact/dialogue, or a GUI click): "click" a
        // parsed saylink by sending OP_ItemLinkClick with its ids. The server resolves the phrase
        // from its saylink table and processes it as if we said it to the NPC (#120).
        let click = self.dialogue_click.lock().unwrap().take();
        if let Some(c) = click {
            let pkt = build_item_link_click(c.item_id, &c.augments, c.link_hash, c.icon);
            tracing::info!("EQ: dialogue click: '{}' (sayid={})", c.text, c.augments[0]);
            stream.send_app_packet(OP_ITEM_LINK_CLICK, &pkt);
            let line = format!("You say, '{}'", c.text);
            gs.log_msg("chat", &line);
        }

        // Drain queued outgoing chat (POST /tell|/ooc|/shout|/group): build + send OP_ChannelMessage.
        let outgoing: Vec<crate::ipc::ChatSend> = {
            let mut q = self.chat_send.lock().unwrap();
            std::mem::take(&mut *q)
        };
        for c in outgoing {
            let pkt = build_channel_message(&gs.player_name, &c.to, c.chan, &c.text);
            stream.send_app_packet(OP_CHANNEL_MESSAGE, &pkt);
            let label = match c.chan { 7 => format!("tell {}", c.to), 5 => "ooc".into(),
                                       3 => "shout".into(), 2 => "group".into(), 0 => "guild".into(),
                                       n => format!("chan{n}") };
            tracing::info!("EQ: {} -> {}", label, c.text);
            // Native-style local echo, logged under the channel's kind so the chat window
            // tab-filters and colors it like the matching incoming traffic.
            let (kind, line): (&str, String) = match c.chan {
                7 => ("tell",  format!("You told {}, '{}'", c.to, c.text)),
                5 => ("ooc",   format!("You say out of character, '{}'", c.text)),
                3 => ("shout", format!("You shout, '{}'", c.text)),
                2 => ("group", format!("You tell your party, '{}'", c.text)),
                0 => ("guild", format!("You say to your guild, '{}'", c.text)),
                _ => ("chat",  format!("You {}: {}", label, c.text)),
            };
            gs.log_msg(kind, &line);
        }
    }

    fn drain_target(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Check target request — set target + auto-consider it (con color comes back as
        // an OP_CONSIDER reply, handled in packet_handler). GameState::set_target seeds
        // target_name/target_hp_pct (name/HP — update_hp/update_hp_pct then keep target_hp_pct
        // live as combat HP updates arrive) AND clears target_con/target_con_name/
        // target_attitude so the PREVIOUS target's con can't survive a re-target (eqoxide#323).
        let target_id = self.target.lock().unwrap().take();
        if let Some(id) = target_id {
            // Never adopt a spawn that isn't in the zone. POST /v1/combat/target 404s on an unknown
            // id, but the entity could still despawn between the HTTP check and this drain — and the
            // server silently IGNORES an OP_TargetMouse for an unknown id, so calling set_target
            // anyway would leave the client believing in a target the server never set. Say so
            // instead of lying. The player's own spawn is legal and is absent from `entities`. (#348)
            if id != gs.player_id && !gs.entities.contains_key(&id) {
                let text = format!("Cannot target spawn {id}: it is not in this zone.");
                gs.log_msg("combat", &text);
                gs.push_event("combat", "target_failed", "", true, &text);
                tracing::info!("EQ: target spawn_id={} REFUSED — not in the entity list", id);
            } else {
                gs.set_target(id);
                stream.send_app_packet(OP_TARGET_MOUSE, &build_target_packet(id));
                stream.send_app_packet(OP_CONSIDER, &build_consider_packet(gs.player_id, id));
                tracing::info!("EQ: target spawn_id={} + consider", id);
            }
        }
    }

    fn drain_who_friends(&mut self, stream: &mut EqStream) {
        // Check /who all request (#300) — send OP_WhoAllRequest (server-wide, type=3); the oneshot
        // sender is held in `pending_who` until OP_WhoAllResponse arrives (see `fulfill_who`). A newer
        // request supersedes an in-flight one (its sender drops → that GET times out).
        if let Some(tx) = self.who_req.lock().unwrap().take() {
            stream.send_app_packet(OP_WHO_ALL_REQUEST, &build_who_all_request(3));
            self.pending_who = Some(tx);
            self.expecting_friends = false; // the next OP_WhoAllResponse is a /who all, not a friends poll
            tracing::info!("EQ: sent OP_WhoAllRequest (/who all)");
        }

        // Check friends-presence request (#301) — send OP_FriendsWho with the client-local friends
        // string; the reply arrives as OP_WhoAllResponse (online subset), routed to `fulfill_friends`
        // by the `expecting_friends` flag. Mirrors the /who all path above.
        if let Some(tx) = self.friends_req.lock().unwrap().take() {
            let names = self.friends_list.lock().unwrap().clone();
            stream.send_app_packet(OP_FRIENDS_WHO, &build_friends_who(&names));
            self.pending_friends = Some(tx);
            self.expecting_friends = true;
            tracing::info!("EQ: sent OP_FriendsWho ({} friend(s))", names.len());
        }
    }

    fn drain_combat(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Check attack request — send OP_AUTO_ATTACK(1) to start, OP_AUTO_ATTACK(0) to stop.
        // Server expects exactly 4 bytes; byte[0]=1 enables, byte[0]=0 disables.
        let attack_req = self.attack.lock().unwrap().take();
        if let Some(on) = attack_req {
            self.auto_attack = on;
            let payload = [if on { 1u8 } else { 0u8 }, 0, 0, 0];
            stream.send_app_packet(OP_AUTO_ATTACK, &payload);
            gs.auto_attack = on;
            tracing::info!("EQ: auto-attack {}", if on { "ON" } else { "OFF" });
        }
    }

    fn drain_pet(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/pet/command or a Pet-window button: send one OP_PetCommands for the player's
        // pet. PET_ATTACK aims at the current target (like the auto-pet path); every other command
        // (back off / follow / guard / sit) targets 0 — the server acts on the pet itself.
        let pet_cmd = self.pet_cmd.lock().unwrap().take();
        if let Some(cmd) = pet_cmd {
            let cmd = cmd as u32;
            if gs.pet_id.is_none() {
                gs.log_msg("pet", "You have no pet");
            } else if cmd == PET_ATTACK {
                match gs.target_id.filter(|&t| t != 0) {
                    Some(tid) => {
                        stream.send_app_packet(OP_PET_COMMANDS, &build_pet_command(PET_ATTACK, tid));
                        // Keep the auto-pet-combat dedupe in sync so it doesn't immediately
                        // re-issue (or back-off-cancel) the manual order.
                        self.last_pet_target = Some(tid);
                        tracing::info!("EQ: pet command attack → target {tid}");
                        gs.log_msg("pet", "Pet attack ordered");
                    }
                    None => gs.log_msg("pet", "Pet attack: no target"),
                }
            } else {
                stream.send_app_packet(OP_PET_COMMANDS, &build_pet_command(cmd, 0));
                if cmd == PET_BACKOFF { self.last_pet_target = None; }
                tracing::info!("EQ: pet command {cmd}");
                gs.log_msg("pet", &format!("Pet command sent ({})", match cmd {
                    PET_BACKOFF => "back off", PET_FOLLOWME => "follow",
                    PET_GUARDHERE => "guard here", PET_SIT => "sit", _ => "other",
                }));
            }
        }
    }

    fn drain_read_book(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/interact/read {"slot":N}: read a book/note. Look up the item at that wire slot;
        // if it carries a Filename it's readable, so send OP_ReadBook with that filename and the
        // server replies with the text (apply_read_book stores it → GET /v1/observe/item_text). (#288)
        let read_slot = self.read_book.lock().unwrap().take();
        if let Some(slot) = read_slot {
            match gs.inventory.iter().find(|i| i.slot == slot) {
                Some(item) if !item.filename.is_empty() => {
                    let pkt = build_read_book_packet(slot as i16, gs.player_id, &item.filename);
                    stream.send_app_packet(OP_READ_BOOK, &pkt);
                    tracing::info!("EQ: read book slot={} file='{}'", slot, item.filename);
                }
                Some(_) => gs.log_msg("book", &format!("Item in slot {slot} is not readable")),
                None    => gs.log_msg("book", &format!("No item in slot {slot} to read")),
            }
        }
    }

    fn drain_guild(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/guild/{invite,accept,leave,remove}: one queued guild action → the matching RoF2
        // guild opcode. Invite/remove/leave share GuildCommand_Struct; accept replies to a captured
        // pending invite with GuildInviteAccept_Struct. (#295)
        let guild_action = self.guild_action.lock().unwrap().take();
        if let Some(action) = guild_action {
            const GUILD_RECRUIT: u32 = 8; // default rank for a fresh invite (RoF2 0-8 scale)
            match action {
                crate::ipc::GuildAction::Invite(name) => {
                    let pkt = build_guild_command(&name, &gs.player_name, gs.player_guild_id, GUILD_RECRUIT);
                    stream.send_app_packet(OP_GUILD_INVITE, &pkt);
                    gs.log_msg("guild", &format!("Inviting {name} to the guild"));
                    tracing::info!("EQ: guild invite -> {name}");
                }
                crate::ipc::GuildAction::Remove(name) => {
                    let pkt = build_guild_command(&name, &gs.player_name, gs.player_guild_id, 0);
                    stream.send_app_packet(OP_GUILD_REMOVE, &pkt);
                    gs.log_msg("guild", &format!("Removing {name} from the guild"));
                    tracing::info!("EQ: guild remove -> {name}");
                }
                crate::ipc::GuildAction::Leave => {
                    // Self-leave: othername == myname.
                    let pkt = build_guild_command(&gs.player_name, &gs.player_name, gs.player_guild_id, 0);
                    stream.send_app_packet(OP_GUILD_REMOVE, &pkt);
                    gs.log_msg("guild", "Leaving guild");
                    tracing::info!("EQ: guild leave");
                }
                crate::ipc::GuildAction::Accept => match gs.pending_guild_invite.take() {
                    Some((inviter, guild_id, rank)) => {
                        let pkt = build_guild_invite_accept(&inviter, &gs.player_name, rank, guild_id);
                        stream.send_app_packet(OP_GUILD_INVITE_ACCEPT, &pkt);
                        gs.log_msg("guild", &format!("Accepting guild invite from {inviter}"));
                        tracing::info!("EQ: guild accept from {inviter} (guild_id={guild_id})");
                    }
                    None => gs.log_msg("guild", "No pending guild invite to accept"),
                },
            }
        }
    }

    fn drain_cast(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Cast a memorized spell gem. Target priority: an explicit API target > the current target
        // > self. `Some(0)` is not a real spawn (the "clear target" sentinel), so collapse it to
        // "none" here or the self-fallback never fires. For BENEFICIAL spells (heals/buffs) that
        // aren't aimed at a friendly target, cast on the caster instead of a hostile/stale mob —
        // matching the real RoF2 client, which self-targets heals/buffs. (eqoxide#95)
        let cast_req = self.cast.lock().unwrap().take();
        if let Some(req) = cast_req {
          if let Some(item_slot) = req.item_slot {
            // Item "clicky" cast (teleport ring / port potion, etc.). Resolve the click spell from
            // the item currently at that wire slot and refuse if it isn't a clicky, so a stale slot
            // can't fire an unrelated cast. Target: explicit > current > self. (eqoxide#193)
            let click = gs.inventory.iter().find(|i| i.slot == item_slot as i32)
                .map(|i| i.click_spell_id).unwrap_or(0);
            if click == 0 {
                // POST /v1/combat/cast validated the slot, but the item can move/vanish between the
                // handler and this drain. Dropping it with only a tracing line meant the agent saw
                // 200 and then nothing at all — report the failure where the agent can read it (#348).
                let text = format!("Cannot cast: no clickable item in slot {item_slot}.");
                gs.finish_cast(0, "cast_failed", &text);
                tracing::info!("EQ: item cast slot={} ignored — no clicky item at that slot", item_slot);
            } else {
                let target = req.target_id.filter(|&t| t != 0)
                    .or(gs.target_id.filter(|&t| t != 0))
                    .unwrap_or(gs.player_id);
                stream.send_app_packet(OP_CAST_SPELL, &build_item_cast_packet(item_slot, click, target));
                tracing::info!("EQ: item cast slot={} spell={} target={}", item_slot, click, target);
            }
          } else {
            let spell_id = gs.mem_spells.get(req.gem as usize).copied()
                .unwrap_or(crate::game_state::EMPTY_GEM);
            if !crate::game_state::gem_is_empty(spell_id) {
                let explicit = req.target_id.filter(|&t| t != 0);
                let current  = gs.target_id.filter(|&t| t != 0);
                let mut target = explicit.or(current).unwrap_or(gs.player_id);
                if let Some(db) = crate::spells::global() {
                    if db.is_self_only(spell_id) {
                        target = gs.player_id; // ST_SELF: always the caster
                    } else if explicit.is_none() && db.is_beneficial(spell_id) {
                        // Keep an explicitly-chosen friendly (PC) target for group heals; otherwise
                        // (no target, cleared, or a hostile NPC) land the buff/heal on ourselves.
                        let friendly = target == gs.player_id
                            || gs.entities.get(&target).map_or(false, |e| !e.is_npc);
                        if !friendly { target = gs.player_id; }
                    }
                }
                stream.send_app_packet(OP_CAST_SPELL, &build_cast_packet(req.gem as u32, spell_id, target));
                tracing::info!("EQ: cast gem={} spell={} target={}", req.gem, spell_id, target);
            } else {
                // POST /v1/combat/cast now 409s on an empty gem, but the gem can be un-memorized
                // between the handler and this drain. This arm used to be a bare `tracing::info!` —
                // the agent got 200 and then ABSOLUTE SILENCE: no packet, no message, no event, no
                // state change, indistinguishable from a cast still in flight. (#348)
                let text = format!("Cannot cast: spell gem {} is empty.", req.gem);
                gs.finish_cast(0, "cast_failed", &text);
                tracing::info!("EQ: cast gem={} ignored — empty gem", req.gem);
            }
          }
        }
    }

    fn drain_mem_spell(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Scribe a scroll into the spellbook (scribing=0) or memorize a known spell into a gem
        // (scribing=1) — OP_MemorizeSpell. The server validates (you hold the scroll / know the
        // spell) and pushes OP_MemorizeSpell back, which updates gs.mem_spells for the gem case.
        let mem_req = self.mem_spell.lock().unwrap().take();
        if let Some((slot, spell_id, scribing, from)) = mem_req {
            // Scribing (0) only takes effect on the scroll sitting on the CURSOR: the RoF2 server
            // reads m_inv[slotCursor] and ignores the packet otherwise (silent fail, eqoxide#11).
            // So move the scroll from its inventory slot → cursor first (same tick; the server
            // processes packets in order, so the cursor holds the scroll when the scribe arrives).
            if scribing == 0 {
                if let Some(from_slot) = from {
                    if from_slot != SLOT_CURSOR {
                        stream.send_app_packet(OP_MOVE_ITEM, &build_move_item(from_slot, SLOT_CURSOR));
                        gs.move_item(from_slot as i32, SLOT_CURSOR as i32); // mirror locally
                        tracing::info!("EQ: scribe — moved scroll slot {} → cursor", from_slot);
                    }
                }
            }
            stream.send_app_packet(OP_MEMORIZE_SPELL, &build_memorize_packet(slot, spell_id, scribing));
            if scribing == 0 {
                // The RoF2 server CONSUMES the scribed scroll: OPMemorizeSpell's memSpellScribing
                // case runs ScribeSpell(...) then DeleteItemInInventory(slotCursor) (zone/
                // client_process.cpp). We already moved the scroll to the cursor above, so mirror
                // that deletion locally — otherwise the (now server-deleted) scroll stays stuck on
                // cursor slot 33 in our view, blocking looting and any later cursor move (#271). No
                // OP_DeleteItem is sent: the server already removed it, so that would double-delete.
                gs.inventory.retain(|i| i.slot != SLOT_CURSOR as i32);
            }
            let what = match scribing { 0 => "scribe", 1 => "memorize", _ => "unmem" };
            tracing::info!("EQ: {what} spell={spell_id} slot={slot}");
            gs.log_msg("spell", &format!("{what} spell {spell_id} (slot {slot})"));
        }
    }

    fn drain_sit(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Sit / stand (OP_SpawnAppearance type=14, param 110/100).
        let sit_req = self.sit.lock().unwrap().take();
        if let Some(sit) = sit_req {
            let param = if sit { 110u32 } else { 100u32 };
            let payload = build_spawn_appearance_packet(gs.player_id as u16, 14, param);
            stream.send_app_packet(OP_SPAWN_APPEARANCE, &payload);
            gs.sitting = sit;
            tracing::info!("EQ: {}", if sit { "sit" } else { "stand" });
        }
    }

    fn drain_consider(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Standalone consider.
        let con_req = self.consider.lock().unwrap().take();
        if let Some(id) = con_req {
            stream.send_app_packet(OP_CONSIDER, &build_consider_packet(gs.player_id, id));
            tracing::info!("EQ: consider spawn_id={}", id);
        }
    }

    fn drain_merchant(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Merchant buy: open the merchant (OP_ShopRequest) then buy its inventory slot
        // (OP_ShopPlayerBuy). Sent in sequence — the server processes the open first so the
        // merchant is open by the time the buy arrives. Must be within ~200u of the merchant.
        let buy_req = self.buy.lock().unwrap().take();
        if let Some((merchant_id, slot)) = buy_req {
            // #360/#361: a failed/unanswered OP_ShopRequest must not leave `merchant_open` reporting
            // a DIFFERENT previous merchant, and the coin balance must read as unverified until this
            // buy is reconciled against a real OP_PlayerProfile — a silent inventory-full/LORE refusal
            // sends no echo at all. begin_shop_open_for only clears when re-targeting a different (or
            // no) merchant, so a routine re-buy from the already-open one doesn't flicker it closed
            // (#361 review FIX 2). See GameState::begin_shop_open_for/begin_shop_buy for the rationale.
            gs.begin_shop_open_for(merchant_id);
            gs.begin_shop_buy();
            let open = merchant_click(merchant_id, gs.player_id, 1);
            stream.send_app_packet(OP_SHOP_REQUEST, &open);
            // RoF2 Merchant_Sell_Struct (32b): npcid@0, playerid@4, itemslot@8, unknown12@12,
            // quantity@16, unknown20@20, price@24, unknown28@28. (Titanium was 24b with price@20;
            // the RoF2 server DECODEs an exact 32 bytes, so a short packet was silently dropped.)
            let mut buy = [0u8; 32];
            buy[0..4].copy_from_slice(&merchant_id.to_le_bytes());
            buy[4..8].copy_from_slice(&gs.player_id.to_le_bytes());
            buy[8..12].copy_from_slice(&slot.to_le_bytes());
            buy[16..20].copy_from_slice(&1u32.to_le_bytes()); // quantity = 1 (server sets the price)
            stream.send_app_packet(OP_SHOP_PLAYER_BUY, &buy);
            tracing::info!("EQ: shop buy sent — merchant_id={} slot={} qty=1", merchant_id, slot);
            // No optimistic "Bought item" log and no local spend_coin here (#345, generalizing the
            // #269 sell fix): the server can refuse — out-of-range/bad merchant/qty, a stale slot,
            // or insufficient funds — with NO OP_ShopPlayerBuy echo at all, and the insufficient-funds
            // case sends nothing whatsoever, so a buy can fail silently server-side. Deducting coin or
            // logging success at send time therefore fabricates a purchase that never happened.
            // (Note: KOS is NOT a refusal path — Handle_OP_ShopPlayerBuy has no faction check at all;
            // faction only gates opening the window. A buy from an already-open KOS merchant succeeds.)
            // On success the server echoes THIS SAME opcode back (Merchant_Sell_Struct, price
            // recomputed server-side) — apply_shop_player_buy (packet_handler.rs) is the only place
            // that may deduct coin or log "Bought item", because it's the only place that knows the
            // buy actually succeeded.
        }

        // Merchant sell: open the merchant (OP_ShopRequest) then sell a player inventory slot
        // (OP_ShopPlayerSell). Same sequencing as buy so the shop is open server-side first.
        // Must be within ~200u of the merchant; the server computes the price (we send 0).
        let sell_req = self.sell.lock().unwrap().take();
        if let Some((merchant_id, slot, quantity)) = sell_req {
            // #360: same staleness hazard as the buy path above — clear a DIFFERENT stale merchant
            // before sending, but don't flicker the one that's already open (#361 review FIX 2).
            gs.begin_shop_open_for(merchant_id);
            let open = merchant_click(merchant_id, gs.player_id, 1);
            stream.send_app_packet(OP_SHOP_REQUEST, &open);
            // RoF2 Merchant_Purchase_Struct is 20 bytes (rof2_structs.h): npcid(u32)@0,
            // inventory_slot(TypelessInventorySlot_Struct: Slot i16@4, SubIndex i16@6, AugIndex i16@8,
            // Unknown i16@10)@4, quantity(u32)@12, price(u32)@16. The old 16-byte body (plain u32
            // slot@4) failed the server's DECODE_LENGTH_EXACT, so EVERY sell was silently dropped
            // (#269). `slot` is the RoF2 wire slot /observe/inventory reports (general inv 23-32);
            // RoF2ToServerTypelessSlot passes it straight through for a top-level possession, so
            // SubIndex/AugIndex are the "none" sentinels (SLOT_INVALID / SOCKET_INVALID = -1).
            let mut sell = [0u8; 20];
            sell[0..4].copy_from_slice(&merchant_id.to_le_bytes());
            sell[4..6].copy_from_slice(&(slot as i16).to_le_bytes());   // Slot (RoF2 wire slot)
            sell[6..8].copy_from_slice(&(-1i16).to_le_bytes());          // SubIndex: not inside a bag
            sell[8..10].copy_from_slice(&(-1i16).to_le_bytes());         // AugIndex: no augment socket
            // Unknown01 @10 stays 0.
            sell[12..16].copy_from_slice(&quantity.to_le_bytes());
            // price @16 = 0: the server charges its own buy-back price.
            stream.send_app_packet(OP_SHOP_PLAYER_SELL, &sell);
            tracing::info!("EQ: shop sell — merchant_id={} slot={} qty={}", merchant_id, slot, quantity);
            // No optimistic "Sold" log: the server's OP_ShopPlayerSell echo (apply_shop_player_sell)
            // confirms the real payout + removes the item, so a premature success can't be printed
            // when the sale fails (#269).
        }

        // Open/close a merchant window (POST /trade/open, /trade/close). OP_ShopRequest with
        // command=1 (open) or 0 (close). The server replies with OP_ShopRequest (Open/Close) +
        // OP_ItemPacket(Merchant) items, decoded in packet_handler into gs.merchant_*.
        let trade_req = self.trade.lock().unwrap().take();
        if let Some(cmd) = trade_req {
            let (merchant_id, command) = match cmd {
                TradeCmd::Open(id) => (id, 1u32),
                TradeCmd::Close    => (gs.merchant_open.unwrap_or(0), 0u32),
            };
            if command == 1 {
                // #360: clear before sending — an Open request that never gets an echo (non-merchant
                // target / out-of-range) must not leave `merchant_open` reporting the merchant we had
                // open before this request. begin_shop_open_for keeps an already-open re-open from
                // flickering the window closed (#361 review FIX 2).
                gs.begin_shop_open_for(merchant_id);
            }
            let open = merchant_click(merchant_id, gs.player_id, command);
            stream.send_app_packet(OP_SHOP_REQUEST, &open);
            tracing::info!("EQ: shop {} — merchant_id={}", if command == 1 { "open" } else { "close" }, merchant_id);
            if command == 0 { gs.merchant_open = None; gs.merchant_items.clear(); }
        }
    }

    fn drain_move_item(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Move/equip/unequip an item between inventory slots (OP_MoveItem).
        // MoveItem_Struct (12b): from_slot(u32), to_slot(u32), number_in_stack(u32).
        // number_in_stack MUST be 0 for a whole-item move (equip/unequip/rearrange): EQEmu's
        // SwapItem rejects number_in_stack > 0 for any non-stackable item (inventory.cpp ~2025,
        // "not a stackable item" -> SwapItemResync = the "Inventory Desyncronization" we hit). 0
        // takes the direct-swap/equip path. (A count would only be for splitting a stack.)
        let move_req = self.move_req.lock().unwrap().take();
        if let Some((from_slot, to_slot)) = move_req {
            // build_move_item emits the structured 28-byte RoF2 MoveItem_Struct; a flat 12-byte
            // packet is silently dropped by the server (see build_move_item / eqoxide#11).
            stream.send_app_packet(OP_MOVE_ITEM, &build_move_item(from_slot, to_slot));
            // EQEmu applies the move silently (no echo), so mirror it into our snapshot or
            // /inventory goes stale and the next move corrupts it (phantom items).
            gs.move_item(from_slot as i32, to_slot as i32);
            tracing::info!("EQ: move item — from_slot={} to_slot={} qty=0(whole)", from_slot, to_slot);
            gs.log_msg("inventory", &format!("Moved item (slot {} -> {})", from_slot, to_slot));
        }
    }

    fn apply_fast_steering(&mut self, gs: &mut GameState) {
        // FAST STEERING (#nav-multires). The plans (`path`, `local_path`) are refreshed on the 150ms
        // gate below, but the controller runs at ~100Hz — driving a 150ms-stale heading overshoots
        // every turn by up to RUN_SPEED·0.15 ≈ 6.6u and clips walls (the "not following the line"
        // bug). So each loop (~10ms), re-project the CURRENT position onto the stable fine path and
        // refresh ONLY nav_intent's `wish_dir` (+ facing) — the flags/speed the walker set stay. The
        // carrot slides along the line as we move, so the avatar hugs it through tight turns.
        // `local_i` — NOT a hard-coded 0 — tracks which local_path segment we're on between rebuilds
        // (#311): pinning the projection to segment 0 for the full 150ms gate let it saturate and
        // measure the carrot from behind the walker once RUN_SPEED carried us past it.
        if !self.local_path.is_empty() && self.goto_target.lock().unwrap().is_some() {
            if let Some((wish_dir, heading)) =
                fast_steer_aim(&self.local_path, &mut self.local_i, [gs.player_x, gs.player_y], 5.0)
            {
                if let Some(intent) = self.nav_intent.lock().unwrap().as_mut() {
                    intent.wish_dir = wish_dir;
                }
                gs.player_heading = heading;
            }
        }
    }

    fn drive_auto_target(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Auto-target: while auto-attacking, pick who to fight each tick. Priority (see
        // `pick_combat_target`): a mob that is actively attacking the player (engage adds instead of
        // tanking them unanswered) > a still-valid current target > the nearest reachable trash mob
        // (name starts "a_"/"an_", excluding named guards/merchants/citizens) within ~200u, so
        // grinding continues hands-free between kills.
        if self.auto_attack {
            // Drop attackers that haven't swung at us in a while so a long-dead aggressor or one
            // we've out-run doesn't keep pulling target priority.
            const ATTACKER_TTL: std::time::Duration = std::time::Duration::from_secs(6);
            gs.recent_attackers.retain(|_, t| t.elapsed() < ATTACKER_TTL);

            let col = self.collision.read().unwrap();
            // LINE of sight, not a walkable path: "is this NPC in the open in front of me", used only
            // to drop targets behind a wall. `line_clear` (a centre ray) is the right primitive —
            // `path_clear` now sweeps the player's whole collision volume (#358), which would also
            // reject a perfectly attackable NPC standing in a doorway.
            let clear_to = |e: &crate::game_state::Entity| -> bool {
                col.as_ref().map_or(true, |c| {
                    c.line_clear([gs.player_x, gs.player_y, e.z + 3.0], [e.x, e.y, e.z + 3.0], 2.0)
                })
            };
            let alive_reachable = |id: u32| -> bool {
                gs.entities.get(&id).map(|e| !e.dead && e.is_npc && clear_to(e)).unwrap_or(false)
            };

            let current = gs.target_id;
            // The current target is valid only if alive AND still reachable in a straight line —
            // otherwise drop it so we retarget or roam (don't get stuck swinging "too far").
            let current_valid = current.map(|id| alive_reachable(id)).unwrap_or(false);
            let current_is_attacker = current.map(|id| gs.recent_attackers.contains_key(&id)).unwrap_or(false);

            // The add to engage: the most-recent attacker that is alive + reachable and isn't already
            // our current target. (If the current target is the attacker, `pick_combat_target` keeps it.)
            let attacker = gs.recent_attackers.iter()
                .filter(|(id, _)| Some(**id) != current && alive_reachable(**id))
                .max_by_key(|(_, t)| **t)
                .map(|(id, _)| *id);

            // Nearest reachable trash, only needed as the fallback (no attacker, no valid current).
            let nearest_trash = if attacker.is_none() && !current_valid {
                let mut best: Option<(f32, u32)> = None;
                for (id, e) in &gs.entities {
                    if e.dead || !e.is_npc { continue; }
                    let nl = e.name.to_ascii_lowercase();
                    if !(nl.starts_with("a_") || nl.starts_with("an_")) { continue; }
                    let dx = e.x - gs.player_x;
                    let dy = e.y - gs.player_y;
                    let d2 = dx * dx + dy * dy;
                    if d2 > 200.0 * 200.0 || !clear_to(e) { continue; }
                    if best.map(|(bd, _)| d2 < bd).unwrap_or(true) { best = Some((d2, *id)); }
                }
                best.map(|(_, id)| id)
            } else { None };
            drop(col);

            let desired = pick_combat_target(current, current_valid, current_is_attacker, attacker, nearest_trash);
            // Only send a target packet when the choice actually changes (avoid per-tick spam). If
            // `desired` is None we keep the current target and idle, matching the old behaviour of
            // waiting for a respawn rather than roaming out of a sealed pocket.
            if let Some(id) = desired {
                if Some(id) != current {
                    gs.set_target(id); // also clears stale con/attitude from the old target (#323)
                    stream.send_app_packet(OP_TARGET_MOUSE, &build_target_packet(id));
                }
            }
        }
    }

    fn drive_auto_pet_combat(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Auto-pet-combat: if the player has a pet (e.g. a summoned necro pet), send it to attack
        // the current target. Only (re)issue PET_ATTACK when the target changes, so we don't spam
        // OP_PetCommands every tick. The player's own melee auto-engage (below) still runs, which
        // keeps her walking into loot range while the pet does the damage.
        if let Some(pet) = gs.pet_id {
            // Engage only a reasonably-close LIVE target (<=200u) so the pet doesn't run across the
            // zone after a distant mob and lose itself. When there's no such target (idle, or the
            // mob died), recall the pet with PET_BACKOFF so it returns to the owner instead of
            // wandering off — the previous version left it chasing and it dropped out of view.
            let engage = if self.auto_attack {
                gs.target_id
                    .and_then(|tid| gs.entities.get(&tid).map(|e| (tid, e)))
                    .filter(|(_, e)| {
                        let dx = e.x - gs.player_x; let dy = e.y - gs.player_y;
                        !e.dead && dx * dx + dy * dy <= 200.0 * 200.0
                    })
                    .map(|(tid, _)| tid)
            } else { None };
            match engage {
                Some(tid) if self.last_pet_target != Some(tid) => {
                    stream.send_app_packet(OP_PET_COMMANDS, &build_pet_command(PET_ATTACK, tid));
                    self.last_pet_target = Some(tid);
                    tracing::info!("EQ: pet {pet} → attack target {tid}");
                }
                Some(_) => {} // already attacking this target
                None => {
                    if self.last_pet_target.is_some() {
                        stream.send_app_packet(OP_PET_COMMANDS, &build_pet_command(PET_BACKOFF, 0));
                        self.last_pet_target = None;
                        tracing::info!("EQ: pet {pet} → back off (no target)");
                    }
                }
            }
        } else {
            self.last_pet_target = None;
        }
    }

    /// Returns true if this handled the tick and the caller must stop (melee engage/hold fired).
    fn drive_auto_engage_melee(&mut self, stream: &mut EqStream, gs: &mut GameState) -> bool {
        // Auto-engage: while auto-attacking, walk into melee range of the target and face it so
        // the server registers swings. Closing the last few units via legit walking (not a held
        // far-away face) is what makes melee actually land. Runs regardless of any pending goto.
        if self.auto_attack {
            if let Some(tid) = gs.target_id {
                if let Some((ex, ey)) = gs.entities.get(&tid).map(|e| (e.x, e.y)) {
                    let dx = ex - gs.player_x;
                    let dy = ey - gs.player_y;
                    let dist = (dx * dx + dy * dy).sqrt();
                    if dist < 200.0 { // engage targets within ~200u (sparse spawns; walk to them)
                        const MELEE: f32 = 5.0;
                        const PET_STANDOFF: f32 = 25.0; // pet classes hang back and let the pet tank
                        // With a pet, DON'T walk into melee — the pet holds aggro (PET_ATTACK) and a
                        // squishy caster who closes to melee just gets killed (a level-1 necro died
                        // to a level-4 skeleton this way). Stand off ~25u: out of the mob's melee but
                        // close enough to loot the corpse after the pet kills it.
                        let engage = if gs.pet_id.is_some() { PET_STANDOFF } else { MELEE };
                        let hdg = if dist > 0.01 { eq_heading(dx, dy) } else { gs.player_heading };
                        gs.player_heading = hdg;
                        if dist > engage {
                            // Drive the controller toward the target (it owns collide-and-slide).
                            let swim = self.collision.read().unwrap().as_ref()
                                .is_some_and(|c| c.in_water([gs.player_x, gs.player_y, gs.player_z]));
                            *self.nav_intent.lock().unwrap() = Some(MoveIntent {
                                wish_dir:    [dx / dist, dy / dist],
                                wish_vspeed: 0.0,
                                jump:        false,
                                want_swim:   swim,
                                speed:       RUN_SPEED,
                                climb:       0.0, // nav uses the native step-up now (#239); fences handled by hop
                                hop:         false,                      // melee approach: no auto-hop
                            });
                        } else {
                            // In melee range: stop the controller and face the target so swings land
                            // (IsFacingMob). The explicit send keeps the server's facing current.
                            *self.nav_intent.lock().unwrap() = None;
                            self.send_position_update(stream, gs, gs.player_x, gs.player_y, gs.player_z, hdg);
                        }
                        *self.goto_target.lock().unwrap() = None; // cancel any stale walk
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Returns true if a controlled fall was in progress (handled this tick; caller must stop).
    fn drive_controlled_fall(&mut self, stream: &mut EqStream, gs: &mut GameState) -> bool {
        // Controlled fall in progress: descend at the native rate until landed, then apply native
        // fall damage (client-computed in EQ; the server only validates OP_EnvDamage). Takes
        // priority over normal walking so the descent isn't interrupted.
        if let Some(land_z) = self.falling {
            const FALL_STEP: f32 = 12.0; // ~native per-update descent (under the 12.8 wire cap)
            let next_z = (gs.player_z - FALL_STEP).max(land_z);
            let hdg = gs.player_heading;
            self.send_position_update(stream, gs, gs.player_x, gs.player_y, next_z, hdg);
            gs.player_z = next_z;
            if next_z <= land_z + 0.5 {
                let height = (self.fall_start_z - land_z).max(0.0);
                self.falling = None;
                let (dmg, _max) = fall_damage(height);
                if dmg > 0 {
                    stream.send_app_packet(OP_ENV_DAMAGE, &build_env_damage_packet(gs.player_id, dmg, DMGTYPE_FALLING));
                    gs.cur_hp = (gs.cur_hp - dmg as i32).max(0);
                    gs.log_msg("combat", &format!("Fell {:.0}u — {} fall damage", height, dmg));
                    tracing::info!("EQ: fall damage {dmg} (fell {height:.0}u)");
                }
                tracing::info!("NAV: landed at z={:.1} after {:.0}u fall", land_z, height);
            }
            return true;
        }
        false
    }

    fn drive_chase(&mut self) {
        // Chase (eqoxide#88): when /goto targets a named ENTITY, re-resolve its CURRENT position each
        // tick and follow it, instead of pathing to a one-time snapshot. Roaming mobs move, and their
        // client position is frozen (stale) until they come within the server's ~300u update range —
        // so as the player approaches the stale spot and the mob enters range, its real position is
        // revealed here and the walk homes in on it. If goto_target was cleared (WASD/arrival)
        // while a chase name lingers, the chase is over; if the entity left view, stop cleanly.
        {
            let chase = self.goto_entity.lock().unwrap().clone();
            if let Some(name) = chase {
                if self.goto_target.lock().unwrap().is_none() {
                    *self.goto_entity.lock().unwrap() = None; // cancelled elsewhere
                } else if let Some(&pos) = self.entity_positions.lock().unwrap().get(&name) {
                    *self.goto_target.lock().unwrap() = Some(pos); // follow the entity's latest position
                } else {
                    *self.goto_target.lock().unwrap() = None; // entity despawned / left view
                    *self.goto_entity.lock().unwrap() = None;
                }
            }
        }
    }

    fn drive_teleport_detect(&mut self, gs: &mut GameState) {
        // Teleport detection (#266): a position jump far bigger than one tick of walking (RUN_SPEED
        // ·0.15 ≈ 6.6u) means we were repositioned — an in-zone waterfall teleport, a GM #goto, or a
        // server correction. If we were mid portal-escape, RESTORE the real goal (we're now on the far
        // side of the teleport) and re-plan; any other jump just forces a re-plan off the stale path.
        let jumped = (gs.player_x - self.last_walk_pos[0]).hypot(gs.player_y - self.last_walk_pos[1]) > 40.0;
        self.last_walk_pos = [gs.player_x, gs.player_y, gs.player_z];
        if jumped {
            if let Some(ret) = self.escape_return.take() {
                *self.goto_target.lock().unwrap() = Some(ret);
                tracing::info!("NAV: teleported via in-zone portal — resuming goto to ({:.0},{:.0}) (#266)", ret.0, ret.1);
            }
            self.path_goal = None; // force a re-plan from the new position
            // The fine plan describes ground we are no longer standing on. Steering along it would aim
            // the walker at a line 40+ units away (#382).
            self.clear_local_plan();
        }
        if self.portal_cooldown > 0 { self.portal_cooldown -= 1; }
    }

    /// Resolves the active `/goto` target for this tick, or performs the "no active goto"
    /// stop-and-reset and returns `None` when there is none (caller must stop the tick).
    fn resolve_goal(&mut self) -> Option<(f32, f32, f32)> {
        let goto = *self.goto_target.lock().unwrap(); // copy out so the lock is released
        let goal = match goto {
            Some(t) => t,
            // No active /goto ⇒ the controller must not be nav-driven. Clearing nav_intent here is the
            // catch-all for the invariant "no goto ⇒ no nav movement": any stop that cleared
            // goto_target without also clearing nav_intent would otherwise leave the controller
            // walking the last wish_dir forever (eqoxide#71). Harmless when already None.
            None    => {
                self.path.clear();
                self.path_goal = None;
                self.escape_return = None; // goto cancelled → abandon any in-progress portal escape (#266)
                // Nav stopped — any plan still in flight is for a goal nobody wants. Abandon it so
                // its reply can never be applied (#340). Same for the fine tier (#382).
                self.planner.cancel();
                self.clear_local_plan();
                self.awaiting_first_plan = false;
                *self.nav_intent.lock().unwrap() = None;
                // Only downgrade from an ACTIVE state (an external cancel / WASD). Keep terminal
                // states (arrived / no_path / search_exhausted / blocked) so a driver can still read
                // the last outcome (#166).
                if self.nav_state_is("navigating") || self.nav_state_is("navigating_partial")
                    || self.nav_state_is("planning")
                {
                    self.set_nav_state("idle");
                }
                return None;
            }
        };
        Some(goal)
    }

    /// The walker: (re)plans the coarse/fine route toward `goal`, steers pure-pursuit along
    /// it, and drives arrival/stall/fall-edge handling. This is the tail of the old `tick()` --
    /// every early `return` here is a return from the tick, exactly as before the split.
    fn drive_walk(&mut self, _stream: &mut EqStream, gs: &mut GameState, goal: (f32, f32, f32)) {
        // (Re)compute a wall-avoiding A* path when the goal changes OR the proactive re-plan is armed
        // (#246). find_path returns waypoints (goal-inclusive); an empty path falls back to a straight
        // line to the goal. `replan_coarse` (armed below when the fine plan can't thread the committed
        // coarse route) re-plans from the CURRENT position without wiping the journey-level recovery
        // budget (nav_repaths / nav_best_gdist) — it's normal steering, not a stall recovery.
        if self.replan_cooldown > 0 { self.replan_cooldown -= 1; }
        // A CHASE goal (/follow, /goto <entity>) is rewritten with the leader's live position every
        // tick — the decision function must know that, or a moving leader re-plans forever and the
        // walker never gets a route (#377 review, B1).
        let is_chase = self.goto_entity.lock().unwrap().is_some();
        let in_flight = self.planner.in_flight_goal().map(|g| (g[0], g[1], g[2]));
        let decision = replan_decision(self.path_goal, goal, in_flight, self.replan_coarse, is_chase);
        if decision.reset_route {
            // A genuinely DIFFERENT destination — the committed route is for somewhere else. Drop it
            // and the journey-level recovery budget with it.
            self.path.clear();
            self.clear_local_plan(); // a fine plan aimed at the OLD route's carrot is not a hint, it's a lie
            self.path_i = 0;
            self.stuck_i = 0;
            self.backoff_ticks = 0;
            self.stuck_best = f32::MAX;
            self.stuck_ticks = 0;
            self.nav_repaths = 0;
            self.proactive_replans = 0; // a new destination: the old spot's proactive budget is moot
            self.nav_best_gdist = f32::MAX;
            self.replan_cooldown = 0;
            self.replan_coarse = false;
            self.goal_snapped = false; // a new destination: whatever we snapped for the old one is moot
        }
        if decision.post {
            // NOTE the walker's cursor into the route (`path_i` / `stuck_i`) is deliberately NOT
            // reset here — `apply_plan` resets it when the NEW route is actually installed.
            //
            // When the plan was computed inline this was the same instant, so resetting here was
            // harmless. It is NOT harmless now: the reply lands a tick or two later, and until then
            // the walker is still driving the OLD route. Zeroing its cursor re-aims it at that
            // route's FIRST waypoint — which is behind it, often far behind — so every proactive
            // re-plan (#246) yanked the walker backwards for a tick.
            if !decision.reset_route {
                // Proactive re-plan (#246) or a drifting chase goal: throttle the next one so it can't
                // thrash the planner, and clear the arm flag. Deliberately DO NOT reset `stuck_ticks` —
                // the stall clock must keep running so a genuine wedge the fresh route also can't escape
                // still trips the ~3 s back-off instead of re-planning forever pressed into a wall.
                self.replan_coarse = false;
                self.local_stuck_ticks = 0;
                self.replan_cooldown = REPLAN_COOLDOWN_TICKS;
            }
            // POST the plan to the worker thread and RETURN IMMEDIATELY (#340). This used to call
            // `plan_path` inline — up to ~2 s of synchronous A* on the network thread, which is how
            // two linkdead bugs (#257, #302) happened and why the search carried a 150 ms budget it
            // then lied about hitting (#337). The cost here is now a channel send (microseconds).
            //
            // Route with the native collision radius (1.0, was 2.0): the 2× radius boxed the player
            // out of gaps the native client threads, causing "boxed in by walls" / platform stalls
            // (issues #22/#13/#2). Collide-and-slide in the controller keeps it off walls.
            // Aggro-avoidance (#67): route AROUND live NPC camps so a long goto doesn't plow through
            // a mob group and get the player killed. Exclude NPCs near the GOAL — you're walking TO
            // the destination (often a target mob), so its own camp must not be avoided.
            let av = *self.nav_avoid.lock().unwrap();
            let avoid = Self::aggro_avoid(gs, goal, av.enabled);
            let col = self.collision.read().unwrap().as_ref().cloned(); // Arc clone, not the grid
            match col {
                Some(c) => {
                    // Is this goal a ZONE LINE? Derived from the goal itself rather than carried as
                    // walker state, so it can't go stale behind a /goto issued from the API thread.
                    // The zone-line target is floor-projected (`find_zone_line_near`), so a
                    // `zone_line_at` at standing height there resolves the DRNTP region the char must
                    // end up INSIDE — which is what A* then accepts arrival on, instead of one cell
                    // at a tier the region's z never had (#229). One BSP point query: microseconds.
                    let goal_region = c.zone_line_at([goal.0, goal.1, goal.2 + 1.0]);
                    let t0 = Instant::now();
                    let gen = self.planner.request(crate::nav::planner::PlanRequest {
                        gen: 0, // assigned by the planner
                        start: [gs.player_x, gs.player_y, gs.player_z],
                        goal:  [goal.0, goal.1, goal.2],
                        avoid,
                        aggro_buffer: av.buffer,
                        goal_region,
                        collision: c,
                    });
                    self.path_goal = Some(goal); // the goal the committed/incoming route is FOR
                    let post_us = t0.elapsed().as_micros();
                    tracing::info!("NAV: posted plan #{gen} to ({:.0},{:.0}) — {post_us}us on the net thread (was: the whole A*)",
                        goal.0, goal.1);
                    // Stand still ONLY when there is nothing to walk. If a route is already committed
                    // — a proactive re-plan (#246), or a chase goal that drifted a few units — keep
                    // walking it while the new plan computes: it is still the best information we
                    // have, and freezing the walker on every micro-goal-change is what broke /follow.
                    if self.path.is_empty() {
                        self.awaiting_first_plan = true;
                        self.set_nav_state("planning");
                        *self.nav_intent.lock().unwrap() = None;
                    }
                }
                // Collision not loaded yet (zoning): keep the old straight-line-toward-goal fallback.
                None => {
                    self.planner.cancel();
                    self.path_goal = Some(goal);
                    self.awaiting_first_plan = false;
                    self.path = Vec::new();
                    self.set_nav_state("navigating");
                }
            }
        }

        // Pick up a finished plan. `poll` already DISCARDS a stale reply — one whose generation is
        // not the request we are waiting on (a goal we have since abandoned, a zone change, a death).
        // That generation check is the ONLY staleness guard we need, and it is sound.
        //
        // There used to be a second guard here: `plan_goal == Some(goal)`, an exact f32 compare that
        // DROPPED the reply if the goal had drifted at all since the request. It was a PERMANENT
        // DEADLOCK. `poll()` consumes the reply and clears `pending`, but dropping it here meant
        // `apply_plan` never ran — and `apply_plan` is the only thing that clears `plan_goal`. So
        // `plan_goal` stayed `Some(stale)` forever, `replan_decision` then refused to post while a
        // plan was "in flight", `post` was false forever, and the character sat at
        // `nav_state: planning` PERMANENTLY. `is_dead()` cannot catch it: the worker is alive and
        // idle. It is the same silent lie as the dead-planner bug, through a different door — and it
        // fired on an ordinary sequence: /goto A, then re-aim 20u away before A's plan lands.
        //
        // Note my live /follow verification PASSED ANYWAY, by luck: NPC position updates are sparse
        // relative to the 150ms nav tick, so the reply happened to land in a window where the leader
        // had not moved. A pure-function test caught what live play structurally could not — which is
        // the same argument this PR makes about wall-clock budgets, turned on my own verification.
        //
        // So: APPLY THE ROUTE regardless of drift. A route to where the leader stood 200ms ago is
        // exactly what you want to be walking while the next plan computes. It is also the honest
        // move: we HAVE a route, so walk it, instead of freezing while pretending to think.
        if let Some(reply) = self.planner.poll() {
            if self.apply_plan(reply, gs, goal) { return; }
        }

        // THE PLANNER IS DEAD (its thread panicked). Nothing will ever answer a plan request again,
        // so a character waiting on one would sit at `nav_state: planning` FOREVER — a silent lie,
        // and a strictly worse failure than the loud net-thread panic this architecture replaced.
        // Say so, terminally, and stop. (#337's own principle, applied to this PR's own machinery.)
        if self.planner.is_dead() {
            self.stop_nav(gs, "no_path", "planner_dead", &format!(
                "The pathfinding worker thread has DIED — no route to ({:.0},{:.0}) or anywhere else can be \
                 planned for the rest of this session. This is a client fault, not an unreachable goal; \
                 movement must be driven manually or the client restarted.", goal.0, goal.1));
            return;
        }

        // Still waiting on the first plan for this goal: DO NOT drive. The straight-line fallback
        // below exists for "no collision loaded", not for "the planner hasn't answered yet" — using
        // it here would charge the character at the goal through whatever is in the way.
        if self.awaiting_first_plan {
            *self.nav_intent.lock().unwrap() = None;
            return;
        }

        // PURE-PURSUIT path following. Chasing each discrete waypoint made the walker OVERSHOOT it
        // (~6.6u/tick at RUN_SPEED vs a 3u arrival radius), oscillate at turns, and drift off the
        // path line into walls — the silent neriakc #2 / gfaydark #4 stall. Instead we steer toward
        // a look-ahead point ON the path line, so the avatar hugs the route through tight turns.
        const LOOK_AHEAD: f32 = 5.0;
        let px = gs.player_x;
        let py = gs.player_y;
        // Advance the active segment while our projection onto it has passed its end.
        while self.path_i + 2 < self.path.len() {
            let (a, b) = (self.path[self.path_i], self.path[self.path_i + 1]);
            let ab = [b[0] - a[0], b[1] - a[1]];
            let l2 = ab[0] * ab[0] + ab[1] * ab[1];
            let t = if l2 < 1e-6 { 1.0 } else { ((px - a[0]) * ab[0] + (py - a[1]) * ab[1]) / l2 };
            if t >= 1.0 { self.path_i += 1; } else { break; }
        }
        let have_path = !self.path.is_empty();
        let target: (f32, f32, f32) = if have_path {
            // TWO-TIER (#nav-multires, #382). The coarse 8u route says WHERE to go; a FINE 2u plan,
            // bounded to LOCAL_BOUND around the character and aimed at a carrot ~LOCAL_REACH ahead on
            // that route, says HOW to thread the next few strides of it — the thin ramps and narrow
            // openings the 8u grid cannot resolve.
            //
            // The fine plan used to be computed RIGHT HERE, inline, on the network thread, every nav
            // tick, under a 150ms wall clock. That was the last A* on this thread (mean 15.3ms, worst
            // 358ms, release/akanon) and the last wall-clock budget in the client — a residual stall of
            // the exact class that caused the #257/#302 linkdead drops, and a budget that made the
            // answer unfalsifiable. It is now POSTED to `local_planner` and picked up a tick or two
            // later; the walker keeps steering on the LAST GOOD fine plan meanwhile, exactly as #377
            // does with the coarse route.
            const LOCAL_REACH: f32 = 24.0;   // how far ahead on the coarse route the fine plan aims
            const LOCAL_BOUND: f32 = 40.0;   // the fine search window (keeps it bounded → it terminates)
            let coarse = carrot_along(&self.path, self.path_i, [px, py], LOOK_AHEAD)
                .unwrap_or([goal.0, goal.1, gs.player_z]);
            // 1. PICK UP a finished fine plan, if one landed. `poll` has already discarded any aimed at
            //    a carrot we have since walked past.
            if let Some(reply) = self.local_planner.poll() {
                self.apply_local_plan(reply);
            }

            // 2. A fine plan the walker has been TELEPORTED away from describes ground it is not on.
            //    (Ordinary walking drift — ~6.6u/tick — is absorbed by the fast-steering cursor, which
            //    re-projects the live position onto the path every ~10ms. A 40u jump is not drift.)
            if !self.local_path.is_empty()
                && (px - self.local_from[0]).hypot(py - self.local_from[1]) > LOCAL_BOUND
            {
                self.clear_local_plan();
            }

            // 3. POST a fresh fine plan for where we are NOW. `post_if_idle` is a no-op while one is
            //    already in flight — and note there is deliberately NO way to ASK whether one is (see
            //    `LocalPlanner::post_if_idle`), because the one thing this code must never be able to
            //    write is `if planner.is_planning() { return; }`. The walker cannot wait on a question
            //    it cannot pose. This call is a channel send: microseconds, not a search.
            let local_goal = carrot_along(&self.path, self.path_i, [px, py], LOCAL_REACH).unwrap_or(coarse);
            if let Some(c) = self.collision.read().unwrap().as_ref().cloned() {
                self.local_planner.post_if_idle(crate::nav::planner::LocalRequest {
                    gen: 0, // assigned by the planner
                    start: [px, py, gs.player_z],
                    goal:  local_goal,
                    cell:  LOCAL_CELL,
                    bound: LOCAL_BOUND,
                    // The walker's own long-standing test for "did the fine plan reach the carrot?"
                    // (#246). It lives here, with the walker, because it IS the walker's question.
                    carrot_tol: LOCAL_CELL * 2.0,
                    collision: c,
                });
            }
            // A dead fine worker is NOT terminal — the coarse route still steers the character — but it
            // must not be silent either: the agent is being steered with 8u detail from here on.
            if self.local_planner.is_dead() {
                self.set_nav_local(Some(crate::ipc::NavLocal {
                    state: "planner_dead".into(), reason: "local_planner_dead".into(),
                    stuck_ticks: 0, plan_us: 0,
                }));
            }

            // 4. STEER. Never blocks, never waits — `steer_target` is TOTAL over every state the fine
            //    tier can be in (never asked / in flight / dead / answered with nothing). See its docs:
            //    "the walker cannot stall on the fine plan" is a universal claim, so it is discharged by
            //    a property test over this function, not by a live run that happened to win the race.
            let aim = steer_target(&self.path, self.path_i, &self.local_path, &mut self.local_i,
                [px, py], LOOK_AHEAD, coarse);
            // Publish the walker's ACTUAL committed plan for the nav-debug overlay (#246) so it draws
            // exactly what the walker follows — coarse route + fine local plan — rather than an
            // independent per-frame recompute that over-states how cleanly the walker is steering.
            *self.nav_path_view.lock().unwrap() = (self.path.clone(), self.local_path.clone());
            (aim[0], aim[1], aim[2])
        } else {
            self.clear_local_plan();
            *self.nav_path_view.lock().unwrap() = (Vec::new(), Vec::new());
            // No path computed: straight-line toward the goal at the player's CURRENT height.
            (goal.0, goal.1, gs.player_z)
        };

        let dx   = target.0 - gs.player_x; // east  delta (server_x)
        let dy   = target.1 - gs.player_y; // north delta (server_y)
        let dist = (dx * dx + dy * dy).sqrt();

        // Controlled-fall waypoint: a big single-step drop the walker can't walk down (find_path's
        // last-resort fall edge). Walk to the edge at the CURRENT height, then begin a controlled
        // fall. Refuse if the fall's native damage would likely be lethal — fall damage is
        // client-applied, so an unguarded drop can suicide a squishy character.
        const FALL_TRIGGER: f32 = 18.0; // bigger than a stair/ledge step (the walk STEP_H is 20)
        let drop_to_target = gs.player_z - target.2;
        // A submerged landing (e.g. a surface pool whose solid bottom find_path routed to) is NOT a
        // lethal fall: you splash into water, which negates fall damage in RoF2, then swim across.
        // Only guard DRY drops — otherwise a ground-level pool reads as a big fall and the character
        // gets stuck at the water's edge (#191).
        let water_landing = self.collision.read().unwrap().as_ref()
            .is_some_and(|c| c.in_water([target.0, target.1, target.2 + 3.0]));
        if drop_to_target > FALL_TRIGGER && dist <= STOP_DIST + 8.0 && !water_landing {
            let (_, max_dmg) = fall_damage(drop_to_target);
            if gs.cur_hp > 0 && max_dmg >= gs.cur_hp as u32 {
                tracing::info!("NAV: fall of {:.0}u (up to {} dmg) would exceed {} hp — stopping at ledge",
                    drop_to_target, max_dmg, gs.cur_hp);
                gs.log_msg("zone", "Fall too dangerous (HP too low) — stopped at the ledge");
                self.set_nav_state_because("blocked", Some("fall_would_be_lethal"));
                *self.goto_target.lock().unwrap() = None;
                *self.nav_intent.lock().unwrap() = None; // else the controller keeps walking the last
                // wish_dir forever — drifting 1000s of units with no nav activity (eqoxide#71).
                return;
            }
            self.falling = Some(target.2);
            self.fall_start_z = gs.player_z;
            tracing::info!("NAV: stepping off a {:.0}u drop — controlled fall begins", drop_to_target);
            return;
        }

        // Arrival: measure distance to the FINAL goal, not the look-ahead carrot (which always leads
        // by ~LOOK_AHEAD). A one-shot /goto stops for good; a /follow (live `goto_entity`, re-resolved
        // by the block above) settles a bit behind the leader but STAYS latched so it re-engages the
        // moment the leader walks off — instead of clearing the chase and idling forever (#268).
        let gdx = goal.0 - gs.player_x;
        let gdy = goal.1 - gs.player_y;
        let gdist = (gdx * gdx + gdy * gdy).sqrt();
        let following = self.goto_entity.lock().unwrap().is_some();
        match arrival_action(gdist, following) {
            ArrivalAction::FollowHold => {
                // Caught up — hold, but keep goto_target/goto_entity so a later tick drives again once
                // the leader moves past FOLLOW_DIST.
                self.set_nav_state("following");
                self.path.clear();
                self.path_goal = None;
                *self.nav_intent.lock().unwrap() = None; // stand still until the leader moves
                gs.player_heading = eq_heading(gdx, gdy);
                return;
            }
            ArrivalAction::Arrived => {
                if let Some(ret) = self.escape_return.take() {
                    // Reached the in-zone portal but did NOT teleport (auto-cross cooldown / region miss)
                    // — give up the escape and resume the real goal; portal_cooldown blocks a re-escape
                    // (#266). A portal escape is a plain goto (following==false), so it lands here.
                    tracing::info!("NAV: reached the in-zone portal without teleporting — resuming goto to ({:.0},{:.0})", ret.0, ret.1);
                    *self.goto_target.lock().unwrap() = Some(ret);
                    self.path_goal = None;
                    *self.nav_intent.lock().unwrap() = None;
                    return;
                }
                tracing::info!("NAV: arrived at ({:.1},{:.1})", goal.0, goal.1);
                // `arrived` alone would tell an agent it got what it asked for. If the planner moved
                // its goal's z onto a real floor, it did not — carry that all the way to the end.
                if self.goal_snapped {
                    self.set_nav_state_because("arrived", Some("goal_z_snapped"));
                } else {
                    self.set_nav_state("arrived");
                }
                *self.goto_target.lock().unwrap() = None;
                *self.nav_intent.lock().unwrap() = None; // stop driving the controller
                gs.player_heading = eq_heading(gdx, gdy);
                return;
            }
            ArrivalAction::Drive => {} // not there yet — keep walking / re-plan below
        }

        // Long-route progress (#229): a distant goto (e.g. zone_cross across a big overland zone)
        // crosses several tricky spots, each of which can cost a stall-recovery re-path. Reset the
        // re-path budget whenever we get meaningfully CLOSER to the goal, so the cap counts
        // CONSECUTIVE failed recoveries at one wedge — not the total over a long journey that is
        // otherwise progressing fine. Without this the 8-cap killed long crossings partway.
        const REPATH_RESET_DIST: f32 = 200.0;
        if gdist < self.nav_best_gdist - REPATH_RESET_DIST {
            self.nav_best_gdist = gdist;
            self.nav_repaths = 0;
            // The fine-impassable spot is behind us — the journey is progressing, so forgive the
            // proactive re-plans spent getting past it (#378 Phase 2). This is what keeps the guard
            // below from ever tripping on a long multi-corner journey that is otherwise fine.
            self.proactive_replans = 0;
        }

        // OSCILLATION GUARD (#378 Phase 2 — the live qcat L-corner honesty fix). The proactive coarse
        // re-plan (#246) re-routes around a spot the fine 2u tier cannot thread, and each fresh route
        // resets the stall clock — so `stuck_ticks` never reaches its 20-tick give-up and the walker
        // oscillated `navigating` FOREVER on a corner it could not round toward an around-the-corner
        // goal. If the proactive re-plan has fired PROACTIVE_REPLAN_CAP times without the journey
        // getting meaningfully closer (the reset above), the re-routing is not helping: this is a
        // genuine wedge, and the honest answer is `blocked / local_no_way_through` — NOT a silent loop,
        // and NOT `no_path` (a coarse route to the goal does exist; the walker cannot physically follow
        // it here). `Exhausted`-style "I don't know" is untouched — this fires only on a real,
        // repeatedly-confirmed local dead-end.
        if self.proactive_replans >= PROACTIVE_REPLAN_CAP {
            self.stop_nav(gs, "blocked", "local_no_way_through", &format!(
                "Wedged near ({:.1},{:.1}) after {} proactive coarse re-plans that did not get the \
                 journey past this spot: the fine 2u planner cannot thread the committed route here, \
                 and re-routing keeps returning to the same impasse. The corridor is not traversable at \
                 the character's collision radius from this approach — a coarse route to the goal exists, \
                 but the walker cannot follow it around this corner. Approach from another direction.",
                gs.player_x, gs.player_y, self.proactive_replans));
            return;
        }

        // Active downhill back-off (eqoxide#212): after a hard stall we drive the REVERSE aim for a
        // few ticks to slide off a wedged slope face onto cleaner ground, THEN re-path from there.
        // This complements #205's start re-anchoring: back off first, then the (now grade-limited)
        // plan routes around the face instead of straight back up it.
        if self.backoff_ticks > 0 {
            self.backoff_ticks -= 1;
            *self.nav_intent.lock().unwrap() = Some(MoveIntent {
                wish_dir:    self.backoff_dir,
                wish_vspeed: 0.0,
                jump:        false,
                want_swim:   false,
                speed:       RUN_SPEED,
                // The back-off must move like a HUMAN (native step-up), NOT with the NAV_CLIMB super-
                // step. Its whole purpose is to slide DOWNHILL off a wedged face; with the 20u nav
                // climb it would instead scale the unwalkable slope/ridge it's wedged against and
                // strand itself higher up (#229). climb 0 → the controller uses STEP_UP (2u), so
                // gravity slides it down the face a player couldn't have climbed in the first place.
                climb:       0.0,
                hop:         false,
            });
            if self.backoff_ticks == 0 {
                // Backed off — re-plan from the cleaner spot. POSTED to the worker like every other
                // coarse plan (#340): this used to be a second synchronous `plan_path` on the net
                // thread, and it fired on exactly the stall that made plans slowest. The reply lands
                // on a later tick via `apply_plan`; until then the walker keeps the old route (which
                // it is, by definition, no longer making progress on — a few hundred ms of that
                // changes nothing). If the re-plan still can't route, the honest outcome now stops
                // us with a reason instead of another silent wedge.
                let av = *self.nav_avoid.lock().unwrap();
                let avoid = Self::aggro_avoid(gs, goal, av.enabled);
                let col = self.collision.read().unwrap().as_ref().cloned();
                if let Some(c) = col {
                    let goal_region = c.zone_line_at([goal.0, goal.1, goal.2 + 1.0]);
                    let gen = self.planner.request(crate::nav::planner::PlanRequest {
                        gen: 0,
                        start: [gs.player_x, gs.player_y, gs.player_z],
                        goal:  [goal.0, goal.1, goal.2],
                        avoid,
                        aggro_buffer: av.buffer,
                        goal_region,
                        collision: c,
                    });
                    self.stuck_ticks = 0;
                    tracing::warn!("NAV: backed off downhill — posted re-plan #{gen} (attempt {})", self.nav_repaths);
                }
            }
            return;
        }

        // Progress-based stall detection. Pure-pursuit advances `path_i` steadily as the avatar moves
        // along the route; if it has NOT advanced for NAV_STUCK_TICKS we're genuinely wedged (or the
        // route crosses a spot the capsule controller can't track). Recover by re-pathing from the
        // ACTUAL position onto a route the controller can follow; cap re-paths so a truly unreachable
        // snag stops instead of looping. (A straight-line goto with no path skips this.)
        if have_path {
            if self.path_i > self.stuck_i {
                self.stuck_i = self.path_i;
                self.stuck_ticks = 0;
            } else {
                self.stuck_ticks += 1;
                if self.stuck_ticks >= NAV_STUCK_TICKS {
                    self.stuck_ticks = 0;
                    if self.nav_repaths < 8 {
                        // Count this recovery, then back off DOWNHILL (reverse the aim) before
                        // re-pathing (the re-plan happens when the back-off completes). This clears
                        // a wedged slope face instead of re-planning from the same stuck spot. (#212)
                        self.nav_repaths += 1;
                        self.backoff_ticks = NAV_BACKOFF_TICKS;
                        self.backoff_dir = if dist > 1e-3 { [-dx / dist, -dy / dist] } else { [0.0, 0.0] };
                        tracing::warn!("NAV: no progress near ({:.1},{:.1}) — backing off downhill (attempt {})",
                            gs.player_x, gs.player_y, self.nav_repaths);
                        return;
                    }
                    // `blocked` means ONE thing: the planner gave us a route and the walker could not
                    // follow it. It is no longer the dumping ground for "the goal was unreachable and
                    // nobody said so" — that is `no_path`, reported before a single step is taken
                    // (#337).
                    //
                    // But there are TWO ways to fail to follow a route, and they want different
                    // responses from an agent (#382). If the FINE tier closed its whole 40u window and
                    // found no way along the corridor, the walker is not "sliding on something" — the
                    // corridor is genuinely too tight, and hopping/nudging will not help; the goal
                    // needs approaching another way. If the fine tier was threading happily and the
                    // walker still didn't move, it IS a physics wedge and `/move/manual` may free it.
                    // Collapsing the two into one `walker_stalled` told the agent a confident story
                    // about a cause we had not established.
                    if self.local_says_no_way_through() {
                        self.stop_nav(gs, "blocked", "local_no_way_through", &format!(
                            "Wedged at ({:.1},{:.1}) after {} re-path attempts — and the FINE 2u planner has \
                             CLOSED its whole 40u window without finding a way along the committed route. The \
                             corridor here is not threadable at the character's own collision radius: this is \
                             not a slide/collision wedge, and nudging will not fix it. Approach the goal from \
                             another direction.",
                            gs.player_x, gs.player_y, self.nav_repaths));
                    } else {
                        self.stop_nav(gs, "blocked", "walker_stalled", &format!(
                            "Wedged at ({:.1},{:.1}) after {} re-path attempts — the route is planned, the fine \
                             planner can thread it, but the walker cannot physically follow it. (The goal itself \
                             IS reachable; this is a collision/steering wedge, not a routing failure.)",
                            gs.player_x, gs.player_y, self.nav_repaths));
                    }
                    return;
                }
            }
        }

        // Planner (design §3.5): the walker no longer slides or writes positions. It emits a
        // MoveIntent toward the current waypoint; the render-thread CharacterController owns
        // collide-and-slide, step-up, gravity and the authoritative position. The streamer
        // (stream_position) sends that position to the server. Heading is set from the aim so the
        // render facing and the streamed heading agree.
        let heading = eq_heading(dx, dy);
        gs.player_heading = heading;
        // Swim when we're in water so the controller actively swims across/up to the surface instead
        // of trudging along the bottom (#191). The controller only swims when want_swim && in_water.
        // Probe the BODY, not just the feet: a character standing on a pool bottom can have its
        // origin a hair below the water volume's lower bound while fully submerged (the qcat spawn
        // shaft — floor at -69.97, water -69.5…-43.0), and a feet-only test then says "dry" and the
        // controller trudges instead of swimming. Same probe as movement's `water_at` (#329).
        let swim = self.collision.read().unwrap().as_ref().is_some_and(|c| {
            c.in_water([gs.player_x, gs.player_y, gs.player_z])
                || c.in_water([gs.player_x, gs.player_y, gs.player_z + 3.0])
        });
        // Jump-edge execution (eqoxide#190): if the current path segment is a jump — a horizontal
        // hop bigger than any adjacent nav cell, which find_path only emits across a real gap — ask
        // the controller to jump. Gated on being near the takeoff waypoint so the leap starts
        // grounded at the near edge and doesn't re-trigger on landing; the forward wish_dir carries
        // it across (the ~22.7u reach the edge was sized for). The controller ignores jump unless
        // grounded, so it fires exactly once at takeoff.
        // path[0] is the CHARACTER'S OWN position (find_path starts every route there so pure pursuit
        // walks the first leg — see assets.rs). That opening leg is a plain step onto the nav grid,
        // never one of A*'s jump edges (those span ≥2 cells BETWEEN cell centres), and it can be up
        // to ~1.5 cells long — so testing it against JUMP_SEG_MIN would fire a bogus running jump at
        // the start of every route. Jump edges can only appear from path_i ≥ 1.
        let jump = match (self.path.get(self.path_i), self.path.get(self.path_i + 1)) {
            (Some(a), Some(b)) if self.path_i >= 1 => {
                let seg = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
                let to_takeoff = ((gs.player_x - a[0]).powi(2) + (gs.player_y - a[1]).powi(2)).sqrt();
                seg > JUMP_SEG_MIN && to_takeoff < JUMP_TAKEOFF_DIST
            }
            _ => false,
        };
        // SWIM UP TO THE WAYPOINT (#329). A* emits water-ascent edges — swim up a flooded shaft, then
        // haul out onto a ledge within ~2.5u of the water SURFACE — but nav only ever drove wish_dir
        // horizontally and left the vertical entirely to buoyancy, which parks the character
        // FLOAT_DEPTH (2u) BELOW the surface. A ledge A* considers a legal haul-out from the surface
        // is then up to 4.5u above the character: past the 2u step-up, so it can never get out. That
        // is the second half of the qcat spawn trap — the character correctly floats up the shaft and
        // then bobs at the waterline under a ledge it cannot reach, for ever.
        //
        // So when swimming toward a waypoint the character cannot simply STEP up to, swim up at it,
        // stopping a step short (the step-up then mounts the ledge). Only ever upward: descending is
        // buoyancy's and gravity's business.
        const SWIM_UP_RATE: f32 = 20.0; // u/s, comfortably under the controller's BUOY_RATE (30)
        let wish_vspeed = if swim && target.2 > gs.player_z + 1.0 { SWIM_UP_RATE } else { 0.0 };
        *self.nav_intent.lock().unwrap() = Some(MoveIntent {
            wish_dir:    [dx / dist, dy / dist],
            wish_vspeed,
            jump,
            want_swim:   swim,
            speed:       RUN_SPEED,
            climb:       0.0, // nav uses the native step-up now (#239); fences handled by hop
            // Net progress has stalled toward this waypoint → ask the controller to hop the barrier
            // (it only does if grounded, off cooldown, and a near-level landing exists beyond). (#41)
            hop:         self.stuck_ticks >= NAV_HOP_TICKS,
        });
    }

    /// Advance the quest turn-in (POST /give) trade-window flow. The full sequence is:
    ///   1. New give request: put the item on the cursor (OP_MoveItem from_slot→30, skip if it's
    ///      already on the cursor), send OP_TradeRequest, and enter the "waiting for ack" state.
    ///   2. The server replies OP_TradeRequestAck (→ gs.trade_ack_ready); only then may we move the
    ///      cursor item into the NPC trade slot — the server rejects cursor→trade moves before the
    ///      trade session exists.
    ///   3. Ack seen: OP_MoveItem cursor(30)→trade slot(3000), then OP_TradeAcceptClick. Clear state.
    /// The server then sends OP_FinishTrade (handled in packet_handler). If no ack arrives within
    /// ~3s we abort and reset. Called every tick (not gated by the 150ms walk throttle).
    fn tick_give(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Begin a new give request if one is queued and we're not already mid-trade.
        if self.give_state.is_none() {
            if let Some((npc_id, from_slot)) = self.give.lock().unwrap().take() {
                // Step 1: put the item on the cursor (skip if it's already there). Use the 28-byte
                // structured MoveItem (possessions→cursor); the old flat 12-byte packet was silently
                // dropped by the server, so the item never reached the cursor (eqoxide#26).
                if from_slot != SLOT_CURSOR {
                    stream.send_app_packet(OP_MOVE_ITEM, &build_move_item(from_slot, SLOT_CURSOR));
                    gs.move_item(from_slot as i32, SLOT_CURSOR as i32); // mirror locally
                }
                // Send OP_TradeRequest { to_mob_id = npc, from_mob_id = player }.
                let mut req = [0u8; 8];
                req[0..4].copy_from_slice(&npc_id.to_le_bytes());
                req[4..8].copy_from_slice(&gs.player_id.to_le_bytes());
                stream.send_app_packet(OP_TRADE_REQUEST, &req);
                gs.trade_ack_ready = false;
                self.give_state = Some(GiveState { npc_id, ticks_waiting: 0 });
                tracing::info!("EQ: give: OP_TradeRequest to npc_id={} (item slot {})", npc_id, from_slot);
                gs.log_msg("trade", "Offering item to NPC...");
            }
            return;
        }

        // Mid-trade: either the ack has arrived (advance) or we keep waiting (with a timeout).
        if gs.trade_ack_ready {
            let npc_id = self.give_state.as_ref().map(|g| g.npc_id).unwrap_or(0);
            // Step 3: move the cursor item into the NPC's first trade slot, then accept. The trade
            // slot needs RoF2 typeTrade encoding (not possessions) — build_move_item_to_trade emits
            // the 28-byte structured MoveItem the server actually accepts (eqoxide#26).
            stream.send_app_packet(OP_MOVE_ITEM, &build_move_item_to_trade(SLOT_CURSOR, SLOT_TRADE_BEGIN));
            gs.move_item(SLOT_CURSOR as i32, SLOT_TRADE_BEGIN as i32); // mirror locally
            let mut accept = [0u8; 8];
            accept[0..4].copy_from_slice(&gs.player_id.to_le_bytes());
            // unknown4 = 0 (already zeroed).
            stream.send_app_packet(OP_TRADE_ACCEPT_CLICK, &accept);
            tracing::info!("EQ: give: cursor→trade slot + OP_TradeAcceptClick (npc_id={})", npc_id);
            self.give_state = None;
            gs.trade_ack_ready = false;
        } else if let Some(g) = self.give_state.as_mut() {
            g.ticks_waiting += 1;
            if g.ticks_waiting >= GIVE_ACK_TIMEOUT_TICKS {
                // Abort: cancel the (possibly half-open) trade session and reset.
                stream.send_app_packet(OP_CANCEL_TRADE, &[]);
                tracing::warn!("EQ: give: no trade ack (timed out)");
                gs.log_msg("trade", "Trade timed out (no NPC ack)");
                self.give_state = None;
                gs.trade_ack_ready = false;
            }
        }
    }

    /// Stream the render controller's authoritative position to the server at native cadence
    /// (design §2/§3.4). Runs every tick (not gated by the 150 ms planner). Mirrors the controller's
    /// position into the network `gs` so combat/targeting see the live position, detects genuine
    /// server corrections (>12u jumps the server pushed) and forwards them to the controller, and
    /// sends OP_ClientUpdate at ≤280 ms while moving with a forced 1300 ms keepalive when idle.
    fn stream_position(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        let view = *self.controller_view.lock().unwrap();
        // Don't stream/mirror until the render controller has spawned (else we'd push origin).
        if !view.initialized { return; }
        // Anti-MQGhost keepalive (#105): send a movement-history entry every 30s (< the server's 70s
        // window) whether or not we're moving, so the server's CheatManager never false-flags us.
        if self.last_movement_history_send.elapsed().as_millis() >= MOVEMENT_HISTORY_MS {
            stream.send_app_packet(OP_FLOAT_LIST_THING,
                &build_movement_history(view.pos[0], view.pos[1], view.pos[2]));
            self.last_movement_history_send = Instant::now();
        }
        // A controlled fall owns the Z descent + fall-damage; let it stream, don't fight it here.
        if self.falling.is_some() { return; }
        let gp = [gs.player_x, gs.player_y, gs.player_z];
        if !self.streamed_init {
            self.last_streamed = gp;
            self.last_pos_send = Instant::now();
            self.streamed_init = true;
            return;
        }
        // Genuine server correction: the network gs player jumped (an incoming server packet moved
        // us) far from what we last mirrored. Hand it to the controller; adopt and re-stream it.
        let cd = [gp[0] - self.last_streamed[0], gp[1] - self.last_streamed[1]];
        if cd[0] * cd[0] + cd[1] * cd[1] > CORRECTION_SQ {
            tracing::info!("NAV: server correction → handing controller new pos ({:.1},{:.1},{:.1})", gp[0], gp[1], gp[2]);
            *self.pos_correction.lock().unwrap() = Some(gp);
            self.send_position_update(stream, gs, gp[0], gp[1], gp[2], gs.player_heading);
            self.last_streamed = gp;
            self.last_pos_send = Instant::now();
            return;
        }
        // Normal: stream the controller's position at cadence, then mirror into gs for game logic.
        let pos = view.pos;
        let since = self.last_pos_send.elapsed().as_millis();
        let d = [pos[0] - self.last_streamed[0], pos[1] - self.last_streamed[1], pos[2] - self.last_streamed[2]];
        let moved = d[0] * d[0] + d[1] * d[1] + d[2] * d[2] > 0.01;
        if (moved && since >= POS_SEND_MOVING_MS) || since >= POS_SEND_KEEPALIVE_MS {
            // send_position_update derives deltas from the still-old gs.player_*, so call it first.
            self.send_position_update(stream, gs, pos[0], pos[1], pos[2], view.heading);
            self.last_pos_send = Instant::now();
        }
        gs.player_x = pos[0];
        gs.player_y = pos[1];
        gs.player_z = pos[2];
        gs.player_heading = view.heading;
        self.last_streamed = pos;
    }

    fn send_position_update(
        &mut self,
        stream:  &mut EqStream,
        gs:      &GameState,
        x: f32, y: f32, z: f32,
        heading: f32,
    ) {
        let dx = x - gs.player_x; // east  delta (server_x)
        let dy = y - gs.player_y; // north delta (server_y)
        let dz = z - gs.player_z;
        let moving = dx != 0.0 || dy != 0.0 || dz != 0.0;
        let anim: i32 = if moving { 1 } else { 0 };
        // Internal heading is CCW (0=north, 90=west). The EQ wire (and server) expects
        // CW (0=north, 90=east). The server decodes the wire heading via EQ12toFloat = wire/4,
        // and EQ headings run 0..512 (= 0..360deg), so wire = EQ_units * 4 = deg_cw * 512/360 * 4
        // = deg_cw * 2048/360. (Previously this used 4096/360 = 2x too large, so the server saw
        // the wrong facing and melee never landed — IsFacingMob failed.)
        // Internal heading is CCW (0=north, 90=west). EQ wire expects CW (0=north, 90=east).
        // EQEmu decodes wire heading via EQ12toFloat = wire/4; full circle = 512 EQ units.
        // So wire = cw_degrees * 512/360 * 4 = cw_degrees * 2048/360.
        let h_cw = crate::eq_net::protocol::ccw_to_cw(heading);
        let eq_heading = crate::eq_net::protocol::deg_cw_to_eq12_client(h_cw);

        // RoF2 PlayerPositionUpdateClient_Struct (rof2_structs.h, 46 bytes):
        //   0: sequence(u16)  2: spawn_id(u16)  4: vehicle_id(u16)=0
        //   6: unknown[4]=0   10: delta_x(f32)  14: heading(u32 field, bits 0-11)
        //  18: x_pos(f32)     22: delta_z(f32)  26: z_pos(f32)  30: y_pos(f32)
        //  34: animation(u32 field, bits 0-9)   38: delta_y(f32)
        //  42: delta_heading(u32 field, bits 0-9 signed) = 0
        let mut buf = [0u8; 46];
        buf[0..2].copy_from_slice(&self.position_seq.to_le_bytes()); // sequence
        self.position_seq = self.position_seq.wrapping_add(1);
        buf[2..4].copy_from_slice(&(gs.player_id as u16).to_le_bytes()); // spawn_id
        // vehicle_id = 0 at [4..6], unknown[4] = 0 at [6..10] (already zeroed)
        buf[10..14].copy_from_slice(&dx.to_le_bytes());   // delta_x
        buf[14..18].copy_from_slice(&eq_heading.to_le_bytes()); // heading (12-bit in u32)
        buf[18..22].copy_from_slice(&x.to_le_bytes());    // x_pos (server east)
        buf[22..26].copy_from_slice(&dz.to_le_bytes());   // delta_z
        buf[26..30].copy_from_slice(&z.to_le_bytes());    // z_pos (height)
        buf[30..34].copy_from_slice(&y.to_le_bytes());    // y_pos (server north)
        buf[34..38].copy_from_slice(&anim.to_le_bytes()); // animation (10-bit in u32)
        buf[38..42].copy_from_slice(&dy.to_le_bytes());   // delta_y
        // delta_heading at [42..46] = 0 (already zeroed)
        // Position is a transient firehose — send it UNRELIABLY (ack_req=false), exactly like the
        // native client and the server's own position broadcasts. Sending it on the reliable stream
        // (which we never retransmit) makes a single dropped datagram an unfillable sequence gap, so
        // long continuous runs — which send the most position packets — reliably linkdead (eqoxide#127).
        stream.send_app_packet_unreliable(OP_CLIENT_UPDATE, &buf);
    }

    /// Send OP_ZONE_CHANGE to request crossing a zone line to `target_zone_id`.
    /// ZoneChange_Struct (88 bytes): char_name[64] + zoneID(u16) + instance_id(u16)
    ///   + y(f32) + x(f32) + z(f32) + zone_reason(u32) + success(i32=0)
    /// NOTE: zoneID is sent as **0** (the "resolve from my position" sentinel), NOT the resolved
    /// destination. On zoneID==0 the server (`Handle_OP_ZoneChange`, `zone/zoning.cpp:49`) routes to
    /// `GetClosestZonePointWithoutZone` (`zone.cpp:2093`) — an XY-only, z-agnostic match with no
    /// water-map/OBB check — and derives the real destination from the matched zone point. Sending a
    /// nonzero destination instead routes to `GetClosestZonePoint`, whose water-map `InZoneLine` OBB
    /// test (z-bounded) rejects a valid walk-in with a stale tracked z and logs
    /// `MQZone … with Unknown Destination` (a false positive that could flag/kick on a strict server),
    /// and also hard-cancels if the matched point's target != the named zone. zoneID=0 avoids both.
    /// (`target_zone_id` is kept for logging/clarity; the server resolves the true target itself.)
    /// This is NOT the same as the old bug of sending our *current* zone (target==current → cancel):
    /// 0 is the documented resolve-from-position sentinel, not a zone id. (eqoxide#199)
    fn send_zone_change_packet(&self, stream: &mut EqStream, gs: &GameState, target_zone_id: u16) {
        // RoF2 ZoneChange_Struct is 100 bytes (rof2_structs.h): char_name[64], zoneID@64,
        // instanceID@66, Unknown068@68, Unknown072@72, y@76, x@80, z@84, zone_reason@88,
        // success@92, Unknown096@96. (Titanium put y/x/z at @68/@72/@76 — 8 bytes earlier — which
        // made the RoF2 server read garbage coords and silently ignore the zone-change request.)
        let mut buf = [0u8; 100];
        let name_bytes = gs.player_name.as_bytes();
        let name_len = name_bytes.len().min(64);
        buf[..name_len].copy_from_slice(&name_bytes[..name_len]);
        buf[64..66].copy_from_slice(&0u16.to_le_bytes());             // zoneID = 0 → server resolves from pos (avoids MQZone false positive; eqoxide#199)
        buf[66..68].copy_from_slice(&0u16.to_le_bytes());             // instanceID = 0 (server resolves from matched zone point)
        // @68..76 Unknown068/Unknown072 left zero.
        buf[76..80].copy_from_slice(&gs.player_y.to_le_bytes());      // y (north)
        buf[80..84].copy_from_slice(&gs.player_x.to_le_bytes());      // x (east)
        buf[84..88].copy_from_slice(&gs.player_z.to_le_bytes());      // z
        buf[88..92].copy_from_slice(&0u32.to_le_bytes());             // zone_reason = 0
        buf[92..96].copy_from_slice(&0i32.to_le_bytes());             // success = 0 (request)
        tracing::info!("EQ: sending OP_ZONE_CHANGE target_zone={} from current_zone={} pos=({:.1},{:.1},{:.1})",
                  target_zone_id, gs.zone_id, gs.player_x, gs.player_y, gs.player_z);
        stream.send_app_packet(OP_ZONE_CHANGE, &buf);
    }
}

/// Build a synthetic OP_CLIENT_UPDATE packet so the render loop can update
/// `scene.player_pos` and keep the camera attached during navigation. Uses the real
/// Titanium bit-packed wire format so it decodes the same way as server updates.
/// `heading` (EQ-CCW degrees) carries the nav step direction so the render loop faces
/// the player along the path — server position updates for the player carry no usable
/// heading, so this synthetic packet is the only channel that delivers it.
pub fn make_position_packet(spawn_id: u32, x: f32, y: f32, z: f32, heading: f32) -> AppPacket {
    AppPacket {
        opcode: OP_CLIENT_UPDATE,
        payload: encode_position_update(spawn_id as u16, x, y, z, heading),
    }
}

#[cfg(test)]
mod fine_tier_tests {
    use super::*;
    use crate::nav::collision::{LocalOutcome, NoRoute, PlanLimit};

    /// A tiny deterministic LCG. No new dependency, and a seeded generator means a failure is
    /// reproducible — which a `rand`-seeded property test would not be.
    struct Lcg(u64);
    impl Lcg {
        fn next_u32(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn f32_in(&mut self, lo: f32, hi: f32) -> f32 {
            lo + (self.next_u32() as f32 / u32::MAX as f32) * (hi - lo)
        }
        fn usize_below(&mut self, n: usize) -> usize {
            if n == 0 { 0 } else { self.next_u32() as usize % n }
        }
    }

    fn random_path(rng: &mut Lcg, n: usize) -> Vec<[f32; 3]> {
        (0..n).map(|_| [rng.f32_in(-500.0, 500.0), rng.f32_in(-500.0, 500.0), rng.f32_in(-50.0, 50.0)])
            .collect()
    }

    /// # PROPERTY: **THE WALKER CAN NEVER STALL WAITING ON THE FINE PLAN.** (#382)
    ///
    /// The fine 2u plan now comes back from a worker thread, so on any given tick the fine tier may be
    /// in ANY of these states, and the walker must drive regardless:
    ///
    /// * never asked (`local` empty, first tick of a route)
    /// * still computing (`local` empty, or holding the PREVIOUS plan)
    /// * dead (`local` frozen at whatever it last held, forever)
    /// * answered with nothing usable (`local` empty or a 1-waypoint stub)
    /// * answered with a partial from a position the walker has since driven past
    ///
    /// **Every one of those is just "some `local` slice", and `steer_target` is TOTAL over all of
    /// them.** There is no input for which it has no aim, and therefore no state in which the walker
    /// waits. That is why the fine tier's absence degrades steering instead of blocking it.
    ///
    /// This is a UNIVERSAL claim ("cannot stall"), and a live run cannot discharge a universal — a race
    /// that usually wins is indistinguishable from one that cannot lose. In this very codebase a
    /// `/follow` deadlock passed live verification by luck and was caught only by a pure-function test.
    /// So it is pinned here, over 20k randomised states including every degenerate shape.
    #[test]
    fn the_walker_never_stalls_waiting_on_the_fine_plan() {
        let mut rng = Lcg(0xF382_0001);
        for case in 0..20_000u32 {
            // Every shape the fine tier can hand us, degenerate ones included.
            let local: Vec<[f32; 3]> = match case % 6 {
                0 => Vec::new(),                        // never asked / still computing / dead-empty
                1 => random_path(&mut rng, 1),          // a 1-waypoint stub: steers nowhere
                2 => random_path(&mut rng, 2),          // the minimum usable plan
                3 => { let n = rng.usize_below(30); random_path(&mut rng, 2 + n) } // an ordinary fine plan
                4 => vec![[7.0, 7.0, 0.0]; 4],          // fully degenerate: zero-length segments
                _ => { let n = rng.usize_below(4); random_path(&mut rng, 2 + n) }  // a stale partial
            };
            // ...against every shape of coarse route, since that is the fallback the aim rests on.
            let coarse: Vec<[f32; 3]> = match case % 4 {
                0 => { let n = rng.usize_below(40); random_path(&mut rng, 2 + n) }
                1 => random_path(&mut rng, 2),
                2 => vec![[3.0, 3.0, 0.0]; 3],          // degenerate coarse route
                _ => random_path(&mut rng, 8),
            };
            // ...from anywhere, with ANY cursor value, including ones far past the end of the path (a
            // cursor that outran a plan the worker then replaced with a shorter one).
            let from = [rng.f32_in(-600.0, 600.0), rng.f32_in(-600.0, 600.0)];
            let path_i = rng.usize_below(coarse.len() + 3);
            let mut local_i = rng.usize_below(local.len() + 3);
            let fallback = [rng.f32_in(-600.0, 600.0), rng.f32_in(-600.0, 600.0), 0.0];

            let aim = steer_target(&coarse, path_i, &local, &mut local_i, from, 5.0, fallback);

            // THE PROPERTY: an aim always exists, and it is a real point the walker can be driven at.
            // (`steer_target` returns `[f32;3]`, not `Option` — the no-stall guarantee is in the TYPE.
            // This pins the other half: that no input makes it produce a NaN the controller would
            // silently turn into a frozen wish_dir.)
            assert!(aim.iter().all(|c| c.is_finite()),
                "case {case}: the walker must ALWAYS have a finite aim — there is no fine-tier state in \
                 which it may wait. got {aim:?} (local={} wp, coarse={} wp)", local.len(), coarse.len());
            // And the cursor stays inside the path it indexes, however absurd its starting value.
            assert!(local.len() < 2 || local_i < local.len(),
                "case {case}: the fine cursor must stay in bounds (local_i={local_i}, len={})", local.len());
        }
    }

    /// # PROPERTY: **A LIMIT CAN NEVER BE REPORTED AS "NO WAY THROUGH".** (#382, the #337 disease)
    ///
    /// The proactive coarse re-plan (#246) is armed when the fine tier says the committed route cannot
    /// be threaded from here. Under the deleted 150 ms wall clock it was armed whenever the fine path
    /// merely fell short of the carrot — and a search that *ran out of clock* falls short of the carrot
    /// in exactly the same way a search that *proved the corridor impassable* does. So a TIMEOUT was
    /// laundered into "the route ahead is blocked": under CPU load, corridors that were perfectly
    /// threadable got torn up and re-planned, and (per #379) the coarse tier learned nothing from it and
    /// re-proposed the same corridor forever.
    ///
    /// The two answers are now different VALUES, and only the one that actually looked at the whole
    /// window may arm anything. This is universal over every limit and every partial, so it is a
    /// property, not an example.
    #[test]
    fn a_search_that_stopped_looking_can_never_arm_a_replan() {
        let mut rng = Lcg(0xF382_0002);
        for _ in 0..2_000 {
            let n = rng.usize_below(20);
            let steer = random_path(&mut rng, n);
            // "I stopped looking" (the node cap — the only limit that exists now, #394), and whatever
            // partial it dribbled out.
            {
                let o = LocalOutcome::Exhausted { limit: PlanLimit::NodeCap, steer: steer.clone() };
                assert!(!arms_coarse_replan(&o),
                    "an EXHAUSTED search did not look at the window — treating it as 'the corridor is \
                     blocked' is a limit laundered into a fact");
                assert_ne!(o.state(), "no_way_through",
                    "and it must never be PUBLISHED as 'no way through' either");
            }
            // "I looked at all of it; there is no way" — the only outcome that is evidence of anything.
            for why in [NoRoute::SearchClosed, NoRoute::StartIsolated, NoRoute::GoalNotWalkable, NoRoute::NoGeometry] {
                let o = LocalOutcome::NoWayThrough { steer: steer.clone(), why };
                assert!(arms_coarse_replan(&o),
                    "a CLOSED window IS evidence the coarse corridor is not threadable — it must still \
                     arm the #246 re-plan, or this change trades one bug for a worse one");
            }
            // And a threaded route obviously arms nothing.
            assert!(!arms_coarse_replan(&LocalOutcome::Threaded(steer.clone())));
        }
    }

    /// # PROPERTY: **THE FINE TIER CAN NEVER SAY `no_path`.** (#382)
    ///
    /// `no_path` is the client's DEFINITIVE, falsifiable "there is no route" — the one word an agent is
    /// entitled to act on by giving up on a goal. The fine tier searches a **40 u window**. A closed
    /// window proves nothing whatever about the goal, which is typically hundreds of units away. If the
    /// fine tier could reach `no_path`, a character standing in front of a tight doorway would tell its
    /// agent the destination is unreachable — a confident falsehood, and the single worst thing this
    /// planner can say.
    ///
    /// It cannot, and the reason is structural rather than a guard: `LocalOutcome` has **no variant
    /// that spells a definitive no**, so there is nothing to map. This pins the mapping anyway, because
    /// a future hand could add one.
    #[test]
    fn the_bounded_fine_tier_can_never_report_a_definitive_no_path() {
        let mut rng = Lcg(0xF382_0003);
        for _ in 0..500 {
            let n = rng.usize_below(10);
            let steer = random_path(&mut rng, n);
            let outcomes = [
                LocalOutcome::Threaded(steer.clone()),
                LocalOutcome::NoWayThrough { steer: steer.clone(), why: NoRoute::SearchClosed },
                LocalOutcome::NoWayThrough { steer: steer.clone(), why: NoRoute::StartIsolated },
                LocalOutcome::NoWayThrough { steer: steer.clone(), why: NoRoute::GoalNotWalkable },
                LocalOutcome::Exhausted { limit: PlanLimit::NodeCap, steer: steer.clone() },
            ];
            for o in &outcomes {
                assert_ne!(o.state(), "no_path",
                    "a 40u window can NEVER prove a goal unreachable — the fine tier must have no way to \
                     say `no_path`, or a tight doorway becomes 'your destination does not exist'");
                assert_ne!(o.state(), "search_exhausted",
                    "nor may it borrow the COARSE planner's terminal states: those stop the walker, and a \
                     local dead-end must not");
                // Every outcome carries a steer hint — the walker is never left with nothing to follow.
                assert_eq!(o.steer().len(), steer.len(),
                    "every fine outcome must carry its steering hint (the halas swimmer, #377 review N1)");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A GOAL THE CLIENT CHANGED MUST NOT BE REPORTED AS THE GOAL THE AGENT ASKED FOR.**
    ///
    /// When the caller's `z` sits below every floor in the goal's column, the planner snaps the goal
    /// onto the real floor. That is a good accommodation — but performing it silently makes it a lie:
    /// an agent that asked for `z: 0` would be told `navigating`, then `arrived`, as though it got
    /// what it requested, having actually been walked to `z: 47`. An accommodation presented as
    /// compliance is exactly the class this PR exists to eliminate, so it is surfaced —
    /// `nav_reason: goal_z_snapped`, all the way through to ARRIVAL, plus the message log.
    #[test]
    fn a_snapped_goal_z_is_reported_not_silently_performed() {
        use crate::nav::planner::PlanReply;
        let g: crate::ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(crate::ipc::GroupSnapshot::default()));
        let mut nav = test_action_loop(g);
        let mut gs = GameState::new();
        let goal = (100.0f32, 100.0f32, 0.0f32); // the agent asked for z = 0

        // The planner routed there — but only by moving the goal onto the floor at z = 47.
        nav.apply_plan(PlanReply {
            gen: 1,
            outcome: crate::nav::collision::PlanOutcome::Route(vec![[0.0, 0.0, 47.0], [100.0, 100.0, 47.0]]),
            plan_ms: 5,
            goal_snapped_z: Some(47.0),
            tight: false,
        }, &mut gs, goal);

        let st = nav.nav_state.lock().unwrap().clone();
        assert_eq!(st.state, "navigating");
        assert_eq!(st.reason.as_deref(), Some("goal_z_snapped"),
            "the agent asked for z=0 and is being walked to z=47 — it must be TOLD its goal was changed");
        assert!(gs.messages.iter().any(|m| m.text.contains("CHANGED your goal")),
            "and it must be said in the message log too, in words");

        // ...and it must survive to ARRIVAL. `arrived` with no reason would tell the agent it got
        // exactly what it asked for, which is the whole lie.
        assert!(nav.goal_snapped, "the snap must be carried to arrival, not forgotten en route");

        // A goal whose z WAS honoured reports nothing — the accommodation must not be cried wolf.
        nav.apply_plan(PlanReply {
            gen: 2,
            outcome: crate::nav::collision::PlanOutcome::Route(vec![[0.0, 0.0, 0.0], [100.0, 100.0, 0.0]]),
            plan_ms: 5,
            goal_snapped_z: None,
            tight: false,
        }, &mut gs, goal);
        let st = nav.nav_state.lock().unwrap().clone();
        assert_eq!(st.reason, None, "a goal that was honoured as given carries no snap reason");
        assert!(!nav.goal_snapped);
    }

    /// **`nav_tier` IS PER-ROUTE AND MUST NOT GO STALE (#378 Phase 2, #343 discipline).** The tier is
    /// the fact for the route being walked RIGHT NOW; it must never survive into a state whose route
    /// it does not describe. Repro the review's finding: journey A commits a `preferred` route →
    /// journey B is unreachable → the `no_path` state must NOT still read `preferred` from A. Also
    /// pins that an ARRIVED and an Exhausted `navigating_partial` state carry no stale tier.
    #[test]
    fn nav_tier_does_not_survive_into_a_later_no_path_or_arrived() {
        use crate::nav::collision::{NoRoute, PlanLimit, PlanOutcome};
        use crate::nav::planner::PlanReply;
        let group: crate::ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(crate::ipc::GroupSnapshot::default()));
        let mut nav = test_action_loop(group);
        let mut gs = GameState::new();
        let goal = (100.0f32, 100.0f32, 0.0f32);

        // Journey A: a committed route at the roomy tier → nav_tier = "preferred".
        nav.apply_plan(PlanReply {
            gen: 1,
            outcome: PlanOutcome::Route(vec![[0.0, 0.0, 0.0], [100.0, 100.0, 0.0]]),
            plan_ms: 5, goal_snapped_z: None, tight: false,
        }, &mut gs, goal);
        assert_eq!(nav.nav_state.lock().unwrap().tier, Some("preferred"),
            "a committed preferred route publishes nav_tier = preferred");

        // Journey B: a definitively unreachable goal → no_path. The tier from A must be GONE.
        nav.apply_plan(PlanReply {
            gen: 2,
            outcome: PlanOutcome::Unreachable {
                reason: NoRoute::SearchClosed, goal_blocked_by: None, frontier_blocked_by: None },
            plan_ms: 5, goal_snapped_z: None, tight: false,
        }, &mut gs, goal);
        let st = nav.nav_state.lock().unwrap().clone();
        assert_eq!(st.state, "no_path");
        assert_eq!(st.tier, None,
            "nav_tier must NOT survive from journey A into journey B's no_path (the #343 stale-field lie)");

        // A fresh minimum-tier route, then an Exhausted partial: the partial is not a confirmed route,
        // so it must carry no tier either.
        nav.apply_plan(PlanReply {
            gen: 3,
            outcome: PlanOutcome::Route(vec![[0.0, 0.0, 0.0], [50.0, 50.0, 0.0]]),
            plan_ms: 5, goal_snapped_z: None, tight: true,
        }, &mut gs, goal);
        assert_eq!(nav.nav_state.lock().unwrap().tier, Some("minimum"));
        nav.apply_plan(PlanReply {
            gen: 4,
            outcome: PlanOutcome::Exhausted {
                limit: PlanLimit::NodeCap,
                progress: Some(vec![[0.0, 0.0, 0.0], [60.0, 60.0, 0.0], [90.0, 90.0, 0.0]]) },
            plan_ms: 5, goal_snapped_z: None, tight: false,
        }, &mut gs, goal);
        let st = nav.nav_state.lock().unwrap().clone();
        assert_eq!(st.state, "navigating_partial");
        assert_eq!(st.tier, None, "an Exhausted partial walk is not a confirmed route — it carries no tier");

        // And an arrived state (reached via set_nav_state) after a committed route carries no stale tier.
        nav.apply_plan(PlanReply {
            gen: 5,
            outcome: PlanOutcome::Route(vec![[0.0, 0.0, 0.0], [100.0, 100.0, 0.0]]),
            plan_ms: 5, goal_snapped_z: None, tight: false,
        }, &mut gs, goal);
        assert_eq!(nav.nav_state.lock().unwrap().tier, Some("preferred"));
        nav.set_nav_state("arrived");
        assert_eq!(nav.nav_state.lock().unwrap().tier, None,
            "arrival ends the route — its tier must not linger");
    }

    /// Build a minimal ActionLoop for unit tests that only exercise a single `sync_*`/tick method —
    /// every other shared slot gets an empty/default placeholder.
    fn test_action_loop(group: crate::ipc::GroupShared) -> ActionLoop {
        ActionLoop::new(
            Default::default(), // goto_target
            std::sync::Arc::new(std::sync::Mutex::new(crate::ipc::NavStatus::default())), // nav_state
            Default::default(), // goto_entity
            Default::default(), // entity_positions
            Default::default(), // entity_ids
            Default::default(), // zone_points
            Default::default(), // task_log
            Default::default(), // task_offers_shared
            Default::default(), // completed_tasks_shared
            Default::default(), // accept_task
            Default::default(), // cancel_task
            group,               // group
            Default::default(), // group_invite
            Default::default(), // trainer_open_req
            Default::default(), // trainer_train_req
            Default::default(), // group_accept
            Default::default(), // group_decline
            Default::default(), // group_leave
            Default::default(), // group_kick
            Default::default(), // group_make_leader
            Default::default(), // zone_cross
            Default::default(), // hail
            Default::default(), // say
            Default::default(), // target
            Default::default(), // who_req
            Default::default(), // friends_list
            Default::default(), // friends_req
            Default::default(), // attack
            Default::default(), // buy
            Default::default(), // sell
            Default::default(), // trade
            Default::default(), // merchant
            Default::default(), // move_req
            Default::default(), // give
            Default::default(), // inventory
            Default::default(), // loot
            Default::default(), // door_click
            Default::default(), // doors_shared
            Default::default(), // messages
            Default::default(), // dialogue
            Default::default(), // dialogue_click
            Default::default(), // chat_events
            Default::default(), // chat_send
            Default::default(), // cast
            Default::default(), // mem_spell
            Default::default(), // sit
            Default::default(), // consider
            Default::default(), // pet_cmd
            Default::default(), // collision
            std::path::PathBuf::new(), // maps_dir
            Default::default(), // camp
            Default::default(), // controller_view
            Default::default(), // nav_intent
            Default::default(), // pos_correction
            Default::default(), // nav_path_view
            Default::default(), // nav_avoid
            Default::default(), // read_book
            Default::default(), // guild
            Default::default(), // guild_action
        )
    }

    #[test]
    fn dead_player_halts_navigation() {
        // #238: a character that dies mid-goto must stop — the corpse must not keep walking the route.
        // Seed an in-progress nav, then assert nav_halt_if_dead() clears everything and reports dead.
        let seed_nav = |nav: &mut ActionLoop| {
            *nav.goto_target.lock().unwrap() = Some((100.0, 200.0, 0.0));
            *nav.goto_entity.lock().unwrap() = Some("a bat".into());
            *nav.nav_intent.lock().unwrap() = Some(crate::movement::MoveIntent::default());
            *nav.nav_path_view.lock().unwrap() = (vec![[0.0, 0.0, 0.0]], vec![[0.0, 0.0, 0.0]]);
            nav.path = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0]];
            nav.local_path = vec![[0.0, 0.0, 0.0]];
            nav.path_goal = Some((100.0, 200.0, 0.0));
            nav.path_i = 1;
            nav.local_i = 1;
            *nav.nav_state.lock().unwrap() = "navigating".into();
        };
        let assert_halted = |nav: &ActionLoop| {
            assert!(nav.goto_target.lock().unwrap().is_none(), "goto_target must clear on death");
            assert!(nav.goto_entity.lock().unwrap().is_none(), "goto_entity must clear on death");
            assert!(nav.nav_intent.lock().unwrap().is_none(), "nav_intent must clear so the controller stops");
            assert!(nav.path.is_empty() && nav.local_path.is_empty(), "route must clear on death");
            // The fast-steering cursor must reset with the path it indexes (#311) — a stale local_i
            // left over a cleared/rebuilt local_path aims the walker at the wrong segment.
            assert_eq!(nav.local_i, 0, "local_i must reset with local_path on death");
            assert_eq!(nav.path_goal, None);
            assert_eq!(*nav.nav_state.lock().unwrap(), "idle");
        };
        let new_nav = || {
            let g: crate::ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(crate::ipc::GroupSnapshot::default()));
            test_action_loop(g)
        };

        // (a) An HP-to-0 update that arrives BEFORE OP_Death (player_dead still false) — the exact
        //     window in which the corpse was seen walking. cur_hp<=0 with a known max must halt nav.
        let mut nav = new_nav();
        seed_nav(&mut nav);
        let mut gs = GameState::new();
        gs.player_dead = false;
        gs.cur_hp = 0;
        gs.max_hp = 1284;
        assert!(nav.nav_halt_if_dead(&gs), "cur_hp<=0 (pre-OP_Death) must halt navigation");
        assert_halted(&nav);

        // (b) The OP_Death flag path (player_dead set, cur_hp already zeroed by apply_death).
        let mut nav = new_nav();
        seed_nav(&mut nav);
        let mut gs = GameState::new();
        gs.player_dead = true;
        gs.cur_hp = 0;
        gs.max_hp = 1284;
        assert!(nav.nav_halt_if_dead(&gs));
        assert_halted(&nav);

        // (c) A LIVE player must NOT be halted (and cur_hp<=0 with max_hp==0 = "unknown", not dead —
        //     e.g. a fresh spawn before the first HP update — must not spuriously stop nav).
        let mut nav = new_nav();
        seed_nav(&mut nav);
        let mut gs = GameState::new();
        gs.player_dead = false;
        gs.cur_hp = 900;
        gs.max_hp = 1284;
        assert!(!nav.nav_halt_if_dead(&gs), "a live player must keep navigating");
        assert!(nav.goto_target.lock().unwrap().is_some(), "live nav must be untouched");
        gs.cur_hp = 0;
        gs.max_hp = 0; // unknown HP, not a death
        assert!(!nav.nav_halt_if_dead(&gs), "cur_hp<=0 with max_hp==0 is unknown HP, not death");
        assert!(nav.goto_target.lock().unwrap().is_some());
    }

    #[test]
    fn zone_change_resets_stale_destination_and_path() {
        // #248: a destination + route left over from the PREVIOUS zone must not survive a crossing —
        // in the new zone's coordinate space they aim the walker at a corner near the arrival point
        // and wedge it there. sync_zone_points must clear the goal, path, and recovery state.
        let group: crate::ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(crate::ipc::GroupSnapshot::default()));
        let mut nav = test_action_loop(group);

        // Simulate an in-progress nav in the OLD zone.
        nav.current_zone = "gfaydark".into();
        *nav.goto_target.lock().unwrap() = Some((100.0, 200.0, 0.0));
        *nav.goto_entity.lock().unwrap() = Some("a bat".into());
        *nav.nav_intent.lock().unwrap() = Some(crate::movement::MoveIntent::default());
        *nav.nav_path_view.lock().unwrap() = (vec![[0.0, 0.0, 0.0]], vec![[0.0, 0.0, 0.0]]);
        nav.path = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0]];
        nav.local_path = vec![[0.0, 0.0, 0.0]];
        nav.path_goal = Some((100.0, 200.0, 0.0));
        nav.path_i = 1;
        nav.local_i = 1;
        nav.stuck_ticks = 5;
        nav.nav_repaths = 3;
        nav.backoff_ticks = 2;
        nav.replan_coarse = true;
        nav.falling = Some(0.0);
        *nav.nav_state.lock().unwrap() = "blocked".into();

        // Cross into a NEW zone.
        let mut gs = GameState::new();
        gs.zone_name = "crushbone".into();
        nav.sync_zone_points(&gs);

        // Destination + route + recovery state all cleared; walker comes to rest in the new zone.
        assert!(nav.goto_target.lock().unwrap().is_none(), "goto_target must clear on zone change");
        assert!(nav.goto_entity.lock().unwrap().is_none(), "goto_entity must clear on zone change");
        assert!(nav.nav_intent.lock().unwrap().is_none(), "nav_intent must clear so the controller stops");
        let (coarse, fine) = &*nav.nav_path_view.lock().unwrap();
        assert!(coarse.is_empty() && fine.is_empty(), "overlay path must clear on zone change");
        assert!(nav.path.is_empty() && nav.local_path.is_empty(), "route must clear on zone change");
        assert_eq!(nav.path_goal, None);
        assert_eq!(nav.path_i, 0);
        // The fast-steering cursor must reset with the path it indexes (#311) — a stale local_i in
        // the NEW zone points at a segment of a route that no longer exists.
        assert_eq!(nav.local_i, 0, "local_i must reset with local_path on zone change");
        assert_eq!(nav.stuck_ticks, 0);
        assert_eq!(nav.nav_repaths, 0);
        assert_eq!(nav.proactive_replans, 0, "the oscillation budget must reset on zone change");
        assert_eq!(nav.backoff_ticks, 0);
        assert!(!nav.replan_coarse);
        assert!(nav.falling.is_none());
        assert_eq!(*nav.nav_state.lock().unwrap(), "idle");
        assert_eq!(nav.current_zone, "crushbone");
    }

    /// **THE OSCILLATION GUARD counts proactive re-plans (#378 Phase 2).** A repeatedly-`NoWayThrough`
    /// fine tier must ARM the proactive coarse re-plan AND bump the oscillation budget, so a spot the
    /// fine tier cannot thread cannot loop `navigating` forever (the live qcat L-corner). A `Threaded`
    /// fine plan resets the local-stuck run (the wedge is over) but does NOT retroactively forgive the
    /// budget — only real journey progress (in `tick`) does that.
    #[test]
    fn proactive_replan_arms_and_counts_toward_the_oscillation_budget() {
        use crate::nav::collision::{LocalOutcome, NoRoute};
        let group: crate::ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(crate::ipc::GroupSnapshot::default()));
        let mut nav = test_action_loop(group);

        let nwt = |start: [f32; 3]| crate::nav::planner::LocalReply {
            gen: 1, start, goal: [start[0] + 40.0, start[1], start[2]],
            outcome: LocalOutcome::NoWayThrough { steer: vec![start], why: NoRoute::SearchClosed },
            plan_us: 100,
        };
        // Healthy walker, cooldown clear: NAV_LOCAL_STUCK_TICKS consecutive NoWayThrough plans arm the
        // proactive re-plan on the LAST one, and that arming bumps the oscillation budget exactly once.
        nav.backoff_ticks = 0;
        nav.stuck_ticks = 0;
        nav.replan_cooldown = 0;
        for _ in 0..NAV_LOCAL_STUCK_TICKS {
            nav.apply_local_plan(nwt([0.0, 0.0, 0.0]));
        }
        assert!(nav.replan_coarse, "NoWayThrough × NAV_LOCAL_STUCK_TICKS must arm the proactive re-plan");
        assert_eq!(nav.proactive_replans, 1, "arming the proactive re-plan bumps the oscillation budget");

        // A Threaded plan ends the local-stuck run but must not forgive the budget (only tick's
        // progress reset does): the fine tier finding one way through does not prove the wedge gone.
        nav.apply_local_plan(crate::nav::planner::LocalReply {
            gen: 2, start: [0.0, 0.0, 0.0], goal: [40.0, 0.0, 0.0],
            outcome: LocalOutcome::Threaded(vec![[0.0, 0.0, 0.0], [40.0, 0.0, 0.0]]), plan_us: 100,
        });
        assert_eq!(nav.local_stuck_ticks, 0, "a threaded fine plan resets the local-stuck run");
        assert_eq!(nav.proactive_replans, 1, "a threaded fine plan must not forgive the oscillation budget");

        // The cap is a real, small bound — the guard is not a no-op.
        assert!(PROACTIVE_REPLAN_CAP > 0 && PROACTIVE_REPLAN_CAP <= 16);
    }

    #[test]
    fn sync_group_publishes_own_and_other_member_hp_pct() {
        use crate::game_state::{Entity, GroupMember};
        let mut gs = GameState::new();
        gs.player_name = "Aldric".into();
        gs.hp_pct = 88.0;
        gs.group_leader = "Aldric".into();
        gs.group_members = vec![
            GroupMember { name: "Aldric".into(), is_leader: true, level: 10, ..Default::default() },
            GroupMember { name: "Sariel".into(), level: 8, ..Default::default() },
        ];
        gs.upsert_entity(Entity {
            spawn_id: 99, name: "Sariel".into(), level: 8, is_npc: false,
            x: 0.0, y: 0.0, z: 0.0, hp_pct: 42.0, cur_hp: 42, max_hp: 100, race: "HUM".into(),
            heading: 0.0, dead: false, equipment: [0; 9], equipment_tint: [[0; 3]; 9],
            gender: 0, helm: 0, showhelm: 0, face: 0, hairstyle: 0, haircolor: 0, animation: 100, floating: false,
        });

        let group: crate::ipc::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(crate::ipc::GroupSnapshot::default()));
        let nav = test_action_loop(group.clone());
        nav.sync_group(&gs);

        let snap = group.lock().unwrap();
        assert_eq!(snap.leader, "Aldric");
        assert!(snap.you_are_leader);
        let aldric = snap.members.iter().find(|m| m.name == "Aldric").unwrap();
        assert_eq!(aldric.hp_pct, 88.0); // own HP comes from gs.hp_pct, not gs.entities
        let sariel = snap.members.iter().find(|m| m.name == "Sariel").unwrap();
        assert_eq!(sariel.hp_pct, 42.0); // other member's HP comes from the matching Entity
    }

    #[test]
    fn build_movement_history_layout() {
        // EQEmu UpdateMovementEntry is a packed 17-byte struct: Y@0, X@4, Z@8, type@12, ts@13.
        // Must be >= sizeof(UpdateMovementEntry) or the server debug-logs + ignores it (#105).
        let p = build_movement_history(10.0, -20.0, 3.5);
        assert_eq!(p.len(), 17, "UpdateMovementEntry is 17 packed bytes");
        assert_eq!(&p[0..4], &(-20.0f32).to_le_bytes(), "Y field @0 = server north");
        assert_eq!(&p[4..8], &(10.0f32).to_le_bytes(), "X field @4 = server east");
        assert_eq!(&p[8..12], &(3.5f32).to_le_bytes(), "Z field @8");
        assert_eq!(p[12], 1, "type = Collision (benign; skips teleport/zoneline cheat checks)");
    }
}
