//! RoF2 binary item deserialization (OP_ItemPacket merchant lists + OP_CharInventory).
//!
//! RoF2 replaced Titanium's pipe-delimited *text* item format with a packed *binary* blob.
//! This mirrors EQEmu `rof2.cpp` `SerializeItem` (line 6441) in full write order:
//!
//!  A. `ItemSerializationHeader`         — 77 bytes  (rof2_structs.h:4733)
//!  B. `EvolvingItem_Struct`             — 25 bytes  ONLY if header.isEvolving > 0 (rof2.cpp:6500)
//!  C. two ornamentation C-strings       — variable  ("IT%d\0" × 2 or "\0\0")
//!  D. `ItemSerializationHeaderFinish`   — 26 bytes  (rof2_structs.h:4765, rof2.cpp:6550)
//!  E. Name / Lore / IDFile C-strings + 1 extra NUL (rof2.cpp:6552–6565)
//!     • Name: skipped entirely if empty (no NUL); Lore/IDFile always write at least a NUL.
//!  F. `ItemBodyStruct`                  — 255 bytes (rof2_structs.h:4777, rof2.cpp:6656)
//!  G. CharmFile C-string               — variable  (at least a NUL)
//!  H. `ItemSecondaryBodyStruct`         — 74 bytes  (rof2_structs.h:4872)
//!     • contains 6 × `AugSlotStruct` (6 bytes each) — augments are NOT recursive, just inline
//!  I. Filename C-string                — variable
//!  J. `ItemTertiaryBodyStruct`          — 76 bytes  (rof2_structs.h:4898)
//!  K. 6 effect blocks (rof2.cpp:6738–6848), each = fixed struct + effect-name C-string + int32(0)
//!     1. ClickEffectStruct  (30 B) + ClickName  C-str + i32  (rof2.cpp:6759)
//!     2. ProcEffectStruct   (30 B) + ProcName   C-str + i32  (rof2.cpp:6776)
//!     3. WornEffectStruct   (30 B) + WornName   C-str + i32  (rof2.cpp:6792)
//!     4. WornEffectStruct   (30 B) + FocusName  C-str + i32  (rof2.cpp:6808)
//!     5. WornEffectStruct   (30 B) + ScrollName C-str + i32  (rof2.cpp:6824)
//!     6. WornEffectStruct   (30 B) + "\0" always + i32       (rof2.cpp:6847 — Bard, always empty)
//!  L. `ItemQuaternaryBodyStruct`        — 171 bytes (rof2_structs.h:4977, rof2.cpp:6892)
//!  M. uint32 subitem_count + (index u32 + recursive SerializeItem) × N  (rof2.cpp:6894–6926)
//!     Augments are in ItemSecondaryBodyStruct.augslots[6] — NOT recursive.
//!     Sub-items are bag contents (depth+1 recursive calls).  No depth guard in rof2.cpp.
//!
//! We extract only the fields the client renders (merchant window + inventory). Strings are
//! variable-length, so fields cannot be read at fixed offsets — we walk the blob in write order.

/// The subset of a RoF2-serialized item the client uses.
pub struct RoF2Item {
    pub slot_type:      u8,
    pub main_slot:      u16,
    pub sub_slot:       u16,
    pub price:          u32,
    /// Merchant stock count (header.merchant_slot for merchant items).
    pub merchant_count: u32,
    /// Number of items in this stack (1 for non-stackable). The display quantity.
    pub stacksize:      u32,
    pub charges:        u32,
    pub id:             u32,
    pub icon:           u32,
    pub name:           String,
    pub idfile:         String,
    /// Book/note text-file id (RoF2 item section I, `Item.Filename`). Empty for non-books; when set,
    /// the item is READABLE — send it in OP_ReadBook to fetch the text (#288).
    pub filename:       String,
    /// Item's click ("clicky") spell id from ClickEffectStruct.effect — 0 if the item has no
    /// clickable effect. Used to activate teleport rings/port potions via an item cast. (eqoxide#193)
    pub click_spell_id: u32,
    /// Nested bag contents (this item is a container): (bag_sub_index 0-9, sub-item). Empty for
    /// non-containers. The caller maps each to a flat bag wire slot. (eqoxide#201)
    pub bag_contents: Vec<(u32, RoF2Item)>,
}

// ── Fixed struct sizes from rof2_structs.h (pragma pack(1)) ──────────────────
const HDR_LEN:          usize = 77;  // sizeof(ItemSerializationHeader)   rof2_structs.h:4733
const EVOLVING_LEN:     usize = 25;  // sizeof(EvolvingItem_Struct)        rof2_structs.h:4756
const FINISH_LEN:       usize = 26;  // sizeof(ItemSerializationHeaderFinish) rof2_structs.h:4765
const BODY_LEN:         usize = 255; // sizeof(ItemBodyStruct)             rof2_structs.h:4777
const SECONDARY_LEN:    usize = 74;  // sizeof(ItemSecondaryBodyStruct)    rof2_structs.h:4872
const TERTIARY_LEN:     usize = 76;  // sizeof(ItemTertiaryBodyStruct)     rof2_structs.h:4898
const CLICK_LEN:        usize = 30;  // sizeof(ClickEffectStruct)          rof2_structs.h:4932
const PROC_LEN:         usize = 30;  // sizeof(ProcEffectStruct)           rof2_structs.h:4947
const WORN_LEN:         usize = 30;  // sizeof(WornEffectStruct)           rof2_structs.h:4962
const QUATERNARY_LEN:   usize = 171; // sizeof(ItemQuaternaryBodyStruct)   rof2_structs.h:4977

use crate::wire::WireReader;

/// Deserialize one RoF2-serialized item.
///
/// Returns `(item, consumed_bytes)` where `consumed_bytes` is the exact number of bytes
/// from the start of `buf` that belong to this one item.  Use this to split back-to-back
/// items in OP_CharInventory (header counts N items, each serialized in full).
///
/// Pass `buf` pointing at the *start* of the item (i.e. the first byte of
/// `ItemSerializationHeader.unknown000`).  For OP_ItemPacket callers, strip the 4-byte
/// `PacketType` prefix first.
pub fn parse_rof2_item(buf: &[u8]) -> Option<(RoF2Item, usize)> {
    // VARIABLE-LENGTH + recursive: an OP_CharInventory / OP_ItemPacket carries a *count* of
    // back-to-back items and this fn returns the exact consumed size so the caller can split them.
    // The caller (`apply_char_inventory`) treats a `None` as "stop, keep what parsed" — so this
    // decoder KEEPS the non-panicking (`try_*` → `?` → None) contract rather than panicking on a
    // short/garbled item, to avoid crashing a whole inventory load on one bad record. It uses the
    // unified `WireReader` mechanism (collapsing the old read_cstr/skip_cstr/skip helpers) but on
    // its non-panicking path. NOTE: this is a deliberate exception to the panic-on-mismatch rule —
    // flagged for the orchestrator.
    let len = buf.len();
    if len < HDR_LEN { return None; }

    // Header fields sit at fixed offsets within the guaranteed-present 77-byte header.
    let u16a = |o: usize| u16::from_le_bytes([buf[o], buf[o + 1]]);
    let u32a = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);

    // ── A. ItemSerializationHeader (77 bytes) ──────────────────────────────────
    // For a STACKABLE item, `stacksize` (@17) is the number in the stack (rof2.cpp:6453 sets
    // it to inst->GetCharges()); `charges` (@56) is only 0/1 for stackables. For a charged
    // non-stackable item (wand) `charges` holds the charge count. So the slot QUANTITY is
    // `stacksize` — reading `charges` made every stack display as 1.
    let stacksize      = u32a(17);
    let slot_type      = buf[25];
    let main_slot      = u16a(26);
    let sub_slot       = u16a(28);
    let price          = u32a(32);
    let merchant_count = u32a(36);
    let charges        = u32a(56);
    let is_evolving    = buf[76];

    let mut r = WireReader::new(buf, "RoF2Item");
    r.skip(HDR_LEN); // consume the 77-byte header (guaranteed present by the len check above)

    // ── B. EvolvingItem_Struct (optional, 25 bytes) ───────────────────────────
    if is_evolving > 0 {
        r.try_skip(EVOLVING_LEN)?;
    }

    // ── C. Two ornamentation C-strings ────────────────────────────────────────
    r.try_cstr()?; // main-hand ornament (or empty NUL)
    r.try_cstr()?; // off-hand ornament  (or empty NUL)

    // ── D. ItemSerializationHeaderFinish (26 bytes) ───────────────────────────
    r.try_skip(FINISH_LEN)?;

    // ── E. Name / Lore / IDFile / extra NUL ──────────────────────────────────
    // Name: only written when non-empty (rof2.cpp:6552–6555) — check for NUL vs data.
    // In practice all real items have a non-empty name; we still handle the empty-Name case.
    let name = if r.peek_u8() == Some(0) {
        // Name is absent (empty string, no NUL written): this NUL belongs to Lore — do NOT consume.
        String::new()
    } else {
        r.try_cstr()?
    };
    let _lore = r.try_cstr()?;
    let idfile = r.try_cstr()?;
    r.try_skip(1)?; // extra NUL (rof2.cpp:6565)

    // ── F. ItemBodyStruct (255 bytes) — id@0, icon@20 ──────────────────────────
    let body_start = r.pos();
    r.try_skip(BODY_LEN)?;
    // id is at offset 0 and icon at offset 20 within body (now guaranteed in-bounds).
    let id   = u32a(body_start);
    let icon = u32a(body_start + 20);

    // ── G. CharmFile C-string ─────────────────────────────────────────────────
    r.try_cstr()?;

    // ── H. ItemSecondaryBodyStruct (74 bytes) ─────────────────────────────────
    r.try_skip(SECONDARY_LEN)?;

    // ── I. Filename C-string — the book/note text-file id (empty for non-books, #288) ─────────
    let filename = r.try_cstr()?;

    // ── J. ItemTertiaryBodyStruct (76 bytes) ──────────────────────────────────
    r.try_skip(TERTIARY_LEN)?;

    // ── K. 6 effect blocks: fixed struct + effect-name C-str + int32(0) ───────
    // 1. ClickEffectStruct (30) + ClickName C-str + i32
    // ClickEffectStruct.effect (int32 @0) is the item's click ("clicky") spell id — >0 for a
    // clickable effect (teleport potions/rings, etc.), 0/-1 for none. Read it before advancing so
    // an item-activate cast can send it as the CastSpell_Struct spell_id. (eqoxide#193)
    let click_effect = { let o = r.pos(); if o + 4 > len { return None; } i32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]) };
    let click_spell_id = if click_effect > 0 { click_effect as u32 } else { 0 };
    r.try_skip(CLICK_LEN)?; r.try_cstr()?; r.try_skip(4)?;
    // 2. ProcEffectStruct (30) + ProcName C-str + i32
    r.try_skip(PROC_LEN)?;  r.try_cstr()?; r.try_skip(4)?;
    // 3. WornEffectStruct (30) + WornName C-str + i32
    r.try_skip(WORN_LEN)?;  r.try_cstr()?; r.try_skip(4)?;
    // 4. WornEffectStruct (30) + FocusName C-str + i32
    r.try_skip(WORN_LEN)?;  r.try_cstr()?; r.try_skip(4)?;
    // 5. WornEffectStruct (30) + ScrollName C-str + i32
    r.try_skip(WORN_LEN)?;  r.try_cstr()?; r.try_skip(4)?;
    // 6. WornEffectStruct (30) + "\0" (Bard — always empty) + i32
    r.try_skip(WORN_LEN)?;  r.try_cstr()?; r.try_skip(4)?;

    // ── L. ItemQuaternaryBodyStruct (171 bytes) ───────────────────────────────
    r.try_skip(QUATERNARY_LEN)?;

    // ── M. Sub-items (bag contents): uint32 count + (uint32 index + item) × N ─
    // Each nested sub-item is preceded by its 0-based bag slot index (EQEmu SerializeItem writes
    // `index` = invbag::SLOT_BEGIN..SLOT_END before each; common/patches/rof2.cpp:6913). Retain
    // them so bagged items are visible/movable — the caller maps (this item's slot, sub_index) to
    // a flat bag wire slot. (eqoxide#201)
    let subitem_count = r.try_u32()?;
    let mut bag_contents = Vec::new();
    for _ in 0..subitem_count {
        let sub_index = r.try_u32()?; // uint32 bag-slot index
        // Recursive: parse the sub-item to get its consumed size.
        let (sub, sub_len) = parse_rof2_item(r.rest())?;
        r.skip(sub_len); // sub_len ≤ remaining (the recursive parse just consumed it)
        bag_contents.push((sub_index, sub));
    }

    Some((RoF2Item { slot_type, main_slot, sub_slot, price, merchant_count, stacksize, charges,
                     id, icon, name, idfile, filename, click_spell_id, bag_contents }, r.pos()))
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a complete minimal RoF2-serialized item blob (all effect-name strings empty,
    /// no sub-items, no ornaments, no evolving data).  Fields set:
    ///   slot_type=0 (possessions), main_slot=23 (RoF2 general slot 1), price=100,
    ///   merchant_slot=1, charges=0, id=1001, icon=678, name="Cloth Cap", idfile="IT63".
    ///
    /// Returns the blob and its expected total length so callers can assert consumed == len.
    pub fn fixture() -> Vec<u8> {
        let mut b = vec![0u8; HDR_LEN];
        // ItemSerializationHeader
        b[25] = 0;                                          // slot_type = possessions
        b[26..28].copy_from_slice(&23u16.to_le_bytes());   // main_slot = 23 (RoF2 general1)
        b[28..30].copy_from_slice(&0xffffu16.to_le_bytes()); // sub_slot = invalid
        b[32..36].copy_from_slice(&100u32.to_le_bytes());  // price
        b[36..40].copy_from_slice(&1u32.to_le_bytes());    // merchant_slot = 1
        b[56..60].copy_from_slice(&0u32.to_le_bytes());    // charges = 0
        b[76] = 0;                                          // isEvolving = false
        // C. Two ornament C-strings (empty)
        b.push(0); b.push(0);
        // D. ItemSerializationHeaderFinish (26 bytes)
        b.extend_from_slice(&[0u8; FINISH_LEN]);
        // E. Name/Lore/IDFile/extra NUL
        b.extend_from_slice(b"Cloth Cap\0");   // Name
        b.push(0);                              // Lore (empty but always writes NUL)
        b.extend_from_slice(b"IT63\0");         // IDFile
        b.push(0);                              // extra NUL (rof2.cpp:6565)
        // F. ItemBodyStruct (255 bytes): id@0, icon@20
        let mut body = vec![0u8; BODY_LEN];
        body[0..4].copy_from_slice(&1001u32.to_le_bytes()); // id
        body[20..24].copy_from_slice(&678u32.to_le_bytes());// icon
        b.extend_from_slice(&body);
        // G. CharmFile C-string (empty)
        b.push(0);
        // H. ItemSecondaryBodyStruct (74 bytes)
        b.extend_from_slice(&[0u8; SECONDARY_LEN]);
        // I. Filename C-string (empty)
        b.push(0);
        // J. ItemTertiaryBodyStruct (76 bytes)
        b.extend_from_slice(&[0u8; TERTIARY_LEN]);
        // K. 6 effect blocks: struct + empty-name NUL + int32(0)
        for struct_len in [CLICK_LEN, PROC_LEN, WORN_LEN, WORN_LEN, WORN_LEN, WORN_LEN] {
            b.extend_from_slice(&vec![0u8; struct_len]); // effect struct
            b.push(0);                                    // effect name (empty)
            b.extend_from_slice(&[0u8; 4]);               // int32 trailing
        }
        // L. ItemQuaternaryBodyStruct (171 bytes)
        b.extend_from_slice(&[0u8; QUATERNARY_LEN]);
        // M. subitem_count = 0
        b.extend_from_slice(&[0u8; 4]);
        b
    }

    /// Second distinct item for multi-item tests (id=2002, icon=999, main_slot=24).
    /// Shares the same name/lore/idfile layout as fixture() so we can patch header+body offsets
    /// by exact byte position without recomputing string positions.
    pub fn fixture2() -> Vec<u8> {
        let mut b = fixture();
        // patch main_slot to 24 (RoF2 general slot 2)
        b[26..28].copy_from_slice(&24u16.to_le_bytes());
        // patch price to 500
        b[32..36].copy_from_slice(&500u32.to_le_bytes());
        // patch ItemBodyStruct id and icon.
        // body_start = HDR_LEN + 2(ornaments) + FINISH_LEN + len("Cloth Cap\0")(10) + 1(lore)
        //            + len("IT63\0")(5) + 1(extra NUL)
        let body_start = HDR_LEN + 2 + FINISH_LEN + 10 + 1 + 5 + 1; // = 122
        b[body_start..body_start + 4].copy_from_slice(&2002u32.to_le_bytes());     // id
        b[body_start + 20..body_start + 24].copy_from_slice(&999u32.to_le_bytes()); // icon
        b
    }

    #[test]
    fn parses_item_fields_and_returns_consumed_size() {
        let blob = fixture();
        let expected_len = blob.len();
        let (it, consumed) = parse_rof2_item(&blob).expect("parse");
        assert_eq!(consumed, expected_len, "consumed must equal full fixture length");
        assert_eq!(it.slot_type, 0);
        assert_eq!(it.main_slot, 23);
        assert_eq!(it.sub_slot, 0xffff);
        assert_eq!(it.price, 100);
        assert_eq!(it.id, 1001);
        assert_eq!(it.icon, 678);
        assert_eq!(it.name, "Cloth Cap");
        assert_eq!(it.idfile, "IT63");
        assert_eq!(it.click_spell_id, 0, "base fixture has no click effect");
    }

    /// eqoxide#193: an item with a positive ClickEffectStruct.effect exposes it as click_spell_id.
    #[test]
    fn parses_click_spell_id_from_click_effect() {
        // The click block (K, first effect) begins right after J (TertiaryBodyStruct). Locate its
        // start from the same layout the fixture writes, then patch effect (int32 @0). Using the
        // module constants keeps this correct if any block size changes.
        let body_start = HDR_LEN + 2 + FINISH_LEN + 10 + 1 + 5 + 1; // matches fixture2() comment
        let click_start = body_start + BODY_LEN + 1 + SECONDARY_LEN + 1 + TERTIARY_LEN;

        let mut b = fixture();
        b[click_start..click_start + 4].copy_from_slice(&2512i32.to_le_bytes()); // effect = spell 2512
        let (it, _) = parse_rof2_item(&b).expect("parse");
        assert_eq!(it.click_spell_id, 2512);

        // A non-positive effect (0 / -1) means "no clicky" → 0.
        let mut b0 = fixture();
        b0[click_start..click_start + 4].copy_from_slice(&(-1i32).to_le_bytes());
        let (it0, _) = parse_rof2_item(&b0).expect("parse");
        assert_eq!(it0.click_spell_id, 0);
    }

    /// eqoxide#201: a container's nested sub-items (section M) are retained with their bag index.
    #[test]
    fn parses_bag_contents_with_sub_index() {
        // Base fixture ends with subitem_count = 0 (last 4 bytes). Rewrite it to 1 and append one
        // sub-record: bag index (u32) + a full nested item blob (fixture2, id 2002).
        let mut bag = fixture();
        let n = bag.len();
        bag[n - 4..n].copy_from_slice(&1u32.to_le_bytes()); // subitem_count = 1
        bag.extend_from_slice(&2u32.to_le_bytes());         // bag sub-index = 2
        bag.extend_from_slice(&fixture2());                 // nested sub-item

        let (it, consumed) = parse_rof2_item(&bag).expect("parse");
        assert_eq!(consumed, bag.len(), "consumes container + nested sub-item");
        assert_eq!(it.bag_contents.len(), 1, "one bag content retained");
        let (sub_index, sub) = &it.bag_contents[0];
        assert_eq!(*sub_index, 2, "bag sub-index preserved");
        assert_eq!(sub.id, 2002, "nested sub-item is fixture2 (id 2002)");
        assert!(sub.bag_contents.is_empty(), "nested item has no further contents");
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
        let (it, consumed) = parse_rof2_item(&b).expect("parse evolving");
        assert_eq!(consumed, b.len());
        assert_eq!(it.id, 1001);
        assert_eq!(it.name, "Cloth Cap");
    }

    #[test]
    fn parses_merchant_item_fields() {
        let mut b = fixture();
        b[25] = 9;                                         // slot_type = merchant
        b[26..28].copy_from_slice(&3u16.to_le_bytes());   // main_slot = merchant slot 3
        b[36..40].copy_from_slice(&5u32.to_le_bytes());   // merchant_slot = stock count 5
        let (it, _consumed) = parse_rof2_item(&b).expect("parse merchant");
        assert_eq!(it.slot_type, 9);
        assert_eq!(it.main_slot, 3);
        assert_eq!(it.price, 100);
        assert_eq!(it.merchant_count, 5);
        assert_eq!(it.id, 1001);
        assert_eq!(it.icon, 678);
        assert_eq!(it.name, "Cloth Cap");
        assert_eq!(it.idfile, "IT63");
    }

    /// Two back-to-back items (as in OP_CharInventory): parse must split correctly.
    #[test]
    fn parse_two_concatenated_items() {
        let blob1 = fixture();
        let blob2 = fixture2();
        let mut combined = blob1.clone();
        combined.extend_from_slice(&blob2);

        let (item1, consumed1) = parse_rof2_item(&combined).expect("item1");
        assert_eq!(consumed1, blob1.len(), "item1 consumed wrong number of bytes");
        assert_eq!(item1.id, 1001);
        assert_eq!(item1.main_slot, 23);

        let (item2, consumed2) = parse_rof2_item(&combined[consumed1..]).expect("item2");
        assert_eq!(consumed1 + consumed2, combined.len());
        assert_eq!(item2.id, 2002);
        assert_eq!(item2.main_slot, 24);
    }
}
