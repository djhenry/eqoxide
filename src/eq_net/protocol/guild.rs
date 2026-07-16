//! Guild command packet builders (invite/remove/accept). Moved out of `navigation.rs`
//! (cleanup step 1) — pure `args -> Vec<u8>` builders with no navigation state.

/// GuildCommand_Struct (RoF2, 140 bytes) — the payload for BOTH OP_GuildInvite and OP_GuildRemove
/// (#295): othername[64]@0 (target / member acted on), myname[64]@64 (sender), u16 guildeqid@128
/// (sender's guild id; server overwrites with our real GuildID if 0), u8 unknown[2]@130,
/// u32 officer@132 (the target RANK on the 0-8 scale — for a plain invite this is GUILD_RECRUIT=8;
/// for a self-leave/remove it's ignored), u32 unknown136. A self-leave is othername == myname.
pub fn build_guild_command(othername: &str, myname: &str, guild_id: u32, rank: u32) -> [u8; 140] {
    let mut buf = [0u8; 140];
    let n = othername.as_bytes().len().min(63);
    buf[0..n].copy_from_slice(&othername.as_bytes()[..n]);
    let n2 = myname.as_bytes().len().min(63);
    buf[64..64 + n2].copy_from_slice(&myname.as_bytes()[..n2]);
    buf[128..130].copy_from_slice(&(guild_id as u16).to_le_bytes());
    buf[132..136].copy_from_slice(&rank.to_le_bytes());
    buf
}

/// GuildInviteAccept_Struct (RoF2, 136 bytes) — reply to an incoming OP_GuildInvite (#295):
/// inviter[64]@0, newmember[64]@64 (us), u32 response@128 (the rank to accept at, 0-8; >=9 declines),
/// u32 guildeqid@132 (the guild being joined).
pub fn build_guild_invite_accept(inviter: &str, newmember: &str, response: u32, guild_id: u32) -> [u8; 136] {
    let mut buf = [0u8; 136];
    let n = inviter.as_bytes().len().min(63);
    buf[0..n].copy_from_slice(&inviter.as_bytes()[..n]);
    let n2 = newmember.as_bytes().len().min(63);
    buf[64..64 + n2].copy_from_slice(&newmember.as_bytes()[..n2]);
    buf[128..132].copy_from_slice(&response.to_le_bytes());
    buf[132..136].copy_from_slice(&guild_id.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guild_command_packet_layout() {
        // #295: GuildCommand_Struct is 140 bytes: othername@0, myname@64, guildeqid(u16)@128,
        // officer(u32 rank)@132. Used for invite (rank=8) and remove/leave.
        let p = build_guild_command("Target", "Me", 42, 8);
        assert_eq!(p.len(), 140);
        assert_eq!(&p[0..6], b"Target");
        assert_eq!(p[6], 0, "othername NUL-terminated");
        assert_eq!(&p[64..66], b"Me");
        assert_eq!(u16::from_le_bytes([p[128], p[129]]), 42);            // guildeqid
        assert_eq!(u32::from_le_bytes([p[132], p[133], p[134], p[135]]), 8); // officer/rank
    }

    #[test]
    fn guild_invite_accept_packet_layout() {
        // #295: GuildInviteAccept_Struct is 136 bytes: inviter@0, newmember@64, response(u32)@128,
        // guildeqid(u32)@132.
        let p = build_guild_invite_accept("Boss", "Me", 8, 42);
        assert_eq!(p.len(), 136);
        assert_eq!(&p[0..4], b"Boss");
        assert_eq!(&p[64..66], b"Me");
        assert_eq!(u32::from_le_bytes([p[128], p[129], p[130], p[131]]), 8);  // response (rank)
        assert_eq!(u32::from_le_bytes([p[132], p[133], p[134], p[135]]), 42); // guildeqid
    }
}
