# No death/respawn handling — after dying the client is stuck as a corpse (can't resume)

**Summary:** When the player dies, the client never processes the death/respawn flow. The
character stays frozen at the death location (not sent to bind), cannot act, and auto-attack /
re-target do nothing. The only recovery is to relaunch the client (a fresh login respawns at bind).

**Severity:** High (any death permanently stalls an unattended session)

**Zone / area:** Death / respawn flow (`OP_Death` / respawn window handling).

## Steps to reproduce
1. Let Brogan (L2 Warrior) take a lethal fight (easy via the add bug,
   `auto-combat-ignores-adds-player-dies.md`).
2. After `*** You have been slain! ***`, observe the client.

## Expected
Client handles death: respawns the player at the bind point (or answers the respawn window),
re-enables control, and auto-combat can resume — like the real client where you select "Resurrect
/ Return to bind".

## Actual
- After the slain message there are **no further log lines** for the player (no respawn, no
  unhandled opcode, no position change).
- `/v1/observe/debug` shows the player frozen at the **death location** (`-383,45,32`), NOT the bind
  (`-510,57,32`).
- `/v1/combat/target/name` + `/v1/combat/attack` produce no swings — a rodent 5u away at the same z is ignored because
  the player is dead.
- The session is dead-in-place until it eventually logs out. (In two earlier deaths the client
  issued a `clean shutdown requested — sending OP_Logout` ~90s after death; in a third it just sat
  as a corpse for 10+ minutes — inconsistent, but never a successful respawn.)

## Diagnosis notes
- No `OP_Death` / respawn / bind / resurrect handling appears anywhere in the log around death.
- Recovery confirmed: relaunching the client (fresh login) respawns Brogan alive at bind and play
  resumes — so the character/server state is fine; it's purely missing client-side death handling.
- Adjacent feature seen working: the client auto-loots corpses (`auto-loot: queued corpse_id=...`
  → `sent OP_LootRequest`), though it logs `OP_MoneyOnCorpse denied (response=1/6)` when trying to
  pull coin — possibly a separate minor issue (coin-on-corpse request rejected).

## Suspected root cause
(unconfirmed) The client doesn't decode `OP_Death` for self nor answer the respawn/bind window
(e.g. `OP_RespawnWindow` / sending the chosen respawn option), so the server leaves the player as a
corpse. Fix: on self-death, present/auto-answer the respawn window (choose bind), reset player
state to alive at the returned location, and re-enable the nav/auto-combat.

## Root cause (CONFIRMED against EQEmu source ~/git/EQEmu)
The suspected cause above is correct. The server's death flow (zone/attack.cpp `Client::Death` →
`SendRespawnBinds`, zone/client.cpp:5565) sends **`OP_RespawnWindow` (RoF2 `0x0ecb`)** right after
`OP_Death` and holds the player as a hovering corpse until the client replies. The client must
answer with a **4-byte option index** (`Handle_OP_RespawnWindow`, client_packet.cpp:13664 — size
must be 4); the server populates **option 0 = "Bind Location"** (pushed to the front in
`SendRespawnBinds`). With the hover auto-respawn timer disabled/long, a client that never replies
stays a corpse indefinitely — exactly the symptom. eqoxide never mapped `0x0ecb`, so the window was
silently unhandled (no "unhandled opcode" line because that logs below the configured level).

After the reply (`HandleRespawnFromHover(0)`, client_process.cpp:2126) the server sends
`OP_ZonePlayerToBind` + `RestoreHealth`/`SendHPUpdate` + `ClearHover`→`OP_ZoneEntry` (re-spawn),
all already handled by eqoxide (`apply_bind_respawn`, `apply_hp_update`, `register_spawn`). So the
only missing piece is sending the reply.

## Fix (branch worktree-death-respawn)
- `protocol.rs`: define `OP_RESPAWN_WINDOW = 0x0ecb` + `build_respawn_select`/`respawn_window_reply`
  helpers (unit-tested).
- `gameplay.rs`: on inbound `OP_RESPAWN_WINDOW`, auto-select option 0 (bind) and send the 4-byte
  reply, mirroring the existing `OP_LOOT_ITEM` echo; clear the "waiting to respawn" strategy. The
  rest of the respawn (position, HP, re-spawn) self-heals through existing handlers.
- 282 unit tests pass.

## Live test (2026-06-29, GM-assisted) — IMPORTANT NUANCE
A GM (Aiquestbot) summoned Campy to qeynos and slew her twice. BOTH deaths recovered correctly via
the **direct `OP_ZonePlayerToBind` path** (existing `apply_bind_respawn`): "*** You have been
slain! ***" → "Respawning at bind point" → alive at bind (0,10,5), movable (verified via /v1/navigate/goto).
The new `OP_RespawnWindow` handler fired **0 times** — this server has
`RuleB(Character, RespawnFromHover)` **OFF**, so it sends `OP_ZonePlayerToBind` directly and never
sends `OP_RespawnWindow (0x0ecb)`.

Implications:
- On the current server the user-visible "stuck corpse" symptom does NOT reproduce — death/respawn
  already works via the direct-bind handler (which predates this bug report, commit 49bfce0). So
  either the server's RespawnFromHover rule changed since the report, or the original stuck death
  occurred under different conditions (natural mob death with hover ON).
- This fix (answer `OP_RespawnWindow`) is correct + verified against EQEmu source and is
  non-regressive (both live deaths recovered fine with it present), but its code path is only
  exercised when `RespawnFromHover` is ON. It is defensive/future-proofing for that config.
- To LIVE-validate the hover path, enable `Character:RespawnFromHover` on the server (`#rules set`
  or DB + `#reload rules`) and die again — then expect the log line "respawn window — auto-selected
  bind (option 0)".

## Status
Fix implemented (source-verified, non-regressive); direct-bind death works live. Hover-path
(this fix's code path) not yet live-exercised — server rule RespawnFromHover is off.
