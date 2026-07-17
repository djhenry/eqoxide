# Flight, Levitate, and the FlyMode/GravityBehavior State (RoF2)

## Question this answers

For nav-architecture purposes: can a normal RoF2 player occupy arbitrary 3D air
space over land (making land a true 3D navigation volume), or is all
over-land movement surface-constrained (ground, or a terrain-following
levitate hover) — keeping land a 2D/2.5D manifold?

**Verdict: surface-constrained. There is no player-accessible free-flight
state in RoF2.** The only non-ground vertical-freedom mechanic available to a
normal player is **Levitate**, and Levitate is a *gravity-off hover*, not a
terrain-following auto-clamp — see §2 for the important distinction. "Flying"
(true free 3D, no vertical constraint at all) exists in the protocol's
`GravityBehavior` enum but is reserved for GM noclip and for genuinely
airborne NPC types (bats, dragonflies, ghosts, etc.) — never reachable by a
player through any spell, AA, item, or mount in RoF2 (2012-era client/server).

---

## 1. The wire-level FlyMode enum — `GravityBehavior` (server) / `AppearanceType::FlyMode` (wire)

**Confirmed** (`EQEmu/common/emu_constants.h:297-304`):

```cpp
enum GravityBehavior : int8 {
    Ground,                // 0
    Flying,                // 1
    Levitating,             // 2
    Water,                  // 3
    Floating,               // 4
    LevitateWhileRunning     // 5
};
```

Wire encoding — **confirmed** via the field comment at
`EQEmu/common/eq_constants.h:35`:
```
constexpr uint32 FlyMode = 19; // 0 = Off, 1 = Flying, 2 = Levitating,
                                // 3 = Water, 4 = Floating, 5 = Levitating while Running
```
This is `AppearanceType::FlyMode` — the `type` value sent in `OP_SpawnAppearance`
when a spawn's gravity-behavior changes. `19` is also used as the byte offset
convention historically cross-checked against real packet captures (see the
`6.2 era packet collects` comment cited in §2 below) — this mapping is long
and consistently established across every EQEmu client-patch generation, not
RoF2-specific drift.

**Client-visible field:** `flymode` is also a byte inside the `Spawn_Struct`
itself (`EQEmu/common/eq_packet_structs.h:286`, `uint8 flymode`) — i.e. every
spawn a player sees (including themselves and other players) carries this
byte at spawn time, and it's updated live via `OP_SpawnAppearance` type 19
thereafter.

---

## 2. What sets each value for a PLAYER — confirmed from `FillSpawnStruct`

**Confirmed**, `EQEmu/zone/mob.cpp:1346-1355`:
```cpp
// The 'flymode' settings have the following effect:
// 0 - Mobs in water sink like a stone to the bottom
// 1 - Same as #flymode 1
// 2 - Same as #flymode 2
// 3 - Mobs in water do not sink. A value of 3 in this field appears to be
//     the default setting for all mobs (in water or not) according to
//     6.2 era packet collects.
if(IsClient())
    ns->spawn.flymode = FindType(SpellEffect::Levitate) ? 2 : 0;
else
    ns->spawn.flymode = flymode;
```

**This is the single most load-bearing fact for the nav decision:** for a
`Client` (real player), the baseline spawn-struct flymode is **derived
live and only from whether a `SpellEffect::Levitate` buff is currently
active** — `2` (Levitating) if yes, `0` (Ground) if no. There is **no other
path** by which a player's baseline flymode becomes `1` (Flying), `3`
(Water), `4` (Floating), or `5` (LevitateWhileRunning) through this function.
`3`/`Water` and `4`/`Floating` are NPC-only defaults (mounts, boats, corpses,
most spawned NPCs — see §5); `1`/`Flying` is GM-only (§4); `5` is a
runtime-only variant broadcast separately (next paragraph), never baked into
the base spawn.

**The Levitate variant broadcast** — `EQEmu/zone/spell_effects.cpp:1444-1458`
(`case SpellEffect::Levitate`):
```cpp
if (spells[spell_id].limit_value[i] == 1) {
    SendAppearancePacket(AppearanceType::FlyMode, GravityBehavior::LevitateWhileRunning, true, true);
} else {
    SendAppearancePacket(AppearanceType::FlyMode, GravityBehavior::Levitating, true, true);
}
```
`limit_value==1` on the specific Levitate spell selects `5`
(LevitateWhileRunning — lets the client keep full run speed while levitating)
vs the default `2` (Levitating — historically walk-speed-only while aloft).
Both are still "gravity off, player-piloted hover," differing only in
locomotion speed cap, not in vertical-freedom semantics.

**`SpellEffect::Levitate` SPA id = 57** (`EQEmu/common/spdat.h:1120`,
`constexpr int Levitate = 57; // implemented`).

---

## 3. Is Levitate a terrain-following hover, or true gravity-off free float?

**Gravity-off free float — confirmed by the server-side model, and consistent
with universal, era-independent EQ mechanics (see confidence note below).**

- The `flymode`/`GravityBehavior` field is a **binary gravity switch** as
  modeled by the server (§2): either normal ground gravity/clamping applies
  (`0`), or it does not (`2`/`5` while the buff is active). There is no third
  "hover at fixed terrain offset" state anywhere in the enum or in
  `FillSpawnStruct`'s logic — the model EQEmu (and by extension, the value it
  is faithfully re-deriving from 20+ years of real client packet captures)
  encodes is strictly **on/off gravity**, not "ground ± N units."
- **Movement is client-authoritative** (already established in
  `player-movement-collision.md` §9 — the server never corrects a player's
  position from `OP_ClientUpdate`; it only logs anti-cheat flags). This means
  the actual vertical-piloting behavior while levitating — ascend by
  pitch+move, hold altitude with no input, no forced terrain-relative
  reclamp — lives entirely in client-side physics code that is **not
  recoverable by string search in the RoF2 decompile**: `grep -c "Levitat"
  eqgame.exe.c` → **0 hits** (spell names/effects are data-driven from the
  server's spell files, not literal strings in the client binary), so the
  exact client function implementing the gravity-disable branch could not be
  pinned to a `FUN_xxxx` address in the time available. **This physics
  behavior itself is INFERRED, not directly disassembled**, though it is
  about as well-corroborated as any EQ mechanic can be: Levitate's
  "gravity fully off, pilot by facing pitch, indefinite hover with no input,
  full 3D positioning including over chasms/lava/pits" behavior has been
  constant and extensively documented across every EQ client generation from
  1999 classic through live-servers-today, with RoF2 (2012, well past when
  this mechanic was last touched) squarely in the middle of that unbroken
  span. It is not a Titanium-era assumption being carried forward — it's a
  mechanic with no client-patch-level variance to speak of.
- **Consequence for the chasm/cliff question:** a levitating player **does
  NOT auto-descend or auto-follow the terrain** when moving over a gap — they
  hold whatever altitude they climbed to (via pitch+forward-move) indefinitely,
  including stationary over open air with zero horizontal input. This is the
  opposite of a "hover-offset," which is why EQ zone design (since classic)
  routinely expects players to levitate-cross lava pits/chasms/vertical shafts
  at arbitrary heights, not skim a fixed distance above the floor.
- **Ascending a vertical wall / climbing at will:** yes, in the sense that a
  levitating player can gain height and reposition horizontally in any
  combination — but altitude change is not a free "hold position + press up"
  action by default; it comes from moving-while-pitched. (Swim-up/down bound
  keys reportedly also drive vertical movement while levitating, matching
  swim mechanics — this specific keybinding detail is **inferred from general
  EQ player knowledge, not verified in either source** for RoF2 specifically.)

**Bottom line on Levitate: it IS effectively a free-3D-position mechanic
(gravity fully suspended, arbitrary altitude, indefinite hold, works over any
terrain including bottomless chasms) — not a terrain-relative offset hover.**
This actually argues *for* land being a true 3D volume **only while under this
specific buff** — but the buff is optional, situational (finite duration,
requires a caster/potion/AA), and most land traversal in RoF2 happens with it
inactive (`flymode == 0`, plain ground physics, fully governed by
`player-movement-collision.md`'s ground-clamp/step-height model).

---

## 4. GM-only "Flying" (`flymode = 1`) — confirmed noclip

**Confirmed**, `EQEmu/zone/gm_commands/set/set_flymode.cpp:21-86`: the *only*
way a `Mob`/`Client`'s flymode member is ever set to an arbitrary value
(including `1`/Flying) during normal server operation is the GM command
`#set flymode [0-5]`, which requires GM command permission and calls
`t->SetFlyMode(...)` + broadcasts `SendAppearancePacket(AppearanceType::FlyMode, flymode_id)`
directly — bypassing the Levitate-buff-only derivation in §2 entirely (that
derivation only runs inside `FillSpawnStruct`, i.e. at *initial* spawn
delivery; a live `#set flymode` change is pushed via the appearance packet
and is not overwritten until the next full respawn/zone).

There is no separate `#fly` or `#noclip` command — `#set flymode 1` is the
mechanism. `EQEmu/zone/client_packet.cpp:548-564` groups `flymode ==
GravityBehavior::Flying` together with `GetHideMe()`, `GetGMSpeed()`,
`GetGMInvul()` in the on-connect "you are in the following unusual states"
message — i.e. the server's own code treats `Flying` as a GM-debug state
alongside noclip/speedhack/invuln, not a player mechanic.

**`GravityBehavior::Flying` is also the flag used for genuinely airborne NPC
spawn types** (bats, will-o-wisps, ghostly pursuers, trap-spawned mobs,
pathfinder waypoint mobs) — confirmed at `EQEmu/zone/aura.cpp:26`,
`npc.cpp:915/955/993/1033`, `pathfinder_waypoint.cpp:574`, `trap.cpp:172/197/529`.
This is server-side NPC AI classification (these NPCs skip ground-clamping in
their own movement manager, `mob_movement_manager.cpp:1070`, `waypoints.cpp:731/788/840`)
— **not** something a player spawn can acquire through play.

---

## 5. Mounts — ground/water-buoyancy only, no flying mounts in RoF2

**Confirmed, `EQEmu/zone/horse.cpp:31-46`:** the (older, spell-summoned)
mount system explicitly constructs the mount NPC with
`GravityBehavior::Water` (value `3`) — i.e. "mobs in water do not sink"
(per the `mob.cpp:1346-1351` comment), which is a **surface-buoyancy flag for
crossing water on foot/hoof, not flight**. Every other NPC-spawn call site in
the codebase (`aa.cpp`, `client.cpp:7251`, `forage.cpp`, `pets.cpp`,
`questmgr.cpp`, `spawn2.cpp`, `zone.cpp:2580`, GM loot-sim spawners, etc.)
likewise defaults new NPCs to `GravityBehavior::Water` — this is the ordinary
default, not a special "flight" grant. `Mob`'s own base-class default is
identical (`mob.cpp:490`, `flymode = GravityBehavior::Water;`).

**`GravityBehavior::Floating` (4)** is reserved specifically for **boats**
(`EQEmu/zone/npc.cpp:256-263`: `else if (GetIsBoat()) flymode =
GravityBehavior::Floating;`) — a surface-riding mode (stays on the water
plane, unaffected by depth), not airborne flight. See also
`boats-and-vehicles.md`.

No search of `common/`/`zone/` turned up a `mount_id`/`IsMounted`/modern
mount-key system at all — EQEmu's mount implementation for RoF2 is the
legacy spell-summoned `Horse` NPC-follower only, and it is ground/water
bound. **No flying-mount mechanic exists in RoF2** (2012-era EQ; live EQ has
never had flying player mounts through any later expansion either — this is
long-standing, well-known EQ design, unlike some other MMOs).

---

## 6. Feather Fall / Slow Fall / Gate / Translocate — not flight, confirmed by category

- **Gate** (`EQEmu/common/spdat.h:1089`, SPA 26) and **Translocate**
  (`spdat.h:1167`, SPA 104) are instantaneous position-set teleports (bind
  point or anchor point) — there is no continuous movement segment at all,
  so by definition they cannot contribute to a "free 3D volume" traversal
  question.
- No `FeatherFall`/`SlowFall`-named SPA constant exists in `spdat.h`; EQ's
  analogous mechanic is the passive **Safe Fall** skill (fall-damage
  reduction while it's active/skilled up), which does not alter movement
  physics or grant airtime — it only reduces the self-reported fall-damage
  number (`swimming-and-fall-damage.md` §3 covers the client-computed,
  self-reported `OP_EnvDamage` fall-damage path this modifies). Neither
  mechanic is a flight mode.

---

## 7. Summary table

| Player-reachable state | `flymode` value | Free 3D altitude? | How reached |
|---|---|---|---|
| Normal ground movement | `0` Ground | No — full ground-clamp physics (`player-movement-collision.md`) | Default |
| Levitate buff active | `2` Levitating / `5` LevitateWhileRunning | **Yes — gravity fully off, pilot by pitch+move, indefinite hold, works over chasms/lava** | Levitate spell (SPA 57), AA, item, potion — finite duration, situational |
| Mounted (spell-summoned Horse) | `3` Water (on the *mount* NPC) | No — ground/water-surface only | Horse-spell summon |
| GM noclip | `1` Flying | Yes — true unconstrained free flight | `#set flymode 1` (GM-only) |
| — (never player-reachable) | `4` Floating | Surface-plane only (boats) | N/A for players |

---

## Recommendation for eqoxide nav architecture

1. **Land pathfinding for normal (non-levitating) play should stay 2.5D
   surface-constrained** — this is the overwhelming majority of play time and
   matches everything already documented in `player-movement-collision.md`
   (ground-clamp, step-height, no slope check, no wall-slide). No change to
   that model is implied by this investigation.
2. **Levitate is a real, if situational, exception that eqoxide currently has
   zero support for** (`grep -rn -i "levitat" src/` → no hits). If/when
   eqoxide's AI needs to path through a levitate-only crossing (a lava pit,
   a vertical shaft, a chasm with no ground route), the correct model is
   **not** a terrain-offset hover — it's "gravity off, free (x,y,z) waypoint
   graph local to the levitated segment," bounded only by the buff's
   duration and by needing to descend back to solid ground (or another
   levitate-safe surface) before it expires. This is a genuinely different
   nav mode from ground A*, not a parameter tweak to it — treat it as an
   optional overlay/segment type triggered only when the player has an active
   Levitate buff, not as the default.
3. **Do not model flying mounts or a general "flight" mode** — there is none
   in RoF2 for normal play; only GM `#set flymode 1` provides it, and that's
   out of scope for normal-play nav per the calling agent's own framing.
4. **Detecting "is the player levitating" client-side:** watch for
   `OP_SpawnAppearance` (`AppearanceType::FlyMode` = wire type `19`) targeting
   the player's own spawn id with value `2` or `5`, or watch the player's own
   `Spawn_Struct.flymode` byte at zone-in/respawn (§2). This is the same
   signal the real client uses to decide whether to apply ground gravity to
   itself.

## Open gaps (flag honestly)

- The exact client-side physics function that disables gravity/implements
  pitch-driven ascent for Levitate was **not** located in the RoF2 decompile
  — `eqgame.exe.c` has no `"Levitat"` string to search from (data-driven spell
  names), and the function is one of thousands of stripped `FUN_xxxx` symbols.
  If exact ascent-rate/turn-rate constants are ever needed (not just the
  qualitative "gravity off" model), that requires either targeted live packet
  capture (position deltas during a levitate flight, similar to the existing
  jump-constant gap noted in `player-movement-collision.md` §15) or deeper
  manual RE of the stripped binary around the ground-clamp function already
  partially characterized in that doc.
- Whether bound swim-up/down keys drive vertical movement while levitating in
  RoF2 specifically is inferred from general EQ knowledge, not verified in
  either source for this client version.

## Related topics

- `player-movement-collision.md` — the ground-physics baseline this document
  is the exception to (§9 client-authoritative movement is the load-bearing
  fact that pushes the Levitate physics question into "inferred" territory).
- `swimming-and-fall-damage.md` — the sibling client-computed/self-reported
  physics domain (fall damage), same client-authoritative caveat applies.
- `boats-and-vehicles.md` — `GravityBehavior::Floating`, the other non-Ground
  flymode value, covered there in more depth.
