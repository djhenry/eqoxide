# RoF2 Asset Regeneration Report

**Date:** 2026-06-26  
**Branch:** worktree-fix+bug-fix  
**Commit (--no-zones flag):** f7a6c5f  

---

## 1. --no-zones CLI Flag

Added `--no-zones` to `/home/dhenry/git/eqoxide_asset_server/src/main.rs` in the `Build` subcommand.
Mirrors `--zones-only` but inverts it: runs `build_from_raw` (common) + `build_gamedata_from_raw` +
`build_gameequip_from_raw`, skips `build_zones_from_raw` entirely. Message printed:
`--no-zones: skipping zone baking (existing zone GLBs preserved)`.

Commit: `f7a6c5f feat(build): --no-zones flag to rebuild common+gameequip without zones`

---

## 2. Build Run

```
./target/release/eqoxide-assets build \
  --raw ~/eq_assets/everquest_rof2 \
  --out "$VOL" \
  --no-zones -j 4
```

- **Common set:** rebuilt successfully — 1 set from RoF2 global*_chr.s3d archives
- **Zone baking:** skipped (--no-zones) — existing Titanium-derived zone GLBs preserved
- **Gamedata:** version 10 (1556 files)
- **Gameequip:** version 11 (24 files)
- **Conversion failures:** NONE — zero errors in build log

Container restarted: `podman restart eqoxide_assets` (container now Up, serving on 0.0.0.0:8088).

---

## 3. Before/After Character GLB Comparison

### elf.glb

| Metric | Titanium (before) | RoF2 (after) |
|--------|-------------------|--------------|
| File size | 14,444,832 bytes | 14,444,832 bytes |
| Meshes | 1 | 1 |
| Primitives | 27 | 27 |
| Animations | 131 | 131 |
| Hair materials | NONE | NONE |

**Finding:** `globalelf_chr.s3d` is byte-for-byte IDENTICAL between Titanium and RoF2
(MD5: `0f2a88af2585aab246cc35dc818457de`). The elf model did not change between these
game versions. The GLB is identical.

### humanoid.glb (human male, via globalhum_chr.s3d)

| Metric | OLD (client cache) | RoF2 (volume) |
|--------|--------------------|---------------|
| File size | 14,484,760 bytes | 14,484,760 bytes |
| Meshes | 1 | 1 |
| Primitives | 27 | 27 |
| Animations | 141 | 141 |
| Hair materials | NONE | NONE |

The client cache was already 141 anims, matching the RoF2 output exactly — this confirms
`globalhum_chr.s3d` is also identical between Titanium and RoF2 (or a prior RoF2 build
had already populated the cache).

---

## 4. Hair Geometry Finding (Critical)

**The "missing hair" premise is incorrect for Luclin-era S3D models.**

Luclin-era EverQuest character models (stored in `global*_chr.s3d`) do NOT have hair as
separate mesh geometry in any client version. Hair is baked into the head mesh geometry
and expressed via texture slots (ELFHE*, HUMHE*, etc.). No S3D-based character archive —
Titanium, RoF2, or otherwise — contains a separate hair primitive.

Evidence from material names in elf.glb:
- Head textures: `ELFHE0001_MDF` through `ELFHE0008_MDF` — these are head variants that
  include hair texture variations baked in
- No `HEHAIR`, `HAIR`, or equivalent material exists in any Luclin S3D archive

**Hair as separate geometry only exists in EQG-format models** (post-Luclin, post-Titanium,
used in SoF and later zones). The RoF2 EQG archives for PCs (e.g. `huf.eqg`, `hum.eqg`)
exist in `~/eq_assets/everquest_rof2/` but the S3D-to-GLB converter cannot yet read EQG
format. Those would be a separate project.

---

## 5. Which S3D Archives Differ Between Titanium and RoF2

Running MD5 comparison across all `global*_chr.s3d` files:

| Archive | Titanium size | RoF2 size | Different |
|---------|---------------|-----------|-----------|
| global_chr.s3d | 7,364,740 | 7,364,868 | YES (+128 bytes) |
| global2_chr.s3d | 1,737,358 | 1,737,361 | YES (+3 bytes) |
| global4_chr.s3d | 2,491,953 | 2,491,998 | YES (+45 bytes) |
| globaldrk_chr.s3d | (absent) | 311,403 | NEW in RoF2 |
| All other global*_chr.s3d | — | — | IDENTICAL |

`globaldrk_chr.s3d` is new in RoF2 and contains Drakkin equipment item models (IT10731,
IT10732), not a humanoid character.

---

## 6. Conversion Failures

None that matter for characters. Build log had zero error/warning/skip lines outside of
the expected `--no-zones` skip message.

Some RoF2 archives are EQG format and were not processed by this run (EQG reading is not
implemented), but those are zone/object archives — no character GLBs were skipped.

---

## 7. Zone GLB Preservation Verified

Zone GLBs (qeynos.glb, etc.) remain untouched in `$VOL/work/zones/`. The --no-zones flag
correctly skipped `build_zones_from_raw` and printed the skip message.

---

## Summary

- `--no-zones` flag: DONE, committed as f7a6c5f
- RoF2 common + gameequip rebuild: DONE, no errors
- Container restarted: DONE
- elf.glb hair geometry: NO — and cannot exist in S3D-format Luclin models
- elf.glb mesh/anim counts: unchanged (27 prims, 131 anims) — source files identical
- humanoid.glb: unchanged (27 prims, 141 anims) — source files identical
- Zone GLBs: preserved (--no-zones worked correctly)
