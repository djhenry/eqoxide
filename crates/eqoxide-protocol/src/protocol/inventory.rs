//! Inventory move / possessions-slot packet builders. Moved out of `navigation.rs`
//! (cleanup step 1) — pure `args -> Vec<u8>` builders with no navigation state.

/// Encode one RoF2 `InventorySlot_Struct` (12 bytes): {Type(i16), Unknown02, Slot(i16),
/// SubIndex(i16), AugIndex(i16), Unknown01} — the record every `rof2_*_slot` builder in this
/// crate emits (possessions, bag-content, and trade slots all share this 12-byte shape; they
/// differ only in `type_`/`slot`/`sub_index`). AugIndex is always SOCKET_INVALID (-1) here: none
/// of this crate's callers target an augment sub-slot. AugIndex MUST be in [-1, 6) or the server
/// rejects the whole slot as SLOT_INVALID (common/patches/rof2.cpp, RoF2ToServerSlot).
pub(crate) fn inventory_slot_struct(type_: i16, slot: i16, sub_index: i16) -> [u8; 12] {
    let mut s = [0u8; 12];
    s[0..2].copy_from_slice(&type_.to_le_bytes());     // Type
    s[2..4].copy_from_slice(&0i16.to_le_bytes());       // Unknown02
    s[4..6].copy_from_slice(&slot.to_le_bytes());       // Slot
    s[6..8].copy_from_slice(&sub_index.to_le_bytes());  // SubIndex
    s[8..10].copy_from_slice(&(-1i16).to_le_bytes());   // AugIndex = SOCKET_INVALID
    s[10..12].copy_from_slice(&0i16.to_le_bytes());     // Unknown01
    s
}

/// Encode one RoF2 `InventorySlot_Struct` (12 bytes) for a flat *possessions* slot — equipment
/// 0-22, general inventory 23-32, cursor 33. RoF2 does NOT send a bare slot int; it sends a
/// structured record (see [`inventory_slot_struct`]) which the server decodes via
/// RoF2ToServerSlot (common/patches/rof2.cpp). For a top-level possessions slot: Type =
/// typePossessions (0), Slot = the flat slot, SubIndex = SLOT_INVALID (-1). (Bank/trade/world
/// slots use other Type values + offsets; not handled here.)
pub(crate) fn rof2_possessions_slot(slot: u32) -> [u8; 12] {
    inventory_slot_struct(0, slot as i16, -1)
}

/// Encode a RoF2 `InventorySlot_Struct` for any possessions OR bag-content flat slot. Top-level
/// slots (equipment/general/cursor, < 251) → [`rof2_possessions_slot`] (SubIndex = -1). A general-
/// bag content flat slot (251-350) → the parent general slot with `SubIndex` = the 0-9 bag index,
/// which the server decodes to the bagged item (`RoF2ToServerSlot`, common/patches/rof2.cpp:7080:
/// `GENERAL_BAGS_BEGIN + (Slot-GENERAL_BEGIN)*SLOT_COUNT + SubIndex`). This is what makes bagged
/// items movable. (eqoxide#201)
pub(crate) fn rof2_inventory_slot(flat: u32) -> [u8; 12] {
    let Some((parent, sub_index)) = eqoxide_core::game_state::bag_wire_parent(flat as i32) else {
        return rof2_possessions_slot(flat);
    };
    inventory_slot_struct(0, parent as i16, sub_index as i16)
}

/// RoF2 `MoveItem_Struct` (28 bytes): from_slot(InventorySlot_Struct,12) + to_slot(…,12) +
/// number_in_stack(u32). NOTE: unlike Titanium's 3×u32 flat struct, RoF2 slots are *structured*
/// (see [`rof2_possessions_slot`]); a flat 12-byte packet fails the server's DECODE_LENGTH_EXACT and
/// the move is silently dropped — that was the real eqoxide#11 scribe failure (the scroll never
/// reached the cursor, so OP_MemorizeSpell scribing=0 saw an empty cursor). number_in_stack = 0 for
/// a whole-item move (equip/cursor/rearrange); a count would split a stack. Handles top-level and
/// general-bag content slots (see [`rof2_inventory_slot`]).
pub fn build_move_item(from_slot: u32, to_slot: u32) -> [u8; 28] {
    let mut buf = [0u8; 28];
    buf[0..12].copy_from_slice(&rof2_inventory_slot(from_slot));
    buf[12..24].copy_from_slice(&rof2_inventory_slot(to_slot));
    buf[24..28].copy_from_slice(&0u32.to_le_bytes()); // number_in_stack = 0 (whole item)
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SLOT_CURSOR;

    #[test]
    fn move_item_is_rof2_28byte_structured_slots() {
        // RoF2 MoveItem_Struct = from_slot(InventorySlot_Struct,12) + to_slot(…,12) +
        // number_in_stack(u32) = 28 bytes. Each slot is structured {Type, Unk02, Slot, SubIndex,
        // AugIndex, Unk01}, NOT a bare int — the server's RoF2ToServerSlot reads these fields and a
        // flat 12-byte packet fails DECODE_LENGTH_EXACT (silently dropped → the eqoxide#11 scribe
        // failure: the scroll never reached the cursor). Used by the scribe flow to move a scroll
        // from general slot 23 → cursor (33) before OP_MemorizeSpell.
        let pkt = build_move_item(23, SLOT_CURSOR);
        assert_eq!(pkt.len(), 28);
        // from_slot: Type=typePossessions(0), Slot=23, SubIndex/AugIndex=SLOT_INVALID(-1)
        assert_eq!(i16::from_le_bytes([pkt[0], pkt[1]]), 0, "from Type=typePossessions");
        assert_eq!(i16::from_le_bytes([pkt[4], pkt[5]]), 23, "from Slot");
        assert_eq!(i16::from_le_bytes([pkt[6], pkt[7]]), -1, "from SubIndex=SLOT_INVALID");
        assert_eq!(i16::from_le_bytes([pkt[8], pkt[9]]), -1, "from AugIndex=SOCKET_INVALID");
        // to_slot (offset +12): Type=typePossessions(0), Slot=cursor(33)
        assert_eq!(i16::from_le_bytes([pkt[12], pkt[13]]), 0, "to Type=typePossessions");
        assert_eq!(i16::from_le_bytes([pkt[16], pkt[17]]), SLOT_CURSOR as i16, "to Slot=cursor");
        assert_eq!(i16::from_le_bytes([pkt[18], pkt[19]]), -1, "to SubIndex=SLOT_INVALID");
        // number_in_stack = 0 (whole-item move; a count would split a stack)
        assert_eq!(u32::from_le_bytes(pkt[24..28].try_into().unwrap()), 0, "whole-item move");
    }

    #[test]
    fn build_move_item_encodes_bag_content_subindex() {
        // eqoxide#201: moving a bagged item OUT to the cursor. Flat slot 263 = general bag at slot
        // 24 (parent wire 24), sub-index 2 (263 = 251 + (24-23)*10 + 2). The server decodes a
        // possessions slot with SubIndex set to the bagged item (RoF2ToServerSlot, rof2.cpp:7080),
        // so the from_slot must carry Slot=24, SubIndex=2 (NOT SubIndex=-1 like a top-level slot).
        let pkt = build_move_item(263, SLOT_CURSOR);
        assert_eq!(pkt.len(), 28);
        assert_eq!(i16::from_le_bytes([pkt[0], pkt[1]]), 0, "from Type=typePossessions");
        assert_eq!(i16::from_le_bytes([pkt[4], pkt[5]]), 24, "from Slot=parent general slot 24");
        assert_eq!(i16::from_le_bytes([pkt[6], pkt[7]]), 2, "from SubIndex=bag index 2");
        assert_eq!(i16::from_le_bytes([pkt[8], pkt[9]]), -1, "from AugIndex=SOCKET_INVALID");
        // to = cursor: a top-level possessions slot (SubIndex=-1).
        assert_eq!(i16::from_le_bytes([pkt[16], pkt[17]]), SLOT_CURSOR as i16, "to Slot=cursor");
        assert_eq!(i16::from_le_bytes([pkt[18], pkt[19]]), -1, "to SubIndex=SLOT_INVALID");
    }
}
