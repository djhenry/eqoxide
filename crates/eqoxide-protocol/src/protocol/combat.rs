//! Targeting/consider packet builders. Moved out of `navigation.rs` (cleanup step 1) — pure
//! `args -> Vec<u8>` builders with no navigation state.
//!
//! This module also held `pick_combat_target`, the auto-combat target-priority function, until
//! #1109 removed the client-side auto-retarget it served. Choosing whom to fight is a strategy
//! decision that belongs to the agent driving the client, not to the client.

/// OP_TargetCommand payload: ClientTarget_Struct = just the target spawn id (u32).
pub fn build_target_packet(spawn_id: u32) -> Vec<u8> {
    spawn_id.to_le_bytes().to_vec()
}

/// OP_Consider payload: RoF2 `Consider_Struct` (**20 bytes**). The client fills playerid+targetid;
/// the server replies with the same opcode carrying faction (con standing) + level
/// (con color). Size must be exactly 20 or EQEmu's `DECODE_LENGTH_EXACT` rejects it — see the body
/// for why a rejected consider is invisible rather than loud. This header said 28 (Titanium's size)
/// long after the body was fixed to 20; the authoritative figure is the `vec![0u8; 20]` below,
/// pinned by `build_consider_packet_layout`.
pub fn build_consider_packet(player_id: u32, target_id: u32) -> Vec<u8> {
    // RoF2 Consider_Struct is 20 bytes (rof2_structs.h): playerid(u32)@0, targetid(u32)@4,
    // faction(u32)@8, level(u32)@12, pvpcon(u8)@16, pad[3]. (RoF2 dropped Titanium's cur_hp/max_hp,
    // so it's 20 not 28.) The old 28-byte send failed the server's DECODE_LENGTH_EXACT, so the
    // consider was silently dropped and no OP_Consider reply ever came back — con returned nothing
    // (#273). Only playerid/targetid are read by the server; the rest are zero.
    let mut buf = vec![0u8; 20];
    buf[0..4].copy_from_slice(&player_id.to_le_bytes());
    buf[4..8].copy_from_slice(&target_id.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_target_packet_is_spawn_id_le() {
        assert_eq!(build_target_packet(0x12345678), vec![0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn build_consider_packet_layout() {
        // #273: RoF2 Consider_Struct is 20 bytes (playerid, targetid, faction, level, pvpcon+pad).
        // The earlier 28-byte size (Titanium, with cur_hp/max_hp) failed the server's
        // DECODE_LENGTH_EXACT, so the consider was dropped and no OP_Consider reply came back.
        let p = build_consider_packet(7, 42);
        assert_eq!(p.len(), 20, "RoF2 Consider_Struct must be exactly 20 bytes");
        assert_eq!(u32::from_le_bytes([p[0], p[1], p[2], p[3]]), 7);
        assert_eq!(u32::from_le_bytes([p[4], p[5], p[6], p[7]]), 42);
    }
}
