# Hair Color — RoF2 Classic Head Models

> **2026-07-01 note:** the tint table below is correct and in use, but this doc's
> reading of `"%sHE%02d%d1_MDF"` as *hairstyle* selection is superseded — the digit
> is the **FACE** index (see `luclin-head-faces-and-hair.md`). Where the text says
> `hairstyle / 10` and `hairstyle % 10`, read `face`.

## Wire field layout

`haircolor` is an independent `uint8` in every appearance-bearing struct. Offsets below are from the
open-source EQEmu server's RoF2 patch structs
([github.com/EQEmu/Server](https://github.com/EQEmu/Server), `common/patches/rof2_structs.h`):

| Struct | File | Location |
|--------|------|----------|
| `Spawn_Struct` (variable-length) | `rof2_structs.h:452` | after `curHp` |
| `PlayerProfile_Struct` | `rof2_structs.h:1125` | `+0x888` |
| `FaceChange_Struct` | `rof2_structs.h:2674` | byte 0 |
| `CharacterSelectEntry_Struct` | `rof2_structs.h:257` | `HairColor` field |

Range is 0–23 (24 values). Values ≥ 24 are clamped/discarded by the client (no tint applied).

---

## Client-side hair color tint table

This is the 24-entry RGB tint palette the client applies to Luclin hair/beard materials. eqoxide
ships the same values as `HAIR_TINT` in `src/head.rs`.

**24-entry RGB table (format 0x00RRGGBB):**

| Index | Hex | RGB | Description |
|-------|-----|-----|-------------|
| 0 | 0x002E1A0C | (46, 26, 12) | Very dark brown |
| 1 | 0x00432916 | (67, 41, 22) | Dark brown |
| 2 | 0x004E3123 | (78, 49, 35) | Medium brown |
| 3 | 0x007F513B | (127, 81, 59) | Light brown |
| 4 | 0x00650B06 | (101, 11, 6) | Dark red |
| 5 | 0x00B93714 | (185, 55, 20) | Red |
| 6 | 0x00D75532 | (215, 85, 50) | Auburn |
| 7 | 0x008B721E | (139, 114, 30) | Dark golden |
| 8 | 0x00CCB361 | (204, 179, 97) | Blonde |
| 9 | 0x00E1DD6C | (225, 221, 108) | Light blonde |
| 10 | 0x00FBFF81 | (251, 255, 129) | Very light blonde |
| 11 | 0x00FDFAC9 | (253, 250, 201) | Near-white blonde |
| 12 | 0x00FFFFFF | (255, 255, 255) | White |
| 13 | 0x00DEDEDE | (222, 222, 222) | Light gray |
| 14 | 0x00808080 | (128, 128, 128) | Gray |
| 15 | 0x006F8690 | (111, 134, 144) | Steel blue-gray |
| 16 | 0x003E585A | (62, 88, 90) | Dark teal |
| 17 | 0x00293E40 | (41, 62, 64) | Very dark teal |
| 18 | 0x00121214 | (18, 18, 20) | Near-black |
| 19 | 0x00C9E5FD | (201, 229, 253) | Light blue |
| 20 | 0x00C9FDFD | (201, 253, 253) | Cyan |
| 21 | 0x00E9C9FD | (233, 201, 253) | Light purple |
| 22 | 0x00CEFDC9 | (206, 253, 201) | Light green |
| 23 | 0x00559B48 | (85, 155, 72) | Green |

**Tint format:** multiplicative RGB — texel × (color / 255.0). The alpha byte is always 0x00 (the
alpha channel is not used). `beardcolor` uses the same table.

---

## Which code path applies the tint

The head material-swap path receives the character appearance object, current hairstyle/face, and
gates the tint on race and gender.

### Race/model gate

The race/gender gate resolves to one of three cases:
- **no Luclin / no tint** — race 4, 9, 10, 13+, etc.
- **classic tintable but NOT in the Luclin subset** — Human=1, Barb=2, Erudite=3, male Dwarf=8,
  Halfling=11, Gnome=12.
- **Luclin-style hair tint eligible** — High Elf=5, Dark Elf=6, Half Elf=7, female Dwarf=8.

**Tint is applied ONLY when all of these hold:**
1. The race is in the Luclin-eligible subset (High Elf, Dark Elf, Half Elf, or female Dwarf).
2. The hairstyle-related flag is non-zero.
3. `haircolor < 24`.
4. The **Luclin head model is actually loaded** (a classic head model fails this gate outright, so
   no tint is applied to classic heads).

---

## Classic head textures (humhe*) — the definitive answer

### WLD material structure (`global_chr.s3d / global_chr.wld`)

All `HUMHE{N}{V}_MDF` material fragments (type `0x30`) have:
- `rgb_pen = 0x00B2B2B2` (neutral gray — same for all 16 materials across 8 hairstyles × 2 gender variants)
- No per-vertex colors: `color_count = 0` in all `HUMHE{N}*_DMSPRITEDEF` mesh fragments

Fragment names: `HUMHE0001_MDF` through `HUMHE0072_MDF`.

Material selection builds the name via the format `"%sHE%02d%d1_MDF"` where:
- `%s` = race prefix (`"HUM"`, `"BAR"`, etc.)
- `%02d` = `hairstyle / 10` (always `00` for hairstyles 0–7)
- `%d` = `hairstyle % 10` (0–7)

**haircolor is NOT part of the material name.** It is not used for texture selection.

### Head mesh groups

| Mesh | Vertices | Faces | Material groups (face count : palette index) |
|------|----------|-------|----------------------------------------------|
| HUMHE00_DMSPRITEDEF | 122 | 68 | 66:idx16, 2:idx17 |
| HUMHE01_DMSPRITEDEF | 137 | 74 | 34:idx16, 40:idx18 |
| HUMHE02_DMSPRITEDEF | 436 | 239 | 162:idx19, 24:idx20, 51:idx16, 2:idx17 |
| HUMHE03_DMSPRITEDEF | 207 | 128 | 24:idx16, 104:idx21 |

All reference `HUM_MP` (material palette fragment 11964), which has `material_count=0` in the WLD (the palette is populated at runtime by the engine).

### Pre-baked texture analysis

Textures are 128×64, 8-bit paletted BMP. Hair is PRE-COLORED (warm brown), not grayscale:
- `humhe0001.bmp`: hair area avg RGB ≈ (58, 40, 19) — dark brown
- `humhe0031.bmp`: hair area avg RGB ≈ (110, 90, 50) — medium golden-brown
- `humhe0071.bmp`: hair area avg RGB ≈ (14, 13, 12) — near-black

The textures do NOT use a white/gray hair region that would require runtime tinting.

### Conclusion for Human classic heads

**Confirmed** (from the WLD/BMP data above, independent of any client-code trace): for Human race
(race ID 1) using classic `humhe*` textures, the `haircolor` byte has NO visual effect:
- Human is not in the Luclin-eligible race subset, so the tint condition fails.
- A classic head model fails the "Luclin head loaded" gate, an additional guard.
- Hair is baked into the texture; the `hairstyle`/face index alone selects the texture.

The same applies to Barbarian (2), Erudite (3), male Dwarf (8), Halfling (11), Gnome (12) — none are
in the Luclin-eligible subset.

**Races that DO use the tint table:** High Elf (5), Dark Elf (6), Half Elf (7), female Dwarf (8) —
only when the Luclin head model is loaded.

---

## Texture/material naming reference

```
HUMHE{tens}{ones}{variant}_MDF
       ^^^  ^^^  ^^^^^^^
       00   0-7  1=male, 2=female
```

`hairstyle / 10` = `tens` (always 0 for human hairstyles 0–7)
`hairstyle % 10` = `ones`

Underlying bitmap: `humhe{tens}{ones}{variant}.bmp` — e.g. `humhe0031.bmp` = hairstyle 3, male.

---

## Related topics

- `spawn-struct.md` — wire layout for haircolor/hairstyle fields in Spawn_Struct
- `equipment-textures.md` — material slot numbering (this file covers head-specific slots 16–21 in HUM_MP)
