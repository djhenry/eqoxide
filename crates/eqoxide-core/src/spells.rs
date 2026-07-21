//! Spell data: parse the EQ `spells_us.txt` (caret-delimited) into id→{name,icon} and
//! map an icon index to a sprite-sheet cell. Used to label/icon the memorized spell gems.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

pub const ICON_COLS: usize = 6; // per-sheet grid; confirmed visually in Task 9
pub const ICON_ROWS: usize = 6;
const ICONS_PER_SHEET: usize = ICON_COLS * ICON_ROWS;

/// SpellTargetType (EQEmu common/spdat.h) value for a self-only spell — always lands on the caster.
pub const ST_SELF: u8 = 6;

/// Number of spell-effect (SPA) slots per spell in `spells_us.txt` — `effectid1..effectid12`,
/// caret columns 86..=97 (EQEmu `common/spdat.cpp` LoadSPDat field ordinals).
pub const SPELL_EFFECTS: usize = 12;
/// First caret column of `effectid1`.
const EFFECT_COL0: usize = 86;
/// The `effectid` value EQEmu writes for "this slot is unused" (SE_Blank). Never a real SPA.
pub const SPA_BLANK: i32 = 254;

/// SPA 57 = `SE_Levitate` — gravity off (the character free-floats). The RoF2 client learns a
/// mid-zone levitate cast on ITSELF only by cross-referencing the buff's spell id against this
/// effect id in its own spell table; the server deliberately does not send the type-19 FlyMode
/// appearance to the buff's own recipient (EQEmu `zone/spell_effects.cpp`,
/// `SendAppearancePacket(FlyMode, …, ignore_self=true)`). (#586)
pub const SPA_LEVITATE: i32 = 57;

pub struct SpellInfo {
    pub name: String,
    pub icon_id: u32,
    /// good_effect (spells_us.txt col 83): 0 = detrimental, 1 = beneficial, 2 = beneficial group-only.
    pub good_effect: i8,
    /// SpellTargetType (col 98): who the spell can be cast on (ST_SELF=6, ST_Target=5, ST_Group=41…).
    pub target_type: u8,
    /// The spell's 12 effect (SPA) ids, `effectid1..effectid12` (cols 86..=97). `254` (SPA_BLANK)
    /// means the slot is unused. This is the raw SPA list ONLY — eqoxide deliberately does not
    /// model any SPA's *behavior* here (no base/limit/max/formula decoding, no bonus math). It is
    /// the minimum needed to answer "does this spell carry effect N?", which is how the real client
    /// recognises its own buffs. (#586)
    pub effects: [i32; SPELL_EFFECTS],
}

#[derive(Default)]
pub struct SpellDb {
    by_id: HashMap<u32, SpellInfo>,
}

/// Process-global spell table, set once at startup (mirrors the eqstr table). Lets the nav thread
/// resolve a spell's target type for self-cast decisions without threading the Arc through the
/// login → ActionLoop arg chain. (eqoxide#95)
static GLOBAL: OnceLock<Arc<SpellDb>> = OnceLock::new();

/// Publish the loaded spell table globally (idempotent — first call wins).
pub fn set_global(db: Arc<SpellDb>) { let _ = GLOBAL.set(db); }

/// The global spell table, or `None` if it wasn't loaded (e.g. missing spells_us.txt).
pub fn global() -> Option<&'static Arc<SpellDb>> { GLOBAL.get() }

/// A spell's display name for the message log / event feed, e.g. "Minor Healing". Falls back to
/// `spell <id>` when the table isn't loaded or the id is unknown, and to `an unknown spell` for id
/// 0 — which is our explicit "the server never named the spell" sentinel, NOT a real spell.
/// Never invents a name. (eqoxide#348)
pub fn name_of(id: u32) -> String {
    if id == 0 || id == crate::game_state::EMPTY_GEM {
        return "an unknown spell".to_string();
    }
    global()
        .and_then(|db| db.get(id))
        .map(|s| s.name.clone())
        .unwrap_or_else(|| format!("spell {id}"))
}

impl SpellDb {
    pub fn empty() -> Self { Self::default() }

    /// A self-only spell (ST_SELF) always targets the caster, regardless of the current target.
    pub fn is_self_only(&self, id: u32) -> bool {
        self.get(id).map_or(false, |s| s.target_type == ST_SELF)
    }

    /// Beneficial spells (heals/buffs) should land on a friendly target (or self), never a mob.
    pub fn is_beneficial(&self, id: u32) -> bool {
        self.get(id).map_or(false, |s| s.good_effect != 0)
    }

    /// Load from a `spells_us.txt` path. Missing/unreadable file → empty db (graceful).
    /// Classic EQ `spells_us.txt` is Latin-1/Windows-1252 (accented spell names), NOT UTF-8, so a
    /// strict `read_to_string` bails on the first non-ASCII byte ("stream did not contain valid
    /// UTF-8"). Read raw bytes and decode as Latin-1 instead (eqoxide#7).
    pub fn load(path: &str) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => Self::parse_str(&latin1_to_string(&bytes)),
            Err(e) => {
                tracing::warn!("spells: could not read {path}: {e} (gems will show no name/icon)");
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
            // col 83 = good_effect (beneficial flag), col 98 = target_type (EQEmu spdat.h field
            // ordinals, same numbering as col144=new_icon). Default to detrimental/target on parse
            // failure so an unknown spell keeps the old current-target behavior. (eqoxide#95)
            let good_effect = cols[83].trim().parse().unwrap_or(0);
            let target_type = cols[98].trim().parse().unwrap_or(0);
            // effectid1..12 (cols 86..=97). An unparseable column becomes SPA_BLANK rather than 0:
            // 0 is a REAL spa (SE_CurrentHP), so defaulting to it would invent an effect the spell
            // does not have. SPA_BLANK is EQEmu's own "unused slot" marker. (#586)
            let mut effects = [SPA_BLANK; SPELL_EFFECTS];
            for (i, e) in effects.iter_mut().enumerate() {
                *e = cols[EFFECT_COL0 + i].trim().parse().unwrap_or(SPA_BLANK);
            }
            by_id.insert(id, SpellInfo { name, icon_id, good_effect, target_type, effects });
        }
        Self { by_id }
    }

    pub fn get(&self, id: u32) -> Option<&SpellInfo> { self.by_id.get(&id) }

    /// Does spell `id` carry SPA `spa` in any of its 12 effect slots?
    ///
    /// **Three-valued on purpose (agent-honesty, #586).** `Some(true)`/`Some(false)` are answers we
    /// can back with the loaded `spells_us.txt`; `None` means *we do not know* — the table is not
    /// loaded, or this id is not in it. Callers MUST NOT collapse `None` into `false`: "we have no
    /// row for this spell" is not evidence the spell lacks the effect, and treating it as such would
    /// let an unknown spell silently overwrite a known-true client state.
    pub fn has_effect(&self, id: u32, spa: i32) -> Option<bool> {
        self.get(id).map(|s| s.effects.contains(&spa))
    }
}

/// [`SpellDb::has_effect`] against the process-global table, or `None` when no table is loaded.
/// Same three-valued contract: `None` = unknown, never "no".
pub fn has_effect(id: u32, spa: i32) -> Option<bool> {
    global().and_then(|db| db.has_effect(id, spa))
}

/// Decode bytes as Latin-1 (ISO-8859-1): each byte 0x00–0xFF maps to the identical Unicode
/// codepoint U+0000–U+00FF. Lossless for Latin-1 content and never fails — unlike strict UTF-8,
/// which rejects classic EQ text tables on their first accented byte.
pub fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
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
    fn parses_good_effect_and_target_type_for_self_cast() {
        // col83 = good_effect, col98 = target_type (ST_SELF=6, ST_Target=5).
        let mut heal = vec!["0"; 150];         // Minor Healing: beneficial, single-target friendly
        heal[0] = "200"; heal[1] = "Minor Healing"; heal[83] = "1"; heal[98] = "5"; heal[144] = "35";
        let mut skin = vec!["0"; 150];         // Skin like Wood: beneficial, self-only
        skin[0] = "26"; skin[1] = "Skin like Wood"; skin[83] = "1"; skin[98] = "6"; skin[144] = "10";
        let mut nuke = vec!["0"; 150];         // a detrimental nuke
        nuke[0] = "300"; nuke[1] = "Lightning Bolt"; nuke[83] = "0"; nuke[98] = "5"; nuke[144] = "9";
        let text = [heal, skin, nuke].map(|f| f.join("^")).join("\n");

        let db = SpellDb::parse_str(&text);
        assert!(db.is_beneficial(200) && !db.is_self_only(200), "heal: beneficial, not self-only");
        assert!(db.is_beneficial(26) && db.is_self_only(26), "Skin like Wood: beneficial + self-only");
        assert!(!db.is_beneficial(300) && !db.is_self_only(300), "nuke: detrimental");
        // Unknown spell → conservative (keep current-target behavior).
        assert!(!db.is_beneficial(9999) && !db.is_self_only(9999));
    }

    #[test]
    fn parses_spa_effect_ids_and_answers_has_effect_three_valued() {
        // #586. Column layout taken from the REAL spells_us.txt row for spell 261 "Levitate":
        // effectid1..3 = 10, 10, 57 (SPA 57 = SE_Levitate), remaining slots 254 (SE_Blank).
        let mut lev = vec!["0"; 150];
        lev[0] = "261"; lev[1] = "Levitate"; lev[144] = "39";
        lev[86] = "10"; lev[87] = "10"; lev[88] = "57";
        for c in lev.iter_mut().take(98).skip(89) { *c = "254"; }
        let mut clarity = vec!["0"; 150];
        clarity[0] = "174"; clarity[1] = "Gate"; clarity[144] = "12";
        clarity[86] = "83";
        for c in clarity.iter_mut().take(98).skip(87) { *c = "254"; }
        let db = SpellDb::parse_str(&[lev, clarity].map(|f| f.join("^")).join("\n"));

        assert_eq!(db.get(261).map(|s| s.effects[2]), Some(SPA_LEVITATE));
        assert_eq!(db.has_effect(261, SPA_LEVITATE), Some(true), "Levitate carries SPA 57");
        assert_eq!(db.has_effect(174, SPA_LEVITATE), Some(false), "Gate does not carry SPA 57");
        // The honesty contract: an id we have no row for is UNKNOWN, not "no".
        assert_eq!(db.has_effect(40404, SPA_LEVITATE), None, "unknown spell id → None, never Some(false)");
    }

    #[test]
    fn unparseable_effect_column_becomes_blank_not_spa_zero() {
        // SPA 0 is a real effect (SE_CurrentHP); defaulting a bad column to 0 would invent it.
        let mut f = vec!["0"; 150];
        f[0] = "700"; f[1] = "Broken Row"; f[144] = "1";
        f[86] = "";           // empty/garbage effect column
        for c in f.iter_mut().take(98).skip(87) { *c = "254"; }
        let db = SpellDb::parse_str(&f.join("^"));
        assert_eq!(db.get(700).map(|s| s.effects[0]), Some(SPA_BLANK));
        assert_eq!(db.has_effect(700, 0), Some(false), "a garbage column must not read as SPA 0");
    }

    #[test]
    fn latin1_decodes_high_bytes_without_failing() {
        // 0xE9 = é, 0xF1 = ñ in Latin-1; these are invalid UTF-8 lead bytes on their own, which is
        // exactly what made read_to_string bail on spells_us.txt (eqoxide#7).
        assert_eq!(latin1_to_string(&[0x45, 0xE9, 0x70, 0xF1]), "Eépñ");
    }

    #[test]
    fn parse_handles_latin1_accented_spell_name() {
        // A spell line whose name carries a Latin-1 byte (0xE9 = é) must decode, not blank out.
        let mut f = vec!["0"; 150];
        f[0] = "300"; f[144] = "42";
        let line = f.join("^");
        let mut bytes = line.into_bytes();
        // splice the name "Fé" into col1 (between the first and second carets)
        let first_caret = bytes.iter().position(|&b| b == b'^').unwrap();
        let second_caret = first_caret + 1 + bytes[first_caret + 1..].iter().position(|&b| b == b'^').unwrap();
        bytes.splice(first_caret + 1..second_caret, vec![b'F', 0xE9]);

        let db = SpellDb::parse_str(&latin1_to_string(&bytes));
        assert_eq!(db.get(300).map(|s| s.name.as_str()), Some("Fé"));
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
