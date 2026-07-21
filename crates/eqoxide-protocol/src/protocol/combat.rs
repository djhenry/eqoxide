//! Targeting/consider packet builders and auto-combat target selection. Moved out of
//! `navigation.rs` (cleanup step 1) — pure `args -> Vec<u8>` builders with no navigation state.

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
