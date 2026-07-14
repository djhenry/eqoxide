# EQG Character Models (RoF2)

**Status: confirmed from file enumeration, eqclient.ini, and client behavior.**

## DECISIVE: Which player races use EQG models?

**None.** All playable race character models in RoF2 are in S3D/WLD format (Luclin-era
archives). No per-race EQG archives (`hum.eqg`, `huf.eqg`, `elf.eqg`, etc.) exist in
the RoF2 client's data files. EQG archives with 3-letter race codes (`aam.eqg`,
`ahm.eqg`, etc.) are NPC/creature models, not player character models.

This means **EQG support is not the correct path to RoF2 player character hair.**

---

## Player Character Model Files (RoF2 Luclin path)

### eqclient.ini confirms Luclin enabled for all races

`eqclient.ini`:
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

### Client model-load path

- The client reads `UseLuclin{Race}Male/Female` keys from `eqclient.ini`
  (gated by an `AllLuclinPcModelsOff` check; race 0x82 / Vah Shir is handled
  separately) to decide whether to load the Luclin model set for a race.
- Loading builds two archive names from the 3-letter race model code:
  `global{code}_chr2` (geometry/skeleton WLD) and `global{code}_chr`
  (textures + stub WLD). The race-name string (e.g. "Human", "WoodElf") used
  for the UseLuclin INI key lookup is a separate mapping from the 3-letter
  model code; the codes themselves are given by the archive names in the
  table below.

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

## Head Polygon Groups — SUPERSEDED (see `luclin-head-faces-and-hair.md`)

**2026-07-01 correction: every claim below that `hesk{N}{L}` is indexed by
HAIRSTYLE is WRONG — the digit is the FACE index (the client's face-change
attribute drives the `%sHE%02d%d1_MDF` swap). Regions 4/5 are the NOSE
(bone-weight verified), the "ear tips" group is the crown strip, and hairstyle
is a dead actor-attach path for S3D races in RoF2 (no `*_HEAD_HAIR` actor ships).
See `luclin-head-faces-and-hair.md` for the validated model. The PFS/EQG
container-format sections later in this file remain valid.**

### Confirmed mesh fragment counts (Wood Elf Female example)

`ELF_DMSPRITEDEF` (fragment [110] in globalelf_chr.wld, size=31032 bytes):
- 1285 vertices, 1688 polygons, 25 polygon texture groups
- MaterialList: `ELF_MP` (27 material slots, 8 head + 19 body/eye slots)

HierarchicalSpriteDef `ELF_HS_DEF` (fragment [333]):
- **Only 3 DmSprite meshes**: ELFEYE_R_DMSPRITEDEF, ELF_DMSPRITEDEF, ELFEYE_L_DMSPRITEDEF
- 109 DAGs. ELFHEHAIR1-9_DAGs are attachment-point BONES ONLY (no geometry).
- No separate hair mesh exists anywhere in the WLD.

### ELF_DMSPRITEDEF Polygon Group → Material Map (head groups 17-24)

The 25 polygon groups use material palette slots 1-25 (0 and 26 are for the eye meshes).
Head groups 17-24 map to slots 18-25:

| Group | Polys | Pal Slot | Material         | Default Texture  | Hairstyle Swapped? | Region Notes |
|-------|-------|----------|------------------|------------------|--------------------|--------------|
| 17    | 40    | 18       | ELFHE0008_MDF    | elfhesk08.dds    | NO                 | Neck/scalp back; centroid X=0.088 |
| 18    | 238   | 19       | ELFHE0001_MDF    | elfhesk01.dds    | YES → layer 1      | Main face+scalp; centroid X=0.202 |
| 19    | 20    | 20       | ELFHE0004_MDF    | elfhesk04.dds    | YES → layer 4      | Adjacent face region; centroid X=0.020 |
| 20    | 10    | 21       | ELFHE0005_MDF    | elfhesk05.dds    | YES → layer 5      | Small face region; centroid X=-0.003 |
| 21    | 10    | 22       | ELFHE0002_MDF    | elfhesk02.dds    | NO                 | **EAR TIPS**: X range [0.52, 0.75], far outlier |
| 22    | 10    | 23       | ELFHE0003_MDF    | elfhesk03.dds    | NO                 | Small head feature; centroid Z=-0.093 |
| 23    | 44    | 24       | ELFHE0007_MDF    | elfhesk07.dds    | NO                 | Forehead/scalp upper; centroid X=0.042 |
| 24    | 30    | 25       | ELFHE0006_MDF    | elfhesk06.dds    | NO                 | Ear base/jaw area; Z range ±0.302 |

Full body polygon groups (0-16) use ELFCH, ELFLG, ELFFT, ELFFA, ELFUA, ELFHN materials
for chest/legs/feet/forearm/upper-arm/hand respectively.

**Critical**: polygon group 21 (ELFHE0002, 10 polys) is identified as the EAR TIPS by its
geometry: X range [0.52, 0.75] places it far from the main head cluster (all other
head groups have X centroid ≤ 0.35). A naive converter that only processes the first
or largest ELFHE group emits 238 polys × 3 = 714 indices (group 18 only) and silently
drops groups 17, 19-24, losing the ears.

### ELFHESK Texture Naming: NOT Hair Variants — Head Region Skin Textures

`elfhesk0N.dds` (N=1-8) are the **base skin textures for the 8 head polygon regions**,
NOT 8 hair-style variants. Evidence from DDS file sizes in globalelf_chr.s3d:
- elfhesk01.dds = 16512 bytes (largest → main face+scalp region, group 18)
- elfhesk06.dds = 2176 bytes (medium → ear base/jaw, group 24)
- elfhesk02/04/07.dds = 1152 bytes (medium-small regions)
- elfhesk03/05/08.dds = 640 bytes (smallest regions)

The `elfhe0001-0008.dds` files (all 192 bytes = empty DDS placeholder) are the
**equipment overlay textures** for head armor. They are not skin textures. When no
helm is worn they contribute nothing visible.

### Hairstyle Textures: ELFHESK{H}{L}.DDS

For hairstyle H (1-7), three texture files exist:
- `elfhesk{H}1.dds` = 16512 bytes → replaces material ELFHE0001 (group 18, main face+scalp)
- `elfhesk{H}4.dds` = 1152 bytes  → replaces material ELFHE0004 (group 19, adjacent)
- `elfhesk{H}5.dds` = 640 bytes   → replaces material ELFHE0005 (group 20, small adjacent)

The digit suffix (1, 4, 5) matches the material region NUMBER in the ELFHE00XX naming.
Only regions 1, 4, and 5 receive hairstyle texture swaps. Regions 2, 3, 6, 7, 8 (ear
tips, ear base, neck, etc.) ALWAYS use their fixed base skin textures regardless of hair.

Confirmed texture/size pairs:
```
elfhesk11.dds = 16512  elfhesk14.dds = 1152  elfhesk15.dds = 640
elfhesk21.dds = 16512  elfhesk24.dds = 1152  elfhesk25.dds = 640
...
elfhesk71.dds = 16512  elfhesk74.dds = 1152  elfhesk75.dds = 640
```

### Material Naming Convention: ELFHE0{F}{H}{L}_MDF

218 ELFHE material fragments exist in globalelf_chr.wld:
- 8 base: `ELFHE000{L}_MDF` for L=1..8 (head region skin, no hairstyle, face 0)
- 210 variants: `ELFHE0{F}{H}{L}_MDF` for F=0..9, H=1..7, L=1/4/5

Where:
- **F** = face variant (0-9; 0-7 are in-game options for elf female)
- **H** = hairstyle (1-7)
- **L** = region-layer matching material region number (1, 4, or 5)

**KEY FINDING**: ALL face variants F for the same H+L combination reference the SAME
`ELFHESK{H}{L}.DDS` texture. The face digit does not change the DDS file. Face variant
is NOT distinguished by texture; it is controlled by facial bone positions (ELFFA* DAGs).

### Face Selection Mechanism

`Spawn_Struct.face` (uint8, 0-7 for elf female) selects which facial bone positions
are active on the ELFFA* DAG family:
- ELFFAEYEL_DAG, ELFFAEYER_DAG (eye position/shape)
- ELFFAEYELIDLTOP/BOT, ELFFAEYELIDRTOP/BOT (eyelid shape)
- ELFFAEYEBROWR/L_DAG (eyebrow shape)
- ELFFANOSE_DAG (nose)
- ELFFAJAW_DAG → ELFFALIPBOTTOM_DAG (jaw/lip)

For the material system: the engine selects material set ELFHE0{F}{H}{L}_MDF based on
spawn's face=F and hairstyle=H. At F=0,H=0 (default), the base palette ELF_MP is used
directly with ELFHE0001-0008. At F=0,H=1 (hairstyle 1, default face): slots 1, 4, 5
in ELF_MP are replaced by ELFHE0011/0014/0015_MDF referencing elfhesk11/14/15.dds.

Face variant F for the same hairstyle substitutes ELFHE0{F}{H}{L} but these all point
to the same elfhesk{H}{L}.dds. **No face-variant textures exist in globalelf_chr.s3d.**

### Spawn_Struct Wire Fields (rof2_structs.h)

From `EQEmu/common/patches/rof2_structs.h` (Spawn_Struct, variable offsets):
```c
uint8  face;          // 0-7 → facial bone set; does NOT change head texture
uint8  haircolor;     // color baked into hesk textures (not a separate index)
uint8  hairstyle;     // 0=no hair (elfhesk01/04/05), 1-7=elfhesk{H}{L}
uint8  beard;         // males only; beard bone track (no separate beard mesh)
uint8  beardcolor;    // baked into beard texture
```

### Body Region Skin Texture Pattern (all body parts)

| Region | Code | Skin textures (N=number of poly groups) | Equipment layers |
|--------|------|----------------------------------------|------------------|
| Chest  | CH   | elfchsk01/02/03 (3 groups)             | elfch0001-0003.dds (and armor sets 01xx, 02xx) |
| Legs   | LG   | elflgsk01/02/03 (3 groups)             | elflg0001-0003.dds |
| Feet   | FT   | elfftsk01/02/03/04 (4 groups)          | elfft0001-0004.dds |
| Forearm| FA   | elffask01/02 (2 groups)                | elffa0001-0002.dds |
| Uparm  | UA   | elfuask01/02 (2 groups)                | elfua0001-0002.dds |
| Hand   | HN   | elfhnsk01/02/03 (3 groups)             | elfhn0001-0003.dds |
| Head   | HE   | elfhesk01-08 (8 groups)                | elfhe0001-0008.dds (all 192 bytes = empty/no helm) |

Pattern: `{race}{region}SK{N}.DDS` = base skin per polygon group; `{race}{region}0{N}.DDS` = equipment overlay.
The equipment overlays for set 0 (worn item = none) are 192-byte placeholders. Armor
textures occupy sets 01/02/03 etc. (`elfch0101/0102/0103` for armor set 01, etc.).

---

## Converter Recommendation (eqoxide)

### For a correct head export

1. **Include ALL 25 polygon groups** from ELF_DMSPRITEDEF (not just the largest ELFHE group).
   Export all 8 head groups (17-24) as separate submeshes tagged by material name.

2. **For hairstyle selection**, emit material variants in the glTF extras:
   - hairstyle=0: groups 18→elfhesk01, 19→elfhesk04, 20→elfhesk05 (bald/default)
   - hairstyle=H (1-7): groups 18→elfheskH1, 19→elfheskH4, 20→elfheskH5

3. **Ear and other static regions** (groups 17, 21-24): always use fixed base skin.
   Group 21 (ears) uses elfhesk02.dds always — no hairstyle swap.

4. **Do NOT attempt face texture swapping** — no face-variant DDS files exist in the
   archive. Face appearance is purely skeletal (ELFFA* bone positions).

5. **Equipment overlay textures** (elfhe0001-0008.dds, all 192 bytes) represent the
   "no helm" state. When the player wears a helm, the appropriate elfhe{armorset}0N.dds
   is swapped in. The converter should export the empty placeholder as-is for the
   default state.

### WLD fragment chain (texture resolution)

```
0x30 MaterialDef → reference (i32 at field_data[24]) → 0x05 SimpleSprite
0x05 SimpleSprite → reference (i32 at field_data[4]) → 0x04 SimpleSpriteDef
0x04 SimpleSpriteDef → frame_count + frame_refs → 0x03 BmInfo
0x03 BmInfo → entry_count (N-1) + (N × EncodedFilename{u16 len, XOR bytes})
```

XOR key for both string table and BmInfo filenames: `[0x95,0x3a,0xc5,0x2a,0x95,0x7a,0x95,0x6a]`

0x30 MaterialDef field_data layout:
```
[0]  i32  name_ref
[4]  u32  flags (bit 0=two_sided, bit 1=has_pair)
[8]  u32  render_method  (0x80000001 = normal)
[12] u32  rgb_pen
[16] f32  brightness
[20] f32  scaled_ambient
[24] i32  reference → 0x05 SimpleSprite  ← CRITICAL: NOT at [28], NOT after 3 floats
[28] (u32,f32) pair — only if flags bit 1 set
```

### WLD fragment header format

```
[0..3]  u32  size          (byte count of field_data)
[4..7]  u32  fragment_type
[8..]   field_data         (first 4 bytes = i32 name_ref)
```
Total bytes per fragment: `8 + size`. Fragment references are 1-based (ref=N → frags[N-1]).

---

## Hair DAG Attachment Points (NOT geometry)

`ELFHEHAIR{1-9}_DAG` in the HierSprDef skeleton are attachment-point bones for
particle effects or equipment pieces (hair extensions, hats with built-in hair, etc.).
They carry NO mesh geometry. The WLD assigns all DAGs track_reference values but
mesh_or_sprite_reference = 0 for all ELFHE* head/hair DAGs.

`ELFHAIR_POINT_DAG` and `ELFHEAD_POINT_DAG` are the canonical hat/hair equipment
attachment points referenced by the client's bone-name string table.

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

### EQGS v1 format (older)

Magic `EQGS` (0x53475145), version field == 1 at offset 4.
Header at offset 8 = string table length; string table at offset 0x18 (24).
Fields at 0x0c, 0x10, 0x14 control material count, geometry groups, and
bone/palette count. Not found in any RoF2 EQG files examined; may be used by
older zone-chr S3D archives or early PoP-era EQG files.

### Other EQG magic bytes (confirmed)

| Magic  | Type  | Notes                  |
|--------|-------|------------------------|
| `EQGM` | 2     | Static mesh (OpenEQ TerMod.cs isTer=false) |
| `EQGT` | 2     | Terrain (OpenEQ TerMod.cs isTer=true) |
| `EQAL` | 5     | Material layer list    |
| `EQGL` | 6     | Geometry list (light?) |
| `EQGZ` | 3     | Zone data              |
| `EQLOD`| 7     | Level-of-detail        |
| `EQTZP`| 8     | Terrain zone point     |
| `EQOBG`| 9     | Object group           |

---

## Recommendation for eqoxide

**EQG parsing is not the right path for RoF2 player character hair.**

The correct implementation path:

### (a) Converter (`eqoxide_asset_server/src/convert/mod.rs`)

1. **Load `global{code}_chr.s3d`** — this single archive contains BOTH the DDS textures
   AND the WLD with mesh geometry (0x36 frags), skeleton (0x10), and material
   definitions.

   PFS reading: use CRC-based lookup (compute `FilenameCrc` per filename, match to
   directory entry CRC). Order-based matching is wrong.

2. **Extract exactly 3 mesh fragments per character** (body + eye_R + eye_L).

3. **For the body mesh (ELF_DMSPRITEDEF etc.), emit ALL polygon groups** — do not
   filter to just the first or largest ELFHE group. The head has 8 distinct polygon
   groups (groups 17-24) that must ALL be included to get ears, neck, jaw, etc.

4. **For hairstyle, replace textures on groups 18, 19, 20** (regions 1, 4, 5):
   - hairstyle=0: `elfhesk01`, `elfhesk04`, `elfhesk05`
   - hairstyle=H (1-7): `elfheskH1`, `elfheskH4`, `elfheskH5`
   Tag each in glTF extras/variants as `hair_style_{H}`.

5. **For face, no texture swap** — emit only the default material set (ELFHE000N).
   Face appearance is purely skeletal; all face variants point to the same DDS files.

6. **Do NOT emit separate hair geometry** — ELFHEHAIR DAGs carry no mesh, they are
   attachment points only.

### (b) Client (spawn appearance → render-time selection)

From `Spawn_Struct` (EQEmu/common/patches/rof2_structs.h):
- `hairstyle` (uint8, 0-indexed): 0 = base/bald, 1-7 = styled; selects which
  elfhesk{H}{L} set replaces the default head texture on groups 18/19/20
- `face` (uint8, 0-7): facial bone position set; no texture change
- `haircolor` (uint8): baked into hesk textures, no separate index needed

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
(armor texture swapping on the same Luclin body mesh)
