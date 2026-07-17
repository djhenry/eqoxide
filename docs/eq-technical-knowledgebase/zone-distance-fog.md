# Zone distance fog (RoF2)

## Wire struct: NewZone_Struct fog fields (CONFIRMED)

Source (RoF2 wire struct, `structs::NewZone_Struct`, used directly by
`ENCODE(OP_NewZone)`): `EQEmu/common/patches/rof2_structs.h:565-661`.

```
/*0470*/ uint8  ztype;            // "Zone type (usually FF) FogOnOff"
/*0471*/ uint8  fog_red[4];
/*0475*/ uint8  fog_green[4];
/*0479*/ uint8  fog_blue[4];
/*0483*/ uint8  unknown323;       // padding
/*0484*/ float  fog_minclip[4];
/*0500*/ float  fog_maxclip[4];
...
/*0612*/ float  minclip;          // separate, NOT part of the fog_* arrays
/*0616*/ float  maxclip;
...
/*0916*/ float  fog_density;      // single scalar, not an array
```

Total struct size 948 bytes (`rof2_structs.h:565`). Field order on the wire
is exactly the struct's declared field order (verified — CORRECTION to an
earlier pass of this doc, see below).

The encode (`rof2.cpp:2368-2375`) loops over slot index `r` and writes all
5 fog fields per iteration:
```cpp
for (r = 0; r < 4; r++) {
    OUT(fog_red[r]); OUT(fog_green[r]); OUT(fog_blue[r]);
    OUT(fog_minclip[r]); OUT(fog_maxclip[r]);
}
```
This reads like a byte-interleaved serialization, but it is **not** one:
`OUT(x)` expands to `eq->x = emu->x;` (`common/patches/ss_define.h:67`),
and `eq` is a `structs::NewZone_Struct*` pointing directly at the outbound
packet buffer (`SETUP_DIRECT_ENCODE`/`ALLOC_VAR_ENCODE`,
`ss_define.h:41-43`). So `OUT(fog_red[r])` is a plain **field assignment**
into the `fog_red[4]` array member at its fixed struct offset — the `for`
loop is just C++ convenience to fill 5 array fields together, not a byte
serializer. After the loop, `fog_red[0..4]`, `fog_green[0..4]`,
`fog_blue[0..4]`, `fog_minclip[0..4]`, `fog_maxclip[0..4]` each occupy their
own contiguous span, exactly as declared in `rof2_structs.h` and exactly as
shown in the offset table above (471/475/479/484/500). **The wire layout is
struct-order, not interleaved** — corrected after independently checking
`ss_define.h`'s `OUT` macro. `fog_density` is copied separately near the end
of the encode (`rof2.cpp:2411`), landing at its struct offset 916.

eqoxide's existing wire struct at `src/eq_net/protocol/mod.rs` already had
`fog_red/green/blue: [u8;4]` and `fog_minclip/fog_maxclip: [f32;4]` at the
correct offsets (471/475/479/484/500) — only `fog_density` @916 was missing
(it fell inside a padding span) and has now been added, confirmed against
`rof2_structs.h:653`.

## Where the DB values come from (CONFIRMED, server side)

`Zone::LoadZoneCFG`, `EQEmu/zone/zone.cpp:1302-1330`, maps DB columns to the
4 wire slots:

```
fog_red[0]  = z->fog_red      fog_red[1..3] = z->fog_red2/3/4
fog_minclip[0] = z->fog_minclip   fog_minclip[1..3] = z->fog_minclip2/3/4
(same pattern for green/blue/maxclip)
fog_density = z->fog_density   (single column, no array)
```
The DB also has orphaned `fog_red1/fog_green1/fog_blue1/fog_minclip1/
fog_maxclip1` columns (`EQEmu/common/repositories/base/base_zone_repository.h:66-96`)
that are **never** read by `LoadZoneCFG` — they're only reachable through
a separate, inconsistent `ZoneStore::GetZoneFogRed/...(zone_id, slot, ...)`
Lua-quest-API accessor family (`EQEmu/common/zone_store.cpp:334-442`,
`FOG_SLOT_ONE..FOUR`) that uses a *different* index convention than the
wire array. Don't use that accessor's indexing as a guide to the wire
layout — it's a known internal inconsistency in EQEmu, not evidence about
client behavior.

## Semantics of the 4 slots (INFERRED, not confirmed from the client)

- The in-game "#fog_red"-style quest command (`QuestManager::UpdateZoneHeader`,
  `EQEmu/zone/questmgr.cpp:4058-4077`) always fills **all 4 slots with the
  same value** in a `for (i<4)` loop — content authors never differentiate
  the 4 slots in practice. This strongly suggests that whatever the client
  does with 4 slots, real zone content relies on all 4 being identical, so
  getting slot selection wrong is very unlikely to be visibly wrong on
  live-authored zones.
- Checked the classic WLD BSP `Region` fragment (type 0x22) for a possible
  "fog index 0-3" selector: it only has a single **boolean** `REGIONFOG`
  bit (bit 3, 0x08) — "does fog apply in this region" — not a 4-way
  selector (`libeq/crates/libeq_wld/src/parser/fragments/region.rs:29,323,351-353`).
  So regions don't pick among the 4 slots either.
- I could not find, within budget, the eqgame.exe call site that reads
  `fog_red[n]/fog_minclip[n]/...` for a specific `n` (eqgame.exe is
  stripped; no cross-reference path found from the `NewZone` packet fields
  to a specific array index). **Not confirmed** which slot(s) the RoF2
  client actually samples.
- Recommendation: treat slot **0** as authoritative (it's the one fed by
  the un-suffixed DB columns and is what every non-quest-scripted zone
  effectively uses, since the other 3 default to 0/unset unless a quest
  script overwrote them — and even then it fills identically). Parse all 4
  for forward compatibility but only need to implement rendering for slot 0.

## D3D fog mode: LINEAR, not exponential — CONFIRMED from the client decompile

Function `FUN_10096640`, `everquest_rof2/decompiled/ghidra/EQGraphicsDX9.dll.c:123400-123431`
is the engine's "SetFog" routine (`this, poolPtr, enable, start, end, density, color`).
When `enable != 0` it stages these exact D3D9 render states through the
render-state cache setter `FUN_1009e820` (confirmed at
`EQGraphicsDX9.dll.c:128710-128737`, which just writes
`this[state*4+0xc]=value` and tracks a dirty min/max range — i.e. it is
`SetRenderState(state, value)` with caching, not something else):

```
D3DRS_FOGENABLE     (0x1c/28) = 1
D3DRS_FOGCOLOR      (0x22/34) = color              (packed ARGB)
D3DRS_FOGSTART      (0x24/36) = start
D3DRS_FOGEND        (0x25/37) = end
D3DRS_FOGVERTEXMODE (0x8c/140) = 3 = D3DFOG_LINEAR   <-- confirms LINEAR
D3DRS_FOGTABLEMODE  (0x23/35) = 0 = D3DFOG_NONE      <-- pixel/table (exp) fog explicitly OFF
```
(`EQGraphicsDX9.dll.c:123419-123427`). `D3DRS_FOGDENSITY` (0x26/38) is
**never** set by this function — `fog_density` never reaches the
fixed-function exponential-fog render state at all.

When `enable == 0`, the function only clears `D3DRS_FOGENABLE` and returns
(`EQGraphicsDX9.dll.c:123429-123430`) — nothing else is touched, i.e. "no
fog" is a hard render-state toggle, not a special-cased color/clip value.

D3DFOG_LINEAR interpolates the fog blend factor as
`t = saturate((FogEnd - dist) / (FogEnd - FogStart))` between `FogStart`
(0% fog) and `FogEnd` (100% fog) — this is the standard D3D9 fixed
semantics for `D3DFOG_LINEAR`, not something client-specific.

## Shader-side fog (CONFIRMED parameter set, formula INFERRED)

Because vs_1_1+ vertex shaders are used for terrain/region rendering, and a
bound vertex shader overrides `D3DRS_FOGVERTEXMODE` (the VS must itself
write the `oFog` output register), the engine *also* exposes fog as named
D3DX Effect parameters, confirmed by extracting strings from the compiled
`.fxo` shader library shipped with the client
(`everquest_rof2/RenderEffects/MPL/*.fxo`, `.../SPL/*.fxo` — e.g.
`Terrain_Base.fxo`, `Region_Base.fxo`, `Region_Full.fxo`, `RegionTerrain.fxo`
all show the identical pattern):

- Global effect parameters (semantic-looked-up once by the engine):
  `FogStart`, `FogEnd`, `FogDensity`, `FogRangeInv`
  (handle registration confirmed at
  `EQGraphicsDX9.dll.c:9264-9273`, via `GetParameterBySemantic(NULL, "FogStart"/"FogEnd"/"FogDensity"/"FogRangeInv")`).
- **`FogRangeInv` is computed live, in native code, as
  `1.0f / (FogEnd - FogStart)`** — confirmed at
  `EQGraphicsDX9.dll.c:123421-123824`:
  ```c
  (**(code**)(*param_2+0x78))(param_2, handle_FogRangeInv,
      1.0 / (*(float*)(this+0xa9bc) - *(float*)(this+0xa9b8)));  // FogEnd - FogStart
  ```
  This is the classic reciprocal-range optimization for **linear**
  interpolation (avoids a per-vertex divide); it has no meaning for
  exponential fog. This is independent, corroborating evidence for LINEAR
  fog on top of the D3DRS_FOGVERTEXMODE=3 finding above.
- Per-technique vertex shader constant tables (CTAB) consistently pull in
  `a_fFogDensity + a_fFogStart + fFogRange` (the last one a **preshader**-
  computed local, `PRES`/`FXLC` blocks whose own CTAB lists only
  `a_fFogEnd, a_fFogStart` as inputs — i.e. the preshader computes
  `fFogRange` from `FogStart`/`FogEnd` before the VS runs). Same pattern in
  every region/terrain technique checked (`Terrain_Base.fxo`,
  `Region_Base.fxo`, `Region_Full.fxo`, `RegionTerrain.fxo`).
- **Inferred (not bytecode-disassembled)**: the shader likely computes a
  linear 0..1 fog fraction from `(dist, FogStart, fFogRange≈FogRangeInv)`
  and then uses `FogDensity` as a secondary **intensity/opacity cap**
  (e.g. `finalBlend = linearFogFraction * FogDensity`), not as an
  exponential decay exponent. This is supported by:
  - `FogDensity` is never wired to `D3DRS_FOGDENSITY` (confirmed above).
  - EQEmu's own comment says most zones ship `fog_density = 0.33`
    (`EQEmu/common/patches/rof2_structs.h:653`) — a value of 0.33 is a
    plausible max-blend-intensity (33%) but implausible as a true
    `D3DFOG_EXP`/`EXP2` density coefficient at typical EQ outdoor draw
    distances (hundreds of units), where a density that large would fully
    white out geometry within a few units. This also matches the user's
    observed behavior — "translucent... fades into a colored haze" (not a
    hard opaque wall), consistent with a capped-intensity linear blend.
  - I did not disassemble the actual `vs_1_1`/preshader bytecode token-by-
    token to confirm the exact arithmetic (effort/ROI tradeoff — the
    render-state-level evidence above is already unambiguous for the
    primary question of linear-vs-exponential). If exact per-pixel parity
    with native is ever needed, the bytecode is recoverable at
    `RenderEffects/{MPL,SPL}/*.fxo` (search for the `vs_1_1`/`PRES` tokens,
    e.g. via the byte pattern `01 01 FE FF` for the `vs_1_1` version token).

## Recommendation for eqoxide

1. Parse `NewZone_Struct` fog fields per the offsets above (struct-order —
   see the correction above — plus the separate scalar `fog_density`
   @916). `src/eq_net/protocol/mod.rs` now has `fog_density` added at 916.
2. Implement **linear** fog: `t = saturate((dist - fog_minclip[0]) /
   (fog_maxclip[0] - fog_minclip[0]))`, fog color = `(fog_red[0],
   fog_green[0], fog_blue[0])`, and multiply the final blend by
   `fog_density` as an intensity cap:
   `outColor = lerp(litColor, fogColor, t * fog_density)`.
   Use slot **0** — see the "Semantics of the 4 slots" caveat above.
3. Disable fog entirely (skip the blend) when `ztype`/enable indicates it's
   off. I could not confirm the exact enable-flag source in eqgame.exe
   within budget; `ztype` is commented `"FogOnOff"` in the struct
   (`rof2_structs.h:571`) and is the likeliest source. As a safe fallback
   that matches the native "no fog = FOGENABLE false, nothing else touched"
   behavior, also treat `fog_maxclip[0] <= fog_minclip[0]` (degenerate/zero
   range) as "disabled" to avoid a NaN/inf-slope blend.
4. This is a per-vertex (not per-pixel/table) linear fog in native RoF2;
   an eqoxide shader can replicate the *visual* result equally well as a
   per-pixel linear fog (per-pixel linear fog is a superset/smoother
   version of per-vertex linear fog on typical EQ terrain tessellation) —
   don't feel compelled to under-tessellate to match vertex-fog banding.
