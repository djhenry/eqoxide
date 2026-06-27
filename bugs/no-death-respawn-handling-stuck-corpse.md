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
- `/debug` shows the player frozen at the **death location** (`-383,45,32`), NOT the bind
  (`-510,57,32`).
- `/target/name` + `/attack` produce no swings — a rodent 5u away at the same z is ignored because
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

## Status
Open
