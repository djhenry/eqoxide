# Boats / Ferries as Moving Platforms (RoF2)

## 1. A boat is an ordinary NPC spawn — two distinct categories

**Category A — big ferries (`IsBoat()`), AI-waypoint-driven, NOT player-controllable.**
`EQEmu/zone/mob.cpp:7021-7032` (`Mob::IsBoat`):
```cpp
race == Race::Ship || race == Race::Launch || race == Race::GhostShip ||
race == Race::DiscordShip || race == Race::MerchantShip || race == Race::PirateShip ||
race == Race::GhostShip2 || race == Race::Boat2
```
Race IDs (`EQEmu/common/races.h`): `Ship=72`, `Launch=73`, `GhostShip=114`,
`DiscordShip=404`, `Boat2=533`, `MerchantShip=550`, `PirateShip=551`, `GhostShip2=552`.
These arrive over the wire exactly like any other NPC — `OP_ZoneSpawns`/`OP_NewSpawn`
with a normal `Spawn_Struct` (race/bodytype/name/position). **No special opcode marks
a boat as a boat** — the client (and eqoxide) must recognize it by race id alone.

**Category B — small rideable boats (`IsControllableBoat()`), player pilots directly.**
`EQEmu/zone/mob.cpp:7035-7039`: `race == Race::Boat (141) || race == Race::Rowboat (502)`.
These are also normal NPC spawns, but the player can take the tiller via `OP_ControlBoat`.

`bodytype` is **data-driven** (DB `npc_types.bodytype`), not derivable from race id in
EQEmu source — don't infer/filter on bodytype for boats; trust whatever the wire sends.

## 2. Movement: NPC waypoints, forced-floating gravity, swim-style path commands

- `zone/npc.cpp:260-264`: if the DB doesn't set an explicit `flymode`, `GetIsBoat()`
  forces `flymode = GravityBehavior::Floating`. Boats never get server-side ground
  clamping (`Mob::FixZ` explicitly bails for boats: `zone/waypoints.cpp:836-838`,
  `if (GetIsBoat()) return;`).
- Waypoint following: `MobMovementManager::UpdatePathBoat` (`zone/mob_movement_manager.cpp:1334-1341`)
  — `PushRotateTo` + `PushSwimTo` (no `PushFlyTo`/jump), i.e. the server drives a boat
  like a swimming mob gliding across/through the water's surface, not a walking one.
  Dispatch: `mob_movement_manager.cpp:1062-1063`, `if (who->GetIsBoat()) UpdatePathBoat(...)`.

## 3. Client-side "on boat" — a client-owned, NOT server-owned mechanic

`PlayerPositionUpdateClient_Struct.vehicle_id` (RoF2, offset 4, u16 —
`EQEmu/common/patches/rof2_structs.h:1657`) and its mirror in
`PlayerPositionUpdateServer_Struct.vehicle_id` (offset 2, u16 —
`rof2_structs.h:1628`) carry which boat (spawn id) the rider is standing on.
**The server never sets this field** (confirmed: no assignment anywhere in
`EQEmu/zone/*.cpp` other than reading `ppu->vehicle_id`) — the RoF2 *client* itself
detects "I am standing on a boat's collision volume" and switches its own outgoing
position report from world coordinates to **boat-local offset coordinates**, with
`vehicle_id` set to the boat's spawn id.

Server-side transform (`EQEmu/zone/client_packet.cpp:4891-4931`,
`Client::Handle_OP_ClientUpdate`):
```cpp
bool on_boat = (ppu->vehicle_id != 0);
// cx, cy, cz start as ppu->x_pos/y_pos/z_pos (LOCAL offsets when on_boat)
if (on_boat) {
    Mob *boat = entity_list.GetMob(ppu->vehicle_id);
    if (boat->turning) return;                 // drop updates while the boat is mid-turn
    double theta = fmod(boat->GetHeading()*360.0/512.0, 360.0);
    // rotate the local offset (cx inverted/left-handed, cy toward heading) into world space
    cx = boat->GetX() + (cx*cos(theta) - -cy*sin(theta));
    cy = boat->GetY() + (-cx*sin(theta) + cy*cos(theta));
    cz += boat->GetZ();
    new_heading += boat->GetHeading();
}
```
This is why a rider visually stays glued to a moving/rocking deck between the rider's
own sparse (~280ms, see `player-movement-collision.md`) position packets: other
clients recompute the rider's world position each time *the boat itself* sends a
position update (high frequency, since the boat is server-simulated every tick),
using the rider's last-known **local** offset — not by waiting for the rider's own
next packet.

Non-controllable boats (`Ship`/`Launch`/etc.) never receive `OP_ClientUpdate`
themselves from a player; only controllable boats (`Boat`/`Rowboat`) are moved
directly by a rider's own `OP_ClientUpdate` when `ppu->spawn_id` targets the boat's
id (`client_packet.cpp:4848-4869` — this branch is taken BEFORE the normal player
handling, keyed off `cmob->IsControllableBoat()`).

## 4. Opcodes (RoF2, confirmed `patch_RoF2.conf`)

| Opcode | Value | Direction | Struct |
|---|---|---|---|
| `OP_BoardBoat` | `0x4211` | C→S | ~64-byte boat-NAME string (`Handle_OP_BoardBoat`, `client_packet.cpp:4134-4154`, copies `app->pBuffer` into a 64-byte name buffer) — NOT the 4-byte `EntityId_Struct` the opcode_dispatch.h comment implies; that comment is stale/wrong for RoF2. |
| `OP_ControlBoat` | `0x0ae7` | C→S | `ControlBoat_Struct` (8 bytes): `boatId(u32)`, `TakeControl(bool)`, `unknown[3]` — `EQEmu/common/eq_packet_structs.h:5326-5330`. |
| `OP_LeaveBoat` | `0x7617` | C→S | No meaningful payload (`INz` — empty). |
| `OP_ClientUpdate` | `0x7dfc` | both | Carries `vehicle_id` (see §3) for BOTH the rider's own packets and every broadcast position update of anyone riding a boat. |

`Handle_OP_BoardBoat` sets `controlling_boat_id = boat->GetID()`
(`client_packet.cpp:4152`) only for `IsControllableBoat()` boats — it's the
"which controllable boat am I currently allowed to steer" state, separate from the
passive-rider `vehicle_id` offset mechanism in §3 (which applies to ANY boat,
controllable or not).

## 5. eqoxide gap analysis (confirmed by reading eqoxide's own source)

**(a) Model mapping is an acknowledged placeholder, not a decode bug.**
`src/eq_net/protocol.rs:707-` `eq_race_to_code()` maps race 72/73/114/141 → `"HUM"`
(and similarly for the rest of the NPC race range) — the surrounding comment
(`protocol.rs:713-715`) is explicit: *"NPC races 13..=127 — best-fit to an available
archetype model"*. There is no boat model asset/mapping implemented at all; a boat
spawn currently renders (if it renders) as a bare human-shaped placeholder standing
in/on the water, not a ship.

**(b) Likely primary cause of "never shows up" — universal floor-snap ignores
`GravityBehavior::Floating`.** `src/app.rs:1854-1868` unconditionally re-snaps
**every** rendered entity's z to `Collision::floor_z()` each frame the raw position
changes:
```rust
// Snap the billboard to the terrain floor so it doesn't hover above geometry.
// NPCs get z from the server spawn/update packets; the player gets floor_z
// applied each frame. Same grounding here ... for ALL entities
m.floor_z  = col.floor_z(b.pos[0], b.pos[1], b.pos[2]);
b.pos[2]   = m.floor_z;
```
`Collision::floor_z` (`src/assets.rs:645-653`) casts a ray from `fallback+2` down to
only `fallback-100` and returns the **fallback (current z) unchanged** if nothing is
hit in that 102-unit window — but in shallow harbor/dock water (very plausibly within
100 units of the surface), it WILL find the seabed/dock floor and forcibly pull the
boat's rendered position down onto it, well below the water surface where the boat
should float. The server-side equivalent of this clamp (`Mob::FixZ`) explicitly
excludes boats (`GetIsBoat() → return`, `zone/waypoints.cpp:836-838`); eqoxide's
renderer has no such exception. This is a strong, directly-verified candidate for a
boat rendering underwater/invisible in exactly the kind of harbor zones (Qeynos,
Butcherblock, Freeport) the caller is asking about.

**(c) No vehicle/rider mechanic implemented at all (either direction).**
- Decode: `src/eq_net/protocol.rs:1334` explicitly **skips** `vehicle_id` when parsing
  incoming `PlayerPositionUpdateServer_Struct` broadcasts — eqoxide can't tell when
  another player (or itself) is riding a boat.
- Encode: `src/eq_net/protocol.rs:1376` and `src/eq_net/navigation.rs:2026`
  (`send_position_update`) hardcode `vehicle_id = 0` on every outgoing
  `PlayerPositionUpdateClient_Struct` — eqoxide never reports "I'm standing on boat
  N," so even a correctly-modeled/positioned boat would leave a riding eqoxide
  character visually sliding off the deck between its own ~150–280ms position sends
  (native compensates for this via the boat-relative-offset scheme in §3).
- No `OP_BoardBoat`/`OP_ControlBoat`/`OP_LeaveBoat` handling exists in eqoxide at all
  (not found in `src/eq_net/protocol.rs` or `navigation.rs`).

## Recommendation for eqoxide

1. **Do not floor-snap boat-race spawns** (or, more generally, any spawn whose
   observed z sits persistently above/away from `floor_z`'s hit — but the robust fix
   is to special-case the known boat race ids from §1, mirroring the server's
   `GetIsBoat()` exclusion in `Mob::FixZ`). Render boats at the server-reported z
   verbatim (own `OP_ClientUpdate`/spawn packets), matching `flymode==Floating`
   semantics.
2. **Add a real ship/boat model** (or at minimum a distinguishable flat-deck
   placeholder mesh) for race ids 72/73/114/141/404/502/533/544/545/546/550/551/552,
   instead of falling through to the generic Human archetype in `eq_race_to_code`.
   Check `~/eq_assets/everquest_rof2/` zone object archives (`_obj.s3d`/`.eqg` for
   harbor zones) for a ship model asset — not yet located/confirmed in this pass.
3. **Implement the `vehicle_id` rider mechanic** if standing on a boat matters for
   eqoxide's gameplay: (a) on the outgoing `PlayerPositionUpdateClient_Struct`, detect
   "player's feet are on top of a boat-race entity's collision volume," set
   `vehicle_id` to that entity's spawn id, and transform outgoing x/y/z/heading into
   the boat's local frame (inverse of the §3 server transform); (b) on incoming
   position broadcasts for OTHER entities, when `vehicle_id != 0`, resolve the named
   boat and apply the same rotate-and-add transform before placing the rider, and
   refresh that placement every time the BOAT's own position updates (not just when
   the rider's own packet arrives) so the rider visually tracks the moving/rocking
   deck.
4. `OP_BoardBoat`/`OP_ControlBoat`/`OP_LeaveBoat` are only needed for **Category B**
   (rideable `Boat`/`Rowboat`, race 141/502) — passenger riding on the big ferries
   (Category A) needs none of these opcodes, only the passive `vehicle_id` offset
   mechanic in §3.

Related: `player-movement-collision.md` (OP_ClientUpdate cadence/struct, ground
clamping), `zone-line-crossing.md` (WLD region parsing background).
