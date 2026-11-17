# Moving-Surface Collision: Design

**Date:** 2026-11-17
**Status:** Proposed. Awaiting approval of the approach in §5 and answers to §8 before any code.

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
| The native client moves the platform | Unconfirmed. See §3. |
| Native riding is a live collider, not a special case | Unconfirmed. The KB called this an inference, and nothing in the client code read so far contradicts or proves it. |

Citations of file and line numbers in the old thread are stale and are not reused here.

## 3. What the decompiled native client shows

Read from `decompiled/ghidra/eqgame.exe.c` in the local RoF2 asset directory.

- The packet dispatch for `OP_MoveDoor` (0x08e8) sits in `FUN_004c3250`, which Ghidra failed to
  decompile. The case body is at `0x004ca1dc` in the capstone listing. It looks up a switch object by
  door id through the manager at `0xdcfd00`, then calls the state change `FUN_0050f540` with the action.
- The switch object stores the opentype at `+0x26` and the state at `+0x27`. The state change plays a
  sound per opentype group. Opentype 59 plays sound `0xb9` and does nothing else in that function.
- Travel distances exist for opentypes `100..=102`, `105..=107`, `115` and `116` (`FUN_0050f2c0`).
  No such value is set for 59.
- The spawn routine creates the visual from `"<name>_ACTORDEF"` and gives opentype 59 a branch keyed
  on the wire `incline` read as a float, equal to 1.0. The lift rows carry incline 0.

Reading: the client code traced so far plays a sound and flips a state for opentype 59 and contains
no position math. With a static model, that leaves the native motion mechanism unexplained. It may
sit in the per-frame switch update, which has not been located, or the lift may not rise in this
build at all.

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
- **Travel distance and duration:** not on the wire and not in the model. Candidate source is the
  per-shaft platform-to-top-lever z difference, 75.3, 105.6 and 75.8 units from the KB. Duration needs
  a measurement against the native client, since no source supplies it.
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

- Does the native client move the lift, and how? A timed capture against the Wine RoF2 client
  settles it, and a trace of the per-frame switch update would explain it. This decides whether the
  travel curve is native behavior or an eqoxide choice.
- Is an eqoxide-defined lift motion acceptable if the native mechanism cannot be recovered?
- The glb axis convention for the `FAYLEVATOR` slab.
- How a rider on a boat interacts with the 2.7 s position update when the boat turns between updates.
