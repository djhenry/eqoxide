# Inventory view desync after quest turn-in / cursor move

**Summary:** The client's `/inventory` snapshot does not apply the item changes
caused by a `/give` quest turn-in or a cursor-sourced `/inventory/move`. The view
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
2. `GET /inventory` — note the note is in a bag slot, and there is no reward yet.
3. Walk to the guildmaster and turn the note in:
   `POST /goto {"name":"Nezzka_Tolax000"}`, then
   `POST /give {"npc":"Nezzka_Tolax000","from":24}` (24 = wire slot of the note).
   Trade completes; the Black Training Tunic (13586) is awarded to the cursor.
4. `GET /inventory` — observe the stale view (bug, step 1 below).
5. Equip the tunic: `POST /inventory/move {"from":30,"to":17}` (30 = cursor,
   17 = wire chest). `GET /inventory` — observe no change in the view (bug, step 2).
6. Issue another cursor move, e.g. `POST /inventory/move {"from":30,"to":27}`.
   `GET /inventory` — observe phantom items (bug, step 3).
7. Cross-check the truth: read the server DB (read-only):
   `SELECT slot_id,item_id FROM inventory WHERE character_id=<id> ORDER BY slot_id;`

## Expected
After each operation, `GET /inventory` reflects the real, server-side state: the
turned-in note removed, the reward shown at its true slot, and the equipped tunic
in the chest slot.

## Actual (observed values, Mordeth / charid 13)
- **After `/give`:** `/inventory` still listed the Tattered Note (18757) in the
  bags and did not show the note as consumed. DB truth: note **gone** (consumed).
- **After equip `30→17`:** `/inventory` still showed the tunic on the cursor
  (slot 30); the chest slot looked empty. DB truth: tunic (13586) **equipped at
  chest** (DB slot 17). So the equip actually succeeded server-side; only the view
  was wrong.
- **After a further `30→27` move:** the tunic (13586) **vanished from the view
  entirely** and two phantom `Copper Coin` (22292) entries appeared at slots 27
  and 30; `count` rose to 9. DB truth: inventory intact, tunic still equipped at
  chest, no Copper Coins.

## Diagnosis notes
- The 3D model rendered the equipped tunic correctly (front-on `/frame`), matching
  the server, while `/inventory` did not — so the desync is specific to the
  inventory snapshot, not the wearchange/render path.
- DB (`inventory` table for `character_id=13`) is authoritative and was correct at
  every step; this rules out any server-side item loss or duplication.
- Wire slots used were correct per the code's own map (`http.rs` post_move docs:
  17=Chest, 30=cursor) — the move packets are well-formed; the very first equip
  even applied server-side. The failure is in *consuming the server's resulting
  item updates* (post-trade `OP_MoveItem` / item-delete / cursor updates) into the
  published `/inventory` snapshot.

## Suspected root cause
(unconfirmed) The inventory snapshot is built from the initial `OP_CharInventory`
and is not incrementally updated by the server packets that follow a trade
turn-in and cursor/worn moves (e.g. `OP_MoveItem` echoes, item-delete, cursor
push/pop). Stale slots then feed the next move, producing phantom/garbage entries.
A fresh login (new `OP_CharInventory`) would resync the view to the correct state.

## Status
Open
