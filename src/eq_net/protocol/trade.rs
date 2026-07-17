//! NPC trade-window packet builders (quest hand-ins). Moved out of `navigation.rs`
//! (cleanup step 1) — pure `args -> Vec<u8>` builders with no navigation state.

use crate::eq_net::protocol::SLOT_TRADE_BEGIN;
use crate::eq_net::protocol::rof2_possessions_slot;
use crate::eq_net::protocol::inventory_slot_struct;

/// Encode one RoF2 `InventorySlot_Struct` (12 bytes) for a *trade-window* slot (handing an item to
/// an NPC / another player). Trade slots are NOT possessions slots: the server decodes typeTrade via
/// RoF2ToServerSlot as `server_slot = TRADE_BEGIN(3000) + Slot`, so the wire `Slot` is the 0-based
/// trade-window index (0 = the NPC's first trade slot). `server_slot` here is the absolute eqoxide
/// slot (SLOT_TRADE_BEGIN..); we subtract TRADE_BEGIN back to the index. Type = typeTrade (3) per
/// rof2_limits.h InventoryTypes; SubIndex = -1 (top-level, not a bag).
pub(crate) fn rof2_trade_slot(server_slot: u32) -> [u8; 12] {
    let index = server_slot.saturating_sub(SLOT_TRADE_BEGIN);
    inventory_slot_struct(3, index as i16, -1)
}

/// RoF2 `MoveItem_Struct` (28 bytes) for moving a *possessions* item (e.g. the cursor) INTO an NPC
/// trade-window slot — the cursor→trade step of a quest hand-in. `from_slot` is a possessions slot
/// (cursor/general); `to_trade_slot` is the absolute trade slot (SLOT_TRADE_BEGIN = first NPC slot).
/// Like [`crate::eq_net::protocol::build_move_item`], a flat 12-byte packet would fail
/// DECODE_LENGTH_EXACT and be dropped — that was the eqoxide#26 turn-in failure (the cursor→trade
/// move never reached the server). (#26)
pub fn build_move_item_to_trade(from_slot: u32, to_trade_slot: u32) -> [u8; 28] {
    let mut buf = [0u8; 28];
    buf[0..12].copy_from_slice(&rof2_possessions_slot(from_slot)); // cursor = possessions
    buf[12..24].copy_from_slice(&rof2_trade_slot(to_trade_slot));
    buf[24..28].copy_from_slice(&0u32.to_le_bytes()); // number_in_stack = 0 (whole item)
    buf
}

/// RoF2 `CancelTrade_Struct` (8 bytes) — sent C->S to abort the trade session mid-trade (OP_CancelTrade,
/// 0x354c). The server (`EQEmu/zone/client_packet.cpp:4317`) size-validates against
/// `sizeof(CancelTrade_Struct)` (8) and DROPS anything else with a LogError — so a 0-byte send is a
/// silent no-op and never ends the trade. Layout `{ fromid: u32, action: u32 }`
/// (`EQEmu/common/eq_packet_structs.h:2598`; RoF2 `rof2_structs.h:2712`). The server OVERWRITES `fromid`
/// with the trade partner's id before relaying, and never interprets `action`, so both fields are
/// effectively opaque to the server logic — only the exact 8-byte SIZE is load-bearing. We populate
/// `fromid` with our own player id (matching the OP_TradeAcceptClick convention) and `action = 0`.
pub fn build_cancel_trade(player_id: u32) -> [u8; 8] {
    let mut buf = [0u8; 8];
    buf[0..4].copy_from_slice(&player_id.to_le_bytes()); // fromid (server overwrites)
    // action = 0 (already zeroed; server ignores it)
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eq_net::protocol::SLOT_CURSOR;

    #[test]
    fn build_cancel_trade_is_eight_bytes_with_player_id() {
        // The server drops any OP_CancelTrade whose size != sizeof(CancelTrade_Struct) (8) — a 0-byte
        // send never ends the trade. Guard the exact size and the fromid field.
        let pkt = build_cancel_trade(0x1234_5678);
        assert_eq!(pkt.len(), 8, "CancelTrade_Struct must be exactly 8 bytes or the server drops it");
        assert_eq!(u32::from_le_bytes(pkt[0..4].try_into().unwrap()), 0x1234_5678, "fromid=player_id");
        assert_eq!(u32::from_le_bytes(pkt[4..8].try_into().unwrap()), 0, "action=0");
    }

    #[test]
    fn build_move_item_to_trade_encodes_typetrade_slot() {
        // Quest hand-in cursor→trade step (eqoxide#26). The NPC's first trade slot is server slot
        // SLOT_TRADE_BEGIN(3000); RoF2 decodes typeTrade as server = TRADE_BEGIN + Slot, so the wire
        // Slot must be 0. from = cursor (a possessions slot). A flat 12-byte move was dropped before.
        let pkt = build_move_item_to_trade(SLOT_CURSOR, SLOT_TRADE_BEGIN);
        assert_eq!(pkt.len(), 28);
        // from_slot: Type=typePossessions(0), Slot=cursor(33), SubIndex/AugIndex=-1
        assert_eq!(i16::from_le_bytes([pkt[0], pkt[1]]), 0, "from Type=typePossessions");
        assert_eq!(i16::from_le_bytes([pkt[4], pkt[5]]), SLOT_CURSOR as i16, "from Slot=cursor");
        assert_eq!(i16::from_le_bytes([pkt[6], pkt[7]]), -1, "from SubIndex=SLOT_INVALID");
        // to_slot (offset +12): Type=typeTrade(3), Slot=0 (3000-TRADE_BEGIN), SubIndex/AugIndex=-1
        assert_eq!(i16::from_le_bytes([pkt[12], pkt[13]]), 3, "to Type=typeTrade");
        assert_eq!(i16::from_le_bytes([pkt[16], pkt[17]]), 0, "to Slot=trade index 0");
        assert_eq!(i16::from_le_bytes([pkt[18], pkt[19]]), -1, "to SubIndex=SLOT_INVALID");
        assert_eq!(i16::from_le_bytes([pkt[20], pkt[21]]), -1, "to AugIndex=SOCKET_INVALID");
        // number_in_stack = 0 (whole-item move)
        assert_eq!(u32::from_le_bytes(pkt[24..28].try_into().unwrap()), 0, "whole-item move");
    }
}
