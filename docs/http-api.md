# HTTP API Reference

The client exposes a REST API on `http://127.0.0.1:<port>`. All endpoints are
available while the client is running. Request bodies are JSON; responses are
JSON or `image/png`.

**Port discovery (multi-instance).** The client binds the next free port starting
at `config.yaml` `http_port` (default 8765), scanning upward through 8765, 8766,
8767… So several instances (e.g. one per worktree) can run side by side without
colliding. On launch the client prints the port it bound to **stdout** as a single
parseable line:

```
API_PORT=8766
```

Capture it when you launch the client, e.g. `PORT=$(grep -m1 -oP 'API_PORT=\K[0-9]+' log)`.

This API is the primary way an agent script controls the client — the client
itself also has WASD keyboard controls and HUD buttons for the same actions.

---

## Camera

### `GET /camera`
Returns current camera state.

```json
{
  "azimuth": 45.0,
  "elevation": 30.0,
  "radius": 20.0,
  "focus": [100.0, 200.0, -25.0]
}
```

`focus` is GPU world space `[east, north, height]`.

### `POST /camera`
Set one or more camera parameters.

```json
{ "azimuth": 90.0, "elevation": 20.0, "radius": 15.0, "focus": [x, y, z] }
```

All fields optional; omit any to leave unchanged.

### `POST /camera/reset`
Reset camera to default orbit position.

---

## Frame Capture

### `GET /frame`
Returns the current rendered frame as a `image/png`. Times out after 2 s if
the renderer is not ready. Use this to visually verify changes.

```sh
curl http://127.0.0.1:8765/frame -o frame.png
```

---

## Navigation

### `POST /goto`
Walk to a destination. Three forms:

**By entity name** (fuzzy-matched against known entity positions):
```json
{ "name": "Guard Phaeton" }
```

**By map coordinates** (from a zone map file; map_x = server_y = east, map_y = server_x = north):
```json
{ "map_x": 150.0, "map_y": 200.0 }
```

**By raw server coordinates**:
```json
{ "x": 200.0, "y": 150.0, "z": -25.0 }
```

Navigation uses **A\* pathfinding** (`Collision::find_path`) that routes *around* walls, then
walks the resulting waypoints with collision-aware sliding (the nav tick fires every 150 ms).
If no path is found it falls back to a straight slide. Arrival is declared within ~2 units.
Limitations: can't cross water or doors/sealed pockets (doors aren't in the collision) — see
`collision-system.md` and `autonomous-play.md` §5.

### `POST /warp`
Teleport request `{ "x": …, "y": …, "z": … }`. **Anti-cheat capped** for non-GM characters (small
hops, rubber-banded, rejected through walls). Unreliable for real positioning — prefer `/goto`.

---

## Entities

### `GET /entities`
Returns all known entity positions as `{ "EntityName000": [x, y, z], … }`.
Coordinates are server-convention `(server_x, server_y, server_z)`.

Entity names include a trailing numeric suffix (e.g. `Guard_Phaeton000`).
Use `clean_entity_name()` (or `POST /hail` which does this automatically) to
strip digits and underscores for display.

---

## NPC Interaction

### `POST /hail`
Say "Hail, \<name\>" so a nearby NPC fires its hail/quest script. The NPC must
be within ~200 units (server-enforced say range).

```json
{ "name": "Guard Phaeton" }   // fuzzy-matched
{}                             // hail the nearest NPC
```

Returns `200 OK` with body `"hailing Guard Phaeton"` or `404` if no match.

### `POST /say`
Send arbitrary text on the Say channel. Use this for quest keyword follow-ups.

```json
{ "text": "shipment" }
```

### `POST /target`
Target a spawn by id and auto-send OP_Consider. Sends `OP_TARGET_MOUSE` (0x6c47, sets the
server-side combat target) + `OP_CONSIDER`. The consider reply arrives asynchronously and appears
in the message log.

```json
{ "id": 1234 }
```

Spawn ids come from `GET /entities` keys (the numeric suffix) or the HUD debug.

### `POST /target/name`
Target by NPC name (fuzzy-matched against loaded entities). Same effect as `/target` but resolves
the spawn id for you.

```json
{ "name": "a_rodent013" }
```

---

## Combat (auto-grind)

### `POST /attack`  /  `DELETE /attack`
Enable / disable **auto-attack** (`OP_AUTO_ATTACK` 1/0). While enabled, the nav thread auto-engages
and auto-retargets: it walks into melee range of the current target and **faces it each tick** (the
server only registers swings when facing — see `protocol-notes.md`), then on kill it retargets the
nearest reachable trash mob (`a_`/`an_` names) within 200u, idling for respawns if none are reachable.
This is the hands-free grinding loop. Verify kills via `/tmp/eq_client.log` (`has been slain`).

## Actions & Spells

These mirror the HUD **action grid** (bottom-center): every button writes the same request
slot these endpoints write, so an agent has full parity with a human clicking the grid.

### `GET /spells`
List the 9 memorized spell gems (from the player profile). Empty gems report `null`.

```json
{ "gems": [
  { "gem": 0, "spell_id": 200, "name": "Minor Healing" },
  { "gem": 1, "spell_id": 202, "name": "Courage" },
  { "gem": 5, "spell_id": null, "name": null }
] }
```

The gem `name` comes from `spells_us.txt` (from the original Titanium game client; path
via env `EQ_SPELLS_FILE`, default: the configured assets dir). If that file is absent,
`name` is `null` but `spell_id` still resolves.
Gems populate only after the `OP_PlayerProfile` arrives — poll until non-`null` after login.

### `POST /cast`
Cast a memorized gem (sends `OP_CastSpell`). Two forms:

```json
{ "gem": 0 }                          // cast gem 0 on current target (self if none)
{ "spell_id": 200, "target_id": 1234 } // resolve the gem holding spell 200, cast on 1234
```

`target_id` is optional; omitted → current target → self. `spell_id` must be one of the
memorized gems. The HUD shows a cast bar (driven by `OP_BeginCast`); a cast clears on
`OP_MemorizeSpell` (spellbar re-enable) or `OP_InterruptCast`.

### `POST /sit`  /  `POST /stand`
Sit or stand (sends `OP_SpawnAppearance` animation 110 / 100). Parameter-free.

### `POST /consider`
Consider a spawn (`OP_Consider`) to get its con color. Body `{ "id": <spawn_id> }`;
omit `id` to consider the current target.

## Combat (auto-grind)

### `POST /buy`
Buy an item from a merchant. Must be within **200u (3D)** of a loaded merchant.

```json
{ "merchant": "Fellweni", "slot": 4 }
```

`merchant` is fuzzy-matched by name; `slot` is the item's `merchantlist.slot` (from the EQEmu DB).
The nav sends `OP_ShopRequest` (open) then `OP_ShopPlayerBuy` (buy slot, qty 1). Needs coin in
`character_currency`. See `autonomous-play.md` §4.

### `POST /inventory/move`
Move an item between inventory slots — equip, unequip, or rearrange.

```json
{ "from": 23, "to": 19 }
```

`from`/`to` are Titanium **wire** slot ids: **0-21** worn equipment (13=Primary, 14=Secondary,
17=Chest, 19=Feet, …), **22-29** general inventory, **30** cursor, **251+** bag contents. The nav
sends `OP_MoveItem` (`MoveItem_Struct` 12b: `from_slot`, `to_slot`, `number_in_stack`=0 for a whole
item). To equip a bag item, move it to its worn slot (e.g. a bag item in general wire-slot 22 → worn
slot 19).

> **⚠ Wire slots ≠ DB slots.** These endpoints use the Titanium **wire** numbering, which is
> **one less** than the EQEmu `inventory.slot_id` for general slots: DB/server general slots **23-30**
> map to wire **22-29** (and DB cursor 33 → wire 30). An item sitting in `inventory.slot_id = 23`
> must be referenced as `from: 22`. Reading slot numbers straight from the DB and passing them as-is
> is off by one.

### `POST /give`
Hand a single inventory item to a (named) NPC to complete a quest **turn-in** (e.g. give a newbie
note to a guildmaster). Must be within trade range of a loaded NPC.

```json
{ "npc": "Mater", "from": 22 }
```

`npc` is fuzzy-matched by name (like `/buy` / `/target/name`); `from` is the **wire** slot of the
item to give (see the wire-slot warning above). The nav runs the Titanium trade-window handshake
across a few ticks: `OP_MoveItem` (item→cursor) → `OP_TradeRequest` → *wait for* `OP_TradeRequestAck`
→ `OP_MoveItem` (cursor→trade slot 3000) → `OP_TradeAcceptClick`; the server replies `OP_FinishTrade`
(with a ~3s ack timeout that aborts via `OP_CancelTrade`). On success the NPC's quest script consumes
the item (and may hand back a reward on the cursor); a rejected hand-in returns the item to the
cursor. Watch `/tmp/eq_client.log` for `give: turn-in complete (OP_FinishTrade)`. Verified live:
Durgan (Dwarf Rogue) handed the "Small, Folded Note" to guildmaster Mater and received the "Ruined
Miner's Tunic" in return. See `autonomous-play.md`.

### `GET /inventory`
The player's current inventory + equipment (published each tick).

```json
{ "count": 1, "items": [
  { "slot": 30, "item_id": 13516, "name": "Ruined Miner's Tunic*", "charges": 1, "icon": 678, "idfile": "IT63" }
] }
```

`slot` is the Titanium **wire** slot — the exact number to pass to `/give` and `/inventory/move`
(see the wire-slot warning above; it's one less than the EQEmu DB `slot_id` for general slots). Use
this to discover which slot holds an item before giving or equipping it. Verified live: returns the
cursor item (13516) at wire slot 30 (DB slot 33).

### `POST /loot`
Open a corpse and take all of its items, reusing the auto-loot pipeline (`OP_LootRequest` → echo each
`OP_LootItem` → `OP_EndLootRequest`). Must be near the corpse; looted items land in inventory
(`GET /inventory`).

```json
{ "id": 1141162142 }      // a specific corpse spawn id
{ "name": "a_rodent" }    // fuzzy-match a corpse name (corpses are "<mob>'s corpse")
{}                         // the nearest corpse
```

Returns `200` with the queued corpse, `404` if no corpse matches. The nav thread pushes the corpse
onto the loot queue; watch `/tmp/eq_client.log` for `loot: queued corpse_id=… (via POST /loot)` then
`auto-loot: sent OP_LootRequest`. You can only loot corpses you have rights to (your own kills);
others return a server denial. (The client also auto-loots your own kills without this call — `/loot`
is for looting a specific corpse on demand.)

### `GET /messages`
The in-game message log as machine-readable text (oldest→newest, last ~50 lines), published each
tick. **This is how an agent reads NPC dialogue** instead of OCR'ing the `/frame` HUD panel.

```json
{ "count": 2, "messages": [
  { "kind": "npc", "text": "Arias says, 'let's [escape] this dungeon.'", "keywords": ["escape"] },
  { "kind": "combat", "text": "a_rodent007 has been slain", "keywords": [] }
] }
```

Each line has a `kind` (the channel), the `text`, and any `[bracketed]` quest `keywords` extracted
from it (say them back with `POST /say` to advance dialogue quests). Channels:

| kind | source |
|------|--------|
| `npc` | NPC say + emotes (quest-giver replies) — **the dialogue channel** |
| `chat` | say/OOC/shout channels (incl. your own `/hail` echo) |
| `combat` | hits, misses, slays |
| `system` | eqstr formatted/simple server messages |
| `exp` / `zone` / `loot` / `trade` | XP, zone changes, loot, trades |

Filter with `?kind=npc` to get only NPC dialogue:

```sh
curl "http://127.0.0.1:$PORT/messages?kind=npc"
```

Verified live: hailing/fighting around Kaladim, NPC barks like `Exterminator Vin says, '…'` show up
as `kind:"npc"`; keyword extraction reuses the same `[bracketed]` splitter the HUD dialogue panel
uses. Workflow: `/hail` an NPC → `GET /messages?kind=npc` to read the reply → `POST /say` a keyword
to continue.

---

## Debug

### `GET /debug`
Returns camera + live player state:
```json
{ "camera": {…}, "player": { "zone": "qcat", "pos": [east, north, up],
  "heading_ccw": 0.0, "heading_cw": 0.0, "server_corrections": 0 } }
```
`server_corrections` rising = the server is rubber-banding the player (e.g. after a warp). There is
**no HP field** — read combat from the log and level/exp from the DB.

---

## Lifecycle

### `POST /exit`
Cleanly shuts down **this** client instance (`std::process::exit(0)`). Returns `200 "shutting down"`,
then exits ~150ms later so the response flushes. Use this to restart your own client to pick up a
rebuild — it targets only the instance on the port you call, so it won't kill another worktree's
client the way `pkill eq_renderer` would.
```
curl -X POST http://127.0.0.1:8766/exit
```

---

## Doors

### `GET /doors`
Returns all doors in the current zone.

```json
[
  { "door_id": 0, "name": "DOOR1", "x": 100.0, "y": 200.0, "z": -25.0,
    "heading": 45.0, "opentype": 5, "is_open": false },
  { "door_id": 1, "name": "PORT1414", "x": 150.0, "y": 250.0, "z": -30.0,
    "heading": 90.0, "opentype": 57, "is_open": true }
]
```

Each door has:
- `door_id` — per-zone door index (0–255)
- `name` — model filename (e.g., `DOOR1`, `BBDOOR1`)
- `x`, `y`, `z` — world position (server coordinates)
- `heading` — door orientation (EQ heading units)
- `opentype` — animation class; 57/58 are teleport portals
- `is_open` — current open/closed state

**Portal doors** (opentype 57 or 58) automatically zone the player to their destination when opened; the server handles the teleport after sending `OP_MoveDoor`.

### `POST /doors/click`
Click a door to request it open or closed (server-authoritative). Two forms:

```json
{ "door_id": 0 }
{ "name": "DOOR1" }
```

Pass either `door_id` (per-zone index) or `name` (exact case-insensitive match against the snapshot). The nav thread sends `OP_ClickDoor` to the server; the server replies with `OP_MoveDoor` to animate the door. The client cannot toggle doors locally — it waits for the server's reply to update visual state.

Returns `200 OK` if the request was queued, `404` if the door name/id does not exist. Verify the door opened via `GET /doors` polling `is_open`.

---

## Zone Traversal

### `GET /zone_points`
Returns all zone exit points received in `OP_SEND_ZONE_POINTS`.

```json
[
  { "iterator": 0, "server_x": 200.0, "server_y": 150.0, "server_z": -25.0,
    "heading": 90.0, "zone_id": 2 }
]
```

⚠️ `server_x/y/z` here are the **ARRIVAL** coords (where you land in the destination), **not** an
in-zone trigger to walk to — don't `/goto` them. `zone_id` is the destination zone. The reachable
target zones are the distinct `zone_id`s.

### `POST /zone_cross`
Travel to a zone. **Pass the destination `zone_id`** — the gameplay thread sends `OP_ZONE_CHANGE`
with that target from the player's current position. (Bug fixed 2026-06-21: it used to send the
*current* zone id, which the server reads as the destination → request cancelled. The server finds
the matching zone point near the player with a very generous range, so you don't need to reach a
trigger.)

```json
{ "zone_id": 1 }
```

The transition takes a few seconds; confirm with `GET /debug` `zone`. Verified qcat↔qeynos both ways.

---

## Typical Agent Workflow

```python
import requests, time

BASE = "http://127.0.0.1:8765"

# 1. Check where we are
cam = requests.get(f"{BASE}/camera").json()
print("Focus:", cam["focus"])

# 2. Find a target NPC
entities = requests.get(f"{BASE}/entities").json()
print(entities)

# 3. Navigate to it
r = requests.post(f"{BASE}/goto", json={"name": "Lanhern Firepride"})
time.sleep(5)  # wait for arrival

# 4. Hail it to start quest dialogue
requests.post(f"{BASE}/hail", json={"name": "Lanhern Firepride"})
time.sleep(1)

# 5. Capture frame to see dialogue
frame_bytes = requests.get(f"{BASE}/frame").content
with open("/tmp/dialogue.png", "wb") as f:
    f.write(frame_bytes)

# 6. Follow up a quest keyword
requests.post(f"{BASE}/say", json={"text": "shipment"})
```
