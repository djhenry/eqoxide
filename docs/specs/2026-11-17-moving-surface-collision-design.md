# Moving-Surface Collision: Design

**Date:** 2026-11-17
**Status:** Proposed. The native lift motion is recovered (§3). Awaiting approval of the approach in §5 and answers to §8 before any code.

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
| Native riding is a live collider, not a special case | Unconfirmed. The KB called this an inference, and the client code read so far does not address rider carrying. |

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

Applied to the KB's door table, `door_param` is 68, 98 and 69 for the three platforms. Those distances
sit 7 to 8 units under the platform-to-top-lever gaps of 75.3, 105.6 and 75.8. The ride takes 7.6,
10.9 and 7.7 seconds at 9 units per second. Both sets of numbers come from the KB's earlier DB query
and were not re-queried. The levers share opentype 59 with `door_param` 1, so by this branch they
travel one unit.

**Not resolved.** The client code read does not show how a rider is carried. The game code moves the
switch position, and the actor follows it, so a collidable actor would move the standing surface. This
is unconfirmed. The handling of a lift that spawns already open is also unread, and the speed rests on
the assumed millisecond clock.

## 4. What the code does today

- `Collision` is a per-zone static triangle set with a cell grid and static caches. The controller
  takes `&Collision` in `CharacterController::step`, so nothing in it can move.
- A door is `Door { is_open, .. }`. The open fraction is eased on `App` (`ease_door_frac`, 0.5 s) and
  read only by the renderer. Neither the net thread nor the agent observation sees it.
- A boat is an entity (`is_boat`) that floats at its server z with no collision.
- The server streams NPC positions about every 2.7 s. `smooth_entity_motion` glides the displayed
  position between updates.
- Incoming `vehicle_id` is skipped on decode and outgoing `vehicle_id` is written as `0`.
- The `FAYLEVATOR` mesh is 38 unique vertices and 12 triangles. A 30 by 30 slab 3 units thick at
  model x 80 to 110 is the candidate standing surface. The axis convention of the glb against the
  renderer's z-up placement is not yet verified.

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
- **Queries:** a floor probe and a ray test that include surfaces and return the surface id.
- **Controller:** a character standing on a surface receives the surface pose delta before each step.
  Support is re-detected after the step.
- **Boats later:** decode `vehicle_id`, parent the rider, and send boat-local offsets while aboard.

## 7. Agent detectability

- `own.standing_on` as an optional surface id.
- A surface list with kind, footprint, position, heading and observed velocity.
- Board and leave transitions in the event feed.
- An explicit statement when the controller cannot carry the rider, as `held` does for a frozen
  controller.

The agent API is being refactored in another session. This design defines the data and leaves the
wire placement to that work.

## 8. Open questions

- Is the 9 units per second rate right? A timed ride in the Wine RoF2 client against the 7.6 s
  expected for the shortest shaft confirms the millisecond clock assumption.
- Does the native client carry riders through a live collider on the moving actor? Nothing read
  says. A test on the Wine client, riding to the top, would show it.
- The glb axis convention for the `FAYLEVATOR` slab.
- How a rider on a boat interacts with the 2.7 s position update when the boat turns between updates.
