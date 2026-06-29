# Inventory view desync after quest turn-in / cursor move

**Summary:** The client's `/v1/observe/inventory` snapshot does not apply the item changes
caused by a `/v1/interact/give` quest turn-in or a cursor-sourced `/v1/inventory/move`. The view
goes stale (still shows the given-away item, hides the reward's real slot), and a
follow-up move computed against that stale view corrupts the snapshot into phantom
items. The server's inventory stays correct throughout — no item loss.

**Severity:** Medium (purely client-side display/state; no data loss, but it makes
inventory automation unreliable and can mislead an agent into corrupting its own
view with further moves).

**Zone / area:** Inventory state tracking (`packet_handler` / `gameplay` inventory
snapshot). Observed in `neriakc` doing the Dark Elf Shadow Knight guild turn-in.

## Steps to reproduce
1. Log in a character with a quest turn-in that returns an item to the cursor
   (e.g. Mordeth, Dark Elf SK in `neriakc`, with the Tattered Note 18757 in bags).
   Discover the API port from the launch (Mordeth used port 8766).
2. `GET /v1/observe/inventory` — note the note is in a bag slot, and there is no reward yet.
3. Walk to the guildmaster and turn the note in:
   `POST /v1/navigate/goto {"name":"Nezzka_Tolax000"}`, then
   `POST /v1/interact/give {"npc":"Nezzka_Tolax000","from":24}` (24 = wire slot of the note).
   Trade completes; the Black Training Tunic (13586) is awarded to the cursor.
4. `GET /v1/observe/inventory` — observe the stale view (bug, step 1 below).
5. Equip the tunic: `POST /v1/inventory/move {"from":30,"to":17}` (30 = cursor,
   17 = wire chest). `GET /v1/observe/inventory` — observe no change in the view (bug, step 2).
6. Issue another cursor move, e.g. `POST /v1/inventory/move {"from":30,"to":27}`.
   `GET /v1/observe/inventory` — observe phantom items (bug, step 3).
7. Cross-check the truth: read the server DB (read-only):
   `SELECT slot_id,item_id FROM inventory WHERE character_id=<id> ORDER BY slot_id;`

## Expected
After each operation, `GET /v1/observe/inventory` reflects the real, server-side state: the
turned-in note removed, the reward shown at its true slot, and the equipped tunic
in the chest slot.

## Actual (observed values, Mordeth / charid 13)
- **After `/v1/interact/give`:** `/v1/observe/inventory` still listed the Tattered Note (18757) in the
  bags and did not show the note as consumed. DB truth: note **gone** (consumed).
- **After equip `30→17`:** `/v1/observe/inventory` still showed the tunic on the cursor
  (slot 30); the chest slot looked empty. DB truth: tunic (13586) **equipped at
  chest** (DB slot 17). So the equip actually succeeded server-side; only the view
  was wrong.
- **After a further `30→27` move:** the tunic (13586) **vanished from the view
  entirely** and two phantom `Copper Coin` (22292) entries appeared at slots 27
  and 30; `count` rose to 9. DB truth: inventory intact, tunic still equipped at
  chest, no Copper Coins.

## Diagnosis notes
- The 3D model rendered the equipped tunic correctly (front-on `/v1/observe/frame`), matching
  the server, while `/v1/observe/inventory` did not — so the desync is specific to the
  inventory snapshot, not the wearchange/render path.
- DB (`inventory` table for `character_id=13`) is authoritative and was correct at
  every step; this rules out any server-side item loss or duplication.
- Wire slots used were correct per the code's own map (`http.rs` post_move docs:
  17=Chest, 30=cursor) — the move packets are well-formed; the very first equip
  even applied server-side. The failure is in *consuming the server's resulting
  item updates* (post-trade `OP_MoveItem` / item-delete / cursor updates) into the
  published `/v1/observe/inventory` snapshot.

## Root cause (confirmed)
`gs.inventory` was only ever mutated by full `OP_CharInventory` loads and single-item
`OP_ItemPacket` upserts; item *moves* were never applied. `OP_MoveItem` is **send-only** —
EQEmu inventory moves are client-authoritative: the server validates the client's `OP_MoveItem`,
updates the server inventory, and sends **no echo** (the real client already moved the item in its
own UI). eqoxide has no such UI, so the move was lost from its snapshot. On a quest turn-in the
server takes the handed-in items via `m_inv.PopItem` (`zone/trading.cpp`) — also with no per-item
packet — while returned/reward items come back as `OP_ItemPacket` on the cursor. So the note
lingered, the reward's slot was wrong, and a move computed against the stale view corrupted it into
phantom items.

## Fix
- `GameState::move_item(from,to)` mirrors a whole-item move locally (swap if the destination is
  occupied, no-op from empty), wired into the `/v1/inventory/move` send and both give-flow moves.
- `GameState::clear_trade_slots()` drops items left in the NPC trade slots (3000-3007) on
  `OP_FinishTrade`. Reward/returned items keep arriving via `OP_ItemPacket` (already handled).
See `src/game_state.rs`, `src/eq_net/navigation.rs`, `src/eq_net/packet_handler.rs`.

## Verification
Live (Mordeth, neriakc, real wire slots): unequip/re-equip (17↔25), bag move (27→28), and a
**cursor** round-trip (28↔33) all reflect in `/v1/observe/inventory` immediately with no phantom items and no
server desync; a second move no longer corrupts the view. Unit tests cover move relocate/swap/guard
and trade-slot clearing. The end-to-end accepted turn-in (note consumed → `OP_FinishTrade` → slot
cleared) is unit-tested + derived from the server source but was not re-run live — the test
character had already consumed its note and the server was too DB-saturated to create a fresh one.

## Status
Fixed
