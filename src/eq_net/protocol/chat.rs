//! Chat/say packet builders. Moved out of `navigation.rs` (cleanup step 1) — pure
//! `args -> Vec<u8>` builders with no navigation state.

/// Build a RoF2 `OP_ChannelMessage` for the Say channel (used for NPC hails).
/// chan_num 8 = ChatChannel_Say; the server delivers say text to NPCs within 200
/// units, triggering EVENT_SAY (a "Hail, <name>" message fires the NPC's hail script).
pub fn build_say_packet(sender: &str, target: &str, message: &str) -> Vec<u8> {
    build_channel_message(sender, target, 8, message) // chan_num 8 = ChatChannel_Say
}

/// Build an `OP_ChannelMessage` for an arbitrary chat channel. `target` is the recipient
/// for directed channels (tell), empty for broadcasts (ooc/shout/group). EQEmu ChatChannel:
/// 2 group, 3 shout, 5 OOC, 7 tell, 8 say.
///
/// RoF2 uses a **variable-length, NUL-terminated** wire format — NOT the fixed Titanium
/// `ChannelMessage_Struct`. See EQEmu `common/patches/rof2.cpp` `DECODE(OP_ChannelMessage)`:
///   sender\0 | target\0 | u32 unknown | u32 language | u32 chan_num
///   | u32 unknown | u8 unknown | u32 skill_in_language | message\0
/// Sending the fixed 64-byte-field struct makes the server read an empty target + garbage
/// chan_num, so tells/OOC are silently dropped (no cross-zone routing).
pub fn build_channel_message(sender: &str, target: &str, chan_num: u32, message: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(sender.len() + target.len() + message.len() + 24);
    buf.extend_from_slice(sender.as_bytes()); buf.push(0);
    buf.extend_from_slice(target.as_bytes()); buf.push(0);
    buf.extend_from_slice(&0u32.to_le_bytes());      // unknown
    buf.extend_from_slice(&0u32.to_le_bytes());      // language = CommonTongue
    buf.extend_from_slice(&chan_num.to_le_bytes());  // chan_num
    buf.extend_from_slice(&0u32.to_le_bytes());      // unknown
    buf.push(0);                                     // unknown (u8)
    buf.extend_from_slice(&100u32.to_le_bytes());    // skill_in_language
    buf.extend_from_slice(message.as_bytes()); buf.push(0);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_say_packet_matches_rof2_layout() {
        // RoF2 wire: sender\0 target\0 u32 unk | u32 lang | u32 chan | u32 unk | u8 unk |
        //            u32 skill | message\0   (see rof2.cpp DECODE(OP_ChannelMessage))
        let p = build_say_packet("Aiquestbot", "Guard Phaeton", "Hail, Guard Phaeton");
        let mut o = 0;
        assert_eq!(&p[o..o + 10], b"Aiquestbot"); o += 10;
        assert_eq!(p[o], 0, "sender NUL-terminated"); o += 1;
        assert_eq!(&p[o..o + 13], b"Guard Phaeton"); o += 13;
        assert_eq!(p[o], 0, "target NUL-terminated"); o += 1;
        assert_eq!(u32::from_le_bytes([p[o], p[o+1], p[o+2], p[o+3]]), 0, "unknown"); o += 4;
        assert_eq!(u32::from_le_bytes([p[o], p[o+1], p[o+2], p[o+3]]), 0, "language=CommonTongue"); o += 4;
        assert_eq!(u32::from_le_bytes([p[o], p[o+1], p[o+2], p[o+3]]), 8, "chan_num=Say"); o += 4;
        o += 4;            // unknown u32
        o += 1;            // unknown u8
        o += 4;            // skill_in_language
        let msg_end = o + "Hail, Guard Phaeton".len();
        assert_eq!(&p[o..msg_end], b"Hail, Guard Phaeton");
        assert_eq!(p[msg_end], 0, "message must be null-terminated");
        assert_eq!(p.len(), msg_end + 1);
    }

    #[test]
    fn build_say_packet_names_are_nul_terminated() {
        // RoF2 names are variable-length cstrings (no fixed 64-byte field). Verify both the
        // sender and target are emitted whole and each terminated by a single NUL.
        let p = build_say_packet("Aiquestbot", "Guard Phaeton", "hi");
        assert_eq!(p[10], 0, "sender NUL-terminated after 'Aiquestbot'");
        assert_eq!(p[11 + 13], 0, "target NUL-terminated after 'Guard Phaeton'");
    }
}
