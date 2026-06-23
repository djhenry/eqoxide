//! Player navigation: walk toward a target position in 15-unit steps at 150 ms
//! intervals, sending EQ movement packets and notifying the render loop.

use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;

use crate::eq_net::protocol::*;
use crate::eq_net::transport::{AppPacket, EqStream};
use crate::game_state::{GameState, ZonePoint};
use crate::http::{AttackReq, BuyReq, DoorClickReq, DoorsShared, MoveReq, GiveReq, InventoryShared, LootReq, MessagesShared, CastReq, SitReq, ConsiderReq, EntityIds, EntityPositions, GotoTarget, HailReq, SayReq, TargetReq, TaskLog, ZoneCrossReq, ZonePoints};

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

/// Build a Titanium `ChannelMessage_Struct` for the Say channel (used for NPC hails).
///
/// Layout (see EQEmu common/patches/titanium_structs.h):
///   targetname[64] | sender[64] | language(u32) | chan_num(u32)
///   | cm_unknown4[2](u32×2) | skill_in_language(u32) | message[var]\0
/// chan_num 8 = ChatChannel_Say; the server delivers say text to NPCs within 200
/// units, triggering EVENT_SAY (a "Hail, <name>" message fires the NPC's hail script).
pub fn build_say_packet(sender: &str, target: &str, message: &str) -> Vec<u8> {
    let mut buf = vec![0u8; 148 + message.len() + 1];
    let t = target.as_bytes();
    let tl = t.len().min(63);
    buf[..tl].copy_from_slice(&t[..tl]);
    let s = sender.as_bytes();
    let sl = s.len().min(63);
    buf[64..64 + sl].copy_from_slice(&s[..sl]);
    // language @128 = 0 (CommonTongue), already zero.
    buf[132..136].copy_from_slice(&8u32.to_le_bytes()); // chan_num = ChatChannel_Say
    buf[144..148].copy_from_slice(&100u32.to_le_bytes()); // skill_in_language
    let m = message.as_bytes();
    buf[148..148 + m.len()].copy_from_slice(m);
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
    hail:             HailReq,
    say:              SayReq,
    target:           TargetReq,
    attack:           AttackReq,
    buy:              BuyReq,
    move_req:         MoveReq,
    give:             GiveReq,
    cast:             CastReq,
    sit:              SitReq,
    consider:         ConsiderReq,
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
}

impl Navigator {
    pub fn new(
        goto_target:      GotoTarget,
        entity_positions: EntityPositions,
        entity_ids:       EntityIds,
        zone_points:      ZonePoints,
        task_log:         TaskLog,
        zone_cross:       ZoneCrossReq,
        hail:             HailReq,
        say:              SayReq,
        target:           TargetReq,
        attack:           AttackReq,
        buy:              BuyReq,
        move_req:         MoveReq,
        give:             GiveReq,
        inventory:        InventoryShared,
        loot:             LootReq,
        door_click:       DoorClickReq,
        doors_shared:     DoorsShared,
        messages:         MessagesShared,
        cast:             CastReq,
        sit:              SitReq,
        consider:         ConsiderReq,
        collision:        crate::assets::SharedCollision,
        maps_dir:         std::path::PathBuf,
    ) -> Self {
        Navigator {
            goto_target,
            entity_positions,
            entity_ids,
            zone_points,
            task_log,
            zone_cross,
            hail,
            say,
            target,
            attack,
            buy,
            move_req,
            give,
            cast,
            sit,
            consider,
            give_state: None,
            inventory,
            loot,
            door_click,
            doors_shared,
            messages,
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
                    eprintln!("zone_map: added exit '{}' at ({:.1}, {:.1}) → zone_id={}",
                              label.text, label.east, label.north, dest_zone_id);
                }
                if shared.len() > before {
                    eprintln!("zone_map: {} fallback exits added (total {})", shared.len() - before, shared.len());
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
            eprintln!("loot: queued corpse_id={} for looting (via POST /loot)", corpse_id);
        }

        // POST /doors/click or a human door click: send OP_ClickDoor. The door opens
        // visually only when the server replies with OP_MoveDoor.
        if let Some(door_id) = self.door_click.lock().unwrap().take() {
            stream.send_app_packet(OP_CLICK_DOOR, &build_click_door(door_id, gs.player_id));
            eprintln!("EQ: click door_id={}", door_id);
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
                eprintln!("zone_cross: requesting zone change to zone_id={dest_zone} from ({:.1},{:.1})",
                          gs.player_x, gs.player_y);
                self.send_zone_change_packet(stream, gs, dest_zone);
            } else {
                eprintln!("zone_cross: no zone line found for zone_id={want_zone}");
                gs.log_msg("zone", "No zone line found to cross");
            }
        }

        // Auto zone-cross: if the player is within range of a zone point, warp to
        // it and send OP_ZONE_CHANGE automatically. Cooldown prevents looping.
        {
            const ZONE_LINE_DIST: f32 = 15.0;
            const ZONE_LINE_DIST2: f32 = ZONE_LINE_DIST * ZONE_LINE_DIST;
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
                eprintln!("zone_cross: auto-triggered near a zone line to zone_id={dest}");
                gs.log_msg("zone", &format!("Crossing to zone {}", dest));
                self.send_zone_change_packet(stream, gs, dest);
                self.last_zone_cross = Instant::now();
            }
            }
        }

        // Server-initiated zone change (portal door etc.): begin the normal zone-change
        // handshake to the requested destination, reusing the zone-cross path.
        if let Some(dest_zone) = gs.pending_server_zone.take() {
            eprintln!("EQ: server-requested zone change → zone_id={dest_zone}");
            self.send_zone_change_packet(stream, gs, dest_zone);
            self.last_zone_cross = Instant::now();
        }

        // Check hail request — say "Hail, <name>" so a nearby NPC fires its hail script.
        let hail_name = self.hail.lock().unwrap().take();
        if let Some(name) = hail_name {
            let msg = format!("Hail, {}", name);
            let pkt = build_say_packet(&gs.player_name, &name, &msg);
            eprintln!("EQ: hailing '{}' (say): {}", name, msg);
            stream.send_app_packet(OP_CHANNEL_MESSAGE, &pkt);
            gs.log_msg("chat", &format!("You say, '{}'", msg));
        }

        // Check say request — arbitrary Say text (HUD say box / quest keyword follow-up).
        let say_text = self.say.lock().unwrap().take();
        if let Some(text) = say_text {
            let pkt = build_say_packet(&gs.player_name, "", &text);
            eprintln!("EQ: say: {}", text);
            stream.send_app_packet(OP_CHANNEL_MESSAGE, &pkt);
            gs.log_msg("chat", &format!("You say, '{}'", text));
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
            eprintln!("EQ: target spawn_id={} + consider", id);
        }

        // Check attack request — send OP_AUTO_ATTACK(1) to start, OP_AUTO_ATTACK(0) to stop.
        // Server expects exactly 4 bytes; byte[0]=1 enables, byte[0]=0 disables.
        let attack_req = self.attack.lock().unwrap().take();
        if let Some(on) = attack_req {
            self.auto_attack = on;
            let payload = [if on { 1u8 } else { 0u8 }, 0, 0, 0];
            stream.send_app_packet(OP_AUTO_ATTACK, &payload);
            gs.auto_attack = on;
            eprintln!("EQ: auto-attack {}", if on { "ON" } else { "OFF" });
        }

        // Cast a memorized spell gem on a target (current target, or self if none).
        let cast_req = self.cast.lock().unwrap().take();
        if let Some(req) = cast_req {
            let spell_id = gs.mem_spells.get(req.gem as usize).copied().unwrap_or(0xFFFF_FFFF);
            if spell_id != 0xFFFF_FFFF {
                let target = req.target_id.or(gs.target_id).unwrap_or(gs.player_id);
                stream.send_app_packet(OP_CAST_SPELL, &build_cast_packet(req.gem as u32, spell_id, target));
                eprintln!("EQ: cast gem={} spell={} target={}", req.gem, spell_id, target);
            } else {
                eprintln!("EQ: cast gem={} ignored — empty gem", req.gem);
            }
        }

        // Sit / stand (OP_SpawnAppearance type=14, param 110/100).
        let sit_req = self.sit.lock().unwrap().take();
        if let Some(sit) = sit_req {
            let param = if sit { 110u32 } else { 100u32 };
            stream.send_app_packet(OP_SPAWN_APPEARANCE,
                &build_spawn_appearance_packet(gs.player_id as u16, 14, param));
            gs.sitting = sit;
            eprintln!("EQ: {}", if sit { "sit" } else { "stand" });
        }

        // Standalone consider.
        let con_req = self.consider.lock().unwrap().take();
        if let Some(id) = con_req {
            stream.send_app_packet(OP_CONSIDER, &build_consider_packet(gs.player_id, id));
            eprintln!("EQ: consider spawn_id={}", id);
        }

        // Merchant buy: open the merchant (OP_ShopRequest) then buy its inventory slot
        // (OP_ShopPlayerBuy). Sent in sequence — the server processes the open first so the
        // merchant is open by the time the buy arrives. Must be within ~200u of the merchant.
        let buy_req = self.buy.lock().unwrap().take();
        if let Some((merchant_id, slot)) = buy_req {
            // MerchantClick_Struct (24b): npc_id, player_id, command(1=open), rate, tab, unk.
            let mut open = [0u8; 24];
            open[0..4].copy_from_slice(&merchant_id.to_le_bytes());
            open[4..8].copy_from_slice(&gs.player_id.to_le_bytes());
            open[8..12].copy_from_slice(&1u32.to_le_bytes());
            stream.send_app_packet(OP_SHOP_REQUEST, &open);
            // Merchant_Sell_Struct (24b): npcid, playerid, itemslot, unknown12, quantity, price.
            let mut buy = [0u8; 24];
            buy[0..4].copy_from_slice(&merchant_id.to_le_bytes());
            buy[4..8].copy_from_slice(&gs.player_id.to_le_bytes());
            buy[8..12].copy_from_slice(&slot.to_le_bytes());
            buy[16..20].copy_from_slice(&1u32.to_le_bytes()); // quantity = 1
            stream.send_app_packet(OP_SHOP_PLAYER_BUY, &buy);
            eprintln!("EQ: shop buy — merchant_id={} slot={} qty=1", merchant_id, slot);
            gs.log_msg("merchant", &format!("Bought item (slot {})", slot));
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
            eprintln!("EQ: move item — from_slot={} to_slot={} qty=0(whole)", from_slot, to_slot);
            gs.log_msg("inventory", &format!("Moved item (slot {} -> {})", from_slot, to_slot));
        }

        if self.last_tick.elapsed().as_millis() < 150 {
            return;
        }
        self.last_tick = Instant::now();

        // Quest turn-in (POST /give) trade-window state machine. Spans multiple ticks: we must
        // wait for the server's OP_TradeRequestAck (sets gs.trade_ack_ready) between sending the
        // trade request and moving the item into the NPC trade slot. Run on the throttled ~150ms
        // cadence so the per-tick ack timeout count matches the documented ~3s window.
        self.tick_give(stream, gs);

        // Auto-retarget: while auto-attacking, if the current target is gone or dead, pick the
        // nearest trash mob (name starts "a_"/"an_", which excludes named guards/merchants/
        // citizens) within ~200u, so grinding continues hands-free between kills.
        if self.auto_attack {
            let col = self.collision.read().unwrap();
            let clear_to = |e: &crate::game_state::Entity| -> bool {
                col.as_ref().map_or(true, |c| {
                    c.path_clear([gs.player_x, gs.player_y, e.z + 3.0], [e.x, e.y, e.z + 3.0], 2.0)
                })
            };
            // The current target is valid only if alive AND still reachable in a straight line —
            // otherwise drop it so we retarget or roam (don't get stuck swinging "too far").
            let valid = gs.target_id
                .and_then(|id| gs.entities.get(&id))
                .map(|e| !e.dead && clear_to(e))
                .unwrap_or(false);
            if !valid {
                // Engage the nearest reachable (clear-path) land mob within 200u. If none are
                // reachable we simply idle and wait for a respawn rather than roam — qcat is a maze
                // of sealed pockets, so roaming toward an out-of-pocket mob just strands her.
                let mut best_clear: Option<(f32, u32)> = None;
                for (id, e) in &gs.entities {
                    if e.dead || !e.is_npc { continue; }
                    let nl = e.name.to_ascii_lowercase();
                    if !(nl.starts_with("a_") || nl.starts_with("an_")) { continue; }
                    let dx = e.x - gs.player_x;
                    let dy = e.y - gs.player_y;
                    let d2 = dx * dx + dy * dy;
                    if d2 > 200.0 * 200.0 || !clear_to(e) { continue; }
                    if best_clear.map(|(bd, _)| d2 < bd).unwrap_or(true) { best_clear = Some((d2, *id)); }
                }
                drop(col);
                if let Some((_, id)) = best_clear {
                    gs.target_id = Some(id);
                    if let Some(e) = gs.entities.get(&id) { gs.target_name = Some(e.name.clone()); }
                    stream.send_app_packet(OP_TARGET_MOUSE, &build_target_packet(id));
                }
            }
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
                        let hdg = if dist > 0.01 { eq_heading(dx, dy) } else { gs.player_heading };
                        if dist > MELEE {
                            // Step toward the target (collision-aware), facing it.
                            let step = 8.0_f32.min(dist - MELEE);
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
                                let _ = app_tx.send(make_position_packet(gs.player_id, nx, ny, nz));
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
            self.path = match self.collision.read().unwrap().as_ref() {
                Some(c) => c
                    .find_path([gs.player_x, gs.player_y, gs.player_z], [goal.0, goal.1, goal.2], 2.0)
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            eprintln!("NAV: path to ({:.0},{:.0}) = {} waypoints", goal.0, goal.1, self.path.len());
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

        // Stop when within 2 units of target. Melee range is ~14 units so we stop well
        // within melee range, ensuring LOS succeeds even with nearby geometry.
        const STOP_DIST: f32 = 2.0;
        if dist <= STOP_DIST {
            eprintln!("NAV: arrived at ({:.1},{:.1})", target.0, target.1);
            *self.goto_target.lock().unwrap() = None;
            // Send a final stationary position update facing the target.
            let hdg = eq_heading(dx, dy);
            self.send_position_update(stream, gs, gs.player_x, gs.player_y, gs.player_z, hdg);
            return;
        }

        // Cap step so we never overshoot past STOP_DIST from the target.
        let step    = 10.0_f32.min(dist - STOP_DIST);
        let full_dx = dx / dist * step; // east component toward goal
        let full_dy = dy / dist * step; // north component toward goal
        // Use the z from goto_target rather than the stale spawn z stored in gs.player_z.
        // WASD sets goto_target.2 to the visual floor height (grounded z from the app's
        // ground snap), so this keeps the server and client z in sync and prevents the
        // server from rubber-banding the player back when it sees them at the wrong height.
        let nz = target.2;

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
                eprintln!("NAV: boxed in by walls near ({:.1},{:.1}) — stopping",
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

        // Synthetic server-side position packet so the render camera follows.
        let _ = app_tx.send(make_position_packet(gs.player_id, nx, ny, nz));
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
                }
                // Send OP_TradeRequest { to_mob_id = npc, from_mob_id = player }.
                let mut req = [0u8; 8];
                req[0..4].copy_from_slice(&npc_id.to_le_bytes());
                req[4..8].copy_from_slice(&gs.player_id.to_le_bytes());
                stream.send_app_packet(OP_TRADE_REQUEST, &req);
                gs.trade_ack_ready = false;
                self.give_state = Some(GiveState { npc_id, ticks_waiting: 0 });
                eprintln!("EQ: give: OP_TradeRequest to npc_id={} (item slot {})", npc_id, from_slot);
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
            let mut accept = [0u8; 8];
            accept[0..4].copy_from_slice(&gs.player_id.to_le_bytes());
            // unknown4 = 0 (already zeroed).
            stream.send_app_packet(OP_TRADE_ACCEPT_CLICK, &accept);
            eprintln!("EQ: give: cursor→trade slot + OP_TradeAcceptClick (npc_id={})", npc_id);
            self.give_state = None;
            gs.trade_ack_ready = false;
        } else if let Some(g) = self.give_state.as_mut() {
            g.ticks_waiting += 1;
            if g.ticks_waiting >= GIVE_ACK_TIMEOUT_TICKS {
                // Abort: cancel the (possibly half-open) trade session and reset.
                stream.send_app_packet(OP_CANCEL_TRADE, &[]);
                eprintln!("EQ: give: no trade ack (timed out)");
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
        let h_cw = crate::eq_net::protocol::ccw_to_cw(heading);
        let eq_heading = ((h_cw * 2048.0 / 360.0) as u16) & 0xFFF;

        let mut buf = [0u8; 36];
        buf[0..2].copy_from_slice(&(gs.player_id as u16).to_le_bytes());
        buf[2..4].copy_from_slice(&self.position_seq.to_le_bytes());
        self.position_seq = self.position_seq.wrapping_add(1);
        // Titanium PlayerPositionUpdateClient_Struct: server x,y,z map directly to the
        // wire's x_pos/y_pos/z_pos — no axis swap. y_pos@4, delta_x@12, delta_y@16,
        // x_pos@24, z_pos@28, heading@32.
        buf[4..8].copy_from_slice(&y.to_le_bytes());    // y_pos  = server_y (north)
        buf[8..12].copy_from_slice(&dz.to_le_bytes());  // delta_z
        buf[12..16].copy_from_slice(&dx.to_le_bytes()); // delta_x = east delta
        buf[16..20].copy_from_slice(&dy.to_le_bytes()); // delta_y = north delta
        buf[20..24].copy_from_slice(&anim.to_le_bytes());
        buf[24..28].copy_from_slice(&x.to_le_bytes());  // x_pos  = server_x (east)
        buf[28..32].copy_from_slice(&z.to_le_bytes());  // z_pos  = server_z (height)
        buf[32..34].copy_from_slice(&eq_heading.to_le_bytes());

        stream.send_app_packet(OP_CLIENT_UPDATE, &buf);
    }

    /// Send OP_ZONE_CHANGE to request crossing a zone line to `target_zone_id`.
    /// ZoneChange_Struct (88 bytes): char_name[64] + zoneID(u16) + instance_id(u16)
    ///   + y(f32) + x(f32) + z(f32) + zone_reason(u32) + success(i32=0)
    /// NOTE: zoneID must be the DESTINATION zone, not our current zone — the server
    /// (ZoneUnsolicited) reads it as the target and finds the matching zone point near our
    /// tracked position. Sending our current zone made target==current → request cancelled.
    fn send_zone_change_packet(&self, stream: &mut EqStream, gs: &GameState, target_zone_id: u16) {
        let mut buf = [0u8; 88];
        let name_bytes = gs.player_name.as_bytes();
        let name_len = name_bytes.len().min(64);
        buf[..name_len].copy_from_slice(&name_bytes[..name_len]);
        // zoneID = DESTINATION zone we want to travel to.
        buf[64..66].copy_from_slice(&target_zone_id.to_le_bytes());
        // instance_id = 0
        buf[66..68].copy_from_slice(&0u16.to_le_bytes());
        // ZoneChange_Struct: y(server_y=north) @68, x(server_x=east) @72 — Y-first, no swap.
        buf[68..72].copy_from_slice(&gs.player_y.to_le_bytes());
        buf[72..76].copy_from_slice(&gs.player_x.to_le_bytes());
        // z
        buf[76..80].copy_from_slice(&gs.player_z.to_le_bytes());
        // zone_reason = 0 (normal zone line crossing)
        buf[80..84].copy_from_slice(&0u32.to_le_bytes());
        // success = 0 (client→server request)
        buf[84..88].copy_from_slice(&0i32.to_le_bytes());
        eprintln!("EQ: sending OP_ZONE_CHANGE target_zone={} from current_zone={} pos=({:.1},{:.1},{:.1})",
                  target_zone_id, gs.zone_id, gs.player_x, gs.player_y, gs.player_z);
        stream.send_app_packet(OP_ZONE_CHANGE, &buf);
    }
}

/// Build a synthetic OP_CLIENT_UPDATE packet so the render loop can update
/// `scene.player_pos` and keep the camera attached during navigation. Uses the real
/// Titanium bit-packed wire format so it decodes the same way as server updates.
pub fn make_position_packet(spawn_id: u32, x: f32, y: f32, z: f32) -> AppPacket {
    AppPacket {
        opcode: OP_CLIENT_UPDATE,
        payload: encode_position_update(spawn_id as u16, x, y, z),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_say_packet_matches_titanium_layout() {
        let p = build_say_packet("Aiquestbot", "Guard Phaeton", "Hail, Guard Phaeton");
        // sender at offset 64
        assert_eq!(&p[64..74], b"Aiquestbot");
        // targetname at offset 0
        assert_eq!(&p[0..13], b"Guard Phaeton");
        // chan_num (u32 @132) == 8 (ChatChannel_Say)
        assert_eq!(u32::from_le_bytes([p[132], p[133], p[134], p[135]]), 8);
        // language (u32 @128) == 0 (CommonTongue)
        assert_eq!(u32::from_le_bytes([p[128], p[129], p[130], p[131]]), 0);
        // message begins at offset 148, null-terminated
        let msg_end = 148 + "Hail, Guard Phaeton".len();
        assert_eq!(&p[148..msg_end], b"Hail, Guard Phaeton");
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
    fn build_say_packet_truncates_overlong_names() {
        let long = "X".repeat(200);
        let p = build_say_packet(&long, &long, "hi");
        // sender/target fields are 64 bytes; name capped at 63 + null padding.
        assert_eq!(p[63], 0, "targetname must stay null-terminated within 64 bytes");
        assert_eq!(p[127], 0, "sender must stay null-terminated within 64 bytes");
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
}
