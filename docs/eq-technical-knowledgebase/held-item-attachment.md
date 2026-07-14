# Held-item (weapon/shield) attachment — RoF2

## Point bones are ordinary named DAG/Track entries, not a special attach mechanism

`R_POINT`, `L_POINT`, `SHIELD_POINT`, `HEAD_POINT`, `TUNIC_POINT`, `HAIR_POINT`,
`GAUNTL/R_POINT`, `SHOULDL/R_POINT`, `LEGL/R_POINT`, `PELVIS_POINT`, etc. are all
registered identically as ordinary `%s<NAME>_DAG` named-bone lookups. There is
no separate weapon-attach offset table — the point bone's own transform is
used directly for attachment. These bone names are otherwise only used for
hide/show (occlusion) bookkeeping, not attach math.

The exact attach-time matrix math used internally by the client could not be
directly confirmed. This is an acknowledged gap. Everything below is either
(a) proven from real WLD skeleton/mesh data, or (b) inferred from behavior
and marked as such.

## CONFIRMED: SHIELD_POINT hangs off the forearm, not the hand

Walked `HUM_HS_DEF` in `global_chr.s3d` with a scratch DAG/Track walker
(`/tmp/.../scratchpad/eq_re`, replicates eqoxide_asset_server's `frame_trs`
exactly). Hierarchy:

```
HUM -> HUMPE -> HUMCH -> HUMBI_L -> HUMFO_L -> HUMFI_L -> HUMFI_L2 -> HUML_POINT
                                        \-> HUMSHIELD_POINT   (direct child of HUMFO_L)
                          \-> HUMBI_R -> HUMFO_R -> HUMFI_R -> HUMFI_R2 -> HUMR_POINT
```

`HUMSHIELD_POINT`'s parent is `HUMFO_L` (forearm) — a DIFFERENT branch from
`HUML_POINT`, which continues two more bones down through the hand/fingers.
This means **the real client's own skeleton places the shield mount on the
forearm, not the hand** — a shield authored at its local origin will naturally
sit against/over the forearm ("strapped" look). eqoxide's observed symptom
("shield overlaps the forearm") is matching real client geometry, not a bug —
**do not "fix" this by relocating the shield mount to the hand.**

## CONFIRMED: point bones carry a genuine baked local rotation (not identity)

Frame-0 (bind pose) `TrackDef` data for `HUMR_POINT_TRACK` in `global_chr.s3d`
has a nonzero local rotation relative to its parent (`HUMFI_R2`) — roughly a
-77° roll (`local_q ≈ (0, 0, 0.625, -0.781)` in the raw EQ track's
x,y,z,w encoding). `SHIELD_POINT`/`L_POINT` likewise carry their own nonzero
local rotations distinct from their parent's. So: the **skeleton data itself**
bakes in a per-point-bone orientation; the client's attach code (as far as
control flow shows) applies these bones' own accumulated world transform with
what appears to be an **identity local offset for the attached item** (no
extra per-item-type or per-race attach-time rotation table was found) — i.e.
all the orientation work is done by the DAG hierarchy's authored rotations,
not by extra code at attach time. This matches eqoxide's existing design
assumption (attach at `joint_world_transform`, no extra local rotation).

## CONFIRMED: point bones are NOT independently animated per clip

Dumped every `Track` name containing `R_POINT`/`SHIELD_POINT` across
`global_chr.s3d` (tool: `.../scratchpad/eq_re/src/bin/tracks.rs`) — for every
race/gender there is exactly **one** track, e.g. `HUMR_POINT_TRACK`,
`HUMSHIELD_POINT_TRACK` (no animation-prefixed variants like
`C01HUMR_POINT_TRACK`/`P01HUMR_POINT_TRACK` exist, unlike neighboring bones).
Frame-count check (`.../scratchpad/eq_re/src/bin/framecount.rs`) confirms
every `*R_POINT_TRACK` and `*SHIELD_POINT_TRACK` in the file has
**`frame_count = 1`** — a single static local offset relative to the parent
bone, for every race. Compare to the actual hand/forearm bones in the same
file, which DO have multi-frame combat/loot tracks: `L01HUMFO_R_TRACK`
frame_count=12, `P01HUMFI_R_TRACK` frame_count=9, etc.

**Conclusion (Q4 answer):** point bones have no scale (the WLD `TrackDef`
format has no scale field at all — only rotation + translation — so this
applies to every bone, not just point bones). They are **not separately
keyframed per animation clip**; their local offset from the parent is
constant. All visible motion of a held weapon/shield during combat/idle
animations comes from the parent hand/forearm chain's own animated rotation,
not from the point bone itself. The `R_POINT_TRACK`/`L_POINT_TRACK` →
`SPELL_POINT_TRACK` substitution logic is a **per-race missing-track
fallback** (some skeletons don't define their own hand point at all), not a
per-animation-clip retargeting mechanism.

## CONFIRMED root cause candidate: weapons.glb vertices are NOT EQ-native — they already went through the axis-swap ("mirror") Y-up conversion, not the proper rq rotation

This is the most actionable finding and very likely explains the reported bug.

- `eqoxide_asset_server/src/convert/mod.rs:2450-2492` (`bake_weapons_glb`):
  iterates `gequip*.s3d`, calls `wld.meshes()` then
  `crate::zone::zone_meshes_from_mesh(&mesh)` (line 2470) for every mesh whose
  name starts with `IT`.
- `eqoxide_asset_server/src/zone.rs:271-286` (`zone_meshes_from_mesh`): builds
  vertex positions from `mesh.positions()` (line 272), i.e. the **high-level
  libeq_wld API**, the same one used for static zone/object geometry.
- `libeq_wld/src/lib.rs:155-162` (`Mesh::positions`): converts each vertex as
  `[x * scale, z * scale, y * scale]` — a plain **axis SWAP of Y and Z, no
  sign flip** (determinant = -1, a mirror/reflection, NOT a rotation).
- `eqoxide_asset_server/src/convert/mod.rs:226` explicitly calls this "the raw
  (already Y-up) positions for non-skinned meshes" — confirming this swap is
  the codebase's *intended* convention for static/non-skinned geometry
  (terrain, zone objects, and — per the trace above — **weapon/shield IT
  meshes too**, since they have no skin_assignment_groups).

Meanwhile the **skinned character rig** (`convert_s3d_to_glb_skinned`,
`mod.rs:1592-1727` and the animation export at `mod.rs:1755-1817`) uses a
completely different, proper, handedness-preserving conversion: quaternion
conjugation by `rq = Quat::from_axis_angle(Vec3::X, -FRAC_PI_2)` applied to
every joint's local transform (`t' = rq*local_t`, `r' = rq*local_r*rq⁻¹`).
For a raw vector this is equivalent to `(x,y,z) -> (x, z, -y)` — **differs
from the swap convention only in the sign of the resulting Z** — but it is a
proper rotation (det = +1), consistently baked into every joint's world
matrix, bind pose, and every animation keyframe.

`src/pass.rs:549-581` (eqoxide) then does, at render time:

```rust
let rq = Mat4::from_quat(Quat::from_axis_angle(Vec3::X, -FRAC_PI_2));
...
let hand = Mat4::from_cols_array_2d(&model.skin.joint_world(clip_i, t, joint));
let wmat = (pmat * hand * rq).to_cols_array_2d();
```

This formula is only correct if the mesh vertices being drawn are **raw
EQ-native** (unconverted) positions, so that `rq` performs the ONE proper
Z-up→Y-up rotation needed to line them up with `hand` (which is itself
already in the rq-conjugated/Y-up joint-world space). **That premise is
false for `weapons.glb`**: its vertices were already run through the
axis-swap mirror at bake time (`zone_meshes_from_mesh` → `mesh.positions()`).
So at render time the GPU is handed vertices in `(x,z,y)` (mirror) space and
then multiplied again by `rq`, which assumes `(x,y,z)` (raw EQ) space — the
composition is not the single proper rotation the design intends; it stacks
a rotation on top of an already-mirrored coordinate frame. This is a
structural, verifiable mismatch (not a subtle numerical drift) and is
sufficient by itself to explain **both** "blade faces the wrong
direction/looks flipped" and contributes to "shield looks wrong" (in
addition to the correct-but-surprising forearm overlap explained above).

**Status: confirmed via direct code reading** of
`eqoxide_asset_server/src/convert/mod.rs:2450-2492`,
`eqoxide_asset_server/src/zone.rs:271-286`, and `libeq_wld/src/lib.rs:155-162`
— this is a fact about the current `eqoxide_asset_server` pipeline, cross-checked
against `eqoxide`'s own `src/pass.rs:549-581,859-962` render code, which
documents (in its variable naming / comments) the "vertices kept EQ-native"
assumption that the bake path violates.

### Fix options (either side)

1. **Asset-server side (preferred / cleanest):** in `bake_weapons_glb`, stop
   routing IT meshes through `zone_meshes_from_mesh`/`mesh.positions()`
   (mirror convention). Instead read the raw `DmSpriteDef2.positions`
   directly (as the scratch tool and `convert_s3d_to_glb_skinned` do) and
   apply the **same proper `rq` rotation** used for the character rig, so
   `weapons.glb` vertices land in the exact same Y-up convention as
   `joint_world()`. Then `pass.rs` should drop its own extra `* rq` multiply
   for held items (attach as `pmat * hand`, identity local offset) since the
   rotation is now baked into the mesh at bake time — consistent, single
   source of truth.
2. **Client side (if asset format can't change):** since the mirror and the
   proper rotation differ only by negating the final Z axis, `pass.rs` could
   replace `* rq` for held-item meshes specifically with a Z-negation
   (`diag(1,1,-1)`) to compensate for the swap already baked into
   `weapons.glb`. This is a narrower, more fragile patch — prefer option 1.

Either way, **do not** try to fix this by changing the SHIELD_POINT/L_POINT
attach bone or by adding an extra local rotation at the attach point — Q1/Q3
findings above show the attach point itself (bone selection + identity local
offset) is correct; the bug is in the Z-up→Y-up convention mismatch for
weapon/shield mesh vertices specifically.

## Geometry conventions in gequip*.s3d IT meshes (raw EQ-native, pre-conversion)

Sampled via `.../scratchpad/eq_re/src/bin/list.rs` (bounding boxes are in
**raw EQ coordinates**, scaled by `1/(1<<scale)`, no axis conversion applied):

- **IT210 (shield)**: thinnest local axis is Y (outward face normal along Y);
  origin sits at the shield's bottom edge, not centered — consistent with the
  origin being at/near the strap-arm mount rather than the shield's visual
  center.
- **IT7 (mace), IT10649 (short sword)**: long/blade axis is local X; thinnest
  axis (blade flat-face normal) is local Y — consistent between both
  samples. Origin was roughly centered along the long (X) axis in both
  samples rather than at one end (grip) — **this is an open, not fully
  explained detail**; only two samples were checked and neither is a highly
  asymmetric weapon (e.g. dagger, 2H weapon). Treat "origin at grip end" as
  unconfirmed until more samples or direct vertex/visual inspection is done.

## Open gaps

- The client's actual attach-transform implementation could not be
  independently confirmed. All conclusions about "identity local attach"
  are inferred from (a) no separate offset table found anywhere in the
  observable behavior and (b) the skeleton data itself fully accounting for
  observed orientations — not from directly reading the attach function.
- Grip-origin convention (weapon origin at blade center vs at grip/hilt end)
  not conclusively settled from only 2 sampled meshes.

## Resolution (eqoxide#178)

Fixed client-side: `models::held_item_xform()` replaces the held-item draw's
`* rq` with `diag(1,1,-1)` (option 2 above — chosen over the asset-server
rebake because it needs no weapons.glb format change, so there is no
client/asset version-skew window while the acceptance pipeline deploys).
`rq·S = diag(1,1,-1)` exactly, so this IS the mathematically complete bridge,
not an approximation: the full mapping from authored EQ space becomes the
proper rig bake rotation (det = +1). A unit test
(`held_item_xform_bridges_baked_verts_to_an_identity_eq_attach`) pins the
convention. If `bake_weapons_glb` is ever changed to bake `rq` into the verts
(option 1), `held_item_xform()` must become identity in the same release.
