# Luclin Heads: Faces, Painted Hair, and the Dead Hairstyle Path (RoF2)

**Status: validated 2026-07-01 against the RoF2 asset archives and bone-weight
analysis of the baked GLBs. SUPERSEDES the hair
sections of `eqg-character-models.md` and the `luclin-character-hair.md` worktree doc,
both of which misread the `hesk` texture digit as a HAIRSTYLE index. It is the FACE
index.**

## The three head attributes and what the client does with them

The client's attribute dispatcher for S3D character heads selects among three
attributes:

| attr | wire field | mechanism |
|------|-----------|-----------|
| 1 | `face` | **material swap** `"%sHE%02d%d1_MDF"` (texture change) |
| 2 | `hairstyle` | **actor attach** at slot 8: Luclin heads look up `"%s_HS%2d_HEAD_HAIR"`; classic heads attach `IT{1000+style+race_offset}` |
| 3 | `beard` | actor attach at slot 9 (`"%s_FACIALHAIR_%02d"` / classic IT2000+) |

### Face (attr 1) — the hesk texture swap

- Material name: `{RACE}HE{colorslot:02}{face}{layer}_MDF`; texture:
  `{race}hesk{face}{layer}.dds`. Layers for huf/elf: 1 (face+scalp), 4 (nose
  bridge), 5 (nose tip) — **verified by bone weights**: the GLB prims for
  hesk{F}4/hesk{F}5 bind to `HUFFANOSE_DAG`, not to any hair bone.
- 8 faces per race (F=0–7). Face differences are eyebrows, makeup, hairline
  shading — the textures at `hesk{0..7}1` are clearly 8 faces, not 8 hairstyles.
- The 10 leading "color slot" material copies (`HE00..HE09`) all reference the
  SAME dds; they exist so the engine can hold differently-tinted material
  instances.

### Hairstyle (attr 2) — DEAD CODE with stock RoF2 assets

Exhaustive scan of every `.s3d` WLD string table in the RoF2 client's asset
archives: **no `*_HEAD_HAIR` actor exists anywhere**, and no `IT13xx/IT14xx`
hair actors exist in `global_chr.s3d`/`gequip*.s3d`. The `{RACE}HEHAIR1–9_DAG`
skeleton bones are sway-chains for those (never-shipped) attachments; **zero
vertices** in any player-race body mesh bind to them. The only real hair
models in RoF2 are the Drakkin EQG ones (`dkf.eqg`/`dkm.eqg`:
`dk{f,m}_hair_00..07.mod`, attached via `"%s_HAIR_%02d"`).

**Therefore: changing `hairstyle` has NO visual effect on S3D player races in the
real RoF2 client.** Bald-looking Luclin heads with painted hair are authentic.

### Haircolor — a runtime multiplicative tint over the painted hair

The hair you "see" on Luclin models is painted into the head textures as a
NEUTRAL LIGHT base (huf scalp texels avg ≈ RGB(158,116,85); elf ≈ (173,123,90) —
lighter than the face). The client colors it by multiplying with a 24-entry tint table (see
`hair-color.md` for the table). In the swap logic (`FUN_0040a240`, `eqgame.exe.c:7166-7197`,
confirmed 2026-07-17) the tint pointer is only passed for **MALE** High Elf, **MALE** Dark Elf,
**MALE** Half Elf, and **FEMALE** Dwarf — the elves are male-only, not both genders; see
`hair-color.md` "Race/model gate" for the exact decompiled condition. The visible result is
neutral/blonde-ish hair for everyone else (including female elves and male Dwarves).

## Head polygon regions (huf, verified by bone weights + debug-color renders)

| Region | Texture | Geometry | Painted content |
|--------|---------|----------|-----------------|
| 1 | hesk{F}1 (256×128) | whole face + skull sides/back (head+face bones) | face + painted hair perimeter |
| 2 | hesk02 (32×64) | crown strip across the skull top (head bone only) | hair part-line |
| 3 | hesk03 (32×32) | under-jaw (jaw bone) | chin shadow |
| 4 | hesk{F}4 (32×64) | **nose bridge** (nose bone) | nose |
| 5 | hesk{F}5 (32×32) | **nose tip** (nose bone) | nose |
| 6 | hesk06 (64×64) | ear (head bone) | ear |
| 7 | hesk07 (64×32) | mouth interior (lip/jaw bones) | palate/throat |
| 8 | hesk08 (64×16) | teeth (head+jaw) | teeth |

(The old claim "regions 4/5 are hair pieces / region 2 is ear tips" was a
geometric misreading; region layouts vary per race but the bone-weight method
generalizes.)

## eqoxide implementation (this fix)

- **Converter** (`eqoxide_asset_server` `convert/mod.rs`): regions 1/4/5 emit 8
  variants tagged `{"eq_face": F}` (F=0 visible, F≥1 `eq_default_hidden`), each
  split into a facial-skin prim (any triangle touching a `{RACE}FA*` bone) and a
  painted-hair scalp prim (all three verts on `{RACE}HEHEAD`), the latter also
  tagged `{"eq_head_part":"hair"}`. Fixed regions fully on the head bone ABOVE
  the topmost facial-bone vertex (the crown strip, region 2) are tagged
  `{"eq_head_part":"hair"}` with no `eq_face` (always visible). No synthetic
  geometry is generated (the old "hair shell" hack is removed).
- **Client** (`models.rs`): `HeadPart::Face(F)` / `HeadPart::Hair(Option<F>)`;
  visibility keyed by `spawn.face`; `Hair` prims tinted by
  `hair_tint(spawn.haircolor)` (>=24 → no tint), **gated to the native client's
  race subset** (`head::hair_tint_applies`: HIE/DKE/HEF both genders + female
  DWF). We previously tinted ALL races ("product decision so haircolor is
  visible") — that superset was #519's root cause: on HUM male, haircolor 0
  multiplied the skin-toned scalp AND the rigid eye-band triangles (which the
  converter's all-verts-on-`HEHEAD` split classifies as "hair" because the
  brow/eye-socket band binds only to the head bone, not any facial bone) by
  RGB(46,26,12), painting a near-black scalp cap and a dark "raccoon-mask"
  band across the eyes. With the native gate the misclassification is invisible
  (hair and face prims share the same hesk texture; the split exists only to
  carry tint).
- `hairstyle` is accepted on the wire but intentionally selects nothing.

## 2026-07-17 fresh re-verification (issue #519, HUM male) — reconfirms dead hairstyle mesh via a STRONGER method, adds eye-mesh ground truth

Re-investigated from scratch (not trusting the string-scan-only method above)
after the owner directly contradicted `classic-vs-luclin-model-selection.md`
and said native HUM male shows the Luclin model with separate/movable eye
meshes and "shaped hair you pick in char creation." Verification used a NEW
bone-weight dump (not just a string-table grep), which is decisive because it
proves a mesh region is or isn't bound to a bone, independent of naming.

**Tooling added** (`eqoxide_asset_server`, reusable):
- `src/bin/skinbones.rs` — `skinbones <archive.s3d> <wld> <mesh_name> <skeleton_name>`:
  dumps every `DmSpriteDef2.skin_assignment_groups` (vertex-range → DAG bone
  name) and `face_material_groups` (face-range → material name) for a named
  mesh, resolved against a named skeleton's dag list.
- `src/bin/skelmeshes.rs` — `skelmeshes <archive.s3d> <wld> <skeleton_name>`:
  dumps a `HierarchicalSpriteDef`'s `dm_sprites` attached-mesh list (the
  mechanism that actually attaches extra meshes like eyes — NOT the per-dag
  `mesh_or_sprite_reference`, which is unused/zero for every dag in
  `HUM_HS_DEF`), plus every `DmSpriteDef2`'s bbox/scale/skin-group summary and
  every raw `DmSprite` (0x2D) wrapper.
- `/tmp/pfslist.py`, `/tmp/wldstrings.py` — standalone PFS directory list +
  WLD string-table XOR decode (key `[0x95,0x3a,0xc5,0x2a,0x95,0x7a,0x95,0x6a]`,
  block at WLD offset 28, length = u32 at offset 20), no Rust build needed.

### Finding #1 (hair mesh) — RECONFIRMED dead, now via bone-weight proof, not just string absence

`skinbones globalhum_chr.s3d globalhum_chr.wld HUM_DMSPRITEDEF HUM_HS_DEF`
(full dump: this session's `/tmp/skinbones_hum.txt`) shows `HUM_DMSPRITEDEF`
(the single body+head mesh, 1264 verts/1674 faces) has **52 skin_assignment_groups
covering 51 of 104 skeleton dags — and NONE of them is** `HUMHEHAIR1_DAG` (dag
idx 8), `HUMHEHAIR2_DAG` (idx 9), `HUMHEBEARD1_DAG` (idx 10),
`HUMHEBEARD2_DAG` (idx 11), `HUMBEARD_POINT_DAG` (idx 25), `HUMHAIR_POINT_DAG`
(idx 26), or `HUMHEAD_POINT_DAG` (idx 27). **Zero vertices, in any mesh in the
file, bind to any hair/beard bone or point.** These 7 dags exist in the
skeleton purely as sway-chain animation targets (each has its own
`_TRACKDEF`/`_TRACK` pair per animation, e.g. `HUMHEHAIR1_TRACKDEF`,
`C01AHUMHEHAIR1_TRACKDEF`, ... — confirmed present in
`/tmp/globalhum_chr_strings.txt`) that were never given renderable geometry —
PROOF, via a different and stronger method than the original string scan.

A full unfiltered string-table scan (`/tmp/globalhum_chr_strings.txt`, 30778
strings) also independently reconfirms **zero** `_HS%02d_HEAD_HAIR`,
`_EB%02d_HEAD_HAIR`, `_FACIALHAIR_%02d`, or `_HAIR_%02d` actor names for `HUM`
in `globalhum_chr.s3d`; the only `_ACTORDEF` in the file is `HUM_ACTORDEF`
itself. Extended the same scan to every `gequip*.s3d` (`gequip.s3d` through
`gequip8.s3d`) and the shared `global2..7_chr.s3d` archives (which turned out
to be pooled NPC/monster models — bear, bat, alligator, kobold, etc. — not a
shared PC-hair pool): the only stray hit anywhere is one `IT1036_ACTORDEF` in
`gequip.wld:3453` (a single unrelated item, not a systematic hairstyle set).
`globalhum_chr2.s3d` (1 file, `globalhum_chr2.wld`) contains **zero** named
`DmSpriteDef2` meshes — its 1060 raw fragments are entirely type 18/19
(`TrackDef`/`Track`, i.e. an alternate animation-frame set), not geometry.

**Conclusion for HUM male Luclin: no hair/beard mesh exists anywhere in this
RoF2 install's searched archives (`globalhum_chr.s3d`, `globalhum_chr2.s3d`,
`gequip.s3d`–`gequip8.s3d`). This is now proven two independent ways (bone
weights + full string scan), not just inferred from a partial grep.**

Freshly re-viewed the actual textures (not trusting the old doc's numbers):
`humhesk01.dds` (256×128, the big 242-face main skull/face material) and
`humhesk02.dds` (32×64, the fixed crown-strip region always-visible above the
face) both show **only skin-toned gradients — no hairline, no hair-colored
region, no eyebrows even** (`/tmp/humhesk01_big.png`, `/tmp/humhesk02_big.png`).
Checked a second face preset, `humhesk11.dds` (face=1, layer=1) — same result,
still bald. **The Luclin HUM head, as shipped in this install, is genuinely
bald with zero painted or modeled hair, regardless of the `face` value
(0–7).** This directly contradicts a literal reading of the owner's
description of visible "swept-back brown hair" on native — see "Reconciling
the owner's observation" below.

### Finding #2/#3 (face/hairstyle/beard dispatch) — re-read fresh, confirms prior doc's mechanism description unchanged

Re-read `FUN_0040d770` (`eqgame.exe.c:9148-9223`) and its callees fresh:
- `param_3==1` → face (material swap, unchanged from before).
- `param_3==2` → hairstyle: gate `(**(code**)(*(int*)param_1[0x5e]+0xbc))()`
  ("is Luclin model loaded"); true → `FUN_0040ab10` (`eqgame.exe.c:7424-7442`,
  attach slot 8, `"%s_HS%2d_HEAD_HAIR"`); false → `FUN_0040aa30(hairstyle+1000)`
  (classic `"IT%d"` item attach, `eqgame.exe.c:7387-7418`).
- `param_3==3` → a simple field setter (`FUN_0040c350`), not mesh-related —
  likely eyecolor2 or a similar non-geometric attribute (not fully traced this
  pass).
- else (beard) → same Luclin gate → `FUN_0040ab80` (`"%s_FACIALHAIR_%02d"`,
  slot 9) / classic `FUN_0040aa30(beard+2000)`.

**New this pass:** there is a THIRD, previously uncited attach mechanism,
`FUN_0040abf0` (`eqgame.exe.c:7472-7490`, `"%s_EB%2d_HEAD_HAIR"`, slot 10,
literally "eyebrow head hair") — but grepping its callers across the whole
decompile finds **none**; the function is defined but never invoked anywhere
in `eqgame.exe`. It is dead at the *engine* level, not just the asset level.
Also found `FUN_0040ac80` (`eqgame.exe.c:7509-7527`, `"%s_HAIR_%02d"`, slot 8)
— this one **is** called (`eqgame.exe.c:8539`, `9663`) and is the mechanism
documented in the pre-existing Drakkin finding below (`dk{f,m}_hair_00..07.mod`);
for `HUM` it resolves to `HUM_HAIR_00`.. which — per the full string scan
above — does not exist either.

### Finding #4 (eyes) — NEW ground truth, answers the "googly eyes" question

`skelmeshes globalhum_chr.s3d globalhum_chr.wld HUM_HS_DEF` (full dump:
this session's output) shows:
- `HUM_HS_DEF.dags[i].mesh_or_sprite_reference` is **0 for every one of the
  104 dags** — i.e. individual bones never carry their own mesh in this WLD;
  that mechanism is unused here.
- The skeleton instead carries `num_attached_skins=3` / `dm_sprites=[321,322,323]`
  — 3 separately-defined `DmSpriteDef2` meshes belonging to one actor:
  `HUMEYE_R_DMSPRITEDEF`, `HUMEYE_L_DMSPRITEDEF`, `HUM_DMSPRITEDEF` (the body).
  **This is the actual Luclin PC-model eye mechanism**: eyes are a genuinely
  separate mesh object attached at the `HierarchicalSpriteDef` level, not a
  per-dag prop and not baked into the body mesh — this matches the owner's
  "separate/movable mesh eyes" description exactly, and is a Luclin-only
  construction (the classic head, per `classic-vs-luclin-model-selection.md`
  finding #4, has no eye mesh — eyes are painted directly into `humhe0001.bmp`
  etc.).
- Each eye mesh (`HUMEYE_R_DMSPRITEDEF` / `HUMEYE_L_DMSPRITEDEF`) is **19
  vertices, 30 faces, and 100% single-bone-rigid**: its one
  `skin_assignment_group` is `(19, piece_idx)` where `piece_idx` = 13
  (`HUMFAEYER_DAG`) for the right eye and 12 (`HUMFAEYEL_DAG`) for the left —
  i.e. the whole eyeball is one rigid prop pinned to its bone's local origin,
  not deformably skinned. `scale=10` matches the body mesh's `scale=10` (same
  fixed-point exponent, so no unit mismatch between eye and body raw
  coordinates). `max_distance` (bounding radius from the mesh's local center)
  is `0.186` for the eye vs `4.043` for the whole body — the eye is ~4.6% of
  the body's radius, a plausible small-eyeball scale in the raw asset. **The
  raw WLD asset data itself is not oversized; if eqoxide renders "bulging
  googly eyeballs," the bug is in how eqoxide places/scales this rigid
  single-bone mesh at runtime, not in the source data.**
- Both eye materials, `HUMR_EYE_MDF` and `HUML_EYE_MDF`, reference the SAME
  single bitmap `CHR_EYE001` (`chr_eye001.dds`, 64×64 DXT1 — confirmed present
  in `globalhum_chr.s3d`'s top-level file list, i.e. NOT race-specific
  despite living in the human archive; likely shared/duplicated per-race
  archive by the exporter). Viewed directly
  (`/tmp/chr_eye001_big.png`): **one flat circular texture with the entire
  eyeball painted on it** — gray sclera with faint red veins, a blue iris with
  radial striations, black pupil, and two white specular highlight dots — a
  single billboard-style painted eyeball, not a separate sclera+iris system.
- **Eyecolor is DEAD CODE too, same pattern as hairstyle/beard.** Found a
  THIRD attach/swap mechanism not in the prior doc: `FUN_0040add0`
  (`eqgame.exe.c:7580-7602`) builds material name
  `"C_%s_%s_S%02d_M%02d"` (race, `"LEFTEYE"`/`"RIGHTEYE"`, two numeric
  params) and calls it via method `+0x108` (a material-variant apply, distinct
  from the `+0x134`/`+0xe4` actor-attach/material-swap calls used elsewhere).
  Called from two call sites (`eqgame.exe.c:8580-8581`, `10209-10210`), both
  passing the SAME clamped byte to both the `LEFTEYE` and `RIGHTEYE` calls in
  the snippets read this pass (not fully traced to confirm `eyecolor1` vs
  `eyecolor2` map to genuinely independent calls elsewhere — **inferred, not
  fully confirmed**, that both eyes might get tied to one wire field in some
  code paths). Grepped the full string table for `LEFTEYE`/`RIGHTEYE`/`_EYE_S`
  — **zero hits** in `globalhum_chr.wld`. No `C_HUM_LEFTEYE_S*_M*` variant
  materials exist. **With stock RoF2 data, `eyecolor1`/`eyecolor2` have no
  visual effect on HUM — every character renders the same fixed blue eye from
  `chr_eye001.dds`.**

### Reconciling the owner's "shaped hair you pick in char creation" — found the actual source of that impression

`~/Games/rof2/Resources/playercustomization.txt` (the character-creation
screen's data-driven customization-limits table; format
`RACE^PARENT_ID^NAME^BASE_COLOR^CLASS_LIST^COLOR_LIST^NUM_FACES^NUM_HAIR_STYLES^NUM_EYES^NUM_BEARDS^NUM_TATTOOS^NUM_FACIAL_ATTACHMENTS^SEX`)
row 2 (RACE=1 Human, SEX=0 male): `NUM_FACES=8 NUM_HAIR_STYLES=8 NUM_EYES=12
NUM_BEARDS=16`. **The char-creation UI genuinely and correctly presents a
working "8 hairstyles / 16 beards / 12 eye colors" selector for Human male —
this is a real, data-driven, functioning UI control**, which fully explains
why the owner recalls "picking shaped hair." **But** per Findings #1–#4 above,
cycling that selector's value has **no rendering effect** on this install's
stock Luclin HUM assets — no hairstyle/beard mesh exists to attach, and no
eyecolor material variant exists to swap to. The two things are not in
conflict: the *picker* is real and functional as a UI widget; the *visual
result* of the pick is asset-dependent and, for HUM specifically, absent.

**This is the most likely reconciliation, but is INFERENCE, not proof** — it
assumes the owner's memory of "picking hair" refers to using the selector
control, not to having specifically confirmed the rendered head visibly
changes shape between hairstyle values for Human male. **Recommended cheap
follow-up before trusting this fully:** in the native client, open Human Male
char creation, and screenshot the head at `hairstyle=0` vs `hairstyle=4` (or
any two values) with the SAME face/haircolor — if genuinely nothing changes,
this reconciliation is confirmed; if something visibly changes, there is
still-undiscovered hair-mesh data (wrong archive/patch searched, or a
different install layer such as a Resources override not covered by this
scan) and the "dead code" conclusion needs to be revisited.

### What this means for the reported #519 symptoms

- **Bald scalp**: authentic to stock RoF2 Luclin HUM assets (Findings #1) —
  NOT a bug to "fix" by adding hair; if eqoxide should look like native, bald
  is correct for HUM male with these assets. (Native visibly having hair, if
  confirmed by a fresh screenshot, would falsify Finding #1 and require
  re-opening the search — see follow-up above.)
- **No mustache**: authentic to stock RoF2 Luclin HUM assets — beard mesh
  attach is dead code (Finding #1/#2), and no beard geometry exists in
  `HUM_DMSPRITEDEF` either (no vertices bind to `HUMHEBEARD1/2_DAG`).
- **Googly/bulging separate eyeballs**: the ONE symptom in this bug report
  that is **NOT explained by asset content** — the raw eye mesh is small
  (19 verts, ~4.6% of body radius) and rigidly single-bone-bound (Finding #4).
  This points at an eqoxide-side placement/scale bug: either the eye mesh's
  bone-local origin/transform isn't being applied (so it renders at model
  origin, unposed, and gets shoved by the renderer's centering logic — see
  `eqoxide_asset_server/src/convert/mod.rs:243-249`, which explicitly guards
  against exactly this failure mode ("skip eye meshes only when they could
  NOT be posed... an unposed one sits at the origin and gets misaligned") in
  the **unskinned** conversion path `convert_s3d_to_glb` — but that guard does
  **not exist** in `convert_s3d_to_glb_skinned` (`mod.rs:1296`), which is the
  path that presumably produces the actual in-game animated player model. If
  the skinned path's generic per-vertex bone-skinning does not correctly seat
  this specific rigid single-bone mesh (e.g. missing bind-pose inverse, wrong
  bone-local origin, or a scale/FPSCALE handling difference between the
  skinned and unskinned paths), that would produce exactly the reported
  "oversized separate bulging spheres" symptom. This is as far as
  asset-ground-truth investigation can go — confirming/fixing this requires
  reading eqoxide's own model-loading/rendering code, out of this agent's
  scope.

## Related
- `hair-color.md` — tint table dump (correct); its "material name = hairstyle"
  reading of `%sHE%02d%d1_MDF` is superseded by this doc (the digit is FACE).
  Covers the CLASSIC `global_chr.s3d` `humhe*` materials, a different archive
  than the Luclin findings in this file.
- `classic-vs-luclin-model-selection.md` — **disputed** as of 2026-07-17 (see
  banner at top of that file); this file's Finding #4 (separate Luclin eye
  meshes exist and match the owner's description) is the key counter-evidence.
- `eqg-character-models.md` — PFS/EQG container format sections remain valid.
