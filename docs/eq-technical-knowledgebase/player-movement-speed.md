# Player Movement Speed — Titanium EQ

## Base runspeed value in Spawn_Struct

`Spawn_Struct.runspeed = 0.7f` for a normal unmodified player character.

**Citations:**
- `EQEmu/common/patches/titanium_structs.h:303` — `float runspeed; // Speed when running` at wire offset 0x233
- `EQEmu/zone/mob.cpp:183-192` — on spawn, `runspeed = in_runspeed` (clamped to 0–20); when `runspeed == 0.7f`, `base_runspeed = 28` is hardcoded (instead of the general `int(runspeed * 40)`)
- `NostalgiaEQ-Client/decompiled/capstone/eqgame.exe.asm:172219` — client writes `0x3f333333` (= 0.7f in IEEE 754) to struct offset 0x230 (confirmed via PE section scan)

`Spawn_Struct.walkspeed = 0.3f` at wire offset 0x324 (`titanium_structs.h:313`). Walk speed in base units: `base_walkspeed = 12` (`mob.cpp:194`).

## Server-internal speed representation

`base_runspeed = int(runspeed * 40.0f)` — general formula (`mob.cpp:190`).

For `runspeed = 0.7f`: `base_runspeed = 28` (hardcoded special case, `mob.cpp:191-192`).

`GetRunspeed()` returns `_GetRunSpeed()` which returns `base_runspeed` (= 28) for an unmodified player. Spell/item bonuses can raise it up to `BaseRunSpeedCap = 158` (rule default, `ruletypes.h:144`), hard capped at 225.

## World-coordinate speed in EQ units per second

**Confirmed value: ~44 EQ units/second at base runspeed.**

Derivation from `EQEmu/zone/cheat_manager.cpp:262-316`:

```
estimated_speed = (dist_accumulated * 100) / elapsed_ms
```

The EQEmu developer comment at `ruletypes.h:1123` reads:
> "clients move at 4.4 about if in a straight line but with movement and to acct for lag we raise it a bit"

Solving: `4.4 = dist * 100 / ms` → speed = `4.4 * (1000/100) = 44 EQ units/second`.

Cross-check: the eqoxide fall-physics code (`src/eq_net/navigation.rs:69`) uses `HZ: f32 = 10.0` as the "native position-update rate the formula is calibrated to." At 44 units/sec and 10 Hz, each position packet moves the character **4.4 units**, matching the comment exactly.

## Per-packet delta at native 10 Hz send rate

- Send interval: 100 ms (10 Hz)
- Per-packet translation at base run: **4.4 EQ units**

## Client movement speed scale constant

`DAT_00640038 = 0.01` (confirmed from eqgame.exe PE binary, .rdata section at file offset 0x240038).

The movement calculation in the decompiled client multiplies: `param * 0.01 * runspeed` to compute a velocity per frame (`capstone/eqgame.exe.asm:22530,22533`). The `param` integer is clamped to [-65, 127] range (`capstone:22521`). At `param = 127`, `0.7 * 127 * 0.01 ≈ 0.889` units/frame — at ~50 FPS this gives ≈ 44 units/sec, consistent with the EQEmu comment.

## Cheat detection thresholds (EQEmu cheat_manager.cpp)

File: `EQEmu/zone/cheat_manager.cpp:262-316`

The check accumulates distance over 2500 ms windows (`MovementCheck(2500)` called on each position update when the player is moving).

```
estimated_speed = (distance_since_last_check * 100) / elapsed_ms
run_speed       = GetRunspeed() = 28   (for std::min(MQWarpDetectionDistanceFactor, 1.0f) = 1.0)
```

| Check | Condition | Threshold in units/sec | Action |
|-------|-----------|----------------------|--------|
| MQWarpLight | estimated_speed > 28 | > 280 units/sec | Log + mark suspicious only |
| MQWarp (hard) | estimated_speed > 28 * 1.5 = 42 | > 420 units/sec | Log + optional quest EVENT_WARP |

**No automatic rubber-band / position correction occurs in Handle_OP_ClientUpdate.** The server simply accepts whatever position the client sends and broadcasts it. The cheat system only emits log events. Rubber-banding from EQEmu at any speed below ~280 units/sec is essentially impossible from this code path.

`MQWarpDetectionDistanceFactor` default = 9.0 (`ruletypes.h:1123`), but `std::min(9.0f, 1.0f) = 1.0f` so the factor is effectively ignored (looks like a code bug; the std::min should probably be std::max to give leeway above run_speed).

## eqoxide current values (as of this analysis)

- **WASD (`src/app.rs:955`)**: `MOVE_SPEED = 35.0` units/sec — BELOW real speed (44 u/s), safe
- **Nav `/goto` (`src/eq_net/navigation.rs:940`)**: `step = 10.0` units per 150 ms tick = **66.7 units/sec** — 52% faster than real but well below the 280 u/s cheat threshold
- **Nav module header comment (line 1)**: says "15-unit steps" — this is STALE; actual code uses 10 units

## Recommendation for eqoxide

To exactly match the real Titanium client's movement rate:

```
step = 44.0 * 0.150 = 6.6 EQ units per 150 ms nav tick
```

This gives 44 units/sec, matching the native client. A small margin (6.5 units = 43.3 u/s) adds a negligible latency buffer.

The current 10.0 units/150ms (66.7 u/s) will NOT trigger server rubber-banding (threshold is 280+ u/s). However, it moves 52% faster than a real player, which means the nav completes paths faster and may look unnatural or clash with server-side NPC aggro range timing. If rubber-banding is actually observed, the cause is something other than the cheat speed check (likely a geometry/floor issue or a delta_x/delta_y inconsistency in the position packet).

Update the header comment in `navigation.rs:1` when the step size is changed.
