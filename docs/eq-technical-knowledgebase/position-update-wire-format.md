# OP_ClientUpdate Wire Format, Quantization, and the Native "Position Changed" Test (RoF2)

Companion to `player-movement-collision.md` §8-9 (cadence, server relay logic) — this file covers
the **bit-level wire encoding** of both OP_ClientUpdate structs and, critically, the **native
client's own "did anything change" epsilon test**, reverse-engineered directly from the RoF2
binary. Read this before touching `send_position_update`/`stream_position` in
`src/eq_net/action_loop.rs`.

## 1. The two structs are NOT the same layout

`OP_ClientUpdate = 0x7dfc` (`EQEmu/utils/patches/patch_RoF2.conf:113`) carries a **different
struct depending on direction** — RoF2 has no `ENCODE(OP_ClientUpdate)`/`DECODE(OP_ClientUpdate)`
override in `common/patches/rof2.cpp` (confirmed: `grep -i clientupdate` on that file is empty), so
both structs in `common/patches/rof2_structs.h` are raw wire layouts (no server-side transform).

### Client → Server: `PlayerPositionUpdateClient_Struct` (46 bytes, `rof2_structs.h:1653`)

Mostly **plain floats**, only heading/animation/delta_heading are bit-packed:

| Offset | Field | Type |
|---|---|---|
| 0 | `sequence` | u16, increments each packet |
| 2 | `spawn_id` | u16 |
| 4 | `vehicle_id` | u16 (0 unless on a boat) |
| 6 | unknown[4] | padding |
| 10 | `delta_x` | **float**, raw units/tick |
| 14 | `heading` | u32-aligned bitfield, low 12 bits |
| 18 | `x_pos` | **float** |
| 22 | `delta_z` | **float** |
| 26 | `z_pos` | **float** |
| 30 | `y_pos` | **float** |
| 34 | `animation` | u32-aligned bitfield, low 10 bits |
| 38 | `delta_y` | **float** |
| 42 | `delta_heading` | u32-aligned bitfield, low 10 bits, signed |

### Server → Client (relayed to OTHER clients): `PlayerPositionUpdateServer_Struct` (24 bytes,
`rof2_structs.h:1625`, header comment says "Size: 22" — that comment is **stale/wrong**, the real
size from the field layout is 24, confirmed independently by eqoxide's own test at
`src/eq_net/protocol/mod.rs:1168`)

Fully bit-packed, 5 × 32-bit words after `spawn_id:u16, vehicle_id:u16`:

```
word1 (@4):  pad:12          y_pos:19(signed)      pad:1
word2 (@8):  delta_z:13      delta_x:13             pad:6
word3 (@12): x_pos:19        heading:12(unsigned)   pad:1
word4 (@16): delta_heading:10 z_pos:19               pad:3
word5 (@20): animation:10    delta_y:13              pad:9
```

The RoF2 **spawn-appearance stream** (`OP_ZoneSpawns`/`OP_NewSpawn`, `Spawn_Struct_Position` at
`rof2_structs.h:404-425`) uses the **identical field order/widths** for its tail position block
(only the first word's leading 12 bits is named `angle` "pitch of camera?" instead of pure padding
— cosmetic, doesn't change the heading encoding). So a spawn's *initial* heading (from the big spawn
list) and its *incremental* heading (from `OP_ClientUpdate`) are the same encoding — see §3.

## 2. Fixed-point conversion factors — confirmed in `common/misc_functions.cpp:138-176`

```c
float EQ19toFloat(int d) { return d / 8.0f; }         int FloatToEQ19(float d) { return (int)(d*8.0f); }   // position
float EQ13toFloat(int d) { return d / 64.0f; }        int FloatToEQ13(float d) { return (int)(d*64.0f); }  // delta x/y/z
float EQ12toFloat(int d) { return d / 4.0f; }         int FloatToEQ12(float d) { return (int)((d+2048.0f)*4.0f) % 2048; }  // heading
float EQ10toFloat(int d) { return d / 20.0f; }        int FloatToEQ10(float d) { return (int)(d*20.0f); }  // delta_heading
```

- **Position** (`x_pos/y_pos/z_pos`, 19-bit signed): resolution 1/8 unit, range ±32768 units.
- **Position delta** (`delta_x/y/z`, 13-bit signed): resolution 1/64 unit/tick, range ±64 units/tick.
- **Delta heading** (`delta_heading`, 10-bit signed): resolution 1/20, range ±25.6 units/tick.
- **Heading**: see §3 — NOT a simple `/4` of degrees, the domain matters.

`Mob::MakeSpawnUpdate` (`zone/mob.cpp:1733-1748`, the function that builds the 24-byte struct
relayed to OTHER clients) and `Mob::SentPositionPacket` (`zone/mob.cpp:1712-1730`) both call these
helpers directly on `m_Position`/`m_Delta`, which is populated verbatim from whatever the moving
client sent in the *46-byte* struct (`Client::Handle_OP_ClientUpdate`, `zone/client_packet.cpp:5022-5024`
— `m_Position = glm::vec4(cx, cy, cz, new_heading); m_Delta = glm::vec4(ppu->delta_x, ppu->delta_y,
ppu->delta_z, EQ10toFloat(ppu->delta_heading));`). **The server does not validate or smooth these
values** — whatever eqoxide puts in the 46-byte struct is what other clients literally see
(quantized only by the EQ13/EQ19/EQ12/EQ10 conversions above).

## 3. Heading domain: internal EQ-heading-units are 0..512 = 0..360°, wire is 4× that (0..2047)

`Mob::GetHeading()`/`m_Position.w` is stored internally in **0..512 units representing a full
circle**, confirmed by an explicit decompiled comment: `zone/mob.cpp:4584`:
`heading = (heading * 360.0f) / 512.0f; // convert to degrees`. Also
`(boat->GetHeading() * 360.0) / 512.0` at `zone/client_packet.cpp:4914`.

`FloatToEQ12(d)` for `d` in `[0,512)` reduces to **`(d*4) mod 2048`** (the `+2048.0f` term is only
there to keep the argument non-negative before the `%`; it contributes `8192 mod 2048 == 0`). So:

```
wire_heading = (EQ_heading_units * 4) mod 2048        // EQ_heading_units ∈ [0,512), 360° = 512 units
             = (degrees * 2048/360) mod 2048
degrees      = wire_heading * 360/2048  =  wire_heading / 5.68888...
```

**This is one unified formula (`2048 = 360°`) used for BOTH structs** — `MakeSpawnUpdate` calls the
exact same `FloatToEQ12(m_Position.w)` that `Handle_OP_ClientUpdate` used to decode the incoming
46-byte struct's heading (`EQ12toFloat(ppu->heading)`, `client_packet.cpp:4903`). There is no
separate/smaller scale for the 24-byte struct.

**Wire values legitimately exceed 511** (e.g. 180° → `EQ_heading_units=256` → `wire=1024`). A decoder
that assumes `0..511 == 0..360°` (scale `512/360`) instead of `0..2047 == 0..360°` (scale `2048/360`)
is wrong by exactly 4× and will alias headings — e.g. wire=1024 (true 180°) decodes to 720° → wraps
to 0° under `mod 360`, i.e. a due-south spawn reads as due-north.

**eqoxide cross-check (separate bug, not the one under investigation):**
`src/eq_net/protocol/mod.rs:493-495` (`eq12_server_to_deg_cw`, used to decode OTHER spawns' headings
from the 24-byte struct AND by the loopback `encode_position_update`/`decode_position_update` pair)
uses `raw * 360/512` — the wrong (4× too coarse) scale per the analysis above. The 46-byte
client→server encoder (`deg_cw_to_eq12_client`, `mod.rs:516-518`, `deg*2048/360`) has the **correct**
scale and is confirmed correct by the module's own comment (a prior melee-facing bug was fixed by
finding this exact `2048/360` factor). **This is a candidate bug in how eqoxide reads other players'/
NPCs' facing direction, not in eqoxide's own outbound report** — flagged here because it directly
answers "heading encoding" but it is NOT the cause of the native-observer blip/slide bug (that bug is
about what eqoxide *sends*, not what it decodes). Worth its own fix/verification pass separately.

## 4. Server relay gating — the server does NOT rebroadcast unconditionally

`Client::Handle_OP_ClientUpdate` (`zone/client_packet.cpp:4832-5048`) only forwards a position
update to nearby clients if:

```c
bool positionUpdated = m_Position != prevPosition || m_Delta != prevDelta
                     || m_Delta != glm::vec4(0.0f) || prevAnimation != animation;
// client_packet.cpp:5027
if (positionUpdated) { /* broadcast MakeSpawnUpdate() to entity_list.QueueCloseClients(...) */ }
```

Note the third clause: **`m_Delta != glm::vec4(0.0f)` alone forces a broadcast even when position
and animation are unchanged** — i.e. sending ANY nonzero delta (even a float epsilon like `1e-6`)
every tick forces the server to keep re-relaying, and toggling between a truly-zero delta and a
near-zero-but-nonzero delta on alternating sends forces alternating broadcasts (delta≠0 → delta==0
→ `m_Delta != prevDelta` is true again) — i.e. **flicker in, flicker out**, exactly the "blip"
symptom, driven purely by how cleanly the outbound reporter zeroes its own deltas.

Range gating: `RuleI(Range, ClientPositionUpdates) = 300` EQ units
(`common/ruletypes.h:763`) — only clients within 300 units get the broadcast; unrelated to staleness.

**No stale-spawn/despawn timer exists for lack of `OP_ClientUpdate` traffic.** Grepped
`zone/entity.cpp`, `zone/entity_list.cpp`(via `SendPosition*`) for any time-since-last-update culling
— none found. A spawn that stops sending updates simply stays wherever it was last placed; it is
never auto-removed by the server for silence. (Confirms: any "blip in/out of existence" the native
observer sees is a rendering/interpolation artifact of the packets it DOES receive, not a
server-side despawn/timeout.)

## 5. Native client send cadence — confirmed via capstone disassembly

Cross-referenced against `player-movement-collision.md` §8 (already had this from an earlier pass);
independently re-derived here directly from `everquest_rof2/decompiled/capstone/eqgame.exe.asm`:

- **Rate gate:** `cmp esi, 0x118 (280); jbe <skip>` at `eqgame.exe.asm:0x0053e0c1` and again at
  `0x0053e278` — a packet is not even considered until **≥280 ms** have elapsed since the client's
  own last position-tick timestamp (`[eax+0x154]`, a running tick counter in ms).
- **Forced keepalive:** `cmp edx, 0x514 (1300); jb <skip>` at `0x0053e410` — reached only when the
  "did anything change" test (§6) returned **false** (stationary); in that branch the client still
  force-sends a packet every **1300 ms** even with nothing changed.
- **When something DID change** (§6 test returns true), the client proceeds straight to building/
  sending the packet without waiting for the 1300 ms keepalive timer — gated only by the 280 ms rate
  limit above.

So: **≥280 ms between any two `OP_ClientUpdate` sends; a genuinely idle client still sends one every
1300 ms as a keepalive; a moving client sends as fast as every 280 ms.**

## 6. The native "has anything changed" test — uses an EPSILON, not exact equality

Function at `eqgame.exe.asm:0x008d1da0` (called from the send-gate at `0x0053e3f2`) compares a
"current" vs "last-sent" internal position-update struct field by field:

- **Packed integer fields (already quantized to wire precision) compared for EXACT equality via
  XOR+mask:**
  - `heading` (offset+0x8, mask `0xfff` = 12 bits)
  - `animation` (offset+0x24, mask `0x3ff` = 10 bits)
  - `delta_heading` (offset+0x1c, mask `0x3ff` = 10 bits)
  - Any bit difference in these masked fields → immediately reports "changed" (`0x008d1dae`,
    `0x008d1dbb`, `0x008d1dc8`, each `jne 0x8d1dea`... — wait, actually the flow returns
    `al=0` at `0x8d1dea` and jumps FORWARD to the float epsilon tests only when these packed fields
    are equal; a difference here still falls through to further checks. Net effect: exact-int
    equality is checked first as a cheap short-circuit for the packed fields.)
- **Raw float fields (position + delta, 6 fields) compared against a global epsilon constant:**
  `fld dword ptr [0x9c4be8]` loads the epsilon once, then each field's `fsub`/`fabs`/`fcomp` is
  tested against it (`0x008d1dcf`-`0x008d1e5x`).
  - **Confirmed value: `DAT_009c4be8 = 0.001`** (extracted directly from the RoF2 binary's `.rdata`
    at file offset 6042600 — `float.frombytes = 0.0010000000474974513`). This constant is also used
    as a generic small-epsilon threshold elsewhere in the binary (`ghidra/eqgame.exe.c:65261`:
    `if (_DAT_009c4be8 < ABS(...))`, `:98750`: `if (ABS(fVar1) < (float10)_DAT_009c4be8)`).
  - A field is only "changed" if `abs(current - last_sent) > 0.001` world units.

**Answer to "does the native client send exactly-zero deltas when standing still": effectively
yes, by construction.** The native client's own position/velocity computation each tick is compared
against its LAST-SENT copy with a 0.001-unit epsilon before it ever builds a packet; if nothing
exceeds that epsilon, no send happens at all until the 1300 ms keepalive fires (and even then it
resends the *same* last values, not fresh noisy ones). There is no code path by which the native
client emits a nonzero-but-tiny delta while genuinely stationary — sub-epsilon jitter never reaches
the wire.

## 7. Diagnosis: why an eqoxide-driven character blips/slides for a native observer

`src/eq_net/action_loop.rs::send_position_update` (`:2376-2424`) computes:

```rust
let dx = x - gs.player_x;
let moving = dx != 0.0 || dy != 0.0 || dz != 0.0;   // EXACT float inequality — no epsilon
let anim: i32 = if moving { 1 } else { 0 };
```

This is the **inverse discipline** of §6: the native client only ever reports "moved" past a 0.001
epsilon (and only after quantizing to wire precision first); eqoxide's gate is an *exact* `!= 0.0`
on raw, unquantized floats coming straight out of the physics/render controller
(`self.controller.controller_view`). Any sub-visual jitter in the controller's per-tick position
(gravity/ground-snap/collision-resolution noise while nominally "standing still" — entirely plausible
for a continuously-stepped character controller) will trip `moving = true` on some ticks and `false`
on others, which:

1. Sends a **genuinely nonzero-but-tiny `delta_x/delta_y/delta_z`** as raw floats in the 46-byte
   struct (not epsilon-gated, not quantized client-side).
2. Server stores it verbatim into `m_Delta` and — per §4 — the `m_Delta != glm::vec4(0.0f)` clause
   **forces a broadcast** to nearby clients even though the true position barely changed.
3. The `animation` field flips `0`↔`1` in lockstep with `moving`, and is relayed to observers
   **completely unvalidated** (`Client::animation = ppu->animation` verbatim,
   `client_packet.cpp:5026`; `MakeSpawnUpdate` copies it straight into the 24-byte struct's 10-bit
   field for `IsClient()`, `mob.cpp:1742-1743`).
4. Because §4's condition is an OR across position/delta/animation, and eqoxide's own `moved`
   flag flickers tick-to-tick around the exact-zero boundary, the server broadcasts **repeatedly**
   with alternating "moving" (nonzero delta, anim=1) and "idle" (zero delta, anim=0) states even
   though the true position is static — exactly the observed "blip in/out" (animation/dead-reckoning
   state flicker) and "slide forward/back" (any observer client that dead-reckons position from a
   nonzero delta between updates, then gets corrected to the unchanged true position on the very next
   packet, will visibly rubber-band).

**This is inferred from code reading, not yet confirmed by a live packet capture against the native
observer**, but it is the only mechanism in either codebase that can produce oscillating delta/anim
values while the true position is static, and it maps cleanly onto both reported symptoms.

## 8. Recommendation for eqoxide

1. **Gate `moving`/anim/delta on an epsilon, not exact equality** — match the native 0.001-unit
   threshold from §6: `moving = dx.abs() > 0.001 || dy.abs() > 0.001 || dz.abs() > 0.001` (or a
   combined squared-distance threshold `0.001²`), and when NOT moving, **force `delta_x/y/z = 0.0`
   exactly** in the outgoing packet regardless of whatever raw sub-epsilon float noise the controller
   produced — do not just gate the "send" decision, also zero the payload so a coincidental send
   (e.g. the 1300 ms keepalive) never carries stale/noisy delta values.
2. Keep `anim` computed from the SAME epsilon-gated `moving` boolean (not the raw exact-inequality
   check) so it can't flicker independently of the delta fields.
3. The existing 280 ms / 1300 ms cadence constants (`POS_SEND_MOVING_MS`, `POS_SEND_KEEPALIVE_MS`,
   `action_loop.rs:22,24`) already match native — no change needed there; `player-movement-collision.md`
   §11's comparison table entry claiming eqoxide uses "150 ms, no keepalive" is **stale** (superseded
   by the current code) and should be corrected in that file.
4. Treat the `eq12_server_to_deg_cw` 4× heading-scale mismatch (§3) as a **separate** follow-up bug —
   fix it to `raw * 360.0/2048.0` (matching `deg_cw_to_eq12_client`'s inverse) — but do not conflate
   it with the blip/slide investigation; it affects reading OTHER spawns' facing, not what native
   observers see of eqoxide's own character.
5. The exact semantics of nonzero `animation` values beyond "0 = idle" are still **unconfirmed** —
   EQEmu treats it as an opaque relayed int (`client_packet.cpp:5026`, no server-side interpretation);
   sending a bare `1` for "moving" is a reasonable placeholder (matches the `pRunAnimSpeed=0`-at-rest
   convention used for NPCs, `mob.cpp:339`) but has not been verified against what the real client
   actually encodes for walk vs run. Cheapest way to pin it down: capture a native client's own
   `OP_ClientUpdate` packets while walking/running and read the raw `animation` field values.

## Related

- `player-movement-collision.md` §8-11 — cadence, server anti-warp thresholds, WASD rubber-band
  (a different, already-diagnosed bug in the visual-vs-server split, not this one).
- `animation-codes.md` — the WLD *visual* animation clip codes (L01/L02/O01/etc.) selected for
  rendering; a distinct layer from this file's wire `animation` field, though the wire field likely
  drives which of these clips an observer's client picks (unconfirmed link).
