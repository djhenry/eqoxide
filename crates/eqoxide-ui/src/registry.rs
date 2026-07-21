//! The window registry — single source of truth for every UI window.
//!
//! The Window Selector iterates this table, hotkeys route through it, and the
//! per-character layout file keys off `WindowDef::id`. Native analogs are noted
//! per entry (see docs/ui-overhaul-design.md §8).

use egui::{Align2, Key};

#[derive(Clone, Copy)]
pub struct WindowDef {
    /// Stable persistence key (never rename without a migration).
    pub id: &'static str,
    pub title: &'static str,
    /// Toggle hotkey (unmodified physical key, guarded by egui keyboard focus).
    pub hotkey: Option<Key>,
    pub default_anchor: Align2,
    pub default_offset: [f32; 2],
    pub default_size: [f32; 2],
    pub resizable: bool,
    /// `false` = no close box and cannot be closed (Window Selector).
    pub closeable: bool,
    pub default_open: bool,
    /// Gated by game state (merchant, trainer, loot, dialogs): opens/closes on
    /// its own, is not toggleable from the Selector, and its open state is not
    /// persisted.
    pub transient: bool,
}

pub const SELECTOR: &str = "selector";
pub const PLAYER: &str = "player";
pub const TARGET: &str = "target";
pub const GROUP: &str = "group";
pub const CHAT: &str = "chat";
pub const INVENTORY: &str = "inventory";
pub const MERCHANT: &str = "merchant";
pub const LOOT: &str = "loot";
pub const SPELL_GEMS: &str = "spell_gems";
pub const CASTING: &str = "casting";
pub const SPELLBOOK: &str = "spellbook";
pub const SKILLS: &str = "skills";
pub const TRAINER: &str = "trainer";
pub const PET: &str = "pet";
pub const QUEST_JOURNAL: &str = "quest_journal";
pub const NPC_DIALOGUE: &str = "npc_dialogue";
pub const ACTIONS: &str = "actions";
pub const MAP: &str = "map";
pub const COMPASS: &str = "compass";
pub const OPTIONS: &str = "options";
pub const HELP: &str = "help";

const fn def(id: &'static str, title: &'static str) -> WindowDef {
    WindowDef {
        id,
        title,
        hotkey: None,
        default_anchor: Align2::CENTER_CENTER,
        default_offset: [0.0, 0.0],
        default_size: [300.0, 200.0],
        resizable: true,
        closeable: true,
        default_open: false,
        transient: false,
    }
}

/// All windows, in draw order (later = on top within the same egui layer).
pub const REGISTRY: &[WindowDef] = &[
    WindowDef {
        hotkey: None,
        default_anchor: Align2::CENTER_TOP,
        default_offset: [0.0, 4.0],
        default_size: [460.0, 40.0],
        resizable: false,
        closeable: false, // the control panel is never closeable (requirement)
        default_open: true,
        ..def(SELECTOR, "Windows")
    },
    WindowDef {
        hotkey: None,
        default_anchor: Align2::LEFT_TOP,
        default_offset: [8.0, 40.0],
        default_size: [190.0, 120.0],
        resizable: true,
        default_open: true,
        ..def(PLAYER, "Player")
    },
    WindowDef {
        hotkey: None,
        default_anchor: Align2::LEFT_TOP,
        default_offset: [8.0, 170.0],
        default_size: [190.0, 80.0],
        resizable: true,
        default_open: true,
        ..def(TARGET, "Target")
    },
    WindowDef {
        hotkey: Some(Key::G),
        default_anchor: Align2::LEFT_TOP,
        default_offset: [8.0, 260.0],
        default_size: [190.0, 140.0],
        resizable: true,
        default_open: true,
        ..def(GROUP, "Group")
    },
    WindowDef {
        hotkey: None,
        default_anchor: Align2::LEFT_BOTTOM,
        default_offset: [8.0, -8.0],
        default_size: [440.0, 180.0],
        resizable: true,
        default_open: true,
        ..def(CHAT, "Chat")
    },
    WindowDef {
        hotkey: Some(Key::I),
        default_anchor: Align2::CENTER_CENTER,
        default_offset: [-160.0, -40.0],
        default_size: [360.0, 420.0],
        ..def(INVENTORY, "Inventory")
    },
    WindowDef {
        default_anchor: Align2::CENTER_CENTER,
        default_size: [430.0, 420.0],
        transient: true,
        ..def(MERCHANT, "Merchant")
    },
    WindowDef {
        default_anchor: Align2::CENTER_CENTER,
        default_offset: [-120.0, 0.0],
        default_size: [260.0, 280.0],
        transient: true,
        ..def(LOOT, "Loot")
    },
    WindowDef {
        hotkey: None,
        default_anchor: Align2::RIGHT_BOTTOM,
        default_offset: [-8.0, -80.0],
        default_size: [46.0, 330.0],
        resizable: false,
        default_open: true,
        ..def(SPELL_GEMS, "Spell Gems")
    },
    WindowDef {
        default_anchor: Align2::CENTER_BOTTOM,
        default_offset: [0.0, -160.0],
        default_size: [220.0, 40.0],
        resizable: false,
        transient: true,
        ..def(CASTING, "Casting")
    },
    WindowDef {
        hotkey: Some(Key::B),
        default_anchor: Align2::CENTER_CENTER,
        default_size: [380.0, 360.0],
        ..def(SPELLBOOK, "Spellbook")
    },
    WindowDef {
        hotkey: Some(Key::K),
        default_anchor: Align2::CENTER_CENTER,
        default_offset: [120.0, 0.0],
        default_size: [280.0, 380.0],
        ..def(SKILLS, "Skills")
    },
    WindowDef {
        default_anchor: Align2::CENTER_CENTER,
        default_size: [300.0, 320.0],
        transient: true,
        ..def(TRAINER, "Trainer")
    },
    WindowDef {
        hotkey: None,
        default_anchor: Align2::LEFT_TOP,
        default_offset: [8.0, 410.0],
        default_size: [170.0, 70.0],
        resizable: true,
        ..def(PET, "Pet")
    },
    WindowDef {
        hotkey: Some(Key::T),
        default_anchor: Align2::RIGHT_TOP,
        default_offset: [-8.0, 300.0],
        default_size: [340.0, 380.0],
        ..def(QUEST_JOURNAL, "Quest Journal")
    },
    WindowDef {
        hotkey: None,
        default_anchor: Align2::CENTER_TOP,
        default_offset: [0.0, 96.0],
        default_size: [440.0, 150.0],
        transient: true,
        ..def(NPC_DIALOGUE, "Dialogue")
    },
    WindowDef {
        hotkey: None,
        default_anchor: Align2::CENTER_BOTTOM,
        default_offset: [0.0, -8.0],
        default_size: [420.0, 60.0],
        resizable: false,
        default_open: true,
        ..def(ACTIONS, "Actions")
    },
    WindowDef {
        hotkey: Some(Key::M),
        default_anchor: Align2::RIGHT_TOP,
        default_offset: [-8.0, 40.0],
        default_size: [240.0, 240.0],
        default_open: true,
        ..def(MAP, "Map")
    },
    WindowDef {
        hotkey: None,
        default_anchor: Align2::CENTER_TOP,
        default_offset: [0.0, 46.0],
        default_size: [260.0, 30.0],
        resizable: false,
        ..def(COMPASS, "Compass")
    },
    WindowDef {
        hotkey: Some(Key::O),
        default_anchor: Align2::CENTER_CENTER,
        default_size: [320.0, 300.0],
        ..def(OPTIONS, "Options")
    },
    WindowDef {
        hotkey: Some(Key::H),
        default_anchor: Align2::CENTER_CENTER,
        default_size: [400.0, 380.0],
        ..def(HELP, "Help")
    },
];

pub fn get(id: &str) -> Option<&'static WindowDef> {
    REGISTRY.iter().find(|d| d.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique() {
        let mut seen = HashSet::new();
        for d in REGISTRY {
            assert!(seen.insert(d.id), "duplicate window id {}", d.id);
        }
    }

    #[test]
    fn selector_is_never_closeable_and_open_by_default() {
        let s = get(SELECTOR).unwrap();
        assert!(!s.closeable);
        assert!(s.default_open);
        assert!(!s.transient);
    }

    #[test]
    fn hotkeys_are_unique() {
        let mut seen = HashSet::new();
        for d in REGISTRY {
            if let Some(k) = d.hotkey {
                assert!(seen.insert(k), "duplicate hotkey {:?} on {}", k, d.id);
            }
        }
    }

    #[test]
    fn transients_are_closeable_and_default_closed() {
        for d in REGISTRY.iter().filter(|d| d.transient) {
            assert!(d.closeable, "{} transient must be closeable", d.id);
            assert!(!d.default_open, "{} transient must default closed", d.id);
        }
    }
}
