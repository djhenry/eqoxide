# Swimming, Water Regions, and Fall Damage (RoF2)

## 1. Water is a WLD BSP region type, not a special zone-line-like object

The zone's own `.wld` (inside `<zone>.s3d`) carries a 0x21 `WorldTree` BSP whose
leaves are tagged by 0x29 `Zone` fragments naming a region-type prefix. Confirmed:
the **terrain-type code is packed into the LOW NIBBLE (bits 0-3)** of the region's
cached flag word:

| Prefix | Low-nibble value | Meaning |
|---|---|---|
| `DR` | 0 | normal/dry |
| `WT` | 5 | water (swimmable) |
| `SL` | 6 | slime |
| `LA` | 7 | lava |
| `VW` | 8 | "vision water" (icy/underwater-vision variant, still swimmable) |
| `W2` | 9 | extra water tier |
| `W3` | 10 | extra water tier |

(e.g., `"WT"` sets the low nibble to 5 while preserving the high-order flag
bits). Separately, high bits of the SAME word carry unrelated flags:
`0x40000000`=PvP (2nd char `'P'`), `0x80000000`=zone-line (`"...TP..."`), plus
M/S/P/F suffix bits — see `zone-line-crossing.md` for the zone-line half of
this same parser.

**Server-side equivalent**: EQEmu compiles the identical WLD BSP into a `.wtr`-style
in-memory `WaterMapV1`/`WaterMapV2` (`EQEmu/zone/water_map_v1.cpp`,
`water_map_v2.cpp`) with `RegionTypeWater/VWater/Lava/PVP/ZoneLine`. `InLiquid()` =
`InWater() || InVWater() || InLava()` (`water_map_v2.cpp:47-50`). This is purely a
server-side data structure for gameplay checks (fall damage, drowning) — it is never
sent over the wire; the client does its own equivalent test against the WLD it
already loaded for rendering/collision.

eqoxide's asset server **already regenerates this exact tree** as a custom `.wtr`
format (`eqoxide_asset_server/src/bsp_regions.rs`, `wtr_from_wld`), and eqoxide's
client already has a loader/query (`src/region_map.rs`, `RegionMap::is_water`,
`RegionMap::surface_z`) — including a `surface_z()` binary search that finds the
water→air boundary directly above a submerged point, built for buoyancy (comment
cites issue #172). **This infrastructure is already correct and in place** — see §4
for where the gap actually is.

## 2. Swimming is the intended way across ground-level pools (no boat needed)

Boats (`boats-and-vehicles.md`) are `Race::Ship`/`Launch`/etc. NPCs used for
zone-to-zone ocean crossings (Butcherblock↔Freeport, Iceclad↔Timorous Deep, etc.).
A small in-village pool like Halas's central pond has no such NPC — it is a `WT`
water region the player simply swims across at the surface. This matches:
- `region_type_for_zone_name`/`wtr_from_wld` finds `WT` (water) regions in ordinary
  village zones (`bsp_regions.rs:37`), and the module's own test fixtures target
  exactly Halas/Everfrost's shared zone-line + water geometry
  (`bsp_regions.rs:227-263`, `halas_wtr_tags_the_everfrost_return_line_with_index_1`,
  `everfrost_wtr_tags_the_halas_zone_line_with_index_1`).
- No boat-race NPC spawn data is reachable from source (spawn placement is
  DB-only); this is **inferred from zone geometry** (a small pond has no ferry
  mechanic in classic EQ design), not directly confirmed from a spawn table dump.
  Cheap to verify further: query the live EQEmu `spawn2`/`spawngroup` tables for
  `zone='halas'` and check for any `Race::Boat`/`Ship` NPC — none is expected.

## 3. Fall damage is CLIENT-COMPUTED and entering water is NOT a "fall"

**Wire evidence (RoF2, confirmed):**
- `OP_EnvDamage` = `0x51fd` (`EQEmu/utils/patches/patch_RoF2.conf:272`).
- `EnvDamage2_Struct` is **39 bytes** in RoF2 (`EQEmu/common/patches/rof2_structs.h:3095-3107`):
  `id(u32)@0, unknown4(u16)@4, damage(u32)@6, unknown10(f32)@10, unknown14[12]@14,
  dmgtype(u8)@26, unknown27[4]@27, unknown31(u16)@31, constant(u16)@33 [always 0xFFFF],
  unknown35(u16)@35, unknown37(u16)@37`.
- `dmgtype` values (`EQEmu/common/eq_constants.h:815` + comment in
  `rof2_structs.h:3101`): `0xFA`=Lava, `0xFB`=Drowning, `0xFC`=Falling, `0xFD`=Trap.
  **Drowning is a SEPARATE damage type from Falling** — entering water is never
  reported as a fall in the first place; at most, staying submerged too long without
  air produces its own, unrelated `0xFB` drowning-damage packet.

**Server behavior — `Client::Handle_OP_EnvDamage`
(`EQEmu/zone/client_packet.cpp:6294-6323`):**
```cpp
if (app->size != sizeof(EnvDamage2_Struct)) { LogError(...); DumpPacket(app); return; }
...
if (ed->dmgtype == EQ::constants::EnvironmentalDamage::Falling) {
    if (zone->HasWaterMap()) {
        auto target_position = glm::vec3(GetX(), GetY(), GetZ());
        if (zone->watermap->InLiquid(target_position)) {
            return;   // <-- landing position is water/lava → the ENTIRE damage report is discarded
        }
    }
    uint32 mod = spellbonuses.ReduceFallDamage + itembonuses.ReduceFallDamage + aabonuses.ReduceFallDamage;
    damage -= damage * mod / 100;
}
```
**Confirms directly: fall damage is client-computed and self-reported (the server
never independently calculates it), but the server nulls it to ZERO whenever the
player's current tracked position is `InLiquid` on the zone's watermap** — this is
the authoritative, server-enforced guarantee that **falling/stepping into water at
any depth deals NO fall damage in RoF2**, independent of whatever the client itself
decided to compute or send.

**Struct-size gate matters**: any `OP_EnvDamage` packet with the wrong byte count is
dropped before the damage-type branch is even reached (`client_packet.cpp:6300-6304`),
so a malformed packet is equivalent to "no damage applied server-side" — but also
means a legitimately-lethal fall reported with a malformed struct would be silently
ignored by the server (own HP desyncs from server HP). See §4(b).

## 4. eqoxide gap analysis (confirmed by reading eqoxide's own source)

**(a) The navigator's A* (`src/assets.rs` `find_path`) already has water-aware
descent/ascent** (`assets.rs:1145-1208`, "WATER DESCENT" / "WATER ASCENT" comments) —
it lets a route pass into and back out of a water body when there's no dry
connection, and even hauls out onto a floor `<= surface + STEP_H` (`assets.rs:1187-1189`).
**But the z it stores per waypoint is the solid floor beneath the water (`nf` from
`column_floors`), not the water surface** — there is no call anywhere in
`assets.rs`'s `find_path` to `RegionMap::surface_z` (confirmed: `surface_z` is only
referenced from `src/movement.rs`/controller code, never from the pathfinder).

**(b) The walker (`src/eq_net/navigation.rs`) treats every big drop the same,
water or dry, and never queries the water region at all.** Confirmed:
`grep -n "RegionMap\|is_water\|surface_z" src/eq_net/navigation.rs` → **no hits**.
The controlled-fall / lethal-fall guard (`navigation.rs:1787-1809`) computes
```rust
let drop_to_target = gs.player_z - target.2;   // target.2 = waypoint z = pool BOTTOM for a water tile
if drop_to_target > FALL_TRIGGER && dist <= STOP_DIST + 8.0 {
    let (_, max_dmg) = fall_damage(drop_to_target);
    if gs.cur_hp > 0 && max_dmg >= gs.cur_hp as u32 {
        // "Fall too dangerous (HP too low) — stopped at the ledge"
        return;
    }
    ...
}
```
For a water-descent waypoint (e.g. Halas's pool, ~90u to the visible bottom), this
computes fall damage against the FULL floor-to-floor height as if it were a dry
cliff, refuses the "fall" as lethal, and the walker stops at the pool's edge — never
attempting the crossing at all, even though the real client would simply enter the
water at the surface (a few units down, not 90) and swim across with **zero** fall
damage (per §3).

**(c) `want_swim` is hardcoded `false` everywhere the navigator builds a `MoveIntent`**
— confirmed at `navigation.rs:1604`, `navigation.rs:1879`, and also in
`movement.rs:457`, `movement.rs:535`, `movement.rs:557`. The CharacterController DOES
support swimming (`movement.rs:169`, `let swimming = intent.want_swim &&
self.in_water;` — gated on BOTH flags), but since the navigator never sets
`want_swim = true`, that branch never engages during autonomous walking — even if
the walker were fixed to not refuse the crossing, the controller would sink/clip
rather than float/swim, because the navigator-driven intent never asks for swimming.

**(d) `build_env_damage_packet` is Titanium-shaped, not RoF2-shaped** — a secondary,
independent bug. `navigation.rs:224-232` builds a **31-byte** `EnvDamage2_Struct`
with `dmgtype` at offset 22 and the `0xFFFF` constant at offset 27; RoF2's real
struct (§3) is **39 bytes** with `dmgtype` at offset 26 and `constant` at offset 33.
Per `Handle_OP_EnvDamage`'s exact-size check (`client_packet.cpp:6300-6303`), the
RoF2 server discards every one of eqoxide's self-reported fall-damage packets
(wrong size) — meaning eqoxide's local `gs.cur_hp -= dmg` (`navigation.rs:1638`) is
purely a client-side cosmetic HP decrement that never reaches the server's
authoritative HP, an independent desync bug worth its own issue.

## Recommendation for eqoxide

1. **Route water crossings by surface, not floor.** In `find_path`'s WATER
   DESCENT/ASCENT branches (`assets.rs:1145-1208`), also compute
   `region_map.surface_z(b[0], b[1], nf)` (or equivalent) for a water-floor waypoint
   and store/tag that alongside the floor z — or simpler: emit the waypoint's
   traversal z as the surface height (or `min(current_z, surface_z)`), not the
   pool-bottom `nf`, whenever the tile is confirmed water via `RegionMap::is_water`.
2. **Teach the walker (`navigation.rs`) about water before applying the lethal-fall
   guard.** Around `navigation.rs:1787-1809`: before computing `drop_to_target` and
   refusing/controlled-falling, check whether the *target* (or the descent path) is
   `is_water` via the same `RegionMap`/`Collision::in_water` the controller already
   has (`assets.rs:552-559`, `movement.rs:150-152`). If so: skip the fall-damage
   guard entirely (native never takes fall damage into water — confirmed §3), set
   `want_swim: true` in the emitted `MoveIntent` for that leg, and let the
   controller's existing buoyancy (`surface_z`-driven, `movement.rs`) carry the
   character to the surface instead of running the "controlled fall to `land_z`"
   descent logic (which is dry-fall-only and assumes a hard landing).
3. **Never hardcode `want_swim: false` for a leg the navigator knows is in water.**
   Thread an "is this waypoint/segment submerged" bit through the `MoveIntent`
   construction sites (`navigation.rs:1604`,`1879`; `movement.rs:457/535/557` are
   free-look/WASD paths and can stay as-is unless they also need this).
4. **Fix `build_env_damage_packet` to the RoF2 39-byte layout** (§3d) regardless of
   the water fix — otherwise every fall-damage report eqoxide sends is silently
   dropped server-side (wrong size), leaving client and server HP out of sync after
   any real dry fall.
5. **Edge cases**:
   - `RegionMap::is_water` treats both `WT` (1) and `VW`/icy (7) as swimmable
     (`region_map.rs:123-125`) — match that when deciding to suppress fall damage.
   - The server's no-fall-damage rule is keyed on the **player's landing position**
     (`GetX/Y/Z` at the moment `OP_EnvDamage` is handled) being `InLiquid`, not on
     the fall's start point — so as long as eqoxide lands the character in the
     water region before/instead of reporting a big fall height, the server-side
     safety net (`client_packet.cpp:6311-6314`) applies regardless of any residual
     client-side fall math.
   - Drowning (`0xFB`) is unrelated to this fix and not currently implemented by
     eqoxide at all (no reference to `0xFB`/Drowning found) — out of scope here but
     worth its own follow-up if long underwater traversal needs an air/breath model.

Related: `player-movement-collision.md` (ground clamping, step height, `OP_ClientUpdate`
cadence), `zone-line-crossing.md` (the sibling `DRNTP` half of the same WLD region
parser), `boats-and-vehicles.md` (the other water-traversal mechanism).
