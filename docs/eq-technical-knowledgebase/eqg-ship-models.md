# EQG Ship/Boat Models (RoF2)

**Status: confirmed by direct binary extraction/parsing of the RoF2 install
(`~/eq_assets/everquest_rof2/`).** All struct offsets below were validated by
parsing real files end-to-end (computed end-of-parse offset == exact file
length, valid vertex-index bounds, unit-length normals). Scratch scripts:
`/tmp/claude-*/scratchpad/pfs2.py` (PFS reader, fixed — see §0), `extract2.py`
(entry extractor).

Related: `boats-and-vehicles.md` (server/wire side of boat NPCs — race ids,
opcodes, vehicle_id), `eqg-character-models.md` (PFS container format basics,
also documents the *skinned* `EQGA` v2 NPC format used by ordinary creatures).

---

## 0. IMPORTANT FIX to the PFS filename-pairing algorithm

`eqg-character-models.md` states filenames pair with directory entries "in
index order." **This is incomplete/misleading and produces silently wrong
data.** Verified against the actual reference implementation,
`EQEmu/utils/deprecated/azone2/pfs.cpp:116-181` (`PFSLoader::Open`):

- The on-disk directory (`struct_directory{crc,offset,size}` array) is stored
  **sorted by CRC ascending** (confirmed empirically: CRCs in shipmvm.eqg's
  raw directory are monotonically increasing).
- The filename block (the entry whose CRC equals the fixed sentinel
  `0xC90A5861`, byte-order-reversed in the source via `ntohl` — i.e. the
  magic is `0xC9 0x0A 0x58 0x61` as an LE u32 read `0x61580AC9`, both notations
  refer to the same 4 bytes) lists filenames in **original pack order**, which
  equals **ascending data offset**, NOT CRC order.
- `pfs.cpp:153-174` makes this explicit: it collects `(file_dir_record_pos,
  data_offset)` for every non-filename-block entry in raw (CRC) order, then
  **bubble-sorts that array by `data_offset`** before it's usable — filenames
  are then indexed 1:1 against the offset-sorted array, not the CRC-sorted raw
  array.

**Bug this causes if skipped:** zipping filenames directly against the raw
(CRC-ordered) directory silently pairs most names with the wrong file (e.g. a
`.ter` name ends up pointing at a `.dds`'s bytes). Symptom: header magic
doesn't match the expected type, or "inflated size" values cluster around a
few suspicious repeated constants (a strong tell that pairing is scrambled).
**Fix: sort non-directory entries by `offset` ascending before zipping with
the filename list**, exactly like the reference `pfs.cpp`.

---

## 1. EQG container = same PFS format as S3D (confirmed)

`shipmvm.eqg`, `shipmvp.eqg`, `shi.eqg`, `row.eqg`, etc. are ordinary PFS
archives (see `eqg-character-models.md` for the byte-level PFS layout — that
part is correct). No S3D vs EQG container difference; only the *internal*
file formats/magics differ.

## 2. Two very different kinds of ship archive exist — do not conflate them

### 2a. `shipmvm.eqg` / `shipmvp.eqg` / `shipuvu.eqg` / `shippvu.eqg` / `shipworkshop.eqg` = FULL SUB-ZONES, not NPC models

Confirmed: `eqgame.exe.c:636134-636138` registers `"shipmvp"`, `"shippvu"`,
`"shipuvu"`, `"shipmvm"` as actual **zone IDs** (`FUN_007dc430(0xd, zone_id,
"shipXXX", "The Open Sea", ...)`), i.e. these are the classic "you board the
ferry and get zoned into the ship's interior" instance zones (the RoF2
successor to the old Trilogy-era boat-crossing mechanic), not a spawn's
render model. Their `*_chr.txt` (e.g. `shipmvm_chr.txt`) is the normal
**zone** race-code table (which NPC model codes are valid spawns *inside*
that zone), unrelated to what a "Ship" NPC race looks like out in the open
world.

Internally `shipmvm.eqg` (9.5 MB) contains ~196 files: two `.ter` (terrain)
pieces (`ter_grnd.ter` = main hull/deck, `ter_box.ter` = a secondary/collision
hull) plus ~75 unique `.mod` (static prop) files — masts, ropes, doors, crates,
furniture, ladders — placed **391 times** via `shipmvm.zon` (see §5). This is
architecturally a mini-zone (terrain + placed-object list), the exact same
pattern as a normal zone's `.zon`/`_obj.s3d`.

### 2b. `shi.eqg` / `row.eqg` = the actual boat-race NPC render models

**Confirmed by cross-referencing zone `_chr.txt` race-code tables**, not
guessed from the filename alone:

- `grep -l "^shi," *_chr.txt` → `butcher_chr.txt`, `erudsxing_chr.txt`,
  `erudnext_chr.txt`, `oceanoftears_chr.txt`, `freeporteast_chr.txt`,
  `qeynos_chr.txt`, `dragonscale_chr.txt`, `maidensgrave_chr.txt`,
  `lopingplains_chr.txt` — i.e. race code `shi` is a valid NPC spawn model in
  **every classic ferry-crossing zone** (Butcherblock↔Erudin↔Freeport↔Qeynos
  ocean routes). This is decisive: `shi` = the **Ship** race (72) model.
- `grep -l "^row," *_chr.txt` → `argath_chr.txt`, `buriedsea_chr.txt`,
  `jardelshook_chr.txt`, `sarithcity_chr.txt` (later-era zones with small
  rideable boats) → `row` = **Rowboat** (race 502).
- `ramp.eqg` is a small loading-ramp prop (`obj_ramp_solid.mod`/
  `obj_ramp_transp.mod`), not a race model.
- `gho.eqg` looked promising for `GhostShip` (114) by name but its contents
  (`idle/jmpa/jmpu/nrun/slpr/stnd/stun/swim/turn/walk` + `crch`/`flch`/`gcst`
  animation set) are a full **biped locomotion set** — this is the ordinary
  "Ghost" humanoid monster model, **not** the GhostShip vehicle. Do not reuse
  this filename-coincidence lead.
- **Not yet located/confirmed**: dedicated single-mesh models for `Launch`
  (73), `GhostShip`/`GhostShip2` (114/552), `DiscordShip` (404), `Boat2`
  (533), `MerchantShip`/`PirateShip` (550/551), `ElvenBoat`/`GnomishBoat`/
  `UndeadBoat` (544-546), `BlimpShip` (693). Best inference (not confirmed):
  the larger sailable-ferry races render using the §2a sub-zone hull meshes
  (`shipmvm`=merchant?, `shipmvp`=pirate?, `shipuvu`/`shippvu`=undead/ghost
  variants — names suggestive but unconfirmed) directly as their NPC model,
  OR there are additional short-code `.eqg` archives not yet identified.
  **Cheapest way to resolve definitively**: capture an `OP_ZoneSpawns`/
  `OP_NewSpawn` packet for one of these races in a live zone and correlate; or
  grep more `*_chr.txt` files for 3-letter codes not yet mapped.

## 3. The `.mod`/`.ter` binary format — TWO sub-versions, confirmed by direct parse

Both magics share a common header shape; the **version field controls the
vertex record layout**, and a `bone_count` field controls whether skin data
follows. **Always branch on `version`, never hardcode one vertex stride.**

### Common header
```
offset 0  : char[4]  magic         "EQGM" (mesh) or "EQGT" (terrain)
offset 4  : u32      version       1 or 3 confirmed in RoF2 files
offset 8  : u32      string_table_len
offset 12 : u32      material_count
offset 16 : u32      vertex_count
offset 20 : u32      polygon_count (triangle count)
offset 24 : u32      bone_count    -- EQGM only; EQGT has no bone_count field
                                      (header is 24 bytes for EQGT, 28 for EQGM)
[string_table_len bytes]: NUL-terminated string pool (material/shader/
  texture/bone names), referenced by byte offset from material records below.
```
Confirmed instances:
- `ter_grnd.ter` (shipmvm.eqg): magic EQGT, version=3, mat=26, vert=704,
  poly=960, header=24B — parses to **exactly** EOF.
- `obj_mast_mid.mod`, `obj_deck_main.mod` (shipmvm.eqg): magic EQGM,
  version=3, bone_count=0 (static) — same string table/material list as
  `ter_grnd.ter` (all ~75 pieces of one ship embed an identical, mostly-unused
  copy of the full 26-material palette — redundant but harmless; don't
  assume string_table_len/mat_count differing = a bug).
- `shi.mod` (shi.eqg, "Ship" NPC model): magic EQGM, **version=1**,
  bone_count=**21** (`ROOT_BONE`, `BONE02`, `BONE03`, ...) — this model IS
  skinned (has `stnd_ba_1_shi.ani` / `walk_ba_1_shi.ani` companion animation
  clips in the same `EQGA` v2 format documented in `eqg-character-models.md`
  for NPC creatures — same bone-name string table pattern). Ships have a
  gentle idle/walk sway animation via a real skeleton, not a static mesh.
- `row.mod` (row.eqg, "Rowboat" NPC model): magic EQGM, version=1,
  **bone_count=0** (static) — confirms bone_count, not the version field
  alone, is the real "is this skinned" signal.
- `col_shi.mod` / `col_row.mod`: `col_` prefix = a separate simplified
  **collision** mesh shipped alongside the visual mesh (both static,
  bone_count=0). `shi.lod` is a tiny (38-byte) plain-text LOD/collision
  reference file: `"EQLOD\r\nLOD,SHI,2500.000\r\nCOL,COL_SHI\r\n"`.

### Material record (immediately after the string table)
```
u32 material_index
u32 name_offset       -> string table (material name, e.g. "intmast")
u32 shader_offset     -> string table (shader/fx name, e.g. "Opaque_MPLBump.fx")
u32 property_count
property_count × {
  u32 prop_name_offset -> string table (e.g. "e_TextureDiffuse0")
  u32 prop_type         -- 0 = float, 2 = string (texture filename, ->
                           string table), 3 = packed color (u32 ARGB/RGBA)
  u32 prop_value        -- reinterpret per prop_type (float bits / string
                           offset / raw color int)
}
```
Confirmed by full-fidelity parse of `obj_mast_mid.mod`'s 26 materials:
diffuse/normal/coverage texture triples for `Opaque_MPLBump.fx`, a
`Opaque_MaxWater.fx` water material with float + color params, `alpha`/
`sidepulley` single-texture `Chroma_MPLBasicAT.fx` cutout materials, etc. For
rendering purposes **only `e_TextureDiffuse0` (prop_type=2) is required** —
treat all other properties (normal maps, coverage/shininess) as optional PBR
extras.

### Vertex record — version-dependent stride (the critical gotcha)
- **version 1** (`row.mod`, confirmed exact-EOF parse): **32 bytes**
  ```
  float x,y,z         // position
  float nx,ny,nz       // normal
  float u,v            // texcoord
  ```
- **version 3** (`ter_grnd.ter`, `obj_mast_mid.mod`, confirmed exact-EOF
  parse): **44 bytes**
  ```
  float x,y,z          // position
  float nx,ny,nz        // normal
  u32   color           // packed vertex color (RGBA), NOT a float — reading
                          // it as f32 produces NaN, a good version-detector
  float u,v             // texcoord 0
  float u2,v2            // texcoord 1 (observed always 0,0 in ship pieces —
                          // likely an unused lightmap channel; safe to ignore)
  ```
- If `bone_count > 0` (only observed on `shi.mod` so far — race 72's actual
  model), an additional per-vertex bone-weight block almost certainly follows
  the base vertex array before the polygon array (mirrors the `EQGA`
  skinned-NPC pattern flagged as "not yet reversed" in
  `eqg-character-models.md`). **Not reverse-engineered in this pass** — out
  of scope for a static-mesh MVP.

### Polygon (triangle) record — confirmed **20 bytes**, both versions
```
i32 v1, v2, v3      // vertex indices
i32 material_index  // index into this file's material list
i32 flag            // bitflag, observed values 0x100000 / 0x400000 in
                      // shipmvm pieces — meaning not identified (possibly
                      // collision/smoothing-group/transparency bit); safe to
                      // ignore for a first-pass renderer
```

## 4. Coordinate system — same convention eqoxide already uses for S3D

Confirmed: RoF2 EQG vertex data is **Z-up**, exactly like S3D/WLD. E.g.
`ter_grnd.ter` (shipmvm's main hull) bbox: X∈[-2659,2130], Y∈[-2412,2377],
Z∈[-528,351] (Z is the ~880-unit vertical extent of a full sailing ship's
hull+deck+superstructure — X/Y are the ~4800×4800-unit horizontal footprint).
`row.mod` (Rowboat) bbox: X∈[-14.9,22.9] (~38 units long), Y∈[-8.4,8.3]
(~16.7 wide), Z∈[-4.0,6.0] (~10 tall) — sane real-world rowboat proportions.
Normals are unit-length in EQ-native space before any transform.

**No EQG-specific coordinate fixup is needed.** Apply exactly the same
Z-up→Y-up conversion eqoxide's S3D pipeline already uses:
`eqoxide_asset_server/src/convert/mod.rs:1604` —
`Quat::from_axis_angle(Vec3::X, -FRAC_PI_2)` applied to both positions and
normals (a pure rotation preserves triangle winding, so no index-order flip is
needed either).

## 5. `.zon` placement format (needed only for the full multi-piece ship, NOT MVP)

`shipmvm.zon`: magic `EQGZ`, **version 2** (the version-1 branch is what's
decompiled at `EQGraphicsDX9.dll.c:87371` `FUN_10064da0`, gated on
`*(int*)(param_2+4) == 1` — RoF2's own top-level zone `.zon` files are
presumably version 1; this ship sub-zone `.zon` is version 2, a variant not
covered by that decompiled branch). Header (28 bytes, all confirmed by exact
byte offsets matching the decompile's field reads):
```
char[4] magic "EQGZ"
u32 version        (2 for shipmvm.zon)
u32 string_table_len   (0x261b = 9755)
u32 model_count        (0x4b = 75 unique .mod/.ter names)
u32 placeable_count     (0x187 = 391 placed instances)
u32 unknown1 (0x2c=44)  -- not decoded
u32 unknown2 (0)        -- not decoded
[string_table_len bytes]: NUL-terminated model filenames, listed as (model
  filename, instance name, instance name, ...) groups — e.g.
  "TER_grnd.TER\0TER_grnd\0OBJ_deck_trd.MOD\0OBJ_deck_trd\0OBJ_deck_trd01\0..."
  i.e. one `.mod`/`.ter` filename can back multiple named placed instances.
```
A `model_count`-entry (4 bytes each) name-offset-fixup table follows the
string table (`EQGraphicsDX9.dll.c:87439-87450`), then the **placeable
array**: confirmed **36-byte stride** per record
(`local_268 = pcVar16 + placeable_count*0x24 + model_count*4` at
`EQGraphicsDX9.dll.c:87454`). Partially decoded fields (not fully solved —
lower priority since §6 MVP doesn't need this):
```
offset 0x00: u32 model_name_index (into the model name table)
offset 0x04: u32 (another string/name offset, used to build a per-instance path)
offset 0x08: u32 unknown
offset 0x0c: u32 unknown
offset 0x10: u32 unknown
offset 0x14: f32  * scaleA   -- likely one position or rotation axis
offset 0x18: f32  * scaleB   -- DIFFERENT scale constant than 0x14/0x1c;
                                likely a rotation axis in different units
offset 0x1c: f32  * scaleA
(remaining ~8 bytes to reach 0x24: not decoded — likely scale, further rotation)
```
**Not needed for a first-pass ship renderer** — see recommendation below.

---

## Recommendation for eqoxide / eqoxide_asset_server

### Minimum viable path (recommended first PR)

1. **Use `row.mod` (Rowboat, race 502) as the first target** — it is fully
   solved end-to-end by this pass (32-byte vertex, 20-byte triangle, static,
   3 materials, ~18 KB), and is the *smallest, simplest, fully-static* real
   ship model available. Convert `row.eqg` → `row.glb`: read PFS (with the
   offset-sort fix in §0), parse `row.mod` per §3 (version 1 → 32B vertex),
   emit one glTF mesh + one base-color texture per material (decode DDS →
   PNG, same as the existing S3D texture path), apply the §4 Z-up→Y-up
   rotation. No skeleton, no animation, no `.zon` placement — a pure static
   mesh converter, structurally simpler than the existing skinned S3D
   character path.
2. **For the big ferries** (`Ship`=72 and friends), the pragmatic first step
   is **`ter_grnd.ter` alone** from `shipmvm.eqg` (version 3 → 44-byte
   vertex) as a visual stand-in for the hull+deck — it is a single confirmed,
   fully-parseable mesh (704 verts/960 tris) with a real ship-sized bounding
   box, requiring zero `.zon` placement work. This won't have masts/rigging/
   furniture, but is a dramatically better placeholder than the current "HUM"
   billboard and is a small, self-contained converter change.
3. **Defer**: `shi.mod`'s skeleton/animation (needs the unreversed skinned
   `EQGM v1`+bone-weight vertex extension) and the full `shipmvm.zon`
   391-instance placement (needs the remaining `0x24`-byte placeable fields
   decoded) to a follow-up PR once the static path is proven.

### Converter shape (`eqoxide_asset_server/src/convert/mod.rs`)

Add an `eqg_to_glb_model` alongside `s3d_to_glb_model` sharing the PNG/DDS
decode and glTF-writing tail (`add_view`, material_to_gltf, the Z-up→Y-up
rotation block at line ~1604) with the existing S3D path — only the front
half (PFS-entry pairing fix from §0, then the `.mod`/`.ter` header/vertex/
polygon parse from §3) is new code. Branch vertex stride on the `version`
field (1→32B, 3→44B); treat any `bone_count > 0` file (only `shi.mod` so far)
as unsupported for now and fall back to the placeholder rather than
misparsing it.

### Client side (eqoxide `src/eq_net/protocol.rs` `eq_race_to_code`)

Map race ids 502 (Rowboat) → `"row"` model and 72/73/114/141/404/533/550/551/
552 (and 544-546/693 if confirmed) → the ship placeholder model, replacing
the current fallthrough to `"HUM"`. Keep the existing floor-snap exemption
recommendation from `boats-and-vehicles.md` §5(a) — it applies identically
regardless of which mesh renders.
