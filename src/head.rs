//! Hair/beard color tint table — **Luclin-head-only**, and only for the race/gender
//! subset the real RoF2 client tints.
//!
//! IMPORTANT: the reference RoF2 client applies this tint ONLY to Luclin head models of
//! **male** High Elf (5), Dark Elf (6), Half Elf (7) and **female** Dwarf (8). Every other
//! race/gender combo's painted Luclin hair renders the authored texels untinted, and classic
//! `humhe*` heads bake hair color into the face texture (the `haircolor` byte is never read
//! there).
//!
//! eqoxide used to tint ALL races' hair prims ("product decision so haircolor is visible",
//! PR #114) — that is exactly what produced #519's dark "raccoon-mask" band and near-black
//! scalp on HUM male (haircolor 0 multiplies the skin-toned scalp/eye-band texels by
//! RGB(46,26,12)). [`hair_tint_applies`] now gates the tint to the native client's subset;
//! see `docs/eq-technical-knowledgebase/luclin-head-faces-and-hair.md` and `hair-color.md`.

/// Hair & beard color tint table (24 entries, RGB). Multiplicative tint (texel × color / 255) on
/// the **Luclin** hair/beard material, indexed by the `haircolor`/`beardcolor` byte clamped to
/// 0–23 (≥24 → no tint).
const HAIR_TINT: [[u8; 3]; 24] = [
    [46,26,12],[67,41,22],[78,49,35],[127,81,59],[101,11,6],[185,55,20],[215,85,50],
    [139,114,30],[204,179,97],[225,221,108],[251,255,129],[253,250,201],[255,255,255],
    [222,222,222],[128,128,128],[111,134,144],[62,88,90],[41,62,64],[18,18,20],
    [201,229,253],[201,253,253],[233,201,253],[206,253,201],[85,155,72],
];

/// RGB tint for a hair/beard color index; out-of-range (≥24) → white (no tint).
pub fn hair_tint(index: u8) -> [u8; 3] {
    HAIR_TINT.get(index as usize).copied().unwrap_or([255, 255, 255])
}

/// Whether the real RoF2 client applies the `haircolor` tint to this race+gender's
/// painted Luclin hair (verified against decompiled eqgame.exe gate `FUN_0040a240` +
/// EQEmu `races.h`, `hair-color.md` "Which code path applies the tint"): **male**
/// High Elf (`HIE`), Dark Elf (`DKE`), Half Elf (`HEF`) and **female** Dwarf (`DWF`,
/// gender 1). All other race/gender combos (incl. HUM/BAR/ERU/ELF/HFL/GNM, female
/// elves, and male Dwarf) render their painted hair untinted in the native client;
/// tinting HUM was #519's raccoon-mask root cause, and tinting female elves would
/// relocate that same bug onto them.
/// `race` is the 3-letter code from `eq_race_to_code` (case-insensitive); eqoxide
/// gender convention is 0=male, 1=female (native's elf branch is "not female").
pub fn hair_tint_applies(race: &str, gender: u8) -> bool {
    let mut buf = [0u8; 3];
    let bytes = race.as_bytes();
    if bytes.len() != 3 {
        return false;
    }
    for (d, s) in buf.iter_mut().zip(bytes) {
        *d = s.to_ascii_uppercase();
    }
    (matches!(&buf, b"HIE" | b"DKE" | b"HEF") && gender == 0) || (&buf == b"DWF" && gender == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hair_tint_in_range() {
        assert_eq!(hair_tint(0), [46, 26, 12]);
        assert_eq!(hair_tint(12), [255, 255, 255]);
        assert_eq!(hair_tint(23), [85, 155, 72]);
    }

    #[test]
    fn hair_tint_out_of_range_is_white() {
        assert_eq!(hair_tint(24), [255, 255, 255]);
        assert_eq!(hair_tint(255), [255, 255, 255]);
    }

    /// The native client's tint race gate (#519 + review follow-up): only MALE
    /// HIE/DKE/HEF and FEMALE Dwarf are tinted. Female elves and HUM (either gender)
    /// must not be — tinting female elves would relocate #519's raccoon-mask bug onto
    /// them instead of fixing it.
    #[test]
    fn hair_tint_gate_matches_native_client() {
        for race in ["HIE", "DKE", "HEF"] {
            assert!(hair_tint_applies(race, 0), "male {race} is tinted");
            assert!(!hair_tint_applies(race, 1), "female {race} is NOT tinted");
        }
        assert!(hair_tint_applies("DWF", 1), "female dwarf is tinted");
        assert!(!hair_tint_applies("DWF", 0), "male dwarf is NOT tinted");
        for race in ["HUM", "BAR", "ERU", "ELF", "HFL", "GNM", "TRL", "OGR", "IKS", "VAH", "FRG", "DRK"] {
            for g in [0u8, 1, 2] {
                assert!(!hair_tint_applies(race, g), "{race} gender {g} must not be tinted");
            }
        }
        // case-insensitive; junk is untinted
        assert!(hair_tint_applies("dke", 0));
        assert!(!hair_tint_applies("", 0));
        assert!(!hair_tint_applies("HUMX", 0));
    }
}
