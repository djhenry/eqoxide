//! EQ protocol opcodes and struct definitions for Titanium client (port 5998).
//!
//! Ported from the Python reference at eq_client/protocol/opcodes.py and
//! eq_client/protocol/structs.py.

use std::mem;

// ── Transport-layer opcodes ────────────────────────────────────────────────

pub const OP_SESSION_REQUEST: u8 = 0x01;
pub const OP_SESSION_RESPONSE: u8 = 0x02;
pub const OP_COMBINED: u8 = 0x03;
pub const OP_SESSION_DISC: u8 = 0x05;
pub const OP_KEEPALIVE: u8 = 0x06;
pub const OP_STAT_REQUEST: u8 = 0x07;
pub const OP_STAT_RESPONSE: u8 = 0x08;
pub const OP_PACKET: u8 = 0x09;
pub const OP_FRAGMENT: u8 = 0x0d;
pub const OP_FRAGMENT_CONT: u8 = 0x0e;
pub const OP_FRAGMENT_CONT2: u8 = 0x0f;
pub const OP_FRAGMENT_CONT3: u8 = 0x10;
pub const OP_OUT_OF_ORDER: u8 = 0x11;
pub const OP_ACK: u8 = 0x15;
pub const OP_APP_COMBINED: u8 = 0x19;
pub const OP_OUT_OF_SESSION: u8 = 0x1d;

// ── Encoding flags ─────────────────────────────────────────────────────────

pub const ENCODE_NONE: u8 = 0;
pub const ENCODE_COMPRESSION: u8 = 1;
pub const ENCODE_XOR: u8 = 4;

// ── Login server opcodes ──────────────────────────────────────────────────

pub const OP_SESSION_READY: u16 = 0x0001;
pub const OP_LOGIN: u16 = 0x0002;
pub const OP_SERVER_LIST_REQUEST: u16 = 0x0004;
pub const OP_PLAY_EVERQUEST_REQ: u16 = 0x000d;
pub const OP_CHAT_MESSAGE: u16 = 0x0016;
pub const OP_LOGIN_ACCEPTED: u16 = 0x0017;
pub const OP_SERVER_LIST_RESPONSE: u16 = 0x0018;
pub const OP_PLAY_EVERQUEST_RESP: u16 = 0x0021;

// ── World server opcodes ──────────────────────────────────────────────────

pub const OP_SEND_LOGIN_INFO: u16 = 0x4dd0;
pub const OP_APPROVE_WORLD: u16 = 0x3c25;
pub const OP_LOG_SERVER: u16 = 0x0fa6;
pub const OP_MOTD: u16 = 0x024d;
pub const OP_SEND_CHAR_INFO: u16 = 0x4513;
pub const OP_ENTER_WORLD: u16 = 0x7cba;
pub const OP_POST_ENTER_WORLD: u16 = 0x52a4;
pub const OP_ZONE_SERVER_INFO: u16 = 0x61b6;
pub const OP_WORLD_COMPLETE: u16 = 0x509d;
pub const OP_WORLD_CLIENT_READY: u16 = 0x5e99;
pub const OP_EXPANSION_INFO: u16 = 0x04ec;
pub const OP_WORLD_CRC1: u16 = 0x5072;
pub const OP_WORLD_CRC2: u16 = 0x5b18;
pub const OP_GUILD_LIST: u16 = 0x6957;

// ── Zone server opcodes ───────────────────────────────────────────────────

pub const OP_ZONE_ENTRY: u16 = 0x7213;
pub const OP_ACK_PACKET: u16 = 0x7752;
pub const OP_NEW_ZONE: u16 = 0x0920;
pub const OP_REQ_CLIENT_SPAWN: u16 = 0x0322;
pub const OP_ZONE_SPAWNS: u16 = 0x2e78;
pub const OP_CHAR_INVENTORY: u16 = 0x5394;
pub const OP_SET_SERVER_FILTER: u16 = 0x6563;
pub const OP_REQ_NEW_ZONE: u16 = 0x7ac5;
pub const OP_PLAYER_PROFILE: u16 = 0x75df;
pub const OP_TIME_OF_DAY: u16 = 0x1580;
pub const OP_WEATHER: u16 = 0x254d;
pub const OP_SEND_ZONE_POINTS: u16 = 0x3eba;
pub const OP_SPAWN_DOOR: u16 = 0x4c24;
pub const OP_SEND_EXP_ZONE_IN: u16 = 0x0587;
pub const OP_CLIENT_READY: u16 = 0x5e20;

// ── Gameplay: spawns & positions ──────────────────────────────────────────

pub const OP_NEW_SPAWN: u16 = 0x1860;
pub const OP_DELETE_SPAWN: u16 = 0x55bc;
pub const OP_CLIENT_UPDATE: u16 = 0x14cb;
pub const OP_SPAWN_APPEARANCE: u16 = 0x7c32;

// ── Gameplay: combat ──────────────────────────────────────────────────────

pub const OP_HP_UPDATE: u16 = 0x3bcf;
pub const OP_DEATH: u16 = 0x6160;
pub const OP_DAMAGE: u16 = 0x5c78;
pub const OP_AUTO_ATTACK: u16 = 0x5e55;
pub const OP_AUTO_ATTACK2: u16 = 0x0701;
pub const OP_TARGET_COMMAND: u16 = 0x1477;
pub const OP_CONSIDER: u16 = 0x65ca;

// ── Gameplay: progression ─────────────────────────────────────────────────

pub const OP_EXP_UPDATE: u16 = 0x5ecd;
pub const OP_LEVEL_UPDATE: u16 = 0x6d44;

// ── Chat ──────────────────────────────────────────────────────────────────

pub const OP_CHANNEL_MESSAGE: u16 = 0x1004;

// ── Misc zone→client ──────────────────────────────────────────────────────

pub const OP_ZONE_PLAYER_TO_BIND: u16 = 0x385e;
pub const OP_ZONE_CHANGE: u16 = 0x5dd8;
pub const OP_REQUEST_CLIENT_ZONE_CHANGE: u16 = 0x7834;
pub const OP_LOGOUT: u16 = 0x61ff;

// ── Struct definitions ────────────────────────────────────────────────────

/// Read a packed struct from a byte slice. Pads with zeros if data is shorter
/// than the struct size.
pub unsafe fn safe_read<T: Copy>(data: &[u8]) -> T {
    let size = mem::size_of::<T>();
    let mut buf = vec![0u8; size];
    let len = data.len().min(size);
    buf[..len].copy_from_slice(&data[..len]);
    std::ptr::read_unaligned(buf.as_ptr() as *const T)
}

// ── Spawn_S bitfield position extraction ───────────────────────────────────

/// Extract (x, y, z, heading) from a Spawn_S's bitfield blocks.
/// EQ stores coords as 19-bit signed integers scaled by 1/8.
/// Heading is 12-bit in EQ units (0-511 per circle), converted to degrees.
pub fn extract_spawn_position(
    bitfield_pos1: u32,
    bitfield_pos2: u32,
    bitfield_pos3: u32,
    bitfield_pos4: u32,
) -> (f32, f32, f32, f32) {
    fn s19(bits: u32) -> f32 {
        let bits = bits & 0x7FFFF;
        let val = if bits & 0x40000 != 0 {
            bits as i32 - 0x80000
        } else {
            bits as i32
        };
        val as f32 / 8.0
    }

    fn s12_to_degrees(bits: u32) -> f32 {
        let bits = bits & 0xFFF;
        let val = if bits & 0x800 != 0 {
            bits as i32 - 0x1000
        } else {
            bits as i32
        };
        val as f32 * (360.0 / 512.0)
    }

    let x = s19((bitfield_pos1 >> 10) & 0x7FFFF);
    let y = s19(bitfield_pos2 & 0x7FFFF);
    let z = s19(bitfield_pos3 & 0x7FFFF);
    let heading = s12_to_degrees((bitfield_pos4 >> 13) & 0xFFF);
    (x, y, z, heading)
}

// ── Race ID → renderer code mapping ────────────────────────────────────────

pub fn eq_race_to_code(race_id: u32) -> &'static str {
    match race_id {
        // Playable races
        1 => "HUM", 2 => "BAR", 3 => "ERU", 4 => "ELF", 5 => "HEF", 6 => "DKE",
        7 => "HEF", 8 => "DWF", 9 => "TRL", 10 => "OGR", 11 => "HFL", 12 => "GNM",
        128 => "IKS", 522 => "VAH",
        // Humanoid NPCs
        13 => "BRD", 15 => "HUM", 16 => "HUM", 17 => "HUM", 18 => "HUM",
        19 => "SKE", 22 => "HUM", 26 => "SKE", 27 => "ZOM", 28 => "BRD",
        29 => "HUM", 31 => "GNL", 32 => "HUM", 33 => "ZOM", 34 => "HUM",
        35 => "HUM", 36 => "ZOM", 37 => "ZOM", 38 => "HUM", 39 => "SKE",
        40 => "HUM", 41 => "GNL",
        // Animals / Creatures
        14 => "WOL", 20 => "GNL", 21 => "WOL", 23 => "SNA", 24 => "SPI",
        25 => "GNL", 30 => "GNL", 42 => "RAT", 43 => "FIS", 44 => "FIS",
        45 => "FRG", 46 => "BRD", 47 => "BEA", 48 => "FLY", 49 => "WSP",
        50 => "BEA", 51 => "BRD", 52 => "BAT", 53 => "WOL", 54 => "WOL",
        55 => "FRG", 56 => "SNA", 57 => "WSP", 58 => "WOL", 59 => "SNA",
        60 => "FIS", 61 => "WRM", 62 => "BRD", 63 => "WOL", 64 => "WOL",
        65 => "WOL", 66 => "BEA", 67 => "WOL", 68 => "WOL", 69 => "WOL",
        70 => "BEA", 71 => "SNA", 72 => "SPI", 73 => "SNA", 74 => "WOL",
        75 => "WRM", 76 => "BRD", 77 => "WOL", 78 => "BEA", 79 => "WOL",
        80 => "WOL", 81 => "RAT", 82 => "RAT", 83 => "WOL", 84 => "WRM",
        85 => "SNA", 86 => "WOL", 87 => "BRD", 88 => "WSP", 89 => "WOL",
        90 => "BRD", 91 => "FRG", 92 => "SNA", 93 => "BEA", 94 => "SPI",
        95 => "WRM", 96 => "WOL", 97 => "WOL", 98 => "SNA", 99 => "WOL",
        100 => "WOL", 101 => "SNA", 102 => "WSP", 103 => "SPI", 104 => "WOL",
        105 => "BRD", 106 => "WRM", 107 => "SNA", 108 => "WOL", 109 => "FRG",
        110 => "SPI", 111 => "WOL", 112 => "SNA", 113 => "BEA", 114 => "WOL",
        115 => "WOL", 116 => "WOL", 117 => "WOL", 118 => "SNA", 119 => "BRD",
        120 => "BRD", 121 => "FIS", 122 => "FIS", 123 => "SNA", 124 => "WSP",
        125 => "SPI", 126 => "SPI", 127 => "WRM",
        // Unknown — default to humanoid
        _ => "HUM",
    }
}

// ── Struct sizes ───────────────────────────────────────────────────────────

pub const SIZE_SPAWN: usize = 252;       // Spawn_S
pub const SIZE_NEW_ZONE: usize = 688;    // NewZone_S
pub const SIZE_ZONE_SERVER_INFO: usize = 130; // ZoneServerInfo_S (ip[128] + port[2])
pub const SIZE_CLIENT_ZONE_ENTRY: usize = 68; // ClientZoneEntry_S
pub const SIZE_ENTER_WORLD: usize = 68;  // EnterWorld_S
pub const SIZE_LOGIN_INFO: usize = 464;  // LoginInfo_S
pub const SIZE_SPAWN_POSITION_UPDATE: usize = 30; // SpawnPositionUpdate_S
pub const SIZE_HP_UPDATE: usize = 10;   // HPUpdate_S
pub const SIZE_DEATH: usize = 32;       // Death_S
pub const SIZE_ZONE_POINT_ENTRY: usize = 24; // ZonePointEntry_S
pub const SIZE_SPAWN_APPEARANCE: usize = 8; // SpawnAppearance_S
pub const SIZE_CONSIDER: usize = 32;     // Consider_S
pub const SIZE_EXP_UPDATE: usize = 4;   // ExpUpdate_S
pub const SIZE_LEVEL_UPDATE: usize = 12; // LevelUpdate_S
pub const SIZE_MONEY_ON_CORPSE: usize = 20; // MoneyOnCorpse_S

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_spawn_position_zero() {
        let (x, y, z, heading) = extract_spawn_position(0, 0, 0, 0);
        assert_eq!(x, 0.0);
        assert_eq!(y, 0.0);
        assert_eq!(z, 0.0);
        assert_eq!(heading, 0.0);
    }

    #[test]
    fn test_extract_spawn_position_known_values() {
        // Construct bitfields for x=100.0, y=200.0, z=50.0, heading=180.0
        // x=100.0 → raw = 800 (100 * 8), placed at bits 10-28 of pos1
        // y=200.0 → raw = 1600 (200 * 8), placed at bits 0-18 of pos2
        // z=50.0  → raw = 400 (50 * 8), placed at bits 0-18 of pos3
        // heading=180.0 → raw = 256 (180 * 512 / 360), placed at bits 13-24 of pos4
        let x_raw = (100.0 * 8.0) as u32; // 800
        let y_raw = (200.0 * 8.0) as u32; // 1600
        let z_raw = (50.0 * 8.0) as u32;  // 400
        let h_raw = (180.0 * 512.0 / 360.0) as u32; // 256

        let pos1 = x_raw << 10;
        let pos2 = y_raw;
        let pos3 = z_raw;
        let pos4 = h_raw << 13;

        let (x, y, z, heading) = extract_spawn_position(pos1, pos2, pos3, pos4);
        assert!((x - 100.0).abs() < 0.125, "x={}", x);
        assert!((y - 200.0).abs() < 0.125, "y={}", y);
        assert!((z - 50.0).abs() < 0.125, "z={}", z);
        assert!((heading - 180.0).abs() < 1.0, "heading={}", heading);
    }

    #[test]
    fn test_eq_race_to_code_playable() {
        assert_eq!(eq_race_to_code(1), "HUM");
        assert_eq!(eq_race_to_code(4), "ELF");
        assert_eq!(eq_race_to_code(128), "IKS");
    }

    #[test]
    fn test_eq_race_to_code_unknown() {
        assert_eq!(eq_race_to_code(9999), "HUM");
    }

    #[test]
    fn test_safe_read_pads_short_input() {
        #[repr(C, packed)]
        #[derive(Debug, Copy, Clone)]
        struct TestStruct {
            a: u32,
            b: u16,
            c: u8,
        }
        let data = vec![0x01, 0x02]; // only 2 bytes, struct is 7
        let result: TestStruct = unsafe { safe_read(&data) };
        // Read packed fields through copies to avoid unaligned reference UB
        let a = result.a;
        let b = result.b;
        let c = result.c;
        assert_eq!(a, 0x0201); // little-endian
        assert_eq!(b, 0);
        assert_eq!(c, 0);
    }
}

// ── Packed struct definitions ──────────────────────────────────────────────
// All structs are repr(C, packed) matching EQEmu's Titanium protocol layout.

/// Core spawn struct (252 bytes). Contains bitfield-encoded position, name, level,
/// and ~100 other fields. We only parse the fields we need.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct Spawn_S {
    pub unknown0000: u8,
    pub gm: u8,
    pub unknown0003: u8,
    pub aatitle: u8,
    pub unknown0004: u8,
    pub anon: u8,
    pub face: u8,
    pub name: [u8; 64],
    pub deity: u16,
    pub unknown0073: u16,
    pub size: f32,
    pub unknown0079: u32,
    pub NPC: u8,
    pub invis: u8,
    pub haircolor: u8,
    pub curHp: u8,
    pub max_hp: u8,
    pub findable: u8,
    pub unknown0089: [u8; 5],
    // Position bitfield block: 16 bytes covering x, y, z, heading, deltas, animation
    pub bitfield_pos1: u32, // deltaHeading:10, x:19, pad:3
    pub bitfield_pos2: u32, // y:19, animation:10, pad:3
    pub bitfield_pos3: u32, // z:19, deltaY:13
    pub bitfield_pos4: u32, // deltaX:13, heading:12, pad:7
    pub bitfield_pos5: u32, // deltaZ:13, pad:19
    pub eyecolor1: u8,
    pub unknown0115: [u8; 11],
    pub StandState: u8,
    pub drakkin_heritage: u32,
    pub drakkin_tattoo: u32,
    pub drakkin_details: u32,
    pub showhelm: u8,
    pub unknown0140: [u8; 4],
    pub is_npc: u8,
    pub hairstyle: u8,
    pub beard: u8,
    pub unknown0147: [u8; 4],
    pub level: u8,
    pub PlayerState: u32,
    pub beardcolor: u8,
    pub suffix: [u8; 32],
    pub petOwnerId: u32,
    pub guildrank: u8,
    pub unknown0194: [u8; 3],
    pub equipment: [u8; 36],
    pub runspeed: f32,
    pub afk: u8,
    pub guildID: u32,
    pub title: [u8; 32],
    pub unknown0274: u8,
    pub set_to_0xFF: [u8; 8],
    pub helm: u8,
    pub race: u32,
    pub unknown0288: u32,
    pub lastName: [u8; 32],
    pub walkspeed: f32,
    pub unknown0328: u8,
    pub is_pet: u8,
    pub light: u8,
    pub class_: u8,
    pub eyecolor2: u8,
    pub flymode: u8,
    pub gender: u8,
    pub bodytype: u8,
    pub unknown0336: [u8; 3],
    pub equip_chest2: u8,
    pub spawnId: u32,
    pub bounding_radius: f32,
    pub IsMercenary: u8,
    pub equipment_tint: [u8; 36],
    pub lfg: u8,
}

impl Spawn_S {
    pub fn name_str(&self) -> String {
        // Truncate at first null byte — the field is 64 bytes but the string
        // ends at the first \0; bytes after it are uninitialised padding.
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(self.name.len());
        let slice = &self.name[..end];
        // Reject anything with non-printable or non-ASCII bytes (binary garbage).
        if slice.iter().all(|&b| b >= 0x20 && b < 0x7f) {
            String::from_utf8_lossy(slice).into_owned()
        } else {
            String::new()
        }
    }
}

/// Entity position update (30 bytes) — sent for every moving entity.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct SpawnPositionUpdate_S {
    pub spawn_id: u16,
    pub delta_heading: i16,
    pub y: f32,
    pub delta_z: f32,
    pub z: f32,
    pub delta_x: f32,
    pub x: f32,
    pub delta_y: f32,
    pub animation: u8,
    pub heading: u8, // 0-255 mapped to 0-360
}

/// HP update (10 bytes).
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct HPUpdate_S {
    pub cur_hp: u32,
    pub max_hp: i32,
    pub spawn_id: i16,
}

/// Death notification (32 bytes).
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct Death_S {
    pub spawn_id: u32,
    pub killer_id: u32,
    pub corpseid: u32,
    pub bindzoneid: u32,
    pub spell_id: u32,
    pub attack_skill: u32,
    pub damage: u32,
    pub unknown028: u32,
}

/// Zone info (688 bytes) — sent on zone entry.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct NewZone_S {
    pub char_name: [u8; 64],
    pub zone_short: [u8; 32],
    pub zone_long: [u8; 278],
    pub ztype: u8,
    pub fog_red: [u8; 4],
    pub fog_green: [u8; 4],
    pub fog_blue: [u8; 4],
    pub unknown323: u8,
    pub fog_minclip: [f32; 4],
    pub fog_maxclip: [f32; 4],
    pub gravity: f32,
    pub time_type: u8,
    pub rain_chance: [u8; 4],
    pub rain_duration: [u8; 4],
    pub snow_chance: [u8; 4],
    pub snow_duration: [u8; 4],
    pub unknown360: [u8; 33],
    pub sky: u8,
    pub unknown331: [u8; 13],
    pub zone_exp_mult: f32,
    pub safe_y: f32,
    pub safe_x: f32,
    pub safe_z: f32,
    pub max_z: f32,
    pub underworld: f32,
    pub minclip: f32,
    pub maxclip: f32,
    pub unknown_end: [u8; 84],
    pub zone_short2: [u8; 68],
    pub unknown672: [u8; 12],
    pub zone_id: u16,
    pub zone_instance: u16,
}

impl NewZone_S {
    pub fn zone_short_str(&self) -> String {
        String::from_utf8_lossy(&self.zone_short)
            .trim_end_matches('\0')
            .to_string()
    }
}

/// Zone server address (130 bytes).
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct ZoneServerInfo_S {
    pub ip: [u8; 128],
    pub port: u16,
}

/// Zone point entry (24 bytes) — zone exit info.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct ZonePointEntry_S {
    pub iterator: u32,
    pub y: f32,
    pub x: f32,
    pub z: f32,
    pub heading: f32,
    pub zoneid: u16,
    pub zoneinstance: u16,
}

/// Spawn appearance change (8 bytes).
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct SpawnAppearance_S {
    pub spawn_id: u16,
    pub type_: u16,
    pub parameter: u32,
}

/// Consider response (32 bytes) — faction/level/HP info.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct Consider_S {
    pub playerid: u32,
    pub targetid: u32,
    pub faction: u32,
    pub level: u32,
    pub cur_hp: i32,
    pub max_hp: i32,
    pub pvpcon: u8,
    pub unknown3: [u8; 3],
}

/// Experience update (4 bytes).
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct ExpUpdate_S {
    pub exp: u32,
}

/// Level update (12 bytes).
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct LevelUpdate_S {
    pub level: u32,
    pub level_old: u32,
    pub exp: u32,
}

/// Money on corpse (20 bytes).
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct MoneyOnCorpse_S {
    pub response: u8,
    pub unknown1: u8,
    pub unknown2: u8,
    pub unknown3: u8,
    pub platinum: u32,
    pub gold: u32,
    pub silver: u32,
    pub copper: u32,
}

/// Client zone entry (68 bytes) — sent when entering a zone.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct ClientZoneEntry_S {
    pub unknown00: u32,
    pub char_name: [u8; 64],
}

/// Enter world (68 bytes) — character select.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct EnterWorld_S {
    pub name: [u8; 64],
    pub tutorial: u32,
    pub return_home: u32,
}

/// Login info (464 bytes) — sent to world server.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct LoginInfo_S {
    pub login_info: [u8; 64],
    pub unknown064: [u8; 124],
    pub zoning: u8,
    pub unknown189: [u8; 275],
}
