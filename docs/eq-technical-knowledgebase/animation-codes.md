# EQ Animation Codes — WLD Track Naming and Semantics

EQ character models store animations as named tracks in WLD files. Each animation
has a 3-char code (classic models) or a 4-char code with a variant letter A/B
(Luclin models). The converter builds clip names as `<code><variant>_<label>`,
e.g. `P01A_idle_neutral`, `O01A_idle`.

## Track naming convention

Classic `global_chr.s3d`: codes are 3 chars, e.g. `C05HUMC05_HUM_TRACK`.
Luclin `globalelf_chr.s3d`: codes are 4 chars with A/B variant, e.g. `O01AELFPEBIP01_TRACK`.
The converter (`tools/src/main.rs` `gather_anims()`) detects the code length and
strips the base-track suffix to find codes.

Source: `eqoxide/tools/src/main.rs:743-828`

## L-series: Locomotion

| Code | Semantic label | Notes |
|------|---------------|-------|
| L01  | walk           |       |
| L02  | run            |       |
| L03  | jump_run       |       |
| L04  | fall           |       |
| L05  | duckwalk       |       |
| L06  | swim           |       |
| L07  | walk_back      |       |
| L08  | swim_idle      |       |
| L09  | swim           |       |

## P-series: Passive/Positional States

These are looping POSE animations for specific body states.

| Code | Semantic label | EmuAppearance / DoAnim      | Confirmed? |
|------|---------------|---------------------------|------------|
| P01  | idle_neutral  | eaStanding / "idle"=26    | Confirmed  |
| P02  | sit           | eaSitting / "sit"=38      | Confirmed  |
| P03  | crouch        | eaCrouching / "crouch"=24 | Confirmed  |
| P04  | (unlabeled)   | ??? possibly looting/freeze | Inferred |
| P05  | (unlabeled)   | ??? possibly lying/dead   | Inferred   |
| P06  | kneel         | "kneel"=36/62             | Confirmed  |
| P07  | swim_idle     | —                         | Inferred (Luclin addition) |
| P08  | (unlabeled)   | Luclin addition; appears in simple creature registration | Inferred |
| P09  | (unlabeled)   | Luclin addition           | Inferred   |

EmuAppearance states and their Animation:: values (from `EQEmu/common/eq_constants.h:68-75`):
- Standing=100, Freeze=102, Looting=105, Sitting=110, Crouching=111, Lying=115

## O-series: Object/Special-State Animations

**Critical: O01 and O02 are standing idle fidgets. O03 is NOT a standing idle.**

| Code | Semantic | DoAnim ID | Height (empirical) | Confirmed? |
|------|----------|-----------|-------------------|------------|
| O01  | idle fidget 1 ("standby")  | 17 | ≈3.6 (full standing) | Confirmed  |
| O02  | idle fidget 2 ("standby2") | 18 | ≈3.6 (full standing) | Confirmed  |
| O03  | low/crouched special pose  | none | ≈0.76 (near seated) | Confirmed NOT standing |

Evidence:
- EQEmu `animations` map `"standby"=17, "standby2"=18` (no "standby3") in
  `EQEmu/zone/dialogue_window.h:356-357` — only two server-triggered standby anims
- `EQGraphicsDX9.dll.c:48225-48246` (case param_3==0x9d = humanoid model type):
  animation list is P01, O01, O02, D01-D06 — **O03 is NOT in this list**
- User empirical measurement: O03A max joint height ≈0.76 vs P01A≈3.6, P02A≈0.65
  This proves O03 is always a low/crouched pose, not a standing fidget
- Classic `global_chr.s3d` only has O01 (not O02/O03) — one classic idle fidget

O03 is likely the "looting/searching" crouch pose (Animation::Looting=105 =
bent over a corpse). The height ≈0.76 is consistent with a forward-bent stance.
This is inferred — not directly confirmed in the stripped eqgame.exe.

**The converter BUG**: `anim_label()` in `tools/src/main.rs:850` maps
`"O01" | "O02" | "O03" => "idle"` — this causes O03 to be included in
`idle_fidget_clips()` via the `n.contains("idle")` filter, which is wrong.

## C-series: Combat animations

C01..C09 (classic has up to C11). Some codes (C07, C08) have both A and B variants
in Luclin. Classic has C11 (additional melee).

## D-series: Damage and Death

D01..D05 = hit/damage animations (A and B variants in Luclin).
D05 = death animation (confirmed by `anim_label("D05") = "death"`).
D06 = exists in humanoid player registration (0x9d case).

Source: `EQGraphicsDX9.dll.c:48255-48296` (param_3==0x9d case registers D01-D06).

## S-series: Social animations

S01..S04 (classic), S01..S29 (Luclin elf). These are emote/social animations
triggered by /emote commands.

## T-series: Emote animations

T01..T06 (Luclin elf), T02..T09 (classic).

---

## Native client idle animation selection

### Simple creature model type 0x9d (humanoid)
`FUN_1003cb60` in `EQGraphicsDX9.dll.c:48225-48296` registers exactly:
`P01, O01, O02, D01, D02, D03, D04, D05, D06`

For idle cycling, the client:
1. Plays P01 (neutral standing) as the looping base
2. Randomly picks O01 or O02 as a periodic "fidget" using `_rand()` to compute delay
3. Returns to P01 after the fidget completes

The random timer logic is at `EQGraphicsDX9.dll.c:49319-49347`:
```c
fVar12 = (float10)(**(code **)*puStack_244)();  // animation duration
iVar5 = _rand();
fVar9 = (float)iVar5 * fVar9 * _DAT_101c7410;  // randomize delay
// ...
FUN_1003abe0(fVar9);  // set idle timer
```

### Luclin full-body models (`%s_ANIMLIST` path)
`EQGraphicsDX9.dll.c:48193-48223` — falls through to read animation list from
a WLD resource named `<model_code>_ANIMLIST`. The complete list (O01..O03, P01..P09,
L01..L09, etc.) is loaded. Which of those are played as idle fidgets is determined
by the same timer-based random selection, but the eligible set comes from the list.

The correct idle set for humanoid Luclin models is P01 + O01 + O02 only.

---

## Converter `anim_label()` — current mapping

From `eqoxide/tools/src/main.rs:834-857`:
```rust
"P01" => "idle_neutral",
"P02" => "sit",
"P03" => "crouch",
"P06" => "kneel",
"P07" => "swim_idle",
"O01" | "O02" | "O03" => "idle",   // BUG: O03 is not a standing idle
```

**Fix needed**: O03 should not map to "idle". Suggested: `"O03" => "looting"` or
`"O03" => "search"`. This will automatically exclude it from `idle_fidget_clips()`
in `anim.rs` which filters on `n.contains("idle")`.

Also P04, P05, P08, P09 are currently unlabeled (return None). Possible labels:
- P04: possibly "looting" or "freeze" — unknown
- P05: possibly "lying" or "dead_idle" — unknown
- P08, P09: Luclin-only additions — unknown

---

## Model track counts (verified from globalelf_chr.s3d)

Each code for a Luclin elf = 222 tracks = 2 variants (A+B) × 111 bones.
The root bone "ELF_TRACK" is dag[0]; each animation variant has its own
root track `O01AELFO01A_ELF_TRACK`, `O01BELFO01B_ELF_TRACK`, etc.
Source: `eqoxide s3d_to_gltf --anims /eq_assets/EQ_Files/globalelf_chr.s3d`

Generated GLB clip names (from race_elf.glb): P01A/B_idle_neutral, P02A/B_sit,
P03A/B_crouch, P04A/B (unlabeled), P05A/B (unlabeled), P06A/B_kneel,
P07A/B_swim_idle, P08A/B (unlabeled), P09A/B (unlabeled),
O01A/B_idle, O02A/B_idle, O03A/B_idle (MISLABELED — not a standing idle),
L01..L09 variants, C01..C09 variants, D01..D05 variants, S01..S29 variants,
T01..T06 variants. Total: 131 clips.
