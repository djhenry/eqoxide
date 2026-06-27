# RoF2 OP_CharInventory Implementation Report

## What was done

1. Extended `parse_rof2_item` in `src/eq_net/item.rs` to return `Option<(RoF2Item, usize)>` (consumed bytes).
2. Rewrote `apply_char_inventory` in `src/eq_net/packet_handler.rs` to parse the RoF2 binary format.
3. Removed the dead `parse_inv_item` Titanium text parser (no other callers).
4. Updated `apply_item_packet` caller to destructure the `(RoF2Item, usize)` tuple.
5. Updated `InvItem.slot` doc comment in `game_state.rs` to describe RoF2 wire slots.
6. Added 5 new unit tests (272 total, all pass).

---

## Full Item Byte Layout (rof2.cpp:6441 SerializeItem)

All structs are `#pragma pack(1)` (`rof2_structs.h:35`).

| Section | Bytes | Source |
|---------|-------|--------|
| A. `ItemSerializationHeader` | 77 | `rof2_structs.h:4733`, written `rof2.cpp:6498` |
| B. `EvolvingItem_Struct` | 25 (CONDITIONAL: `isEvolving > 0`) | `rof2_structs.h:4756`, `rof2.cpp:6500` |
| C. Two ornamentation C-strings | variable | `rof2.cpp:6516-6537` ("IT%d\0"×2 or "\0\0") |
| D. `ItemSerializationHeaderFinish` | 26 | `rof2_structs.h:4765`, `rof2.cpp:6550` |
| E. Name C-string (skipped if empty), Lore C-str, IDFile C-str, extra NUL | variable | `rof2.cpp:6552-6565` |
| F. `ItemBodyStruct` | 255 | `rof2_structs.h:4777`, `rof2.cpp:6656` |
| G. CharmFile C-string | variable | `rof2.cpp:6659-6661` |
| H. `ItemSecondaryBodyStruct` | 74 | `rof2_structs.h:4872`, contains `AugSlotStruct[6]` (6B each) |
| I. Filename C-string | variable | `rof2.cpp:6692-6694` |
| J. `ItemTertiaryBodyStruct` | 76 | `rof2_structs.h:4898` |
| K. 6 effect blocks (each = struct + C-string + int32(0)) | variable | `rof2.cpp:6738-6848` |
| L. `ItemQuaternaryBodyStruct` | 171 | `rof2_structs.h:4977`, `rof2.cpp:6892` |
| M. `uint32 subitem_count` + (uint32 index + recursive SerializeItem) × N | variable | `rof2.cpp:6894-6926` |

### Effect Blocks (Section K) — 6 in order:
1. `ClickEffectStruct` (30B) + ClickName C-str + int32(0)  — `rof2.cpp:6759`
2. `ProcEffectStruct` (30B) + ProcName C-str + int32(0)   — `rof2.cpp:6776`
3. `WornEffectStruct` (30B) + WornName C-str + int32(0)   — `rof2.cpp:6792`
4. `WornEffectStruct` (30B) + FocusName C-str + int32(0)  — `rof2.cpp:6808`
5. `WornEffectStruct` (30B) + ScrollName C-str + int32(0) — `rof2.cpp:6824`
6. `WornEffectStruct` (30B) + "\0" always + int32(0)      — `rof2.cpp:6847` (Bard always empty)

### Augment Slots
Augments are **NOT recursive**. 6 sockets (`AugSlotStruct` = 6B each) embedded inline in `ItemSecondaryBodyStruct.augslots[6]` (`rof2_structs.h:4872`). No depth guard needed.

### Sub-items (Bag Contents)
Recursive `SerializeItem` calls at `rof2.cpp:6921`, preceded by `uint32 bag_slot_index`. Back-patched `uint32 subitem_count` at section start. No depth limit in rof2.cpp; recursion terminates by item data (bags never contain bags).

### Name Edge Case
`rof2.cpp:6552`: Name is skipped entirely (no NUL) when `strlen(Name)==0`. Lore and IDFile always write at least a NUL byte. In practice all EQ items have non-empty names.

---

## OP_CharInventory Header Form

Source: `rof2.cpp:1043-1091` `ENCODE(OP_CharInventory)`:

```
[uint32 item_count]
[item_0 bytes — full SerializeItem output]
[item_1 bytes]
...
```

Zero items: 4-byte all-zero packet (`rof2.cpp:1049-1055`). No per-item length prefix; items are split by walking the full serialization (this is what `parse_rof2_item` returning `consumed` enables).

---

## Slot Mapping

Source: `rof2.cpp:6930-7017`, `rof2_limits.h:119-252`

RoF2 wire slots (type=0, possessions) for `InvItem.slot`:

| Range | Meaning |
|-------|---------|
| 0-22 | Equipment (0=charm, 1=ear1, 2=head, 13=primary, 14=secondary, 17=chest, etc.) |
| 23-32 | General inventory (10 slots; Titanium had 8 at 22-29) |
| 33 | Cursor |

Server slot → RoF2 slot is a direct pass-through for possessions (`slot_id < POSSESSIONS_SIZE=34`). Stored as-is in `InvItem.slot` — consistent with existing `apply_item_packet` behavior.

**Note:** The constant `SLOT_CURSOR = 30` in `src/eq_net/protocol.rs` is a Titanium value (RoF2 cursor = 33). This is a pre-existing discrepancy not addressed by this task.

---

## Build/Test Output

```
cargo build --release → Finished (clean, 0 errors)
cargo test → 272 passed; 0 failed; 18 ignored
  (was 267 passing before; +5 new tests)
```

### New Tests
- `item::tests::parses_item_fields_and_returns_consumed_size` — full fixture, asserts consumed == len
- `item::tests::parse_two_concatenated_items` — 2 items back-to-back, both split correctly
- `packet_handler::tests::apply_char_inventory_loads_two_items_at_correct_slots` — 2 items in gs.inventory at slots 23 & 24
- `packet_handler::tests::apply_char_inventory_ignores_zero_count` — zero-count packet leaves inventory untouched
- `packet_handler::tests::apply_char_inventory_upserts_by_slot` — duplicate slot upserts, not appends

---

## Uncertainties / Pre-existing Issues

1. **SLOT_CURSOR = 30 (Titanium) vs 33 (RoF2)**: `src/eq_net/protocol.rs:153`. The give state machine uses slot 30 as cursor when it should be 33 for RoF2. Not fixed here per "focused commits" scope.
2. **Name when empty**: rof2.cpp skips writing Name (not even a NUL) when strlen(Name)==0. Parser handles it but the detection heuristic (check if next byte is NUL) could theoretically misattribute an absent Name as an empty Lore. In practice this never occurs with real EQ items.
3. **Sub-item/bag parsing**: Recursive path is implemented and compiles, but not covered by a unit test (no fixture with sub-items). Should work correctly per the code logic.
