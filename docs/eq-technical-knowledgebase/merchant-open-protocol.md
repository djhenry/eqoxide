# Merchant open (OP_ShopRequest command=1) — RoF2 server reply protocol

## Request/echo struct

`MerchantClick_Struct`, `EQEmu/common/eq_packet_structs.h:2135-2145` (approx —
verify exact field offsets if serializing):

```
/*000*/ uint32 npc_id;
/*004*/ uint32 player_id;
/*008*/ uint32 command;     // 1=open, 0=cancel/close
/*012*/ float  rate;        // cost multiplier, doesn't work anymore
/*016*/ int32  tab_display; // bitmask b000 none, b001 Purchase/Sell, b010 Recover, b100 Parcels
/*020*/ int32  unknown020;
```

`enum MerchantActions { Close = 0, Open = 1 }` —
`EQEmu/common/eq_packet_structs.h:2147-2150`.

Opcode `OP_ShopRequest = 0x4fed`, `EQEmu/utils/patches/patch_RoF2.conf:469`
(same opcode value used for both the client's open request and the server's
echo — direction distinguishes them, same pattern as `OP_ShopPlayerBuy`, see
`merchant-buy-protocol.md`).

## Handler: `Client::Handle_OP_ShopRequest`

`EQEmu/zone/client_packet.cpp:14589-14708`. Not RoF2-specific except one
branch (parcel tab bit, RoF2-only); the open/close signalling logic below is
shared across all client versions.

### Case 1 — genuine merchant, in range, all checks pass → OPEN confirmed

Falls through the whole function to the unconditional send at
`client_packet.cpp:14667-14690`:
```
mco->npc_id      = mc->npc_id;
mco->player_id   = 0;              // NOT echoed as the player's real id — always 0
mco->command     = action;         // MerchantActions::Open == 1
mco->tab_display = tabs_to_display;
mco->rate        = ...             // price-mod rate
```
Then, only `if (action == MerchantActions::Open)`
(`client_packet.cpp:14693-14705`): `BulkSendMerchantInventory(...)`,
`SetMerchantSessionEntityID(tmp->GetID())`, optional `SendBulkParcels()`,
optional `EVENT_MERCHANT_OPEN` quest event.

**Confirmed: caller's belief is correct** — an `OP_ShopRequest` echo with
`command=1` is the open confirmation, followed immediately by the merchant
inventory dump packet(s).

### Case 2 — genuine merchant (Class::Merchant), in range, faction KOS/dubious

**Correction to caller's hypothesis: this is NOT `OP_ShopEndConfirm`.**
`SendMerchantEnd()` (which sends the empty `OP_ShopEndConfirm`,
`client.cpp:13276-13287`) is **never called anywhere inside
`Handle_OP_ShopRequest`** — confirmed by grep over
`client_packet.cpp:14589-14709`, zero hits for `SendMerchantEnd`/
`ShopEndConfirm`.

What actually happens: the faction check at `client_packet.cpp:14648-14656`
does **not** early-return. It downgrades a local `action` variable
(initialized to `MerchantActions::Open` at `:14623`) to
`MerchantActions::Close` and calls `MerchantRejectMessage(tmp,
primaryfaction)` (`:14654`, impl `client.cpp:8465-...`), which sends a chat
`SayString`/`MessageString` (e.g. `WONT_SELL_DEEDS1..6`,
`WONT_SELL_NONSTDRACE*`, etc. — from the merchant NPC, a normal chat opcode,
not a merchant-protocol opcode) — this is a side-effect message, not the
open/close confirmation itself.

Execution then falls through to the **same** unconditional send at
`:14667-14690`, but now with `mco->command = action` = `MerchantActions::Close`
== **0**. `BulkSendMerchantInventory`/`SetMerchantSessionEntityID` are
skipped because of the `if (action == MerchantActions::Open)` guard at
`:14693`.

So: **`OP_ShopRequest` echo IS sent, with `command=0`** (the caller's
alternate guess was right; the `OP_ShopEndConfirm` guess was wrong for this
path). The same `command=0` echo (via the same fallthrough + same `action`
downgrade mechanism) also covers: merchant `IsEngaged()` (`:14638-14641`,
+ `MERCHANT_BUSY` message), caster `GetFeigned()`/`IsInvisible()`
(`:14643-14646`, + plain `Message`), merchant `Charmed()` (`:14658-14660`,
no message), and merchant not open (`!IsMerchantOpen()`, `:14662-14665`, +
`SayString(MERCHANT_CLOSED_ONE..THREE)`). All five conditions share this one
`command=0` reply path — they are NOT distinguishable from each other via the
`OP_ShopRequest` echo alone; only the accompanying (or absent) chat message
differs, and Charmed sends no message at all.

### Case 3 — target is not a merchant NPC at all (no Class::Merchant)

`client_packet.cpp:14605-14607`:
```c
if (tmp == 0 || !tmp->IsNPC() || tmp->GetClass() != Class::Merchant) {
    return;
}
```
**Confirmed: totally silent.** Zero packets of any kind — not an
`OP_ShopRequest` echo, not `OP_ShopEndConfirm`, not a chat message. Bare
`return`. This is the exact gap behind eqoxide issue #479: nothing
distinguishes "server ignored an invalid request" from "reply still in
flight."

### Case 4 — genuine merchant NPC, but out of range

`client_packet.cpp:14609-14612`:
```c
// you have to be somewhat close to them to be properly using them
if (DistanceSquared(m_Position, tmp->GetPosition()) > USE_NPC_RANGE2) {
    return;
}
```
**Confirmed: also totally silent, zero packets** — same "nothing sent"
behavior as case 3, but it is a **separate early-return statement**
(`:14610-14612`, distinct from `:14605-14607`). Both are silent for the same
reason (bare `return` before any packet is constructed) but they are
different code paths/line ranges, not a shared branch.

### Bonus case — merchant NPC with `MerchantType == 0` (misconfigured/no wares)

`client_packet.cpp:14614, 14623-14636`: if `tmp->CastToNPC()->MerchantType`
resolves to 0 (no wares list assigned in `merchant_id`/`merchantlist`
tables), the handler takes a **separate early branch** that unconditionally
sends the `OP_ShopRequest` echo with `command = MerchantActions::Open` (== 1,
hardcoded, not gated by faction/engaged/feigned/charmed/open-flag checks —
those checks are below this branch and never reached) and then `return`s
*without* calling `BulkSendMerchantInventory` or `SetMerchantSessionEntityID`.
Net effect: the client-side merchant window opens (client believes
`command=1` = success) but the server never populates it and never marks a
merchant session active server-side. This is a distinct "NPC has
`Class::Merchant` but is data-misconfigured" case, different from cases 1-4
above — worth a defensive timeout in eqoxide even on a `command=1` echo if no
follow-up inventory packet(s) arrive.

## Recommendation for eqoxide (issue #479)

- `apply_shop_request`/the merchant-open flow should branch on the echoed
  `command` field, not merely presence of a reply:
  - `command == 1` (Open): success, proceed to await
    `BulkSendMerchantInventory`-derived packets.
  - `command == 0` (Close): **explicit negative ack** — map to a real
    "refused" outcome (409-style), and note this covers 5 different
    server-side reasons (faction, engaged, feigned/invis, charmed, merchant
    closed) that are NOT distinguishable from the opcode alone — surface
    whatever chat `Message`/`MessageString` arrived alongside it (if any) as
    the human-readable reason, but treat Charmed as "refused, no reason
    given."
- For "no reply arrives at all" (covers case 3: not-a-merchant, and case 4:
  out-of-range, both bare silent `return`s with zero packets,
  `client_packet.cpp:14605-14607` and `:14610-14612`): this is genuinely
  unknowable-by-protocol-design, not a bug in eqoxide's read of the wire.
  eqoxide's existing `packet_handler.rs` comments describing this as silent
  are **correct and confirmed**. The honest fix for #479 is a client-side
  **timeout-based negative signal**: if the caller sent `OP_ShopRequest`
  command=1 and no `OP_ShopRequest` echo (of either command value) arrives
  within N ms, resolve the pending request as "unconfirmed/no response"
  rather than leaving the HTTP call hanging or silently returning 200. This
  is a `202 Unconfirmed`-shaped outcome, not a `409 Refused` one — the server
  gave zero information, so eqoxide cannot claim to know *why* it failed,
  only that it didn't open.
- Do not conflate the `command=0` case with `OP_ShopEndConfirm`
  (`0x3196`/`opcode` — empty struct, `client.cpp:13276-13287`).
  `OP_ShopEndConfirm` is exclusively a buy-path (and post-open session
  teardown) signal per `merchant-buy-protocol.md`; it is never sent from
  `Handle_OP_ShopRequest`. If eqoxide's HTTP handler currently listens for
  `OP_ShopEndConfirm` as a possible "open refused" signal, that listener is
  dead code for the open path specifically — remove/relabel that assumption.
- Recommended state machine for `POST /v1/merchant/open`:
  1. Send `OP_ShopRequest` command=1, start timeout.
  2. On `OP_ShopRequest` echo `command=1` (+ inventory packets): 200 open.
  3. On `OP_ShopRequest` echo `command=0`: 409 Refused (merchant declined —
     faction/engaged/invis/charmed/closed, not further distinguishable).
  4. On timeout with no echo: 202/504-style "no response" — NOT 200. This is
     the actual bug in #479 (a non-merchant target currently falls through to
     a default-200 because nothing ever arrives to flip it to an error).
  5. Treat "echo command=1 but no inventory packets follow within a second
     timeout" as an edge case (misconfigured `MerchantType==0` NPC,
     `client_packet.cpp:14614,14623-14636`) — optionally surface as "opened
     empty" rather than a hard error, since the server did send `command=1`.
