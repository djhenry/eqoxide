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
| Wall sphere radius | **1.0 unit** | `PLAYER_RADIUS = 2.0` | eqoxide is 2× too large; players will get stuck in narrow gaps the native client navigates |
| Ground probe origin | foot_z + **1.0** | `floor_z()` from foot_z | eqoxide probes from foot, not 1 above; OK since render floor-ray starts at current z |
| Ground probe range | **200 units** down | Configurable in `nearest_floor` | Confirm ≥ 200 for tall multi-level zones (Kelethin tree platforms) |
| Step-up height | **2.0 units** | `STEP_HEIGHT = 3.0` | eqoxide is 1.5× higher; may overstep in staircase seams |
| Position update interval | **280 ms min** | **150 ms** | eqoxide sends twice as often; server tolerates it but wastes bandwidth |
| Force-send interval | **1300 ms** | N/A (no keepalive) | Consider adding a keepalive send every ~1–2 s when stationary |
| Axis-separated slide | At higher-level caller | `path_clear` x/y retry | Architecture matches |
| Collision geometry | BSP including INVIS faces | Render tris only | INVIS invisible barriers may be missing; can cause walk-through-wall bugs |

---

## Related Topics

- `opcodes.md` — 0x7dfc = OP_ClientUpdate wire mapping.
- `spawn-struct.md` — PlayerPositionUpdateClient/Server layout.
- `eqg-format.md` — zone geometry loading (EQG vs S3D).
