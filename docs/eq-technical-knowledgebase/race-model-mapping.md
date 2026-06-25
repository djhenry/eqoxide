# Race → Model Mapping (Titanium Client)

## Ground Truth: per-race distinct models

The Titanium client ships a **distinct skeletal model archive for every playable race and
gender**. There is NO model sharing between races in the real client. The registration
is confirmed by direct binary extraction from `eqgame.exe` (`/home/dhenry/git/NostalgiaEQ-Client/eqgame.exe`)
reading the string table at the addresses populated by `FUN_0048c83e` (the actordef
registration function, `eqgame.exe.c:113895`).

## Actordef Registration: How the Client Picks a Model

The function `FUN_0048c8af` (`eqgame.exe.c:113944`) registers a lookup table mapping
`(race_id, gender)` → 3-letter model code string. This is queried by `FUN_0048c879`
(`eqgame.exe.c:113916`) which calls `FUN_0048c7e7` (`eqgame.exe.c:113864`) to return
the registered name. When no entry matches, the fallback is `"HUM"` (male) or `"HUF"`
(female) — confirmed at `eqgame.exe.c:113875-113884`.

The zone loading function (the function containing the block at `eqgame.exe.c:57266-57302`)
loops over race IDs **1–12, 128, 130** in sequence (note the jump at `eqgame.exe.c:57268-57270`
that skips from race 12 to 128, then 130, then exits the loop at 131). For each race+gender:
1. Gets 3-letter code via `FUN_0048c879`.
2. Loads `global<CODE>_chr2` (secondary model variant, optional).
3. Loads `global<CODE>_chr` (primary Luclin model archive).

The archive name is case-insensitive on Windows. The fallback (non-Luclin) archive is
`global_chr.s3d` which contains BMP-format textures for all races.

## Complete Race → Actordef Code Mapping (Confirmed from Binary)

Extracted by reading string data at registered VA addresses in eqgame.exe .rdata section
(image base 0x00400000, .rdata at 0x0063f000):

| Race         | Race ID | Gender | Code | Archive (`global<code>_chr.s3d`) |
|--------------|---------|--------|------|----------------------------------|
| Human        | 1       | Male   | HUM  | globalhum_chr.s3d                |
| Human        | 1       | Female | HUF  | globalhuf_chr.s3d                |
| Barbarian    | 2       | Male   | BAM  | globalbam_chr.s3d                |
| Barbarian    | 2       | Female | BAF  | globalbaf_chr.s3d                |
| Erudite      | 3       | Male   | ERM  | globalerm_chr.s3d                |
| Erudite      | 3       | Female | ERF  | globalerf_chr.s3d                |
| Wood Elf     | 4       | Male   | ELM  | globalelm_chr.s3d                |
| Wood Elf     | 4       | Female | ELF  | globalelf_chr.s3d                |
| High Elf     | 5       | Male   | HIM  | globalhim_chr.s3d                |
| High Elf     | 5       | Female | HIF  | globalhif_chr.s3d                |
| Dark Elf     | 6       | Male   | DAM  | globaldam_chr.s3d                |
| Dark Elf     | 6       | Female | DAF  | globaldaf_chr.s3d                |
| Half Elf     | 7       | Male   | HAM  | globalham_chr.s3d                |
| Half Elf     | 7       | Female | HAF  | globalhaf_chr.s3d                |
| Dwarf        | 8       | Male   | DWM  | globaldwm_chr.s3d                |
| Dwarf        | 8       | Female | DWF  | globaldwf_chr.s3d                |
| Troll        | 9       | Male   | TRM  | globaltrm_chr.s3d                |
| Troll        | 9       | Female | TRF  | globaltrf_chr.s3d                |
| Ogre         | 10      | Male   | OGM  | globalogm_chr.s3d                |
| Ogre         | 10      | Female | OGF  | globalogf_chr.s3d                |
| Halfling     | 11      | Male   | HOM  | globalhom_chr.s3d                |
| Halfling     | 11      | Female | HOF  | globalhof_chr.s3d                |
| Gnome        | 12      | Male   | GNM  | globalgnm_chr.s3d                |
| Gnome        | 12      | Female | GNF  | globalgnf_chr.s3d                |
| Iksar        | 128     | Male   | IKM  | globalikm_chr.s3d                |
| Iksar        | 128     | Female | IKF  | globalikf_chr.s3d                |
| Vah Shir     | 130     | Male   | KEM  | globalkem_chr.s3d                |
| Vah Shir     | 130     | Female | KEF  | globalkef_chr.s3d / Global7_chr  |
| Froglok      | 330     | Male   | FRM  | globalpcfroglok_chr.s3d *        |
| Froglok      | 330     | Female | FRF  | globalpcfroglok_chr.s3d *        |

\* Froglok (race_id 330 = 0x14a) is NOT covered by the main loading loop (which covers
only races 1–12 and 128–130). The archive is `globalpcfroglok_chr.s3d` which contains
BOTH male (FRM prefix) and female (FRF prefix) textures and models in a single file.
It is loaded via zone-level character loading. Fallback: if no froglok model is loaded,
the client falls back to HUM/HUF ACTORDEF.

### Special cases

- **Ogre Female (OGF)**: uses `globalogf_chr2.s3d` for secondary variants.
- **Vah Shir Female (KEF)**: also in `Global7_chr.s3d` (old combined pack loaded at
  `eqgame.exe.c:57357-57359` when VahShir Luclin model is enabled).
- **VEquip**: when VahShir Luclin model is loaded, `VEquip` armor archive is also
  loaded (`eqgame.exe.c:57288-57290`).
- **Iksar Female (IKF)**: has its own archive `globalikf_chr.s3d` (confirmed present in
  `/home/dhenry/eq_assets/EQ_Files/`).

### Fallback behavior

If a race's Luclin model fails to load (archive missing or `AllLuclinPcModelsOff=TRUE`),
the client uses the classic `global_chr.s3d` (BMP textures, `eqgame.exe.c:56330/56338`),
with ACTORDEF `HUF_ACTORDEF` for female and `HUM_ACTORDEF` for male.

## All Archives Confirmed Present

All 30 character archives (15 races × 2 genders) are confirmed present in
`/home/dhenry/eq_assets/EQ_Files/`.

## What This Means for eqoxide

### Current (wrong) mapping in `src/models.rs`

`race_to_archetype()` (`models.rs:621`) collapses:
- Barbarian, Troll, Ogre, Erudite, Iksar, Vah Shir, Halfling, Gnome → "humanoid" (human model)
- Wood/High/Dark/Half Elf → "elf" (wood elf model)

This is wrong. In the real Titanium client, every race has its own distinct skeleton and mesh.

### Correct race → archive mapping for eqoxide

Each entry should map `(race_id, gender)` to its own `s3d_to_gltf`-converted `.glb` file:

```
race_id  gender  code  archive
1        M       HUM   globalhum_chr.s3d   → humanoid_m.glb
1        F       HUF   globalhuf_chr.s3d   → humanoid_f.glb
2        M       BAM   globalbam_chr.s3d   → barbarian_m.glb
2        F       BAF   globalbaf_chr.s3d   → barbarian_f.glb
3        M       ERM   globalerm_chr.s3d   → erudite_m.glb
3        F       ERF   globalerf_chr.s3d   → erudite_f.glb
4        M       ELM   globalelm_chr.s3d   → woodelf_m.glb
4        F       ELF   globalelf_chr.s3d   → woodelf_f.glb
5        M       HIM   globalhim_chr.s3d   → highelf_m.glb
5        F       HIF   globalhif_chr.s3d   → highelf_f.glb
6        M       DAM   globaldam_chr.s3d   → darkelf_m.glb
6        F       DAF   globaldaf_chr.s3d   → darkelf_f.glb
7        M       HAM   globalham_chr.s3d   → halfelf_m.glb
7        F       HAF   globalhaf_chr.s3d   → halfelf_f.glb
8        M       DWM   globaldwm_chr.s3d   → dwarf_m.glb
8        F       DWF   globaldwf_chr.s3d   → dwarf_f.glb
9        M       TRM   globaltrm_chr.s3d   → troll_m.glb
9        F       TRF   globaltrf_chr.s3d   → troll_f.glb
10       M       OGM   globalogm_chr.s3d   → ogre_m.glb
10       F       OGF   globalogf_chr.s3d   → ogre_f.glb
11       M       HOM   globalhom_chr.s3d   → halfling_m.glb  (NOT "human" — this is Halfling!)
11       F       HOF   globalhof_chr.s3d   → halfling_f.glb
12       M       GNM   globalgnm_chr.s3d   → gnome_m.glb
12       F       GNF   globalgnf_chr.s3d   → gnome_f.glb
128      M       IKM   globalikm_chr.s3d   → iksar_m.glb
128      F       IKF   globalikf_chr.s3d   → iksar_f.glb
130      M       KEM   globalkem_chr.s3d   → vahshir_m.glb
130      F       KEF   globalkef_chr.s3d   → vahshir_f.glb
330      M/F     FRM/F globalpcfroglok_chr.s3d → froglok_m.glb / froglok_f.glb
```

### Priority order for conversion

Priority = visual impact of wrong model (most different from human):
1. **Troll** (TRM/TRF) — enormous, hunched, completely different silhouette
2. **Ogre** (OGM/OGF) — very large
3. **Iksar** (IKM/IKF) — reptilian skeleton, unique rig
4. **Vah Shir** (KEM/KEF) — feline, large
5. **Gnome** (GNM/GNF) — very small, different proportions
6. **Halfling** (HOM/HOF) — small (currently mapped to "humanoid" which accidentally WAS halfling before the bug fix, now maps to human)
7. **Barbarian** (BAM/BAF) — larger than human, different musculature
8. **Erudite** (ERM/ERF) — tall, thin; humanoid-ish but distinct head/proportions
9. **High Elf** (HIM/HIF) — different from Wood Elf (currently both use globalelf)
10. **Dark Elf** (DAM/DAF) — different from Wood Elf
11. **Half Elf** (HAM/HAF) — between human/elf
12. **Dwarf** (DWM/DWF) — already has its own archetype
13. **Froglok** (FRM/FRF) — already has its own archetype
14. **Wood Elf** (ELM/ELF) — already has its own archetype

## _chr2 Archives

Each playable race also has a `global<code>_chr2.s3d` secondary archive. These contain
additional WLD model data (likely alternate armor meshes). The client loads `_chr2` first,
then `_chr`. For eqoxide's s3d_to_gltf pipeline, `_chr2` may not be needed for
the base skeleton/pose — investigate if conversion is incomplete without it.

## global_chr.s3d

This combined archive (`/home/dhenry/eq_assets/EQ_Files/global_chr.s3d`) contains
BMP-format (non-Luclin) textures for ALL races. It is the pre-Luclin fallback. The
.wld inside is a single combined file. Do not use this as a primary conversion source;
use the per-race `global<code>_chr.s3d` archives with DDS textures instead.
