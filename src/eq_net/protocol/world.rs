//! Environmental damage, translocate, pet command, spawn appearance, and door-click packet
//! builders. Moved out of `navigation.rs` (cleanup step 1) — pure `args -> Vec<u8>` builders
//! (plus the pure `fall_damage` calculation) with no navigation state.

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

/// RoF2 `EnvDamage2_Struct` (39 bytes): id@0, damage(u32)@6, dmgtype(u8)@26, constant(u16)@33.
/// The RoF2 server's DECODE reads only id/damage/dmgtype (it forces `constant = 0xFFFF` itself); the
/// rest of the struct is unknown padding. The old Titanium 31-byte layout (dmgtype@22) failed the
/// server's `DECODE_LENGTH_EXACT` and was silently dropped, so a fall's local HP decrement never
/// reached the server and HP desynced (#195). dmgtype: 0xFA=Lava, 0xFB=Drowning, 0xFC=Falling,
/// 0xFD=Trap.
pub fn build_env_damage_packet(player_id: u32, damage: u32, dmgtype: u8) -> Vec<u8> {
    let mut buf = vec![0u8; 39];
    buf[0..4].copy_from_slice(&player_id.to_le_bytes());
    buf[6..10].copy_from_slice(&damage.to_le_bytes());
    buf[26] = dmgtype;
    buf[33..35].copy_from_slice(&0xFFFFu16.to_le_bytes());
    buf
}

/// Accept a translocate offer (#192). The server sends `OP_Translocate` with a `Translocate_Struct`
/// (92 bytes: ZoneID@0, SpellID@4, Caster[64]@12, y@76, x@80, z@84, Complete@88) as a "do you accept?"
/// prompt; the client accepts by echoing the SAME struct back with `Complete@88 = 1`. The RoF2 wire
/// struct isn't transformed, so we just copy the prompt and flip that field. Returns the 92-byte ack.
pub fn build_translocate_ack(prompt: &[u8]) -> Vec<u8> {
    let mut ack = vec![0u8; 92];
    let n = prompt.len().min(92);
    ack[..n].copy_from_slice(&prompt[..n]);
    ack[88..92].copy_from_slice(&1u32.to_le_bytes()); // Complete = 1 → accept
    ack
}

/// Titanium `PetCommand_Struct` (8 bytes): command(u32), target(u32). e.g. PET_ATTACK + a mob
/// spawn id sends the player's pet to attack it.
pub fn build_pet_command(command: u32, target: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 8];
    buf[0..4].copy_from_slice(&command.to_le_bytes());
    buf[4..8].copy_from_slice(&target.to_le_bytes());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_damage_packet_is_rof2_39_byte_layout() {
        // RoF2 EnvDamage2_Struct: 39 bytes with dmgtype@26, constant@33 — the server's
        // DECODE_LENGTH_EXACT drops any other size (Titanium's 31 → silent HP desync, #195).
        let buf = build_env_damage_packet(0x1234_5678, 250, 0xFC /* falling */);
        assert_eq!(buf.len(), 39, "must be the RoF2 39-byte size");
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 0x1234_5678, "id@0");
        assert_eq!(u32::from_le_bytes(buf[6..10].try_into().unwrap()), 250, "damage@6");
        assert_eq!(buf[26], 0xFC, "dmgtype@26 (falling)");
        assert_eq!(u16::from_le_bytes(buf[33..35].try_into().unwrap()), 0xFFFF, "constant@33");
    }

    #[test]
    fn translocate_ack_echoes_prompt_with_complete_set() {
        // A 92-byte prompt: ZoneID=30@0, SpellID=1234@4, coords, Complete=0@88.
        let mut prompt = vec![0u8; 92];
        prompt[0..4].copy_from_slice(&30u32.to_le_bytes());
        prompt[4..8].copy_from_slice(&1234u32.to_le_bytes());
        prompt[80..84].copy_from_slice(&(-76.0f32).to_le_bytes()); // x
        let ack = build_translocate_ack(&prompt);
        assert_eq!(ack.len(), 92, "ack is the 92-byte Translocate_Struct");
        assert_eq!(&ack[0..4], &prompt[0..4], "ZoneID echoed");
        assert_eq!(&ack[4..8], &prompt[4..8], "SpellID echoed");
        assert_eq!(&ack[80..84], &prompt[80..84], "dest x echoed");
        assert_eq!(u32::from_le_bytes(ack[88..92].try_into().unwrap()), 1, "Complete=1 (accept)");
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
