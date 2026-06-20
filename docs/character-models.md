# Character Models: pipeline, normalization, gender

How EQ character models get from `.s3d` archives onto the screen, why they're scaled
and positioned the way they are, and how race/gender selection works. Companion to
`docs/equipment-textures-findings.md` (armor textures).

## Source assets → GLB

Character models live in EQ `global{race2}{gender1}_chr.s3d` archives, e.g.:

| Archive | Race / gender |
|---------|---------------|
| `globalhum` / `globalhuf` | Human male / female |
| `globalelm` / `globalelf` | Wood Elf male / female |
| `globaldwm` / `globaldwf` | Dwarf male / female |
| `globalhom` / `globalhof` | **Halfling** male / female (NOT human — see history) |

The `s3d_to_gltf` tool (in `tools/`, built to the workspace `./target/release/s3d_to_gltf`)
converts an archive to a skinned, animated `.glb`:

1. **Skeleton + bind pose** — `HierarchicalSpriteDef` → per-bone world bind matrices.
2. **Pose the mesh** — each vertex is transformed by its bone's world bind matrix into
   model space.
3. **Z-up → Y-up** — EQ is Z-up, glTF is Y-up; vertices, joint transforms, inverse-bind
   matrices, and animation keyframes are all rotated by a fixed quaternion.
4. **Skinning data** — per bone, `inverse_bind = inverse(world_bind)`; the GPU computes
   `skinned(v) = Σ weightᵢ · jointᵢ · inverseBindᵢ · v`.
5. **Animations** — per-clip per-bone keyframes become glTF animation channels.
6. **`eq_height`** — the model's true height (EQ-native Z extent of the posed bind) is
   written into the root node's glTF `extras`, for render-time scaling.

Regenerate all character models with `tools/regen_models.sh` (run from repo root; uses
`$EQ_ASSETS` or `~/eq_assets/EQ_Files`). Output `.glb` files are **gitignored** — only the
script and code are committed. Naming: `<archetype>.glb` = male, `<archetype>_f.glb` = female.

## Loading + race/gender selection

`renderer.rs: load_character_models` loads each archetype's `.glb` and, when present, its
`_f.glb` female variant, into `gpu_character_models` keyed by `(archetype, gender)`
(gender 0 = male, 1 = female).

- `race_to_archetype(race)` (`models.rs`) maps an EQ race code to an archetype key. Many
  playable races collapse into `humanoid` (human model) — per-race models are future work.
- `gender` is parsed from the spawn (`Entity.gender`, `GameState.player_gender`), carried
  to `Billboard.gender` / `SceneState.player_gender`.
- `EqRenderer::model_for(archetype, gender)` returns the gender variant, falling back to
  male (gender 0) when no female variant exists. All character passes use it.

## Normalization: scale + position (load/render-time)

Raw conversions are not centered or uniformly scaled, so the **renderer** normalizes each
frame (we tried doing this at conversion time but it fought the animations — see history):

- **Scale** — `mesh_scale = archetype_target_height(archetype) / model.true_height`
  (`true_height` = `eq_height` from extras, else measured `y_extent`). So a model renders
  at its archetype's `target` EQ height regardless of its raw authoring scale.
  `archetype_target_height` returns the desired **rendered** height; humanoid = 12 EQ
  (calibrated to the doorway), other human-height races match it, others proportional.
- **Centering** — `entity_model_matrix_heading` recenters by the model's measured posed
  horizontal centers (`center_xz = [x_center, z_center]`), computed in `ModelAsset::load`
  from the skinned bind pose, so the model sits over the entity position.
- **Grounding** — lift by the **constant** bind-pose feet height
  (`bind_lowest_skinned_z`), so the body stays at a fixed height and the animation's foot
  motion is visible (per-frame lowest-point grounding caused the body to bob up mid-stride).

`bind_pose()` (`anim.rs`) returns the real rest skinning matrices (`global_rest *
inverse_bind`), **not** identity — EQ meshes are authored off-pose, so identity would
render the raw, off-center mesh.

### Verifying placement

`models.rs` test `humanoid_player_transform_grounds_and_centers` (`--ignored`, needs the
glb) runs the exact player-pass placement math and asserts the model ends up grounded
(feet ≈ pos.z), horizontally centered on pos, and ≈ target tall. Useful as a regression
guard when touching the transform.

## History / gotchas

- `humanoid.glb` was originally built from `globalhom` = **Halfling** (furry feet!), not
  `globalhum` = Human. Symptom: every humanoid race rendered as a halfling. Fixed by
  regenerating from `globalhum` and keying the `_chr.s3d` fallback to it.
- The human male model (`globalhum`) converts with an unusually large/off-center raw bbox
  (≈2× the others); the load/render normalization handles it, so don't be alarmed by its
  raw `y_extent`/`x_center` in the load logs.
- Conversion-time translation normalization (offsetting the skeleton root) was implemented
  then reverted: a single offset can't center every animation clip (each has its own root
  baseline), which displaced the model while moving. Normalization is load/render-time.

## Known limitations / future work

- Per-race models: only human/wood-elf/dwarf archetypes exist; barbarian, erudite, ogre,
  etc. all use the human `humanoid` model.
- Gender variants only for humanoid/elf/dwarf; monsters are single-model.
- `gnoll` archetype is actually the Gnome model (`GNM`).
- Monster target heights are proportional guesses pending visual calibration.
