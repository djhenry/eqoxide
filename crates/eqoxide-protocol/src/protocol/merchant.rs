//! Merchant window packet builders. Moved out of `navigation.rs` (cleanup step 1) — pure
//! `args -> Vec<u8>` builders with no navigation state.

/// RoF2 `MerchantClick_Struct` (24 bytes): npc_id@0, player_id@4, command@8 (1=open, 0=close),
/// rate@12, **tab_display@16** (bitmask — b001 = Purchase/Sell tab), unknown02@20 (-1 from client).
/// Titanium was 16 bytes with no tab_display; without tab_display set the RoF2 server opens the
/// window but sends NO merchant inventory, so it must be 1.
pub fn merchant_click(npc_id: u32, player_id: u32, command: u32) -> [u8; 24] {
    let mut b = [0u8; 24];
    b[0..4].copy_from_slice(&npc_id.to_le_bytes());
    b[4..8].copy_from_slice(&player_id.to_le_bytes());
    b[8..12].copy_from_slice(&command.to_le_bytes());
    b[16..20].copy_from_slice(&1i32.to_le_bytes());    // tab_display = Purchase/Sell
    b[20..24].copy_from_slice(&(-1i32).to_le_bytes());  // unknown02 = -1 (client value)
    b
}
