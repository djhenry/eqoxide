# Moving-Surface Collision: Design

**Date:** 2026-11-17
**Status:** Proposed. Awaiting approval of the approach in §4 before any code.

Tracks #194 (gap 3, the rider and vehicle mechanic) and the collision representation questions that
#240 raised before it moved to the harness repository. Motivation: `docs/specs/2026-11-17-visible-terrain-observation-design.md`.

## 1. Goal

A character can stand on a moving surface, move with it, and leave it, and an agent can detect all
three. The first surface is a boat. The same layer must carry a rising platform without a redesign.

## 2. What the code does today

- `Collision` is a per-zone static triangle set with a cell grid and static caches. The controller
  takes `&Collision` in `CharacterController::step`, so nothing in it can move.
- A boat is an entity (`is_boat`). It floats at its server z and has no collision.
- A door is `Door { is_open, .. }` with a render-side easing in `src/app.rs`. No door is in collision.
- The server streams NPC positions about every 2.7 s. `smooth_entity_motion` glides the displayed
  position between updates.
- Incoming `vehicle_id` is skipped on decode, and outgoing `vehicle_id` is written as `0`
  (`eqoxide-protocol` `mod.rs`, `eqoxide-net` `action_loop.rs`). The server rotates a rider's boat-local
  offsets by the boat heading and adds the boat position (EQEmu `client_packet.cpp`, per the issue).

## 3. Goals and non-goals

- **Goals:** ride a boat, board and leave it, expose the motion to agents, keep one pose per
  surface shared by renderer, controller and agent.
- **Non-goals:** driving a controllable rowboat (`OP_ControlBoat`), a ship model change, and
  routing onto a surface (planning stays in the harness).

## 4. Approaches

1. **Boat-only rider state.** Track `vehicle_id` and boat-local position in the controller and
   skip collision. Smallest change. It leaves lifts uncovered and gives ray queries (line of sight,
   the visible terrain grid) no surface to hit.
2. **A moving-surface layer beside `Collision` (recommended).** A set of surfaces, each with a
   local-frame shape and a pose that changes over time. The controller and the ray queries consult
   the layer in addition to the static `Collision`. Boats and lifts are two sources for the same
   layer.
3. **Merge moving triangles into `Collision` each tick.** Rejected. The cell grid and clearance
   field are built once per zone, so a per-tick rebuild costs too much.

## 5. Proposed shape of approach 2

- **Surface:** an id, a kind (boat or platform), a local shape, and a pose of position, heading and
  velocity. One shared snapshot owns the poses.
- **Pose source:** a boat takes the smoothed entity position, the pose the renderer already draws.
  A lift takes door state and a deterministic travel curve. One source per surface prevents the
  deck and the feet from diverging.
- **Queries:** a floor probe under a point and a ray test, both including surfaces. Surface hits
  carry the surface id, so a caller can tell a deck from terrain.
- **Controller:** a character that stands on a surface gets the surface pose delta applied before
  each step. Support is re-detected after the step. Leaving the surface clears it.
- **Protocol:** decode `vehicle_id` and parent the rider. Send `vehicle_id` and boat-local offsets
  while aboard. Board and leave opcodes arrive with controllable boats and stay out of scope.
- **Shape source:** a deck volume for a boat. Whether it comes from the boat model or from a fixed
  footprint per race is open (§7).

## 6. Agent detectability

Motion must be observable. The data to expose, whichever channel carries it:

- `own.standing_on` as an optional surface id, so the agent can work in the surface frame.
- A surface list with kind, footprint, position, heading and observed velocity.
- Board and leave transitions in the event feed.
- An explicit statement when the controller cannot carry the rider, in the way `held` reports a
  frozen controller.

The agent API is being refactored in another session. This design defines the data and leaves the
wire placement to that work.

## 7. Open questions

- Scope of the first change: boats only, or boats and the Kelethin lift together.
- Where the boat deck shape comes from, and whether a baked boat model exists on the asset server
  (the #194 thread left that unconfirmed).
- How a lift's pose is derived, since door state carries no timing beyond the open flag.
- How riding interacts with the 2.7 s update interval when the boat turns between updates.
