# Zone-in below-floor burial

**Summary:** After zoning into a new zone, the player is rendered sunk into the
ground (buried up to the chest) and stays that way. The zone-in z is below the
zone's floor, and the ground-snap can't lift the player back up.

**Severity:** Medium (visual; the player is mispositioned in Z on zone-in. Walking
does not reliably recover it.)

**Zone observed:** felwithea (North Felwithe), zoning in from gfaydark.

## Steps to reproduce
1. Be in `gfaydark` near the North Felwithe zone line.
2. Cross to Felwithe: `POST /zone_cross {"zone_id":61}` (or walk through the zone
   line).
3. Observe the player after the zone loads: `GET /debug` and a `GET /frame`.

## Expected
The player stands on the Felwithe floor with feet on the ground.

## Actual
The player spawns at `(200, 40, 4.0)` (the zone point's `server_z`) and is
rendered **buried to the chest** — the floor surface sits at shoulder height.
`z` stays at `4.0` while idle and does not self-correct.

## Diagnosis notes (2026-06-26)
- `z` stayed exactly `4.0` while idle and after a short `/goto` nudge.
- `POST /warp {"x":200,"y":40,"z":20}` → the player then stands **correctly on
  the floor** and remains at z ≈ 20. So the actual floor at `(200,40)` is roughly
  z ≈ 18–20, i.e. **~16 units above the zone-in z of 4**.
- Warping *above* the floor lets the snap work; spawning *below* it does not.

## Suspected root cause
The per-frame ground-snap / `ground_z` (floor probe) ray-casts **downward** from
roughly `z + 2`. When the player spawns **below** the floor (zone-point
`server_z` lower than the client's GLB floor), the downward ray finds no surface
above it, so `z` is left unchanged and the player stays buried.

Likely fixes to evaluate:
- On zone-in, probe the floor from a high anchor (or bidirectionally) so a floor
  *above* the spawn z is found and the player is lifted onto it.
- Or snap the player to the nearest floor (up or down) for the first frame(s)
  after a zone change, rather than only downward.

Note: the mismatch between the EQEmu zone-point `server_z` and the client's
rendered/collision floor height is the trigger; the one-directional snap is what
makes it unrecoverable.

## Status
Open — assigned for fix.
