# Falling Physics — Titanium Client + EQEmu

## Status: confirmed from decompiled client + EQEmu source

---

## Fall Damage Is CLIENT-COMPUTED and CLIENT-SENT

**Confirmed.** The server handler `Handle_OP_EnvDamage` at
`EQEmu/zone/client_packet.cpp:6295` is purely reactive: it validates (water,
GM, tutorial zone, spell bonuses) and applies `SetHP(GetHP() - damage *
RuleR(...))`. The server does NOT independently compute fall damage from
`Handle_OP_ClientUpdate` position updates.

The **client** runs the fall damage logic in `FUN_00420dec` (VA 0x420dec,
`decompiled/ghidra/eqgame.exe.c:29228`), then sends `OP_EnvDamage` opcode
`0x31b3` (confirmed in `EQEmu/utils/patches/patch_Titanium.conf`).

**Consequence for eqoxide:** The Rust client MUST compute fall damage
and send `OP_EnvDamage` itself. The server will not impose it automatically
from position updates.

---

## Internal Spawn Object Fields (Client In-Memory, Not Wire)

| Offset | Type | Meaning |
|--------|------|---------|
| `spawn + 0x38` | float | z position |
| `spawn + 0x44` | float | z velocity from physics (negative = falling) |
| `spawn + 0x5c` | float | manual z velocity (keyboard input) |
| `spawn + 0x238` | float | step/collision height offset |
| `spawn + 0x1a8` | ptr | actor_ext pointer |
| `actor_ext + 0x48` | float | floor height (set by collision system) |
| `actor_ext + 0x4c` | float | fall-start z sentinel (-1e27 = not tracking) |

`spawn+0x44` is the **gravity-driven z velocity**. It starts at 0 when the
player leaves the ground and grows negatively each physics frame. On landing it
is reset to 0.0 (`eqgame.exe.asm:0x48b797`). This field is independent of
`spawn+0x5c` (keyboard swim/jump input).

---

## Landing Detection — FUN_0048b6c5 (VA 0x48b6c5)

`decompiled/ghidra/eqgame.exe.c:112968`

Landing fires when `spawn+0x38 - spawn+0x238 > actor_ext+0x48` (player has
moved below floor height). When landing and `spawn+0x44 <= -2.5`:

```
param2 = fabs(spawn+0x44 - (-2.5))   // = |z_vel + 2.5| = |z_vel| - 2.5 when falling
call FUN_0047ca9d(param2)              // -> FUN_00420dec (fall damage calc)
spawn+0x44 = 0.0                       // reset velocity
```

The `-2.5` threshold comes from `.rdata` constant at VA `0x642d5c` (confirmed
by PE binary extraction).

The intermediate function is `fabs()` at VA `0x60f60d` — confirmed by looking
at its body: it clears the sign bit of the IEEE754 double (at `eqgame.exe.asm:689190`
`AND eax, 0x7fffffff`), which is the definition of floating-point absolute value.

---

## Fall Damage Formula — FUN_00420dec (VA 0x420dec)

`decompiled/ghidra/eqgame.exe.c:29232`, capstone `eqgame.exe.asm:42694`

```
fall_score = (*(int*)(char_struct + 0xc2c8) * 0.01 + param2) - 1.5
```

Where:
- `char_struct + 0xc2c8` = integer field in the character class (safe fall skill
  or a related counter). **Starts at 0** (confirmed from initialization block at
  capstone `0x4bf432`). When 0, this term contributes nothing.
- `param2 = fabs(spawn+0x44 + 2.5)` = processed impact velocity from landing
  detection
- `1.5` subtracted = `.rdata` constant at `0x6410d0`
- `9.0` = lethal threshold at `0x6410cc`
- `0.01` scale at `0x640038`
- `10.0` multiplier for random damage at `0x6410c8`
- Lethal damage = `0x4e20 = 20000` HP

**Outcome (with char+0xc2c8 = 0):**

| `spawn+0x44` at landing | `param2` | `fall_score` | Result |
|--------------------------|----------|--------------|--------|
| > -2.5 | — | — | No damage check |
| -2.5 to -4.0 | 0 to 1.5 | ≤ 0 | No damage |
| -5.0 | 2.5 | 1.0 | Random damage (small) |
| -8.0 | 5.5 | 4.0 | Random damage (moderate) |
| -11.5 | 9.0 | 7.5 | Random damage (heavy) |
| ≤ -13.0 | ≥ 10.5 | ≥ 9.0 | Lethal (20000 HP) |

Non-lethal damage = `rand(0, fall_score^2 * 10)` (from capstone `0x420ec6`: fmul
st(1) twice with the 10.0 constant, then call to random function at `0x60e98c`).

After computing base damage, safe fall skill (ID `0x27` = 39) reduces it.
The skill check is at capstone `0x420ee1` (`call 0x41b0de(0x27)`).

---

## OP_EnvDamage Packet

Opcode `0x31b3` (Titanium). Struct is `EnvDamage2_Struct` from
`EQEmu/common/eq_packet_structs.h:3019` (31 bytes):

```
uint32 spawn_id;       // offset 0
uint8  unknown[2];     // offset 4
uint32 damage;         // offset 6
uint8  unknown2[12];   // offset 10
uint8  dmgtype;        // offset 22  -- 0xFC for falling
uint8  unknown3[4];    // offset 23
uint16 constant;       // offset 27  -- 0xFFFF
uint8  unknown4[2];    // offset 29
```

Set `dmgtype = 0xFC` (`DamageTypeFalling` in `EQEmu/common/eq_packet_structs.h:815`).
Observed at capstone `0x420ea2: mov byte ptr [ebp-0xa], 0xfc`.

---

## Server Validation in Handle_OP_EnvDamage

`EQEmu/zone/client_packet.cpp:6295`

The server:
1. Checks `dmgtype == EQ::constants::EnvironmentalDamage::Falling` (0xFC)
2. Checks water immunity: `zone->watermap->InLiquid()` → early return
3. Applies `ReduceFallDamage` spell/item/AA bonuses: `damage -= damage * mod / 100`
4. Checks tutorial zone / LoadZone (exempt)
5. Calls `TakeDamage(...)` with the CLIENT-provided `damage` value scaled by
   `RuleR(Character, EnvironmentDamageMulipliter, 1.0)` (default 1.0)

**Water negates fall damage on the server side** even if the client sends the
packet. Levitate (SE 57) prevents the client from even entering the fall state
(client-side cancellation before the fling/fall code runs).

---

## Fall Rate / Gravity Constant

**Partially determined; exact constant is in the actor physics vtable (opaque).**

What is known:
- `spawn+0x44` = z velocity accumulates from the actor physics vtable call at
  `decompiled/ghidra/eqgame.exe.c:112635`:
  `fVar11 = (*actor_vtable[0x28/4])()` — this is the gravity step per frame
- `spawn+0x44` is clamped to `[-128.0, +128.0]` (`.rdata` at `0x641f8c`/`0x649218`)
- The outgoing `delta_z` field in `PlayerPositionUpdateClient_Struct` is clamped to
  `[-12.8, 12.7]` EQ units per packet by the encoder `FUN_00465244`
  (from PE binary at `0x645ec8`/`0x645ecc`)

Manual (keyboard) z velocity uses:
```
step_velocity = frame_delta_ms * 0.02   // e.g., 25ms * 0.02 = 0.5 at 40fps
spawn+0x5c += counter * step_velocity   // counter up to 12 per frame
clamp to [-128.0, +128.0]
```
Constant `0.02` at `.rdata 0x6410b8`, max counter 12 gives max manual step ≈ 6.0
EQ/frame. This is manual swim/jump, NOT gravity.

Gravity (from actor vtable) remains opaque. The best observable bound:
- Client sends at most **12.8 EQ units per position update** as delta_z
- Internal velocity reaches lethal (-13.0) while delta_z stays within ±12.8
- This implies `spawn+0x44` and the network `delta_z` are in the same general
  unit system but `spawn+0x44` is per-frame, `delta_z` is per-update-interval

---

## Recommendation for eqoxide

### Fall Rate (Q1)

The gravity constant is inside the actor engine vtable and cannot be read
without tracing `dpvs.dll` or similar. For a **controlled fall in the
pathfinder bot**, use a fixed descent rate:

- Send delta_z = **-10.0 to -12.0 EQ units per position-update tick** for a
  "fast controlled drop" (close to terminal velocity as seen by the server)
- This gives a visually correct fall since the server will relay it to other
  clients at this rate
- At ~5 updates/sec (200ms): -10 EQ/update = -50 EQ/sec = ~50 feet/sec descent

Do **not** use a value above 12.8 (the client hard cap); values beyond that
would be anomalous and could be flagged.

### Fall Damage (Q2)

The Rust client **must** implement fall damage computation and send
`OP_EnvDamage`. The server does not auto-compute it.

Algorithm (with safe-fall skill = 0, which is correct for a bot without the
skill trained up, though the skill reduces damage):

```rust
// After descending and landing:
let z_vel = /* internal velocity accumulated during fall (negative) */;

if z_vel <= -2.5 {
    let param2 = (z_vel + 2.5).abs();   // = |z_vel| - 2.5
    let fall_score = param2 - 1.5;       // = |z_vel| - 4.0

    let damage: u32 = if fall_score >= 9.0 {
        20000  // lethal
    } else if fall_score > 0.0 {
        // random in [0, fall_score^2 * 10]
        rng.gen_range(0..=(fall_score * fall_score * 10.0) as u32)
    } else {
        0
    };

    if damage > 0 {
        send_env_damage(damage, 0xFC);  // OP_EnvDamage, dmgtype=falling
    }
}
```

For a pathfinder bot that does **controlled falls** (not free-fall from rest):
the bot can choose a descent velocity. If it limits delta_z to
**<= 4.0 EQ/update**, `|z_vel|` never exceeds 4.0 → no fall damage and no
packet needed. If a larger drop is required and the character will take damage,
compute and send the packet above.

Water and levitate are server-validated; the Rust client does not need to
special-case them (the server will absorb the damage packet if the character is
in water).

---

## Key File:Line Citations

| Finding | Source |
|---------|--------|
| Fall damage is client-computed | `EQEmu/zone/client_packet.cpp:6295` (`Handle_OP_EnvDamage`) |
| Spawn struct fields (+0x38/+0x44/+0x5c) | `decompiled/ghidra/eqgame.exe.c:112995-113020` |
| Landing detection function | `decompiled/ghidra/eqgame.exe.c:112968` (`FUN_0048b6c5`) |
| -2.5 velocity threshold | `decompiled/capstone/eqgame.exe.asm:180366` (`fcomp [0x642d5c]`) |
| fabs(z_vel+2.5) identification | `decompiled/capstone/eqgame.exe.asm:689189-689203` |
| Fall damage formula | `decompiled/ghidra/eqgame.exe.c:29253` (`FUN_00420dec`) |
| 1.5 safe threshold constant | `.rdata:0x6410d0` (PE binary verified) |
| 9.0 lethal constant | `.rdata:0x6410cc` (PE binary verified) |
| 0.01 scale constant | `.rdata:0x640038` (PE binary verified) |
| 10.0 damage multiplier | `.rdata:0x6410c8` (PE binary verified) |
| 20000 lethal damage | `decompiled/capstone/eqgame.exe.asm:42710` (`mov ebx,0x4e20`) |
| dmgtype 0xFC = falling | `decompiled/capstone/eqgame.exe.asm:42752`, `EQEmu/common/eq_packet_structs.h:815` |
| OP_EnvDamage opcode 0x31b3 | `EQEmu/utils/patches/patch_Titanium.conf` |
| EnvDamage2_Struct layout | `EQEmu/common/eq_packet_structs.h:3019` |
| delta_z client clamp [-12.8, 12.7] | `.rdata:0x645ec8/0x645ecc` (PE binary verified) |
| spawn+0x44 reset on landing | `decompiled/capstone/eqgame.exe.asm:180395-180397` |
| Manual z velocity code | `decompiled/ghidra/eqgame.exe.c:127777-127808` |
| 0.02 ms multiplier for manual velocity | `.rdata:0x6410b8` (PE binary verified) |
| Gravity from actor vtable | `decompiled/ghidra/eqgame.exe.c:112635` |
| Water immunity | `EQEmu/zone/client_packet.cpp:6318-6320` |
| ReduceFallDamage spell effect | `EQEmu/common/spdat.h:1291` (SE 228) |
| Safe fall skill (ID 39) | `decompiled/capstone/eqgame.exe.asm:42773` (`push 0x27`) |
