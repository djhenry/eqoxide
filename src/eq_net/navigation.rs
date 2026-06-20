//! Player navigation: walk toward a target position in 15-unit steps at 150 ms
//! intervals, sending EQ movement packets and notifying the render loop.

use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;

use crate::eq_net::protocol::*;
use crate::eq_net::transport::{AppPacket, EqStream};
use crate::game_state::{GameState, ZonePoint};
use crate::http::{AttackReq, EntityIds, EntityPositions, GotoTarget, HailReq, SayReq, TargetReq, ZoneCrossReq, ZonePoints};

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
    zone_cross:       ZoneCrossReq,
    hail:             HailReq,
    say:              SayReq,
    target:           TargetReq,
    attack:           AttackReq,
    collision:        crate::assets::SharedCollision,
    maps_dir:         std::path::PathBuf,
    current_zone:     String,
    last_zone_cross:  Instant,
    position_seq:     u16,
    last_tick:        Instant,
}

impl Navigator {
    pub fn new(
        goto_target:      GotoTarget,
        entity_positions: EntityPositions,
        entity_ids:       EntityIds,
        zone_points:      ZonePoints,
        zone_cross:       ZoneCrossReq,
        hail:             HailReq,
        say:              SayReq,
        target:           TargetReq,
        attack:           AttackReq,
        collision:        crate::assets::SharedCollision,
        maps_dir:         std::path::PathBuf,
    ) -> Self {
        Navigator {
            goto_target,
            entity_positions,
            entity_ids,
            zone_points,
            zone_cross,
            hail,
            say,
            target,
            attack,
            collision,
            maps_dir,
            current_zone: String::new(),
            last_zone_cross: Instant::now(),
            position_seq: 0,
            last_tick: Instant::now(),
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
            if let Some((dest_zone, tx, ty, tz)) = exit {
                eprintln!("zone_cross: warping to zone line for zone_id={dest_zone} at ({:.1},{:.1},{:.1})", tx, ty, tz);
                gs.player_x = tx;
                gs.player_y = ty;
                gs.player_z = tz;
                // Report our position at the zone line so the server's zone-line check passes.
                let _ = app_tx.send(make_position_packet(gs.player_id, tx, ty, tz));
                self.send_position_update(stream, gs, tx, ty, tz, 0.0);
                self.send_zone_change_packet(stream, gs);
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
                let tx = zp.server_x;
                let ty = zp.server_y;
                let tz = zp.server_z;
                drop(zps); // release lock before mutating gs
                eprintln!("zone_cross: auto-triggered near zone_id={dest} at ({:.1},{:.1},{:.1})", tx, ty, tz);
                gs.log_msg("zone", &format!("Crossing to zone {}", dest));
                gs.player_x = tx;
                gs.player_y = ty;
                gs.player_z = tz;
                let _ = app_tx.send(make_position_packet(gs.player_id, tx, ty, tz));
                self.send_position_update(stream, gs, tx, ty, tz, gs.player_heading);
                self.send_zone_change_packet(stream, gs);
                self.last_zone_cross = Instant::now();
            }
            }
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
            let payload = [if on { 1u8 } else { 0u8 }, 0, 0, 0];
            stream.send_app_packet(OP_AUTO_ATTACK, &payload);
            eprintln!("EQ: auto-attack {}", if on { "ON" } else { "OFF" });
        }

        if self.last_tick.elapsed().as_millis() < 150 {
            return;
        }
        self.last_tick = Instant::now();

        let target = match *self.goto_target.lock().unwrap() {
            Some(t) => t,
            None    => return,
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
        // CW (0=north, 90=east). Convert then pack into the 12-bit field:
        // EQ_heading_cw_units = deg_cw * 512/360, heading_12bit = EQ_heading_cw_units * 8.
        let h_cw = crate::eq_net::protocol::ccw_to_cw(heading);
        let eq_heading = ((h_cw * 4096.0 / 360.0) as u16) & 0xFFF;

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

        eprintln!("POS: x={:.1} y={:.1} z={:.1} hdg={:.0} eq12_hdg={} anim={}", x, y, z, heading, eq_heading, anim);
        stream.send_app_packet(OP_CLIENT_UPDATE, &buf);
    }

    /// Send OP_ZONE_CHANGE to request crossing a zone line.
    /// ZoneChange_Struct (88 bytes): char_name[64] + zone_id(u16) + instance_id(u16)
    ///   + y(f32) + x(f32) + z(f32) + zone_reason(u32) + success(i32=0)
    fn send_zone_change_packet(&self, stream: &mut EqStream, gs: &GameState) {
        let mut buf = [0u8; 88];
        let name_bytes = gs.player_name.as_bytes();
        let name_len = name_bytes.len().min(64);
        buf[..name_len].copy_from_slice(&name_bytes[..name_len]);
        // zone_id = current zone (tells server which zone_point table to look up)
        buf[64..66].copy_from_slice(&gs.zone_id.to_le_bytes());
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
        eprintln!("EQ: sending OP_ZONE_CHANGE from zone_id={} pos=({:.1},{:.1},{:.1})",
                  gs.zone_id, gs.player_x, gs.player_y, gs.player_z);
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
            &crate::assets::ZoneAssets { meshes: vec![wall], textures: vec![] }, 4.0)
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
}
