//! Player navigation: walk toward a target position in 15-unit steps at 150 ms
//! intervals, sending EQ movement packets and notifying the render loop.

use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;

use crate::eq_net::protocol::*;
use crate::eq_net::transport::{AppPacket, EqStream};
use crate::game_state::GameState;
use crate::http::{EntityPositions, GotoTarget, ZoneCrossReq, ZonePoints};

pub struct Navigator {
    goto_target:      GotoTarget,
    entity_positions: EntityPositions,
    zone_points:      ZonePoints,
    zone_cross:       ZoneCrossReq,
    position_seq:     u16,
    last_tick:        Instant,
}

impl Navigator {
    pub fn new(
        goto_target:      GotoTarget,
        entity_positions: EntityPositions,
        zone_points:      ZonePoints,
        zone_cross:       ZoneCrossReq,
    ) -> Self {
        Navigator {
            goto_target,
            entity_positions,
            zone_points,
            zone_cross,
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
