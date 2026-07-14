# Merchant buy (OP_ShopPlayerBuy) — RoF2 server reply protocol

## Request struct (client -> server), confirmed

`Merchant_Sell_Struct`, 32 bytes, `EQEmu/common/patches/rof2_structs.h:2284-2294`:

```
/*000*/ uint32 npcid;      // merchant NPC entity id
/*004*/ uint32 playerid;   // buyer entity id
/*008*/ uint32 itemslot;   // merchant window slot (NOT an inventory slot)
/*012*/ uint32 unknown12;
/*016*/ uint32 quantity;   // requested qty
/*020*/ uint32 unknown20;
/*024*/ uint32 price;      // client-suggested price; server recomputes it
/*028*/ uint32 unknown28;
```

RoF2 wire ENCODE/DECODE for `OP_ShopPlayerBuy` is a straight 1:1 field copy
(`EQEmu/common/patches/rof2.cpp:3662-3673` ENCODE, `:6104-6115` DECODE) — no
translation, so the struct above IS the wire layout in both directions.
Opcode value `0x0ddd`, `EQEmu/utils/patches/patch_RoF2.conf:472`.

## Handler: `Client::Handle_OP_ShopPlayerBuy`

`EQEmu/zone/client_packet.cpp:14126-14367`.

### On SUCCESS (item found, in range, affordable, room in inventory)

Two packets are queued, in this order in the code:

1. **`OP_ItemPacket`** (opcode `0x368e`, `patch_RoF2.conf:260`) via
   `SendItemPacket(freeslotid, inst, ItemPacketTrade)`
   (`client_packet.cpp:14308`, impl at `EQEmu/zone/inventory.cpp:2970`).
   `ItemPacket_Struct` (`EQEmu/common/eq_packet_structs.h:1589-1594`):
   `uint32 PacketType` (offset 0) + serialized item bytes (offset 4+).
   `PacketType = ItemPacketTrade = 0x67` (`eq_packet_structs.h:1546`).
   **`ItemPacketTrade` is NOT merchant-specific** — it's the generic "item
   placed into one of your inventory slots" tag, reused for loot claims,
   trade-window completion, quest rewards, bag pulls, equip-on-login, etc.
   (30+ call sites in `zone/*.cpp`). **eqoxide cannot identify a merchant buy
   from this packet's `PacketType` alone.**
2. **Echo of `OP_ShopPlayerBuy`** (same opcode `0x0ddd`, server->client) via
   `QueuePacket(outapp)` (`client_packet.cpp:14304`), same 32-byte
   `Merchant_Sell_Struct` layout, fields set at `client_packet.cpp:14217-14221`
   (`npcid`, `playerid`, `itemslot` echoed from the request; `quantity`
   possibly clamped to stack/charge limits; `price` recomputed server-side
   from `item->Price * item->SellRate` with rule mods,
   `client_packet.cpp:14235-14239`). **No success/fail flag field exists** —
   the struct's mere arrival (with `npcid`/`itemslot` matching the pending
   request) IS the success signal.
   - `unknown12`/`unknown20`/`unknown28` are left as whatever `new
     EQApplicationPacket` zero-initializes them to (0) — not populated by
     the handler.
   - **This is the authoritative success confirmation eqoxide should key
     off of**, matched against the outstanding request by
     `(npcid, itemslot, quantity)`.

Charged/limited stock (`tmpmer_used`) additionally sends either:
- `OP_ShopDelItem` (`Merchant_DelItem_Struct`) broadcast to all clients
  viewing that merchant, if stock hits 0 (`client_packet.cpp:14315-14322`), or
- another `SendItemPacket(mp->itemslot, inst, ItemPacketMerchant)` —
  `ItemPacketMerchant = 0x64` — to the buyer only, to update the merchant
  window's remaining-charges display (`client_packet.cpp:14324-14330`).

### On FAILURE — mostly SILENT, no dedicated error packet/opcode exists

| Condition | What's sent | Cite |
|---|---|---|
| Merchant not found/not NPC/not Merchant class/qty<1/out of range | `OP_ShopEndConfirm` (empty, 0-byte packet, opcode `0x3196`) via `SendMerchantEnd()` — closes the merchant window, no error text | `client_packet.cpp:14150-14153`, `SendMerchantEnd` impl `client.cpp:13276-13287` |
| Item id resolved to 0 (stale client-side merchant list) | `MessageString(ALREADY_SOLD)` chat text + `SendMerchantInventory` refresh + `SendMerchantEnd()` | `client_packet.cpp:14190-14196` |
| Lore conflict | `MessageString(DUPE_LORE_MERCHANT)` chat text only — **no packet, handler just `return`s** | `client_packet.cpp:14198-14201` |
| Computed price negative | `MessageString(ALREADY_SOLD)` + `SendMerchantEnd()` | `client_packet.cpp:14226-14231` |
| **Insufficient funds** (`TakeMoneyFromPP` returns false) | **NOTHING** — only a server-side hack-log entry, handler `return`s with no message and no packet at all | `client_packet.cpp:14247-14264` |
| Inventory full (no free slot / cursor occupied) | Red chat `Message()` only, **no confirm packet — and by this point money has ALREADY been deducted with no refund** (code comment admits this is a known bug) | `client_packet.cpp:14267-14284` |

There is no "buy failed" opcode/struct in RoF2. Absence of the echoed
`OP_ShopPlayerBuy` (or presence of `OP_ShopEndConfirm` / a chat `Message`)
is the only failure signal, and for insufficient-funds it's total silence.

## `OP_MoneyUpdate` — confirmed NOT sent for buys

`TakeMoneyFromPP(uint64 copper, bool update_client = false)` default arg at
`EQEmu/zone/client.h:899`. `SendMoneyUpdate()` (-> `OP_MoneyUpdate`,
`0x640c`) is only invoked `if (update_client)` inside `TakeMoneyFromPP`
(`EQEmu/zone/client.cpp:2793,2809,...`). The buy handler calls
`TakeMoneyFromPP(mpo->price)` — single argument — at `client_packet.cpp:14257`,
so `update_client` is `false`. **Confirmed: no `OP_MoneyUpdate` packet is
ever sent as a result of a merchant buy.** The client must derive its new
coin total itself from `mpo->price` in the echoed `OP_ShopPlayerBuy`.

## Recommendation for eqoxide

- Add an inbound handler for `OP_ShopPlayerBuy` (opcode `0x0ddd`, same as
  the outbound request opcode — direction, not opcode, distinguishes them).
  Parse the 32-byte `Merchant_Sell_Struct` reply.
- Match it against the last-sent buy request by `(npcid, itemslot)` (and
  optionally `quantity`) to confirm it's the reply to *our* pending buy, not
  a stray/duplicate.
- On match: this IS success. Deduct `reply.price` copper from local coin
  state (do NOT wait for `OP_MoneyUpdate` — none will come), and treat the
  quantity as `reply.quantity` (server may have clamped it), not the
  originally requested quantity.
- Do not rely on `OP_ItemPacket`/`ItemPacketTrade` to confirm the buy — it's
  ambiguous with every other "item entered your inventory" event. Use it
  only to learn which inventory slot/item instance the item landed in,
  once you already know (from the `OP_ShopPlayerBuy` echo) that a buy
  succeeded. Order-wise the item packet is queued before the echo
  (`client_packet.cpp:14308` vs `:14304` — `SendItemPacket` uses
  `FastQueuePacket`, the echo uses `QueuePacket`), so eqoxide should not
  assume a strict enqueue-order guarantee across two different packet
  queues; correlate by content, not arrival order.
- Treat "no reply within a timeout" as a failure to close out the pending
  buy state, since several failure paths (insufficient funds, inventory
  full) send nothing at all. Optionally listen for `OP_ShopEndConfirm`
  (0-byte, opcode `0x3196`) and chat `Message`/`MessageString` packets as
  secondary failure signals, but they are not guaranteed for every failure
  case (insufficient funds is fully silent).
- Do not spend coin or log "Bought item" at SEND time (the bug being
  fixed) — gate both on receiving and matching this reply.
