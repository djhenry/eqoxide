# Player Movement, Collision, and Position-Update Protocol (RoF2)

Sources: `eqgame.exe.c` (Ghidra decompile), `eqgame.exe.asm` (Capstone), `EQEmu/zone/client_packet.cpp`, `EQEmu/zone/cheat_manager.cpp`, `EQEmu/common/patches/rof2_structs.h`, `EQEmu/common/ruletypes.h`.

---

## 1. Collision Classes

The EQ client registers three collision-info class types visible in the decompile vtable assignments:

| Class | Use |
|---|---|
| `CCollisionInfoSphere` | Wall / obstacle sweep — a sphere at chest height |
| `CCollisionInfoLineSegment` / `CCollisionInfoLineSegmentVisibility` | Ray cast (ground probe, LOS checks) |
| `CCollisionInfoCapsuleVisibility` | Visibility/range queries (not player movement) |

Vtable references: `eqgame.exe.c:47587`, `eqgame.exe.c:46329`, `eqgame.exe.c:831852`.

---

## 2. Ground Clamping

**Function:** `FUN_00507230 @ 0x00507230` (`eqgame.exe.c:160473`).

Signature: `float10 FUN_00507230(entity_ptr, x, y, z, flag)`.

Called as:
```c
FUN_00507230(_DAT_00ddfb3c, _DAT_00ddfb40, _DAT_00ddfb44 + _DAT_009c3390, 1)
// = FUN_00507230(player_x, player_y, player_z + 1.0, 1)
```
(`eqgame.exe.c:99831`, `99845`, `126100`, `126114`, `172740`, `172752`)

**Probe origin:** foot-z **+ 1.0 unit** above the feet (`_DAT_009c3390 = 1.0f`; file 0x5c1b90, hex `0000803f`).

**Probe downward range:** **200 units** (`_DAT_009c58e4 = 200.0f`; file 0x5c40e4, hex `00004843`).
- The floor search starts at z+1 and sweeps down to z+1−200 = z−199.

**Invalid floor sentinel:** `_DAT_009c5760` ≈ −5.2×10^31 (hex `8fcb4eec`; file 0x5c3f60). Returned when no collision is found below; callers check `if (sentinel != result) { use result; }`.

**Result epsilon:** adds `_DAT_009c4be8 = 0.001f` to the raw hit z (`eqgame.exe.c:160543`).

**Ground snap:** After the floor query, the entity's z is set to:
```c
*(float *)(entity + 0x6c) = floor_z + *(float *)(entity + 0x138);
```
Offset `0x138` is the entity's model-height offset (distance from the floor-contact point to the model origin / foot anchor). (`eqgame.exe.c:46358-46359`).

---

## 3. Wall / Obstacle Collision Shape

**Shape:** `CCollisionInfoSphere` with a **hardcoded radius of 1.0 unit**.

Confirmed in Capstone (`eqgame.exe.asm` near call at `0x00440445`):
```asm
0x00440418:  fld1                 ; push 1.0 onto FP stack = the radius
...
0x0044043a:  fstp dword ptr [esp] ; store 1.0 as the radius argument
0x00440445:  call 0x441ea0        ; FUN_00441ea0 = CCollisionInfoSphere constructor
```
(`FUN_00441ea0 @ eqgame.exe.c:47578`).

The sphere is centered at the **player's current position at the test frame** (chest height, approximately `z + entity_collision_height/2`).

---

## 4. Step Height / Stair Climbing

From movement case `0x17b` (`eqgame.exe.c:46259-46368`):

```c
fStack_434 = entity_z + _DAT_009c58e8;   // z + 2.0  → step-UP probe
fStack_44c = entity_z - _DAT_009c58e4;   // z - 200.0 → floor-search depth
```

**Step-up height:** **2.0 EQ units** per step (`_DAT_009c58e8 = 2.0f`; file 0x5c40e8, hex `00000040`).

**Step loop:** Steps decrement by 2.0 until the step value drops to or below `_DAT_009c58a0 = 5.0f` (`eqgame.exe.c:46308`, `46364-46365`). This is the loop exit threshold; the climb iteration does not retry smaller sub-steps.

After a successful floor hit in the step code:
```c
entity_z = floor_z + entity_0x138_height_offset;   // eqgame.exe.c:46358-46359
```

---

## 4a. Slope / Max Climb Angle — NO explicit grade/angle check exists

Searched the full Ghidra decompile for any slope/grade/angle test (`grep -n
"slope\|Slope\|grade\|Grade\|steep\|Steep\|climb\|Climb" ghidra/eqgame.exe.c`)
— **zero hits** relevant to movement (only unrelated "Upgrade/Downgrade
mercenary" UI strings). There is no dot-product-against-floor-normal test, no
stored "max walkable slope" constant, and no separate "slide down if too
steep" branch anywhere in the movement code.

**What actually happens instead** (re-examined `FUN_00506a20`,
`eqgame.exe.c:160147-160206`, called from the case-`0x17b` step loop):
- `FUN_00506a20` re-tests the *same* candidate XY point at a shrinking set of
  z heights, decrementing by the same `_DAT_009c58e8 = 2.0` step constant used
  by the outer loop (`eqgame.exe.c:160193`), calling `FUN_0048c890` (the raw
  point/BSP intersection primitive) at each height until it finds a clear one
  or runs out of candidates (`0.0 < param_4` guards the loop).
- The outer step loop (`eqgame.exe.c:46259-46368`, documented in §4 above)
  starts its candidate ladder at `entity_z + 2.0` and steps *down* by 2.0 per
  iteration, stopping once the remaining candidate value drops to/below
  `_DAT_009c58a0 = 5.0`.

**So the client has no angle-based slope limit at all — "can't climb this
hill" is a pure side effect of the 2.0-unit step-height cap interacting with
however much the floor rises within the horizontal distance covered by one
movement resolution.** A gentle grade lets the floor probe (§2, foot+1 down
200) always find a walkable surface within ±2.0u of the previous step, so the
step loop keeps succeeding every tick and you walk straight up it — a genuine
50-60° grade in EQ terrain is walkable in the real client if it's smooth
(no ledge), because each per-tick horizontal delta is small enough that the
per-tick rise stays under 2.0u. A grade becomes unwalkable only where a
*single* per-tick horizontal step would need >2.0u of rise to find the next
floor (i.e., short, steep, and/or "steppy" terrain, or an actual vertical
ledge) — which is a **step-height-vs-per-tick-distance** relationship, not a
fixed degree threshold.

**Not fully traced (time-boxed, flag as inferred):** the exact number of step
iterations the outer loop is allowed to retry per movement resolution (govern
by the table lookup `puVar6[(int)fVar20 + 0xb]`, `eqgame.exe.c:46304`,
apparently a per-race/model or per-animation-state value) was not pinned down
to a concrete constant. This bounds how many consecutive 2.0-unit steps can
be climbed in a single input frame, which in turn is the real (indirect)
"max climbable rise" — likely small (a handful of steps), since staircases in
EQ zones are built from many short risers rather than the client bounding a
huge single climb.

**Practical corollary for a fixed-grid A\*:** because the true test is
step-height-vs-move-distance (continuous, per-tick), any constant grade
threshold picked for an 8-unit grid cell is necessarily an *approximation* of
the native behavior, not a client-derived constant. A tighter approximation
than a flat `rise/run` cutoff: treat a cell-to-cell edge as walkable if the
rise is achievable in `≤ N` 2.0-unit steps where `N ≈ cell_horizontal_dist /
per_tick_move_dist` (run speed × tick interval) — i.e. **scale the allowed
rise with the distance being covered**, not a fixed ratio. At `~44 u/s` run
speed and a `~280 ms` client tick (§8) that's roughly `12 u` of horizontal
travel per tick, over which the client could climb multiple 2.0u steps if the
per-tick floor stays within reach — meaning short, punchy grade spikes (a
low curb, a rock lip) are far more forgiving than the smooth average slope of
a long hillside. A single blended `rise/run > 1.2` cutoff evaluated over an
8-unit cell is a reasonable, but not client-exact, stand-in for this.

---

## 5. Movement and Collision Sequence per Frame

1. Compute desired (x, y, z) from inputs.
2. `CCollisionInfoLineSegmentVisibility` test: ray from current to desired XY at step-up probe z — checks for wall obstruction (`eqgame.exe.c:46329`).
3. If not blocked: `FUN_00506a20` further validates the candidate position (bounding-sphere overlap against zone BSP, `eqgame.exe.c:160147`).
4. If valid: snap z via `FUN_00507230` (downward vertical ray from foot+1, range 200).
5. Entity position is updated; dirty flag `DAT_00ddfd59 = 1` is set to trigger a position packet.

There is **no explicit wall-slide** vector computed. If the primary move is blocked, the movement simply does not apply (the native client's step-loop tries multiple floor candidates but does not compute a tangent slide vector). Axis-separated retry is handled at a higher level (caller tries X-only, Y-only).

---

## 6. Depenetration / Anti-Stuck

No explicit depenetration pass. Recovery is via the fallback spawn-search loop: if the entity is in an invalid position (floor query returns sentinel for up to 500 random radius samples), a `/rewind` position is available at the server. (`eqgame.exe.c:36978-37046`).

The server stores a rewind position when the player moves >√750 units (≈27 units) from it (`EQEmu/zone/client_packet.cpp:4954`).

---

## 7. Collision Geometry

The zone collision runs against the **WLD BSP tree** loaded from `<zone>.wld` (inside the zone's `.s3d`). This tree includes:
- All rendered terrain triangles.
- **INVIS** (invisible barrier) polygons — collision-only faces that are NOT part of the render mesh.
- Zone-bounding floors/walls.

The renderer (EQGraphicsDX9.dll) loads `objects.wld`, `lights.wld`, and the zone wld separately (`EQGraphicsDX9.dll.c:89248-89279`). The physics query (`DAT_015d46a8` vtable call to the world's IntersectRay method) goes against all loaded collision faces including INVIS.

**No `"INVIS"` string literal exists in the RoF2 client decompile** (checked
`grep -n "INVIS\|invis" ghidra/EQGraphicsDX9.dll.c` and `ghidra/eqgame.exe.c`
— zero hits); the "INVIS = separate invisible-but-solid faces" model above is
not a literal client-side name, it's the well-established WLD authoring
convention (faces with no render material / render_method 0, kept only for
collision) and is precisely reconstructable from the **wire format bit**
that actually encodes it: each WLD `DmSpriteDef2` face entry (0x36 fragment)
carries a per-face flag word where **bit `0x0010` = "PASSABLE" (player can
walk through this face)**. A face is solid collision iff that bit is
**clear**, independent of whether it has a visible render material. eqoxide's
asset server already reconstructs the client's true collision set this way —
confirmed, not inferred: `eqoxide_asset_server/src/zone.rs:392-399`
(`load_collision_geometry`, doc comment: *"Uses libeq `Mesh::collision_indices()`,
which keeps every face whose flag bit 0x0010 is CLEAR — i.e. all SOLID faces,
INCLUDING invisible-but-solid ones (zone boundaries, invisible walls,
doorframes) that have no render material, while excluding PASSABLE faces
(water surfaces, foliage)"*). The render-method-0 "baked as opaque black
terrain" issue tracked separately (see `eq-invisible-boundary-render.md`
memory note, asset-server PR#30) is the *rendering* side of the same
face-set; `is_invisible_render_method` (`eqoxide_asset_server/src/convert/mod.rs:419`)
is what filters those faces OUT of the visual mesh while `load_collision_geometry`
keeps them IN the collision mesh — the two pipelines are intentionally
different views of the same WLD faces, matching how the native client
renders one geometry set (visible materials only) but collides against a
larger set (all non-PASSABLE faces).

**eqoxide's own collision-mesh consumer** already builds and prefers this
baked mesh over the render mesh when present (`src/assets.rs:355`, `:486-520`,
with an explicit walk-through-invisible-wall regression test at
`src/assets.rs:1694-1741`) — this is the correct, client-faithful design;
no further change needed here, only confirmation that the pathfinder queries
should be run against this collision mesh (not the render/terrain mesh) for
every ray/step test in §3-4 above.

---

## 8. OP_ClientUpdate (0x7dfc) — Send Cadence

**Opcode:** `0x7dfc` (confirmed `patch_RoF2.conf:113` and `eqgame.exe.asm:0x0053e197`).

**Struct:** `PlayerPositionUpdateClient_Struct`, 46 bytes (`rof2_structs.h:1653`):
- float `delta_x/y/z` — frame-to-frame velocity
- float `x_pos/y_pos/z_pos` — absolute position
- 12-bit heading, 10-bit animation, 10-bit delta_heading
- `sequence` counter increments each packet

**Dirty flag:** `DAT_00ddfd59` (byte at 0xddfd59) is set to 1 on every movement delta (`eqgame.exe.asm:0x0044097a`, `0x0044163e`, `0x0052cdcf`, `0x0052ce04`, `0x0052ce2d`).

**Rate gate — minimum interval:** `0x118` = **280 ms** between packets when dirty.
Confirmed in Capstone:
```asm
0x0053e0c1:  cmp esi, 0x118
0x0053e0c7:  jbe 0x53e235      ; skip send if < 280 ms elapsed
```
(`eqgame.exe.asm:416435-416438`)

**Forced keepalive:** `0x514` = **1300 ms** — packet is forced regardless of delta-position change (`eqgame.exe.asm:0x0053e410`).

**Effective rate:** ~3.6 Hz (one packet per 280 ms when moving), with a ~0.77 Hz keepalive.

**Timer storage:** last-send timestamps at `DAT_00ddf7f8` (self) and `DAT_00ddf7fc` (controlled entity / boat). Reset after each send (`eqgame.exe.asm:416522`, `416524`).

---

## 9. Server Position Handling — No Rubber-Band

`Client::Handle_OP_ClientUpdate` (`EQEmu/zone/client_packet.cpp:4832`):

1. Decodes the packet position (x, y, z, heading, deltas) — no server-side validation of the position itself.
2. Calls `cheat_manager.MovementCheck(...)` — **logs** suspicious movement but does NOT correct it.
3. Sets `m_Position` to the client-provided values unconditionally (`client_packet.cpp:5023`).
4. Broadcasts to other nearby clients: `entity_list.QueueCloseClients(this, &outapp, true, dist, nullptr, true)` — the final `true` is `ignore_sender`; **the packet is NOT sent back to the player who moved** (`client_packet.cpp:5047`).

**The server never sends a position correction to the moving client during normal movement.** There is no native EQ rubber-band from position updates.

### Anti-Warp Thresholds (EQEmu rule defaults)

```c
// cheat_manager.cpp:266-297
float estimated_speed = (distance * 100) / (float)(cur_time_ms - last_check_ms);
float run_speed = GetRunspeed();   // integer; default character ~50 (= 1.25 * 40)
// Soft flag  (log only): estimated_speed > run_speed
// Hard flag  (log + timer): estimated_speed > run_speed * 1.5
```

`MQWarpDetectionDistanceFactor = 9.0` (`ruletypes.h:1123`) is divided by `std::min(9.0, 1.0)` which clamps to 1.0, so the factor is effectively inactive. Detection fires when `estimated_speed` (in units·100/ms) exceeds base_runspeed ≈ 50. Normal run at ~44 u/s gives `estimated_speed ≈ 4.4` — well below the threshold.

**Range for other clients:** `RuleI(Range, ClientPositionUpdates) = 300` EQ units (`ruletypes.h:763`). Server only relays the position update to clients within 300 units.

---

## 10. Cause of Rubber-Band in eqoxide (WASD)

The native client is position-authoritative: it never receives its own position back and never gets snapped. The eqoxide rubber-band is an artifact of the **visual-vs-server split**:

1. `override_pos` advances the visual at ~60 fps (35 u/s · dt).
2. The nav thread sends one position packet per 150 ms (matches native 280 ms).
3. When WASD keys are released, `override_pos = None` (`app.rs:1144`) and the visual snaps to the server-tracked position (`game_state.player_x/y`).
4. The server position trails by up to `MOVE_SPEED * ~0.15s ≈ 5 units` at any moment because gs.player_x is updated only when the nav thread sends and stores the step.
5. This causes a visible "snap-back" of the visual when keys are released.

**Fix:** Do not clear `override_pos` the instant keys are released. Keep it until `|visual - server_pos| < threshold` (e.g. < 2 units). Alternatively, update `gs.player_x/y` from goto_target immediately on input (not only after the nav-thread round-trip).

---

## 11. Comparison: eqoxide vs Native Constants

| Parameter | Native RoF2 | eqoxide current | Notes |
|---|---|---|---|
| Wall sphere radius | **1.0 unit** | `PLAYER_RADIUS = 1.0` (`src/movement.rs:13`) | Fixed — now matches native exactly |
| Ground probe origin | foot_z + **1.0** | `floor_z()` from foot_z | eqoxide probes from foot, not 1 above; OK since render floor-ray starts at current z |
| Ground probe range | **200 units** down | Configurable in `nearest_floor` | Confirm ≥ 200 for tall multi-level zones (Kelethin tree platforms) |
| Step-up height (controller, per-tick) | **2.0 units** | `STEP_UP = 2.0` (`src/movement.rs:17`) | Fixed — matches native exactly |
| Step-up height (A\* cell-to-cell) | N/A — native has no single "max climb," see §4a | `STEP_H = 20.0` (`src/assets.rs:1062`) | Deliberately larger than the controller's 2.0: represents a *sequence* of native 2.0u steps across an 8u cell, gated by `MAX_WALK_GRADE` below, not a literal per-tick cap |
| Slope/grade limit | **none — no angle check exists**; emergent from step-height (2.0u) vs per-tick horizontal distance (§4a) | `rise/run > 1.2` (~50°) flat cutoff | eqoxide's constant-grade cutoff is a reasonable approximation, not a client-derived value; real client's tolerance scales with how far you move per tick, so short steep steps are more forgiving than a long steep grade of the same average angle |
| Position update interval | **280 ms min** | **150 ms** | eqoxide sends twice as often; server tolerates it but wastes bandwidth |
| Force-send interval | **1300 ms** | N/A (no keepalive) | Consider adding a keepalive send every ~1–2 s when stationary |
| Axis-separated slide | At higher-level caller | `path_clear` x/y retry | Architecture matches |
| Collision geometry | WLD faces with PASSABLE bit (0x0010) clear, incl. no-material INVIS faces | `__collision__` GLB mesh when baked (falls back to render tris for legacy/un-rebaked zones) | Fixed — see §7; `Collision::build` (`src/assets.rs:519-529`) already prefers `__collision__`; only un-rebaked zones fall back to render-only |

---

## 12. Max Walkable Slope — NO Explicit Angle/Normal Test Found

Traced the full ground-movement path (`FUN_0043e8c0` movement-command switch case
`0x17b`, `eqgame.exe.c:46259-46368`; the raw ground-clamp `FUN_00507230`,
`eqgame.exe.c:160473`; and the candidate-validate `FUN_00506a20`,
`eqgame.exe.c:160147-160206`). **No dot-product / surface-normal / degree-threshold
comparison exists anywhere in this path.** There is no `MaxSlope` constant.

The two gates that exist are purely §2 (unrestricted vertical floor-snap, foot+1
down 200, **no cap on the magnitude of the Z change**) and §3/§4 (a forward
LOS/sphere obstruction test, radius 1.0, at a probe height `z_old + 2.0`
(`_DAT_009c58e8`, confirmed `case 0x17b` line 46264/46307)). Concretely:

- If the forward LOS ray at `z_old + 2.0` is **not** obstructed, the move commits
  and Z re-snaps to *whatever floor is found* — **no matter how large the Z
  delta is**. A continuous ramp of any steepness is walkable up to the point
  where the ramp surface itself intersects that forward ray, i.e. where the
  **local rise exceeds ~2.0 units within the per-tick horizontal travel
  distance**. At normal run speed (~44 u/s) and a render-frame tick, horizontal
  travel per tick is well under 2 units, so the *effective* tolerated grade for
  smooth, continuously-sampled ramps is very high (near-vertical faces are the
  practical limit, not a fixed angle) — this matches the well-known EQ player
  experience of "cheesing" absurdly steep terrain by approaching it that way,
  which native EQ has always allowed because there never was a real slope check.
- A **discrete** riser (stair edge, curb, fence rail) that's taller than ~2.0
  units directly in the path DOES intersect the z_old+2.0 ray and is rejected —
  this is what reads as a step-height cap (§4), not a slope cap.

**Confidence:** the absence-of-a-check claim is as strong as a full trace of the
cited functions can make it (no stray float comparison against a normal.z or
`cos(angle)`-shaped constant appears anywhere in the reachable code). The
"effective grade from ray-height / per-tick travel distance" reasoning above is
**inference**, not a literal cited constant — there is no single native "max
slope" number to port to eqoxide; slope-walkability is emergent, not declared.

## 13. Slope Slide Behavior — Block, Not Slide

Per §5 (already established): there is no wall-slide/tangent vector computed
anywhere in the traced path. A move that fails the forward-obstruction test
simply does not apply that tick — the player is **blocked in place** (can still
turn/attempt other headings), not physically slid back down the face. This
applies identically whether the obstruction is a genuine wall or a too-steep
local rise that intersects the step-up ray. There is no separate "on-slope"
downhill-slide force in the ground-movement code (any slide-like drift players
report near steep terrain is `depenetrate`-adjacent geometry noise, not a
declared physics feature).

## 14. Fences / Low Walls / Carts — Same 2.0u Ray, No Distinct "Vault"

There is no separate code path for climbing over a fence/cart rail. It uses the
exact same z_old+2.0 forward-obstruction ray as any other terrain: if the
rail's top is below the ray height (`z_old + 2.0`), the ray clears and the
floor-snap (§2, unrestricted) pops the player onto it (walking-over a low
rail/curb). If the rail is taller than ~2.0 units, the ray is blocked and the
native client treats it as a plain wall — the **only** ways past it are jump
(§15) or walking around; there is no "auto-vault" for anything taller than the
ordinary step height. This directly answers the "super-human climb" question:
**native RoF2 has no discrete-obstacle climb bigger than ~2.0 units under any
circumstances** during ground movement.

## 15. Jump — Not Recoverable From This Decompile Pass (Gap)

No jump-impulse or gravity-accel constant was identified in this pass — the
input-command giant switch (`FUN_0043e8c0`) has many unlabeled cases and the
specific jump case (bound to a keybind ID, not found by string/opcode grep
since the binary is stripped) was not isolated in the time budget available.
**This is an open gap**, not a "confirmed absent" finding — jump almost
certainly exists as a `vel_z += const` on a `Space`-bound command elsewhere in
this same function, but pinning down which numbered case, and the vertical
velocity / gravity-decel constants it uses, needs either (a) more exhaustive
jump-table tracing in `capstone/eqgame.exe.asm` around `FUN_0043e8c0`
(`0x0043e8c0`-`0x00441a00`, asm lines 86265-90029), or (b) the cheaper route:
capture real `OP_ClientUpdate` telemetry (z_pos over time, §8) during a live
jump and curve-fit `v0`/`g` from the parabola — no live client access was used
for this note (offline decompile only, per this agent's constraints).

eqoxide's own `movement.rs:28-36` already flags its `GRAVITY = 120.0` /
`JUMP_VELOCITY = 31.0` (peak ≈ 4.0u) as an approximation pending decompile/live
verification — this pass did not resolve that; still open.

## 16. eqoxide `NAV_CLIMB` Cross-Check (movement.rs / assets.rs, read `HEAD` of this worktree)

Confirmed by reading `src/movement.rs` and `src/assets.rs` directly (not the
client — this is an eqoxide-side finding, recorded here because it's the direct
answer to "what should replace NAV_CLIMB"):

- `PLAYER_RADIUS = 1.0` (`movement.rs:13`) and `STEP_UP = 2.0` (`movement.rs:17`)
  **already match native exactly** (§3, §4/§14 above). Good — no change needed
  for the WASD path.
- `NAV_CLIMB = 20.0` (`movement.rs:23`) is fed into `try_step_up` as `max_step`
  (`movement.rs:198,368-380`) **only when `slide()` reports the horizontal move
  blocked** (`low_hit`, `movement.rs:202`). `try_step_up` **teleports** the
  cylinder to `pos.z + max_step`, sweeps, and — if a floor is found in-band —
  snaps straight there in one frame (`movement.rs:369-376`). At `NAV_CLIMB=20`
  this is a literal one-frame 20-unit vertical teleport with no native
  counterpart at all (§14: native's discrete-obstruction climb caps at ~2.0u,
  period).
- **Continuous ramps do not need `NAV_CLIMB`.** The vertical ground-clamp for
  an on-ground character re-snaps upward **unconditionally** whenever the new
  floor is higher (`movement.rs:286`, `Some(f) if ... || f > foot => self.pos[2]
  = f`) — this already matches native's unrestricted floor-snap (§2/§12) with no
  magnitude cap. A rising ramp is walked by ordinary forward `slide()` +
  per-frame ground-snap and never even reaches `try_step_up` unless the ramp is
  steep enough to intersect the `slide()` chest-height ray (`CHEST = 4.0`,
  `movement.rs:316`) — which is exactly the case A*'s `MAX_WALK_GRADE` (`assets.rs:1069`)
  is already built to keep out of the routed graph (per its own `#212` comment).
  So `try_step_up` firing for a *routed* ramp edge should be rare in practice;
  when it *does* fire, it should behave like native — cap at 2.0u, not 20.

**Recommendation:** eliminate `NAV_CLIMB` as a distinct, larger constant.
`intent.climb` should not exceed `STEP_UP` (2.0) for genuine step resolution —
matching native's real limit exactly. For a fence/cart lip between ~2u and
whatever a running jump clears, use the **existing** `NAV_HOP_VELOCITY` /
`can_hop` mechanism (`movement.rs:52,386-399`, already built for exactly this,
issue #41) instead of inflating the climb — that's what a real player does
(jump it), not an instant teleport-climb. If a specific routed A* edge still
can't be traversed by 2.0u-step + hop, the fix belongs in A* (reject/avoid that
edge, or add a jump-edge per the existing `running_jump_reach` mechanism at
`assets.rs:1070-1078`, already used for horizontal gaps) — not in raising the
controller's instantaneous climb budget.

## 17. Comparison Table Addendum

| Parameter | Native RoF2 | eqoxide | Notes |
|---|---|---|---|
| Max walkable slope | **no explicit check** — emergent from step-ray vs local rise (§12) | `MAX_WALK_GRADE = 1.2` (A*, `assets.rs:1069`) | Not a literal client constant; a reasonable coarse-grid conservative approximation. No native number to replace it with — keep as a tunable, not "fix" to a cited value. |
| Slope behavior | **block, not slide** (§13) | matches (`slide()` has no slide-down force) | Consistent. |
| Discrete obstacle climb (fence/curb) | **~2.0u max, always** (§14) | `STEP_UP=2.0` (WASD, correct) vs `NAV_CLIMB=20.0` (nav, super-human) | `NAV_CLIMB` is the one confirmed mismatch (§16) — drop it to `STEP_UP`. |
| Jump impulse / gravity | not recovered this pass (§15) | `JUMP_VELOCITY=31.0`, `GRAVITY=120.0` (peak ≈4.0u) | Open — approximation, self-flagged in `movement.rs:31-35`. |

## Related Topics

- `swimming-and-fall-damage.md` — water regions, fall-damage self-report, and
  why entering water is never treated as a fall (relevant to the collision
  volume when swimming vs walking).
- `zone-line-crossing.md` — the sibling WLD-BSP-region parser (`DRNTP`) that
  shares the same region-flag decoding path referenced in `swimming-and-fall-damage.md`.
- `boats-and-vehicles.md` — the other water-traversal mechanism (not
  relevant to on-foot collision, but the adjacent "how do you cross this"
  decision tree).
