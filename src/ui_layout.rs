//! Persisted, movable/resizable HUD window layout. Owns per-character window
//! geometry (position/size/alpha) and the `managed_window` wrapper (see
//! `managed_window.rs` section of this module) that replaces the hard-anchored
//! `egui::Window` calls in `hud.rs`. See docs/superpowers/specs/2026-06-23-ui-position-adjustments-design.md.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Per-window persisted state. `None` pos/size means "use the spec default".
#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Debug)]
pub struct WinState {
    pub pos:   Option<[f32; 2]>,
    pub size:  Option<[f32; 2]>,
    #[serde(default = "full_alpha")]
    pub alpha: u8,
}
fn full_alpha() -> u8 { 255 }

/// On-disk form (only the persistent bits).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Persisted {
    #[serde(default)]
    locked:  bool,
    #[serde(default)]
    windows: HashMap<String, WinState>,
}

pub struct UiLayout {
    pub locked:    bool,
    windows:       HashMap<String, WinState>,
    path:          PathBuf,
    dirty:         bool,
    last_save:     Instant,
    /// Runtime-only: last observed (min,size) per window, for change detection.
    observed:      HashMap<String, ([f32; 2], [f32; 2])>,
    /// Runtime-only: windows to force back to anchored default for one frame.
    pending_reset: HashSet<String>,
    reset_all:     bool,
}

/// Strip characters that are unsafe in a filename.
pub(crate) fn sanitize(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect()
}

impl UiLayout {
    pub fn load(character_name: &str) -> Self {
        let file = format!("ui_layout_{}.json", sanitize(character_name));
        Self::from_path(PathBuf::from(file))
    }

    pub fn from_path(path: PathBuf) -> Self {
        let persisted = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| match serde_json::from_str::<Persisted>(&s) {
                Ok(p) => Some(p),
                Err(e) => { eprintln!("ui_layout: ignoring corrupt {}: {e}", path.display()); None }
            })
            .unwrap_or_default();
        UiLayout {
            locked: persisted.locked,
            windows: persisted.windows,
            path,
            dirty: false,
            last_save: Instant::now(),
            observed: HashMap::new(),
            pending_reset: HashSet::new(),
            reset_all: false,
        }
    }

    pub fn win(&self, id: &str) -> Option<&WinState> { self.windows.get(id) }

    pub fn set_win(&mut self, id: &str, ws: WinState) {
        if self.windows.get(id) != Some(&ws) {
            self.windows.insert(id.to_string(), ws);
            self.dirty = true;
        }
    }

    pub fn alpha_of(&self, id: &str) -> u8 { self.windows.get(id).map(|w| w.alpha).unwrap_or(255) }

    pub fn set_alpha(&mut self, id: &str, alpha: u8) {
        let e = self.windows.entry(id.to_string())
            .or_insert(WinState { pos: None, size: None, alpha });
        if e.alpha != alpha { e.alpha = alpha; self.dirty = true; }
    }

    pub fn reset(&mut self, id: &str) {
        self.windows.remove(id);
        self.observed.remove(id);
        self.pending_reset.insert(id.to_string());
        self.dirty = true;
    }

    pub fn reset_all(&mut self) {
        self.windows.clear();
        self.observed.clear();
        self.reset_all = true;
        self.dirty = true;
    }

    pub fn is_reset_pending(&self, id: &str) -> bool {
        self.reset_all || self.pending_reset.contains(id)
    }

    pub fn end_frame(&mut self) {
        self.pending_reset.clear();
        self.reset_all = false;
    }

    // Runtime observed-rect helpers (used by managed_window).
    pub(crate) fn observed(&self, id: &str) -> Option<([f32; 2], [f32; 2])> {
        self.observed.get(id).copied()
    }
    pub(crate) fn set_observed(&mut self, id: &str, min: [f32; 2], size: [f32; 2]) {
        self.observed.insert(id.to_string(), (min, size));
    }

    pub fn maybe_save(&mut self) {
        if self.dirty && self.last_save.elapsed() >= Duration::from_millis(1000) {
            self.save_now();
        }
    }

    pub fn save_now(&mut self) {
        if !self.dirty { return; }
        let persisted = Persisted { locked: self.locked, windows: self.windows.clone() };
        match serde_json::to_string_pretty(&persisted) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&self.path, s) {
                    eprintln!("ui_layout: save failed ({}): {e}", self.path.display());
                } else {
                    self.dirty = false;
                    self.last_save = Instant::now();
                }
            }
            Err(e) => eprintln!("ui_layout: serialize failed: {e}"),
        }
    }

    // ── test-only accessors ──
    #[cfg(test)] pub(crate) fn dirty(&self) -> bool { self.dirty }
    #[cfg(test)] pub(crate) fn clear_dirty_for_test(&mut self) { self.dirty = false; }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winstate_json_round_trip() {
        let ws = WinState { pos: Some([10.0, 20.0]), size: Some([300.0, 150.0]), alpha: 200 };
        let json = serde_json::to_string(&ws).unwrap();
        let back: WinState = serde_json::from_str(&json).unwrap();
        assert_eq!(ws, back);
    }

    #[test]
    fn load_missing_file_yields_defaults() {
        let l = UiLayout::load("__nonexistent_char__");
        assert!(!l.locked);
        assert_eq!(l.win("anything"), None);
        assert_eq!(l.alpha_of("anything"), 255);
    }

    #[test]
    fn load_corrupt_file_yields_defaults() {
        let path = std::env::temp_dir().join("ui_layout___corrupt__.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        // load() reads from CWD by character name; emulate by constructing via from_path.
        let l = UiLayout::from_path(path.clone());
        assert!(!l.locked);
        assert_eq!(l.win("x"), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn set_win_marks_dirty_only_on_change() {
        let mut l = UiLayout::from_path(std::env::temp_dir().join("ui_layout___dirty__.json"));
        let ws = WinState { pos: Some([1.0, 2.0]), size: None, alpha: 255 };
        l.set_win("hud", ws.clone());
        assert!(l.dirty());
        l.clear_dirty_for_test();
        l.set_win("hud", ws.clone()); // identical → no dirty
        assert!(!l.dirty());
    }

    #[test]
    fn reset_removes_state_and_flags_pending() {
        let mut l = UiLayout::from_path(std::env::temp_dir().join("ui_layout___reset__.json"));
        l.set_win("hud", WinState { pos: Some([5.0,6.0]), size: None, alpha: 100 });
        l.reset("hud");
        assert_eq!(l.win("hud"), None);
        assert!(l.is_reset_pending("hud"));
        l.end_frame();
        assert!(!l.is_reset_pending("hud"));
    }

    #[test]
    fn reset_all_flags_every_window() {
        let mut l = UiLayout::from_path(std::env::temp_dir().join("ui_layout___resetall__.json"));
        l.set_win("a", WinState { pos: Some([1.0,1.0]), size: None, alpha: 255 });
        l.reset_all();
        assert_eq!(l.win("a"), None);
        assert!(l.is_reset_pending("a"));
        assert!(l.is_reset_pending("anything_else"));
    }

    #[test]
    fn sanitize_strips_path_chars() {
        assert_eq!(sanitize("Bob"), "Bob");
        assert_eq!(sanitize("E'vil/../x"), "Evilx");
    }
}
