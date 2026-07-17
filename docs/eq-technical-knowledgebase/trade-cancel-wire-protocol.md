# Trade cancel: OP_CancelTrade wire protocol (RoF2)

Related: `npc-trade-finish-ordering.md` (the accept/`OP_FinishTrade` path — read that for the
"kept vs returned" detection problem on the accept side; this note covers the cancel side, which
is a parallel/mutually-exclusive path, not a variant of the accept flow).

## 1. Opcode

`OP_CancelTrade` — confirmed RoF2 wire value **`0x354c`**.
`EQEmu/utils/patches/patch_RoF2.conf:451`.

Full trade opcode block for reference (`patch_RoF2.conf:446-458`):
```
OP_TradeRequest=0x77b5
OP_TradeAcceptClick=0x69e2
OP_TradeRequestAck=0x14bf
OP_TradeCoins=0x4206
OP_FinishTrade=0x3993
OP_CancelTrade=0x354c
OP_TradeMoneyUpdate=0x68c2
OP_TradeBusy=0x5505
OP_FinishWindow=0x7349
OP_FinishWindow2=0x40ef
```

No `OP_TradeReset`/`OP_TradeCancel`/`OP_CANCEL_TRADE` exists anywhere in the RoF2 opcode table or
`emu_oplist.h` — `OP_CancelTrade` is the only cancel opcode.

## 2. Struct — 8 bytes, `{ fromid: u32, action: u32 }` — NOT `TradeRequest_Struct`

Common definition `EQEmu/common/eq_packet_structs.h:2598-2604`; RoF2 patch struct is an identical
layout at `EQEmu/common/patches/rof2_structs.h:2712-2719`:
```c
struct CancelTrade_Struct {
/*00*/ uint32 fromid;
/*04*/ uint32 action;
/*08*/
};
```
RoF2 `ENCODE(OP_CancelTrade)` (`EQEmu/common/patches/rof2.cpp:977-985`) is a direct 1:1 field
passthrough (`OUT(fromid); OUT(action);`), no reordering/translation. There is no
`DECODE(OP_CancelTrade)` override in `rof2.cpp` — client→server decode uses the default
struct-copy path since the wire layout already matches the internal struct.

`TradeRequest_Struct` (`to_mob_id`/`from_mob_id`, also 8 bytes, `rof2_structs.h:2698-2702`) is a
**different struct** used only for `OP_TradeRequest`/`OP_TradeRequestAck`. Same size, different
field semantics — do not conflate.

`action` is not interpreted meaningfully by the server on receipt (see §3) — one real send site
(`EQEmu/zone/client.cpp:886-890`, logout-triggered cancel, not the mid-trade UI cancel) populates
it with an unrelated enum (`groupActUpdate`) as a placeholder. Treat as opaque/pass-through.

## 3. Server behavior on receipt — `Client::Handle_OP_CancelTrade`

`EQEmu/zone/client_packet.cpp:4317-4356`. Size-validated against `sizeof(CancelTrade_Struct)` (8
bytes) or dropped with a log error.

- Looks up `trade->With()`. If found (client or non-client mob):
  - Overwrites `msg->fromid = with->GetID()` and **relays the same raw packet** to the trade
    partner (`with->CastToClient()->QueuePacket(app)` for a client partner, line 4331) — this IS
    the "ack"/notification mechanism; there's no separate cancel-ack opcode.
  - Calls `FinishTrade(this)` — passes **self**, not the partner. Inside
    `Client::FinishTrade` (`EQEmu/zone/trading.cpp:319+`), items sitting in the local trade-window
    inventory slots get returned to the *caller's own* inventory (bags/stackables/misc via
    `PutItemInInventory`, falling back to `PushItemOnCursor`/`DropInst` if full) — NOT
    transferred to the partner. Comment confirms intent: `// Put trade items/cash back into
    inventory` (`client_packet.cpp:4333`).
  - Calls `trade->Reset()` (`EQEmu/zone/trading.cpp:60-65`) — zeroes `state`, `with_id`,
    `pp/gp/sp/cp`.
  - RoF2-only: also cancels any parcel-merchant engagement if
    `RuleB(Parcel, EnableParcelMerchants)` (lines 4345-4348) — no Titanium analog.
- **Unconditionally**, regardless of whether a partner was found, sends two 0-byte packets back
  to the cancelling client only (not the partner) to close its trade UI (lines 4350-4354):
  `OP_FinishWindow` (`0x7349`) then `OP_FinishWindow2` (`0x40ef`).
- `OP_FinishTrade` (`0x3993`) is **never** sent as part of cancel — it's exclusive to the mutual-
  accept completion path.

## 4. Relationship to the accept/complete sequence

Normal flow:
1. `OP_TradeRequest` initiator→server→target (`Handle_OP_TradeRequest`, `client_packet.cpp:15675-15720`).
2. `OP_TradeRequestAck` target→server→initiator; `trade->Start()` runs here
   (`client_packet.cpp:15722-15747`) — trade session officially begins.
3. Item placement uses the **generic inventory-move opcode** (`OP_MoveItem`) targeting the
   trade-window inventory slots. RoF2 trade slots: `TRADE_BEGIN=3000`, `TRADE_SIZE=8`,
   `TRADE_END=3007` (`EQEmu/common/patches/rof2_limits.h:80,174-175`). There is **no dedicated
   `OP_TradeMoveItem` opcode** — grepped `patch_RoF2.conf` and `emu_oplist.h`, found none. Coins
   go via `OP_TradeCoins` (`0x4206`).
4. `OP_TradeAcceptClick` from each side (`Handle_OP_TradeAcceptClick`,
   `client_packet.cpp:15428-15502`). Once both sides' `trade->state` match (`TradeAccepted`), the
   server runs `FinishTrade()` on **each side against the real counterpart** (genuine transfer,
   unlike cancel's self-pass), resets both `Trade` objects, then queues `OP_FinishTrade` (0-byte)
   to **both** clients.
5. `OP_CancelTrade` is a **parallel exit path, not a predecessor of `OP_FinishTrade`.** Can arrive
   any time after step 2, before mutual accept completes. Never triggers `OP_FinishTrade`.
   Converges with the accept path only in that both ultimately call `Client::FinishTrade()` +
   `Trade::Reset()` — accept passes the real counterpart (item transfer), cancel passes `this`
   (item return to self).

## Recommendation for eqoxide

- Implement `CancelTrade_Struct { fromid: u32, action: u32 }`, opcode `0x354c`. `action` can be 0.
- After sending `OP_CancelTrade`: do NOT wait for `OP_FinishTrade` (it will never come). Expect
  `OP_FinishWindow` (`0x7349`) then `OP_FinishWindow2` (`0x40ef`), both 0-byte, as the "trade UI
  closed" signal — treat these as terminal for the cancel flow.
- Placed-but-not-yet-transferred items return to your own inventory via ordinary
  inventory-mutation packets (no distinct "trade cancelled, here's your stuff back" packet type);
  if precise detection is needed, apply the same cursor/`ItemPacketLimbo` watch technique
  documented in `npc-trade-finish-ordering.md` §2.
- Cancel handling is identical regardless of partner type (client/NPC/bot) — no need to branch.
- Not verified here (out of scope for this question): the RoF2 client-side (`eqgame.exe`) trigger
  conditions for sending `OP_CancelTrade` (ESC vs explicit Cancel button, any client-side range
  check). Would require `everquest_rof2/decompiled/ghidra/eqgame.exe.c`.
