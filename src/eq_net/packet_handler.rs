//! Single source of truth for applying EQ server packets to GameState.
//!
//! Called from both the login phase (to keep entity positions current) and the
//! render loop (to update the scene).  No I/O or logging here — just pure state
//! mutation.

use crate::eq_net::protocol::*;
use crate::eq_net::transport::AppPacket;
use crate::game_state::{GameState, Entity, ZonePoint};

/// Apply one EQ server packet to `gs`.
pub fn apply_packet(gs: &mut GameState, packet: &AppPacket) {
    let p = &packet.payload;
    match packet.opcode {
        OP_NEW_SPAWN            => apply_new_spawn(gs, p),
        OP_DELETE_SPAWN         => apply_delete_spawn(gs, p),
        OP_CLIENT_UPDATE        => apply_position_update(gs, p),
        OP_HP_UPDATE            => apply_hp_update(gs, p),
        OP_NEW_ZONE             => apply_new_zone(gs, p),
        OP_ZONE_SPAWNS          => apply_zone_spawns(gs, p),
        OP_ZONE_ENTRY           => apply_zone_entry(gs, p),
        OP_WEATHER              => { gs.zone_changed = false; }
        OP_PLAYER_PROFILE       => apply_player_profile(gs, p),
        OP_DEATH                => apply_death(gs, p),
        OP_EXP_UPDATE           => apply_exp_update(gs, p),
        OP_LEVEL_UPDATE         => apply_level_update(gs, p),
        OP_CHANNEL_MESSAGE      => apply_channel_message(gs, p),
        OP_SPECIAL_MESG         => apply_special_message(gs, p),
        OP_SEND_ZONE_POINTS           => apply_zone_points(gs, p),
        OP_REQUEST_CLIENT_ZONE_CHANGE => {
            if p.len() >= 74 {
                let zone_id = u16::from_le_bytes([p[64], p[65]]);
                eprintln!("EQ: OP_REQUEST_CLIENT_ZONE_CHANGE → zone_id={zone_id} ({} bytes)", p.len());
            } else {
                eprintln!("EQ: OP_REQUEST_CLIENT_ZONE_CHANGE ({} bytes)", p.len());
            }
            gs.log_msg("zone", "Zone change requested by server");
        }
        OP_ZONE_PLAYER_TO_BIND  => apply_bind_respawn(gs, p),
        _                       => {}
    }
}

// ── Per-opcode helpers ────────────────────────────────────────────────────────

fn apply_new_spawn(gs: &mut GameState, payload: &[u8]) {
    if payload.len() >= SIZE_SPAWN {
        let spawn = unsafe { safe_read::<Spawn_S>(payload) };
        register_spawn(gs, spawn);
    }
}

fn apply_delete_spawn(gs: &mut GameState, payload: &[u8]) {
    if payload.len() >= 4 {
        let id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        gs.remove_entity(id);
    }
}

fn apply_position_update(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < SIZE_SPAWN_POSITION_UPDATE { return; }
    let upd = unsafe { safe_read::<SpawnPositionUpdate_S>(payload) };
    let sid = upd.spawn_id as u32;
    if sid == gs.player_id {
        gs.player_x = upd.x;
        gs.player_y = upd.y;
        gs.player_z = upd.z;
    } else if let Some(e) = gs.entities.get_mut(&sid) {
        e.x = upd.x;
        e.y = upd.y;
        e.z = upd.z;
    }
}

fn apply_hp_update(gs: &mut GameState, payload: &[u8]) {
    if payload.len() >= SIZE_HP_UPDATE {
        let hp = unsafe { safe_read::<HPUpdate_S>(payload) };
        gs.update_hp(hp.spawn_id as u32, hp.cur_hp as i32, hp.max_hp);
    }
}

fn apply_new_zone(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < SIZE_NEW_ZONE { return; }
    let zone = unsafe { safe_read::<NewZone_S>(payload) };
    gs.zone_name = zone.zone_short_str();
    gs.zone_id   = zone.zone_id;
    gs.safe_x    = zone.safe_x;
    gs.safe_y    = zone.safe_y;
    gs.safe_z    = zone.safe_z;
    gs.zone_changed = true;
    gs.entities.clear();
    gs.log_msg("zone", &format!("Entered {}", gs.zone_name));
}

fn apply_zone_spawns(gs: &mut GameState, payload: &[u8]) {
    let mut offset = 0;
    while offset + SIZE_SPAWN <= payload.len() {
        let spawn = unsafe { safe_read::<Spawn_S>(&payload[offset..]) };
        register_spawn(gs, spawn);
        offset += SIZE_SPAWN;
    }
}

fn apply_zone_entry(gs: &mut GameState, payload: &[u8]) {
    // Server echoes our own Spawn_S back with a possible 0-, 2-, or 4-byte prefix.
    for offset in [0usize, 2, 4] {
        if payload.len() < offset + SIZE_SPAWN { continue; }
        let spawn = unsafe { safe_read::<Spawn_S>(&payload[offset..]) };
        let name = spawn.name_str();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        if !gs.player_name.is_empty() && name == gs.player_name {
            let (x, y, z, heading) = extract_spawn_position(
                spawn.bitfield_pos1, spawn.bitfield_pos2,
                spawn.bitfield_pos3, spawn.bitfield_pos4,
            );
            gs.player_id      = spawn.spawnId;
            gs.player_x       = x;
            gs.player_y       = y;
            gs.player_z       = z;
            gs.player_heading = heading;
            gs.player_level   = spawn.level as u32;
            gs.player_race    = eq_race_to_code(spawn.race).to_string();
            eprintln!("EQ: player located via OP_ZONE_ENTRY id={} pos=({:.1},{:.1},{:.1})",
                      gs.player_id, x, y, z);
        }
        break;
    }
}

fn apply_player_profile(gs: &mut GameState, payload: &[u8]) {
    if payload.len() > 104 {
        let level = payload[104];
        if (1..=65).contains(&level) {
            gs.player_level = level as u32;
        }
    }
}

fn apply_death(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < SIZE_DEATH { return; }
    let d = unsafe { safe_read::<Death_S>(payload) };
    let d_id = d.spawn_id;
    if d_id == gs.player_id {
        gs.hp_pct    = 0.0;
        gs.strategy  = "Dead — waiting to respawn".into();
        gs.log_msg("combat", "*** You have been slain! ***");
    } else {
        let name = gs.entities.get(&d_id).map(|e| e.name.clone());
        if let Some(name) = name {
            if let Some(e) = gs.entities.get_mut(&d_id) {
                e.dead   = true;
                e.hp_pct = 0.0;
            }
            gs.log_msg("combat", &format!("{} has been slain", name));
        }
    }
}

fn apply_exp_update(gs: &mut GameState, payload: &[u8]) {
    if payload.len() >= 4 {
        gs.log_msg("exp", "Experience gained");
    }
}

fn apply_level_update(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < SIZE_LEVEL_UPDATE { return; }
    let lu    = unsafe { safe_read::<LevelUpdate_S>(payload) };
    let level = lu.level;
    gs.player_level = level;
    gs.log_msg("exp", &format!("*** Level {}! ***", level));
}

fn apply_channel_message(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 140 { return; }
    let sender = String::from_utf8_lossy(&payload[64..128])
        .trim_end_matches('\0').to_string();
    let msg = String::from_utf8_lossy(&payload[140..])
        .trim_end_matches('\0').to_string();
    if !sender.is_empty() {
        gs.log_msg("chat", &format!("<{}> {}", sender, msg));
    }
}

/// OP_SpecialMesg — NPC dialogue / emotes, where quest text arrives.
/// SpecialMesg_Struct: header[3] + msg_type(u32) + target_spawn_id(u32) +
/// sayer(null-terminated, variable) + unknown[12] + message(null-terminated).
/// Logged with kind "npc" so the quest dialogue panel can pick it out.
fn apply_special_message(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 12 { return; }
    // sayer is a null-terminated string starting at offset 11.
    let sayer_start = 11;
    let rel_end = payload[sayer_start..].iter().position(|&b| b == 0);
    let Some(rel_end) = rel_end else { return; };
    let sayer = String::from_utf8_lossy(&payload[sayer_start..sayer_start + rel_end]).to_string();
    // message follows sayer's null + 12 unknown bytes.
    let msg_start = sayer_start + rel_end + 1 + 12;
    if msg_start >= payload.len() { return; }
    let msg = String::from_utf8_lossy(&payload[msg_start..])
        .trim_end_matches('\0')
        .to_string();
    if msg.trim().is_empty() { return; }
    if sayer.is_empty() {
        gs.log_msg("npc", &msg);
    } else {
        gs.log_msg("npc", &format!("{} says, '{}'", sayer, msg));
    }
}

fn apply_zone_points(gs: &mut GameState, payload: &[u8]) {
    // Wire format: optional 4-byte header + N × ZonePointEntry_S (24 bytes each).
    // Detect header: if (len-4) % 24 == 0 and len >= 4, skip header.
    let offset = if payload.len() >= 4 && (payload.len() - 4) % SIZE_ZONE_POINT_ENTRY == 0 {
        4
    } else {
        0
    };
    gs.zone_points.clear();
    let mut i = offset;
    while i + SIZE_ZONE_POINT_ENTRY <= payload.len() {
        let e = unsafe { safe_read::<ZonePointEntry_S>(&payload[i..]) };
        // EQ wire convention: struct field y = north = server_x, field x = east = server_y
        gs.zone_points.push(ZonePoint {
            iterator: e.iterator,
            server_x: e.y,
            server_y: e.x,
            server_z: e.z,
            heading:  e.heading,
            zone_id:  e.zoneid,
        });
        i += SIZE_ZONE_POINT_ENTRY;
    }
    eprintln!("EQ: {} zone exit points received:", gs.zone_points.len());
    for zp in &gs.zone_points {
        eprintln!("  zone_id={} server_x={:.1} server_y={:.1} z={:.1} heading={:.1}",
                  zp.zone_id, zp.server_x, zp.server_y, zp.server_z, zp.heading);
    }
}

fn apply_bind_respawn(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 20 { return; }
    gs.player_x = f32::from_le_bytes([payload[4],  payload[5],  payload[6],  payload[7]]);
    gs.player_y = f32::from_le_bytes([payload[8],  payload[9],  payload[10], payload[11]]);
    gs.player_z = f32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]);
    gs.strategy = "Respawning...".into();
    gs.log_msg("zone", "Respawning at bind point");
}

// ── Shared spawn registration ─────────────────────────────────────────────────

/// Insert or update one spawn in `gs`. If it matches the player name the
/// player fields are updated instead and the spawn is NOT added to entities.
pub fn register_spawn(gs: &mut GameState, spawn: Spawn_S) {
    let (x, y, z, heading) = extract_spawn_position(
        spawn.bitfield_pos1, spawn.bitfield_pos2,
        spawn.bitfield_pos3, spawn.bitfield_pos4,
    );
    let name    = spawn.name_str();
    let is_npc  = spawn.NPC != 0;

    if !is_npc && !gs.player_name.is_empty() && name == gs.player_name {
        gs.player_id      = spawn.spawnId;
        gs.player_x       = x;
        gs.player_y       = y;
        gs.player_z       = z;
        gs.player_heading = heading;
        gs.player_level   = spawn.level as u32;
        gs.player_race    = eq_race_to_code(spawn.race).to_string();
        let sid = spawn.spawnId;
        eprintln!("EQ: player spawn id={} pos=({:.1},{:.1},{:.1})", sid, x, y, z);
        return;
    }

    gs.upsert_entity(Entity {
        spawn_id: spawn.spawnId,
        name,
        level:    spawn.level as u32,
        is_npc,
        x, y, z,
        hp_pct:   100.0,
        cur_hp:   spawn.curHp as i32,
        max_hp:   spawn.max_hp as i32,
        race:     eq_race_to_code(spawn.race).to_string(),
        heading,
        dead:     false,
    });
}
