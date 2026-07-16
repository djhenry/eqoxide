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
use crate::ipc::{TradeCmd, CampReq, CampCmd};
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
// moved to `crate::nav::steering` (cleanup step 2 — nav must not live inside net); the walker
// methods that used them live in `crate::nav::walker::Walker` now too (M1 extraction), so this
// module only needs `eq_heading` for its own remaining melee-approach/position-packet code below
// (the tests module still exercises a couple of `nav::steering` consts directly — see its own
// `use`).
use crate::coord::eq_heading;


pub struct ActionLoop {
    /// `/v1/move/*` slots (#M4 — see `ipc::NavSlots`).
    nav:              crate::ipc::NavSlots,
    /// The live entity registry + zone exit points (#M4 — see `ipc::WorldSlots`).
    world:            crate::ipc::WorldSlots,
    /// `/v1/quests/*` slots (#M4 — see `ipc::QuestSlots`).
    quest:            crate::ipc::QuestSlots,
    /// `/v1/group/*` slots (#M4 — see `ipc::GroupSlots`).
    group_slots:      crate::ipc::GroupSlots,
    /// `/v1/trainer/*` slots (#M4 — see `ipc::TrainerSlots`).
    trainer:          crate::ipc::TrainerSlots,
    /// The typed write-path facade (#446). Combat is fully migrated onto it — this thread drains
    /// combat commands via `self.command.take_*` (no direct `ipc::CombatSlots` field any more);
    /// other domains still use their own bundle fields until Wave-2 migrates them. See
    /// `crate::command_state`.
    command:          crate::command_state::CommandState,
    /// GET /v1/observe/who registers a oneshot here; drained in `tick` to send OP_WhoAllRequest.
    /// Client-local friends list + a pending friends-presence poll mirror the same shape (#300/#301,
    /// #M4 — see `ipc::SocialSlots`).
    social:           crate::ipc::SocialSlots,
    /// Held between sending the `/who` request and receiving OP_WhoAllResponse; fired by
    /// `fulfill_who`. (#300)
    pending_who:      Option<tokio::sync::oneshot::Sender<Vec<crate::game_state::WhoEntry>>>,
    /// The OP_FriendsWho reply arrives on the SAME opcode as /who all (OP_WhoAllResponse), so
    /// `expecting_friends` records that the next such reply is a friends poll, not a /who all. (#301)
    pending_friends:  Option<tokio::sync::oneshot::Sender<Vec<crate::game_state::WhoEntry>>>,
    expecting_friends: bool,
    /// `/v1/merchant/*` slots (#M4 — see `ipc::MerchantSlots`).
    merchant_slots:   crate::ipc::MerchantSlots,
    /// `/v1/inventory/*` slots (#M4 — see `ipc::InventorySlots`).
    inventory_slots:  crate::ipc::InventorySlots,
    /// Camp request slot, shared with the gameplay loop. The nav thread only WRITES it — when the
    /// `/camp` chat keyword is typed it pushes a `Toggle` here instead of sending the text as Say.
    /// Not part of `ipc::LifecycleSlots`: this is the only lifecycle field the nav thread touches
    /// (`camp_until`/`respawn` go straight to `eq_net::gameplay::run_gameplay_phase`), so bundling
    /// the whole triple here would just be a field `ActionLoop` never reads.
    camp:             CampReq,
    /// In-progress quest turn-in (POST /give), or None when idle. Drives the trade-window
    /// state machine across nav ticks (request → wait for ack → move item + accept).
    give_state:       Option<GiveState>,
    /// `/v1/interact/*` slots — hail, say, loot, give, doors, sit/stand, dialogue, read (#M4 — see
    /// `ipc::InteractSlots`).
    interact:         crate::ipc::InteractSlots,
    /// Outgoing chat + async events + the message log (#M4 — see `ipc::ChatSlots`).
    chat:             crate::ipc::ChatSlots,
    collision:        crate::nav::collision::SharedCollision,
    maps_dir:         std::path::PathBuf,
    current_zone:     String,
    last_zone_cross:  Instant,
    position_seq:     u16,
    last_tick:        Instant,
    /// Whether auto-attack is currently engaged (set by the /attack toggle). While true and a
    /// target is set, the nav thread keeps the player facing the target so melee swings land.
    auto_attack:      bool,
    /// The path-walker (M1 extraction, #eq-dev-process) — the `/goto` route, stall/backoff/
    /// oscillation recovery, and arrival. Holds its OWN clones of `nav`/`world`/`collision` (the
    /// same shared state as this struct's own fields, not a copy of it) plus the pathfinding
    /// workers, which it owns exclusively. See `crate::nav::walker` for the intent-only movement
    /// boundary: `Walker` writes ONLY `controller.nav_intent`, never a position or the controller.
    walker:           crate::nav::walker::Walker,
    /// The spawn id the pet was last ordered to attack (avoids re-spamming OP_PetCommands every
    /// tick). Reset when the target changes; see the auto-pet-combat block.
    last_pet_target:  Option<u32>,
    /// Single-authority controller integration (design §2): `controller_view` is the render
    /// thread's authoritative position snapshot we stream to the server; `nav_intent` is the
    /// `/goto` planner's per-frame wish written for the render controller; `pos_correction` hands a
    /// genuine server correction back to the controller (#M4 — see `ipc::ControllerSlots`).
    controller:       crate::ipc::ControllerSlots,
    /// `/v1/guild/*` slots (#M4 — see `ipc::GuildSlots`).
    guild_slots:      crate::ipc::GuildSlots,
    /// Last time we sent OP_FloatListThing (movement history) — the anti-MQGhost keepalive (#105).
    last_movement_history_send: Instant,
    /// Last position we streamed, and the last-send timestamp (for the 280 ms / 1300 ms cadence).
    last_streamed:    [f32; 3],
    last_pos_send:    Instant,
    streamed_init:    bool,
}

impl ActionLoop {
    /// Takes the M4 domain bundles (see `ipc.rs`) rather than ~59 flat slot params. Each bundle
    /// passed here MUST be a `.clone()` of the SAME bundle `main.rs` also hands to `HttpState` —
    /// that shared-Arc identity (not a fresh `Default::default()` bundle) is what keeps this the
    /// same cross-thread channel the HTTP/agent side writes into. See `ipc.rs` module docs.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        nav:             crate::ipc::NavSlots,
        world:           crate::ipc::WorldSlots,
        quest:           crate::ipc::QuestSlots,
        group_slots:     crate::ipc::GroupSlots,
        trainer:         crate::ipc::TrainerSlots,
        command:         crate::command_state::CommandState,
        social:          crate::ipc::SocialSlots,
        merchant_slots:  crate::ipc::MerchantSlots,
        inventory_slots: crate::ipc::InventorySlots,
        interact:        crate::ipc::InteractSlots,
        chat:            crate::ipc::ChatSlots,
        controller:      crate::ipc::ControllerSlots,
        guild_slots:     crate::ipc::GuildSlots,
        collision:       crate::nav::collision::SharedCollision,
        maps_dir:        std::path::PathBuf,
        camp:            CampReq,
    ) -> Self {
        let walker = crate::nav::walker::Walker::new(
            nav.clone(), world.clone(), collision.clone(), controller.nav_intent.clone(),
        );
        ActionLoop {
            nav,
            world,
            quest,
            group_slots,
            trainer,
            command,
            social,
            pending_who: None,
            pending_friends: None,
            expecting_friends: false,
            merchant_slots,
            inventory_slots,
            camp,
            give_state: None,
            interact,
            chat,
            collision,
            maps_dir,
            current_zone: String::new(),
            last_zone_cross: Instant::now(),
            position_seq: 0,
            last_tick: Instant::now(),
            auto_attack: false,
            walker,
            last_pet_target: None,
            controller,
            guild_slots,
            last_streamed: [0.0, 0.0, 0.0],
            last_pos_send: Instant::now(),
            last_movement_history_send: Instant::now(),
            streamed_init: false,
        }
    }

    /// Copy all entity positions from `gs` into the shared entity map
    /// (used by the HTTP /entities endpoint and /goto by-name lookup).
    pub fn sync_entities(&self, gs: &GameState) {
        let mut map = self.world.entity_positions.lock().unwrap();
        let mut ids = self.world.entity_ids.lock().unwrap();
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
        let mut log = self.quest.task_log.lock().unwrap();
        log.clear();
        let mut tasks: Vec<_> = gs.tasks.values().cloned().collect();
        tasks.sort_by_key(|t| t.task_id);
        log.extend(tasks);
        drop(log);

        let mut offers = self.quest.task_offers_shared.lock().unwrap();
        offers.clear();
        offers.extend(gs.task_offers.iter().cloned());
        drop(offers);

        let mut completed = self.quest.completed_tasks_shared.lock().unwrap();
        completed.clear();
        completed.extend(gs.completed_task_history.iter().cloned());
    }

    /// Publish the group roster from `gs` into the shared slot (GET /v1/group/roster + the UI
    /// roster panel). Looks up each other member's HP% from `gs.entities` by name (group
    /// membership is what unlocks receiving another mob's OP_MobHealth percent, so this reuses
    /// existing Entity.hp_pct rather than needing a new opcode); the player's own HP% comes
    /// directly from `gs.hp_pct` since the player is never in `gs.entities`.
    pub fn sync_group(&self, gs: &GameState) {
        let mut g = self.group_slots.group.lock().unwrap();
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
        let mut g = self.guild_slots.guild.lock().unwrap();
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
        let mut inv = self.inventory_slots.inventory.lock().unwrap();
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
        let mut m = self.merchant_slots.merchant.lock().unwrap();
        m.open = gs.merchant_open.is_some();
        m.merchant_id = gs.merchant_open;
        m.items.clear();
        m.items.extend(gs.merchant_items.iter().cloned());
    }

    /// Publish the in-game message log from `gs` into the shared slot (GET /messages), converting
    /// each LogEntry into a serializable MessageEntry and extracting `[bracketed]` quest keywords
    /// (the same splitter the HUD dialogue panel uses).
    pub fn sync_messages(&self, gs: &GameState) {
        let mut out = self.chat.messages.lock().unwrap();
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
        *self.interact.dialogue.lock().unwrap() = gs.dialogue_choices.clone();
        // Publish async events (GET /v1/events/*), preserving their stable monotonic ids.
        let mut ev = self.chat.chat_events.lock().unwrap();
        ev.clear();
        ev.extend(gs.chat_events.iter().map(|e| crate::ipc::Event {
            id: e.id, category: e.category.clone(), kind: e.kind.clone(),
            from: e.from.clone(), directed: e.directed, text: e.text.clone(),
        }));
    }

    /// Publish the current zone's doors from `gs` into the shared slot (GET /doors).
    pub fn sync_doors(&self, gs: &GameState) {
        let mut out = self.interact.doors_shared.lock().unwrap();
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
            self.walker.reset_for_zone_change();

            let mut shared = self.world.zone_points.lock().unwrap();
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
            let mut shared = self.world.zone_points.lock().unwrap();
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
    // `set_nav_state`/`stop_nav`/`apply_plan`/`apply_local_plan`/`is_player_dead`/`nav_halt_if_dead`/
    // `find_in_zone_portal`/`aggro_avoid` moved to `crate::nav::walker::Walker` (M1 extraction).
    // `is_player_dead` itself moved further, to `GameState::is_player_dead` — both `Walker` and
    // `drain_zone_cross` (below) need it, and it depends only on `GameState`.

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
        if self.walker.nav_halt_if_dead(gs) {
            return;
        }

        self.walker.apply_fast_steering(gs);

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

        // (The dead-player guard now runs earlier — right after stream_position, before the fast-
        // steering refresh and the 150 ms gate — so a corpse stops within a tick. See #238.)

        self.walker.drive_chase();

        self.walker.drive_teleport_detect(gs);

        let goal = match self.walker.resolve_goal() {
            Some(g) => g,
            None => return,
        };

        // `Walker::drive_walk` never touches position/`EqStream` itself (intent-only boundary — see
        // `crate::nav::walker`'s module doc): it only writes the per-frame `nav_intent`. A big drop
        // is no longer a special handoff — the walker just keeps walking toward the goal and the
        // render controller's ONE collided gravity path descends off the edge (§442, #442); the
        // landing damage is applied driver-agnostically in `stream_position`.
        self.walker.drive_walk(gs, goal);
    }

    // TODO(MVC): this and the other `drain_*` methods below are slot CONSUMERS — they poll a
    // request slot that both UI click-handlers (src/ui/) and the HTTP agent API (src/http/) write
    // into independently today. Program Phase 2 should unify those two producers behind one shared
    // controller-verb call so "click Loot" and "POST /v1/interact/loot" both go through the same
    // code path instead of two independent writers racing into the same `Arc<Mutex<Option<T>>>`.
    fn drain_loot(&mut self, gs: &mut GameState) {
        // POST /loot: queue the requested corpse onto the existing auto-loot pipeline. The gameplay
        // loop drains pending_loot — sends OP_LootRequest, echoes each OP_LootItem to take it, then
        // OP_EndLootRequest. The 500ms delay (loot_queued_at) lets the server register the corpse.
        if let Some(corpse_id) = self.command.take_loot() {
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
        if let Some(door_id) = self.command.take_door_click() {
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
        if let Some(task_id) = self.command.take_accept_task() {
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
        if let Some(task_id) = self.command.take_cancel_task() {
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

    // #446: the HUD group window and POST /v1/group/* both write through the shared
    // `CommandState::request_group_*` verbs now, and this drain reads them back via
    // `take_group_*` — one typed surface over each slot instead of two call sites poking the raw
    // `Arc<Mutex<..>>`.
    fn drain_group(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/group/invite {"name":"X"}: send OP_GroupInvite.
        if let Some(target) = self.command.take_group_invite() {
            stream.send_app_packet(OP_GROUP_INVITE, &build_group_invite(&target, &gs.player_name));
            tracing::info!("EQ: group: invited {target}");
            gs.log_msg("group", &format!("Invited {target} to group"));
        }

        // POST /v1/group/accept: send OP_GroupFollow. Optimistically clear pending_invite now —
        // the real roster confirmation arrives via OP_GroupUpdateB/OP_GroupAcknowledge.
        if self.command.take_group_accept().is_some() {
            if let Some(inviter) = gs.pending_invite.take() {
                stream.send_app_packet(OP_GROUP_FOLLOW, &build_group_follow(&inviter, &gs.player_name));
                tracing::info!("EQ: group: accepted invite from {inviter}");
                gs.log_msg("group", &format!("Accepted group invite from {inviter}"));
            }
        }

        // POST /v1/group/decline: RoF2 has no working OP_GroupCancelInvite, so send a defensive
        // OP_GroupDisband(self, self) cleanup instead.
        if self.command.take_group_decline().is_some() {
            if let Some(inviter) = gs.pending_invite.take() {
                stream.send_app_packet(OP_GROUP_DISBAND, &build_group_disband(&gs.player_name, &gs.player_name));
                tracing::info!("EQ: group: declined invite from {inviter}");
                gs.log_msg("group", &format!("Declined group invite from {inviter}"));
            }
        }

        // POST /v1/group/leave: send OP_GroupDisband(self, self). If leader with < 3 members this
        // fully disbands the group server-side (no auto handoff — see Global Constraints).
        if self.command.take_group_leave().is_some() {
            stream.send_app_packet(OP_GROUP_DISBAND, &build_group_disband(&gs.player_name, &gs.player_name));
            tracing::info!("EQ: group: left group");
            gs.log_msg("group", "Left group");
        }

        // POST /v1/group/kick {"name":"X"}: send OP_GroupDisband(self, target). HTTP layer already
        // validated leadership + membership before queuing this.
        if let Some(target) = self.command.take_group_kick() {
            stream.send_app_packet(OP_GROUP_DISBAND, &build_group_disband(&gs.player_name, &target));
            tracing::info!("EQ: group: kicked {target}");
            gs.log_msg("group", &format!("Kicked {target} from group"));
        }

        // POST /v1/group/makeleader {"name":"X"}: send OP_GroupMakeLeader.
        if let Some(target) = self.command.take_group_make_leader() {
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
        if let Some(npc_id) = self.command.take_trainer_open() {
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
        if let Some(skill_id) = self.command.take_train_skill() {
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
        let cross_req = self.nav.zone_cross.lock().unwrap().take();
        if let Some(want_zone) = cross_req {
            // want_zone != 0 → resolve it to a zone-point index; want_zone == 0 → any nearest line.
            let want_index = if want_zone != 0 {
                match self.world.zone_points.lock().unwrap().iter()
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
                        self.walker.set_nav_state_because("no_path", Some("no_zone_line_to_zone"));
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
                            let idxs: Vec<i32> = self.world.zone_points.lock().unwrap().iter()
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
                        let dest_zone = self.world.zone_points.lock().unwrap().iter()
                            .find(|zp| zp.iterator as i32 == index).map(|zp| zp.zone_id).unwrap_or(want_zone);
                        let d2 = (tx - gs.player_x).powi(2) + (ty - gs.player_y).powi(2);
                        const ZONE_LINE_DIST2: f32 = 15.0 * 15.0;
                        if d2 <= ZONE_LINE_DIST2 {
                            // Already standing on the line — the auto-cross below fires this tick.
                            tracing::info!("zone_cross: already on the zone_id={dest_zone} line (index={index})");
                        } else {
                            tracing::info!("zone_cross: walking {:.0}u to the zone_id={dest_zone} line at ({tx:.0},{ty:.0}) (index={index})", d2.sqrt());
                            gs.log_msg("zone", &format!("Walking to the zone {} line", dest_zone));
                            *self.nav.goto_target.lock().unwrap() = Some((tx, ty, tz));
                            *self.nav.goto_entity.lock().unwrap() = None;
                        }
                    }
                    None => {
                        tracing::info!("zone_cross: no zone-line region found for zone_id={want_zone}");
                        gs.log_msg("zone", "No zone line found to cross");
                        // Advertised in OP_SendZonepoints but no DRNTP region in the loaded map (a .wtr
                        // gap): report it so the caller isn't left thinking the 200 meant success (#267).
                        self.walker.set_nav_state_because("no_path", Some("zone_line_not_in_map"));
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
            if !gs.is_player_dead() && self.last_zone_cross.elapsed().as_millis() > ZONE_CROSS_COOLDOWN_MS {
                let index = self.collision.read().unwrap().as_ref()
                    .and_then(|c| c.zone_line_at([gs.player_x, gs.player_y, gs.player_z]));
                if let Some(index) = index {
                    // Resolve destination: the advertised zone point whose iterator matches this
                    // region's index. A region with no matching zone point (e.g. a WLD index the DB
                    // doesn't advertise) is left alone rather than crossing blindly.
                    let dest = self.world.zone_points.lock().unwrap().iter()
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
        let hail_req = self.command.take_hail();
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
        let say_text = self.command.take_say();
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
        let click = self.command.take_dialogue_click();
        if let Some(c) = click {
            let pkt = build_item_link_click(c.item_id, &c.augments, c.link_hash, c.icon);
            tracing::info!("EQ: dialogue click: '{}' (sayid={})", c.text, c.augments[0]);
            stream.send_app_packet(OP_ITEM_LINK_CLICK, &pkt);
            let line = format!("You say, '{}'", c.text);
            gs.log_msg("chat", &line);
        }

        // Drain queued outgoing chat (POST /tell|/ooc|/shout|/group): build + send OP_ChannelMessage.
        // #446: both the Chat window and the POST handlers write through the shared
        // `CommandState::request_chat_send` verb now; this drains the whole FIFO queue at once via
        // `take_chat_send` (same `std::mem::take` behavior the raw slot drain had).
        let outgoing: Vec<crate::ipc::ChatSend> = self.command.take_chat_send();
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
        let target_id = self.command.take_target();
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

    // #446: GET /v1/observe/who and the /v1/social/friends presence poll now register their
    // oneshot senders through the shared `CommandState::request_who`/`request_friends_who` verbs,
    // and this drain reads them back via `take_who_req`/`take_friends_req`.
    fn drain_who_friends(&mut self, stream: &mut EqStream) {
        // Check /who all request (#300) — send OP_WhoAllRequest (server-wide, type=3); the oneshot
        // sender is held in `pending_who` until OP_WhoAllResponse arrives (see `fulfill_who`). A newer
        // request supersedes an in-flight one (its sender drops → that GET times out).
        if let Some(tx) = self.command.take_who_req() {
            stream.send_app_packet(OP_WHO_ALL_REQUEST, &build_who_all_request(3));
            self.pending_who = Some(tx);
            self.expecting_friends = false; // the next OP_WhoAllResponse is a /who all, not a friends poll
            tracing::info!("EQ: sent OP_WhoAllRequest (/who all)");
        }

        // Check friends-presence request (#301) — send OP_FriendsWho with the client-local friends
        // string; the reply arrives as OP_WhoAllResponse (online subset), routed to `fulfill_friends`
        // by the `expecting_friends` flag. Mirrors the /who all path above.
        if let Some(tx) = self.command.take_friends_req() {
            let names = self.social.friends_list.lock().unwrap().clone();
            stream.send_app_packet(OP_FRIENDS_WHO, &build_friends_who(&names));
            self.pending_friends = Some(tx);
            self.expecting_friends = true;
            tracing::info!("EQ: sent OP_FriendsWho ({} friend(s))", names.len());
        }
    }

    // #446: the HUD attack button and POST /v1/combat/attack now both write through the shared
    // `CommandState::request_attack` verb, and this drain reads it back via `take_attack` — one
    // typed surface over the slot instead of two call sites poking the raw `Arc<Mutex<..>>`.
    fn drain_combat(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Check attack request — send OP_AUTO_ATTACK(1) to start, OP_AUTO_ATTACK(0) to stop.
        // Server expects exactly 4 bytes; byte[0]=1 enables, byte[0]=0 disables.
        let attack_req = self.command.take_attack();
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
        let pet_cmd = self.command.take_pet_command();
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
        let read_slot = self.command.take_read_book();
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

    // #446: POST /v1/guild/* now writes through the shared `CommandState::request_guild_action`
    // verb (which also preserves the original "one pending action at a time" CONFLICT check), and
    // this drain reads it back via `take_guild_action`.
    fn drain_guild(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // POST /v1/guild/{invite,accept,leave,remove}: one queued guild action → the matching RoF2
        // guild opcode. Invite/remove/leave share GuildCommand_Struct; accept replies to a captured
        // pending invite with GuildInviteAccept_Struct. (#295)
        let guild_action = self.command.take_guild_action();
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
        let cast_req = self.command.take_cast();
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
        let mem_req = self.command.take_mem_spell();
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
        let sit_req = self.command.take_sit();
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
        let con_req = self.command.take_consider();
        if let Some(id) = con_req {
            stream.send_app_packet(OP_CONSIDER, &build_consider_packet(gs.player_id, id));
            tracing::info!("EQ: consider spawn_id={}", id);
        }
    }

    fn drain_merchant(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Merchant buy: open the merchant (OP_ShopRequest) then buy its inventory slot
        // (OP_ShopPlayerBuy). Sent in sequence — the server processes the open first so the
        // merchant is open by the time the buy arrives. Must be within ~200u of the merchant.
        let buy_req = self.command.take_merchant_buy();
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
        let sell_req = self.command.take_merchant_sell();
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
        let trade_req = self.command.take_merchant_trade();
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
        let move_req = self.command.take_inventory_move();
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

    // `apply_fast_steering` moved to `crate::nav::walker::Walker` (M1 extraction).

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
                            *self.controller.nav_intent.lock().unwrap() = Some(MoveIntent {
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
                            *self.controller.nav_intent.lock().unwrap() = None;
                            self.send_position_update(stream, gs, gs.player_x, gs.player_y, gs.player_z, hdg);
                        }
                        *self.nav.goto_target.lock().unwrap() = None; // cancel any stale walk
                        return true;
                    }
                }
            }
        }
        false
    }

    // `drive_chase`/`drive_teleport_detect`/`resolve_goal`/`drive_walk` moved to
    // `crate::nav::walker::Walker` (M1 extraction) — see `tick`'s `self.walker.*` calls above.

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
            if let Some((npc_id, from_slot)) = self.command.take_give() {
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
        let view = *self.controller.controller_view.lock().unwrap();
        // Don't stream/mirror until the render controller has spawned (else we'd push origin).
        if !view.initialized { return; }
        // Anti-MQGhost keepalive (#105): send a movement-history entry every 30s (< the server's 70s
        // window) whether or not we're moving, so the server's CheatManager never false-flags us.
        if self.last_movement_history_send.elapsed().as_millis() >= MOVEMENT_HISTORY_MS {
            stream.send_app_packet(OP_FLOAT_LIST_THING,
                &build_movement_history(view.pos[0], view.pos[1], view.pos[2]));
            self.last_movement_history_send = Instant::now();
        }
        // Driver-agnostic fall damage (§442, #442). The render controller runs the ONE collided
        // descent (for WASD AND nav) and latches the height of any airborne stretch it just LANDED
        // from — computed from its OWN tracked airborne start, never a nav waypoint z. We take-and-
        // clear that one-shot exactly once here and, if the fall was past the safe height, apply the
        // native (client-computed) fall damage + OP_ENV_DAMAGE — the same formula/threshold the old
        // `drive_controlled_fall` used. Any fall past the safe height damages, so WASD off a ledge
        // now damages too, matching the native RoF2 client. A teleport / server correction clears the
        // signal at the controller (see `CharacterController::teleport`), so a correction is never
        // misread as a fall (hazard 2b); a mid-fall depenetration/ground-snap recovery latches nothing
        // (hazard 2a). `SAFE_FALL_HEIGHT` is named so the threshold is easy to tune/revert.
        const SAFE_FALL_HEIGHT: f32 = 6.0; // below the fall_damage() zero-damage cutoff (~6.7u); the
                                           // formula's `dmg > 0` stays the final arbiter.
        if let Some(height) = self.controller.controller_view.lock().unwrap().landed_fall_height.take() {
            if height > SAFE_FALL_HEIGHT {
                let (dmg, _max) = fall_damage(height);
                if dmg > 0 {
                    stream.send_app_packet(OP_ENV_DAMAGE, &build_env_damage_packet(gs.player_id, dmg, DMGTYPE_FALLING));
                    gs.cur_hp = (gs.cur_hp - dmg as i32).max(0);
                    gs.log_msg("combat", &format!("Fell {:.0}u — {} fall damage", height, dmg));
                    tracing::info!("EQ: fall damage {dmg} (fell {height:.0}u)");
                }
            }
        }
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
            *self.controller.pos_correction.lock().unwrap() = Some(gp);
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
    use crate::nav::steering::*;
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
    use crate::nav::steering::{NAV_LOCAL_STUCK_TICKS, PROACTIVE_REPLAN_CAP};

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
        nav.walker.apply_plan(PlanReply {
            gen: 1,
            outcome: crate::nav::collision::PlanOutcome::Route(vec![[0.0, 0.0, 47.0], [100.0, 100.0, 47.0]]),
            plan_ms: 5,
            goal_snapped_z: Some(47.0),
            tight: false,
        }, &mut gs, goal);

        let st = nav.nav.nav_state.lock().unwrap().clone();
        assert_eq!(st.state, "navigating");
        assert_eq!(st.reason.as_deref(), Some("goal_z_snapped"),
            "the agent asked for z=0 and is being walked to z=47 — it must be TOLD its goal was changed");
        assert!(gs.messages.iter().any(|m| m.text.contains("CHANGED your goal")),
            "and it must be said in the message log too, in words");

        // ...and it must survive to ARRIVAL. `arrived` with no reason would tell the agent it got
        // exactly what it asked for, which is the whole lie.
        assert!(nav.walker.goal_snapped, "the snap must be carried to arrival, not forgotten en route");

        // A goal whose z WAS honoured reports nothing — the accommodation must not be cried wolf.
        nav.walker.apply_plan(PlanReply {
            gen: 2,
            outcome: crate::nav::collision::PlanOutcome::Route(vec![[0.0, 0.0, 0.0], [100.0, 100.0, 0.0]]),
            plan_ms: 5,
            goal_snapped_z: None,
            tight: false,
        }, &mut gs, goal);
        let st = nav.nav.nav_state.lock().unwrap().clone();
        assert_eq!(st.reason, None, "a goal that was honoured as given carries no snap reason");
        assert!(!nav.walker.goal_snapped);
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
        nav.walker.apply_plan(PlanReply {
            gen: 1,
            outcome: PlanOutcome::Route(vec![[0.0, 0.0, 0.0], [100.0, 100.0, 0.0]]),
            plan_ms: 5, goal_snapped_z: None, tight: false,
        }, &mut gs, goal);
        assert_eq!(nav.nav.nav_state.lock().unwrap().tier, Some("preferred"),
            "a committed preferred route publishes nav_tier = preferred");

        // Journey B: a definitively unreachable goal → no_path. The tier from A must be GONE.
        nav.walker.apply_plan(PlanReply {
            gen: 2,
            outcome: PlanOutcome::Unreachable {
                reason: NoRoute::SearchClosed, goal_blocked_by: None, frontier_blocked_by: None },
            plan_ms: 5, goal_snapped_z: None, tight: false,
        }, &mut gs, goal);
        let st = nav.nav.nav_state.lock().unwrap().clone();
        assert_eq!(st.state, "no_path");
        assert_eq!(st.tier, None,
            "nav_tier must NOT survive from journey A into journey B's no_path (the #343 stale-field lie)");

        // A fresh minimum-tier route, then an Exhausted partial: the partial is not a confirmed route,
        // so it must carry no tier either.
        nav.walker.apply_plan(PlanReply {
            gen: 3,
            outcome: PlanOutcome::Route(vec![[0.0, 0.0, 0.0], [50.0, 50.0, 0.0]]),
            plan_ms: 5, goal_snapped_z: None, tight: true,
        }, &mut gs, goal);
        assert_eq!(nav.nav.nav_state.lock().unwrap().tier, Some("minimum"));
        nav.walker.apply_plan(PlanReply {
            gen: 4,
            outcome: PlanOutcome::Exhausted {
                limit: PlanLimit::NodeCap,
                progress: Some(vec![[0.0, 0.0, 0.0], [60.0, 60.0, 0.0], [90.0, 90.0, 0.0]]) },
            plan_ms: 5, goal_snapped_z: None, tight: false,
        }, &mut gs, goal);
        let st = nav.nav.nav_state.lock().unwrap().clone();
        assert_eq!(st.state, "navigating_partial");
        assert_eq!(st.tier, None, "an Exhausted partial walk is not a confirmed route — it carries no tier");

        // And an arrived state (reached via set_nav_state) after a committed route carries no stale tier.
        nav.walker.apply_plan(PlanReply {
            gen: 5,
            outcome: PlanOutcome::Route(vec![[0.0, 0.0, 0.0], [100.0, 100.0, 0.0]]),
            plan_ms: 5, goal_snapped_z: None, tight: false,
        }, &mut gs, goal);
        assert_eq!(nav.nav.nav_state.lock().unwrap().tier, Some("preferred"));
        nav.walker.set_nav_state("arrived");
        assert_eq!(nav.nav.nav_state.lock().unwrap().tier, None,
            "arrival ends the route — its tier must not linger");
    }

    /// Build a minimal ActionLoop for unit tests that only exercise a single `sync_*`/tick method —
    /// every other shared slot gets an empty/default placeholder.
    fn test_action_loop(group: crate::ipc::GroupShared) -> ActionLoop {
        ActionLoop::new(
            crate::ipc::NavSlots {
                nav_state: std::sync::Arc::new(std::sync::Mutex::new(crate::ipc::NavStatus::default())),
                ..Default::default()
            },
            Default::default(), // world
            Default::default(), // quest
            crate::ipc::GroupSlots { group, ..Default::default() },
            Default::default(), // trainer
            Default::default(), // command (CommandState)
            Default::default(), // social
            Default::default(), // merchant_slots
            Default::default(), // inventory_slots
            Default::default(), // interact
            Default::default(), // chat
            Default::default(), // controller
            Default::default(), // guild_slots
            Default::default(), // collision
            std::path::PathBuf::new(), // maps_dir
            Default::default(), // camp
        )
    }

    #[test]
    fn dead_player_halts_navigation() {
        // #238: a character that dies mid-goto must stop — the corpse must not keep walking the route.
        // Seed an in-progress nav, then assert nav_halt_if_dead() clears everything and reports dead.
        let seed_nav = |nav: &mut ActionLoop| {
            *nav.nav.goto_target.lock().unwrap() = Some((100.0, 200.0, 0.0));
            *nav.nav.goto_entity.lock().unwrap() = Some("a bat".into());
            *nav.controller.nav_intent.lock().unwrap() = Some(crate::movement::MoveIntent::default());
            *nav.nav.nav_path_view.lock().unwrap() = (vec![[0.0, 0.0, 0.0]], vec![[0.0, 0.0, 0.0]]);
            nav.walker.path = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0]];
            nav.walker.local_path = vec![[0.0, 0.0, 0.0]];
            nav.walker.path_goal = Some((100.0, 200.0, 0.0));
            nav.walker.path_i = 1;
            nav.walker.local_i = 1;
            *nav.nav.nav_state.lock().unwrap() = "navigating".into();
        };
        let assert_halted = |nav: &ActionLoop| {
            assert!(nav.nav.goto_target.lock().unwrap().is_none(), "goto_target must clear on death");
            assert!(nav.nav.goto_entity.lock().unwrap().is_none(), "goto_entity must clear on death");
            assert!(nav.controller.nav_intent.lock().unwrap().is_none(), "nav_intent must clear so the controller stops");
            assert!(nav.walker.path.is_empty() && nav.walker.local_path.is_empty(), "route must clear on death");
            // The fast-steering cursor must reset with the path it indexes (#311) — a stale local_i
            // left over a cleared/rebuilt local_path aims the walker at the wrong segment.
            assert_eq!(nav.walker.local_i, 0, "local_i must reset with local_path on death");
            assert_eq!(nav.walker.path_goal, None);
            assert_eq!(*nav.nav.nav_state.lock().unwrap(), "idle");
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
        assert!(nav.walker.nav_halt_if_dead(&gs), "cur_hp<=0 (pre-OP_Death) must halt navigation");
        assert_halted(&nav);

        // (b) The OP_Death flag path (player_dead set, cur_hp already zeroed by apply_death).
        let mut nav = new_nav();
        seed_nav(&mut nav);
        let mut gs = GameState::new();
        gs.player_dead = true;
        gs.cur_hp = 0;
        gs.max_hp = 1284;
        assert!(nav.walker.nav_halt_if_dead(&gs));
        assert_halted(&nav);

        // (c) A LIVE player must NOT be halted (and cur_hp<=0 with max_hp==0 = "unknown", not dead —
        //     e.g. a fresh spawn before the first HP update — must not spuriously stop nav).
        let mut nav = new_nav();
        seed_nav(&mut nav);
        let mut gs = GameState::new();
        gs.player_dead = false;
        gs.cur_hp = 900;
        gs.max_hp = 1284;
        assert!(!nav.walker.nav_halt_if_dead(&gs), "a live player must keep navigating");
        assert!(nav.nav.goto_target.lock().unwrap().is_some(), "live nav must be untouched");
        gs.cur_hp = 0;
        gs.max_hp = 0; // unknown HP, not a death
        assert!(!nav.walker.nav_halt_if_dead(&gs), "cur_hp<=0 with max_hp==0 is unknown HP, not death");
        assert!(nav.nav.goto_target.lock().unwrap().is_some());
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
        *nav.nav.goto_target.lock().unwrap() = Some((100.0, 200.0, 0.0));
        *nav.nav.goto_entity.lock().unwrap() = Some("a bat".into());
        *nav.controller.nav_intent.lock().unwrap() = Some(crate::movement::MoveIntent::default());
        *nav.nav.nav_path_view.lock().unwrap() = (vec![[0.0, 0.0, 0.0]], vec![[0.0, 0.0, 0.0]]);
        nav.walker.path = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0]];
        nav.walker.local_path = vec![[0.0, 0.0, 0.0]];
        nav.walker.path_goal = Some((100.0, 200.0, 0.0));
        nav.walker.path_i = 1;
        nav.walker.local_i = 1;
        nav.walker.stuck_ticks = 5;
        nav.walker.nav_repaths = 3;
        nav.walker.backoff_ticks = 2;
        nav.walker.replan_coarse = true;
        *nav.nav.nav_state.lock().unwrap() = "blocked".into();

        // Cross into a NEW zone.
        let mut gs = GameState::new();
        gs.zone_name = "crushbone".into();
        nav.sync_zone_points(&gs);

        // Destination + route + recovery state all cleared; walker comes to rest in the new zone.
        assert!(nav.nav.goto_target.lock().unwrap().is_none(), "goto_target must clear on zone change");
        assert!(nav.nav.goto_entity.lock().unwrap().is_none(), "goto_entity must clear on zone change");
        assert!(nav.controller.nav_intent.lock().unwrap().is_none(), "nav_intent must clear so the controller stops");
        let (coarse, fine) = &*nav.nav.nav_path_view.lock().unwrap();
        assert!(coarse.is_empty() && fine.is_empty(), "overlay path must clear on zone change");
        assert!(nav.walker.path.is_empty() && nav.walker.local_path.is_empty(), "route must clear on zone change");
        assert_eq!(nav.walker.path_goal, None);
        assert_eq!(nav.walker.path_i, 0);
        // The fast-steering cursor must reset with the path it indexes (#311) — a stale local_i in
        // the NEW zone points at a segment of a route that no longer exists.
        assert_eq!(nav.walker.local_i, 0, "local_i must reset with local_path on zone change");
        assert_eq!(nav.walker.stuck_ticks, 0);
        assert_eq!(nav.walker.nav_repaths, 0);
        assert_eq!(nav.walker.proactive_replans, 0, "the oscillation budget must reset on zone change");
        assert_eq!(nav.walker.backoff_ticks, 0);
        assert!(!nav.walker.replan_coarse);
        assert_eq!(*nav.nav.nav_state.lock().unwrap(), "idle");
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
        nav.walker.backoff_ticks = 0;
        nav.walker.stuck_ticks = 0;
        nav.walker.replan_cooldown = 0;
        for _ in 0..NAV_LOCAL_STUCK_TICKS {
            nav.walker.apply_local_plan(nwt([0.0, 0.0, 0.0]));
        }
        assert!(nav.walker.replan_coarse, "NoWayThrough × NAV_LOCAL_STUCK_TICKS must arm the proactive re-plan");
        assert_eq!(nav.walker.proactive_replans, 1, "arming the proactive re-plan bumps the oscillation budget");

        // A Threaded plan ends the local-stuck run but must not forgive the budget (only tick's
        // progress reset does): the fine tier finding one way through does not prove the wedge gone.
        nav.walker.apply_local_plan(crate::nav::planner::LocalReply {
            gen: 2, start: [0.0, 0.0, 0.0], goal: [40.0, 0.0, 0.0],
            outcome: LocalOutcome::Threaded(vec![[0.0, 0.0, 0.0], [40.0, 0.0, 0.0]]), plan_us: 100,
        });
        assert_eq!(nav.walker.local_stuck_ticks, 0, "a threaded fine plan resets the local-stuck run");
        assert_eq!(nav.walker.proactive_replans, 1, "a threaded fine plan must not forgive the oscillation budget");

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
