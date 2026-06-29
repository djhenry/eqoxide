//! Player navigation: walk toward a target position in capped steps at 150 ms intervals,
//! sending EQ movement packets and notifying the render loop.

use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;

/// Nav tick interval (ms). Steps are gated to fire no more often than this.
const NAV_TICK_MS: u128 = 150;
/// Native Titanium base run speed in EQ units/second (runspeed 0.7 → 44 u/s; 10 Hz updates of
/// 4.4 u each). Per eq-client-expert, see docs/eq-technical-knowledgebase/player-movement-speed.md.
/// We must NOT move faster than this: even where THIS server tolerates it, others rubber-band or
/// reject motion the real client can't produce.
const RUN_SPEED: f32 = 44.0;
/// Max distance to move per nav tick. `RUN_SPEED * tick_seconds`; the >=150 ms gate guarantees the
/// realized speed never exceeds RUN_SPEED.
const NAV_STEP: f32 = RUN_SPEED * (NAV_TICK_MS as f32 / 1000.0); // 44 * 0.150 = 6.6 units

use crate::eq_net::protocol::*;
use crate::eq_net::transport::{AppPacket, EqStream};
use crate::game_state::{GameState, ZonePoint};
use crate::http::{AttackReq, BuyReq, SellReq, TradeReq, TradeCmd, MerchantShared, DoorClickReq, DoorsShared, MoveReq, GiveReq, InventoryShared, LootReq, MessagesShared, ChatEventsShared, ChatSendShared, CastReq, MemSpellReq, SitReq, ConsiderReq, CampReq, CampCmd, EntityIds, EntityPositions, GotoTarget, HailReq, SayReq, TargetReq, TaskLog, WarpReq, ZoneCrossReq, ZonePoints};

/// Pending state of a quest turn-in (POST /give). The trade window spans multiple nav ticks:
/// we send OP_TradeRequest, then must wait for the server's OP_TradeRequestAck before moving the
/// item into the NPC trade slot. `ticks_waiting` counts nav ticks (~150ms each) for the timeout.
struct GiveState {
    npc_id:        u32,
    ticks_waiting: u32,
}

/// ~3 second ack timeout, in nav ticks (tick gating is ~150ms → 20 ticks ≈ 3s).
const GIVE_ACK_TIMEOUT_TICKS: u32 = 20;

/// OP_TargetCommand payload: ClientTarget_Struct = just the target spawn id (u32).
pub fn build_target_packet(spawn_id: u32) -> Vec<u8> {
    spawn_id.to_le_bytes().to_vec()
}

/// Auto-combat target priority. Prefers the mob currently attacking the player (an add that aggros
/// mid-fight) so the player fights back instead of being beaten unanswered — but keeps the current
/// target when it is itself one of the attackers, so two adds don't cause target thrash. Falls back
/// to a still-valid current target, then the nearest reachable trash mob.
///
/// - `current_valid`: the current target is alive and reachable.
/// - `current_is_attacker`: the current target has swung at the player recently.
/// - `attacker`: a recent attacker that is alive + reachable (the add to engage), if any.
pub fn pick_combat_target(
    current: Option<u32>,
    current_valid: bool,
    current_is_attacker: bool,
    attacker: Option<u32>,
    nearest_trash: Option<u32>,
) -> Option<u32> {
    // Already fighting one of our attackers — stay on it (don't thrash to a second add).
    if current_valid && current_is_attacker {
        return current;
    }
    // An add is hitting us and isn't our current target — engage it.
    if let Some(a) = attacker {
        return Some(a);
    }
    // Nobody attacking us; finish the current target if it's still good, else pick fresh trash.
    if current_valid {
        return current;
    }
    nearest_trash
}

/// OP_Consider payload: Consider_Struct (28 bytes). The client fills playerid+targetid;
/// the server replies with the same opcode carrying faction (con standing) + level
/// (con color). Size must be exactly 28 or EQEmu rejects it.
pub fn build_consider_packet(player_id: u32, target_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 28];
    buf[0..4].copy_from_slice(&player_id.to_le_bytes());
    buf[4..8].copy_from_slice(&target_id.to_le_bytes());
    buf
}

/// Titanium `CastSpell_Struct` (20 bytes): slot, spell_id, inventoryslot, target_id, unk[4].
/// `slot` is the gem index 0-8 for a memorized-gem cast; inventoryslot 0xFFFF = gem cast.
pub fn build_cast_packet(slot: u32, spell_id: u32, target_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 20];
    buf[0..4].copy_from_slice(&slot.to_le_bytes());
    buf[4..8].copy_from_slice(&spell_id.to_le_bytes());
    buf[8..12].copy_from_slice(&0xFFFFu32.to_le_bytes());
    buf[12..16].copy_from_slice(&target_id.to_le_bytes());
    buf
}

/// `MemorizeSpell_Struct` (16 bytes): slot, spell_id, scribing, reduction. Identical layout under
/// Titanium and RoF2 (verified against EQEmu rof2_structs.h — no ENCODE), opcode 0x217c.
/// scribing: 0 = scribe a scroll into the spellbook at `slot`; 1 = memorize a known spell into
/// gem `slot` (0-8); 2 = un-memorize. NOTE: scribing (0) only works if the scroll is on the CURSOR
/// (the server reads `m_inv[slotCursor]`); the caller must move it there first. See eqoxide#11.
pub fn build_memorize_packet(slot: u32, spell_id: u32, scribing: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 16];
    buf[0..4].copy_from_slice(&slot.to_le_bytes());
    buf[4..8].copy_from_slice(&spell_id.to_le_bytes());
    buf[8..12].copy_from_slice(&scribing.to_le_bytes());
    buf
}

/// `MoveItem_Struct` (12 bytes): from_slot, to_slot, number_in_stack. number_in_stack = 0 for a
/// whole-item move (equip/cursor/rearrange); a count would split a stack. See the inline note at
/// the /inventory/move handler for why 0 (not 1) is required for non-stackables.
pub fn build_move_item(from_slot: u32, to_slot: u32) -> [u8; 12] {
    let mut buf = [0u8; 12];
    buf[0..4].copy_from_slice(&from_slot.to_le_bytes());
    buf[4..8].copy_from_slice(&to_slot.to_le_bytes());
    buf[8..12].copy_from_slice(&0u32.to_le_bytes());
    buf
}

/// Native Titanium fall damage for a fall of `height` EQ units. Fall damage is CLIENT-computed in
/// EQ (the server only validates OP_EnvDamage). Model: impact velocity = min(terminal,
/// sqrt(2·g·h)) converted to the client's internal per-update z-velocity units (~5-13); then
/// `fall_score = |z_vel| − 4` (char_counter≈0, no safe-fall skill): ≤0 → no damage, ≥9 → lethal
/// (20000), else a roll in `[0, score²·10]`. Returns (rolled_damage, max_damage). See
/// docs/eq-technical-knowledgebase/falling-physics.md.
pub fn fall_damage(height: f32) -> (u32, u32) {
    const GRAVITY: f32 = 120.0;   // matches the renderer's fall physics
    const TERMINAL: f32 = 128.0;  // native internal z-velocity clamp
    const HZ: f32 = 10.0;         // native position-update rate the formula is calibrated to
    let v = (2.0 * GRAVITY * height.max(0.0)).sqrt().min(TERMINAL);
    let score = v / HZ - 4.0;
    if score <= 0.0 { return (0, 0); }
    if score >= 9.0 { return (20_000, 20_000); }
    let max = (score * score * 10.0) as u32;
    let roll = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos()).unwrap_or(0);
    (if max == 0 { 0 } else { roll % (max + 1) }, max)
}

/// Titanium `EnvDamage2_Struct` (31 bytes): id@0, damage(u32)@6, dmgtype(u8)@22, constant(u16)@27.
pub fn build_env_damage_packet(player_id: u32, damage: u32, dmgtype: u8) -> Vec<u8> {
    let mut buf = vec![0u8; 31];
    buf[0..4].copy_from_slice(&player_id.to_le_bytes());
    buf[6..10].copy_from_slice(&damage.to_le_bytes());
    buf[22] = dmgtype;
    buf[27..29].copy_from_slice(&0xFFFFu16.to_le_bytes());
    buf
}

/// Titanium `PetCommand_Struct` (8 bytes): command(u32), target(u32). e.g. PET_ATTACK + a mob
/// spawn id sends the player's pet to attack it.
pub fn build_pet_command(command: u32, target: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 8];
    buf[0..4].copy_from_slice(&command.to_le_bytes());
    buf[4..8].copy_from_slice(&target.to_le_bytes());
    buf
}

/// RoF2 `MerchantClick_Struct` (24 bytes): npc_id@0, player_id@4, command@8 (1=open, 0=close),
/// rate@12, **tab_display@16** (bitmask — b001 = Purchase/Sell tab), unknown02@20 (-1 from client).
/// Titanium was 16 bytes with no tab_display; without tab_display set the RoF2 server opens the
/// window but sends NO merchant inventory, so it must be 1.
fn merchant_click(npc_id: u32, player_id: u32, command: u32) -> [u8; 24] {
    let mut b = [0u8; 24];
    b[0..4].copy_from_slice(&npc_id.to_le_bytes());
    b[4..8].copy_from_slice(&player_id.to_le_bytes());
    b[8..12].copy_from_slice(&command.to_le_bytes());
    b[16..20].copy_from_slice(&1i32.to_le_bytes());    // tab_display = Purchase/Sell
    b[20..24].copy_from_slice(&(-1i32).to_le_bytes());  // unknown02 = -1 (client value)
    b
}

/// Titanium `SpawnAppearance_Struct` (8 bytes): spawn_id(u16), type(u16), parameter(u32).
/// For sit/stand: kind=14 (Animation), parameter=110 (sit) / 100 (stand).
pub fn build_spawn_appearance_packet(spawn_id: u16, kind: u16, parameter: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 8];
    buf[0..2].copy_from_slice(&spawn_id.to_le_bytes());
    buf[2..4].copy_from_slice(&kind.to_le_bytes());
    buf[4..8].copy_from_slice(&parameter.to_le_bytes());
    buf
}

/// OP_ClickDoor payload: ClickDoor_Struct (16 bytes). The lite client is an observer —
/// picklockskill and item_id are 0; the server only uses doorid for lookup and reads
/// skills/inventory from the Client object. player_id is our own spawn id (u16).
pub fn build_click_door(door_id: u8, player_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 16];
    buf[0] = door_id;                                       // doorid @0x00
    // [1..4] action/unknown = 0
    buf[4] = 0;                                             // picklockskill @0x04
    // [8..12] item_id = 0
    buf[12..14].copy_from_slice(&(player_id as u16).to_le_bytes()); // player_id @0x0c
    buf
}

/// Build a RoF2 `OP_ChannelMessage` for the Say channel (used for NPC hails).
/// chan_num 8 = ChatChannel_Say; the server delivers say text to NPCs within 200
/// units, triggering EVENT_SAY (a "Hail, <name>" message fires the NPC's hail script).
pub fn build_say_packet(sender: &str, target: &str, message: &str) -> Vec<u8> {
    build_channel_message(sender, target, 8, message) // chan_num 8 = ChatChannel_Say
}

/// Build an `OP_ChannelMessage` for an arbitrary chat channel. `target` is the recipient
/// for directed channels (tell), empty for broadcasts (ooc/shout/group). EQEmu ChatChannel:
/// 2 group, 3 shout, 5 OOC, 7 tell, 8 say.
///
/// RoF2 uses a **variable-length, NUL-terminated** wire format — NOT the fixed Titanium
/// `ChannelMessage_Struct`. See EQEmu `common/patches/rof2.cpp` `DECODE(OP_ChannelMessage)`:
///   sender\0 | target\0 | u32 unknown | u32 language | u32 chan_num
///   | u32 unknown | u8 unknown | u32 skill_in_language | message\0
/// Sending the fixed 64-byte-field struct makes the server read an empty target + garbage
/// chan_num, so tells/OOC are silently dropped (no cross-zone routing).
pub fn build_channel_message(sender: &str, target: &str, chan_num: u32, message: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(sender.len() + target.len() + message.len() + 24);
    buf.extend_from_slice(sender.as_bytes()); buf.push(0);
    buf.extend_from_slice(target.as_bytes()); buf.push(0);
    buf.extend_from_slice(&0u32.to_le_bytes());      // unknown
    buf.extend_from_slice(&0u32.to_le_bytes());      // language = CommonTongue
    buf.extend_from_slice(&chan_num.to_le_bytes());  // chan_num
    buf.extend_from_slice(&0u32.to_le_bytes());      // unknown
    buf.push(0);                                     // unknown (u8)
    buf.extend_from_slice(&100u32.to_le_bytes());    // skill_in_language
    buf.extend_from_slice(message.as_bytes()); buf.push(0);
    buf
}

/// Choose a movement delta `(dx, dy)` from the desired `(full_dx, full_dy)` step,
/// sliding along a single axis when the diagonal is blocked by a wall. `dx`/`dy` are
/// in EQ server axes: dx = east (server_x), dy = north (server_y). Returns `None`
/// only when fully boxed in. Cast at chest height (z+3) so low lips/stairs don't block.
/// Collision world points are `[east, north, height]` = `[server_x, server_y, server_z]`.
pub fn slide_move(
    col: &crate::assets::Collision,
    px: f32, py: f32, z: f32,
    full_dx: f32, full_dy: f32, radius: f32,
) -> Option<(f32, f32)> {
    let chest = z + 3.0;
    let clear = |sx: f32, sy: f32| col.path_clear([px, py, chest], [px + sx, py + sy, chest], radius);
    if clear(full_dx, full_dy) {
        Some((full_dx, full_dy))
    } else if clear(full_dx, 0.0) {
        Some((full_dx, 0.0))
    } else if clear(0.0, full_dy) {
        Some((0.0, full_dy))
    } else {
        None
    }
}

/// Minimum distance (units) the walker must close toward its current aim for a tick to
/// count as progress. Below this, the tick is "no progress" (sliding sideways, wedged).
const NAV_PROGRESS_EPS: f32 = 1.0;
/// Consecutive no-progress nav ticks (~150 ms each) before the walker is declared stuck.
/// ~3 s — long enough to ride out a brief wall-slide, short enough to recover quickly.
const NAV_STUCK_TICKS: u32 = 20;

/// What the no-progress detector decided after a nav step.
#[derive(Debug, PartialEq, Eq)]
enum StuckAction {
    /// Still making (or recently made) progress — keep walking the current waypoint.
    Continue,
    /// No progress for NAV_STUCK_TICKS — the caller should recover (skip waypoint or stop).
    Recover,
}

/// No-progress detector for the path walker. `dist` is the current straight-line distance
/// to the aim point; `best` is the smallest distance seen since the detector last reset;
/// `stuck_ticks` is the running count of consecutive no-progress ticks. Closing more than
/// `NAV_PROGRESS_EPS` resets the counter and records the new best; otherwise the counter
/// accumulates until it reaches `NAV_STUCK_TICKS`, which yields `Recover` (and resets, so
/// recovery starts with a fresh window). Returns the action plus the `(best, stuck_ticks)`
/// the caller should store.
fn nav_progress(dist: f32, best: f32, stuck_ticks: u32) -> (StuckAction, f32, u32) {
    if dist + NAV_PROGRESS_EPS < best {
        (StuckAction::Continue, dist, 0)
    } else {
        let n = stuck_ticks + 1;
        if n >= NAV_STUCK_TICKS {
            (StuckAction::Recover, f32::MAX, 0)
        } else {
            (StuckAction::Continue, best, n)
        }
    }
}

/// EQ heading in degrees (0..360) for a movement delta in server axes.
/// EQ convention: heading 0 faces +Y (north) and increases counter-clockwise
/// (90 = -X = west, 180 = -Y = south, 270 = +X = east). A point at heading θ lies
/// at (east, north) = (-sinθ, cosθ), so θ = atan2(-east, north).
pub fn eq_heading(d_east: f32, d_north: f32) -> f32 {
    (-d_east).atan2(d_north).to_degrees().rem_euclid(360.0)
}

/// Squared 2D distance from a zone point to the player's current position.
fn dist2(zp: &crate::game_state::ZonePoint, gs: &GameState) -> f32 {
    let dx = zp.server_x - gs.player_x;
    let dy = zp.server_y - gs.player_y;
    dx * dx + dy * dy
}

pub struct Navigator {
    goto_target:      GotoTarget,
    entity_positions: EntityPositions,
    entity_ids:       EntityIds,
    zone_points:      ZonePoints,
    task_log:         TaskLog,
    zone_cross:       ZoneCrossReq,
    /// Direct teleport request (POST /warp). The nav thread jumps the player to these coords,
    /// sends a position update so the server agrees, and cancels any in-progress /goto.
    warp:             WarpReq,
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
    cast:             CastReq,
    mem_spell:        MemSpellReq,
    sit:              SitReq,
    consider:         ConsiderReq,
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
    chat_events:      ChatEventsShared,
    chat_send:        ChatSendShared,
    collision:        crate::assets::SharedCollision,
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
}

impl Navigator {
    pub fn new(
        goto_target:      GotoTarget,
        entity_positions: EntityPositions,
        entity_ids:       EntityIds,
        zone_points:      ZonePoints,
        task_log:         TaskLog,
        zone_cross:       ZoneCrossReq,
        warp:             WarpReq,
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
        chat_events:      ChatEventsShared,
        chat_send:        ChatSendShared,
        cast:             CastReq,
        mem_spell:        MemSpellReq,
        sit:              SitReq,
        consider:         ConsiderReq,
        collision:        crate::assets::SharedCollision,
        maps_dir:         std::path::PathBuf,
        camp:             CampReq,
    ) -> Self {
        Navigator {
            goto_target,
            entity_positions,
            entity_ids,
            zone_points,
            task_log,
            zone_cross,
            warp,
            hail,
            say,
            target,
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
            camp,
            give_state: None,
            inventory,
            loot,
            door_click,
            doors_shared,
            messages,
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
            last_pet_target: None,
            falling: None,
            fall_start_z: 0.0,
            stuck_best: f32::MAX,
            stuck_ticks: 0,
            stuck_i: 0,
        }
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
    }

    /// Publish the player's inventory + equipment from `gs` into the shared slot (GET /inventory).
    pub fn sync_inventory(&self, gs: &GameState) {
        let mut inv = self.inventory.lock().unwrap();
        inv.clear();
        inv.extend(gs.inventory.iter().cloned());
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
            let keywords = crate::hud::split_keywords(&m.text).into_iter()
                .filter(|(_, is_kw)| *is_kw)
                .map(|(seg, _)| seg.trim_matches(|c| c == '[' || c == ']').trim().to_string())
                .filter(|k| !k.is_empty())
                .collect();
            crate::http::MessageEntry { kind: m.kind.clone(), text: m.text.clone(), keywords }
        }));
        drop(out);
        // Publish async events (GET /v1/events/*), preserving their stable monotonic ids.
        let mut ev = self.chat_events.lock().unwrap();
        ev.clear();
        ev.extend(gs.chat_events.iter().map(|e| crate::http::Event {
            id: e.id, category: e.category.clone(), kind: e.kind.clone(),
            from: e.from.clone(), directed: e.directed, text: e.text.clone(),
        }));
    }

    /// Publish the current zone's doors from `gs` into the shared slot (GET /doors).
    pub fn sync_doors(&self, gs: &GameState) {
        let mut out = self.doors_shared.lock().unwrap();
        out.clear();
        out.extend(gs.doors.values().map(|d| crate::http::DoorView {
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

    /// Advance one navigation tick (no-op if fewer than 150 ms have elapsed).
    pub fn tick(
        &mut self,
        stream:  &mut EqStream,
        gs:      &mut GameState,
        app_tx:  &UnboundedSender<AppPacket>,
    ) {
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

        // POST /doors/click or a human door click: send OP_ClickDoor. The door opens
        // visually only when the server replies with OP_MoveDoor.
        if let Some(door_id) = self.door_click.lock().unwrap().take() {
            stream.send_app_packet(OP_CLICK_DOOR, &build_click_door(door_id, gs.player_id));
            tracing::info!("EQ: click door_id={}", door_id);
            gs.log_msg("door", &format!("Clicked door {}", door_id));
        }

        // Check zone-cross request — warp onto a zone line, then send OP_ZONE_CHANGE.
        let cross_req = self.zone_cross.lock().unwrap().take();
        if let Some(want_zone) = cross_req {
            // Choose a zone line: the requested destination if given (want_zone != 0),
            // otherwise the one nearest the player. Zone points are in server coords
            // (server_x = east, server_y = north) — same frame as the player.
            let exit = {
                let zps = self.zone_points.lock().unwrap();
                let candidates = zps.iter().filter(|zp| zp.zone_id != 0);
                if want_zone != 0 {
                    candidates
                        .filter(|zp| zp.zone_id == want_zone)
                        .min_by(|a, b| dist2(a, gs).total_cmp(&dist2(b, gs)))
                        .map(|zp| (zp.zone_id, zp.server_x, zp.server_y, zp.server_z))
                } else {
                    candidates
                        .min_by(|a, b| dist2(a, gs).total_cmp(&dist2(b, gs)))
                        .map(|zp| (zp.zone_id, zp.server_x, zp.server_y, zp.server_z))
                }
            };
            if let Some((dest_zone, _tx, _ty, _tz)) = exit {
                // Request the zone change to the DESTINATION zone. The server (ZoneUnsolicited)
                // looks up the closest zone point matching this target zone near our tracked
                // position and zones us there — so we send the player's real position (no warp;
                // warping to the destination's arrival coords put us far from the source trigger
                // and zoned us back to the same zone). The key is sending the TARGET zone id, not
                // our current zone id.
                tracing::info!("zone_cross: requesting zone change to zone_id={dest_zone} from ({:.1},{:.1})",
                          gs.player_x, gs.player_y);
                self.send_zone_change_packet(stream, gs, dest_zone);
            } else {
                tracing::info!("zone_cross: no zone line found for zone_id={want_zone}");
                gs.log_msg("zone", "No zone line found to cross");
            }
        }

        // Auto zone-cross: if the player is within range of a zone point, warp to
        // it and send OP_ZONE_CHANGE automatically. Cooldown prevents looping.
        {
            const ZONE_CROSS_COOLDOWN_MS: u128 = 10000; // 10 seconds
            if self.last_zone_cross.elapsed().as_millis() > ZONE_CROSS_COOLDOWN_MS {
            const ZONE_LINE_DIST: f32 = 15.0;
            const ZONE_LINE_DIST2: f32 = ZONE_LINE_DIST * ZONE_LINE_DIST;
            let zps = self.zone_points.lock().unwrap();
            let nearby = zps.iter()
                .filter(|zp| zp.zone_id != 0)
                .find(|zp| dist2(zp, gs) < ZONE_LINE_DIST2);
            if let Some(zp) = nearby {
                let dest = zp.zone_id;
                drop(zps); // release lock before mutating gs
                tracing::info!("zone_cross: auto-triggered near a zone line to zone_id={dest}");
                gs.log_msg("zone", &format!("Crossing to zone {}", dest));
                self.send_zone_change_packet(stream, gs, dest);
                self.last_zone_cross = Instant::now();
            }
            }
        }

        // Server-initiated zone change (portal door etc.): begin the normal zone-change
        // handshake to the requested destination, reusing the zone-cross path.
        if let Some(dest_zone) = gs.pending_server_zone.take() {
            tracing::info!("EQ: server-requested zone change → zone_id={dest_zone}");
            self.send_zone_change_packet(stream, gs, dest_zone);
            self.last_zone_cross = Instant::now();
        }

        // Check hail request — say "Hail, <name>" so a nearby NPC fires its hail script.
        let hail_name = self.hail.lock().unwrap().take();
        if let Some(name) = hail_name {
            let msg = format!("Hail, {}", name);
            let pkt = build_say_packet(&gs.player_name, &name, &msg);
            tracing::info!("EQ: hailing '{}' (say): {}", name, msg);
            stream.send_app_packet(OP_CHANNEL_MESSAGE, &pkt);
            gs.log_msg("chat", &format!("You say, '{}'", msg));
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
                gs.log_msg("chat", &format!("You say, '{}'", text));
            }
        }

        // Drain queued outgoing chat (POST /tell|/ooc|/shout|/group): build + send OP_ChannelMessage.
        let outgoing: Vec<crate::http::ChatSend> = {
            let mut q = self.chat_send.lock().unwrap();
            std::mem::take(&mut *q)
        };
        for c in outgoing {
            let pkt = build_channel_message(&gs.player_name, &c.to, c.chan, &c.text);
            stream.send_app_packet(OP_CHANNEL_MESSAGE, &pkt);
            let label = match c.chan { 7 => format!("tell {}", c.to), 5 => "ooc".into(),
                                       3 => "shout".into(), 2 => "group".into(), n => format!("chan{n}") };
            tracing::info!("EQ: {} -> {}", label, c.text);
            gs.log_msg("chat", &format!("You {}: {}", label, c.text));
        }

        // Check target request — set target + auto-consider it (con color comes back as
        // an OP_CONSIDER reply, handled in packet_handler).
        let target_id = self.target.lock().unwrap().take();
        if let Some(id) = target_id {
            gs.target_id = Some(id);
            if let Some(e) = gs.entities.get(&id) {
                gs.target_name = Some(e.name.clone());
            }
            stream.send_app_packet(OP_TARGET_MOUSE, &build_target_packet(id));
            stream.send_app_packet(OP_CONSIDER, &build_consider_packet(gs.player_id, id));
            tracing::info!("EQ: target spawn_id={} + consider", id);
        }

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

        // Cast a memorized spell gem on a target (current target, or self if none).
        let cast_req = self.cast.lock().unwrap().take();
        if let Some(req) = cast_req {
            let spell_id = gs.mem_spells.get(req.gem as usize).copied().unwrap_or(0xFFFF_FFFF);
            if spell_id != 0xFFFF_FFFF {
                let target = req.target_id.or(gs.target_id).unwrap_or(gs.player_id);
                stream.send_app_packet(OP_CAST_SPELL, &build_cast_packet(req.gem as u32, spell_id, target));
                tracing::info!("EQ: cast gem={} spell={} target={}", req.gem, spell_id, target);
            } else {
                tracing::info!("EQ: cast gem={} ignored — empty gem", req.gem);
            }
        }

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
            let what = match scribing { 0 => "scribe", 1 => "memorize", _ => "unmem" };
            tracing::info!("EQ: {what} spell={spell_id} slot={slot}");
            gs.log_msg("spell", &format!("{what} spell {spell_id} (slot {slot})"));
        }

        // Sit / stand (OP_SpawnAppearance type=14, param 110/100).
        let sit_req = self.sit.lock().unwrap().take();
        if let Some(sit) = sit_req {
            let param = if sit { 110u32 } else { 100u32 };
            stream.send_app_packet(OP_SPAWN_APPEARANCE,
                &build_spawn_appearance_packet(gs.player_id as u16, 14, param));
            gs.sitting = sit;
            tracing::info!("EQ: {}", if sit { "sit" } else { "stand" });
        }

        // Standalone consider.
        let con_req = self.consider.lock().unwrap().take();
        if let Some(id) = con_req {
            stream.send_app_packet(OP_CONSIDER, &build_consider_packet(gs.player_id, id));
            tracing::info!("EQ: consider spawn_id={}", id);
        }

        // Merchant buy: open the merchant (OP_ShopRequest) then buy its inventory slot
        // (OP_ShopPlayerBuy). Sent in sequence — the server processes the open first so the
        // merchant is open by the time the buy arrives. Must be within ~200u of the merchant.
        let buy_req = self.buy.lock().unwrap().take();
        if let Some((merchant_id, slot)) = buy_req {
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
            // Deduct the cost from on-hand coin for the HUD: the server takes the money with
            // update_client=false (Handle_OP_ShopPlayerBuy → TakeMoneyFromPP) and sends no
            // OP_MoneyUpdate, so the displayed coin would otherwise stay stale after a purchase.
            // spend_coin here only updates *this* (network-thread) GameState; the HUD / HTTP coin
            // is published from the render thread's separate GameState, which is fed solely by
            // packets through app_tx. So after deducting, synthesize an OP_MoneyUpdate carrying the
            // new total and route it through app_tx — apply_money_update applies it on the render
            // copy, keeping the HUD in sync (mirrors how real money packets reach both copies).
            let price = gs.merchant_items.iter().find(|m| m.merchant_slot == slot).map(|m| m.price);
            if let Some(p) = price {
                if gs.spend_coin(p as u64) {
                    let mut money = Vec::with_capacity(16);
                    for v in gs.coin { money.extend_from_slice(&(v as i32).to_le_bytes()); }
                    let _ = app_tx.send(AppPacket { opcode: OP_MONEY_UPDATE, payload: money });
                }
            }
            tracing::info!("EQ: shop buy — merchant_id={} slot={} qty=1 cost={}", merchant_id, slot, price.unwrap_or(0));
            gs.log_msg("merchant", &format!("Bought item (slot {})", slot));
        }

        // Merchant sell: open the merchant (OP_ShopRequest) then sell a player inventory slot
        // (OP_ShopPlayerSell). Same sequencing as buy so the shop is open server-side first.
        // Must be within ~200u of the merchant; the server computes the price (we send 0).
        let sell_req = self.sell.lock().unwrap().take();
        if let Some((merchant_id, slot, quantity)) = sell_req {
            let open = merchant_click(merchant_id, gs.player_id, 1);
            stream.send_app_packet(OP_SHOP_REQUEST, &open);
            // Merchant_Purchase_Struct (16b): npcid, itemslot(player slot), quantity, price.
            let mut sell = [0u8; 16];
            sell[0..4].copy_from_slice(&merchant_id.to_le_bytes());
            sell[4..8].copy_from_slice(&slot.to_le_bytes());
            sell[8..12].copy_from_slice(&quantity.to_le_bytes());
            // price = 0: the server charges its own buy-back price.
            stream.send_app_packet(OP_SHOP_PLAYER_SELL, &sell);
            tracing::info!("EQ: shop sell — merchant_id={} slot={} qty={}", merchant_id, slot, quantity);
            gs.log_msg("merchant", &format!("Sold item (slot {} x{})", slot, quantity));
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
            let open = merchant_click(merchant_id, gs.player_id, command);
            stream.send_app_packet(OP_SHOP_REQUEST, &open);
            tracing::info!("EQ: shop {} — merchant_id={}", if command == 1 { "open" } else { "close" }, merchant_id);
            if command == 0 { gs.merchant_open = None; gs.merchant_items.clear(); }
        }

        // Move/equip/unequip an item between inventory slots (OP_MoveItem).
        // MoveItem_Struct (12b): from_slot(u32), to_slot(u32), number_in_stack(u32).
        // number_in_stack MUST be 0 for a whole-item move (equip/unequip/rearrange): EQEmu's
        // SwapItem rejects number_in_stack > 0 for any non-stackable item (inventory.cpp ~2025,
        // "not a stackable item" -> SwapItemResync = the "Inventory Desyncronization" we hit). 0
        // takes the direct-swap/equip path. (A count would only be for splitting a stack.)
        let move_req = self.move_req.lock().unwrap().take();
        if let Some((from_slot, to_slot)) = move_req {
            let mut buf = [0u8; 12];
            buf[0..4].copy_from_slice(&from_slot.to_le_bytes());
            buf[4..8].copy_from_slice(&to_slot.to_le_bytes());
            buf[8..12].copy_from_slice(&0u32.to_le_bytes()); // number_in_stack = 0 (whole item)
            stream.send_app_packet(OP_MOVE_ITEM, &buf);
            // EQEmu applies the move silently (no echo), so mirror it into our snapshot or
            // /inventory goes stale and the next move corrupts it (phantom items).
            gs.move_item(from_slot as i32, to_slot as i32);
            tracing::info!("EQ: move item — from_slot={} to_slot={} qty=0(whole)", from_slot, to_slot);
            gs.log_msg("inventory", &format!("Moved item (slot {} -> {})", from_slot, to_slot));
        }

        if self.last_tick.elapsed().as_millis() < NAV_TICK_MS {
            return;
        }
        self.last_tick = Instant::now();

        // Quest turn-in (POST /give) trade-window state machine. Spans multiple ticks: we must
        // wait for the server's OP_TradeRequestAck (sets gs.trade_ack_ready) between sending the
        // trade request and moving the item into the NPC trade slot. Run on the throttled ~150ms
        // cadence so the per-tick ack timeout count matches the documented ~3s window.
        self.tick_give(stream, gs);

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
            let clear_to = |e: &crate::game_state::Entity| -> bool {
                col.as_ref().map_or(true, |c| {
                    c.path_clear([gs.player_x, gs.player_y, e.z + 3.0], [e.x, e.y, e.z + 3.0], 2.0)
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
                    gs.target_id = Some(id);
                    if let Some(e) = gs.entities.get(&id) { gs.target_name = Some(e.name.clone()); }
                    stream.send_app_packet(OP_TARGET_MOUSE, &build_target_packet(id));
                }
            }
        }

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
                        if dist > engage {
                            // Step toward the target (collision-aware), facing it. Capped to the
                            // native run speed like the main walk step.
                            let step = NAV_STEP.min(dist - engage);
                            let fdx = dx / dist * step;
                            let fdy = dy / dist * step;
                            let nz = gs.player_z;
                            let mv = match self.collision.read().unwrap().clone() {
                                None    => Some((fdx, fdy)),
                                Some(c) => slide_move(&c, gs.player_x, gs.player_y, nz, fdx, fdy, 2.0),
                            };
                            if let Some((mdx, mdy)) = mv {
                                let nx = gs.player_x + mdx;
                                let ny = gs.player_y + mdy;
                                self.send_position_update(stream, gs, nx, ny, nz, hdg);
                                gs.player_x = nx; gs.player_y = ny; gs.player_heading = hdg;
                                let _ = app_tx.send(make_position_packet(gs.player_id, nx, ny, nz, hdg));
                            }
                        } else {
                            // In melee range: hold and face the target.
                            self.send_position_update(stream, gs, gs.player_x, gs.player_y, gs.player_z, hdg);
                            gs.player_heading = hdg;
                        }
                        *self.goto_target.lock().unwrap() = None; // cancel any stale walk
                        return;
                    }
                }
            }
        }

        // Controlled fall in progress: descend at the native rate until landed, then apply native
        // fall damage (client-computed in EQ; the server only validates OP_EnvDamage). Takes
        // priority over normal walking so the descent isn't interrupted.
        if let Some(land_z) = self.falling {
            const FALL_STEP: f32 = 12.0; // ~native per-update descent (under the 12.8 wire cap)
            let next_z = (gs.player_z - FALL_STEP).max(land_z);
            let hdg = gs.player_heading;
            self.send_position_update(stream, gs, gs.player_x, gs.player_y, next_z, hdg);
            let _ = app_tx.send(make_position_packet(gs.player_id, gs.player_x, gs.player_y, next_z, hdg));
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
            return;
        }

        // Direct teleport (POST /warp): jump to the coords and tell the server, then CANCEL any
        // in-progress navigation. Unlike a /goto this does not path or walk, so it can't be dragged
        // back by a stalled walk (the old behavior wrote the warp coords into goto_target, which
        // made the nav thread try to *walk* there and stall). A teleport also stops a controlled fall.
        let warp_req = self.warp.lock().unwrap().take();
        if let Some((wx, wy, wz)) = warp_req {
            gs.player_x = wx;
            gs.player_y = wy;
            gs.player_z = wz;
            self.falling = None;
            self.path.clear();
            self.path_goal = None;
            *self.goto_target.lock().unwrap() = None;
            self.send_position_update(stream, gs, wx, wy, wz, gs.player_heading);
            tracing::info!("NAV: teleport (warp) to ({:.1},{:.1},{:.1}) — navigation cancelled", wx, wy, wz);
            return;
        }

        let goto = *self.goto_target.lock().unwrap(); // copy out so the lock is released
        let goal = match goto {
            Some(t) => t,
            None    => { self.path.clear(); self.path_goal = None; return }
        };

        // (Re)compute a wall-avoiding A* path when the goal changes. find_path returns
        // waypoints (goal-inclusive); an empty path falls back to a straight line to the goal.
        if self.path_goal != Some(goal) {
            self.path_goal = Some(goal);
            self.path_i = 0;
            self.stuck_i = 0;
            self.stuck_best = f32::MAX;
            self.stuck_ticks = 0;
            self.path = match self.collision.read().unwrap().as_ref() {
                Some(c) => c
                    .find_path([gs.player_x, gs.player_y, gs.player_z], [goal.0, goal.1, goal.2], 2.0)
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            tracing::info!("NAV: path to ({:.0},{:.0}) = {} waypoints", goal.0, goal.1, self.path.len());
        }

        // Aim at the current waypoint; advance past any we've already reached. The final
        // waypoint equals the goal, so reaching it falls through to the STOP_DIST arrival below.
        let target = loop {
            match self.path.get(self.path_i) {
                Some(&wp) => {
                    let wdx = wp[0] - gs.player_x;
                    let wdy = wp[1] - gs.player_y;
                    if (wdx * wdx + wdy * wdy).sqrt() <= 3.0 && self.path_i + 1 < self.path.len() {
                        self.path_i += 1;
                        continue;
                    }
                    // Use the waypoint's OWN floor z, so the move + collision happen at the right
                    // height while following a climbing/descending path (prevents clipping walls).
                    break (wp[0], wp[1], wp[2]);
                }
                // No path computed: straight-line toward the goal, but collision-check at the
                // player's CURRENT height (not the goal's z) so we still can't clip walls.
                None => break (goal.0, goal.1, gs.player_z),
            }
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
        if drop_to_target > FALL_TRIGGER && dist <= STOP_DIST + 8.0 {
            let (_, max_dmg) = fall_damage(drop_to_target);
            if gs.cur_hp > 0 && max_dmg >= gs.cur_hp as u32 {
                tracing::info!("NAV: fall of {:.0}u (up to {} dmg) would exceed {} hp — stopping at ledge",
                    drop_to_target, max_dmg, gs.cur_hp);
                gs.log_msg("zone", "Fall too dangerous (HP too low) — stopped at the ledge");
                *self.goto_target.lock().unwrap() = None;
                return;
            }
            self.falling = Some(target.2);
            self.fall_start_z = gs.player_z;
            tracing::info!("NAV: stepping off a {:.0}u drop — controlled fall begins", drop_to_target);
            return;
        }

        // Stop when within 2 units of target. Melee range is ~14 units so we stop well
        // within melee range, ensuring LOS succeeds even with nearby geometry.
        const STOP_DIST: f32 = 2.0;
        if dist <= STOP_DIST {
            tracing::info!("NAV: arrived at ({:.1},{:.1})", target.0, target.1);
            *self.goto_target.lock().unwrap() = None;
            // Send a final stationary position update facing the target.
            let hdg = eq_heading(dx, dy);
            self.send_position_update(stream, gs, gs.player_x, gs.player_y, gs.player_z, hdg);
            return;
        }

        // No-progress (stuck) detection. The only stops above are arrival and a controlled
        // fall; below, the sole stop is being fully boxed in (slide_move -> None). But a
        // sliding step can make ~zero net progress indefinitely — the avatar wedges into
        // tight platform/corridor geometry and slide_move keeps returning a perpendicular,
        // non-advancing step. That is a SILENT permanent stall (gfaydark #4, neriakc #2):
        // none of the three stop logs ever fire. Track progress toward the current aim and,
        // after NAV_STUCK_TICKS without closing distance, recover: skip to the next waypoint
        // to route past the snag, or — if this is the last/only waypoint — stop with a log.
        if self.path_i != self.stuck_i {
            self.stuck_i = self.path_i;
            self.stuck_best = f32::MAX;
            self.stuck_ticks = 0;
        }
        match nav_progress(dist, self.stuck_best, self.stuck_ticks) {
            (StuckAction::Continue, best, ticks) => {
                self.stuck_best = best;
                self.stuck_ticks = ticks;
            }
            (StuckAction::Recover, best, ticks) => {
                self.stuck_best = best;
                self.stuck_ticks = ticks;
                if self.path_i + 1 < self.path.len() {
                    tracing::info!("NAV: no progress toward waypoint {} near ({:.1},{:.1}) — skipping to next",
                        self.path_i, gs.player_x, gs.player_y);
                    self.path_i += 1;
                    self.stuck_i = self.path_i;
                    return;
                }
                tracing::info!("NAV: stalled (no progress) near ({:.1},{:.1}) — stopping",
                    gs.player_x, gs.player_y);
                gs.log_msg("zone", "Path stalled — stopped");
                *self.goto_target.lock().unwrap() = None;
                return;
            }
        }

        // Cap step to the native run speed (and never overshoot past STOP_DIST from the target).
        let step    = NAV_STEP.min(dist - STOP_DIST);
        let full_dx = dx / dist * step; // east component toward goal
        let full_dy = dy / dist * step; // north component toward goal
        // Use the z from goto_target rather than the stale spawn z stored in gs.player_z.
        // WASD sets goto_target.2 to the visual floor height (grounded z from the app's
        // ground snap), so this keeps the server and client z in sync and prevents the
        // server from rubber-banding the player back when it sees them at the wrong height.
        // While approaching a controlled-fall waypoint, stay at the current height (walk to the
        // edge) instead of snapping down to the landing z; the fall is handled above on arrival.
        let nz = if drop_to_target > FALL_TRIGGER { gs.player_z } else { target.2 };

        // Collision: slide along walls instead of walking through them. Try the full
        // step, then each axis alone; only stop (clear the goal) if fully boxed in.
        // Use nz (correct floor z) not gs.player_z (stale spawn z) for chest height.
        let chosen = match self.collision.read().unwrap().clone() {
            None    => Some((full_dx, full_dy)),
            Some(c) => slide_move(&c, gs.player_x, gs.player_y, nz, full_dx, full_dy, 2.0),
        };
        let (mdx, mdy) = match chosen {
            Some(v) => v,
            None => {
                tracing::info!("NAV: boxed in by walls near ({:.1},{:.1}) — stopping",
                          gs.player_x, gs.player_y);
                gs.log_msg("zone", "Path blocked by a wall");
                *self.goto_target.lock().unwrap() = None;
                return;
            }
        };

        let nx      = gs.player_x + mdx;
        let ny      = gs.player_y + mdy;
        let heading = eq_heading(mdx, mdy);

        self.send_position_update(stream, gs, nx, ny, nz, heading);

        gs.player_x       = nx;
        gs.player_y       = ny;
        gs.player_z       = nz;
        gs.player_heading = heading;

        // Synthetic server-side position packet so the render camera follows — carries the
        // step heading so the render loop faces the player along the path (Block B in app.rs
        // reads gs.player_heading, which this packet keeps live).
        let _ = app_tx.send(make_position_packet(gs.player_id, nx, ny, nz, heading));
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
                // Step 1: put the item on the cursor (skip if it's already there).
                if from_slot != SLOT_CURSOR {
                    let mut mv = [0u8; 12];
                    mv[0..4].copy_from_slice(&from_slot.to_le_bytes());
                    mv[4..8].copy_from_slice(&SLOT_CURSOR.to_le_bytes());
                    // number_in_stack = 0 → whole-item move (see the /inventory/move note above).
                    stream.send_app_packet(OP_MOVE_ITEM, &mv);
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
            // Step 3: move the cursor item into the NPC's first trade slot, then accept.
            let mut mv = [0u8; 12];
            mv[0..4].copy_from_slice(&SLOT_CURSOR.to_le_bytes());
            mv[4..8].copy_from_slice(&SLOT_TRADE_BEGIN.to_le_bytes());
            // number_in_stack = 0 → whole-item move.
            stream.send_app_packet(OP_MOVE_ITEM, &mv);
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
        let eq_heading = ((h_cw * 2048.0 / 360.0) as u32) & 0xFFF;

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
        stream.send_app_packet(OP_CLIENT_UPDATE, &buf);
    }

    /// Send OP_ZONE_CHANGE to request crossing a zone line to `target_zone_id`.
    /// ZoneChange_Struct (88 bytes): char_name[64] + zoneID(u16) + instance_id(u16)
    ///   + y(f32) + x(f32) + z(f32) + zone_reason(u32) + success(i32=0)
    /// NOTE: zoneID must be the DESTINATION zone, not our current zone — the server
    /// (ZoneUnsolicited) reads it as the target and finds the matching zone point near our
    /// tracked position. Sending our current zone made target==current → request cancelled.
    fn send_zone_change_packet(&self, stream: &mut EqStream, gs: &GameState, target_zone_id: u16) {
        // RoF2 ZoneChange_Struct is 100 bytes (rof2_structs.h): char_name[64], zoneID@64,
        // instanceID@66, Unknown068@68, Unknown072@72, y@76, x@80, z@84, zone_reason@88,
        // success@92, Unknown096@96. (Titanium put y/x/z at @68/@72/@76 — 8 bytes earlier — which
        // made the RoF2 server read garbage coords and silently ignore the zone-change request.)
        let mut buf = [0u8; 100];
        let name_bytes = gs.player_name.as_bytes();
        let name_len = name_bytes.len().min(64);
        buf[..name_len].copy_from_slice(&name_bytes[..name_len]);
        buf[64..66].copy_from_slice(&target_zone_id.to_le_bytes());   // zoneID = destination
        buf[66..68].copy_from_slice(&0u16.to_le_bytes());             // instanceID = 0
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
mod tests {
    use super::*;

    #[test]
    fn auto_combat_engages_add_attacking_player() {
        // Fighting rat #10 (valid, but NOT hitting us); rat #20 aggros and hits us → switch to #20.
        assert_eq!(
            pick_combat_target(Some(10), true, false, Some(20), Some(99)),
            Some(20),
        );
    }

    #[test]
    fn auto_combat_keeps_current_when_it_is_the_attacker() {
        // Current target is one of the mobs hitting us → stay on it; don't thrash to a second add.
        assert_eq!(
            pick_combat_target(Some(10), true, true, Some(20), Some(99)),
            Some(10),
        );
    }

    #[test]
    fn auto_combat_retargets_attacker_when_current_dead() {
        // Current target died; an add is on us → engage the add, not the nearest trash.
        assert_eq!(
            pick_combat_target(Some(10), false, false, Some(20), Some(99)),
            Some(20),
        );
    }

    #[test]
    fn auto_combat_falls_back_to_nearest_trash() {
        // No attacker, current invalid → nearest trash (existing grind behavior).
        assert_eq!(pick_combat_target(Some(10), false, false, None, Some(99)), Some(99));
        // No attacker, current still valid, nobody hitting us → finish current.
        assert_eq!(pick_combat_target(Some(10), true, false, None, Some(99)), Some(10));
        // Nothing to do.
        assert_eq!(pick_combat_target(None, false, false, None, None), None);
    }

    #[test]
    fn build_say_packet_matches_rof2_layout() {
        // RoF2 wire: sender\0 target\0 u32 unk | u32 lang | u32 chan | u32 unk | u8 unk |
        //            u32 skill | message\0   (see rof2.cpp DECODE(OP_ChannelMessage))
        let p = build_say_packet("Aiquestbot", "Guard Phaeton", "Hail, Guard Phaeton");
        let mut o = 0;
        assert_eq!(&p[o..o + 10], b"Aiquestbot"); o += 10;
        assert_eq!(p[o], 0, "sender NUL-terminated"); o += 1;
        assert_eq!(&p[o..o + 13], b"Guard Phaeton"); o += 13;
        assert_eq!(p[o], 0, "target NUL-terminated"); o += 1;
        assert_eq!(u32::from_le_bytes([p[o], p[o+1], p[o+2], p[o+3]]), 0, "unknown"); o += 4;
        assert_eq!(u32::from_le_bytes([p[o], p[o+1], p[o+2], p[o+3]]), 0, "language=CommonTongue"); o += 4;
        assert_eq!(u32::from_le_bytes([p[o], p[o+1], p[o+2], p[o+3]]), 8, "chan_num=Say"); o += 4;
        o += 4;            // unknown u32
        o += 1;            // unknown u8
        o += 4;            // skill_in_language
        let msg_end = o + "Hail, Guard Phaeton".len();
        assert_eq!(&p[o..msg_end], b"Hail, Guard Phaeton");
        assert_eq!(p[msg_end], 0, "message must be null-terminated");
        assert_eq!(p.len(), msg_end + 1);
    }

    fn wall_collision() -> crate::assets::Collision {
        // Vertical wall at world east=5: libeq p2=5 (render.X), north=p0 [0,10], height=p1 [0,10].
        let wall = crate::assets::MeshData {
            positions: vec![[0.0, 0.0, 5.0], [10.0, 0.0, 5.0], [10.0, 10.0, 5.0], [0.0, 10.0, 5.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0, 0.0, 0.0],
            render_mode: crate::assets::RenderMode::Opaque, anim: None,
        };
        crate::assets::Collision::build(
            &crate::assets::ZoneAssets { terrain: vec![wall], objects: vec![], textures: vec![] }, 4.0)
    }

    #[test]
    fn slide_move_slides_along_wall_when_diagonal_blocked() {
        let col = wall_collision();
        // Player at east=3, north=5, stepping toward the wall (east +2) and north (+2).
        // The diagonal hits the wall at east=5, so it should slide to north-only.
        // slide_move(col, px=east, py=north, z, full_dx=east, full_dy=north, radius)
        let r = slide_move(&col, 3.0, 5.0, 0.0, 2.0, 2.0, 2.0);
        assert_eq!(r, Some((0.0, 2.0)), "should slide along north, dropping the blocked east");

        // Moving away from the wall (east -2) is unobstructed → full move.
        assert_eq!(slide_move(&col, 3.0, 5.0, 0.0, -2.0, 2.0, 2.0), Some((-2.0, 2.0)));
    }

    #[test]
    fn nav_progress_resets_counter_when_closing_distance() {
        // Closed from best=20 to dist=10 (>EPS) → progress: counter resets, best updates.
        assert_eq!(nav_progress(10.0, 20.0, 5), (StuckAction::Continue, 10.0, 0));
    }

    #[test]
    fn nav_progress_accumulates_when_not_closing() {
        // dist not better than best (equal) → no progress: counter increments, best held.
        assert_eq!(nav_progress(10.0, 10.0, 5), (StuckAction::Continue, 10.0, 6));
        // dist worse than best (moved away) → still no progress.
        assert_eq!(nav_progress(12.0, 10.0, 5), (StuckAction::Continue, 10.0, 6));
    }

    #[test]
    fn nav_progress_tiny_gain_below_eps_is_not_progress() {
        // Closed only 0.5u (< EPS=1.0) → not counted as progress; counter still climbs.
        assert_eq!(nav_progress(9.5, 10.0, 5), (StuckAction::Continue, 10.0, 6));
    }

    #[test]
    fn nav_progress_recovers_at_stuck_threshold() {
        // One tick short of the threshold: still Continue.
        let (a, _, _) = nav_progress(10.0, 10.0, NAV_STUCK_TICKS as u32 - 2);
        assert_eq!(a, StuckAction::Continue);
        // The tick that reaches NAV_STUCK_TICKS no-progress ticks → Recover, counter reset.
        assert_eq!(
            nav_progress(10.0, 10.0, NAV_STUCK_TICKS - 1),
            (StuckAction::Recover, f32::MAX, 0)
        );
    }

    #[test]
    fn build_target_packet_is_spawn_id_le() {
        assert_eq!(build_target_packet(0x12345678), vec![0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn build_consider_packet_layout() {
        let p = build_consider_packet(7, 42);
        assert_eq!(p.len(), 28, "Consider_Struct must be exactly 28 bytes");
        assert_eq!(u32::from_le_bytes([p[0], p[1], p[2], p[3]]), 7);
        assert_eq!(u32::from_le_bytes([p[4], p[5], p[6], p[7]]), 42);
    }

    #[test]
    fn build_say_packet_names_are_nul_terminated() {
        // RoF2 names are variable-length cstrings (no fixed 64-byte field). Verify both the
        // sender and target are emitted whole and each terminated by a single NUL.
        let p = build_say_packet("Aiquestbot", "Guard Phaeton", "hi");
        assert_eq!(p[10], 0, "sender NUL-terminated after 'Aiquestbot'");
        assert_eq!(p[11 + 13], 0, "target NUL-terminated after 'Guard Phaeton'");
    }

    #[test]
    fn cast_packet_layout() {
        // gem 0, spell 200, target 1234 → [0, 200, 0xFFFF, 1234, 0] all u32 LE = 20 bytes.
        let p = build_cast_packet(0, 200, 1234);
        assert_eq!(p.len(), 20);
        assert_eq!(&p[0..4], &0u32.to_le_bytes());
        assert_eq!(&p[4..8], &200u32.to_le_bytes());
        assert_eq!(&p[8..12], &0xFFFFu32.to_le_bytes());
        assert_eq!(&p[12..16], &1234u32.to_le_bytes());
        assert_eq!(&p[16..20], &[0, 0, 0, 0]);
    }

    #[test]
    fn spawn_appearance_sit_layout() {
        // self 77, type 14 (Animation), 110 (sit) → 8 bytes: u16 id, u16 type, u32 param.
        let p = build_spawn_appearance_packet(77, 14, 110);
        assert_eq!(p.len(), 8);
        assert_eq!(&p[0..2], &77u16.to_le_bytes());
        assert_eq!(&p[2..4], &14u16.to_le_bytes());
        assert_eq!(&p[4..8], &110u32.to_le_bytes());
    }
}

#[cfg(test)]
mod door_tests {
    use super::*;

    #[test]
    fn click_door_layout() {
        let pkt = build_click_door(7, 0x1234);
        assert_eq!(pkt.len(), 16);
        assert_eq!(pkt[0], 7);            // doorid @0
        assert_eq!(pkt[4], 0);            // picklockskill @4 = 0 (observer)
        assert_eq!(&pkt[8..12], &[0, 0, 0, 0]); // item_id @8 = 0
        assert_eq!(&pkt[12..14], &0x1234u16.to_le_bytes()); // player_id @12
        assert_eq!(&pkt[14..16], &[0, 0]); // trailing unknowns zero
    }

    #[test]
    fn move_item_layout_is_from_to_zero() {
        // MoveItem_Struct: from_slot, to_slot, number_in_stack(=0 whole item). Used by the scribe
        // flow to put the scroll on the cursor (slot 33) before OP_MemorizeSpell. (eqoxide#11)
        let pkt = build_move_item(23, SLOT_CURSOR);
        assert_eq!(pkt.len(), 12);
        assert_eq!(u32::from_le_bytes(pkt[0..4].try_into().unwrap()), 23);
        assert_eq!(u32::from_le_bytes(pkt[4..8].try_into().unwrap()), SLOT_CURSOR);
        assert_eq!(u32::from_le_bytes(pkt[8..12].try_into().unwrap()), 0, "whole-item move");
    }
}
