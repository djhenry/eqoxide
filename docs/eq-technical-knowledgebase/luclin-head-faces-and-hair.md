# Luclin Heads: Faces, Painted Hair, and the Dead Hairstyle Path (RoF2)

**Status: validated 2026-07-01 against the RoF2 decompile (`eqgame.exe.c`), the RoF2
asset archives, and bone-weight analysis of the baked GLBs. SUPERSEDES the hair
sections of `eqg-character-models.md` and the `luclin-character-hair.md` worktree doc,
both of which misread the `hesk` texture digit as a HAIRSTYLE index. It is the FACE
index.**

## The three head attributes and what the client does with them

`FUN_0040d770` (eqgame.exe.c:9150) is the attribute dispatcher for S3D character
heads. Its `param_3` selects the attribute:

| attr | wire field | mechanism |
|------|-----------|-----------|
| 1 | `face` | **material swap** `"%sHE%02d%d1_MDF"` via `FUN_0040d1a0` (texture change) |
| 2 | `hairstyle` | **actor attach** at slot 8: Luclin heads look up `"%s_HS%2d_HEAD_HAIR"` (`FUN_0040ab10`); classic heads attach `IT{1000+style+race_offset}` (`FUN_0040aa30`, offsets in `FUN_0040a290`) |
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

Exhaustive scan of every `.s3d` WLD string table in `~/eq_assets/everquest_rof2`:
**no `*_HEAD_HAIR` actor exists anywhere**, and no `IT13xx/IT14xx` hair actors
exist in `global_chr.s3d`/`gequip*.s3d`. The `{RACE}HEHAIR1–9_DAG` skeleton bones
are sway-chains for those (never-shipped) attachments; **zero vertices** in any
player-race body mesh bind to them. The only real hair models in RoF2 are the
Drakkin EQG ones (`dkf.eqg`/`dkm.eqg`: `dk{f,m}_hair_00..07.mod`, attached via
`"%s_HAIR_%02d"`, `FUN_0040ac80`).

**Therefore: changing `hairstyle` has NO visual effect on S3D player races in the
real RoF2 client.** Bald-looking Luclin heads with painted hair are authentic.

### Haircolor — a runtime multiplicative tint over the painted hair

The hair you "see" on Luclin models is painted into the head textures as a
NEUTRAL LIGHT base (huf scalp texels avg ≈ RGB(158,116,85); elf ≈ (173,123,90) —
lighter than the face). The client colors it by multiplying with the 24-entry
tint table at VA `0x00AC1A70` (see `hair-color.md` for the table). In the swap
function the tint pointer is only passed for `FUN_0040a240()==2` races (High
Elf, Dark Elf, Half Elf, female Dwarf) — races whose region layout isolates the
hair texels; the visible result is neutral/blonde-ish hair for the others.

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
  `hair_tint(spawn.haircolor)` (>=24 → no tint). We tint ALL races (superset of
  the real client's race gate — product decision so haircolor is visible;
  haircolor 0xFF/unset renders the authentic neutral base).
- `hairstyle` is accepted on the wire but intentionally selects nothing.

## Related
- `hair-color.md` — tint table dump (correct); its "material name = hairstyle"
  reading of `%sHE%02d%d1_MDF` is superseded by this doc (the digit is FACE).
- `eqg-character-models.md` — PFS/EQG container format sections remain valid.
