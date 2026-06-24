# Equipment & Character Texture Findings

Empirical notes on how EQ character armor/textures map onto this client's GLB models.
Verified 2026-06-19 against the original Titanium game client's `.s3d` files and
`assets/models/*.glb`. Keep this so we don't have to re-derive it.

## The rendering pipeline (what's actually true)

- Characters/NPCs render from **pre-converted GLB** files: `assets/models/<archetype>.glb`
  (flat files — `humanoid.glb`, `gnoll.glb`, `skeleton.glb`, …), loaded **once at startup**
  by `renderer.rs: load_character_models`. There is **no runtime `_chr.s3d` loading** for
  the live renderer (a `load_from_chr_s3d` fallback exists in `models.rs` but the `.glb`
  files always win when present).
- `assets/models/<archetype>/<archetype>.glb` (subdir form) described in `dev-workflow.md`
  is stale; the files are flat `assets/models/<archetype>.glb`.
- The GLB→GPU path: `ModelAsset::load` (models.rs) → `GpuModel::{Static,Skinned}` (gpu.rs)
  → drawn in `pass.rs` (`encode_entity_pass`, `encode_skinned_entity_pass`,
  `encode_player_pass`, `render_static_model`). Each pass iterates **all** `model.meshes`
  and binds a texture per mesh via `mesh.texture_idx`.

## GLB primitive structure (verified on humanoid.glb)

- One glTF mesh named `combined`, **27 primitives**, **all sharing one vertex buffer**
  (POSITION accessor 0, 1331 verts, identical bbox). Primitives differ only by their
  **index list** and **material**.
- The 27 primitives are **disjoint** triangle subsets — checked pairwise, **zero shared
  vertices** between any two, including between `HOMCH0001`/`0002`/`0003` and between the
  8 `HOMHE000x` head pieces. So:
  - They are NOT overlapping alternates. Together they form the **complete naked body**.
  - **Drawing all primitives is correct.** Do not "pick one primitive per slot."
- The `0001/0002/0003` suffixes are **variant/piece numbers within a region** (different
  patches of the same body region), not skin-tone alternates.

## Material name → meaning

glTF material names look like `HOMCH0001_MDF`:

```
HOM   CH      00        01
^prefix ^region ^material ^variant
(race+gender)  (body)   (number)  (piece)
```

- **prefix** (3 chars): race + gender, e.g. `HOM` = human male, `HUF` = human female,
  `GNM` = gnoll, `SKE` = skeleton. It is already baked into the model's material names —
  derive it from there; do **not** build a race_id→prefix table.
- **region** (2 chars): body part (see table).
- **material** (2 digits): armor material number. `00` = naked body.
- **variant** (2 digits): piece index within the region.

Non-armor materials exist and don't follow the pattern (e.g. `HOFL_EYE_MDF`,
`HOMR_01_MDF`). Parse defensively: unknown region / non-numeric digits → not an armor slot.

### Region code → equipment slot (EQEmu `MaterialType`)

| Slot | EQEmu name | Region |
|------|------------|--------|
| 0 | Head    | `HE` |
| 1 | Chest   | `CH` |
| 2 | Arms    | `UA` |
| 3 | Wrist   | `FA` |
| 4 | Hands   | `HN` |
| 5 | Legs    | `LG` |
| 6 | Feet    | `FT` |
| 7 | Primary weapon | n/a (held mesh, not a texture) |
| 8 | Secondary      | n/a |

Note: a smaller model's earlier plan got this wrong (it used `HN` for arms, `FA` for
hands). The correct mapping is arms=`UA`, wrist=`FA`, hands=`HN`.

## Equipment = texture swap (not mesh swap)

`equipment[slot]` (a `u32` from the spawn struct) is the **armor material number**. To
equip armor on a region, swap every primitive of that region from its `material=00`
texture to `material=equipment[slot]`, keeping each primitive's own `variant`:

```
texture filename = "{prefix}{region}{material:02}{variant:02}.{ext}"   (lowercased)
e.g. chest variant 01 with armor 17 -> "homch1701.bmp"
```

`equipment_tint[slot]` is an RGB multiplier applied to the texture (use the existing
`EntityUniform.tint`).

## Where the textures live

- **GLB-baked textures**: only the **naked/skin** set (e.g. `homchsk01.dds`, `chr_eye001.dds`).
  The GLB does **not** contain armor textures.
- **`global17_amr.s3d` … `global23_amr.s3d`**: armor materials **17–23** (Velious sets),
  `.bmp`, all races in one archive. Filenames like `homch1701.bmp`, `gnmft1702.bmp`.
  Each `globalNN_amr.s3d` holds material number `NN`.
- **`<archetype>_chr.s3d` / `_chr2.s3d`** (e.g. `globalhom_chr.s3d`, `globalhom_chr2.s3d`):
  the lower material numbers + skin textures.
- `lgequip_amr.s3d` / `lgequip_amr2.s3d`, `lgequip*` etc. exist too (large/luclin); not
  needed for the Titanium classic path.

## Spawn struct fields (protocol.rs `Spawn_S`, Titanium)

Already present and parseable:
- `equipment: [u8; 36]`  — 9 slots × `u32` LE = material id per slot.
- `equipment_tint: [u8; 36]` — 9 slots × 4 bytes; EQEmu `Tint_Struct` wire order is Blue, Green, Red (UseTint). All tint sources (spawn, WearChange, player profile) reverse wire BGR to stored RGB on parse.
- `equip_chest2: u8`, `helm: u8`, `showhelm: u8`, `gender: u8`, `class_: u8`,
  `bodytype: u8`, `race: u32`.

`WearChange` packet (`OP_WearChange`) carries runtime equip/unequip: spawn_id, material,
color, wear_slot_id. **Verify the opcode hex against EQEmu's
[`utils/patches/patch_Titanium.conf`](https://github.com/EQEmu/Server/blob/master/utils/patches/patch_Titanium.conf)** and the struct against
EQEmu's [`common/patches/titanium_structs.h`](https://github.com/EQEmu/Server/blob/master/common/patches/titanium_structs.h) before trusting any constant — an earlier
plan's `0x6427` is unverified.

## Texture decoding

`assets.rs: ZoneAssets::load` already decodes both `.bmp` and `.dds` via the `image` crate
(`image::load_from_memory_with_format(..).to_rgba8()`). Reuse that for armor textures.
`libeq_pfs::PfsReader` reads the S3D archives; `.filenames()` + `.get(name)` give bytes.

## Inspecting GLBs (handy)

- `tools/target/release/s3d_to_gltf --list <archive.s3d>` — list meshes/textures.
- The GLB JSON chunk can be parsed directly (it's binary glTF: 12-byte header, then a
  JSON chunk) to dump `materials[].name`, `meshes[].primitives[].material`, and accessor
  bounds — how the facts above were verified.

## Related

- Design/plan: `docs/superpowers/specs/2026-06-19-equipment-textures-design.md`.
- Superseded (incorrect) plan: `docs/equipment_texture_plan.md`.
