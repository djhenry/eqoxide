# Boat ferry riding design

**Status:** Revised after recon, awaiting approval. Builds on `2026-11-17-moving-surface-collision-design.md`, which built the surface layer for the Kelethin lift and reserved `SurfaceKind::Boat`.

## 1. Intent

An agent rides the Freeport to Butcherblock ferry as travel between zones. The user chose this over standing on a moving deck, because zoning aboard is the use that matters for an agent exploring the world. The client must work on any EQEmu server, so it follows the native client and never depends on edited server data.

**Goals**
- A character walks onto a ferry, stays on the deck while it moves and turns, and walks off.
- The character stays aboard until the server's own ferry mechanism moves it to the next zone, then continues the journey the way a native player does.
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
| The server has no zone line for the ferry. The Freeport boat, `SirensBane` (race 72, npc 10183, a looping path grid with a 60 s dock pause), has a quest script. At waypoint 1 it opens a proximity box of 40 units around itself, and `event_enter` casts spell 2279, "Portal to Ocean", on the rider, a teleport to `oot` (spell effect 83). At waypoint 2 it clears the box. | jimbo quest `freporte/10183.lua`, `spells_new` 2279 |
| In `oot` the translocator Narrik moves a rider on to Freeport (`movepc`) or to Butcherblock (`selfcast` of spell 2280, "Portal to Butcher"). | jimbo quest `oot/#Translocator_Narrik.pl` |
| Both moves are server-initiated zone changes. eqoxide already follows `OP_RequestClientZoneChange` and replies with `OP_ZoneChange`, which the server accepts without a zone line. | `gameplay.rs:409`, `zoning.cpp` solicited mode |
| The native vehicle handoff (`vehiclename`, `MSG_LOCATE_VEHICLE`, a 20 s wait for the boat to arrive) only runs for a client that zones while a vehicle is set. A rider moved by a spell is not on a vehicle in the new zone. The server never sends boat state. | decompile `eqgame.exe.c:190644`, `rof2_structs.h` |
| The server streams boat positions about every 2.7 s. `smooth_entity_motion` glides the displayed position between updates. | design doc 1, §4 |

## 3. Approaches

1. **Native faithful (chosen).** A boat collision surface with a carry, boat-local offsets on the wire, and board and leave messages. Recon showed the cross-zone step is a server teleport spell that the client already follows, so the handoff stage of the earlier draft is dropped.
2. **Parent only.** No deck collision, the rider is glued at a fixed offset and boards through an op. Less work, but the agent cannot walk the deck.
3. **Zone-line shortcut.** Skips the ride, and has no zone line to use on this server.

## 4. Design

- **Boat surface.** Each frame a boat entity becomes a `Surface` of kind `Boat` in the layer, placed by the entity's displayed position and heading, the same pose the renderer draws. Its velocity comes from the last two position updates. `carries_riders` is true.
- **Carry.** A boat moves horizontally, so the lift rule (re-query the floor) is not enough. Before each controller step, a rider whose `standing_on` is a boat receives the boat's pose change since the previous frame, a translation and a rotation about the boat center. The controller gains one method for that. The controller position stays in world coordinates, so zone line detection and the observation need no change.
- **Wire.** While `standing_on` is a boat, the update sets `vehicle_id` to the boat spawn id. The position is the world offset from the boat, rotated by the inverse of the server's formula, and the heading is the world heading minus the boat heading. A golden test pins the transform against the server's code. Board and leave messages follow the native client, sent once per change. The server ignores them for a ferry, so they cost nothing.
- **Zone while aboard.** The server moves the rider with its teleport spell once the boat reaches the proximity waypoint. The rider must therefore be on the deck, in world coordinates inside the box, which the carry provides. The client sends no zone request of its own.
- **After the zone.** The client clears `standing_on` and the boat name on any zone change and waits for nothing. The character arrives where the server puts it, in `oot`, and the journey continues through the native dialogue with Narrik (`hail`, then the `travel to butcherblock` saylink).
- **Agent view.** No new fields. Boats appear in `surfaces` and `own.standing_on` holds the spawn id in the current zone.

## 5. Staging and test

One stage. Unit tests cover the transform against the server formula, the carry on a moving and turning deck, a rider who walks off, and the wire encoding with `vehicle_id`. Live: board `SirensBane` at the Freeport dock, ride until the spell teleports the character to `oot`, then hail Narrik and travel to Butcherblock.

## 6. Recon results

- **Host and boat.** `freporte` hosts `SirensBane` (race 72, npc 10183 and two copies, 68228 in `butcher` and 69134). Its path grid 23 is a harbor loop with a 60 s dock pause.
- **Zone line.** None exists between `freporte`, `butcher` and `oot`. The transfer is the teleport spell above, so no server data changes are needed.
- **Boat visibility.** The boat was not in `Observation.visible` when the GM character stood at the dock, so it may be away on its loop. The live test must confirm the boat spawns and which model it uses.
- **Not yet confirmed.** Whether the boat model has a deck to stand on, and the `turning` rate. Both are checked in the first plan task, with a deck box authored per ferry as the fallback.

## 7. Open questions

- How the rider's z maps while aboard, because boats skip the wire z offset (#548) and players do not.
- Whether the boat pose between two 2.7 s updates should extrapolate, and how that is kept in step with what the server computes.
- Where in `oot` the rider arrives, and whether the old zone's boat name must be cleared on the net thread when a spell zone change interrupts a ride.
