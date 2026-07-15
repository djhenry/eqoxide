# Autonomous Play: combat, leveling, travel, buying

How an agent drives a **regular (non-GM) player character** through real gameplay — fighting,
leveling, traveling between zones, and buying from merchants — and the protocol/EQEmu facts that
make each work. Verified live 2026-06-21 by playing the character "Claude" (Female Wood Elf Ranger)
on the local EQEmu server. Companion to `protocol-notes.md`, `http-api.md`, `collision-system.md`.

The client was originally a GM observer (see `architecture.md`); it now also runs as an ordinary
player. The renderer/HTTP-API code is identical — only the logged-in account/character differs
(set in the per-character login config under `~/.config/eqoxide/`, which carries the
account + character_name fields).

---

## 0. Finding quests — no oracle; discover them like a human

The client deliberately exposes **no** list of quest givers, no "what does this NPC want" readout,
and no map/HUD marker over quest NPCs. A human player has none of that, and neither does the agent:
figuring out which quests exist and what to do is part of the game. Discover quests the way a person
would — **in-game context clues** (NPC hails, `[bracketed]` keywords, zone text, item drops),
**internet searches** (EQ quest wikis/allakhazam-style references), and **trial + error** (hail an
NPC, say its keywords, try handing it likely items).

The one quest surface the client *does* expose is EQ's **native Task-system journal**, and only
because it is **server-pushed** — the same quest window a human sees in their own client. `GET
/v1/quests/log` returns active tasks with title, description, reward, and objectives showing live
progress (`done_count`/`goal_count`); completed tasks move to `GET /v1/quests/completed`; `POST
/v1/quests/cancel` abandons an active task; `GET /v1/quests/offers` + `POST /v1/quests/accept`/
`/decline` handle the (rare) case where an NPC presents a choice of tasks instead of auto-granting
one. Most classic Qeynos quests (Rat Whiskers, Gnoll Fangs, the guild-note hand-in) are emergent
server Lua with **no protocol representation** — they never appear in the task journal, and there is
no legitimate way for the client to surface them. You find them by playing.

To *complete* a turn-in quest you: reach + kill the mob → **loot** the item (`/v1/interact/loot`) →
reach the giver → **hand it in** (`/v1/interact/give`). The hail/say flow for dialogue quests is in
`docs/npc-interaction.md`.

---

## 1. Creating + configuring a non-GM character (EQEmu DB)

DB: connect to the EQEmu `peq` database with your own local credentials (host/container, user, and
password are environment-specific — do not hard-code or commit them). Tables that matter:

- **`login_accounts`** (login server auth): set `account_password` to the **lowercase-hex SHA512**
  of the password. EQEmu's `eqcrypt_verify_hash` falls through all hash modes, so a SHA512 string
  verifies via the mode-9 fallback even though new accounts default to salted SCrypt (mode 14),
  which never re-verifies by string compare. (Do NOT try to crack *other* accounts' hashes — set
  your own.)
- **`account`** (world): `status` field — `0` = normal player, `>=100` = GM. Create the character
  as status 0 to play as a non-GM.
- **`character_data`**: a minimal INSERT works (no required-no-default columns). NOTE there is **no
  platinum/gold column here** — coin lives in `character_currency`. Class/Race IDs: Warrior=1,
  Ranger=4; Human=1, WoodElf=4.
- **`character_skills`** (id, skill_id, value) — give combat skills (1H slashing, offense, etc.).
- **`character_bind`** (id, slot 0, zone_id, x, y, z) — respawn point.
- **`character_currency`** (id, platinum, gold, silver, copper, …) — coin for buying.
- **`inventory`** (character_id, slot_id, item_id, charges) — worn slots: **13=primary, 14=secondary,
  11=ranged, 17=chest**, general bags start at 23. (The server may relocate a worn item to a bag
  slot if the character can't equip it there.)

`#zone` / `#goto` / GM warp are **GM-only** — a non-GM character cannot use them. Move via legit
walking (`/v1/move/goto` pathfinding) or DB edits during a clean
logout (see §6).

---

## 2. Combat — the heading bug was the whole story

### The bug (fixed)
A player's melee **only swings server-side when facing the target**. EQEmu gates each swing
(`zone/client_process.cpp` ~line 398) on: `may_use_attacks` (alive, not casting/stunned, has a
target) && `attack_timer` && `CombatRange` && `CheckLosFN` (LOS) && **`IsFacingMob`** (facing).
`IsFacingMob` (`zone/mob.cpp`) passes only when `|HeadingAngleToMob - GetHeading()| <= 80` EQ-units
(~56°).

The client was sending the player's heading **2× too large**: `send_position_update` packed
`deg_cw * 4096/360`, but the server decodes the wire heading via `EQ12toFloat = wire/4` and EQ
headings are 0..512 (=0..360°), so the wire value must be `deg_cw * 2048/360` (= `EQ_units * 4`).
The doubled heading meant the server thought the player faced the wrong way → `IsFacingMob` failed →
**zero swings landed**, while movement (x/y/z) and the local visual heading looked fine. Fixed in
`src/eq_net/navigation.rs: send_position_update` (`2048.0/360.0`). This is the single most important
fix for any client-initiated melee.

### Auto-combat (navigation.rs, runs in the 150ms nav tick when auto-attack is ON)
- **Auto-engage**: while attacking, if the target is within ~60u, walk (collision-aware) to ~5u and
  **face it each tick** so swings register; hold + face once in melee.
- **Auto-retarget**: when the target dies/goes unreachable, target the nearest reachable trash mob
  (name starts `a_`/`an_`, excludes named guards/merchants) within 200u that has a clear path
  (`path_clear`). If none reachable, idle and wait for respawns (do NOT roam toward out-of-pocket
  mobs — it strands the bot; see §5 limitations).
- **Reachability matters**: the combat approach drives the shared collide-and-slide mover
  (`CharacterController::slide`) toward the target, so a mob behind a wall it cannot slide around, or
  across water / at a different z, is not reliably meleeable (see §5). For a mob around a corner, issue
  a `/v1/move/goto` first (A\* routes around walls) and engage once adjacent.

### Verifying combat
Client logs outgoing hits as `EQ: combat: Claude hits <mob> for N damage` and kills as
`<mob> has been slain` in `/tmp/eqoxide.log`. The client does NOT expose HP via `/v1/observe/debug`; use the
combat log + `/v1/observe/entities` (mob despawns on death) + a level/exp DB read. EQEmu combat logging is OFF
by default, so the zone log won't show swings.

---

## 3. Travel between zones — send the TARGET zone id

### The bug (fixed)
`send_zone_change_packet` put the **current** zone id in `ZoneChange_Struct.zoneID`, but EQEmu
(`zone/zoning.cpp`, `ZoneUnsolicited` mode) reads that field as the **destination**. target==current
→ the request was cancelled / looped back to the same zone. Fixed: pass the destination zone id.

### How zone crossing actually works here
- `GET /v1/observe/zone_points` returns the points from `OP_SEND_ZONE_POINTS`, but those coords are the
  **ARRIVAL** coords (where you land in the destination), **NOT** the in-zone trigger to walk into.
  So don't `/v1/move/goto` them.
- `POST /v1/move/zone_cross {"zone_id": N}` → the nav sends `OP_ZONE_CHANGE` with zoneID=N from the player's
  current position. The server's `GetClosestZonePoint(GetPosition(), N, range)` finds the matching
  zone point. The range check compares **linear distance to a squared max** (`zone/zone.cpp`,
  effectively a no-op), so position barely matters — being within ~400u just avoids a cheat-flag
  warning (which logs, doesn't block). So you can travel to any zone reachable from the current one
  without precisely reaching a trigger.
- Verified both directions: qcat(45) ↔ qeynos(1). Check the result with `GET /v1/observe/debug` `zone`.
- Reachable target zones = the distinct `zone_id`s in `/v1/observe/zone_points`.

The classic "walk into the zone-line geometry" auto-cross is NOT implemented (the client lacks the
trigger-box coords; only arrival coords are sent). Use `/v1/move/zone_cross {zone_id}`.

---

## 4. Buying from a merchant

Opcodes (Titanium, from `patch_Titanium.conf`, verified to match this server):
`OP_ShopRequest=0x45f9`, `OP_ShopPlayerBuy=0x221e`, `OP_ShopEnd=0x7e03`.

Flow (`POST /v1/merchant/buy {"merchant":"<name>","slot":N}` → nav sends both in sequence):
1. **`OP_ShopRequest`** with `MerchantClick_Struct` (24 bytes): `npc_id`(entity/spawn id),
   `player_id`, `command`=1 (open), `rate`(f32), `tab_display`(i32), `unknown020`(i32). Opens the
   merchant server-side.
2. **`OP_ShopPlayerBuy`** with `Merchant_Sell_Struct` (24 bytes): `npcid`, `playerid`, `itemslot`,
   `unknown12`, `quantity`, `price`. `itemslot` = the **`merchantlist.slot`** of the item (query the
   EQEmu DB `merchantlist where merchantid=<npc.merchant_id>`); quantity=1; price can be 0 (server
   charges its sell price — typically a markup over `items.price`).

Requirements:
- Must be within **`USE_NPC_RANGE2` = 40000 = 200u** of the merchant, and it's a **3D** distance —
  mind the z (a merchant on a ledge above/below you can be out of range even if close in 2D).
- Need coin in `character_currency`.
- Verified: bought "Spell: Diamondskin" from a qcat merchant, item landed in inventory + coin
  dropped, confirmed in the DB after a save. NOTE: qcat merchants sell **spells**; equipment
  (armor/weapons) merchants are in cities (qeynos), e.g. Captain_Rohand (merchant_id 1101) near the
  qeynos dock.

---

## 4b. Moving / equipping items

Opcode (Titanium, from `patch_Titanium.conf`): `OP_MoveItem=0x420f`.

`POST /v1/inventory/move {"from":N,"to":M}` → nav sends one **`OP_MoveItem`** with `MoveItem_Struct` (12 bytes):
`from_slot`(u32), `to_slot`(u32), `number_in_stack`(u32, =1 for a single non-stacked item).

Slot ids (Titanium): **0-21** worn equipment, **22-29** general inventory, **30** cursor, **251+**
bag contents. **Equipping = moving a bag/general-slot item to its worn slot**, e.g. boots → slot 19
(Feet), a chest piece → slot 17 (Chest), a 1H weapon → slot 13 (Primary). Unequipping = move a worn
slot to a free general slot (22-29). Read the current item→slot mapping from the decoded inventory
(`GameState.inventory`) before issuing the move.

---

## 5. Navigation / pathfinding (find_path)

`Collision::find_path(start, goal, radius) -> Option<Vec<[east,north]>>` (`src/assets.rs`) is grid
A* over the collision grid. It routes AROUND walls (a plain slide toward the goal only slides along one
wall and stalls at corners). `/v1/move/goto` computes a path when the goal changes and walks the
waypoints by emitting a `MoveIntent` each tick; the frame-by-frame motion — for nav, free WASD, and the
combat auto-engage alike — is resolved by the ONE collide-and-slide mover, `CharacterController::slide`
(the divergent `navigation::slide_move` was deleted in #378 Phase 2).

Floor handling: the per-cell floor is probed **relative to the floor of the cell it was reached from**
(`floor_near`), so multi-level dungeons work even when the caller's start `z` is stale (a common
bug — `gs.player_z` is often the spawn z, not the real floor). Walkable = a floor exists; an edge
needs a small floor-height step (`STEP_H=20`) and a clear chest-height segment.

**Limitations (real, hit during play):**
- **Water**: fish/water mobs sit in water a walking character can't path to; `find_path` returns no
  route to them, and even if `path_clear` (LOS over water) passes, melee won't connect across/below
  water. The auto-grind excludes nothing by name now, relying on `path_clear`; if you target water
  mobs, exclude `fish` by name.
- **Sealed pockets / doors**: doors are NOT in the collision (the client suppresses `OP_SPAWN_DOOR`),
  so a room behind a closed door is a disconnected "pocket" — `find_path` correctly finds no route
  (e.g. qcat's merchant rooms are unreachable on foot from the fish room). Reaching them needs door
  modeling or DB-positioning.
- A* is capped at `MAX_NODES=200000`.

---

## 6. Position persistence — the linkdead clobber (keystone)

Editing `character_data` x/y/z while the client was recently connected gets **clobbered**: a quick
reconnect RESUMES the still-live *linkdead* zone session (at the old in-memory position) instead of
loading from the DB. `Zone:ClientLinkdeadMS` (DB rule) = 60000ms here.

**Recipe to make a DB position edit stick** (used to place the character at a merchant / safe spot):
1. Stop the client. (If using `dev-run.sh`, kill the **dev-run watcher** too or it auto-relaunches —
   `pgrep -af dev-run.sh | grep -v 'bash -c'`, kill that PID; verify with `ps` that nothing remains.)
2. **Wait > ~90s** so the linkdead session expires and the zone removes+saves the character.
3. `UPDATE character_data SET x=…, y=…, z=…, zone_id=… WHERE name='…';` and verify the DB holds it.
4. Relaunch — the fresh login reads the DB and the position sticks.

Verified: set the character onto a merchant's exact coords and she loaded there; set a safe-point on
logout and it persisted. (Also recorded in the project memory `eqemu-position-clobber.md`.)

A `z` gotcha: a "spot" has a floor `z`. In qcat's fish room, the meleeable grind spot is the **edge
at z≈-40** (where surface fish are reachable); the **pit bottom z≈-76** leaves you 35u below the fish
(un-meleeable). Always set the floor-level z, not a pit bottom.

---

## 7. Movement constraints for a non-GM character

- **`/v1/move/goto`**: legit incremental walking the server accepts (no rubber-band). Now routes around walls
  via `find_path`. The reliable movement primitive.
- City **guards** are tough and gang up; attacking one aggros others and can make you KOS to that
  guard faction (resets on a fresh login / no persistent `faction_values` here). Fine for testing,
  but don't grind city guards.

---

## 8. What "playing" looked like (sanity reference)

A non-GM Female Wood Elf Ranger was created, then **won fights**, **leveled** hands-free (L1→L4 via
real kills, 0 deaths grinding the qcat fish-room edge), **traveled** qcat↔qeynos via zone lines, and
**bought** a spell from a merchant — all via the HTTP API + the auto-combat nav, after the heading,
zone-change, merchant, and pathfinding fixes above. The recurring friction was always *reaching* the
target (walls/water/pockets + the position clobber), never the actions themselves once in range.
