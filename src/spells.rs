//! Spell data: parse the EQ `spells_us.txt` (caret-delimited) into id→{name,icon} and
//! map an icon index to a sprite-sheet cell. Used to label/icon the memorized spell gems.

use std::collections::HashMap;

pub const ICON_COLS: usize = 6; // per-sheet grid; confirmed visually in Task 9
pub const ICON_ROWS: usize = 6;
const ICONS_PER_SHEET: usize = ICON_COLS * ICON_ROWS;

pub struct SpellInfo {
    pub name: String,
    pub icon_id: u32,
}

#[derive(Default)]
pub struct SpellDb {
    by_id: HashMap<u32, SpellInfo>,
}

impl SpellDb {
    pub fn empty() -> Self { Self::default() }

    /// Load from a `spells_us.txt` path. Missing/unreadable file → empty db (graceful).
    pub fn load(path: &str) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse_str(&text),
            Err(e) => {
                eprintln!("spells: could not read {path}: {e} (gems will show no name/icon)");
                Self::empty()
            }
        }
    }

    pub fn parse_str(text: &str) -> Self {
        let mut by_id = HashMap::new();
        for line in text.lines() {
            let cols: Vec<&str> = line.split('^').collect();
            if cols.len() < 145 { continue; }
            let id: u32 = match cols[0].trim().parse() { Ok(n) if n != 0 => n, _ => continue };
            let name = cols[1].trim().to_string();
            let icon_id = cols[144].trim().parse().unwrap_or(0);
            by_id.insert(id, SpellInfo { name, icon_id });
        }
        Self { by_id }
    }

    pub fn get(&self, id: u32) -> Option<&SpellInfo> { self.by_id.get(&id) }
}

/// Flat 1-based icon index → (sheet, col, row). icon 0 is treated as index 1.
pub fn icon_cell(icon_id: u32) -> (usize, usize, usize) {
    let idx0 = (icon_id.max(1) - 1) as usize;
    let sheet = idx0 / ICONS_PER_SHEET;
    let cell = idx0 % ICONS_PER_SHEET;
    (sheet, cell % ICON_COLS, cell / ICON_COLS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_caret_lines_builds_id_to_name_and_icon() {
        // Synthetic spells_us.txt: col0=id, col1=name, col144=new_icon.
        // Build a line with 150 caret-separated fields.
        let mut f = vec!["0"; 150];
        f[0] = "200"; f[1] = "Minor Healing"; f[144] = "35";
        let line_a = f.join("^");
        let mut g = vec!["0"; 150];
        g[0] = "5"; g[1] = "Cloak"; g[144] = "138";
        let line_b = g.join("^");
        // A zero-id line must be skipped.
        let mut z = vec!["0"; 150];
        z[1] = "Skip Me";
        let line_z = z.join("^");
        let text = format!("{line_a}\n{line_z}\n{line_b}\n");

        let db = SpellDb::parse_str(&text);
        assert_eq!(db.get(200).map(|s| s.name.as_str()), Some("Minor Healing"));
        assert_eq!(db.get(200).map(|s| s.icon_id), Some(35));
        assert_eq!(db.get(5).map(|s| s.name.as_str()), Some("Cloak"));
        assert!(db.get(0).is_none(), "zero-id lines are skipped");
    }

    #[test]
    fn icon_cell_maps_flat_index_to_sheet_col_row() {
        // 6x6 = 36 per sheet. icon 1 -> sheet0 col0 row0; icon 36 -> sheet0 col5 row5;
        // icon 37 -> sheet1 col0 row0.
        assert_eq!(icon_cell(1), (0, 0, 0));
        assert_eq!(icon_cell(36), (0, 5, 5));
        assert_eq!(icon_cell(37), (1, 0, 0));
    }
}
