# Neriak Third Gate (neriakc) navmesh pathing stall

**Summary:** `/goto` toward the Third Gate library stalls: the player walks ~35
units, wedges into a narrow stone passage/corner, and stops permanently ~330
units short of the destination. Likely the same navmesh-stall root cause as the
gfaydark platform stall ([gfaydark-platform-pathing-stall](gfaydark-platform-pathing-stall.md)),
reproduced in neriakc's complex moat/library geometry.

**Severity:** Medium (blocks autonomous navigation to a chunk of the zone — e.g.
the library NPC Lokar To`Biath — so any quest there is unreachable by `/goto`).

**Zone / area:** `neriakc` (Neriak Third Gate), routing from near the Lodge of the
Dead toward the elevated library.

## Steps to reproduce
1. Mordeth (Dark Elf SK) in `neriakc`, standing near the Lodge at approx
   `(-1282, 1249, -80)`.
2. `POST /goto {"name":"Lokar_To`Biath000"}` (Lokar is in the library at approx
   `(-1246, 894, -66.6)`).
3. Poll `GET /debug` for ~25s.

## Expected
The player paths around the moat/up to the library and arrives at Lokar.

## Actual
The player moves only to about `(-1253, 1223, -87)` — ~35 units — then stops and
stays frozen there for the remainder of the poll (checked every 3s for 24s). Z
dropped from -80 to -87 (stepped down into the passage). A `/frame` shows the
character wedged against stone walls in a narrow corridor/corner. `/warp` to
`(-1246, 889, -66.6)` reaches Lokar fine, confirming the destination itself is
valid and only the *pathing* is at fault.

## Diagnosis notes
- Same signature as the gfaydark stall: navmesh finds an initial path, the avatar
  advances a short distance, then jams against geometry and never recovers.
- `/warp` (collision-bypassing teleport) reaches the target, so this is a
  path-following / navmesh-routing problem, not an unreachable destination.

## Suspected root cause
(unconfirmed) Navmesh routing/path-following stalls on tight or multi-level
geometry (narrow Neriak passages, the moat, the elevated library). Probably shares
a root cause with the gfaydark platform stall; worth fixing together.

## Status
Open
