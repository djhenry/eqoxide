//! Per-character UI layout persistence, v2.
//!
//! Supersedes the old `ui_layout.rs`. One JSON file per character
//! (`~/.config/eqoxide/ui_layout_<name>.json`); old v1 files load unchanged via
//! serde defaults. New in v2: per-window `open` state, OS-window geometry,
//! UI-scale/fades preferences, and the native client's cross-resolution
//! edge-relative position remap (decompiled `FUN_REDACTED`), which replaces the
//! old letterbox/constrain heuristics.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Per-window persisted state. `None` = "use the registry default".
#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Debug, Default)]
pub struct WinState {
    #[serde(default)]
    pub open: Option<bool>,
    #[serde(default)]
    pub pos: Option<[f32; 2]>,
    #[serde(default)]
    pub size: Option<[f32; 2]>,
    #[serde(default = "full_alpha")]
    pub alpha: u8,
}
fn full_alpha() -> u8 {
    255
}

/// Saved OS window geometry. `pos` is best-effort: winit cannot read or set the
/// outer position on Wayland, so it round-trips only on X11/XWayland.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct OsWindowState {
    pub size: [u32; 2],
    #[serde(default)]
    pub pos: Option<[i32; 2]>,
    #[serde(default)]
    pub maximized: bool,
}

/// On-disk form. Every field defaults so v1 files (locked + windows only) load.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Persisted {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    locked: bool,
    #[serde(default = "default_scale")]
    ui_scale: f32,
    #[serde(default = "default_fades")]
    fades: bool,
    /// Point-space (design-space) size at last save, for the load-time remap.
    #[serde(default)]
    screen: Option<[f32; 2]>,
    #[serde(default)]
    os_window: Option<OsWindowState>,
    #[serde(default)]
    windows: HashMap<String, WinState>,
}
fn default_scale() -> f32 {
    1.0
}
fn default_fades() -> bool {
    true
}

pub struct Layout {
    pub locked: bool,
    /// User multiplier on the window-size-derived zoom (0.5–2.0).
    pub ui_scale: f32,
    /// Global mouse-proximity window fading on/off.
    pub fades: bool,
    pub os_window: Option<OsWindowState>,
    windows: HashMap<String, WinState>,
    /// Screen size (points) the stored positions were saved under; consumed by
    /// [`Layout::remap_all`] on the first frame, then kept current.
    saved_screen: Option<[f32; 2]>,
    remapped: bool,
    path: PathBuf,
    dirty: bool,
    last_save: Instant,
    /// Runtime-only: last observed (min,size) per window, for change detection.
    observed: HashMap<String, ([f32; 2], [f32; 2])>,
    /// Runtime-only: windows to force back to their default placement for one frame.
    pending_reset: HashSet<String>,
    reset_all: bool,
}

/// Strip characters that are unsafe in a filename.
pub(crate) fn sanitize(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

impl Layout {
    pub fn load(character_name: &str) -> Self {
        let file = format!("ui_layout_{}.json", sanitize(character_name));
        Self::from_path(crate::config::config_dir().join(file))
    }

    pub fn from_path(path: PathBuf) -> Self {
        let persisted = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| match serde_json::from_str::<Persisted>(&s) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!("ui layout: ignoring corrupt {}: {e}", path.display());
                    None
                }
            })
            .unwrap_or_default();
        Layout {
            locked: persisted.locked,
            ui_scale: persisted.ui_scale.clamp(0.5, 2.0),
            fades: persisted.fades,
            os_window: persisted.os_window,
            windows: persisted.windows,
            saved_screen: persisted.screen,
            remapped: false,
            path,
            dirty: false,
            last_save: Instant::now(),
            observed: HashMap::new(),
            pending_reset: HashSet::new(),
            reset_all: false,
        }
    }

    /// Remap all stored window positions from the screen size they were saved
    /// under to the current one (both in points). Runs once, on the first frame
    /// the current screen size is known, and again is a no-op.
    pub fn remap_all(&mut self, screen: [f32; 2]) {
        if self.remapped {
            // Track the live screen size so saves record the right one.
            if self.saved_screen != Some(screen) {
                self.saved_screen = Some(screen);
                self.dirty = true;
            }
            return;
        }
        self.remapped = true;
        let old = match self.saved_screen {
            Some(o) if o[0] > 0.0 && o[1] > 0.0 => o,
            _ => {
                self.saved_screen = Some(screen);
                return;
            }
        };
        if old != screen {
            for ws in self.windows.values_mut() {
                if let Some(pos) = ws.pos.as_mut() {
                    let size = ws.size.unwrap_or([100.0, 40.0]);
                    pos[0] = remap_axis(pos[0], size[0], old[0], screen[0]);
                    pos[1] = remap_axis(pos[1], size[1], old[1], screen[1]);
                }
            }
            self.dirty = true;
        }
        self.saved_screen = Some(screen);
    }

    pub fn win(&self, id: &str) -> Option<&WinState> {
        self.windows.get(id)
    }

    fn entry(&mut self, id: &str) -> &mut WinState {
        self.windows.entry(id.to_string()).or_default()
    }

    pub fn set_geometry(&mut self, id: &str, pos: [f32; 2], size: Option<[f32; 2]>) {
        let e = self.entry(id);
        if e.pos != Some(pos) || (size.is_some() && e.size != size) {
            e.pos = Some(pos);
            if size.is_some() {
                e.size = size;
            }
            self.dirty = true;
        }
    }

    pub fn is_open(&self, id: &str, default_open: bool) -> bool {
        self.windows
            .get(id)
            .and_then(|w| w.open)
            .unwrap_or(default_open)
    }

    pub fn set_open(&mut self, id: &str, open: bool) {
        let e = self.entry(id);
        if e.open != Some(open) {
            e.open = Some(open);
            self.dirty = true;
        }
    }

    pub fn toggle_open(&mut self, id: &str, default_open: bool) {
        let now = self.is_open(id, default_open);
        self.set_open(id, !now);
    }

    pub fn alpha_of(&self, id: &str) -> u8 {
        self.windows.get(id).map(|w| w.alpha).unwrap_or(255)
    }

    pub fn set_alpha(&mut self, id: &str, alpha: u8) {
        let e = self.entry(id);
        if e.alpha != alpha {
            e.alpha = alpha;
            self.dirty = true;
        }
    }

    pub fn set_ui_scale(&mut self, s: f32) {
        let s = s.clamp(0.5, 2.0);
        if (self.ui_scale - s).abs() > f32::EPSILON {
            self.ui_scale = s;
            self.dirty = true;
        }
    }

    pub fn set_locked(&mut self, locked: bool) {
        if self.locked != locked {
            self.locked = locked;
            self.dirty = true;
        }
    }

    pub fn set_fades(&mut self, fades: bool) {
        if self.fades != fades {
            self.fades = fades;
            self.dirty = true;
        }
    }

    pub fn set_os_window(&mut self, st: OsWindowState) {
        if self.os_window != Some(st) {
            self.os_window = Some(st);
            self.dirty = true;
        }
    }

    /// Reset one window to its registry default (geometry + alpha; keeps `open`).
    pub fn reset(&mut self, id: &str) {
        if let Some(w) = self.windows.get_mut(id) {
            w.pos = None;
            w.size = None;
            w.alpha = 255;
        }
        self.observed.remove(id);
        self.pending_reset.insert(id.to_string());
        self.dirty = true;
    }

    pub fn reset_all(&mut self) {
        for w in self.windows.values_mut() {
            w.pos = None;
            w.size = None;
            w.alpha = 255;
        }
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

    // Runtime observed-rect helpers (used by chrome for change detection).
    pub(crate) fn observed(&self, id: &str) -> Option<([f32; 2], [f32; 2])> {
        self.observed.get(id).copied()
    }
    pub(crate) fn set_observed(&mut self, id: &str, min: [f32; 2], size: [f32; 2]) {
        self.observed.insert(id.to_string(), (min, size));
    }

    /// Debounced save; call once per frame. The actual disk write happens on a
    /// throwaway thread so a slow disk can't hitch the render loop.
    pub fn maybe_save(&mut self) {
        if self.dirty && self.last_save.elapsed() >= Duration::from_millis(1000) {
            if let Some(json) = self.serialize() {
                let path = self.path.clone();
                std::thread::spawn(move || {
                    if let Err(e) = std::fs::write(&path, json) {
                        tracing::warn!("ui layout: save failed ({}): {e}", path.display());
                    }
                });
                self.dirty = false;
                self.last_save = Instant::now();
            }
        }
    }

    /// Synchronous flush — call on every exit path.
    pub fn save_now(&mut self) {
        if !self.dirty {
            return;
        }
        if let Some(json) = self.serialize() {
            if let Err(e) = std::fs::write(&self.path, json) {
                tracing::warn!("ui layout: save failed ({}): {e}", self.path.display());
            } else {
                self.dirty = false;
                self.last_save = Instant::now();
            }
        }
    }

    fn serialize(&self) -> Option<String> {
        let persisted = Persisted {
            version: 2,
            locked: self.locked,
            ui_scale: self.ui_scale,
            fades: self.fades,
            screen: self.saved_screen,
            os_window: self.os_window,
            windows: self.windows.clone(),
        };
        match serde_json::to_string_pretty(&persisted) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("ui layout: serialize failed: {e}");
                None
            }
        }
    }

    // ── test-only accessors ──
    #[cfg(test)]
    pub(crate) fn dirty(&self) -> bool {
        self.dirty
    }
    #[cfg(test)]
    pub(crate) fn clear_dirty_for_test(&mut self) {
        self.dirty = false;
    }
}

/// One axis of the native client's cross-resolution window remap (decompiled
/// RoF2 `FUN_REDACTED`): windows in the left/top half keep their absolute
/// coordinate, windows in the right/bottom half keep their distance from that
/// edge, windows straddling the center shift by the center delta; finally the
/// result is clamped on-screen (title always reachable).
pub(crate) fn remap_axis(pos: f32, size: f32, old_dim: f32, new_dim: f32) -> f32 {
    let end = pos + size;
    let center = old_dim / 2.0;
    let p = if end <= center {
        pos // entirely in the left/top half: absolute
    } else if pos >= center {
        pos - old_dim + new_dim // entirely in the right/bottom half: edge-relative
    } else {
        pos + (new_dim - old_dim) / 2.0 // straddles the center: follow the center
    };
    // Clamp: keep at least a sliver on-screen and never above/left of origin
    // beyond -size+40 (title strip stays grabbable).
    p.clamp(-(size - 40.0).max(0.0), (new_dim - 40.0).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ui_layout_v2_{name}.json"))
    }

    #[test]
    fn v1_file_loads_with_defaults() {
        let path = tmp("v1compat");
        std::fs::write(
            &path,
            r#"{ "locked": true, "windows": { "inventory": { "pos": [8,90], "size": null, "alpha": 200 } } }"#,
        )
        .unwrap();
        let l = Layout::from_path(path.clone());
        assert!(l.locked);
        assert_eq!(l.ui_scale, 1.0);
        assert!(l.fades);
        assert_eq!(l.os_window, None);
        let w = l.win("inventory").unwrap();
        assert_eq!(w.pos, Some([8.0, 90.0]));
        assert_eq!(w.alpha, 200);
        assert_eq!(w.open, None, "v1 files have no open state; use default");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn corrupt_file_yields_defaults() {
        let path = tmp("corrupt");
        std::fs::write(&path, b"{ not json").unwrap();
        let l = Layout::from_path(path.clone());
        assert!(!l.locked);
        assert_eq!(l.win("x"), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn open_state_round_trip() {
        let path = tmp("open");
        let _ = std::fs::remove_file(&path);
        let mut l = Layout::from_path(path.clone());
        assert!(!l.is_open("inventory", false));
        assert!(l.is_open("player", true));
        l.set_open("inventory", true);
        l.save_now();
        let l2 = Layout::from_path(path.clone());
        assert!(l2.is_open("inventory", false));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn set_geometry_marks_dirty_only_on_change() {
        let mut l = Layout::from_path(tmp("dirty"));
        l.set_geometry("hud", [1.0, 2.0], None);
        assert!(l.dirty());
        l.clear_dirty_for_test();
        l.set_geometry("hud", [1.0, 2.0], None);
        assert!(!l.dirty());
    }

    #[test]
    fn os_window_round_trip() {
        let path = tmp("oswin");
        let _ = std::fs::remove_file(&path);
        let mut l = Layout::from_path(path.clone());
        l.set_os_window(OsWindowState { size: [1600, 900], pos: Some([10, 20]), maximized: false });
        l.save_now();
        let l2 = Layout::from_path(path.clone());
        assert_eq!(
            l2.os_window,
            Some(OsWindowState { size: [1600, 900], pos: Some([10, 20]), maximized: false })
        );
        let _ = std::fs::remove_file(path);
    }

    // ── remap math (the native FUN_REDACTED table) ──
    #[test]
    fn remap_left_half_keeps_absolute() {
        assert_eq!(remap_axis(10.0, 100.0, 1280.0, 1920.0), 10.0);
    }

    #[test]
    fn remap_right_half_keeps_edge_distance() {
        // window at right edge: pos 1180 + 100 = 1280 (old right edge)
        assert_eq!(remap_axis(1180.0, 100.0, 1280.0, 1920.0), 1820.0);
    }

    #[test]
    fn remap_center_straddle_follows_center() {
        // window centered on 640 (old center): stays centered on 960
        assert_eq!(remap_axis(590.0, 100.0, 1280.0, 1920.0), 910.0);
    }

    #[test]
    fn remap_clamps_offscreen() {
        // shrinking the screen pulls a far-right window back on-screen
        let p = remap_axis(1800.0, 200.0, 1920.0, 640.0);
        assert!(p <= 600.0, "window must stay reachable, got {p}");
    }

    #[test]
    fn remap_all_only_once_and_tracks_screen() {
        let path = tmp("remapall");
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            r#"{ "version": 2, "screen": [1280.0, 720.0],
                 "windows": { "w": { "pos": [1180.0, 620.0], "size": [100.0, 100.0], "alpha": 255 } } }"#,
        )
        .unwrap();
        let mut l = Layout::from_path(path.clone());
        l.remap_all([1920.0, 1080.0]);
        let w = l.win("w").unwrap();
        assert_eq!(w.pos, Some([1820.0, 980.0]));
        // second call with same size is a no-op
        l.remap_all([1920.0, 1080.0]);
        assert_eq!(l.win("w").unwrap().pos, Some([1820.0, 980.0]));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reset_clears_geometry_keeps_open() {
        let mut l = Layout::from_path(tmp("reset"));
        l.set_open("w", true);
        l.set_geometry("w", [5.0, 6.0], Some([10.0, 10.0]));
        l.reset("w");
        let w = l.win("w").unwrap();
        assert_eq!(w.pos, None);
        assert_eq!(w.size, None);
        assert_eq!(w.open, Some(true));
        assert!(l.is_reset_pending("w"));
        l.end_frame();
        assert!(!l.is_reset_pending("w"));
    }

    #[test]
    fn sanitize_strips_path_chars() {
        assert_eq!(sanitize("Bob"), "Bob");
        assert_eq!(sanitize("E'vil/../x"), "Evilx");
    }
}
