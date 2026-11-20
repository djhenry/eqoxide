# Moving-Surface Collision: Design

**Date:** 2026-11-17
**Status:** Approved 2026-11-17 (approach 2 of §5). The lift is implemented. Boats and the open questions in §8 remain.

Tracks #194 (gap 3, the rider and vehicle mechanic) and the collision representation questions that
#240 raised before it moved to the harness repository. Motivation: `docs/specs/2026-11-17-visible-terrain-observation-design.md`.

## 1. Goal

A character can stand on a moving surface, move with it, and leave it, and an agent can detect all
three. The first surface is the Kelethin lift in `gfaydark`, because a lift is deterministic and
testable. A boat is the second surface and must fit the same layer without a redesign.

## 2. Earlier claims, re-checked on 2026-11-17

The #194 thread and the `eq_kb` note `kelethin-lift-and-doors.md` predate the workspace refactor, so
each claim was checked against current code, assets and the decompiled RoF2 client.

| Claim | Result |
|---|---|
| The lift is a door, not the boat `vehicle_id` mechanic | Confirmed. The client routes `OP_MoveDoor` to a switch lookup by door id, then a state change. Nothing boat related is involved. |
| The lift is opentype 59, a `FAYLEVATOR` platform plus two `FELE2` levers per shaft | Confirmed against the KB's DB query. `gfaydark_doors.glb` contains `FAYLEVATOR` and `FELE2`. The DB itself was not re-queried, since the server moved to jimbo.lan. |
| The server sends no position, travel distance or speed | Confirmed in the EQEmu door code. Opentype 59 is only special-cased in the NPC open exclusion. |
| eqoxide renders 59 as a hinge swing | Confirmed. `crates/eqoxide-renderer/src/pass.rs` handles only `100..=119` and `11..=15`, and 59 takes the default rotation. |
| No door is in collision | Confirmed. `crates/eqoxide-zone-geometry/src/collision.rs` has no door geometry. |
| The lift model has no animation | Confirmed twice. `gfaydark_doors.glb` has no animations and `gfaydark_obj.s3d` holds `FAYLEVATOR_DMSPRITEDEF` and `FAYLEVATOR_ACTORDEF` with no track fragment. Only the two flag models carry `DMTRACKDEF`. |
| The wire carries no travel distance, so the native distance is unrecoverable | Refuted. The distance is the wire `door_param`, which eqoxide already parses. See §3. |
| The native client moves the platform | Confirmed. The client translates the door up its z axis in game code every frame. See §3. |
| The native travel speed and duration are unrecoverable | Refuted. The speed is a client constant of 9 units per second. See §3. |
| Native riding is a live collider, not a special case | Confirmed in substance. The client applies no rider delta. A rider follows because the floor is re-queried every frame. See §3. |

Citations of file and line numbers in the old thread are stale and are not reused here.

## 3. What the decompiled native client shows

Read from `decompiled/ghidra/eqgame.exe.c` in the local RoF2 asset directory. Function names are the
Ghidra placeholders. The three most relevant functions are the door record loader, the state change
and the per-frame door update.

**Door record.** `FUN_00511990` copies the wire door record into a switch object. The opentype lands
at `+0x26` and the state at `+0x27`. The wire position lands at `+0x28..+0x34` and is copied to a
separate current position at `+0x44..+0x54`. The wire `door_param` (record offset `0x40`) lands at
`+0x78`. The `OP_MoveDoor` case at `0x004ca1dc` looks a switch up by door id and calls the state
change `FUN_0050f540`.

**State change.** `FUN_0050f540` stores the new state and plays a sound per opentype group. It does
not move the door.

**Per-frame update.** `FUN_0048d530` runs every frame and moves the current position of each switch.
It takes the clock difference since the previous frame (assumed to be milliseconds) and multiplies it
by 0.02. Opentype 59 shares a branch with opentypes 60 to 77 and moves the current z at `+0x4c` as
follows.

- The travel distance is `FUN_0050ff20`, which returns the `door_param` at `+0x78` when positive, and
  its absolute value when below -3.
- While the state is opening (2), z rises by `0.45 * scaled_time` from the original z at `+0x30`, which is
  9 units per second. At the full distance the update clamps z to the top and sets the state to open (1).
- While the state is closing (3), z falls at the same rate. At the original z the update clamps and
  sets the state to closed (0).
- A door whose state differs from its rest state (`+0x58`) reverts on a timer. State 4 reverts after
  60001 ms. Other states revert after the per-opentype time from `FUN_0050f370`, 20000 ms for opentype 59 with
  `door_param` up to 170. The timers use the same clock as the motion, so the clock is in milliseconds.

Applied to the KB's door table, `door_param` is 68, 98 and 69 for the three platforms. Those distances
sit 7 to 8 units under the platform-to-top-lever gaps of 75.3, 105.6 and 75.8. The ride takes 7.6,
10.9 and 7.7 seconds at 9 units per second. A lever base sits 6.75 to 7.6 units above the platform
spawn z at the bottom and above the spawn z plus `door_param` at the top, so the platform deck is the
door z. The live client confirmed 68, 98 and 69 in §9. The levers share opentype 59 with `door_param` 1, so by this branch they
travel one unit.

**Riding.** The client applies no position delta to a rider and no horizontal carry. After it moves the
actor, the update runs a loop for opentypes that `FUN_0050ff80` marks as carriers, which includes 59,
while the state is 2, 3 or 4. The loop visits every spawn whose horizontal distance to the door is
under the spawn radius plus the door actor radius plus 20 (`DAT_00ac1a6c`, with the gate
`DAT_00ac1a68` initially 1). For each one it calls `FUN_00506310`, which writes 0 to the counter at
spawn `+0x1060`.

That counter belongs to the per-spawn floor query `FUN_00507230(x, y, z)`. The query caches the floor
height and returns the cache once the counter passes 2. Zeroing the counter forces a fresh sweep
against the collision scene every frame. A spawn standing on a rising platform therefore follows it
because its floor is re-read each frame. This applies to every spawn, not only the player.

**Not resolved.** The floor sweep `FUN_004767a0` is not decoded. That the sweep includes door actors is
inferred from the loop above, which would have no purpose otherwise. Which of the model's faces the
sweep tests is also unread. A platform spawns with `open` and `invert` set, and the live client showed it resting at the top.

## 4. What the code does today

- `Collision` is a per-zone static triangle set with a cell grid and static caches. The controller
  takes `&Collision` in `CharacterController::step`, so nothing in it can move.
- A door is `Door { is_open, .. }`. The open fraction is eased on `App` (`ease_door_frac`, 0.5 s) and
  read only by the renderer. Neither the net thread nor the agent observation sees it.
- A boat is an entity (`is_boat`) that floats at its server z with no collision.
- The server streams NPC positions about every 2.7 s. `smooth_entity_motion` glides the displayed
  position between updates.
- Incoming `vehicle_id` is skipped on decode and outgoing `vehicle_id` is written as `0`.
- The `FAYLEVATOR` mesh is 88 vertices and 62 triangles in three pieces, all with face flags 0 (no
  passable faces). A hexagonal platform about 50 across and 6.5 thick (`dshaft1.bmp`) is centered on
  the model origin with its top at local z -0.1. A tapering strip up to 5 units thick (`dshaft.bmp`) runs
  from its edge to a 30 by 30 square, 3 units thick (`fayfloor.bmp`), centered about 95 units out.
  Which pieces the native client treats as the floor is unknown, so collision uses all 62 triangles.
- Axis convention, checked against the WLD fragment. The glb stores `[wld x, wld z (up), wld y]` with
  the WLD mesh `center` (44.55, -5.21, -2.30) already folded in. The door loader maps glb
  `[p0, p1, p2]` to door-local `[p2, p0, p1]`, so local x is wld y, local y is wld x and local z is up.
  Collision must apply the same mapping and the same placement matrix as the renderer.

## 5. Approaches

1. **Lift-only controller special case.** Carry the player by the platform delta while the feet are
   over its footprint. Smallest change. Boats, ray queries and the visible terrain grid get nothing.
2. **A moving-surface layer beside `Collision` (recommended).** A set of surfaces, each with a local
   shape and a pose that changes over time. The controller and the ray queries consult the layer in
   addition to the static `Collision`. Lifts and boats are two sources for the layer.
3. **Merge moving triangles into `Collision` each tick.** Rejected. The cell grid and clearance field
   are built once per zone, so a per-tick rebuild costs too much.

## 6. Proposed shape of approach 2

- **Surface:** an id, a kind (`Platform` or `Boat`), a local shape, and a pose of position, heading
  and velocity.
- **Pose source for a lift:** door state plus a travel curve. The pose must be computed where the
  controller, the renderer and the observation can all read it. Today the easing lives only on `App`,
  so the curve moves into shared state and `ease_door_frac` is replaced by it.
- **Travel distance and duration:** the distance is the wire `door_param`. The curve is linear at 9
  units per second, moving z from the spawn z while the state is opening and back while closing. The
  speed is a client constant and does not come from the server.
- **Collision shape:** every triangle of the door model, placed by the renderer's matrix and moved
  by the surface z.
- **Queries:** a floor probe and a ray test that include surfaces and return the surface id.
- **Controller:** a lift moves only in z, so a rider needs no carry. The floor probe reads the
  surface at its current z each step, and the existing step-up of 2.0 plus the snap tolerance of 0.5
  follows a platform that moves 0.15 units per frame at 60 Hz. This matches the native mechanism.
- **Boats later:** a boat moves horizontally, so it needs a carry. Decode `vehicle_id`, parent the
  rider, and send boat-local offsets while aboard.

## 7. Agent detectability

Built as `own.standing_on` (an optional surface id) and `Observation.surfaces`, a nearest-first list
of `SurfaceView` with id, kind, position, footprint radius, velocity and `carries_riders`. Both new
fields are additive and default when absent. The velocity is the commanded velocity of the surface
rather than a value an agent derives from two samples.

Board and leave events were not built. An agent diffs `standing_on` between ticks, which needs no
event feed. `carries_riders` states whether a surface carries a rider, and it is true for every
platform and every boat.

The agent API is being refactored in another session. The data lives in `eqoxide-agent-protocol`'s
`observation.rs`, so moving the wire placement is a change to that file and to the builder in
`eqoxide-agent-plugin-host`'s `observation_builder.rs`.

## 8. Open questions

- Which faces of the door model does the native floor sweep test? A ride in the Wine client that
  steps on the strip and the square would show it. Until then collision uses all faces.
- Does the controller tick fast enough to keep the rider grounded on a platform descending at 9
  units per second? Answered for 20 fps and above by `src/lift_ride_tests.rs`. At about 10 fps a
  step descends 0.9 units, which exceeds the snap tolerance of 0.5, so a rider could briefly leave
  the floor. No test covers that rate.
- How a rider on a boat interacts with the 2.7 s position update when the boat turns between updates.

## 9. Live test

Run on the `jimbo.lan` server with a GM character in `gfaydark` shaft 3, platform 80. These results
were observed and are not covered by unit tests.

- `door_param` was 68, 98 and 69 for platforms 69, 77 and 80. Each platform spawned with `open` and
  `invert` set, and rested at the top.
- A lever click made the platform descend at 9 units per second and stop after about 7.7 seconds. The
  rider kept a constant offset from the platform for the whole ride, with `standing_on` set to the
  platform id. At the bottom the deck sits level with the zone floor, so `standing_on` is `None`
  there.
- The server sends the platform `OP_MoveDoor` action 3 on every click and never action 2. It
  already flips the action for an inverted door, and the native client applies the action unchanged.
  `apply_move_door` therefore sets the `is_open` of a lift door (opentype 59) from `action == 2`
  without an XOR, for an inverted and a non-inverted lift alike. Every other door keeps the XOR with
  `invert_state`, because only the lift was tested live.
- The platform returns up about 20 seconds after it stops, and the server sends no packet for that.
  The client reproduces the native revert timer in `lift_revert_delay` and `WorldState::revert_lifts`.
  The rest state is `Door::invert_state`.
- The drawn platform was rotated a quarter turn from the ramp. `door_placement` adds a quarter turn
  that suits hinged doors. A lift deck is 132 units long on door-local y, and the two levers of a
  shaft lie along that axis. `lift_placement` omits the quarter turn and turns clockwise by the
  heading. The renderer and the collision surfaces both use it. The result was checked by the lever
  positions in `door.rs` tests, and the ramp alignment still needs a visual check.
- The server accepted clicks from about 113 units away, so no click range was observed.
