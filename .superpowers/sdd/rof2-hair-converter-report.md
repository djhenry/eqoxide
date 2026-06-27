# RoF2 Hair Converter Report

**Status:** DONE  
**Commit:** `ff4f4ca` (eqoxide_asset_server, branch `main`)

---

## What was implemented

### Where head variants are emitted

In `eqoxide_asset_server/src/convert/mod.rs`, function `convert_s3d_to_glb_skinned` (the skinned character model path). The primitive-building loop that was already iterating `mesh.primitives()` now:

1. **Detects face variant primitives** via `face_variant_from_material_name()` — checks if `material.name()` matches `{RACE}HE000{N}_MDF` (N=1..8). The texture-source name is NOT used because libeq_wld's high-level API does not resolve the BitmapInfo→BitmapName chain for these materials (confirmed: `material.base_color_texture().and_then(|t| t.source())` returns `None` for all face materials).

2. **Tags face primitives** with `extras` per the contract.

3. **Generates 7 hair primitives** after the mesh loop by reusing the face-1 head polygon group indices, loading `{race}hesk{N}1.dds` directly from the PFS archive for each style N=1..7 (primary diffuse layer only).

New helpers added:
- `face_variant_from_material_name(name: &str) -> Option<u8>`  
- `face_variant_from_texture(src: &str) -> Option<u8>` (fallback; not currently triggered in practice)  
- `race_code_from_archive(path: &Path) -> Option<String>`

`src/zone.rs` also updated: `PrimitiveData` construction there gained `extras: None` (compile fix only).

---

## Exact extras tags emitted

### Face primitives (8 total)

**Face 1 (default visible):**
```json
{ "eq_head_part": "face", "eq_part_index": 1 }
```

**Faces 2–8 (default hidden):**
```json
{ "eq_head_part": "face", "eq_part_index": N, "eq_default_hidden": true }
```

where N ∈ {2,3,4,5,6,7,8}.

### Hair primitives (7 total, all default hidden)

```json
{ "eq_head_part": "hair", "eq_part_index": N, "eq_default_hidden": true }
```

where N ∈ {1,2,3,4,5,6,7}.

### Body/eye primitives

No `extras` field emitted. These are always rendered.

---

## Default visibility convention (client must match)

**Rule:** At model-load time, the client must iterate all mesh primitives and:

1. If a primitive has no `extras` → always visible (body, eyes).
2. If `extras.eq_head_part == "face"`:
   - Show ONLY the primitive where `eq_part_index == spawn.face + 1` (Spawn_Struct `face` is 0-indexed).
   - Hide all others (including face 1 if spawn.face != 0, but face 1 is shown as the initial fallback before spawn data arrives).
3. If `extras.eq_head_part == "hair"`:
   - Show the primitive where `eq_part_index == spawn.hairstyle` if `spawn.hairstyle > 0`.
   - Hide all hair primitives if `spawn.hairstyle == 0` (no hair / bald).

**Boot default (before spawn data):** client should hide any primitive with `extras.eq_default_hidden == true`. This produces: face 1 visible, all hair hidden, body + eyes visible.

**Selection:** on spawn packet arrival, use `eq_part_index` to select exactly one face and at most one hair. The `eq_default_hidden` field is only for the initial render; after spawn data it should be ignored in favor of `eq_part_index` matching.

---

## Build + test + regeneration summary

- `cargo build --release` — clean, 13.85s
- `cargo test` — **29 passed, 0 failed, 3 ignored** (new tests: `face_variant_detects_he000n_pattern`, `race_code_extraction_from_path`)
- `cargo test -- --include-ignored elf_glb_has_face_and_hair_variant_extras` — **PASSED** (verifies live elf GLB has all 8 face + 7 hair variants with correct extras)
- Rebuild: `build --raw ~/eq_assets/EQ_Files --out $VOL --no-zones -j 4` succeeded; all humanoid races emitted `hair style N → '{race}hesk{N}1'` lines
- `podman restart eqoxide_assets` succeeded, port 8088 up
- Live `elf.glb` in volume verified: face 1 visible, faces 2–8 hidden, hair 1–7 hidden

---

## Verified live GLB stats (race_elf / elf.glb)

```
Total primitives: 34
  Face variants: 8  (eq_part_index 1-8; face 1 = DEFAULT VISIBLE, 2-8 = hidden)
  Hair variants: 7  (eq_part_index 1-7; all hidden by default)
  Body/eye primitives: 19  (always visible)
```

---

## Uncertainties / limitations

1. **Hair texture layer selection:** only `layer 1` (`hesk{N}1.dds`) is loaded per style. The WLD also references `layer 4` and `layer 5` per style (`hesk{N}4.dds`, `hesk{N}5.dds`). The real EQ client composites all 3 layers. For eqoxide's single-texture material model, layer 1 is the primary diffuse — this may show hair without tint channels until a multi-layer material model is added.

2. **Face material textures:** the face primitive materials (`ELFHE0001_MDF` etc.) have `texture_idx: None` in the emitted GLB (libeq_wld's high-level API doesn't resolve their BitmapInfo chains). The face polygons will render without texture. A deeper fix would require using the raw `WldDoc` API to walk the fragment reference chain and load the face texture directly — the texture files do exist in the archive (`elfhe0001.dds` etc.).

3. **Non-humanoid races:** races without `{race}hesk{N}1.dds` files (e.g. troll, ogre) will silently skip hair generation (logged as "hair style N texture '...' not found"). This is correct behavior.

4. **tools/src/main.rs mirror:** this standalone dev tool is a copy of the converter logic and was NOT updated (task scope excluded the eqoxide client repo). It does not emit face/hair extras and its `PrimitiveData` still lacks the `extras` field. The production path (asset server binary) is correct.
