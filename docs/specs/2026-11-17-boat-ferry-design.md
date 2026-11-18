# Boat ferry riding design

**Status:** Revised after discovery, awaiting approval. Builds on `2026-11-17-moving-surface-collision-design.md`, which built the surface layer for the Kelethin lift and reserved `SurfaceKind::Boat`.

## 1. Intent

An agent rides the Freeport to Butcherblock ferry as travel between zones, and can tell from its observation when the ferry is there to board. The client must work on any EQEmu server, so it follows the native client and never depends on edited server data.

**Goals**
- The ferry is visible to the client and to the agent, with its position, heading and motion, so the agent decides when to board.
- A character boards by walking onto the dock where the ferry waits, and the server's own ferry mechanism moves it to the next zone.
- The journey continues the way a native player does, through the translocator in `oot`.

**Non-goals:** boats a player steers (races Boat and Rowboat), boat combat, transports other than ferries.

**Anti-goals:** a rider sent to a safe point with no log line, a zone request the server refuses as an invalid zone line, and a ferry that exists on the server but is silently absent from the client.

## 2. What is known

| Fact | Source |
|---|---|
| The server has no zone line for the ferry. `SirensBane` (race 72, npc 10183 in `freporte`, 68228 in `butcher`) follows path grid 23, a 20 waypoint harbor loop of 3547 units with a 60 s pause at waypoint 1. | jimbo `grid_entries`, `npc_types` |
| At waypoint 1 its quest script opens a fixed proximity box of 40 units around the waypoint (not around the moving boat), and at waypoint 2, 112 units on, it clears the box. `event_enter` casts spell 2279, "Portal to Ocean", on the rider, a teleport to `oot`. | jimbo quest `freporte/10183.lua` |
| In `oot` the translocator Narrik moves a rider on to Freeport (`movepc`) or to Butcherblock (`selfcast` of spell 2280, "Portal to Butcher"). | jimbo quest `oot/#Translocator_Narrik.pl` |
| Both moves are server initiated zone changes. eqoxide already follows `OP_RequestClientZoneChange` and replies with `OP_ZoneChange`. | `gameplay.rs:409`, `zoning.cpp` solicited mode |
| A client update with `vehicle_id != 0` carries a boat-local offset that the server rotates by the boat heading. The server drops it while `boat->turning`. | EQEmu `client_packet.cpp:4890`, `:4911` |
| `turning` is set only by `RotateToCommand`, which runs on a waypoint arrival with a pause and a heading of 0 or more, or on the first node of a walking route. Every waypoint of this ferry has heading -1, so it never turns in place and never sets `turning`. A 180 degree turn elsewhere takes about 1 s. | EQEmu `mob_movement_manager.cpp:84`, `mob_ai.cpp:1605`, grid rows |
| `OP_BoardBoat` (0x4211) and `OP_LeaveBoat` (0x7617) only act for controllable boats. | `Mob::IsControllableBoat` |
| The native client carries a rider continuously. It stores the rider's position in boat-local coordinates and reprojects it every frame through the boat's orientation, with the boat spawn origin as pivot. It does not depend on `turning`. | decompile `FUN_008d01c0`, `FUN_008cfea0`, `FUN_008d00e0` |
| After a zone change the native client waits up to 20 s for the boat it was on, then puts the player on it. A rider moved by a spell is not on a vehicle in the new zone. | decompile `eqgame.exe.c:190644` |
| The native client picks a boat model from a race table registered through `call 0x50a440` in `eqgame.exe`. Race 72 draws `SHIP` (or `PRE`), a classic actor in each zone's `<zone>_chr.s3d`. Race 533 draws `shi.eqg` and race 502 draws `row.eqg`, and both carry an authored collision hull (`col_shi.mod`, `col_row.mod`). `SHIP` has no collision file. | `eqgame.exe` registrations, `shi.eqg`, `row.eqg`, `freporte_chr.s3d` |
| `SirensBane` has `bodytype` 11 (NoTarget). The client drops every spawn with bodytype 11 or 60 at registration, as an untargetable utility spawn (#478), so the ferry never enters the entity table. | `packet_handler.rs:3508`, jimbo `npc_types` |
| eqoxide draws every boat race with the rowboat model `boat.glb`, never `SHIP`. | `race_class.rs:36`, `models.rs` |
| `Observation.visible` entries carry no boat flag and no velocity, and the vision filter keeps entities within 500 units and in line of sight. | `vision-filter/src/lib.rs` |

## 3. Approaches

1. **Proximity boarding (recommended, stage 1).** Make the ferry visible and observable, and let the agent walk onto the dock. The server's box does the rest. No deck, carry or vehicle wire is needed for this ferry, which also holds for the PEQ ferries that use the same script pattern.
2. **Native faithful deck (stage 2, deferred).** A boat collision surface with a carry, boat-local offsets on the wire and board messages. It matters only for boats the server expects a rider to stand on, such as steerable boats, and needs a deck source that does not exist for `SHIP`.
3. **Zone-line shortcut.** Rejected. This server has no zone line, and writing one would break other servers.

## 4. Design (stage 1)

- **Keep the ferry.** Registration drops bodytype 11 and 60 spawns, except races that `is_boat_race` names. A boat stays in the entity table with its race, position and heading. The native client renders such ships although the player cannot target them, so the exemption follows it. The entity is marked untargetable.
- **Agent view.** `VisibleEntity` gains `is_boat: bool` and `velocity: [f32; 3]` in units per second, taken from the last two position updates the way `SurfaceView.velocity` is. A ferry is a `visible` entry sorted by distance like any entity, and the agent derives the rest.
  - Present: the ferry appears in `visible`.
  - Absent: it is outside 500 units or out of line of sight, which is what a player sees too. No claim is made about where it is.
  - Docked: speed near zero while it is within the dock box. Leaving: speed rises from zero.
- **Boarding.** The agent walks onto the dock near the ferry's stop (-1084, -32) while the box is active, which is from the ferry's arrival at waypoint 1 until it reaches waypoint 2. The script gives a box of x -1124 to -1044 and y -72 to 8, but the live trigger did not match it exactly (§6), so the plan measures the edge. The server evaluates a character only when it moves, so one that already stands inside must step. No client code is involved.
- **After the zone.** The character arrives in `oot` and continues through the native dialogue with Narrik (`hail`, then the `travel to butcherblock` saylink).
- **Model.** Drawing race 72 as `SHIP` from the zone archive is a separate rendering change. It is not needed to board and is not part of stage 1.

## 5. Staging and test

Stage 1 is one stage. Unit tests cover the boat exemption of the bodytype filter (a boat is kept, a utility spawn of another race is still dropped), `is_boat` and `velocity` on `VisibleEntity`, and the protocol version. Live: stand at the dock, watch the ferry appear in `visible` and dock, walk into the box, and arrive in `oot`.

Stage 2 stays a separate design. Its first task decides the deck source, either an authored box per ferry or a surface derived from the `SHIP` mesh.

## 6. Discovery results

- **Confirmed live (2026-11-18).** The test character stood at the dock at (-1060, -30, -72) with nothing happening. After stepping east to x=-969 and walking back west it changed zone to `oot`, with the client at x=-1024.8 on the last frame in `freporte`, and with no ferry in the client at any time. `interact.hail` on `SirensBane` returned "No NPC matching", so the ferry was never in the entity table. This shows that the trigger needs no deck and that the client drops the ferry.
- **Trigger window.** A second pass in 10 unit steps from x=-967 to x=-1376 along the same line caused no zone change, so the box is active only part of the loop. The first trigger also happened 20 units east of the computed box edge (x=-1044), and the cause is unknown. The client position may lag the server, or the box may be wider than the script suggests.
- **Confirmed by code.** `turning` never fires for this ferry, and the native client carries riders by continuous reprojection (§2).
- **Deck, `SHIP`.** The mesh has 662 vertices and 532 polygons, all solid, with bounds x -64 to 197, y -113 to 110 and z -38 to 131 around the model origin. Upward facing surfaces sit at z = -2 and z = -13, and the hull bottom at z = -38. Whether the native client collides against this mesh is not confirmed, and no authored hull exists. The shi and row boats have authored hulls.
- **Loop timing.** Nine of the 20 waypoints lie within 500 units of the dock, the farthest 1191 units away. Speed is the npc `runspeed` of 1.25, whose conversion to units per second is not yet measured. The plan measures it from position updates.
- **Not confirmed.** The `oot` arrival point, how long Narrik takes to act without dialogue (the test character stayed 45 s with no move), and whether the native client keeps the bodytype 11 ferry visible by the same rule (taken from how the game plays, not from the decompile).

## 7. Open questions

- Whether stage 2 is wanted at all, given that proximity boarding covers this ferry.
- Whether `is_boat` should also appear on `SurfaceView` once boats become surfaces.
- Whether the box z range is unbounded on the server. The test character was within 2 units of the ferry height, so it does not tell.
- How long the box stays active and where its true edge lies, which the plan measures once the ferry is visible to the client.
