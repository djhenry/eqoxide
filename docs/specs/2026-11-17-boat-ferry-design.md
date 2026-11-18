# Boat ferry riding design

**Status:** Stage 1 built and measured live on 2026-11-18. Builds on `2026-11-17-moving-surface-collision-design.md`, which built the surface layer for the Kelethin lift and reserved `SurfaceKind::Boat`.

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
- **Boarding.** The agent walks onto the dock near the ferry's stop (-1084, -32) while the box is active, which the quest script gives as the ferry's arrival at waypoint 1 until it reaches waypoint 2. Live runs did not measure the start and end of that window (§6). The script gives a box of x -1124 to -1044 and y -72 to 8, and the live east edge matched it (§6). The server fires on an outside to inside crossing at a client update, so a character that already stands inside when the ferry docks must step out and back in. No client code is involved.
- **After the zone.** The character arrives in `oot` and continues through the native dialogue with Narrik (`hail`, then the `travel to butcherblock` saylink).
- **Model.** Drawing race 72 as `SHIP` from the zone archive is a separate rendering change. It is not needed to board and is not part of stage 1.

## 5. Staging and test

Stage 1 is one stage. Unit tests cover the boat exemption of the bodytype filter (a boat is kept, a utility spawn of another race is still dropped), `is_boat` and `velocity` on `VisibleEntity`, and the additive wire fields, which do not bump the protocol version. Live: stand at the dock, watch the ferry appear in `visible` and dock, walk into the box, and arrive in `oot`.

Stage 2 stays a separate design. Its first task decides the deck source, either an authored box per ferry or a surface derived from the `SHIP` mesh.

## 6. Discovery results

- **Confirmed live (2026-11-18, before stage 1).** The test character stood at the dock at (-1060, -30, -72) with nothing happening. After stepping east to x=-969 and walking back west it changed zone to `oot`, and the client had no ferry at any time. `interact.hail` on `SirensBane` returned "No NPC matching", so the ferry was never in the entity table. This shows that the trigger needs no deck and that the client dropped the ferry.
- **Visibility (live, stage 1 build).** The ferry shows in `visible` as `SirensBane000` with race `SHP`, `is_boat` true and a velocity. It appears at 485 to 498 units and drops out at 490 to 499 units, so the 500 unit range binds. Line of sight breaks near the dock, so the entry flickers at 56 to 121 units from a character on the street. From (-960, -30) the ferry appears only about 3 s before it docks. This server runs every static spawn twice, so `visible` lists `SirensBane000` and `SirensBane001` at identical positions with identical velocity in every observation.
- **Motion.** Cruise speed is 10.4 units/s (median over 83 samples, range 9.7 to 14.2 at waypoint turns), which is 8.3 units/s for each unit of the npc `runspeed` of 1.25. Docked reads 0.0 and the first update after the pause already reads full speed, so no ramp shows. Position updates arrive every 1.5 s (median over 76 intervals, longest 1.95 s at an observation step of 0.15 s), about 15.6 units apart.
- **Zero velocity at waypoints.** At 6 of the waypoints in range (2, 3, 5, 17, 18 and 19) the sampled velocity reads 0 for one update interval, with the ferry still cruising. The position repeats across two samples, which is consistent with a second update at the waypoint. `MOTION_STALE` (3 s) is not the cause, since it exceeds the longest update interval, and it stays unchanged.
- **Loop time.** The ferry reached the dock at 1795042571.8 and again at 1795042974.8 (epoch seconds), so the loop takes 403 s with one update interval of error. The pause from arrival to the first moved update is 61.9 s. The path is 3547 units, which at 10.4 units/s is 341 s and with the pause gives 403 s. Two longer intervals (468 s and about 450 s) each contain a stretch of 110 to 130 s with no character in `freporte`. In both, the extra time matches the absence minus 60 s to within 7 s, and the zone log reports "Zone has gone idle after 60 seconds". A third interval with a 36 s absence took about 400 s. The loop is likely to stand still in an empty zone after a 60 s grace, which was not tested further.
- **Trigger edge.** The box edge is x=-1044 as computed from the script. A character that settled at x=-1038.1 or x=-1035.3 did not trigger. In five walks the last client position in `freporte` before the zone change was x=-1045.7, -1048.7, -1051.1, -1055.1 and -1055.6, which is 1.7 to 11.6 units inside the edge. The walk that stopped at x=-1045.7 triggered 1.3 s after it stopped, with no further move. The character arrives in `oot` 6 to 22.5 units east of its last client position (6.2, 12.8, 20.6, 20.7 and 22.5), so the server saves an older position than the client last reported. The earlier trigger at x=-1024.8 is probably this lag from a trigger at the box edge, but no walk triggered outside the edge, and the size of the lag is not explained. Only the east edge was walked, the other three were not.
- **Ferry away.** From x=-960 to x=-1149 in 10 unit steps along y=-30 (through the whole box, starting 22 s after the ferry left and crossing the edge 34 s after it) there was no zone change.
- **Already inside.** A character that stood inside the box when the ferry docked did not trigger, and neither did 25 steps inside it over 38 s. Entering from outside triggered at once. The box fires on an outside to inside crossing, so a rider must step out and back in.
- **z bound.** A character falling from z=28 to the floor (z=-72) triggered at a point where its z was -18 to -35 (37 to 54 units above the floor, 50 to 66 above the ferry), and the server saved z=1.4 one update earlier. The box has no z bound at those heights, and the script gives none (`eq.set_proximity` with four arguments sets z to +-999999).
- **Duplicate portal.** Each trigger gave two casts of spell 2279 and two `OP_RequestClientZoneChange` packets within 1 ms, which fits one for each of the two stacked ferries. The client answers both with `OP_ZoneChange`. The server refuses the second as "Invalid unsolicited zone request", cancels the zone change and resets the character to its saved position. The character therefore arrives in `oot` at the saved x and y of `freporte`, for example (-1038.3, -30, -1.1), and not at the spell destination (-9000, 459, 14).
- **Hail by name.** `interact.hail {"name": "SirensBane000"}` was accepted while the ferry was visible at 493 units. The client sent `Hail, SirensBane` as a Say with `target_id` null before and after. The `freporte` zone log has no `POSSIBLE_HACK` line, but it was not shown to record one, so this is weak evidence.
- **Narrik, live.** From (-9190, 262) in `oot`, `interact.hail {"name": "Narrik"}` returns the saylinks "travel to Freeport" and "travel to Butcherblock". Choosing "travel to Butcherblock" gave "Off you go!", the message "Gmkblr's Portal to Butcher lands on Gmkblr" and an arrival in `butcher` at (3112, 856, 7.9) after 6.3 s.
- **Confirmed by code.** `turning` never fires for this ferry, and the native client carries riders by continuous reprojection (§2).
- **Deck, `SHIP`.** The mesh has 662 vertices and 532 polygons, all solid, with bounds x -64 to 197, y -113 to 110 and z -38 to 131 around the model origin. Upward facing surfaces sit at z = -2 and z = -13, and the hull bottom at z = -38. Whether the native client collides against this mesh is not confirmed, and no authored hull exists. The shi and row boats have authored hulls.
- **Not confirmed.** The `oot` arrival point of a single portal cast, because every live trigger on this server sends two. How long Narrik takes to act without dialogue (the test character stayed 45 s with no move). Whether the native client keeps the bodytype 11 ferry visible by the same rule (taken from how the game plays, not from the decompile). The y and west edges of the box. The exact start and end of the active window, which live runs bound only coarsely, with triggers 6.9 s to 43.2 s after the ferry reached the dock and none 34 s after it left.

## 7. Open questions

- Whether stage 2 is wanted at all, given that proximity boarding covers this ferry.
- Whether `is_boat` should also appear on `SurfaceView` once boats become surfaces.
- When the box starts and stops being active, which §4 takes from the quest script and §6 bounds only coarsely.
- Whether the client should ignore an `OP_RequestClientZoneChange` that arrives while a zone change is already pending, so that two stacked ferries do not cancel the rider's move to `oot`.
