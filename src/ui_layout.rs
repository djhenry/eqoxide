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
            .or_insert_with(|| { WinState { pos: None, size: None, alpha: 255 } });
        if e.alpha != alpha { e.alpha = alpha; self.dirty = true; }
    }

    /// Mark dirty after a direct `locked` mutation (which bypasses set_win).
    pub fn set_dirty_locked(&mut self) { self.dirty = true; }

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

/// Static description of one managed HUD window. The anchor + offset reproduce
/// the window's default on-screen position (used on first frame / reset); after
/// that the position is owned by egui memory + the persisted `WinState`.
pub struct WindowSpec {
    pub id:             &'static str,
    pub title:          &'static str,
    pub default_anchor: egui::Align2,
    pub default_offset: [f32; 2],
    pub default_size:   Option<[f32; 2]>,
    pub resizable:      bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum PosMode { Anchor, Free }

/// Decide whether to anchor (default placement) or free-position a window.
pub(crate) fn decide_pos(
    stored: Option<&WinState>,
    observed: bool,
    locked: bool,
    reset_pending: bool,
) -> PosMode {
    if reset_pending { return PosMode::Anchor; }
    if stored.map(|s| s.pos.is_some()).unwrap_or(false) { return PosMode::Free; }
    if locked { return PosMode::Anchor; }
    if observed { PosMode::Free } else { PosMode::Anchor }
}

/// Given the window's current rect, decide the new `WinState` to persist (or
/// `None` to leave state untouched). Persists when the window has stored geometry
/// or when it is free-positioned and the rect changed from last frame.
pub(crate) fn record_change(
    stored: Option<&WinState>,
    is_free: bool,
    rect_min: [f32; 2],
    rect_size: [f32; 2],
    resizable: bool,
    observed: Option<([f32; 2], [f32; 2])>,
    alpha: u8,
) -> Option<WinState> {
    const EPS: f32 = 0.5;
    let changed = observed.map(|(o_min, o_size)| {
        (o_min[0] - rect_min[0]).abs() > EPS
            || (o_min[1] - rect_min[1]).abs() > EPS
            || (o_size[0] - rect_size[0]).abs() > EPS
            || (o_size[1] - rect_size[1]).abs() > EPS
    }).unwrap_or(false);
    let has_stored_pos = stored.map(|s| s.pos.is_some()).unwrap_or(false);
    if !(has_stored_pos || (is_free && changed)) { return None; }
    let size = if resizable { Some(rect_size) } else { None };
    Some(WinState { pos: Some(rect_min), size, alpha })
}

/// Wrap a HUD window so it is movable/resizable/persisted. Drops the hard
/// `.anchor()` that froze the window; uses the spec anchor only for first-frame
/// and reset placement.
pub fn managed_window<R>(
    ctx: &egui::Context,
    layout: &mut UiLayout,
    spec: &WindowSpec,
    base_frame: egui::Frame,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) {
    let id = spec.id;
    let locked = layout.locked;
    let reset_pending = layout.is_reset_pending(id);
    let observed = layout.observed(id);
    let stored = layout.win(id).cloned();
    let alpha = layout.alpha_of(id);

    let mode = decide_pos(stored.as_ref(), observed.is_some(), locked, reset_pending);

    let frame = base_frame.multiply_with_opacity(alpha as f32 / 255.0);

    let mut win = egui::Window::new(spec.title)
        .title_bar(false)
        .collapsible(false)
        // `constrain(true)` is our off-screen guard: egui keeps the window within
        // screen_rect each frame, so we don't separately clamp stored positions.
        .constrain(true)
        .frame(frame)
        .movable(!locked)
        .resizable(spec.resizable && !locked);

    match mode {
        PosMode::Anchor => {
            win = win.anchor(spec.default_anchor,
                crate::hud::canvas_off(ctx, spec.default_anchor, spec.default_offset));
        }
        PosMode::Free => {
            if let Some(p) = stored.as_ref().and_then(|s| s.pos) {
                win = win.default_pos(egui::pos2(p[0], p[1]));
            } else if let Some((m, _)) = observed {
                win = win.default_pos(egui::pos2(m[0], m[1]));
            }
            if let Some(sz) = stored.as_ref().and_then(|s| s.size).or(spec.default_size) {
                win = win.default_size(egui::vec2(sz[0], sz[1]));
            }
        }
    }

    let resp = win.show(ctx, |ui| {
        ui.set_opacity(alpha as f32 / 255.0);
        // Drag/affordance strip + per-window menu, only when unlocked.
        if !locked {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("\u{2630} {}", spec.title))
                    .size(10.0).weak());
            });
        }
        add_contents(ui)
    });

    if let Some(resp) = resp {
        // Right-click context menu: opacity slider, reset, global lock.
        let mut new_alpha = alpha;
        let mut do_reset = false;
        let mut toggle_lock = false;
        resp.response.context_menu(|ui| {
            ui.label(spec.title);
            ui.add(egui::Slider::new(&mut new_alpha, 40..=255).text("Opacity"));
            if ui.button("Reset this window").clicked() { do_reset = true; ui.close_menu(); }
            let mut locked_now = locked;
            if ui.checkbox(&mut locked_now, "Lock all windows").clicked() {
                toggle_lock = true; ui.close_menu();
            }
        });
        if new_alpha != alpha { layout.set_alpha(id, new_alpha); }
        if toggle_lock { layout.locked = !layout.locked; layout.set_dirty_locked(); }

        let rect = resp.response.rect;
        let rect_min = [rect.min.x, rect.min.y];
        let rect_size = [rect.width(), rect.height()];
        let is_free = matches!(mode, PosMode::Free);
        if let Some(ns) = record_change(stored.as_ref(), is_free, rect_min, rect_size,
                                        spec.resizable, observed, new_alpha) {
            layout.set_win(id, ns);
        }
        layout.set_observed(id, rect_min, rect_size);
        if do_reset { layout.reset(id); }
    }
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

    #[test]
    fn set_alpha_on_fresh_layout_marks_dirty_and_stores() {
        let mut l = UiLayout::from_path(std::env::temp_dir().join("ui_layout___alpha__.json"));
        l.set_alpha("hud", 128);
        assert!(l.dirty(), "set_alpha on new entry must mark dirty");
        assert_eq!(l.alpha_of("hud"), 128, "alpha must be stored");
    }

    #[test]
    fn decide_pos_uses_stored_when_present() {
        let ws = WinState { pos: Some([1.0,2.0]), size: None, alpha: 255 };
        assert_eq!(decide_pos(Some(&ws), true, false, false), PosMode::Free);
        // stored wins even when locked (it still renders at the stored pos, just not movable)
        assert_eq!(decide_pos(Some(&ws), true, true, false), PosMode::Free);
    }

    #[test]
    fn decide_pos_anchors_when_locked_and_no_stored() {
        assert_eq!(decide_pos(None, true, true, false), PosMode::Anchor);
    }

    #[test]
    fn decide_pos_anchors_on_first_frame_then_frees() {
        // no stored, unlocked, never observed -> Anchor (first frame)
        assert_eq!(decide_pos(None, false, false, false), PosMode::Anchor);
        // once observed -> Free
        assert_eq!(decide_pos(None, true, false, false), PosMode::Free);
    }

    #[test]
    fn decide_pos_anchors_on_reset_pending() {
        let ws = WinState { pos: Some([1.0,2.0]), size: None, alpha: 255 };
        assert_eq!(decide_pos(Some(&ws), true, false, true), PosMode::Anchor);
    }

    #[test]
    fn record_change_persists_when_free_and_moved() {
        let out = record_change(None, true, [10.0, 20.0], [100.0, 50.0], false,
                                Some(([0.0, 0.0], [100.0, 50.0])), 255);
        assert_eq!(out, Some(WinState { pos: Some([10.0,20.0]), size: None, alpha: 255 }));
    }

    #[test]
    fn record_change_keeps_size_for_resizable() {
        let out = record_change(None, true, [0.0,0.0], [200.0, 80.0], true,
                                Some(([0.0,0.0], [100.0,50.0])), 180);
        assert_eq!(out, Some(WinState { pos: Some([0.0,0.0]), size: Some([200.0,80.0]), alpha: 180 }));
    }

    #[test]
    fn record_change_noop_when_unchanged_and_unstored() {
        let out = record_change(None, true, [0.0,0.0], [100.0,50.0], false,
                                Some(([0.0,0.0], [100.0,50.0])), 255);
        assert_eq!(out, None);
    }

    #[test]
    fn record_change_always_updates_when_stored() {
        let stored = WinState { pos: Some([0.0,0.0]), size: None, alpha: 255 };
        let out = record_change(Some(&stored), true, [5.0,5.0], [100.0,50.0], false,
                                None, 255);
        assert_eq!(out, Some(WinState { pos: Some([5.0,5.0]), size: None, alpha: 255 }));
    }
}
