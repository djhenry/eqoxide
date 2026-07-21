//! GM trainer packet builders (open/train/close). Moved out of `navigation.rs` (cleanup
//! step 1) — pure `args -> Vec<u8>` builders with no navigation state.

/// OP_GMTraining open request (GMTrainee_Struct, 448 bytes): npcid@0, playerid@4, skills[100]@8
/// (sent as zeros — the server fills them with the offered CAPS in its reply), unknown[40]@408.
pub fn build_gm_training(npcid: u32, playerid: u32) -> Vec<u8> {
    let mut b = vec![0u8; 448];
    b[0..4].copy_from_slice(&npcid.to_le_bytes());
    b[4..8].copy_from_slice(&playerid.to_le_bytes());
    b
}

/// OP_GMTrainSkill (GMSkillChange_Struct, 12 bytes): npcid u16@0, skillbank u16@4 (0 = normal
/// skills, not languages), skill_id u16@8. Trains one point of `skill_id` at the given trainer.
pub fn build_gm_train_skill(npcid: u32, skill_id: u32) -> Vec<u8> {
    let mut b = vec![0u8; 12];
    b[0..2].copy_from_slice(&(npcid as u16).to_le_bytes());
    b[8..10].copy_from_slice(&(skill_id as u16).to_le_bytes());
    b
}

/// OP_GMEndTraining (GMTrainEnd_Struct, 8 bytes): npcid@0, playerid@4. Closes the training window.
pub fn build_gm_end_training(npcid: u32, playerid: u32) -> Vec<u8> {
    let mut b = vec![0u8; 8];
    b[0..4].copy_from_slice(&npcid.to_le_bytes());
    b[4..8].copy_from_slice(&playerid.to_le_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_gm_training_layout() {
        // GMTrainee_Struct: npcid@0, playerid@4, skills[100]@8 (zero on send), 448 bytes total.
        let b = build_gm_training(0x1122, 0x3344);
        assert_eq!(b.len(), 448);
        assert_eq!(&b[0..4], &0x1122u32.to_le_bytes());
        assert_eq!(&b[4..8], &0x3344u32.to_le_bytes());
        assert!(b[8..].iter().all(|&x| x == 0), "skills[] + trailing sent as zero");
    }

    #[test]
    fn build_gm_train_skill_layout() {
        // GMSkillChange_Struct (12 bytes): npcid u16@0, skillbank u16@4 (0), skill_id u16@8.
        let b = build_gm_train_skill(0x1122, 7 /* Archery */);
        assert_eq!(b.len(), 12);
        assert_eq!(&b[0..2], &0x1122u16.to_le_bytes(), "npcid @0");
        assert_eq!(&b[4..6], &0u16.to_le_bytes(), "skillbank @4 = normal skills");
        assert_eq!(&b[8..10], &7u16.to_le_bytes(), "skill_id @8");
    }

    #[test]
    fn build_gm_end_training_layout() {
        let b = build_gm_end_training(0x1122, 0x3344);
        assert_eq!(b.len(), 8);
        assert_eq!(&b[0..4], &0x1122u32.to_le_bytes());
        assert_eq!(&b[4..8], &0x3344u32.to_le_bytes());
    }
}
