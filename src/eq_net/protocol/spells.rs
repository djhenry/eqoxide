//! Spell-cast, memorize, and book-reading packet builders. Moved out of `navigation.rs`
//! (cleanup step 1) — pure `args -> Vec<u8>` builders with no navigation state.

use crate::eq_net::protocol::rof2_possessions_slot;

/// RoF2 `BookRequest_Struct` (fixed 8216 bytes, rof2_structs.h:2899) for OP_ReadBook — reads a
/// book/note item's text (#288). Fixed-size: the server's DECODE_LENGTH_EXACT rejects any other
/// length. Layout: window(u32)@0, invslot(TypelessInventorySlot 8B)@4, type(u32)@12,
/// target_id(u32)@16, can_cast(u8)@20, can_scribe(u8)@21, txtfile(char[8194])@22. The server keys the
/// `books` table by the FILENAME in `txtfile`, so that string is what matters; `invslot` only drives a
/// secondary type/can_scribe override. The server copies txtfile into a 20-char buffer, so a filename
/// ≥20 chars won't resolve — keep it short.
pub fn build_read_book_packet(slot: i16, target_id: u32, filename: &str) -> Vec<u8> {
    let mut buf = vec![0u8; 8216];
    buf[0..4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());   // window = new
    // invslot: TypelessInventorySlot_Struct { Slot, SubIndex, AugIndex, Unknown01 } — i16 each.
    buf[4..6].copy_from_slice(&slot.to_le_bytes());
    buf[6..8].copy_from_slice(&(-1i16).to_le_bytes());          // SubIndex = -1 (not inside a bag)
    buf[8..10].copy_from_slice(&(-1i16).to_le_bytes());         // AugIndex = -1
    buf[12..16].copy_from_slice(&1u32.to_le_bytes());           // type = 1 (Book) — echoed in the reply
    buf[16..20].copy_from_slice(&target_id.to_le_bytes());
    // txtfile @22: the item's Filename, NUL-terminated. Cap at 19 so the server's 20-char copy stays
    // NUL-terminated and resolves against the books table.
    let fb = filename.as_bytes();
    let n = fb.len().min(19);
    buf[22..22 + n].copy_from_slice(&fb[..n]);
    buf
}

/// Parse an OP_ReadBook REPLY (same 8216-byte struct). The book text is at offset 22, NUL-terminated;
/// RoF2 uses a backtick as the newline marker. Returns the readable text. (#288)
pub fn parse_read_book_reply(payload: &[u8]) -> Option<String> {
    if payload.len() < 23 { return None; }
    let body = &payload[22..];
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    Some(String::from_utf8_lossy(&body[..end]).replace('`', "\n"))
}

/// RoF2 `CastSpell_Struct` (44 bytes, rof2_structs.h): slot(u32), spell_id(u32),
/// inventory_slot(InventorySlot_Struct, 12B), target_id(u32), cs_unknown[2](u32), y/x/z_pos(f32).
/// The client targets RoF2; the old Titanium 20-byte layout failed the server's
/// DECODE_LENGTH_EXACT and every cast was silently dropped — no spell ever landed (eqoxide#42).
///
/// `slot` is the gem index 0-8 (RoF2 CastingSlot::Gem1..Gem9 == server enum, passes through). For a
/// normal memorized-gem cast the server reads only slot/spell_id/target_id and IGNORES
/// inventory_slot (that's for Item/Potion clicky casts), so inventory_slot is sent as an INVALID
/// structured slot (all -1 → RoF2ToServerSlot = SLOT_INVALID). y/x/z are the cast position, only
/// used by ground-targeted AE spells; 0 is fine for single-target casts.
pub fn build_cast_packet(slot: u32, spell_id: u32, target_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 44];
    buf[0..4].copy_from_slice(&slot.to_le_bytes());
    buf[4..8].copy_from_slice(&spell_id.to_le_bytes());
    // inventory_slot @8..20: InventorySlot_Struct all -1 (no clicky item → SLOT_INVALID server-side).
    for b in &mut buf[8..20] { *b = 0xFF; }
    buf[20..24].copy_from_slice(&target_id.to_le_bytes());
    // cs_unknown[2] @24..32 = 0; y_pos@32 / x_pos@36 / z_pos@40 = 0.0 (already zeroed).
    buf
}

/// RoF2 item "clicky" cast — activates an item's click effect (teleport ring / port potion, etc.).
/// Same 44-byte `CastSpell_Struct` as [`build_cast_packet`], but `slot` = `CastingSlot::Item` (22)
/// and `inventory_slot` carries the real possessions slot of the item (as an `InventorySlot_Struct`)
/// instead of SLOT_INVALID. `spell_id` is the item's click effect (`ClickEffectStruct.effect`).
/// Server (common/patches/rof2.cpp): `RoF2ToServerCastingSlot` maps 22→Item 1:1, `RoF2ToServerSlot`
/// decodes the slot, and `Handle_OP_CastSpell` validates the item at that slot has that click
/// effect — so both the slot value and the real inventory_slot must be correct. (eqoxide#193)
pub fn build_item_cast_packet(inventory_slot: u32, spell_id: u32, target_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 44];
    buf[0..4].copy_from_slice(&22u32.to_le_bytes());   // slot = CastingSlot::Item
    buf[4..8].copy_from_slice(&spell_id.to_le_bytes()); // item's click effect spell id
    buf[8..20].copy_from_slice(&rof2_possessions_slot(inventory_slot)); // the item's real slot
    buf[20..24].copy_from_slice(&target_id.to_le_bytes());
    // cs_unknown[2] @24..32 = 0; y/x/z_pos @32..44 = 0.0 (already zeroed).
    buf
}

/// `MemorizeSpell_Struct` (16 bytes): slot, spell_id, scribing, reduction. Identical layout under
/// Titanium and RoF2 (verified against EQEmu rof2_structs.h — no ENCODE), opcode 0x217c.
/// scribing: 0 = scribe a scroll into the spellbook at `slot`; 1 = memorize a known spell into
/// gem `slot` (0-8); 2 = un-memorize. NOTE: scribing (0) only works if the scroll is on the CURSOR
/// (the server reads `m_inv[slotCursor]`); the caller must move it there first. See eqoxide#11.
pub fn build_memorize_packet(slot: u32, spell_id: u32, scribing: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 16];
    buf[0..4].copy_from_slice(&slot.to_le_bytes());
    buf[4..8].copy_from_slice(&spell_id.to_le_bytes());
    buf[8..12].copy_from_slice(&scribing.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_book_packet_layout() {
        // #288: RoF2 BookRequest_Struct is a fixed 8216 bytes (DECODE_LENGTH_EXACT). Verify size,
        // the window/slot/type/target fields, and that the item's Filename lands at offset 22.
        let p = build_read_book_packet(23, 99, "book0001");
        assert_eq!(p.len(), 8216, "BookRequest_Struct must be exactly 8216 bytes");
        assert_eq!(u32::from_le_bytes([p[0], p[1], p[2], p[3]]), 0xFFFF_FFFF); // window = new
        assert_eq!(i16::from_le_bytes([p[4], p[5]]), 23);                       // invslot.Slot
        assert_eq!(u32::from_le_bytes([p[12], p[13], p[14], p[15]]), 1);        // type = Book
        assert_eq!(u32::from_le_bytes([p[16], p[17], p[18], p[19]]), 99);       // target_id
        assert_eq!(&p[22..30], b"book0001");                                    // txtfile = Filename
        assert_eq!(p[30], 0, "txtfile is NUL-terminated after the filename");
    }

    #[test]
    fn read_book_reply_decodes_backtick_newlines() {
        // The reply reuses the same 8216-byte struct; text starts at offset 22, NUL-terminated, and
        // RoF2 encodes newlines as a backtick. Build a synthetic reply and round-trip it.
        let mut reply = vec![0u8; 8216];
        let body = b"line one`line two";
        reply[22..22 + body.len()].copy_from_slice(body);
        let text = parse_read_book_reply(&reply).unwrap();
        assert_eq!(text, "line one\nline two");
    }

    #[test]
    fn cast_packet_layout() {
        // RoF2 CastSpell_Struct = 44 bytes (eqoxide#42). gem 1, spell 93, target 27.
        // slot@0, spell_id@4, inventory_slot@8..20 (all -1 = invalid/no-item), target_id@20,
        // cs_unknown@24..32, y/x/z@32..44 all 0. A 20-byte Titanium packet was dropped by the
        // server's DECODE_LENGTH_EXACT — that was the "no spell ever casts" bug.
        let p = build_cast_packet(1, 93, 27);
        assert_eq!(p.len(), 44, "RoF2 CastSpell_Struct is 44 bytes");
        assert_eq!(&p[0..4], &1u32.to_le_bytes(), "slot (gem)");
        assert_eq!(&p[4..8], &93u32.to_le_bytes(), "spell_id");
        assert_eq!(&p[8..20], &[0xFFu8; 12], "inventory_slot = all -1 (no clicky item)");
        assert_eq!(&p[20..24], &27u32.to_le_bytes(), "target_id");
        assert_eq!(&p[24..44], &[0u8; 20], "cs_unknown + y/x/z position = 0");
    }

    #[test]
    fn item_cast_packet_layout() {
        // eqoxide#193: item clicky cast — slot = CastingSlot::Item (22), spell = the item's click
        // effect, inventory_slot = the real possessions slot (Type=0, Slot=n, SubIndex/AugIndex=-1),
        // target@20. Activate the item at general slot 25 (spell 2512) on target 27.
        let p = build_item_cast_packet(25, 2512, 27);
        assert_eq!(p.len(), 44, "RoF2 CastSpell_Struct is 44 bytes");
        assert_eq!(&p[0..4], &22u32.to_le_bytes(), "slot = CastingSlot::Item");
        assert_eq!(&p[4..8], &2512u32.to_le_bytes(), "spell_id = item click effect");
        // inventory_slot @8..20 must equal the possessions-slot encoding for slot 25.
        assert_eq!(&p[8..20], &rof2_possessions_slot(25), "inventory_slot = real item slot");
        assert_eq!(&p[12..14], &25i16.to_le_bytes(), "…Slot field (struct @4) carries wire slot 25");
        assert_eq!(&p[20..24], &27u32.to_le_bytes(), "target_id");
        assert_eq!(&p[24..44], &[0u8; 20], "cs_unknown + y/x/z position = 0");
    }
}
