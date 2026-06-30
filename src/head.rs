//! Hair/beard color tint table — **Luclin-head-only**, kept here as verified ground truth.
//!
//! IMPORTANT: the real RoF2 client does NOT apply this tint to classic `humhe*` heads (the
//! models eqoxide currently renders). Per the decompiled client (`FUN_0040d1a0` @ VA 0x0040d1a0):
//! the tint fires only when the Luclin-head flag (`[model+0x34]`) is set AND the race gate
//! (`FUN_0040a240`) returns 2 — i.e. only High Elf (5), Dark Elf (6), Half Elf (7) and female
//! Dwarf. For classic heads, hair color is 100% baked into the 8 `hairstyle` textures
//! (`humhe00{N}{1|2}`); the `haircolor` wire byte is never read on that path. So this table is
//! dormant until Luclin head models are implemented — do not apply it to classic heads.

/// Hair & beard color tint table from eqgame.exe (data @ VA 0x00AC1A70, file off 0x6BFC70;
/// 24 × DWORD `0x00RRGGBB`). Multiplicative tint (texel × color / 255) on the **Luclin** hair/
/// beard material, indexed by the `haircolor`/`beardcolor` byte clamped to 0–23 (≥24 → no tint).
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
}
