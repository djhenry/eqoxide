# Quest turn-in reward delivery + the "Inventory Desyncronization" resync mechanism (RoF2)

Related: `item-serialization.md` (SerializeItem byte layout + slot mapping tables — read that
first for the RoF2 wire slot numbering used below).

---

## 1. How EQEmu delivers quest-turn-in rewards to the client

Confirmed in `EQEmu/zone/trading.cpp:319-687` (`Client::FinishTrade`, NPC branch) and
`EQEmu/zone/npc.cpp:4611-4701` (`NPC::ReturnHandinItems`):

- The NPC side of `FinishTrade` (trading.cpp:509-686) pops the handed-in items
  (`m_inv.PopItem`, trading.cpp:519) with **no client packet** — the client is expected to
  already believe those trade-window slots are empty once it receives `OP_FinishTrade`.
- Reward/return items are delivered via **`Client::PushItemOnCursor`**
  (`EQEmu/zone/inventory.cpp:1035-1048`), called from `ReturnHandinItems`
  (`EQEmu/zone/npc.cpp:4687`) and from `quest::summonitem` (default `to_slot =
  EQ::invslot::slotCursor`, `EQEmu/zone/client.h:1145`, `EQEmu/zone/questmgr.cpp:188-192`).
- `PushItemOnCursor` → `SendItemPacket(EQ::invslot::slotCursor, &inst, ItemPacketLimbo)`
  (inventory.cpp:1043). **Opcode: `OP_ItemPacket` (not OP_MoveItem, not a fresh
  OP_CharInventory).** `ItemPacketLimbo = 0x6A` in the modern shared enum
  (`EQEmu/common/eq_packet_structs.h`); the RoF2-namespaced copy of the same enum spells it
  `ItemPacketSummonItem = 0x6A` (`EQEmu/common/patches/rof2_structs.h:1762`) — **same wire
  value, two source-level names**; don't be confused if you see either name.
- If a reward is placed directly into a numbered slot instead (rare; e.g. `quest::summonitem`
  called with an explicit `to_slot`), `PutItemInInventory` sends
  `ItemPacketTrade` (0x67) instead of `ItemPacketLimbo` for any non-cursor destination
  (inventory.cpp:1066).
- **Money** rewards (`quest::givecash`) go through `Client::AddMoneyToPP` → `OP_MoneyUpdate`,
  a completely separate path from item packets (not investigated further here; if needed,
  grep `AddMoneyToPP`/`OP_MoneyUpdate` ENCODE in rof2.cpp).

### The server-side "cursor buffer" — RoF2 clients see rewards ONE AT A TIME

`EQ::InventoryProfile`'s cursor is actually a **deque** (`m_inv.PushCursor`,
`m_inv.cursor_cbegin()/cend()`), not a single slot. `Client::SendCursorBuffer`
(`inventory.cpp:916-957`) is a documented **RoF+-specific workaround**:

> "Temporary work-around for the RoF+ Client Buffer — Instead of dealing with client moving
> items in cursor buffer, we can just send the next item in the cursor buffer to the cursor."
> (inventory.cpp:918-920)

Only the **front** of the server's cursor deque is ever shown to an RoF2 client (one
`ItemPacketLimbo` at slot 33 at a time). When the client moves that item off the cursor (an
`OP_MoveItem` with `from_slot == slotCursor`), `SwapItem`'s step 7 calls `SendCursorBuffer()`
again if the cursor stack was fully consumed (`inventory.cpp:2211-2216`), which pushes the
**next** buffered reward to slot 33 via another `ItemPacketLimbo`. **Multiple quest rewards
therefore arrive as a sequence of single-item `ItemPacketLimbo` packets at slot 33, gated on
the client actually moving the previous one off the cursor** — not as a batch of
`ItemPacketTrade`s into distinct free slots. If eqoxide never issues the cursor-vacating move,
any 2nd+ reward item sits server-side in the cursor deque, invisible to the client, until the
next unrelated cursor-clearing action.

---

## 2. RoF2 SerializeItem slot encoding for these packets — already ENCODEd, do not re-translate

Confirmed (cross-ref `item-serialization.md`): every `OP_ItemPacket`/`OP_CharInventory` item
blob's `main_slot`/`sub_slot`/`slot_type` fields (`ItemSerializationHeader`,
`rof2_structs.h:4733-4754`) are produced by `SerializeItem` calling
**`ServerToRoF2Slot`** (`rof2.cpp:6930-7017`) on the server's internal slot number *before*
serialization. **The client must treat `main_slot` as already being the correct RoF2 wire
slot and must NOT run any further slot translation on it.** For `slotCursor`
(server slot 33, which is `< POSSESSIONS_SIZE(34)` so it passes straight through,
`rof2.cpp:6942-6944`) the header will read `slot_type=typePossessions(0)`, `main_slot=33`.
That is a legitimate cursor placement, not a sentinel.

**However** — and this is the crux of the bug below — `ServerToRoF2Slot` is a **generic
function used for every kind of slot the server ever names**, including trade-window slots
(`typeTrade=3`, server slots 3000-3007, `rof2.cpp:6995-6998`) and the "invalid" sentinel
(`server_slot` that matches none of the ranges → `Type=TYPE_INVALID(-1)`,
`Slot=SLOT_INVALID(-1)` — the all-`0xFFFF` defaults set at `rof2.cpp:6933-6938`). A slot
number arriving via `OP_ItemPacket` is only meaningful as "put this in my visible
inventory/equipment" when `slot_type == typePossessions(0)` **and** `0 <= main_slot <= 33`.
Any other `slot_type`, or a `main_slot` outside `0..=33`, must NOT be written into the
player's displayed inventory model — it belongs to a different (trade/bank/world/invalid)
namespace and the eqoxide model conflates them today (see §4).

---

## 3. What triggers "Inventory Desyncronization detected: Resending slot data..." — and what it actually sends

Exact string: `EQEmu/zone/inventory.cpp:2245`, inside **`Client::SwapItemResync`**
(`inventory.cpp:2238-2336`). It is called **only** from two places, both reacting to a
**client-sent `OP_MoveItem` (or `OP_MoveMultipleItems`) that the server's authoritative
`Client::SwapItem(mi)` rejected** (`client_packet.cpp:10933-10934` and
`client_packet.cpp:11011-11013`):

```cpp
if (!SwapItem(mi) && IsValidSlot(mi->from_slot) && IsValidSlot(mi->to_slot)) {
    SwapItemResync(mi);
    ...
}
```

So the trigger is: **the client asked to move an item between two slots, and the server's
inventory state didn't actually have what the client thought was there** (stale/incorrect
`from_slot`, or a `to_slot` that's no longer valid for the intended operation, e.g. a trade
slot after the trade session already closed). This is a client-bug symptom, not a server-
initiated maintenance ping. `SwapItemResync` does **not** re-send authoritative "here's what's
really in your inventory" data — it does something much cruder, per-slot, for whichever
`from_slot`/`to_slot` were in the *failed* packet:

1. Creates a scratch `EQ::ItemInstance` of **item id 22292 ("Copper Coin")**
   (`inventory.cpp:2251-2252`, comment: *"This prevents the client from crashing when closing
   any 'phantom' bags"*).
2. Sends that Copper Coin to the resync slot via `SendItemPacket(resync_slot, token_inst,
   ItemPacketTrade)` (**opcode `OP_ItemPacket`, type 0x67**) — this is a throwaway repaint,
   not a real item grant.
3. Immediately follows it with **either**:
   - the *actual* item really occupying that slot server-side, resent via another
     `ItemPacketTrade` (`inventory.cpp:2256`), **or**
   - if the slot is genuinely empty, an **`OP_DeleteItem`** packet
     (`inventory.cpp:2258-2265`, `from_slot=resync_slot`, `to_slot=0xFFFFFFFF`,
     `number_in_stack=0xFFFFFFFF` — same `DeleteItem_Struct`/`MoveItem_Struct` 28-byte wire
     shape, `rof2_structs.h:1814-1828`; RoF2 `ENCODE(OP_DeleteItem)` at `rof2.cpp:1148-1158`
     calls `ServerToRoF2Slot` on both fields, and `ENCODE(OP_DeleteCharge)` simply forwards to
     `ENCODE(OP_MoveItem)`, `rof2.cpp:1141-1146` — all three opcodes share one struct layout,
     differing only by opcode number: `OP_MoveItem=0x32ee`, `OP_DeleteItem=0x18ad`,
     `OP_DeleteCharge=0x01b8`, `EQEmu/utils/patches/patch_RoF2.conf:256,258,259`).

This runs **twice**, once for `move_slots->from_slot` and once for `move_slots->to_slot`
(`inventory.cpp:2247-2336`), and logs `"Source slot %i resyncronized."` /
`"Destination slot %i resyncronized."` — matching exactly the caller's observed log text.

**A slot value of 3000 in these log lines is `EQ::invslot::TRADE_BEGIN`**
(`rof2_limits.h:174`, `emu_constants.h` alias) — the first NPC trade-window slot, *not* a bank
slot (bank is 2000-2023, `rof2_limits.h:168-169`); if you see "3000" in a resync line it means
one side of the failed `OP_MoveItem` was a trade-window slot.

### The correct client behavior that avoids ever triggering this

Never send an `OP_MoveItem` whose `from_slot`/`to_slot` isn't known-good **at the moment the
packet is actually transmitted** — in particular:

- Don't reference NPC trade-window slots (3000+) except during an active trade session that
  the *server* has acknowledged (`OP_TradeRequestAck`), and only for a `from_slot` (cursor)
  that is verified to actually hold the item (not assumed from a stale caller-supplied slot
  number).
- After `OP_FinishTrade`, the trade slots are already gone server-side
  (`m_inv.PopItem`/`DeleteItemInInventory` with `client_update=false`,
  `trading.cpp:518-521`, `inventory.cpp:960` default) — the client only needs to clear its
  *own* mirrored trade-slot display; it must never re-reference 3000-3007 in a later
  `OP_MoveItem`.
- After a genuinely-accepted move, the authoritative final state is: the handed-in item is
  gone; any NPC reward sits on the cursor (delivered as `ItemPacketLimbo`/0x6A at slot 33, one
  at a time per §1); there is **no** `OP_TradeReset` needed and **no** fresh `OP_CharInventory`
  is sent for a normal handin — the client must rely on the item/delete packets it's already
  receiving.

---

## 4. Root-cause diagnosis for the eqoxide bug (Copper Coin / item 22292 at slots 0, 28, 33; log mentions trade slot 3000)

This is **not** a quest-reward-serialization bug. Item id **22292 is EQEmu's own internal
diagnostic placeholder** (`inventory.cpp:2251` / `2276` / `2293` / literal string `'Copper
Coin'` in the comment) used by `SwapItemResync` — it is never a real quest reward. Its
appearance is direct proof that **eqoxide sent a client-authoritative `OP_MoveItem` that the
server's `SwapItem()` rejected**, and two independent eqoxide bugs then compound the symptom:

**(a) `apply_item_packet` blindly trusts `ItemPacketTrade`'s `main_slot` as an inventory
write target with no bounds/sanity check** (`eqoxide/src/eq_net/packet_handler.rs:584-593`):

```rust
} else {
    // OP_CharInventory / equip / cursor etc.: `main_slot` IS the authoritative slot.
    ...
    for it in upserts {
        gs.inventory.retain(|x| x.slot != it.slot);
        gs.inventory.push(it);
    }
}
```

`ItemPacketTrade` (0x67) is neither `ITEM_PACKET_MERCHANT` (0x64) nor `ITEM_PACKET_LOOT`
(0x66), so it always falls into this unconditional-upsert branch — including when it's really
a `SwapItemResync` diagnostic packet naming a `main_slot` that came from a *failed* move
(which can be equipment slot 0, a general slot, cursor 33, or even a bag/trade-range slot the
UI has no business displaying).

**(b) `OP_DeleteItem` / `OP_DeleteCharge` are completely unhandled** — there is no
`OP_DELETE_ITEM`/`OP_DELETE_CHARGE` constant in `eqoxide/src/eq_net/protocol.rs` and no match
arm in `apply_packet` (`eqoxide/src/eq_net/packet_handler.rs:12-110`, falls to `_ => {}`).
Per §3, the Copper Coin token is **always** followed by a corrective packet — either the real
item, or (when the slot is truly empty, which is the common case for a bogus resync) an
**`OP_DeleteItem`** that the real client would use to remove the just-placed token. eqoxide
drops that corrective packet on the floor, so the Copper Coin sticks around permanently at
whatever slot the failed move named.

**Likely upstream trigger**, from `eqoxide/src/eq_net/navigation.rs:2342-2390`
(`tick_give`, the `POST /v1/interact/give` state machine): step 1 sends
`build_move_item(from_slot, SLOT_CURSOR)` where **`from_slot` is caller-supplied** at request
time (`eqoxide/src/http/interact.rs:192-199`, `GiveBody.from`) and never re-validated against
the live `gs.inventory` at the moment the tick actually fires. If the caller passes a stale or
wrong slot (e.g. `from=0`, which in RoF2 is the **charm equipment slot**, not a general slot —
see the Titanium-vs-RoF2 slot numbering trap in `item-serialization.md`), the server's
`m_inv.GetItem(0)` is empty, `SwapItem` fails, and `SwapItemResync(mi)` fires with
`from_slot=0, to_slot=33 (SLOT_CURSOR)` — producing **exactly** "equipment slot 0" and
"cursor slot 33" Copper Coins. A `general slot 28` artifact is consistent with a second,
separate failed move (e.g. another `/give` call with a wrong `from`, or the cursor→trade step
at `navigation.rs:2372` firing when the cursor was unexpectedly already empty).

### Recommendation for eqoxide

1. **Add `OP_DELETE_ITEM = 0x18ad` and `OP_DELETE_CHARGE = 0x01b8`** to `protocol.rs`
   (`patch_RoF2.conf:258-259`) and a handler in `apply_packet` that parses the 28-byte
   `MoveItem_Struct`-shaped payload (`InventorySlot_Struct from_slot`(12) +
   `InventorySlot_Struct to_slot`(12) + `u32 number_in_stack`(4)) and **removes** whatever is
   at the RoF2-wire `from_slot.Slot` (when `from_slot.Type == typePossessions(0)`) from
   `gs.inventory`. This alone stops the Copper Coin (and any other resync artifact) from
   sticking around.
2. **Harden `apply_item_packet`'s non-Merchant/non-Loot branch**: only upsert into
   `gs.inventory` when the parsed item's slot type resolves to `typePossessions` (equivalent —
   `parse_rof2_item` would need to also decode `slot_type`, currently `main_slot` is read
   without it, per `item-serialization.md`'s header layout, offset 25) **and** `0 <= main_slot
   <= 33`. Anything else (trade/bank/world/invalid) must be dropped, not written into the
   player-visible inventory model.
3. Optionally, treat **item id 22292 arriving via `ItemPacketTrade`** as a known
   server-diagnostic token (log a warning) rather than a real item — it should never persist;
   if it does, that's a signal a prior `OP_MoveItem` was rejected and is worth surfacing to the
   caller/agent immediately rather than silently corrupting the inventory view.
4. **Fix `tick_give`'s `from_slot` trust**: re-resolve the slot to move from the live
   `gs.inventory` at the moment the give tick actually executes (e.g. look up by item id/name
   passed in the request, not a slot captured earlier), so a stale or Titanium-era slot number
   from the caller can't reach the wire as an `OP_MoveItem` and trip `SwapItemResync` in the
   first place.
5. For reward delivery generally: expect rewards on **slot 33 (cursor)** via `ItemPacketLimbo`
   (0x6A), **one at a time** — after accepting/consuming the cursor item (moving it to a real
   slot), expect the *next* reward (if any) as another `ItemPacketLimbo` at slot 33, not a
   batch. Don't assume `OP_CharInventory` or a fresh full-inventory resend accompanies a normal
   handin.
