//! RoF2 binary item deserialization (OP_ItemPacket merchant lists + single items).
//!
//! RoF2 replaced Titanium's pipe-delimited *text* item format with a packed *binary* blob.
//! This mirrors EQEmu `rof2.cpp` `SerializeItem` (line 6441), which writes, in order:
//!   1. `ItemSerializationHeader`            — 77 bytes (rof2_structs.h)
//!   2. `EvolvingItem_Struct`                — 25 bytes, ONLY if header.isEvolving > 0
//!   3. two ornamentation C-strings          — main-hand + off-hand idfile ("" → just a NUL)
//!   4. `ItemSerializationHeaderFinish`      — 26 bytes
//!   5. Name, Lore, IDFile C-strings + 1 NUL — variable length, NUL-terminated, inline
//!   6. `ItemBodyStruct`                     — id@0, … icon@20 (then many more stats)
//!
//! We extract only the fields the client renders (merchant window + inventory). Strings are
//! variable-length, so fields cannot be read at fixed offsets — we walk the blob in write order.

/// The subset of a RoF2-serialized item the client uses.
pub struct RoF2Item {
    pub slot_type:      u8,
    pub main_slot:      u16,
    pub price:          u32,
    /// Merchant stock count (header.merchant_slot for merchant items).
    pub merchant_count: u32,
    pub charges:        u32,
    pub id:             u32,
    pub icon:           u32,
    pub name:           String,
    pub idfile:         String,
}

const HDR_LEN:      usize = 77; // sizeof(ItemSerializationHeader)
const EVOLVING_LEN: usize = 25; // sizeof(EvolvingItem_Struct)
const FINISH_LEN:   usize = 26; // sizeof(ItemSerializationHeaderFinish)

/// Read a NUL-terminated string starting at `off`; returns (string, offset just past the NUL).
fn read_cstr(buf: &[u8], off: usize) -> Option<(String, usize)> {
    let rel = buf.get(off..)?.iter().position(|&b| b == 0)?;
    let end = off + rel;
    Some((String::from_utf8_lossy(&buf[off..end]).into_owned(), end + 1))
}

/// Deserialize one RoF2-serialized item — the bytes AFTER the OP_ItemPacket 4-byte type header.
pub fn parse_rof2_item(buf: &[u8]) -> Option<RoF2Item> {
    if buf.len() < HDR_LEN { return None; }
    let u16a = |o: usize| u16::from_le_bytes([buf[o], buf[o + 1]]);
    let u32a = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);

    let slot_type      = buf[25];
    let main_slot      = u16a(26);
    let price          = u32a(32);
    let merchant_count = u32a(36);
    let charges        = u32a(56);
    let is_evolving    = buf[76];

    let mut off = HDR_LEN;
    if is_evolving > 0 { off += EVOLVING_LEN; }
    // two ornamentation C-strings (absent ornament → an empty string / single NUL each)
    off = read_cstr(buf, off)?.1;
    off = read_cstr(buf, off)?.1;
    // ItemSerializationHeaderFinish (fixed 26 bytes)
    off = off.checked_add(FINISH_LEN)?;
    // Name, Lore, IDFile, then one trailing NUL before the body
    let (name, o)   = read_cstr(buf, off)?; off = o;
    let (_lore, o)  = read_cstr(buf, off)?; off = o;
    let (idfile, o) = read_cstr(buf, off)?; off = o;
    off = off.checked_add(1)?;
    // ItemBodyStruct: id@0 (u32), icon@20 (u32)
    if off + 24 > buf.len() { return None; }
    let id   = u32a(off);
    let icon = u32a(off + 20);

    Some(RoF2Item { slot_type, main_slot, price, merchant_count, charges, id, icon, name, idfile })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal non-evolving serialized item: a "Cloth Cap" (id 1001, icon 678) at
    /// merchant slot 3, price 100, stock 5.
    fn fixture() -> Vec<u8> {
        let mut b = vec![0u8; HDR_LEN];
        b[17..21].copy_from_slice(&1u32.to_le_bytes());   // stacksize
        b[25] = 9;                                         // slot_type = merchant
        b[26..28].copy_from_slice(&3u16.to_le_bytes());    // main_slot = merchant slot
        b[32..36].copy_from_slice(&100u32.to_le_bytes());  // price
        b[36..40].copy_from_slice(&5u32.to_le_bytes());    // merchant_slot = stock count
        b[76] = 0;                                          // isEvolving = false
        b.push(0); // ornament main-hand (empty)
        b.push(0); // ornament off-hand (empty)
        b.extend_from_slice(&[0u8; FINISH_LEN]);           // ItemSerializationHeaderFinish
        b.extend_from_slice(b"Cloth Cap\0");               // Name
        b.push(0);                                          // Lore (empty)
        b.extend_from_slice(b"IT63\0");                    // IDFile
        b.push(0);                                          // trailing NUL before body
        let mut body = vec![0u8; 24];
        body[0..4].copy_from_slice(&1001u32.to_le_bytes()); // id
        body[20..24].copy_from_slice(&678u32.to_le_bytes());// icon
        b.extend_from_slice(&body);
        b
    }

    #[test]
    fn parses_merchant_item_fields() {
        let it = parse_rof2_item(&fixture()).expect("parse");
        assert_eq!(it.slot_type, 9);
        assert_eq!(it.main_slot, 3);
        assert_eq!(it.price, 100);
        assert_eq!(it.merchant_count, 5);
        assert_eq!(it.id, 1001);
        assert_eq!(it.icon, 678);
        assert_eq!(it.name, "Cloth Cap");
        assert_eq!(it.idfile, "IT63");
    }

    #[test]
    fn rejects_too_short() {
        assert!(parse_rof2_item(&[0u8; 10]).is_none());
    }

    #[test]
    fn skips_evolving_block() {
        // Same item but flagged evolving: a 25-byte block sits between header and ornament strings.
        let mut b = fixture();
        b[76] = 1; // isEvolving
        // splice 25 zero bytes after the 77-byte header
        let tail = b.split_off(HDR_LEN);
        b.extend_from_slice(&[0u8; EVOLVING_LEN]);
        b.extend_from_slice(&tail);
        let it = parse_rof2_item(&b).expect("parse evolving");
        assert_eq!(it.id, 1001);
        assert_eq!(it.name, "Cloth Cap");
    }
}
