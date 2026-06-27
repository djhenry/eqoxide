# EQG Character Models (RoF2)

**Status: confirmed from file enumeration, eqclient.ini, and eqgame.exe.c decompile.**

## DECISIVE: Which player races use EQG models?

**None.** All playable race character models in RoF2 are in S3D/WLD format (Luclin-era
archives). No per-race EQG archives (`hum.eqg`, `huf.eqg`, `elf.eqg`, etc.) exist in
`~/eq_assets/everquest_rof2/`. EQG archives with 3-letter race codes (`aam.eqg`,
`ahm.eqg`, etc.) are NPC/creature models, not player character models.

This means **EQG support is not the correct path to RoF2 player character hair.**

---

## Player Character Model Files (RoF2 Luclin path)

### eqclient.ini confirms Luclin enabled for all races

`~/eq_assets/everquest_rof2/eqclient.ini`:
```
UseLuclinHumanMale=TRUE
UseLuclinHumanFemale=TRUE
UseLuclinWoodElfMale=TRUE
UseLuclinWoodElfFemale=TRUE
UseLuclinDarkElfMale=TRUE
... (all races TRUE)
```
RoF2 always loads Luclin models; the classic `global_chr.s3d` is a fallback that the
client retains but never reaches under defaults.

### Client model-load path (eqgame.exe.c)

- `FUN_0048e420` (eqgame.exe.c:100868) — reads `UseLuclin{Race}Male/Female` from
  eqclient.ini
- `FUN_0048e510` (eqgame.exe.c:100902) — wrapper that also checks
  `AllLuclinPcModelsOff`; race 0x82 (Vah Shir) handled separately
- Loading code at eqgame.exe.c:103363,103382:
  ```c
  _sprintf(acStack_cbc, "global%s_chr2", auStack_89c);  // geometry/skel WLD
  _sprintf((char *)&uStack_cc4, "global%s_chr");         // textures + stub WLD
  ```
  Where `auStack_89c` is the 3-letter race model code built from `FUN_00488c00`.
- `FUN_00488c00` (eqgame.exe.c:97394) — returns race name string ("Human", "WoodElf",
  etc.) used for UseLuclin INI key lookup. The 3-letter model codes come from a
  separate lookup (not shown separately in the decompile — they follow from the
  archive names below).

### Per-race archive pairs

| Race | Sex | Texture+stub WLD | Geometry/skeleton WLD |
|------|-----|------------------|-----------------------|
| Human | M | `globalhum_chr.s3d` | `globalhum_chr2.s3d` (138,880 bytes) |
| Human | F | `globalhuf_chr.s3d` | `globalhuf_chr2.s3d` (347,736 bytes) |
| Wood Elf | M | `globalelm_chr.s3d` | `globalelm_chr2.s3d` (757,952 bytes) |
| Wood Elf | F | `globalelf_chr.s3d` | `globalelf_chr2.s3d` (213,556 bytes) |
| Drakkin | - | `globaldrk_chr.s3d` | (no chr2 found; may use `drk_chr.s3d` for textures) |
| Froglok (PC) | - | `globalpcfroglok_chr.s3d` | also `globalkem_chr.s3d` / `globalkef_chr.s3d` |

Pattern: `global{code}_chr.s3d` holds all DDS textures plus a tiny stub WLD (typically
~2-16 KB) that just references the geometry. `global{code}_chr2.s3d` holds only
a single large WLD (no textures) with the actual mesh, skeleton, and animation data.

Verified by listing archive contents (PFS reader). Drakkin is an anomaly: its
textures appear in the zone-prefixed `drk_chr.s3d` (53 DDS files) while
`globaldrk_chr.s3d` holds only weapon-item textures + WLD — load path for Drakkin
body textures not yet fully traced.

---

## Hair: Texture Variants, Not Geometry — CONFIRMED DEFINITIVE (2026-06)

**Theory A is CORRECT. Theory B (separate HEHAIR geometry meshes) is WRONG.**

Evidence from direct WLD fragment scan of RoF2 archives (scripts in
`/tmp/claude.../scratchpad/`; PFS CRC-based reader with correct 8-byte XOR key
`[0x95, 0x3a, 0xc5, 0x2a, 0x95, 0x7a, 0x95, 0x6a]` per
`libeq_wld/src/parser/strings.rs:32`):

### Confirmed mesh fragment counts per race/sex

| Archive                  | Mesh frags (type 0x36)                                    |
|--------------------------|-----------------------------------------------------------|
| `globalelf_chr.wld`      | 3: `ELFEYE_R_DMSPRITEDEF`, `ELF_DMSPRITEDEF`, `ELFEYE_L_DMSPRITEDEF` |
| `globalelm_chr.wld`      | 3: `ELMEYE_R_DMSPRITEDEF`, `ELM_DMSPRITEDEF`, `ELMEYE_L_DMSPRITEDEF` |
| `globalhum_chr.wld`      | 3: `HUMEYE_R_DMSPRITEDEF`, `HUM_DMSPRITEDEF`, `HUMEYE_L_DMSPRITEDEF` |
| `globalhuf_chr.wld`      | 3: `HUFEYE_R_DMSPRITEDEF`, `HUF_DMSPRITEDEF`, `HUFEYE_L_DMSPRITEDEF` |

NO `HEHAIR*_DMSPRITEDEF` or `HEBEARD*_DMSPRITEDEF` mesh fragments exist in any
of the four archives examined.

### What the HEHAIR/HEBEARD fragment names actually are

`ELFHEHAIR1_TRACKDEF` through `ELFHEHAIR9_TRACKDEF` are **TrackDef (type 0x12)** +
**Track (type 0x13)** pairs — skeleton bone animation definitions, NOT mesh geometry.
They are attachment-point DAGs (Directed Acyclic Graph nodes) in the
`HierarchicalSpriteDef` (type 0x10) skeleton fragment. The corresponding
`HAIR_POINT_DAG` and `BEARD_POINT_DAG` strings found in `eqgame.exe.c:8038,8043`
are bone name lookups for equipment/particle attachment, not hair mesh loading.

Similarly, `ELMHEBEARD1_TRACKDEF`, `HUMHEBEARD1_TRACKDEF` etc. are bone tracks
(TrackDef 0x12) representing beard attachment-point positions, not geometry.

### How hair IS implemented: WLD material/skin system

The single body+head mesh (`ELF_DMSPRITEDEF`, size=31032 bytes) includes the head
polygon group. The chr.wld contains:
- `BitmapName` (0x03) fragments for `ELFHESK01`–`ELFHESK08` (base head skin set)
  and `ELFHESK11`, `ELFHESK14`, `ELFHESK21`, `ELFHESK24`... etc. (hair style sets)
- `Material` (0x30) fragments named `ELFHE0001_MDF`–`ELFHE0008_MDF` (face variants)
- 1 `MaterialPalette` (0x31) holding all 288 material slots

The `HierarchicalSpriteDef` (0x10) has a `link_skin_updates_to_dag_index` array
that ties specific DAG bone indices to material palette skin swap slots. The
`HEHAIR{N}` DAGs trigger skin updates selecting which `ELFHESK{N}*` material group
is active on the head polygon group.

The chr.wld is large (~4MB for elf female, 30460 total fragments) because it
contains animation track copies for each of the ~65 animation sets (C01A, C01B,
C02B, … D01A, L01, L04, P01, S04, etc.), each with its own set of HEHAIR bone
tracks. The chr2.s3d contains additional animation sets at different LOD/behavior
levels (L04B prefix etc.).

### Head texture naming convention

Pattern: `{raceCode}hesk{hairVariant}{layer}.dds`

- `{raceCode}` = lowercase 3-letter model code ("hum", "huf", "elm", "elf", ...)
- `{hairVariant}` = 1–7 (the 7 available hair styles from character creation)
- `{layer}` = 1, 4, or 5 (3 texture layers per variant — diffuse/channel-4/channel-5)

Examples from `globalelf_chr.s3d`:
```
elfhesk11.dds, elfhesk14.dds, elfhesk15.dds   <- Elf Female, hair style 1
elfhesk21.dds, elfhesk24.dds, elfhesk25.dds   <- Elf Female, hair style 2
...
elfhesk71.dds, elfhesk74.dds, elfhesk75.dds   <- Elf Female, hair style 7
```
Plus `elfhesk01.dds`–`elfhesk08.dds` = 8 base head skin variants (haircolor/default
texture, not the styled variants). Same pattern confirmed for Wood Elf Male
(`elmhesk{1-7}{1,4,5}.dds`), Human Male (`humhesk{1-7}{1,4,5}.dds`), Human Female
(`hufhesk{1-7}{1,4,5}.dds`).

`humhesk15.dds` is ~4 MB — confirms large DDS atlas (1024×1024+ with mipmaps).

### Base head/face textures

`{race}he0001.dds`–`{race}he0008.dds` are base head/face textures (8 face variants
from character creation, indexed by the `face` field in Spawn_Struct/PlayerProfile).
These are rendered on a separate polygon group from the hair layer.

### Wire struct fields (rof2_structs.h)

From `EQEmu/common/patches/rof2_structs.h`:
- `Spawn_Struct.hairstyle` (uint8, offset variable — depends on variable-size name
  prefix): 0-indexed → selects `{race}hesk{hairstyle+1}` DDS set
- `Spawn_Struct.face` (uint8): 0-indexed → selects `{race}he000{face+1}.dds`
- `Spawn_Struct.haircolor` (uint8): hair color — likely selects among hesk01–hesk08
  base set or is a tint index baked into the hesk textures themselves
- `Spawn_Struct.beard` (uint8): beard type (males only)
- `Spawn_Struct.beardcolor` (uint8): beard color

For males, beard is encoded within the `hesk` texture system (no separate beard
mesh fragments exist). Wood Elf Male has `ELMHEBEARD1-2_TRACKDEF`; Human Male has
`HUMHEBEARD1-2_TRACKDEF` + `HUMBEARD_POINT_TRACKDEF` — all TrackDef bones, no geometry.

### WLD fragment header format (IMPORTANT: corrected)

Fragment header layout (confirmed via `libeq_wld/src/parser/mod.rs:303-316`):
```
[0..3]  uint32 size          -- byte length of field_data
[4..7]  uint32 fragment_type
[8..8+size-1] field_data     -- first 4 bytes = name_ref (int32, negative index)
```
Total bytes per fragment in file: `8 + size`. Do NOT use `12 + size` (off by 4).
String table XOR key: `[0x95, 0x3a, 0xc5, 0x2a, 0x95, 0x7a, 0x95, 0x6a]` (8 bytes
repeating, NOT single-byte 0x95). name_ref: `abs(name_ref)` is the string table
offset (not `abs(name_ref)-1`).

---

## EQG Container Format (PFS)

**Confirmed** from reading `aam.eqg` (9,927,935 bytes).

```
Offset 0:  uint32 LE  = offset to directory block (near end of file)
Offset 4:  4 bytes    = "PFS " magic
Offset 8:  uint32 LE  = version (0x00020000)

Directory block at dir_offset:
  uint32 LE  = num_entries (N)
  N × {
    uint32 LE  = CRC
    uint32 LE  = data offset within archive
    uint32 LE  = inflated (decompressed) size
  }
```

CRC `0x61580AC9` = filename directory entry (always present).

Data blocks at each offset: zlib-compressed chunks:
```
repeat until inflated_size bytes read:
  uint32 LE  = compressed chunk size
  uint32 LE  = uncompressed chunk size
  bytes      = zlib-compressed data (zlib header + deflate)
```

Filename directory (inflated):
```
uint32 LE  = count (N-1, excluding the dir-CRC entry itself)
N-1 × {
  uint32 LE  = strlen (including null terminator)
  bytes      = filename (null-terminated)
}
```
Filenames correspond to non-dir directory entries **in index order** (no per-entry
CRC in the filename directory block). Confirmed by successful extraction of
`aam.mds`, `aam.lay`, `crmp_ba_1_aam.ani` from `aam.eqg`.

---

## EQG Inner File Formats (NPC character models)

### aam.eqg contents (confirmed, NPC model archive)

```
aam.mds          84,398 bytes   EQGA v2 (skeleton + mesh data)
aam.lay       1,398,228 bytes   DDS texture (layer/material atlas)
aam.prt             ?           particle definition
aam.pts             ?           particle template  
crmp_ba_1_aam.ani  151,498 bytes  EQGA v2 (animation data)
c_aam_bd_s00_c.dds              DDS body texture (skin 00, channel c)
c_aam_bd_s00_n.dds              DDS body texture (skin 00, normal)
... (many DDS files)
```

### EQGA v2 format (`.mds` and `.ani`)

Magic `EQGA` (0x41475145). Used for both skinned-model and animation in RoF2 NPC
EQG archives.

Header (first 20 bytes):
```
[0]  4 bytes: "EQGA" magic
[4]  uint32: version = 2
[8]  uint32: string table length (e.g., 686 for aam)
[12] uint32: entry count (e.g., 61 — number of bones based on string table)
[16] uint32: flag: 1 = model with geometry, 0 = animation-only
```
String table follows at offset 20; contains null-terminated bone/joint names:
`PELV\0CHEST_CHEST01\0LEGL_THIGH\0LEGL_CALF\0LEGL_FOOT\0...HEAD_HEAD\0ARML_CLAV\0...`

Full parse of bone data + vertex format not yet reversed (out of scope for PC
character support).

### EQGS v1 format (older, in EQGraphicsDX9.dll.c)

Magic `EQGS` (0x53475145), version field == 1 at offset 4.
Checked by `FUN_100631e0` (EQGraphicsDX9.dll.c:86173).
Header at offset 8 = string table length; string table at offset 0x18 (24).
Fields at 0x0c, 0x10, 0x14 control material count, geometry groups, and
bone/palette count. Not found in any RoF2 EQG files examined; may be used by
older zone-chr S3D archives or early PoP-era EQG files.

### Other EQG magic bytes (all confirmed from EQGraphicsDX9.dll.c:88169-88213)

| Magic  | Type  | Handler function    | Notes                  |
|--------|-------|---------------------|------------------------|
| `EQGM` | 2     | FUN_10063f10... no  | Static mesh (OpenEQ TerMod.cs isTer=false) |
| `EQGT` | 2     | FUN_10063f10        | Terrain (OpenEQ TerMod.cs isTer=true) |
| `EQAL` | 5     | FUN_10064c70        | Material layer list    |
| `EQGL` | 6     | FUN_100645f0        | Geometry list (light?) |
| `EQGZ` | 3     | FUN_10064da0        | Zone data              |
| `EQLOD`| 7     | FUN_10064ab0        | Level-of-detail        |
| `EQTZP`| 8     | FUN_10064bd0        | Terrain zone point     |
| `EQOBG`| 9     | FUN_10065b60        | Object group           |

---

## Recommendation for eqoxide

**EQG parsing is not the right path for RoF2 player character hair.**

The correct implementation path:

### (a) Converter (`eqoxide_asset_server/src/convert/mod.rs`)

1. **Load `global{code}_chr.s3d` only** — this single archive contains BOTH the DDS
   textures AND the WLD with mesh geometry (0x36 frags), skeleton (0x10), and material
   definitions. The chr2.s3d contains only animation tracks and is loaded separately
   for animation data.

   PFS reading: use CRC-based lookup (compute `FilenameCrc` per filename, match to
   directory entry CRC). Order-based matching is wrong and returns DDS content for
   the WLD entry. See `/tmp/.../scratchpad/scan_chr_wld.py` for working Python reference.

2. **Extract exactly 3 mesh fragments per character** (body + eye_R + eye_L). No
   separate hair or beard mesh exists.

3. **For hair, emit 7 head material variants** in the exported glTF/asset:
   - Variant 0 (hairstyle=0): `{race}hesk01.dds` (base, no styled hair)
   - Variant N (hairstyle=1..7): `{race}hesk{N}1.dds`, `{race}hesk{N}4.dds`,
     `{race}hesk{N}5.dds` as the 3 texture layers for that style
   - Tag each as `hair_style_{N}` in the manifest/extras

4. **For face, emit 8 face material variants**:
   - `{race}he000{N}.dds` for N=1..8, tagged as `face_{N-1}` (0-indexed)

5. **Do NOT attach separate hair geometry or load additional archives** for standard
   playable races. The `HEHAIR*_DAG` bones are attachment-point only; they carry no
   mesh geometry.

### (b) Client (spawn appearance → render-time selection)

From `Spawn_Struct` (EQEmu/common/patches/rof2_structs.h):
- `hairstyle` (uint8, 0-indexed) → select head material variant `hairstyle` (0 = base,
  1..7 = styled); if > 7 clamp to 7
- `face` (uint8, 0-indexed) → select face variant `face` (0..7 → he0001..he0008)
- `haircolor` (uint8) → may select among hesk01..hesk08 base set OR is baked into
  DDS; not yet fully traced — inferred only
- `beard` / `beardcolor` (uint8, males only) → similarly encoded within hesk textures;
  no separate geometry needed

### 3-letter model codes by race+sex (confirmed on disk)

| Race | M code | F code |
|------|--------|--------|
| Human | hum | huf |
| Wood Elf | elm | elf |
| High Elf | him | hif |
| Dark Elf | dam | daf |
| Half Elf | ham | haf |
| Dwarf | dwm | dwf |
| Gnome | gnm | gnf |
| Barbarian | bam | baf |
| Erudite | erm | erf |
| Halfling | hom | hof |
| Troll | trm | trf |
| Ogre | ogm | ogf |
| Iksar | ikm | ikf |
| Vah Shir | vsm? | vsf? |
| Froglok | kem | kef |
| Drakkin | drk | drk |

### EQG support

EQG parsing IS needed only for NPC models and some zone objects. For those,
parse the PFS container (same as S3D), then dispatch on inner file magic:
`EQGA` v2 for animated NPCs, `EQGT`/`EQGM` for static zone geometry.

Related topics: `spawn-struct.md` (face/hairstyle fields), `equipment-textures.md`
(armor texture swapping on the same Luclin body mesh).
