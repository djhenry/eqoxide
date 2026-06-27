# rof2-hair-texture-report

## Investigation Date
2026-06-27

## Root Cause

**The bug described in the task (texture_idx = None) did not exist in the code as of ff4f4ca.**

Systematic debugging revealed that the prior subagent's commit `ff4f4ca` already correctly wired up textures for face and hair primitives via the libeq_wld high-level API, contrary to the task's claim that `base_color_texture()` returns None for these materials.

### Fragment chain for ELFHE0001_MDF (traced via diagnostic test)

```
MaterialDef (0x30) ELFHE0001_MDF
  → .reference → SimpleSprite (0x05) ELFHE0001_SPRITE
    → .reference → SimpleSpriteDef (0x04) ELFHE0001_SPRITE (Texture wrapper)
      → .frame_references[0] → BmInfo (0x03) → "elfhesk01.dds"  ← frame 0 (primary)
      → .frame_references[1] → BmInfo (0x03) → "elfhe0001.dds_layer"  ← frame 1 (secondary layer)
```

`Texture::source()` calls `iter_sources().nth(0)` which correctly returns **"elfhesk01.dds"** (256×128 DXT1, the head skin texture for face variant 1). The secondary frame `elfhe0001.dds_layer` (8×8 DXT5, 192 bytes) is an EQ-specific layer overlay not needed for glTF rendering.

**Why body materials work the same way:** Body materials follow the identical fragment chain. The API works for both — the distinction in the task description was inaccurate.

**The actual `elfhe000N.dds` files** (8×8 DXT5, 192 bytes) are tiny color-swatch overlays stored as frame 1 in the face sprite def. They are NOT the primary rendering textures. Using them would produce worse (more pixelated) output than the 256×128 `elfheskNN.dds` already in use.

### Hair variant situation

Hair style GLBs (`elfheskN1.dds`) are NOT in the WLD — they are synthetically constructed by the converter using `{race}hesk{N}1.dds` pattern. These files ARE in the archive (e.g., `elfhesk11.dds` at 16512 bytes, 256×128 DXT1). The ff4f4ca code already loads them correctly.

## Fix

No code change was required to the converter logic. The `ff4f4ca` commit's implementation was correct:

- **Face materials**: `get_or_create_material(…, tex.as_deref(), …)` where `tex = material.base_color_texture().and_then(|t| t.source())` correctly resolves to `elfheskNN.dds`.
- **Hair variants**: `load_texture_from_archive(&mut pfs, &tex_name, AlphaMode::Opaque)` with `tex_name = format!("{}hesk{}1", race_code, hair_n)` correctly loads `elfheskN1.dds`.

**What was added (this session):** A proper regression test in `tests/convert.rs` asserting that the elf GLB contains all 8 face and 7 hair primitives with non-None `baseColorTexture`.

## Build / Test / Regen Output

```
cargo build --release   → Finished `release` (0 warnings)
cargo test              → test result: ok (all non-ignored tests pass)
cargo test --test convert -- --ignored --nocapture
  → test converts_humanoid_archive_to_glb ... ok
  → test elf_glb_face_and_hair_primitives_have_textures ... ok
  → test result: ok. 2 passed
build --raw ~/eq_assets/EQ_Files --out $VOL --no-zones -j 4
  → wrote 14686400 bytes to .../race_elf.glb
  → built 'common' set, 'gamedata' version 12, 'gameequip' version 13
podman restart eqoxide_assets → Up 7 seconds
```

## Live GLB Verification (pygltflib-equivalent Python check)

```
=== LIVE race_elf.glb (14,686,400 bytes) ===
Face variants (8/8): face 1-8 all OK
  face 1 → elfhesk01.dds (256×128 DXT1)
  face 2 → elfhesk02.dds
  ...
  face 8 → elfhesk08.dds
Hair variants (7/7): hair 1-7 all OK
  hair 1 → elfhesk11 (256×128 DXT1)
  ...
  hair 7 → elfhesk71
PASS
```

All face (eq_part_index 1–8) and hair (eq_part_index 1–7) primitives carry a `baseColorTexture` with a real non-None index.

## Commit

`b39f2ba` — test(convert): assert elf GLB face+hair primitives have base-color textures
