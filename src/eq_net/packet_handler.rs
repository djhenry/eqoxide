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
        OP_FORMATTED_MESSAGE    => apply_formatted_message(gs, p),
        OP_SIMPLE_MESSAGE       => apply_simple_message(gs, p),
        OP_EMOTE                => apply_emote(gs, p),
        OP_CONSIDER             => apply_consider(gs, p),
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
        OP_DAMAGE               => apply_combat_damage(gs, p),
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
    let Some(upd) = decode_position_update(payload) else { return; };
    let sid = upd.spawn_id as u32;
    if sid == gs.player_id {
        gs.player_x = upd.x;
        gs.player_y = upd.y;
        gs.player_z = upd.z;
    } else if let Some(e) = gs.entities.get_mut(&sid) {
        e.x = upd.x;
        e.y = upd.y;
        e.z = upd.z;
        e.heading = upd.heading; // NPCs now face the direction the server reports
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

/// EQ class id (1..=16) → name. From EQEmu common/classes.h.
pub fn class_name(id: u32) -> &'static str {
    match id {
        1 => "Warrior", 2 => "Cleric", 3 => "Paladin", 4 => "Ranger",
        5 => "Shadow Knight", 6 => "Druid", 7 => "Monk", 8 => "Bard",
        9 => "Rogue", 10 => "Shaman", 11 => "Necromancer", 12 => "Wizard",
        13 => "Magician", 14 => "Enchanter", 15 => "Beastlord", 16 => "Berserker",
        _ => "",
    }
}

/// Useful fields parsed from the Titanium PlayerProfile_Struct.
pub struct ProfileInfo {
    pub level: u32,
    pub class_id: u32,
    pub coin: [u32; 4],  // platinum, gold, silver, copper
    pub stats: [u32; 7], // STR, STA, CHA, DEX, INT, AGI, WIS
}

/// Parse the Titanium PlayerProfile_Struct. Offsets from EQEmu
/// common/patches/titanium_structs.h: class_ @12, level @20, stats @2236..2260,
/// currency @4428..4440. Returns None if the payload is too short to be a full profile.
pub fn parse_player_profile(payload: &[u8]) -> Option<ProfileInfo> {
    if payload.len() < 4444 { return None; }
    let u32_at = |o: usize| u32::from_le_bytes([payload[o], payload[o + 1], payload[o + 2], payload[o + 3]]);
    Some(ProfileInfo {
        class_id: u32_at(12),
        level:    payload[20] as u32,
        stats: [
            u32_at(2236), u32_at(2240), u32_at(2244), u32_at(2248),
            u32_at(2252), u32_at(2256), u32_at(2260),
        ],
        coin: [u32_at(4428), u32_at(4432), u32_at(4436), u32_at(4440)],
    })
}

fn apply_player_profile(gs: &mut GameState, payload: &[u8]) {
    if let Some(p) = parse_player_profile(payload) {
        if (1..=65).contains(&p.level) {
            gs.player_level = p.level;
        }
        let cls = class_name(p.class_id);
        if !cls.is_empty() {
            gs.player_class = cls.to_string();
        }
        gs.coin = p.coin;
        gs.stats = p.stats;
    }
}

fn apply_death(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < SIZE_DEATH { return; }
    let d = unsafe { safe_read::<Death_S>(payload) };
    let d_id = d.spawn_id;
    if d_id == gs.player_id {
        gs.hp_pct    = 0.0;
        gs.strategy  = "Dead — waiting to respawn".into();
        eprintln!("EQ: combat: *** You have been slain! ***");
        gs.log_msg("combat", "*** You have been slain! ***");
    } else {
        let name = gs.entities.get(&d_id).map(|e| e.name.clone());
        if let Some(name) = name {
            if let Some(e) = gs.entities.get_mut(&d_id) {
                e.dead   = true;
                e.hp_pct = 0.0;
            }
            eprintln!("EQ: combat: {} has been slain", name);
            gs.log_msg("combat", &format!("{} has been slain", name));
        }
    }
}

fn apply_exp_update(gs: &mut GameState, payload: &[u8]) {
    if payload.len() >= 4 {
        gs.log_msg("exp", "Experience gained");
    }
}

// CombatDamage_Struct (23 bytes): target(u16) source(u16) type(u8) spellid(u16) damage(u32) ...
fn apply_combat_damage(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 11 { return; }
    let target_id = u16::from_le_bytes([payload[0], payload[1]]) as u32;
    let source_id = u16::from_le_bytes([payload[2], payload[3]]) as u32;
    let damage    = u32::from_le_bytes([payload[7], payload[8], payload[9], payload[10]]);
    let type_val  = payload[4];
    let target_name = gs.entities.get(&target_id).map(|e| e.name.clone())
        .unwrap_or_else(|| if target_id == gs.player_id { gs.player_name.clone() } else { format!("#{target_id}") });
    let source_name = gs.entities.get(&source_id).map(|e| e.name.clone())
        .unwrap_or_else(|| if source_id == gs.player_id { gs.player_name.clone() } else { format!("#{source_id}") });
    let msg = if damage == 0 {
        format!("{source_name} misses {target_name} (type={type_val})")
    } else {
        format!("{source_name} hits {target_name} for {damage} damage")
    };
    eprintln!("EQ: combat: {msg}");
    gs.log_msg("combat", &msg);
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

/// EQEmu sends GM-flagged accounts verbose quest/loot debug (e.g. each NPC's loot table:
/// "[Loot] [AddLootDrop] NPC [...] Item (...) ... trivial min/max [0/0] npc min/max [0/0]").
/// It floods the NPC dialogue panel and isn't player-facing, so drop it.
fn is_debug_spam(msg: &str) -> bool {
    msg.contains("AddLootDrop") || msg.contains("min/max") || msg.contains("[Loot]")
}

/// OP_FormattedMessage — eqstr-table text with arguments. Layout: unknown0(u32) +
/// string_id(u32) + type(u32) + args (null-separated strings). Resolved via the eqstr
/// table loaded at startup; if the table or id is missing, the raw args are shown.
fn apply_formatted_message(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 12 { return; }
    let string_id = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let args: Vec<String> = payload[12..]
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let text = crate::eqstr::format_id(string_id, &arg_refs)
        .unwrap_or_else(|| arg_refs.join(" "));
    if !text.trim().is_empty() && !is_debug_spam(&text) {
        gs.log_msg("system", &text);
    }
}

/// OP_Emote — world/NPC emote text (some quest flavor, often with [keywords]).
/// Emote_Struct: type(u32) + message[1024]. type 0xffffffff = animation command (no
/// useful text); only non-empty custom text is shown, in the NPC dialogue panel.
fn apply_emote(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 5 { return; }
    let etype = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    if etype == 0xffff_ffff { return; } // /dance, /flip, etc. — animation only
    let msg = String::from_utf8_lossy(&payload[4..])
        .trim_end_matches('\0')
        .trim()
        .to_string();
    if !msg.is_empty() && !is_debug_spam(&msg) {
        gs.log_msg("npc", &msg);
    }
}

/// OP_SimpleMessage — eqstr-table text, no arguments. Layout: string_id(u32) + color(u32)
/// + unknown(u32).
fn apply_simple_message(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 8 { return; }
    let string_id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    if let Some(text) = crate::eqstr::format_id(string_id, &[]) {
        if !text.trim().is_empty() && !is_debug_spam(&text) {
            gs.log_msg("system", &text);
        }
    }
}

/// Map a faction-con value (EQEmu FACTION_*, 1..=9) to the line EQ shows on /consider.
pub fn consider_message(faction: u32) -> &'static str {
    match faction {
        1 => "regards you as an ally",
        2 => "looks upon you warmly",
        3 => "kindly regards you",
        4 => "regards you amiably",
        5 => "regards you indifferently",
        6 => "looks your way apprehensively",
        7 => "looks at you dubiously",
        8 => "glares at you threateningly",
        9 => "scowls at you, ready to attack",
        _ => "regards you",
    }
}

/// Map an EQEmu ConsiderColor (the OP_Consider reply's `level` field) to a nameplate RGB.
/// Titanium remaps Gray→Green and White→WhiteTitanium server-side, but we cover all.
pub fn con_color(level: u32) -> [u8; 3] {
    match level {
        2  => [ 90, 220,  90], // Green   — trivial
        4  => [ 80, 120, 240], // DarkBlue
        6  => [150, 150, 150], // Gray    — no exp
        18 => [120, 200, 240], // LightBlue
        10 | 20 => [235, 235, 235], // White / WhiteTitanium — even con
        15 => [240, 230,  80], // Yellow  — slightly higher
        13 => [240,  80,  80], // Red     — much higher / dangerous
        _  => [235, 235, 235],
    }
}

/// OP_Consider reply — the server's con of our target. Consider_Struct: playerid(u32) +
/// targetid(u32) + faction(u32) + level(u32 = con color) + cur_hp + ... Sets the target
/// (so its nameplate highlights) plus the con-color tint and a /consider log line.
fn apply_consider(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 16 { return; }
    let target_id = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let faction   = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
    let level     = u32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]);
    let name = gs.entities.get(&target_id).map(|e| e.name.clone())
        .unwrap_or_else(|| "Your target".to_string());
    gs.target_id  = Some(target_id);
    gs.target_con = Some(con_color(level));
    let msg = format!("{} {}.", name, consider_message(faction));
    eprintln!("EQ: consider: {msg}");
    gs.log_msg("combat", &msg);
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
    if msg.trim().is_empty() || is_debug_spam(&msg) { return; }
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
        // Spawn_Struct curHp is an HP *percent* (100 for players, up to ~110 for some
        // NPCs), not absolute HP — so a damaged NPC spawns showing its real health.
        hp_pct:   (spawn.curHp as f32).min(100.0),
        cur_hp:   spawn.curHp as i32,
        max_hp:   spawn.max_hp as i32,
        race:     eq_race_to_code(spawn.race).to_string(),
        heading,
        dead:     false,
    });
}

#[cfg(test)]
mod tests {
    use super::{apply_emote, class_name, con_color, consider_message, parse_player_profile};
    use crate::game_state::GameState;

    #[test]
    fn class_name_maps_ids() {
        assert_eq!(class_name(1), "Warrior");
        assert_eq!(class_name(11), "Necromancer");
        assert_eq!(class_name(16), "Berserker");
        assert_eq!(class_name(0), "");
        assert_eq!(class_name(99), "");
    }

    #[test]
    fn parse_player_profile_reads_offsets() {
        // Too short → None.
        assert!(parse_player_profile(&[0u8; 100]).is_none());

        let mut buf = vec![0u8; 5000];
        buf[12..16].copy_from_slice(&9u32.to_le_bytes());   // class_ = Rogue
        buf[20] = 12;                                        // level
        buf[4428..4432].copy_from_slice(&5u32.to_le_bytes());   // platinum
        buf[4432..4436].copy_from_slice(&3u32.to_le_bytes());   // gold
        buf[4436..4440].copy_from_slice(&7u32.to_le_bytes());   // silver
        buf[4440..4444].copy_from_slice(&9u32.to_le_bytes());   // copper
        buf[2236..2240].copy_from_slice(&75u32.to_le_bytes());  // STR
        buf[2260..2264].copy_from_slice(&110u32.to_le_bytes()); // WIS
        let p = parse_player_profile(&buf).unwrap();
        assert_eq!(p.level, 12);
        assert_eq!(p.class_id, 9);
        assert_eq!(p.coin, [5, 3, 7, 9]);
        assert_eq!(p.stats[0], 75);  // STR
        assert_eq!(p.stats[6], 110); // WIS
        assert_eq!(class_name(p.class_id), "Rogue");
    }

    #[test]
    fn is_debug_spam_filters_gm_loot_dumps() {
        assert!(super::is_debug_spam(
            "[Loot] [AddLootDrop] NPC [Guard_Tyrak000] Item (5019) ... trivial min/max [0/0]"));
        assert!(!super::is_debug_spam("Greetings, traveler. Are you my [contact]?"));
    }

    #[test]
    fn con_color_maps_levels() {
        assert_eq!(con_color(2), [90, 220, 90]);    // green (trivial)
        assert_eq!(con_color(13), [240, 80, 80]);   // red (dangerous)
        assert_eq!(con_color(15), [240, 230, 80]);  // yellow
        assert_eq!(con_color(20), con_color(10));   // WhiteTitanium == White
        assert_eq!(con_color(999), [235, 235, 235]); // unknown → white
    }

    #[test]
    fn consider_message_covers_faction_cons() {
        assert_eq!(consider_message(9), "scowls at you, ready to attack");
        assert_eq!(consider_message(5), "regards you indifferently");
        assert_eq!(consider_message(1), "regards you as an ally");
        // Out-of-range falls back to a neutral phrasing.
        assert_eq!(consider_message(0), "regards you");
        assert_eq!(consider_message(99), "regards you");
    }

    fn emote_payload(etype: u32, msg: &str) -> Vec<u8> {
        let mut p = etype.to_le_bytes().to_vec();
        p.extend_from_slice(msg.as_bytes());
        p.push(0);
        p
    }

    #[test]
    fn apply_emote_logs_custom_text_skips_animations() {
        let mut gs = GameState::new();
        apply_emote(&mut gs, &emote_payload(0, "Guard Phaeton beckons you closer."));
        assert!(gs.messages.iter().any(|m| m.kind == "npc"
            && m.text == "Guard Phaeton beckons you closer."));

        // Animation-command emotes (0xffffffff) carry no useful text and are skipped.
        let before = gs.messages.len();
        apply_emote(&mut gs, &emote_payload(0xffff_ffff, ""));
        assert_eq!(gs.messages.len(), before, "animation emote should not be logged");
    }
}
