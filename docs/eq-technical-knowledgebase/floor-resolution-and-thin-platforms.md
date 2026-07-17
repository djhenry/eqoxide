# Floor / Standing-Z Resolution (RoF2 client) and Thin Multi-Tier Platforms

Topic for eqoxide issue **#522** (player settles ~3u BELOW a Kelethin plank in gfaydark).
Related: `player-movement-collision.md` (the broader collision/step model), `kelethin-lift-and-doors.md`.

## TL;DR — the native rule vs eqoxide's divergence

- **Native RoF2 floor-find = "highest solid triangle at/below (foot_z + 1.0)", first hit of a
  downward ray, with NO headroom/clearance test and NO up-facing-normal requirement.** Confirmed in
  the decompiled client (below).
- **eqoxide's controller clamp (`ground_below`) adds two filters native does not have** — an
  `is_standable` **headroom >= NAV_AGENT_HEIGHT (5.0)** gate and an up-facing-normal gate
  (`nz >= NAV_NEAR_HORIZONTAL 0.64`). The **headroom gate is the #522 bug**: a thin Kelethin plank
  that has a higher plank/branch/railing within 5.0u overhead is rejected as "under a ceiling," so
  the clamp skips it and tunnels down to the next tier ~3u below.

## 1. Native client — confirmed from the decompile (stripped RoF2 eqgame.exe)

### Per-tick ground snap — `FUN_00491340`
`everquest_rof2/decompiled/ghidra/eqgame.exe.c:102693`

```
fVar3 = FUN_0048c5b0(x=*(ent+0x64), y=*(ent+0x68), z=*(ent+0x6c) + 1.0, 0, &out, 0,0, 1.0f);
*(ent+0x28) = fVar3;                       // store standing/floor Z
if (NO_FLOOR_sentinel == fVar3) *(ent+0x28) = 1.0f;   // fallback
```

- **Probe start = entity z + 1.0** (hardcoded `+ 1.0` on `*(ent+0x6c)`), i.e. a small "toe-clearance"
  above the feet. The trailing `0x3f800000` = `1.0f` arg is added again inside the callee as the cast
  bias (`local_1c = base + param_4 + param_9`, `eqgame.exe.c:99694`).
- Result is stored to the entity's floor-Z slot (`+0x28`). If no floor, sentinel `_DAT_009c5760`
  (`FindZoneTopZ()`'s `NO_FLOOR`) is returned and Z falls back to 1.0.

### The floor-find itself — `FUN_0048c5b0`
`everquest_rof2/decompiled/ghidra/eqgame.exe.c:99664`

- Casts a single ray straight down against the world collision object
  (`DAT_015d46a8`/`DAT_015d46b0`, the loaded WLD/BSP) via `FUN_004767a0` and returns the **first
  (highest) hit's Z** (`eqgame.exe.c:99700-99710`); on miss returns the `NO_FLOOR` sentinel
  (`_DAT_009c5760`, `:99704/:99719`).
- **There is NO headroom / ceiling-clearance loop and NO surface-normal test.** It takes whatever
  triangle the downward ray hits first at/below the probe start. This is the whole divergence from
  eqoxide.

### `FindZoneTopZ()` — `FUN_00491190`
`everquest_rof2/decompiled/ghidra/eqgame.exe.c:102632` (name proven by the literal
`"...FindZoneTopZ() is NAN or INF (%f), return NO_FLOOR!"` at `:102663`). A vertical stepping search
(`param_2 += _DAT_009c4998` each iter) that returns the crossing height; the sibling of the raycast
path. Both return the same `NO_FLOOR` sentinel.

### Model foot offset
After the floor Z is found, callers add a per-model foot-to-origin offset before setting the entity's
render Z (e.g. `eqgame.exe.c:158845` `fVar7 = FUN_0048c5b0(...) + piVar4[0x4e]`). This is the
client-side analogue of the server's `GetZOffset()`.

> `FloorOffset` in `eqgame.exe.c:252590` / `:284292` is a **particle-emitter** config (inside a
> Sine/Twist/Emitter block), NOT the movement floor offset — do not cite it for grounding. The
> movement probe offset is the hardcoded `+1.0` above.

## 2. Server (EQEmu RoF2) — for cross-reference; does NOT drive the player's own Z

`Map::FindBestZ` `EQEmu/zone/map.cpp:49`:
- `start.z += RuleI(Map, FindBestZHeightAdjust)` — **default 1** (`EQEmu/common/ruletypes.h:401`), so
  it probes from **z + 1** — same bias as the client.
- Casts DOWN to `BEST_Z_INVALID` (-99999, `map.h:25`) and returns the **nearest = highest** hit below
  (`raycast` returns `nearestDistance`, `zone/raycast_mesh.cpp:714-716`). If nothing below, casts UP
  for the nearest above. **No headroom test** — same as the client.
- `Mob::FixZ` **explicitly skips clients**: `if (IsClient() && !fix_client_z) return;`
  (`EQEmu/zone/waypoints.cpp:832`). Grep shows no caller ever passes `fix_client_z=true` for a player.
  **The server never corrects the player's own Z** — the client is authoritative (matches
  `player-movement-collision.md` §9). So a "server-sent Z ~3u below the plank" is NOT the mechanism;
  the wrong Z is computed locally by eqoxide's clamp.
- Leap guard reference: `LineIntersectsZoneNoZLeaps` rejects a BestZ step whose |Δz| ≥ **12.0**
  (`map.cpp:237`) — server pathing only; a useful precedent for a client anti-tunnel guard.
- NPC/mob Z-offset default = **3.125** for humanoids (`waypoints.cpp:882`).

## 3. eqoxide side — where it diverges

`ground_below` (controller per-frame clamp, `src/nav/collision.rs:1306`) is called at
`src/movement.rs:420` with `origin = foot + GROUND_ORIGIN(1.0)`, `depth = GROUND_DEPTH(200)`
(`movement.rs:25,27`) — **the probe origin and depth already match native (foot+1, down 200).**

The divergence is the **selection predicate**. `ground_below` routes through
`column_hits(..., floors_only=true)` (`collision.rs:1063`), whose `is_standable` classifier
(`collision.rs:1136-1154`) drops any surface where:
- `nz.abs() < NAV_NEAR_HORIZONTAL` (0.64) — harmless for a flat plank; **and**
- `headroom < NAV_AGENT_HEIGHT` (**5.0**, `traversability.rs:162`) — i.e. a solid surface is within
  5.0u above it → treated as "under a ceiling, not standing room."

`NAV_AGENT_HEIGHT` const: `collision.rs:517` → `traversability.rs:150-162` `PLAYER_BODY.agent_height = 5.0`.

On a Kelethin plank with a higher plank/branch/railing < 5.0u overhead, the plank top (z≈77) fails
the headroom gate and is deleted; `ground_below` then returns the next standable tier below (~74),
producing the observed ~3u drop. **A pure native highest-hit would have returned 77.** The headroom
gate was introduced at the PLANNER layer for the qcat inverted-art wedge (#375/#329) and then forced
onto the CONTROLLER clamp too (D-2); that is what breaks thin overhung platforms.

`nearest_floor` (`collision.rs:1166`) uses the same `column_hits(true)` gather but returns the
surface **nearest** `ref_z` instead of highest — same headroom filter, same failure on Kelethin.

## 4. Recommendation for eqoxide

**Match native: the CONTROLLER's ground clamp must select the highest solid up-facing surface at/below
`foot + 1.0`, WITHOUT the `NAV_AGENT_HEIGHT` headroom gate.** Keep the headroom/`is_standable` gate for
the PLANNER's route admission only (its #375 purpose), not for the moment-to-moment "what am I standing
on" clamp — native has no such gate, and EQ deliberately lets you stand under low overhead (crouch).

Concretely:
1. Give `ground_below` (and the zone-in resolve at `app.rs:1184-1185`) a variant of `column_hits` that
   filters on the up-facing normal only (`nz >= NAV_NEAR_HORIZONTAL`) and **skips the headroom test** —
   i.e. native's "highest hit at/below origin." Probe origin `foot + 1.0`, depth 200 are already right.
2. Keep it **sticky / anti-tunnel**: because it takes the *highest* hit at/below `foot+1`, a character
   already on the plank re-selects the plank every tick (foot≈plank ⇒ plank is the highest hit ≤ foot+1);
   it will not dive to the lower tier unless the plank triangle is genuinely absent. Optionally add a
   server-style **|Δz| leap guard (~12u, `map.cpp:237`)**: reject a new floor that is >12u below the
   current standing Z in a single clamp, to hard-stop tunnelling through a thin plank into a deep tier.
3. Because native cannot step UP >2.0u (`STEP_UP`), the *initial* tier a character lands on is decisive.
   The zone-in resolve must pick 77 not 74 for the same reason (drop the headroom gate there too), or the
   character is stuck one tier low with no way to climb the 3u back.
4. Edge cases: `NO_FLOOR` → keep the current Z (native falls back to 1.0 only as a last resort; eqoxide
   keeping current Z is safer). Ensure the plank's thin/double-sided collision faces are in the collision
   mesh (PASSABLE bit 0x0010 clear) and the spatial-grid cell actually lists the thin triangle — a
   missing face makes the ray fall through exactly like native would through a real gap.

**Confidence:** native probe-origin (foot+1), down-cast, highest-first-hit, and no-headroom-test are
**confirmed** in `eqgame.exe.c` (§1). That eqoxide's `NAV_AGENT_HEIGHT` headroom gate is the specific
surface being wrongly rejected at the #522 XY is **strong inference** (the gate is the only path by which
a start at/above 77 yields 74; a pure highest-hit yields 77) — verify by probing `column_surfaces` at
(-126.375,-15.875) and checking whether a surface exists at ~77 that fails `headroom < 5.0`.

---

## RETRACTED: the headroom-gate theory above (§4 original) is DISPROVEN

A direct probe of the real gfaydark collision mesh at both XYs
((-126.375,-15.875) and (-138.5,-17.5)) found **no surface near z=77 at all** — the raw facing-blind
column gather is exactly `[(73.96875, nz=+1), (69.96875, nz=-1), (-3.84375, +1)]`. The plank top IS
73.97 in the mesh (matching the "wrong" eqoxide answer). There is nothing at 77 for `NAV_AGENT_HEIGHT`
to have gated out. **§3/§4's "headroom gate rejects the 77 surface" claim was wrong — the mesh simply
does not have a surface at 77.**

The correct root cause (confirmed below): **native RoF2's wire/DB `z` for a character is NOT foot/floor
level — it is close to the MODEL ORIGIN, sitting a per-race constant above the feet** (Human ≈3.1–3.75u,
depending on which side computes it — see below). The native player standing on the 73.97 plank reports
wire z≈77.0 because **77.0 = floor(73.97) + origin-offset(~3.0–3.1)**, not because a different, higher
floor triangle exists. eqoxide's ~73.97 was the **floor**, correctly found — but eqoxide was reporting/
using the FOOT level as if it were the wire-comparable value, an offset bug, not a floor-selection bug.

## 5. Wire Z = model origin, not feet — confirmed mechanism (client side)

**`FUN_00504b60`** (spawn/actor (re)creation — fires on spawn appearing or model/appearance change),
`everquest_rof2/decompiled/ghidra/eqgame.exe.c:158772-158849`:

```c
// param_1+0x8c/0x90/0x94 = the entity's x/y/z (consistent field pattern across this function)
if (... /* new model just loaded */) {
    piVar4 = (int *)FUN_00599d00(iVar3, ...);          // look up cached actor-def for this model tag
    if (piVar4 == NULL) {
        // ...fallback: derive tag from param_1+0x3c, load via FUN_00599bd0 (asset cache by name)
    }
    ...
    if (cVar2 != '\0') {                                 // "snap this actor to the ground" flag
        fVar1 = (float)piVar4[0x4e];                      // per-model Z ORIGIN OFFSET, byte 0x138
                                                           // into the loaded actor-def record —
                                                           // populated when the MODEL ASSET loads,
                                                           // not from any race-id switch/table
        fVar7 = FUN_0048c5b0(x, y, z, 0, &tmp, 0, 0, 1.0); // same floor-probe as §1 (z+1 down, highest hit)
        fVar7 = fVar7 + fVar1;                             // floor_hit + model origin offset
        if (fVar7 != NO_FLOOR) {
            *(float *)(param_1 + 0x94) = (float)fVar7;      // ← entity's z is SET to floor + offset
        }
    }
}
```

- The probe re-derives the floor from whatever `x,y,z` the entity currently has (for a freshly-spawned
  remote entity, that "current z" is the wire value just received — used only as the *search window*,
  foot+1 down 200 as in §1) — **then adds `piVar4[0x4e]`, a per-model constant baked from the loaded
  model asset**, and stores the sum back as the entity's Z. This Z field (`+0x94`) is the same field
  used elsewhere as the entity's world position (consistent x/y/z=0x8c/0x90/0x94 pattern throughout the
  binary, e.g. `eqgame.exe.c:158843-158844`).
- **This confirms direction #1: the client computes and holds Z = floor + model-origin-offset**, not
  raw floor, for any entity it (re)creates — including itself at zone-in / teleport / model-change.
  Since `OP_ClientUpdate` construction reads the player entity's own position fields, **the wire Z the
  client SENDS for itself is this floor+offset value**, matching the peer's measurement
  (77.0957 = 73.9707 + 3.125) far better than a raw-floor hypothesis (73.97) would.
- **This block only fires on spawn (re)creation** (guarded by the heavy actor/model-load call chain
  `FUN_005996e0`/`FUN_00599d00`/`FUN_00599bd0`, expensive asset-cache lookups) — **not on every incoming
  movement packet**. This is **inferred, not directly observed**, from the cost/structure of the call
  (a full model-asset lookup per position-update tick would be prohibitively expensive and no
  lighter-weight per-tick equivalent was found at any `OP_ClientUpdate`-receive site this session) —
  flagged as the one unconfirmed link in the chain; the cheapest way to nail it down is a packet capture
  correlated with a live remote-player's rendered Z during ordinary (non-zone-in) movement.
- **The offset source is per-model-ASSET, not a race-ID table lookup** — `piVar4[0x4e]` comes from the
  cached actor-definition object returned by `FUN_00599d00`/`FUN_00599bd0` (keyed by the 3-character
  model tag at `param_1+0x3c`, e.g. `"HUM"`), i.e. it is baked into (or alongside) the model file itself
  at load time. This is a **separate, independently-tuned constant from EQEmu's `GetZOffset()`
  table** (§6) — the two systems were never meant to match bit-for-bit.

## 6. `Mob::GetZOffset()` — EQEmu's server-side approximation (verbatim)

`EQEmu/zone/waypoints.cpp:881-1006`:

```c
float Mob::GetZOffset() const {
    float offset = 3.125f;                 // default (most humanoids)
    switch (GetModel()) {
        case Race::Basilisk:      offset = 0.577f; break;
        case Race::Drake2:        offset = 0.5f;   break;
        // ... ~50 more per-race/model special cases (dragons, spiders, giants, etc.) ...
        case Race::Goral:
        case Race::Selyrah:       offset = 2.0f;   break;
        default:                  offset = 3.125f;
    }
    float mob_size = (GetSize() > 0 ? GetSize() : GetDefaultRaceSize());
    return static_cast<float>(0.2 * mob_size * offset);
}
```

- Formula: **`0.2 * size * per-model-offset`**. Default humanoid `offset = 3.125f`.
- `GetDefaultRaceSize()` → `GetRaceGenderDefaultHeight(race, gender)` → `male_height[race]` /
  `female_height[race]` tables, **indexed directly by numeric race id** (`EQEmu/common/races.cpp:1441,
  1493-1497`). **Human** (`races.h:45`, id=1): `male_height[1] = 6.0f` → `GetZOffset ≈ 0.2*6*3.125 =
  3.75`. **Wood Elf** (`races.h:48`, id=4): `male_height[4] = 5.0f` → `GetZOffset ≈ 0.2*5*3.125 =
  3.125`.
- **Used only for spawns the SERVER itself positions** — NPC AI movement/step-fix, corpse drop
  location (`corpse.cpp:2368`), `TryMoveAlong` (`waypoints.cpp:1020,1031`), grid/waypoint pathing
  (`mob_movement_manager.cpp:185` etc., gated by `RuleB(Map, FixZWhenPathing)`), and bot pathing
  (`bot.cpp:12165`, `fearpath.cpp:283`). **`Mob::FixZ` explicitly refuses to touch a connected client's
  own Z**: `if (IsClient() && !fix_client_z) { return; }` (`waypoints.cpp:832`); grep of the whole
  server tree found no caller ever passing `fix_client_z=true` for a live player. **The server never
  computes or corrects a connected player's wire Z** — a player's self-reported Z is 100% client-local
  (per `player-movement-collision.md` §9, reconfirmed here).
- **Numeric mismatch vs. the peer's measurement, noted honestly:** the peer measured **+3.125** exactly
  for a "Human" via `#goto`, and **+3.03** average for static Kelethin NPCs — closer to the *default
  unscaled* constant (3.125) or the *Wood-Elf-scaled* value (3.125) than the *Human-scaled* server value
  (3.75). Two explanations, **neither confirmed this session**: (a) the client's own per-model-asset
  offset (§5, `piVar4[0x4e]`) for the Human model is simply a different, independently-tuned number from
  EQEmu's approximation — the two codebases were authored decades apart with no shared source — or
  (b) the specific `#goto`'d character's `GetSize()` wasn't the Human default 6.0 (an explicit override,
  a different race, or a mid-range value). **Do not assume `GetZOffset()`'s exact scaled constant is
  wire-exact for the native client** — treat EQEmu's per-race table as the best *available* stand-in
  (close enough that live players/NPCs already tolerate the mismatch), not a guaranteed bit-exact
  match to the closed client's internal table.

## 7. Answering the peer's four questions directly

1. **Yes — floor_hit + offset, confirmed.** `FUN_00504b60` (`eqgame.exe.c:158842-158847`): the client
   probes the floor from the entity's current (x,y,z) via the same `z+1`/first-hit ray as §1
   (`FUN_0048c5b0`), then adds a **per-model-asset** constant (`piVar4[0x4e]`, loaded via
   `FUN_00599d00`/`FUN_00599bd0` keyed by the model's tag, NOT a race-id switch/table) and stores the
   sum as the entity's z. This happens for any entity (re)created client-side, including the local
   player at zone-in/teleport/model-load — so the client's own `OP_ClientUpdate` wire Z is this
   floor+offset value.
2. **`Mob::GetZOffset()` given verbatim above** (`waypoints.cpp:881-1006`). Formula
   `0.2 * GetSize-or-default * per-model-constant`; default per-model constant `3.125f`. **Human PC**
   (size 6.0): **≈3.75**. **Wood Elf PC** (size 5.0): **≈3.125**. It DOES scale with `GetSize()` (falling
   back to `GetDefaultRaceSize()` when unset) — confirmed, not inferred, straight from the source line
   `float mob_size = (GetSize() > 0 ? GetSize() : GetDefaultRaceSize());`.
3. **Not fully confirmed — the strongest available evidence points to "trusts wire z as origin height,
   does not re-run floor physics per movement packet."** The floor+offset snap in `FUN_00504b60` is
   gated behind an expensive model-asset-cache lookup chain that structurally reads as a
   spawn-creation/model-change path, not a per-tick movement-receive path; no lighter per-packet
   equivalent was located this session (an `OP_ClientUpdate`/mob-position receive handler that writes
   `x/y/z` directly from decoded packet fields was not pinned to a specific `FUN_xxxx` — **this is the
   one open gap**, flagged as inferred/unconfirmed rather than cited). Practically: an eqoxide client
   sending a **foot-level** wire z (73.97, no offset added) will very likely be rendered by observers
   (native or eqoxide) with its **origin at 73.97 and feet further below** — i.e. sunk into the floor by
   roughly the humanoid origin offset (~3–3.75u) — because observers treat received z as origin height,
   not foot height. This matches the reported symptom and is the actionable conclusion regardless of
   whether the exact receive-side mechanism is a per-packet trust or a periodic re-snap.
4. **Not fully enumerated this session — flag as an open follow-up, not a confirmed no.** Only
   `OP_ClientUpdate`/spawn-creation Z was traced. Candidates that likely carry the same origin-vs-foot
   distinction and were NOT individually verified: `OP_ZoneChange`/zone-in safe coords (`Client::MovePC`
   forwards x/y/z **unmodified** — `EQEmu/zone/zoning.cpp:575-590,678+` — so whatever Z a zone-line/bind/
   safe-coord table stores is sent through as-is; if those tables were authored as floor-relative and the
   client re-derives+offsets locally at spawn-creation per §5, this is self-correcting on the RECEIVING
   client only for the LOCAL player's own spawn, not for how the SERVER stores/interprets that Z for
   AI/LoS purposes), bind point, and server-side `Map::CheckLoS`/`FindBestZ` calls on a client's stored
   `m_Position.z` (which — per `Mob::FixZ` skipping clients, §6 — is whatever the client last reported,
   i.e. **already includes the client's own origin-offset convention**, not a server-normalized foot
   value; any EQEmu server code that assumes `m_Position.z` is foot-level for a **client** mob would be
   silently off by the same ~3–3.75u for LoS/collision purposes — **not verified this session**, flag as
   a follow-up if eqoxide's server-facing LoS/collision code ever needs to match EQEmu's assumptions).

## 8. Recommendation for eqoxide (revised — supersedes §4's headroom-gate fix)

**The `ground_below`/`nearest_floor` floor-SELECTION logic (§1-§4 above, including the "drop the
headroom gate" recommendation) was answering the wrong question — the collision mesh only ever had one
floor (73.97) at that XY. #522 is a Z-CONVENTION bug, not a floor-selection bug:**

1. **Distinguish two Z's throughout eqoxide**: a **foot/floor Z** (what `ground_below`/`nearest_floor`
   correctly compute and what the character controller should use for its own physics/collision) vs. a
   **wire/origin Z** (what must be sent in `OP_ClientUpdate` and what must be expected when reading
   another entity's `Spawn_Struct`/position-update Z). These are NOT the same number — they differ by a
   per-race constant (~3.0-3.75 for a Human-sized PC).
2. **At the wire-serialization boundary only**: add a per-race origin offset before sending self
   position (`foot_z + offset`), and subtract that same race's offset when placing a REMOTE entity's
   FEET from its received wire z (`wire_z - offset`) for eqoxide's own rendering/animation-ground
   placement. Do NOT change the internal collision/physics representation (`self.pos[2]` etc.) — keep
   that foot-relative as it already (correctly) is; convert only at the network read/write edge.
   `player-movement-collision.md` (position-update-wire-format.md's sibling doc) should be checked/
   updated for whether eqoxide already does this conversion anywhere (spawn-struct ingestion) or is
   currently treating wire z as foot z uniformly — that is very likely the actual #522 bug location, not
   `nav/collision.rs`.
3. **Best available offset table**: use EQEmu's `Mob::GetZOffset()` (§6) as the practical per-race
   constant — it is a documented, already-live-tuned approximation used server-side for exactly this
   purpose, even though it is not proven bit-exact to the client's internal `piVar4[0x4e]` asset-baked
   value. Do not invent a flat constant (e.g. a single "3.0 for all humanoids") without checking this
   table — non-humanoid models diverge a lot (e.g. Basilisk 0.577-scaled vs. default 3.125-scaled).
4. **Edge cases**: mounts/boats and flying entities skip Z-offset logic entirely on the server
   (`flymode == Flying` early-returns in `GetFixedZ`/`FixZ`, `waypoints.cpp:788,840`) — do not apply the
   humanoid offset to a flying/boat-riding entity's wire Z. Corpses use the same `GetZOffset()`
   (`corpse.cpp:2368`) so a dropped corpse's Z is also origin-relative, not floor-relative — relevant if
   eqoxide ever renders corpse positions from server data.
5. **Open verification item**: pin down whether native RE-DERIVES floor for remote entities on ordinary
   movement packets (§7 Q3, unconfirmed) — if it does, eqoxide observers must do the same (re-floor +
   offset every packet, not just trust `wire_z - offset` blindly) to avoid the mirror bug (an entity
   whose SENDER got its offset right still being drawn wrong locally). Cheapest test: packet-capture a
   live native client's movement and diff reported Z against the known collision mesh floor along a
   sloped path — if Z tracks the floor exactly (with the same fixed offset) even where the SENDER
   couldn't have re-derived it (e.g. lag/teleport edge), that would suggest per-packet re-grounding by
   the RECEIVER too.
