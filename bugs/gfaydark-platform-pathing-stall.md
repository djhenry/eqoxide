# gfaydark platform pathing stall

**Summary:** Agent auto-walk (`/v1/navigate/goto`) stalls partway across the Greater Faydark
(Kelethin) tree platforms — the character stops making progress well short of the
target and never reaches it.

**Severity:** Medium (blocks reliable agent navigation in gfaydark; manual WASD
still works).

**Zone:** gfaydark (Greater Faydark / Kelethin)

## Steps to reproduce
1. Launch the client and zone into `gfaydark` (the player spawns up on the
   Kelethin platforms, z ≈ 72).
2. `POST /v1/navigate/goto {"x":200,"y":40,"z":4}` (toward the North Felwithe zone line, or
   any target a few hundred units away across the platforms).
3. Watch the position via `GET /v1/observe/debug`.

## Expected
The character walks the full path to the target (or reports `NAV: arrived` /
`boxed in` / `Path blocked`).

## Actual
The character advances ~150–160 units, then **stops** at an intermediate point
and makes no further progress. No `NAV: arrived`, `boxed in`, or `Path blocked`
log line is emitted — it just stalls silently.

Observed example (2026-06-26):
- Start `(473, 239, 72)` → walked to `(340, 149, 85)` then stopped, still
  **177 units** from the `(200, 40)` zone line.
- A second `/v1/navigate/goto` from `(466, 237)` advanced only ~6 units to `(473, 238)` then
  stalled again.
- Note the z climbed 72 → 85 mid-walk, suggesting it was traversing a
  ramp/platform when it got stuck.

## Diagnosis notes
- The walk *speed* and the camera-follow are fine (verified separately); this is
  a **pathfinding/collision** problem specific to the tree-platform geometry.
- Suspected: `find_path` / the collision `slide_move` cannot continue across the
  multi-level platform geometry (overhangs, gaps, narrow walkways) and the nav
  loop ends up with no usable step but doesn't clear the goal or log a stop.
- Silent stall (no stop log) implies the nav tick is returning early without
  picking a step rather than hitting the explicit "boxed in" path.

## Suspected root cause (unconfirmed)
`find_path`'s floor probe / nearest-floor handling on stacked platform geometry
in gfaydark — likely the same family of issues noted in the nav floor-fix work
(overhangs mistaken for floor / unable to descend between levels).

## Status
Migrated to GitHub issue https://github.com/djhenry/eqoxide/issues/4 (in-repo tracker deprecated).
