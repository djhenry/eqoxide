# Hair + Ears Converter Report (rof2-client task)

**Commit:** a5ae849 (eqoxide_asset_server main)
**Date:** 2026-06-27

---

## Group → Region Mapping

ELF_DMSPRITEDEF has 25 polygon groups. Head groups (17-24) use materials
`ELFHE000{N}_MDF` where N=1..8.

| Group | Material      | N | Region       | Treatment   |
|-------|--------------|---|--------------|-------------|
| 17    | ELFHE0008_MDF | 8 | Neck/scalp   | FIXED        |
| 18    | ELFHE0001_MDF | 1 | Main face+scalp | SWAPPABLE  |
| 19    | ELFHE0004_MDF | 4 | Adjacent face | SWAPPABLE  |
| 20    | ELFHE0005_MDF | 5 | Small adjacent | SWAPPABLE |
| 21    | ELFHE0002_MDF | 2 | EAR TIPS     | FIXED        |
| 22    | ELFHE0003_MDF | 3 | Head feature  | FIXED       |
| 23    | ELFHE0007_MDF | 7 | Forehead      | FIXED       |
| 24    | ELFHE0006_MDF | 6 | EAR BASE/JAW  | FIXED       |

Fixed = always visible, no eq_hairstyle extras.
Swappable = 8 variants H=0..7 per group.

---

## Texture Filenames per Group/Hairstyle

For swappable regions (N ∈ {1,4,5}):
- H=0: `elfhesk01.dds`, `elfhesk04.dds`, `elfhesk05.dds`
- H=1: `elfhesk11.dds`, `elfhesk14.dds`, `elfhesk15.dds`
- H=2: `elfhesk21.dds`, `elfhesk24.dds`, `elfhesk25.dds`
- ... through H=7.

For fixed regions (N ∈ {2,3,6,7,8}):
- `elfhesk02.dds` (ear tips), `elfhesk03.dds` (features),
  `elfhesk06.dds` (ear base), `elfhesk07.dds` (forehead),
  `elfhesk08.dds` (neck)

---

## DDS Existence: Elf Female

All textures for H=0..7 on N=1,4,5 were present in the archive (8/8 emitted
per swappable region). All 5 fixed region textures also found. No missing DDS
files for elf female.

Other races similarly emitted 8/8 hairstyle variants for each swappable region
(confirmed in build output for ogm, ikf, kem, kef, etc.).

---

## Build / Test / Regen Output

- `cargo build --release`: clean (0 errors, 0 warnings affecting new code)
- `cargo test`: all non-ignored tests pass; unit test `head_region_detects_he000n_pattern` green
- Ignored integration tests `elf_glb_has_ears_and_hairstyle_variants` (tests/convert.rs)
  and `elf_glb_has_ears_and_hairstyle_extras` (src/convert/mod.rs): both OK (98s)
- `cargo run --release -- build --raw ~/eq_assets/EQ_Files --out $VOL --no-zones -j 4`:
  completed successfully, built gamedata set version 13 (585 files)

---

## Live GLB Verification (race_elf.glb)

Verified with pygltflib / struct parse of live volume GLB:

| Check | Result |
|-------|--------|
| H=0 primitives | 3 (one per N=1,4,5) |
| H=1..7 primitives each | 3 (one per N=1,4,5) |
| elfhesk02 (ear tips) material | present, mat_idx=43, 1 prim, has_tex=True, no eq_hairstyle |
| elfhesk06 (ear base) material | present, mat_idx=46, 1 prim, has_tex=True, no eq_hairstyle |
| H=0 N=1 texture | elfhesk01 (bald base) |
| H=1 N=1 texture | elfhesk11 (hairstyle 1) |
| Textures distinct H=0 vs H=1 | YES |
| Total primitives | 48 |
| Total materials | 48 |

**Ears are present. Hairstyle variants H=0..7 are tagged. Textures are distinct.**

---

## What Changed (src/convert/mod.rs)

Removed:
- `face_variant_from_material_name` (wrongly treating head-region groups as face variants)
- `face_variant_from_texture` (texture fallback for same wrong logic)
- Hair synthesis block (reused only group-18 indices with wrong `eq_head_part:"hair"` schema)
- `face1_indices` caching

Added:
- `head_region_from_material_name(name) -> Option<u8>` — same regex, correct semantics
- `load_or_cache_texture(pfs, name, alpha_mode, textures, map) -> Option<usize>` — texture loader with dedup cache
- In the primitive loop: match on `head_region_from_material_name`:
  - N ∈ {1,4,5}: emit 8 primitives per group, one per H=0..7
  - N ∈ {2,3,6,7,8}: emit 1 primitive with fixed base texture
  - else: WLD material as before (body/eye groups)

Updated tests:
- `tests/convert.rs::elf_glb_has_ears_and_hairstyle_variants` (replaces old face/hair test)
- `src/convert/mod.rs::convert::tests::elf_glb_has_ears_and_hairstyle_extras` (inline unit)
- `src/convert/mod.rs::convert::tests::head_region_detects_he000n_pattern` (renamed from face_variant)
