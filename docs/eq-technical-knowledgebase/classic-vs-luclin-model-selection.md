# Classic vs Luclin PC Model Selection (RoF2) — HUM Male Case Study

**Status: confirmed by decompile + asset extraction + side-by-side native/eqoxide
screenshots, 2026-07-17. Resolves issues #519 (head) and #520 (clothing).**

## The headline finding

The native RoF2 client (`~/Games/rof2`, account `rofcheck`/GM char `Acceptancetest`)
renders **HUM male using the CLASSIC (`global_chr.s3d`) head model**, not the Luclin
(`globalhum_chr.s3d`) model — **even though `eqclient.ini` has
`UseLuclinHumanMale=TRUE`** (`~/Games/rof2/eqclient.ini:55`). eqoxide renders the
Luclin model. This single model-family mismatch is the root cause of **both**
the head-texture bug cluster (#519: bare scalp, raccoon mask, no mustache,
googly eyes) and the clothing bug cluster (#520: tunic stops at belt, wrong
yoke color) — eqoxide is applying Luclin polygon-group/bone-weight boundaries
and Luclin's (empty) hair/beard content model to a character the native client
actually renders with the classic assets.

**This directly contradicts the ini file's literal setting** — see "Open
question" below. Do not assume `UseLuclin*=TRUE` in `eqclient.ini` is sufficient
evidence that a live RoF2 character renders as Luclin.

## Evidence

### 1. Screenshot comparison (decisive)

Captured during the Fable visual-eval job that filed #519/#520:
- Native: `/home/dhenry/.claude/jobs/a7ac6dcf/tmp/n_head.png`,
  `native_upper.png`
- eqoxide: `/home/dhenry/.claude/jobs/a7ac6dcf/tmp/e_head.png`,
  `eqoxide_upper.png`

Native shows visible flat-shaded polygon facets on the forehead/cheek, a sharp
angular hairline, and warm-brown hair + a full mustache baked as one
front-projected texture — the signature of a ~100-450-vertex classic head mesh
with a single flat BMP texture (not the smooth, higher-poly Luclin head with
separate white-sclera+iris eye meshes that eqoxide renders).

### 2. `humhe0001.bmp` / `humhe0031.bmp` (classic, `global_chr.s3d`) — direct proof

Extracted and viewed directly:
```
/home/dhenry/git/eqoxide_asset_server/target/release/wlddump extract \
  ~/Games/rof2/global_chr.s3d humhe0001.bmp /tmp/humhe0001.bmp
```
`humhe0001.bmp` (128×64, 8-bit palette BMP) is a **single front-projected face
texture with full swept-back brown hair AND a full beard/mustache baked
directly into it** — exactly matching the native screenshot. `humhe0031.bmp`
is a different face preset with an eyepatch, confirming these are discrete
**face presets** (each a complete hair+beard+face "look"), not a modular
face/hairstyle/beard system. 16 files exist:
`humhe000{1,2}` .. `humhe007{1,2}` (8 presets × variant suffix 1/2), listed via
direct PFS filename enumeration of `global_chr.s3d`.

### 3. Luclin HUM head has ZERO hair or mustache content (rules out Luclin explaining native's look)

- `globalhum_chr.s3d` head textures `humhesk{F}1.dds` for F=1..7 (extracted and
  viewed: `/tmp/humhesk{01,11,21,31,41,51,61,71}_big.png`) — **none of the 8
  face variants (F=0 base + F=1..7) paint any hair or mustache**. All show only
  bare-skin-toned face/eyebrow/nose/mouth variation. The crown-strip region
  (`humhesk02.dds`, always-fixed, not face-swapped) is also plain skin tone —
  confirmed by direct pixel sampling (band averages 134–182 R, all skin-toned,
  no distinct hair-colored band).
- String-table scan of `globalhum_chr.wld`, `global_chr.wld`, and all 7
  `gequip*.wld` archives (XOR-decoded per the documented key
  `[0x95,0x3a,0xc5,0x2a,0x95,0x7a,0x95,0x6a]`) for the actor names the client
  looks up at runtime — **zero hits** for `HEAD_HAIR`, `_HS0`/`_HS1`/`_HS2`,
  `FACIALHAIR`, `_EB0`, or any `IT1xxx_ACTORDEF`/`IT2xxx_ACTORDEF` (hair/beard
  item-attach range) except one stray `IT1036_ACTORDEF` (not a systematic
  hairstyle set). This independently reconfirms (for HUM specifically, not
  just the prior ELF-based finding) that **Luclin hairstyle/beard attach is
  dead code with stock RoF2 assets** — see `luclin-head-faces-and-hair.md`.

Therefore native's mustache + voluminous hair **cannot be produced from the
Luclin `globalhum_chr` assets under stock data**. It must be coming from the
classic `global_chr.s3d` assets shown in finding #2.

### 4. Client code: two independent attribute-set mechanisms, gated by "is Luclin loaded"

`eqgame.exe.c` (Ghidra decompile of the RoF2 2019 build):

- `FUN_0040d770` (`eqgame.exe.c:9148`) is the per-attribute dispatcher. For
  `param_3==2` (hairstyle), it calls a vtable check
  (`(**(code**)(*(int*)param_1[0x5e]+0xbc))()`, `eqgame.exe.c:9182`) — if true
  (Luclin model loaded for this actor), it calls `FUN_0040ab10` which does an
  **actor-attach lookup** `"%s_HS%2d_HEAD_HAIR"` (`eqgame.exe.c:7439`,
  attach slot 8). If false (classic model), it calls `FUN_0040aa30` which
  builds an **item name** `"IT%d"` with `value = hairstyle + 1000`
  (`eqgame.exe.c:7415`) and attaches it like an equipped item — i.e., on the
  classic model, "hairstyle" is (or was originally designed as) a literal 3D
  item prop, separate from the baked-face-texture hair we found in finding #2.
  Both of these hairstyle mechanisms are absent from stock RoF2 data (finding
  #3), so **neither model family's dedicated "hairstyle" system does anything
  with stock assets** — classic HUM's hair instead comes from the FACE-preset
  texture itself (`humhe0001.bmp` etc., finding #2), not from the hairstyle
  field.
- The equivalent beard dispatch is `param_3` else-branch → `FUN_0040ab80`
  (`"%s_FACIALHAIR_%02d"`, Luclin) / `FUN_0040aa30(beard+2000)` (classic
  `IT2000+`) at `eqgame.exe.c:9203-9220`. Also unpopulated in stock data.
- Face texture swap (regions 1/4/5, format `"%sHE%02d%d1_MDF"`) is a THIRD,
  separate mechanism (`eqgame.exe.c:9125`, inside `FUN_0040d714`) — this is
  the one documented in `luclin-head-faces-and-hair.md` / `hair-color.md` and
  applies to both model families, but on classic HUM it resolves to a
  **different mesh per face value** (`HUMHE00`..`HUMHE03`_DMSPRITEDEF, 122–436
  verts each, per `hair-color.md`), not just a texture swap — i.e. classic
  face presets can (and do) carry extra hair-silhouette geometry, unlike
  Luclin where the mesh is fixed and only the flat texture changes.
- Luclin-model gate: `FUN_0048e510`/`FUN_0048e420` (`eqgame.exe.c:100868-100944`)
  read `AllLuclinPcModelsOff` and `UseLuclin{Race}{Male|Female}` from
  `eqclient.ini`. For race=1 (Human) with `UseLuclinHumanMale=TRUE` in the ini,
  this function returns **true** (use Luclin) per a straight reading of the
  code — which is the opposite of what the screenshot evidence shows actually
  happens. See "Open question" below.

## Open question — not resolved

**Why does the live `rofcheck`/`Acceptancetest` character render classic HUM
male despite `UseLuclinHumanMale=TRUE` in `eqclient.ini`?** Static reading of
`FUN_0048e510` says the ini setting alone should select Luclin. Hypotheses
(none independently confirmed):
- A per-account/character or runtime override (e.g. a `/loadskeleton`-style
  toggle, if RoF2 retains one — not found in this decompile: no `loadskeleton`
  string hit) forces classic for this specific character.
- A different code path re-evaluates or caches the Luclin-model decision at
  spawn-load time separately from `FUN_0048e510`.
- Missing/partial Luclin asset on THIS specific install triggers a silent
  runtime fallback despite `globalhum_chr.s3d`/`.wld` parsing successfully
  offline (parse success doesn't prove the live client's loader accepts it —
  e.g. a CRC/version check inside the live loader could reject it).

**This does not block the eqoxide fix** — the fix target is "match what native
visibly renders" (classic), not "replicate the exact ini-resolution algorithm."
But if eqoxide later needs the ini-driven Luclin/classic *toggle* to be
data-driven rather than hardcoded, this gate function is the place to
implement it, and the open question should be resolved first (test on a fresh
account/character to see if the mismatch reproduces, or try toggling
`UseLuclinHumanMale=FALSE` explicitly and compare).

## Recommendation for eqoxide

1. **For HUM male (and, given the "confounding domain, repeatedly attempted"
   history on this bug, likely other playable races too — verify per-race)**,
   source the head + body model from **`global_chr.s3d`** (classic), not
   `globalhum_chr.s3d`/`globalhum_chr2.s3d` (Luclin), to match what the native
   RoF2 client actually renders for this population of characters.
2. **Head/face selection on classic HUM**: `spawn.face` (0-7) selects one of
   the mesh+texture pairs `HUMHE0{tens}{ones}1_MDF` → `humhe00{F}1.bmp`
   (`tens=face/10` always 0, `ones=face%10`), each a DIFFERENT mesh
   (`HUMHE00_DMSPRITEDEF` 122v/68f, `HUMHE01` 137v/74f, `HUMHE02` 436v/239f,
   `HUMHE03` 207v/128f per `hair-color.md`) carrying its own baked hair
   silhouette + beard/mustache/eyepatch — do NOT try to reuse the Luclin
   split-scalp/tint-hair converter path for these; treat each classic face
   value as a self-contained mesh+texture unit, no runtime haircolor tint
   (confirmed: Human is not in the haircolor-tint-eligible race set either
   way — see `hair-color.md`).
3. **`hairstyle` and `beard` wire fields have no visual effect on classic HUM**
   with stock RoF2 assets (confirmed dead: no `IT1000+`/`IT2000+` item actors
   exist). Accept on the wire, select nothing — same posture as the existing
   Luclin implementation already takes for `hairstyle`.
4. **Eyes**: classic HUM has no separate eye mesh — eyes are painted directly
   into the face texture (confirmed: `humhe0001.bmp` shows fully-painted eyes,
   no `HUMEYE_L/R` equivalent exists in `global_chr.s3d`'s classic HUM actor).
   This alone would fix the "bulging googly eyes" symptom for HUM if it's
   using the classic path, since there's no separate eye mesh to mis-scale.
5. **Clothing (#520)**: re-derive the chest/legs polygon-group boundary from
   the **classic** HUM body mesh's bone weights, not the Luclin one — the
   below-belt tunic "skirt" is very likely bound to the CHEST bone/material in
   the classic mesh topology and to the LEG bone/material in the Luclin mesh
   (unverified in this pass — the head finding strongly predicts this same
   class of bug, but the body mesh itself was not fragment-dumped/compared).
   Material slot numbering for reference: `EQEmu/common/textures.h:27-39`
   (`armorChest=1`, `armorLegs=5` — these are MATERIAL slots, not inventory
   slots; do not confuse with `invslot`).
6. **Do not blindly switch ALL races to classic** — confirm per-race whether
   native actually renders Luclin or classic before changing the converter's
   source archive; the open question above means the ini's `UseLuclin*` flags
   are not a reliable oracle for what the live client does. Cheapest
   verification: capture a native close-up screenshot per race/gender the way
   `a7ac6dcf` did, or find/inspect whatever runtime override is causing the
   ini/render mismatch and check its value per race.

## Tooling used (reusable)

- `eqoxide_asset_server`'s `wlddump` bin (`src/bin/wlddump.rs`) — already had
  an `extract <archive> <filename> <out>` mode; used to pull `.dds`/`.bmp`
  files and raw `.wld` blobs out of any S3D for direct inspection.
- Manual PFS directory parse + WLD string-table XOR decode in Python (see this
  session's `/tmp` scratch, key `[0x95,0x3a,0xc5,0x2a,0x95,0x7a,0x95,0x6a]`,
  string block starts at WLD offset 28, length = u32 at offset 20) — useful
  for a quick actor-name grep across many archives without a full fragment
  parser. `eqg-character-models.md` documents the fuller fragment chain if
  material-level (not just string-table) detail is needed.
- ImageMagick (`magick`) + PIL for DDS/BMP → PNG so the `Read` tool can view
  textures directly.

## Related
- `luclin-head-faces-and-hair.md` — the underlying "face vs hairstyle" digit
  confusion this finding builds on; independently reconfirmed for HUM here.
- `hair-color.md` — classic `humhe*` texture/material naming (now understood
  as FACE presets, not hairstyle slots) and the haircolor tint race-gate.
- `eqg-character-models.md` — PFS/WLD container format, per-race archive
  naming (`global{code}_chr[2].s3d`).
