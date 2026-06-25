# Luclin Character Model Textures: Skin + Armor Overlay System

**Confirmed against:** `globalelf_chr.s3d` / `globalelf_chr.wld` (from the original Titanium
game client, fully parsed), EQEmu's [`common/textures.h`](https://github.com/EQEmu/Server/blob/master/common/textures.h),
and observed behavior of the original Titanium game client (`eqgame.exe`).

## The Two-Layer System (Luclin/"new" models)

Luclin character models use a **two-layer multitexture** approach:

1. **Skin base layer** — named `<race><region>sk<NN>.dds`. Format: DXT1 (no alpha channel), 100%
   opaque. Rendered with MaterialDef params `0x80000001` (render method 1 = opaque). This is the
   only layer referenced in the default `ELF_MP` WLD MaterialPalette.

2. **Armor/cloth overlay layer** — named `<race><region><MM><PP>.dds` where MM = 2-digit material
   number and PP = 2-digit mesh-piece index. Format: DXT5 (has alpha channel). Rendered with
   MaterialDef params `0x80000014` (render method 20 = alpha-masked). Alpha > 0 draws armor pixels;
   alpha = 0 is transparent and the skin below shows through.

### Why the overlay files exist even for material 0

The overlay files exist for ALL material numbers, including 0 (no armor worn). For material 0,
the overlay is either:
- A **stub** (8x8 DXT5, 100% alpha-0): no cloth painted, pure skin visible. Use for: arm and leg
  pieces where the elf is bare at material 0.
- A **real partial overlay** (real DDS size, mixed opaque/transparent): cloth garment that the elf
  wears at material 0 (the basic starter clothing). Use for: chest piece 2, chest piece 3.

## Elf Female Chest Region (material 0) — Confirmed

Source: `globalelf_chr.wld` fragment analysis (WLD parsed, all 30,460 fragments).

The `ELF_MP` MaterialPalette (frag [109]) has these chest entries:

| Palette slot | MaterialDef | Method | Bitmap (WLD frag name) | Actual DDS file |
|---|---|---|---|---|
| [1] | ELFCH0001_MDF | 0x01 (opaque) | ELFCHSK01 | elfchsk01.dds — 64x32 DXT1, skin |
| [2] | ELFCH0003_MDF | 0x01 (opaque) | ELFCHSK03 | elfchsk03.dds — 256x256 DXT1, skin |
| [3] | ELFCH0002_MDF | 0x01 (opaque) | ELFCHSK02 | elfchsk02.dds — 64x128 DXT1, skin |

The WLD **default palette only contains skin textures** (`sk`). The cloth/armor overlays are loaded
by name convention outside the WLD:

| Overlay file | Size | Alpha analysis | Meaning |
|---|---|---|---|
| elfch0001.dds | 8x8 DXT5 | 100% alpha-0 | Stub — piece 1 is pure skin |
| elfch0002.dds | 64x128 DXT5 | ~54% alpha-0, rest opaque | Cloth garment over part of chest |
| elfch0003.dds | 256x256 DXT5 | ~56% alpha-0, rest opaque | Cloth garment over midriff region |

elfch0003.dds is the **midriff/torso piece**. The 56% transparent region is where skin shows
through. The 44% opaque region is the basic cloth at material 0 (starter clothing). It is NOT
meant to render as a standalone opaque texture — it is an alpha-masked overlay on top of
elfchsk03.dds.

## Arm Region (material 0) — Comparison

| Piece | Overlay file | Size | Alpha | Meaning |
|---|---|---|---|---|
| elfua (upper arm) piece 1 | elfua0001.dds | 8x8 DXT5 | 100% alpha-0 | Stub — pure skin |
| elfua (upper arm) piece 2 | elfua0002.dds | 8x8 DXT5 | 100% alpha-0 | Stub — pure skin |
| elffa (forearm) piece 1 | elffa0001.dds | 8x8 DXT5 | 100% alpha-0 | Stub — pure skin |
| elffa (forearm) piece 2 | elffa0002.dds | 8x8 DXT5 | 100% alpha-0 | Stub — pure skin |

Arms at material 0 are 100% pure skin — all overlay stubs. The eqoxide "reject transparent
stub" arm fix is correct: those 8x8 stubs should be skipped and baked skin rendered instead.

## Material 1+ (armor/leather) 

For material 1 (leather), the WLD MaterialDef still references the SAME skin bitmaps (e.g.
ELFCH0101_MDF -> ELFCHSK01), but with params = `0x80000014` instead of `0x80000001`. The overlay
file is e.g. elfch0101.dds (leather chest piece 1). The skin layer still renders first; the armor
overlay is drawn on top with alpha-masking.

## The Diagnostic Signals

For identifying "stub vs real overlay" robustly:

1. **Size**: 8x8 DXT5 = unconditional stub (zero useful content). This covers elfch0001 and all
   arm/leg pieces at material 0 for elf. Size <= 64 bytes total is a safe proxy.
2. **Alpha-0 fraction (fallback)**: A DXT5 texture that is 100% alpha-0 is a stub.
3. **Fragment flags**: WLD Bitmap fragments with `flags=1` and zero filenames are the skin base
   textures (sk); the overlay textures (0001, 0002, etc.) are loaded by name outside the WLD.

The problematic case (elfch0003.dds) is NOT a stub — it is a real 256x256 DXT5 with real cloth
pixels. The correct fix is not to reject it, but to render it as an alpha-masked overlay on top of
the skin base (elfchsk03.dds).

## Recommended Rendering for eqoxide

For any Luclin character mesh piece at any material:

1. Identify the **skin base** texture: name `<prefix>sk<NN>.dds`. This is always a DXT1 opaque
   texture. Render it first, opaque.
2. Load the **overlay** texture: name `<prefix><MM><PP>.dds`.
   - If it is an 8x8 DXT5 stub (192 bytes, all alpha-0): skip — skin renders alone.
   - Otherwise: render it on top of the skin using alpha-masked blending
     (where DXT5 alpha channel > 0, draw the RGB; where alpha == 0, discard the fragment).
3. The "baked skin fallback" used for arms is correct for stub detection but is the wrong
   abstraction for the chest: the chest does have real skin — it's elfchsk03.dds.

## WLD Fragment Naming Convention (Luclin)

All Fragment03 (Bitmap) entries in `globalelf_chr.wld` follow a single naming convention:
- `ELFCHSKnn` — skin base for chest piece nn (DXT1, opaque, loaded by WLD bitmap name = fragment name)
- The overlay files (`elfch00nn.dds`) are NOT in the WLD; they are loaded by filename only.

Confirmed: `globalelf_chr.wld` fragment analysis, 30,460 fragments parsed.
ELF_MP MaterialPalette contains 27 materials; ALL body material entries reference sk textures only.

## References

- `globalelf_chr.s3d` (from the original Titanium game client) — all elf textures
- Behavior from the original Titanium game client (`eqgame.exe`) — classic equipment/model rendering
- OpenEQ's `LegacyFileReader/Wld.cs` — WLD format reference (string hash key, fragment layout)
- s3d (PFS) archive listing/extraction — standard PFS container format used by the original Titanium game client
