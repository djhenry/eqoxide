//! Group management packet builders (invite/follow/disband/make-leader). Moved out of
//! `navigation.rs` (cleanup step 1) — pure `args -> Vec<u8>` builders with no navigation state.

/// OP_GroupInvite payload: GroupInvite_Struct (148 bytes): invitee_name[64], inviter_name[64],
/// then 5 unknown/zero-filled u32s.
pub fn build_group_invite(invitee_name: &str, inviter_name: &str) -> [u8; 148] {
    let mut buf = [0u8; 148];
    let n = invitee_name.as_bytes().len().min(63);
    buf[0..n].copy_from_slice(&invitee_name.as_bytes()[..n]);
    let n2 = inviter_name.as_bytes().len().min(63);
    buf[64..64 + n2].copy_from_slice(&inviter_name.as_bytes()[..n2]);
    buf
}

/// OP_GroupFollow payload (accepting an invite): GroupFollow_Struct (152 bytes): name1=inviter[64],
/// name2=invitee(us)[64], then 6 unknown/zero-filled u32s.
pub fn build_group_follow(inviter_name: &str, invitee_name: &str) -> [u8; 152] {
    let mut buf = [0u8; 152];
    let n = inviter_name.as_bytes().len().min(63);
    buf[0..n].copy_from_slice(&inviter_name.as_bytes()[..n]);
    let n2 = invitee_name.as_bytes().len().min(63);
    buf[64..64 + n2].copy_from_slice(&invitee_name.as_bytes()[..n2]);
    buf
}

/// OP_GroupDisband payload (leave/kick/decline-cleanup). CONFIRMED LIVE (2026-07-01, task-6
/// validation pass) against a running EQEmu RoF2 zone server: the doc's inferred 128-byte
/// "common" GroupGeneric_Struct is WRONG for this opcode — the server logged
/// `Wrong size on incoming [OP_GroupDisband] (structs::GroupGeneric_Struct): Got [128], expected
/// [148]` and silently dropped the packet (no roster change, no disband on either side). The
/// server actually wants the 148-byte RoF2-namespaced struct (same shape as GroupInvite_Struct):
/// name1[64], name2[64], then 5 trailing zero uint32s. `own_name` is the acting player's own
/// name; `target_name` is who's being removed (self for leave/decline, the kicked member's name
/// for a kick).
pub fn build_group_disband(own_name: &str, target_name: &str) -> [u8; 148] {
    let mut buf = [0u8; 148];
    let n = own_name.as_bytes().len().min(63);
    buf[0..n].copy_from_slice(&own_name.as_bytes()[..n]);
    let n2 = target_name.as_bytes().len().min(63);
    buf[64..64 + n2].copy_from_slice(&target_name.as_bytes()[..n2]);
    buf
}

/// OP_GroupMakeLeader payload: GroupMakeLeader_Struct (456 bytes): Unknown000(u32)=0,
/// CurrentLeader[64], NewLeader[64], Unknown072[324]=0. Only NewLeader is read server-side.
pub fn build_group_make_leader(current_leader: &str, new_leader: &str) -> [u8; 456] {
    let mut buf = [0u8; 456];
    let n = current_leader.as_bytes().len().min(63);
    buf[4..4 + n].copy_from_slice(&current_leader.as_bytes()[..n]);
    let n2 = new_leader.as_bytes().len().min(63);
    buf[68..68 + n2].copy_from_slice(&new_leader.as_bytes()[..n2]);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_group_invite_layout() {
        let b = build_group_invite("Sariel", "Aldric");
        assert_eq!(b.len(), 148);
        assert_eq!(&b[0..6], b"Sariel");
        assert_eq!(b[6], 0); // NUL after the name within the 64-byte field
        assert_eq!(&b[64..70], b"Aldric");
    }

    #[test]
    fn build_group_follow_layout() {
        let b = build_group_follow("Aldric", "Sariel");
        assert_eq!(b.len(), 152);
        assert_eq!(&b[0..6], b"Aldric");
        assert_eq!(&b[64..70], b"Sariel");
    }

    #[test]
    fn build_group_disband_layout_is_148_bytes_confirmed_live() {
        // CONFIRMED against a running EQEmu RoF2 zone server (task-6 live validation, 2026-07-01):
        // the doc's inferred 128-byte COMMON GroupGeneric_Struct was wrong for this build — the
        // server rejected it ("Wrong size on incoming [OP_GroupDisband] ... Got [128], expected
        // [148]") and silently dropped leave/kick/decline packets. It wants the 148-byte
        // RoF2-namespaced struct (name1[64], name2[64], 5 trailing zero uint32s), like GroupInvite.
        let b = build_group_disband("Aldric", "Sariel");
        assert_eq!(b.len(), 148);
        assert_eq!(&b[0..6], b"Aldric");
        assert_eq!(&b[64..70], b"Sariel");
        assert!(b[128..148].iter().all(|&x| x == 0), "trailing 20 bytes (5 u32s) must be zero-filled");
    }

    #[test]
    fn build_group_make_leader_layout() {
        let b = build_group_make_leader("Aldric", "Sariel");
        assert_eq!(b.len(), 456);
        assert_eq!(&b[0..4], &0u32.to_le_bytes()); // Unknown000
        assert_eq!(&b[4..10], b"Aldric");           // CurrentLeader @4
        assert_eq!(&b[68..74], b"Sariel");          // NewLeader @68
    }
}
