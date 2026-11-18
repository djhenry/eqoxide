# Boat ferry riding design

**Status:** Draft, awaiting approval. Builds on `2026-11-17-moving-surface-collision-design.md`, which built the surface layer for the Kelethin lift and reserved `SurfaceKind::Boat`.

## 1. Intent

An agent rides the Freeport to Butcherblock ferry as travel between zones. The user chose this over standing on a moving deck, because zoning aboard is the use that matters for an agent exploring the world.

**Goals**
- A character walks onto a ferry, stays on the deck while it moves and turns, and walks off.
- The character zones aboard, appears on the same ferry in the next zone and walks off at the far dock.
- An agent sees the ferry in `Observation.surfaces` with its position and velocity, and `own.standing_on` names it.

**Non-goals:** boats a player steers (races Boat and Rowboat), boat combat, rendering changes, transports other than ferries.

**Anti-goals:** a rider left behind in the water, a rider sent to a safe point with no log line, or a zone request the server refuses as an invalid zone line.

## 2. What is known

| Fact | Source |
|---|---|
| A client update with `vehicle_id != 0` carries x, y, z as an offset from that mob. The server rotates it by the boat heading and adds the boat position. | `EQEmu zone/client_packet.cpp:4891` |
| The server drops such an update while the boat is turning. | same, `boat->turning` |
| The server accepts the offset for any mob id. `OP_BoardBoat` only works for races Boat and Rowboat. | `Mob::IsControllableBoat` |
| An unsolicited zone request finds its zone point from the client's world position, which is boat plus offset. | `zone/zoning.cpp:90` |
| The native client sends `OP_BoardBoat` (0x4211, the boat name) and `OP_LeaveBoat` (0x7617) as it steps on and off a vehicle. | decompile `eqgame.exe.c:65668` |
| After a zone change the native client looks the boat up by name, waits up to 20 s for it to arrive, then puts the player on it. Otherwise it sends the player to a safe point. | decompile `eqgame.exe.c:190644` |
| The server sends no boat state in the zone-in packets. The boat name is client memory. | `common/patches/rof2_structs.h` |
| eqoxide skips incoming `vehicle_id` and writes `0` on every outgoing update. Boats are entities that float with no collision. | `protocol/mod.rs:1677`, `action_loop.rs:95` |
| The server streams boat positions about every 2.7 s. `smooth_entity_motion` glides the displayed position between updates. | design doc 1, §4 |

## 3. Approaches

1. **Native faithful, in two stages (chosen).** Stage 1 is one zone: a boat collision surface with a carry, boat-local offsets on the wire, and board and leave messages. Stage 2 adds the cross-zone handoff.
2. **Parent only.** No deck collision, the rider is glued at a fixed offset and boards through an op. Less work, but the agent cannot walk the deck.
3. **Zone-line shortcut.** Skips the ride.

## 4. Design

- **Boat surface.** Each frame a boat entity becomes a `Surface` of kind `Boat` in the layer, placed by the entity's displayed position and heading, the same pose the renderer draws. Its velocity comes from the last two position updates. `carries_riders` is true.
- **Carry.** A boat moves horizontally, so the lift rule (re-query the floor) is not enough. Before each controller step, a rider whose `standing_on` is a boat receives the boat's pose change since the previous frame, a translation and a rotation about the boat center. The controller gains one method for that. The controller position stays in world coordinates, so zone line detection and the observation need no change.
- **Wire.** While `standing_on` is a boat, the update sets `vehicle_id` to the boat spawn id. The position is the world offset from the boat, rotated by the inverse of the server's formula, and the heading is the world heading minus the boat heading. A golden test pins the transform against the server's code. Board and leave messages follow the native client, sent once per change. The server ignores them for a ferry, so they cost nothing.
- **Zone while aboard.** The existing client-side zone line detection fires from the world position, which matches what the server computes. The net thread keeps the boat name through the zone change.
- **Arrival (stage 2).** In the new zone the net thread waits up to 20 s for an entity with that name, places the character at the same deck offset and resumes riding. On timeout it logs and leaves the character where the server put it.
- **Agent view.** No new fields. Boats appear in `surfaces` and `own.standing_on` holds the spawn id in the current zone.

## 5. Staging and test

Stage 1 ships and gets a live test before stage 2 starts. Unit tests cover the transform against the server formula, the carry on a moving and turning deck, a rider who walks off, and the wire encoding with `vehicle_id`. Live: board at the Freeport dock and ride to the zone line, then stage 2 to Butcherblock.

## 6. Recon before building

These decide the shape of stage 1 and cannot be read from the repo.
- Which zone hosts the ferry, and the race, name pattern and spawn id of its boat.
- Whether the boat model has a deck to stand on. The model comes from the character glb. If it has none, a deck box is authored per ferry, and approach 2 becomes the fallback.
- Where the zone line lies on the route, and where the server places a character that arrives by boat.
- Whether the boat reports `turning` often enough to drop a rider's updates.

## 7. Open questions

- How the rider's z maps while aboard, because boats skip the wire z offset (#548) and players do not.
- Whether the boat pose between two 2.7 s updates should extrapolate, and how that is kept in step with what the server computes.
