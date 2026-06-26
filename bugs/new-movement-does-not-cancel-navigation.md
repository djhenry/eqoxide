# A new goto / warp / manual movement does not cancel active navigation

**Summary:** Once a `/goto` is in progress, issuing a new `/goto`, a `/warp`, or
moving manually (WASD) does NOT cancel the in-progress navigation. The old nav
target keeps driving the avatar every tick, fighting the new command. A stalled
goto therefore can't be overridden and effectively pins the player in place.

**Severity:** Medium (makes navigation unrecoverable without a workaround or
relog; directly compounds the pathing-stall bugs — when a goto stalls you can't
warp out of it).

**Zone / area:** Navigation / movement control (the nav state machine vs.
warp/goto/manual input).

## Steps to reproduce
1. Issue a `/goto` that stalls against geometry (e.g. Mordeth in `neriakc`:
   `POST /goto {"name":"Lokar_To`Biath000"}` — stalls at ~(-1253,1223), see
   [neriakc-library-pathing-stall](neriakc-library-pathing-stall.md)).
2. Try to `POST /warp {"x":-1413,"y":910,"z":-80.6}` to somewhere else.
3. `GET /debug` — the player is still at the stall point, not the warp target.

## Expected
Any new movement command cancels the current navigation:
- a new `/goto` replaces the target,
- a `/warp` teleports and clears the active path,
- manual WASD movement cancels auto-navigation (as in the native client).

## Actual
The original nav target persists and keeps pulling the avatar back. Observed:
- `/warp` to a different point doesn't stick — the avatar is dragged back to the
  stalled path's wall (a `Path blocked by a wall` message appears).
- `/warp` only "sticks" if you happen to warp to the *old* goto's destination
  (then nav arrives and stops).

## Workaround (discovered)
`POST /goto {x,y,z}` to the player's **current** position first — nav treats it as
"arrived" and stops — then `/warp` to the real destination holds.

## Suspected root cause / fix
(unconfirmed) Warp/goto/manual-input handlers don't clear the navigator's active
path/target. Fix: have a new `/goto` replace the path, have `/warp` clear the
active nav target, and have manual movement input cancel auto-nav.

## Status
Fixed (branch `worktree-mordeth`). The `/warp` slot is now consumed by the NAV
thread, which performs a real teleport (jump position + send a position update +
clear the A* path + clear `goto_target`) instead of the app writing the warp
coords into `goto_target` (which made the nav *walk* there and stall). Manual
movement (WASD/QE keydown) now clears `goto_target` so it cancels an active
/goto; a new `/goto` already replaces the target. Verified live: a `/warp` issued
mid-`/goto` teleports to the exact coords and holds (no drag-back); navigation is
cancelled.
