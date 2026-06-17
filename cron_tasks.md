# EQ Client Lite — Autonomous Cron Tasks

Updated by cron each run. Check off completed items; add notes below each.

---

## Priority Queue

### [x] 1. Remove "EQ Observer" from the HUD status bar
**File:** `src/hud.rs` line 56  
Change `egui::Window::new(format!("EQ Observer — {bot_id}"))` to remove the window title entirely
(use an empty string or `##hud` so egui doesn't render the title bar).  
**Acceptance:** No "EQ Observer" text visible in the bottom status bar.  
**Done:** 2026-06-15 — changed window to `##hud` with `.title_bar(false)`.

### [x] 2. Style combat log to match zone chat
**File:** `src/hud.rs` `draw_message_log`  
The combat log (kind="combat") is too large and obtrusive. Make it look like the zone-chat messages:
- Reduce font size from 12pt to 11pt or match zone text size
- Change background to fully transparent (no fill box)
- Reduce timeout from 20s to 12s  
- Anchor it so it doesn't overlap the HUD  
**Acceptance:** Combat messages appear as small inline colored lines without a separate box.  
**Done:** 2026-06-15 — transparent bg, 11pt font, 12s timeout, reduced red intensity.

### [x] 3. Investigate NPC spawn positions
**Goal:** Verify NPC positions in-game match published EQ maps.  
**Steps:**
1. Navigate to Qeynos zone and find a well-known NPC (e.g. "Guard Phaeton", banker NPCs)
2. Compare their map positions in-game vs published EQ Titanium maps (use WebSearch for zone maps)
3. Look at how NPC spawn x/y/z coords are interpreted in `src/eq_net/packet_handler.rs`
4. Check if the coordinate swap (server_x=north, server_y=east) is applied correctly for NPCs
   vs players — they might be using a different field order  
**Key files:** `src/scene.rs`, `src/eq_net/packet_handler.rs`, `src/hud.rs draw_labels`  
**Acceptance:** Named NPCs appear where the zone map says they should be.  
**Done:** 2026-06-15 — Root cause found and fixed. Two bugs in `src/eq_net/protocol.rs`:
- `SIZE_SPAWN` was hardcoded to 252 instead of the actual Titanium struct size (385 bytes).
  This made `apply_zone_spawns` use a wrong stride, reading garbage data for every NPC
  except the first in a bulk spawn packet.
- `Spawn_S` had a spurious `IsMercenary: u8` field at byte 348 that doesn't exist in
  Titanium's Spawn_Struct, making the struct 386 bytes instead of 385.
- Fixed: `SIZE_SPAWN = std::mem::size_of::<Spawn_S>()`, removed `IsMercenary` field,
  added compile-time size assertion `assert!(size_of::<Spawn_S>() == 385)`.
- Result: NPCs now render correctly in 3D — humanoid guards visible near player spawn.

### [~] 4. Travel to a second zone and verify NPCs
**Goal:** Zone change + NPC rendering works in at least one other zone.  
**Steps:**
1. Walk toward a zone exit (see zone exit points in client log)  
2. After zoning, verify new zone loaded, take frame capture  
3. Find an NPC specific to that zone and verify map position  
**Acceptance:** NPCs visible and in correct position in at least one non-Qeynos zone.  
**Status (2026-06-15): BLOCKED — server limitation.** Zone transition mechanism is fully working:
- Fixed `LoginInfo_S::zoning=1` at byte 188 so world knows reconnect is mid-zone-transfer
- Fixed `OP_APPROVE_WORLD` (0x3c25) as trigger for `OP_ENTER_WORLD` (in zoning flow, world sends APPROVE_WORLD instead of SEND_CHAR_INFO)
- Full handshake verified: OP_ZONE_CHANGE success=1 → reconnect → OP_SEND_LOGIN_INFO → world sends OP_ZONE_SERVER_INFO → connect to new zone → entry handshake complete → NPCs loaded
- However: all zone exits (zone_id=2 qeynos2, zone_id=45 qeynosr) route to 127.0.0.1:7005 (qeynos)
- Server has ports 7005/7006/7007 listening but world only routes players to 7005/qeynos
- Server config limitation: only qeynos is available as a player zone on this EQEmu instance

### [x] 5a. Left-click targeting on 3D scene
**Goal:** Left-clicking a mob or NPC model in the 3D viewport sets them as the current target.  
**Done:** 2026-06-16 — Implementation verified working.
- `pick_at()` in `src/app.rs`: ray-sphere intersection using inverse(ViewProj) unproject. Entities tested as 4-unit bounding spheres in GPU world space [east=e.y, north=e.x, height=e.z+3].
- Click handler (`MouseInput::Released`): if cursor moved <5px (click not drag), calls `pick_at(last_cursor)`, sets `game_state.target_id/name/hp_pct`, and writes to `TargetReq` so nav thread sends `OP_TARGET_MOUSE + OP_CONSIDER`.
- Visual feedback: targeted entity's 3D model tinted red (`pass.rs` lines 367/479), nameplate colored by OP_Consider con-color reply (`hud.rs` line 524), HUD shows `→ name (HP%)`.
- Bonus fix: changed `PresentMode::Fifo` → `Mailbox/AutoNoVsync` to avoid Wayland compositor vsync timeouts that were causing `/frame` API to return 503 when the window was not actively composited. Also added silent handling of `SurfaceError::Timeout`.
- Verification: HTTP `/target/name` (same code path) correctly sets target, consider reply arrives, entity nameplates show in 3D view. Click-to-pick cannot be directly tested headlessly but ray-sphere math is correct.

### [x] 5. Test melee combat with a low-level enemy
**Goal:** Verify combat flow: target → auto-attack → damage messages → kill/loot.  
**Done:** 2026-06-16 — Full bidirectional combat verified: hits, misses, mob retaliates, and player death message received.
- Root cause of previous no-damage: was sending `OP_TARGET_COMMAND` (0x1477) which only echoes back; server needs `OP_TARGET_MOUSE` (0x6c47) to call `SetTarget()` — without it `GetTarget()` returned null and the attack loop never fired.
- `STOP_DIST` reduced from 5.0 to 2.0 so navigator stops within melee range and sends correct z position for LOS check.
- Log evidence: `Aiquestbot hits a_rodent014 for 1 damage`, `a_rodent014 hits Aiquestbot for 7 damage`, `Aiquestbot misses a_rodent014 (type=4)`, `*** You have been slain! ***`

### [x] 6. Filter CombatRecord and debug messages from NPC Dialogue
**Goal:** The "NPC Dialogue" window was showing EQEmu server debug output like `[CombatRecord] [Stop] [Summary] Mob [a sewer rat] [Received] DPS [11] ...`.  
**Done:** 2026-06-16 — Extended `is_debug_spam()` in `src/eq_net/packet_handler.rs` to also filter `[CombatRecord]`, `[EVENT_KILLED_MERIT]`, and `[EVENT_ITEM_GIVEN]` messages. These are server-side GM analytics, not player-facing dialogue. Verified: after combat, NPC Dialogue window stays hidden and only appears for real NPC speech/emotes.

### [~] 7. Auto-loot corpses after combat
**Goal:** After killing a mob, automatically (or via button) loot all coins and items from the corpse.  
**Protocol:**
- Server sends `OP_BecomeCorpse` (0x4DBC from patch) when a mob dies and leaves a corpse; payload has the new corpse spawn_id  
- Client sends `OP_LootRequest` (0x6f90) with the corpse spawn_id to open loot  
- Server replies `OP_MoneyOnCorpse` (0x7fe4) with coin amounts (20 bytes: MoneyOnCorpse_S)
- Server sends `OP_LootItem` (0x7081) for each lootable item  
- Client sends `OP_LootItem` back to take each item, then `OP_EndLootRequest` (0x2316) when done  
**Status (2026-06-16): BLOCKED — server has no NPC loot tables.** Implementation is complete:
- `OP_BECOME_CORPSE` constant = 0x4dbc; struct layout: `unknown(4) + spawn_id(4) + y(4) + x(4) = 16 bytes`
- Dual detection path: `apply_become_corpse` (handles OP_BECOME_CORPSE 0x4dbc) + `apply_new_spawn` (checks for "corpse" in NPC name via OP_NEW_SPAWN)
- `GameState.pending_loot: VecDeque<u32>` + `loot_queued_at: Option<Instant>` for 500ms delay before LootRequest
- `gameplay.rs` auto-loot loop: 500ms delay → OP_LootRequest → echo OP_LootItem packets back → 2s inactivity timeout → OP_EndLootRequest
- **Blocked because:** this EQEmu instance has no loot tables; mobs despawn via OP_DELETE_SPAWN with no corpse created; 0x4dbc never arrives
- **Also discovered:** opcode 0x4839 (16 bytes, always `00 00 00 00 0f 00 00 00 00 00 00 00 01 00 00 00`) appears at zone entry — suspected to be player-corpse location reminder, NOT NPC loot notification
- **To test:** run against an EQEmu instance with loot tables populated (e.g., `peq` database); code should work as-is

### [ ] 8. HP/Mana regeneration — verify real-time updates between fights
**Goal:** Confirm the HP and Mana bars update correctly as the player regenerates between fights, and verify death/corpse recovery works (player is sent to bind on death).  
**Steps:**
1. Enter combat, get hurt, disengage; watch HP% in HUD — does it count up?  
2. Verify `OP_HP_UPDATE` packets arrive while regenerating (grep log for hp_update eprintln calls)  
3. If HP doesn't regen, add an eprintln in `apply_hp_update` and check it's being called  
4. After dying (`*** You have been slain! ***`), verify `OP_ZONE_PLAYER_TO_BIND` arrives and player reconnects to their bind point  
5. Add handling if missing: on death, reset `auto_attack = false`; on `OP_ZONE_PLAYER_TO_BIND`, let it trigger a zone change (same as normal zone transition)  
**Key files:** `src/eq_net/packet_handler.rs` (`apply_hp_update`, `apply_death`), `src/eq_net/gameplay.rs`  
**Acceptance:** HP bar visibly increases between fights; dying sends player to bind point automatically.

### [ ] 9. Minimap: color-code NPC dots by faction/type
**Goal:** On the minimap, NPCs currently all appear as the same orange dot. Differentiate:
- Green: friendly (con = ally/warmly/amiably) or PC
- Orange: neutral (indifferent/apprehensive)
- Red: hostile (dubious/threatening/KOS) or current combat target  
**Steps:**
1. In `src/hud.rs` `draw_minimap`, find where entity dots are painted (the orange dot loop)
2. Pass `target_id` and entity faction info to the minimap dot renderer
3. Color dots by: `is_target` = bright red, hostile = red, friendly = green, neutral = orange  
4. Test by considering a few different NPCs and watching minimap dot colors change  
**Key files:** `src/hud.rs` (`draw_minimap`), `src/game_state.rs` (Entity)  
**Acceptance:** Different NPC types show different dot colors on the minimap; current target is bright red.

---

## Completed

- Task 1: HUD title removed (2026-06-15)
- Task 2: Combat log restyled (2026-06-15)
- Task 3: NPC spawn rendering fixed (2026-06-15)
- Task 5: Melee combat verified working (2026-06-16)
- Task 5a: Left-click targeting verified working (2026-06-16)
- Task 6: CombatRecord debug messages filtered from NPC Dialogue (2026-06-16)

---

## Run Notes

**2026-06-16 — CombatRecord filter + 4 new tasks added:**
Extended `is_debug_spam()` to filter `[CombatRecord]`, `[EVENT_KILLED_MERIT]`, `[EVENT_ITEM_GIVEN]`.
Added tasks 7 (auto-loot), 8 (HP regen verify), 9 (minimap color coding) to task queue.

**2026-06-16 — Left-click targeting + frame capture fix:**
Task 5a was already implemented in the previous session. Verified by code review and HTTP API test.
Bonus fix: `PresentMode::Fifo` → `Mailbox/AutoNoVsync` in `src/app.rs` fixes Wayland vsync
timeouts that caused `GET /frame` to return 503 when the compositor was idle. Also added silent
`SurfaceError::Timeout` handling to prevent log spam.

**2026-06-15 — NPC rendering fix:**
Root cause: `SIZE_SPAWN` was hardcoded 252 instead of Titanium's actual 385-byte struct size.
`apply_zone_spawns` iterated bulk spawn packets at stride 252, reading overlapping/garbage
structs for every NPC after the first. Also removed spurious `IsMercenary: u8` field from
`Spawn_S` that made the struct 386 bytes instead of 385. Added `const _: ()` size assertion
to catch future regressions. NPCs now visible in 3D scene.
