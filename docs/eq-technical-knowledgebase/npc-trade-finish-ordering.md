# NPC hand-in: OP_FinishTrade packet ordering, range checks, and payload (RoF2)

Related: `quest-turnin-and-inventory-desync.md` (item delivery mechanics — read that first for
how `PushItemOnCursor`/`ItemPacketLimbo` work). This note answers a narrower question that doc
doesn't cover: **exactly when, relative to `OP_FinishTrade`, does a returned (non-kept) item
packet get queued to the client** — because that determines whether a client can trust
"inventory state at the moment `OP_FinishTrade` arrives" to detect a failed give.

---

## 1. `OP_FinishTrade` is sent to the client for EVERY NPC trade-accept-click — kept or not

Confirmed `EQEmu/zone/client_packet.cpp:15486-15498` (`Client::Handle_OP_TradeAcceptClick`, the
`else if (with)` branch taken when trading with a non-Client `Mob` — i.e. an NPC or Bot):

```cpp
else if (with) {
    auto outapp = new EQApplicationPacket(OP_FinishTrade, 0);
    QueuePacket(outapp);
    safe_delete(outapp);
    if (with->IsNPC()) {
        FinishTrade(with->CastToNPC());
    }
    else if (with->IsBot())
        with->CastToBot()->FinishTrade(this, Bot::BotTradeClientNormal);
    trade->Reset();
}
```

There is **no branch anywhere that skips `OP_FinishTrade`** for a rejected/returned hand-in. The
server does not distinguish "NPC kept everything" vs "NPC quest logic returned some/all of it"
vs "NPC has no quest handler at all" at the point `OP_FinishTrade` is queued — `OP_FinishTrade`
fires unconditionally as soon as the client clicks Accept on an NPC trade, *before* the NPC-side
consume/return logic has even run (see §3). It is purely "the trade UI session is over," not
"your items were accepted."

## 2. How a returned item comes back — `PushItemOnCursor` → `OP_ItemPacket` (slot 33, `ItemPacketLimbo`)

Confirmed `EQEmu/zone/npc.cpp:4681` (`NPC::ReturnHandinItems`, called from
`Client::FinishTrade`'s NPC branch, `EQEmu/zone/trading.cpp:670-677`):

```cpp
c->PushItemOnCursor(*i.item, true);
```

`PushItemOnCursor` (`EQEmu/zone/inventory.cpp:1035-1048`) sends
`SendItemPacket(EQ::invslot::slotCursor, &inst, ItemPacketLimbo)` — **opcode `OP_ItemPacket`**
(`inventory.cpp:3017`, `opcode = (packet_type==ItemPacketViewLink) ? OP_ItemLinkResponse :
OP_ItemPacket`), **not** `OP_MoveItem`, **not** a fresh `OP_CharInventory`. RoF2
`slotCursor = 33` (`EQEmu/common/patches/rof2_limits.h:122-158`, `InventorySlots` enum starting
at `slotCharm=INULL=0`, counting through `slotGeneral10`; `slotCursor` is the 34th entry = 33).

Money handed in but not consumed goes back via `Client::AddMoneyToPP`
(`EQEmu/zone/npc.cpp:4715` inside `ReturnHandinItems`) — a separate `OP_MoneyUpdate`-class path,
not an item packet (see `quest-turnin-and-inventory-desync.md` §1 for the pointer to that code;
not re-verified line-by-line here).

**When does the return actually happen (i.e. is anything returned at all)?** The default rule
`RuleB(Items, AlwaysReturnHandins)` is `true` (`EQEmu/common/ruletypes.h:1160`). The catch-all in
`Client::FinishTrade`'s NPC branch (`trading.cpp:648-677`) calls `ReturnHandinItems` whenever the
NPC's `EVENT_TRADE` quest sub either doesn't exist, doesn't run (e.g. the NPC has aggro on the
player — `has_aggro` gates the `EventNPC(EVENT_TRADE, ...)` call at `trading.cpp:622-623`), or
runs but doesn't consume every handed-in item/coin (`NPC::CheckHandin`,
`EQEmu/zone/npc.cpp:4241` tracks what's still outstanding in `m_hand_in`). So on a stock/default
server: no quest handler for that item → item comes back on the cursor via `OP_ItemPacket`.

## 3. CRITICAL ORDERING: `OP_FinishTrade` is queued to the wire BEFORE the returned-item packet

This is the answer to the caller's core question. Trace, in call order, for an NPC trade-accept:

1. `Handle_OP_TradeAcceptClick` constructs `OP_FinishTrade` (0-byte) and calls
   **`QueuePacket(outapp)`** — `client_packet.cpp:15488-15489` — **before** anything about the
   handed-in items has been touched.
2. *Then* `FinishTrade(with->CastToNPC())` runs (`client_packet.cpp:15492`), which pops the trade
   items (`trading.cpp:518-521`), runs quest logic, and — only if something wasn't consumed —
   calls `ReturnHandinItems` → `PushItemOnCursor` → **`SendItemPacket(...)` →
   `FastQueuePacket(&outapp)`** (`inventory.cpp:1043`, `inventory.cpp:3025`).

Both `QueuePacket` (step 1) and `FastQueuePacket` (step 2, reached via `SendItemPacket`) resolve
to the **same underlying connection object**: `Client::QueuePacket` →
`eqs->QueuePacket(app, ack_req)` (`EQEmu/zone/client.cpp:1179-1180`); `Client::FastQueuePacket` →
`eqs->FastQueuePacket(...)` (`client.cpp:1192-1193`). Both push onto the same `EQStream`'s
reliable outbound queue for this client, in call order, and RoF2's OP_Combined /
sequence-numbered reliable transport delivers same-stream packets to the client **in the order
they were queued** (see `eqstream-reliable-retransmit.md` for the general reliability/ordering
guarantee — not re-derived here).

**Conclusion: the client receives `OP_FinishTrade` strictly BEFORE any returned-item
`OP_ItemPacket`/cursor-restore packet, for the NPC-trade path.** A client that snapshots its
mirrored inventory/cursor state exactly when `OP_FinishTrade` arrives will see the trade-window
slots already emptied (server did that with `client_update=false`,
`m_inv.PopItem`/`DeleteItemInInventory` at `trading.cpp:519-521`, no packet) but will **not yet**
see the returned item back on the cursor — that arrives in a follow-up packet moments later (same
tick, same TCP-like reliable stream, but a distinct, later-sequenced packet). **You cannot decide
"kept vs. returned" by inspecting inventory state synchronously at the moment `OP_FinishTrade` is
processed; you must wait briefly afterward (or watch for the specific follow-up `OP_ItemPacket`
at cursor slot 33) before concluding the NPC kept the item.**

### Contrast: player-to-player trade has the OPPOSITE ordering

For a Client↔Client trade (`client_packet.cpp:15433-15484`), both sides' `FinishTrade(...)` calls
happen **first** (`client_packet.cpp:15474-15479`, inventory fully transferred/returned
synchronously, including any `PushItemOnCursor` calls for failed transfers), and **only after
that** are the two `OP_FinishTrade` packets queued (`client_packet.cpp:15481-15483`). So for
player trades, by the time `OP_FinishTrade` arrives, the inventory-mirror side effects have
already been queued ahead of it — the reverse of the NPC case. **Do not assume the same
ordering rule applies to both trade partner types.**

## 4. No server-side distance/range check on NPC trade accept or request

Searched `EQEmu/zone/trading.cpp`, `EQEmu/zone/client_packet.cpp`
(`Handle_OP_TradeRequest:15675-15721`, `Handle_OP_TradeAcceptClick:15428-15502`), and
`EQEmu/zone/npc.cpp` (`CheckHandin`, `ReturnHandinItems`) for any `Dist`/`InRange`/range-rule
gate on the trade itself: **there is none**. `Handle_OP_TradeRequest` only rejects an
untargetable body-type-11 mob (`client_packet.cpp:15594-15596`) and whether the NPC
`IsEngaged()` (won't start a trade session with an NPC that's in combat,
`client_packet.cpp:15611`). No distance constant, no `RuleI/RuleR` trade-range rule exists in
`EQEmu/common/ruletypes.h` (grepped for `Trade` — only tradeskill-unrelated rules present, e.g.
`ruletypes.h:147-158`), and no `850`-unit (or any) trade-distance literal appears anywhere in
`zone/*.cpp`/`common/*.h`.

**Inferred, not verified against the client decompile:** the real RoF2 client's trade window is
almost certainly closed/blocked client-side once the target mob exceeds some max-target/consider
range (a UI-side restriction), and *that* is what the caller is observing as "out of range
~850 units" — it is not a server-enforced trade rule. If eqoxide needs the exact client-side
range constant, that requires checking `everquest_rof2/decompiled/ghidra/eqgame.exe.c` around the
trade-window/target-range logic (not done here — out of scope for this server-source
investigation) or the "too far away" client message triggers, which are UI-local and don't
round-trip through the server for a plain trade with an NPC that's still in range of
`OP_TradeRequest`'s `entity_list.GetMob` lookup. If the player moves out of the NPC's zone/vis
radius mid-trade, the more likely server-visible failure mode is the NPC's `EVENT_TRADE` sub
never firing usefully (e.g. `has_aggro` or the quest script's own `$client->Distance($npc)`
check inside Perl/Lua, which is script-defined, not engine-enforced) — that still flows through
the same "was it consumed" catch-all in §2, i.e. `ReturnHandinItems`, same ordering as §3.

## 5. `OP_FinishTrade` payload: zero bytes, no discriminator

Confirmed both NPC and Client branches construct it identically:
`new EQApplicationPacket(OP_FinishTrade, 0)` — `client_packet.cpp:15481` (client-client) and
`client_packet.cpp:15488` (NPC/Bot). **0-byte packet.** There is no struct, no success/failure
flag, no item count. The client cannot learn accepted-vs-returned from `OP_FinishTrade` itself
under any circumstance — that information only exists implicitly in whatever
item/money/delete packets do or don't follow it.

---

## Recommendation for eqoxide

To detect "did the NPC actually keep the item" for an honesty-correct client:

1. **Do not conclude "kept" the instant `OP_FinishTrade` arrives.** At that moment the trade
   slots are already cleared from the mirror (correct — the server did pop them), but a return is
   still in flight if one is coming.
2. **Arm a short post-`OP_FinishTrade` watch window** (a few hundred ms to a couple seconds is
   generous given same-tick reliable delivery) for a follow-up `OP_ItemPacket` whose parsed
   `ItemPacketLimbo`/`ItemPacketSummonItem` type (0x6A) targets slot 33 (cursor) and whose item id
   matches (or, conservatively, any such packet at all) — that is the "NPC didn't keep it, it's
   back on your cursor" signal. See `quest-turnin-and-inventory-desync.md` §1 for the exact
   opcode/slot/packet-type values and the "one reward at a time via the cursor deque" caveat.
3. **Money**: if the give included coin and no corresponding `OP_MoneyUpdate`-class adjustment
   restoring it is observed, treat as kept (mirror of the item case) — not independently verified
   here, flag as a follow-up if precise plat/gold/silver/copper accounting matters.
4. **Do not build a distance/range pre-check into the client's trade-attempt logic based on a
   server-enforced ~850 unit rule** — no such server rule exists (§4). Any range gating is either
   client-UI-local (RoF2 client itself) or embedded in the individual NPC's quest script, and in
   the latter case it surfaces only via the normal "was it consumed" return-item signal in step 2,
   with the same ordering guarantees.
5. **Trade-type asymmetry**: if eqoxide's honesty check is reused for player-to-player trades,
   invert the assumption — for Client↔Client trades the inventory-affecting side effects are
   already queued *before* `OP_FinishTrade`, so by the time `OP_FinishTrade` arrives for a P2P
   trade, checking inventory state synchronously IS valid (opposite of the NPC case, §3).
