//! Player navigation: walk toward a target position in 15-unit steps at 150 ms
//! intervals, sending EQ movement packets and notifying the render loop.

use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;

use crate::eq_net::protocol::*;
use crate::eq_net::transport::{AppPacket, EqStream};
use crate::game_state::GameState;
use crate::http::{EntityPositions, GotoTarget, HailReq, SayReq, TargetReq, ZoneCrossReq, ZonePoints};

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

pub struct Navigator {
    goto_target:      GotoTarget,
    entity_positions: EntityPositions,
    zone_points:      ZonePoints,
    zone_cross:       ZoneCrossReq,
    hail:             HailReq,
    say:              SayReq,
    target:           TargetReq,
    collision:        crate::assets::SharedCollision,
    position_seq:     u16,
    last_tick:        Instant,
}

impl Navigator {
    pub fn new(
        goto_target:      GotoTarget,
        entity_positions: EntityPositions,
        zone_points:      ZonePoints,
        zone_cross:       ZoneCrossReq,
        hail:             HailReq,
        say:              SayReq,
        target:           TargetReq,
        collision:        crate::assets::SharedCollision,
    ) -> Self {
        Navigator {
            goto_target,
            entity_positions,
            zone_points,
            zone_cross,
            hail,
            say,
            target,
            collision,
            position_seq: 0,
            last_tick: Instant::now(),
        }
    }

    /// Copy all entity positions from `gs` into the shared entity map
    /// (used by the HTTP /entities endpoint and /goto by-name lookup).
    pub fn sync_entities(&self, gs: &GameState) {
        let mut map = self.entity_positions.lock().unwrap();
        for e in gs.entities.values() {
            map.insert(e.name.clone(), (e.x, e.y, e.z));
        }
    }

    /// Copy zone exit points from `gs` into the shared zone_points map
    /// (used by the HTTP /zone_points endpoint).
    pub fn sync_zone_points(&self, gs: &GameState) {
        if !gs.zone_points.is_empty() {
            *self.zone_points.lock().unwrap() = gs.zone_points.clone();
        }
    }

    /// Advance one navigation tick (no-op if fewer than 150 ms have elapsed).
    pub fn tick(
        &mut self,
        stream:  &mut EqStream,
        gs:      &mut GameState,
        app_tx:  &UnboundedSender<AppPacket>,
    ) {
        // Check zone_cross flag — send OP_ZONE_CHANGE if set.
        let do_cross = {
            let mut flag = self.zone_cross.lock().unwrap();
            if *flag { *flag = false; true } else { false }
        };
        if do_cross {
            self.send_zone_change_packet(stream, gs);
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
            stream.send_app_packet(OP_TARGET_COMMAND, &build_target_packet(id));
            stream.send_app_packet(OP_CONSIDER, &build_consider_packet(gs.player_id, id));
            eprintln!("EQ: target spawn_id={} + consider", id);
        }

        if self.last_tick.elapsed().as_millis() < 150 {
            return;
        }
        self.last_tick = Instant::now();

        let target = match *self.goto_target.lock().unwrap() {
            Some(t) => t,
            None    => return,
        };

        let dx   = target.0 - gs.player_x;
        let dy   = target.1 - gs.player_y;
        let dist = (dx * dx + dy * dy).sqrt();

        if dist <= 10.0 {
            eprintln!("NAV: arrived at ({:.1},{:.1})", target.0, target.1);
            *self.goto_target.lock().unwrap() = None;
            return;
        }

        let step    = 15.0_f32.min(dist);
        let nx      = gs.player_x + dx / dist * step;
        let ny      = gs.player_y + dy / dist * step;
        let nz      = gs.player_z;
        let heading = dy.atan2(dx).to_degrees().rem_euclid(360.0);

        // Collision: don't path through walls. Cast at chest height from the current
        // position to the proposed step (world points are [east, north, height], so
        // east = player_y / ny, north = player_x / nx). If blocked, stop and clear the
        // goal — scripted /goto should route around buildings via the A* navpath tool.
        if let Some(col) = self.collision.read().unwrap().clone() {
            let chest = gs.player_z + 3.0;
            let from = [gs.player_y, gs.player_x, chest];
            let to   = [ny, nx, chest];
            if !col.path_clear(from, to, 2.0) {
                eprintln!("NAV: blocked by wall at ({:.1},{:.1}) — stopping", nx, ny);
                gs.log_msg("zone", "Path blocked by a wall");
                *self.goto_target.lock().unwrap() = None;
                return;
            }
        }

        self.send_position_update(stream, gs, nx, ny, nz, heading);

        gs.player_x       = nx;
        gs.player_y       = ny;
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
        let dx = x - gs.player_x;
        let dy = y - gs.player_y;
        let dz = z - gs.player_z;
        let moving = dx != 0.0 || dy != 0.0 || dz != 0.0;
        let anim: i32 = if moving { 1 } else { 0 };
        // EQ uses 0-511 heading units
        let eq_heading = ((heading * 512.0 / 360.0) as u16) & 0x1FF;

        let mut buf = [0u8; 36];
        buf[0..2].copy_from_slice(&(gs.player_id as u16).to_le_bytes());
        buf[2..4].copy_from_slice(&self.position_seq.to_le_bytes());
        self.position_seq = self.position_seq.wrapping_add(1);
        buf[4..8].copy_from_slice(&y.to_le_bytes());
        buf[8..12].copy_from_slice(&dz.to_le_bytes());
        buf[12..16].copy_from_slice(&dx.to_le_bytes());
        buf[16..20].copy_from_slice(&dy.to_le_bytes());
        buf[20..24].copy_from_slice(&anim.to_le_bytes());
        buf[24..28].copy_from_slice(&x.to_le_bytes());
        buf[28..32].copy_from_slice(&z.to_le_bytes());
        buf[32..34].copy_from_slice(&eq_heading.to_le_bytes());

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
        // y = player_y (east/west = server_y)
        buf[68..72].copy_from_slice(&gs.player_y.to_le_bytes());
        // x = player_x (north/south = server_x)
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
/// `scene.player_pos` and keep the camera attached during navigation.
///
/// Layout mirrors SpawnPositionUpdate_S (30 bytes, server→client):
///   spawn_id(u16) | delta_heading(i16) | y(f32) | delta_z(f32)
///   z(f32) | delta_x(f32) | x(f32) | delta_y(f32) | animation(u8) | heading(u8)
pub fn make_position_packet(spawn_id: u32, x: f32, y: f32, z: f32) -> AppPacket {
    let mut buf = [0u8; 30];
    buf[0..2].copy_from_slice(&(spawn_id as u16).to_le_bytes());
    buf[4..8].copy_from_slice(&y.to_le_bytes());
    buf[12..16].copy_from_slice(&z.to_le_bytes());
    buf[20..24].copy_from_slice(&x.to_le_bytes());
    AppPacket { opcode: OP_CLIENT_UPDATE, payload: buf.to_vec() }
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
