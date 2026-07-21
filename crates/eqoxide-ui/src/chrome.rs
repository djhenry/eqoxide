//! `eq_window` — the chrome wrapper every UI window renders through.
//!
//! Wraps `egui::Window` (title_bar off) with: a custom RoF2-style title strip
//! (gradient + gold close box), default placement from the registry anchor,
//! stored-geometry restore, drag/resize gating by the global lock, native-style
//! mouse-proximity fading (2 s delay, 0.5 s animation, matching the native
//! client), a right-click context menu (opacity / fades /
//! reset / lock), and persisted geometry recording.

use super::persist::Layout;
use super::registry::WindowDef;
use super::theme;
use std::collections::HashMap;
use std::time::Instant;

/// Alpha floor a faded window settles at (out of 255).
const FADE_TO: f32 = 100.0;
/// Mouse must be off the window this long before it fades.
const FADE_DELAY: f32 = 2.0;

/// The manual gap `title_strip` adds after its own bar (via `ui.add_space`),
/// on top of the automatic `item_spacing` egui inserts before the next
/// widget. Named so the restore-side footprint math below can never drift
/// out of sync with what `title_strip` actually draws — see
/// `title_footprint`.
const TITLE_STRIP_TRAILING_SPACE: f32 = 2.0;

/// The title strip's total live footprint (height): `TITLE_H` (the bar
/// itself) + `TITLE_STRIP_TRAILING_SPACE` (its own `add_space`) + one
/// automatic `item_spacing` gap (egui inserts this before the next widget
/// laid out after the strip — i.e. before `add_contents`'s first item).
///
/// Unlike a cosmetic estimate, this MUST equal the strip's actual measured
/// footprint exactly for a self-filling body (one that calls
/// `ui.available_size()`, e.g. `windows/map.rs`'s canvas) to round-trip
/// idempotently: such a body's measured bottom always lands at the
/// container's bottom edge regardless of where the title strip's cursor
/// left off, so `content_size = after_content - after_title` only cancels
/// the `+ title_footprint(ctx)` added below (in `size`) when the two match
/// bit-for-bit. A mismatch of even a few px does NOT self-correct — it
/// reproduces #613's own bug shape (a constant per-cycle drift), just at a
/// smaller magnitude. (This was caught by `round_trip_size_is_idempotent_across_reloads`
/// during development: an earlier hardcoded `TITLE_H + 3.0` — missing the
/// `add_space` term — drifted by -2.0px/cycle instead of the pre-fix
/// +21.0px/cycle. Fixed windows and natural-height bodies are unaffected
/// either way, since their content bottom doesn't depend on container size.)
fn title_footprint(ctx: &egui::Context) -> f32 {
    theme::TITLE_H + TITLE_STRIP_TRAILING_SPACE + ctx.style().spacing.item_spacing.y
}

/// Runtime window-system state that isn't persisted.
pub struct WinSys {
    pub layout: Layout,
    /// Last instant the pointer was over each window's rect.
    last_over: HashMap<String, Instant>,
}

/// What the chrome reports back to the manager.
#[derive(Default)]
pub struct FrameResult {
    /// The title-strip close box was clicked this frame.
    pub close_clicked: bool,
}

impl WinSys {
    pub fn new(layout: Layout) -> Self {
        WinSys { layout, last_over: HashMap::new() }
    }

    /// Effective opacity for a window this frame (fades + per-window alpha).
    fn effective_alpha(&mut self, ctx: &egui::Context, id: &str, being_dragged: bool) -> f32 {
        let base = self.layout.alpha_of(id) as f32;
        if !self.layout.fades {
            return base / 255.0;
        }
        let over = self
            .layout
            .observed(id)
            .map(|(min, size)| {
                ctx.input(|i| i.pointer.latest_pos())
                    .map(|p| {
                        egui::Rect::from_min_size(min.into(), size.into())
                            .expand(4.0)
                            .contains(p)
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(true);
        let now = Instant::now();
        if over || being_dragged {
            self.last_over.insert(id.to_string(), now);
        }
        let recent = self
            .last_over
            .get(id)
            .map(|t| t.elapsed().as_secs_f32() < FADE_DELAY)
            .unwrap_or(false);
        let target = if recent { base } else { base.min(FADE_TO) };
        let shown = ctx.animate_value_with_time(egui::Id::new(("win_fade", id)), target, 0.5);
        // While fading we need repaints even without input.
        if (shown - target).abs() > 0.5 {
            ctx.request_repaint();
        }
        shown / 255.0
    }
}

/// Compute a window's default top-left position from its registry anchor.
fn default_pos(def: &WindowDef, screen: egui::Rect, size: [f32; 2]) -> egui::Pos2 {
    let x = match def.default_anchor.0[0] {
        egui::Align::Min => screen.min.x + def.default_offset[0],
        egui::Align::Center => screen.center().x - size[0] / 2.0 + def.default_offset[0],
        egui::Align::Max => screen.max.x - size[0] + def.default_offset[0],
    };
    let y = match def.default_anchor.0[1] {
        egui::Align::Min => screen.min.y + def.default_offset[1],
        egui::Align::Center => screen.center().y - size[1] / 2.0 + def.default_offset[1],
        egui::Align::Max => screen.max.y - size[1] + def.default_offset[1],
    };
    egui::pos2(x, y)
}

/// Show one chromed window. `screen` is the true point-space screen rect
/// computed by the caller — NOT `ctx.screen_rect()`, which is stale on the
/// first frame after any zoom change.
pub fn eq_window(
    ctx: &egui::Context,
    sys: &mut WinSys,
    def: &WindowDef,
    screen: egui::Rect,
    add_contents: impl FnOnce(&mut egui::Ui),
) -> FrameResult {
    let id = def.id;
    let locked = sys.layout.locked;
    let reset_pending = sys.layout.is_reset_pending(id);
    let stored = sys.layout.win(id).cloned();
    let observed = sys.layout.observed(id);

    // Stored size is CONTENT-ONLY — what `add_contents` itself draws,
    // EXCLUDING the custom title strip (see the capture at the bottom of
    // this function). The strip lives in the SAME `ui` as `add_contents`,
    // so before #613 was fixed, "content size" silently included it: the
    // saved total got fed straight back in below as the window's content
    // area, the strip drew AGAIN inside that area on the next session, and
    // the measured union kept growing by the strip's own footprint every
    // single save/reload cycle. (Egui's OWN window chrome — the guard this
    // comment used to describe — is genuinely excluded by `.title_bar(false)`
    // and was never the problem; this code's OWN title bar, drawn inline as
    // regular content, was.) Reconstructing the container we hand to egui
    // therefore means adding the strip's footprint back on; see
    // `title_footprint` for why that value must match the strip's actual
    // measured height exactly, not just approximately.
    let stored_content_size = stored.as_ref().and_then(|s| s.size).filter(|_| def.resizable);
    let size = match stored_content_size {
        Some(c) => [c[0], c[1] + title_footprint(ctx)],
        None => def.default_size,
    };
    let dpos = default_pos(def, screen, size);

    let opacity = sys.effective_alpha(ctx, id, false);

    let frame = egui::Frame::window(&ctx.style()).multiply_with_opacity(opacity);

    let mut win = egui::Window::new(def.title)
        .id(egui::Id::new(("eq_window", id)))
        .title_bar(false)
        .collapsible(false)
        .frame(frame)
        // Native behavior (design §5): windows drag freely — even partly
        // offscreen ("tucking") — and off-screen repair happens only at load
        // time via persist::remap_all. constrain(true) would silently rewrite
        // stored positions every frame.
        .constrain(false)
        .movable(!locked)
        .resizable(def.resizable && !locked);

    if reset_pending {
        win = win
            .current_pos(default_pos(def, screen, def.default_size))
            .fixed_size(egui::Vec2::from(def.default_size));
    } else {
        let pos = stored.as_ref().and_then(|s| s.pos).map(|p| egui::pos2(p[0], p[1])).unwrap_or(dpos);
        win = win.default_pos(pos).default_size(egui::Vec2::from(size));
        // Stored geometry must win over egui's memory of a prior session-frame.
        if observed.is_none() {
            win = win.current_pos(pos);
            if def.resizable {
                win = win.fixed_size(egui::Vec2::from(size));
            }
        }
    }

    let mut result = FrameResult::default();
    let mut content_size = egui::Vec2::ZERO;

    let resp = win.show(ctx, |ui| {
        ui.set_opacity(opacity);
        result.close_clicked = title_strip(ui, def, locked);
        // Union bbox after JUST the title strip — its own footprint
        // (TITLE_H plus the spacing egui inserts before the next item),
        // measured LIVE so it's exactly right regardless of theme tweaks.
        let after_title = ui.min_rect().size();
        add_contents(ui);
        // Union bbox after strip + body. They stack vertically in the same
        // `ui`, so the strip's height is baked into this total — but width
        // is a "max" across siblings, not a sum, so the strip never
        // inflates it (this matches the #613 report: width never drifted).
        let after_content = ui.min_rect().size();
        // #613 fix: persist only what `add_contents` itself needed, by
        // subtracting the strip's own (live-measured) footprint back out.
        // The OLD code stored `after_content` directly — title strip
        // included — which is exactly the number that then got fed back in
        // above as next session's content area, drawing the strip again on
        // top of it. This is the invariant that must hold now: the number
        // we save means the same thing `size` above applies it to.
        content_size = egui::vec2(after_content.x, (after_content.y - after_title.y).max(0.0));
    });

    if let Some(resp) = resp {
        // Right-click context menu: per-window opacity, fades, reset, lock-all.
        let mut new_alpha = sys.layout.alpha_of(id);
        let mut do_reset = false;
        let mut toggle_lock = false;
        let mut fades = sys.layout.fades;
        let mut fades_changed = false;
        resp.response.context_menu(|ui| {
            ui.label(egui::RichText::new(def.title).strong());
            ui.add(egui::Slider::new(&mut new_alpha, 40..=255).text("Opacity"));
            if ui.checkbox(&mut fades, "Fade when idle").clicked() {
                fades_changed = true;
            }
            if ui.button("Reset this window").clicked() {
                do_reset = true;
                ui.close_menu();
            }
            let mut locked_now = locked;
            if ui.checkbox(&mut locked_now, "Lock all windows").clicked() {
                toggle_lock = true;
                ui.close_menu();
            }
        });
        if new_alpha != sys.layout.alpha_of(id) {
            sys.layout.set_alpha(id, new_alpha);
        }
        if fades_changed {
            sys.layout.set_fades(fades);
        }
        if toggle_lock {
            let cur = sys.layout.locked;
            sys.layout.set_locked(!cur);
        }

        // Record geometry once the window has settled (skip the reset frame).
        // Position is the OUTER rect min (what default_pos/current_pos set);
        // size is the CONTENT size (what default_size/fixed_size set).
        let rect = resp.response.rect;
        let rect_min = [rect.min.x, rect.min.y];
        let rect_size = [rect.width(), rect.height()];
        if !reset_pending {
            let moved = observed
                .map(|(o_min, o_size)| {
                    (o_min[0] - rect_min[0]).abs() > 0.5
                        || (o_min[1] - rect_min[1]).abs() > 0.5
                        || (o_size[0] - rect_size[0]).abs() > 0.5
                        || (o_size[1] - rect_size[1]).abs() > 0.5
                })
                .unwrap_or(false);
            let has_stored = stored.as_ref().map(|s| s.pos.is_some()).unwrap_or(false);
            if has_stored || moved {
                let size = (def.resizable && content_size != egui::Vec2::ZERO)
                    .then_some([content_size.x, content_size.y]);
                sys.layout.set_geometry(id, rect_min, size);
            }
        }
        sys.layout.set_observed(id, rect_min, rect_size);
        if do_reset {
            sys.layout.reset(id);
        }
    }

    result
}

/// Draw the RoF2-style title strip inside the window; returns close-clicked.
fn title_strip(ui: &mut egui::Ui, def: &WindowDef, locked: bool) -> bool {
    let strip_h = theme::TITLE_H;
    let width = ui.available_width();
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(width, strip_h), egui::Sense::hover());
    let painter = ui.painter();

    // Top-lit vertical gradient.
    let mut mesh = egui::Mesh::default();
    let (t, b) = (theme::TITLE_TOP, theme::TITLE_BOTTOM);
    mesh.vertices.push(egui::epaint::Vertex { pos: rect.left_top(), uv: egui::epaint::WHITE_UV, color: t });
    mesh.vertices.push(egui::epaint::Vertex { pos: rect.right_top(), uv: egui::epaint::WHITE_UV, color: t });
    mesh.vertices.push(egui::epaint::Vertex { pos: rect.right_bottom(), uv: egui::epaint::WHITE_UV, color: b });
    mesh.vertices.push(egui::epaint::Vertex { pos: rect.left_bottom(), uv: egui::epaint::WHITE_UV, color: b });
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(egui::Shape::mesh(mesh));
    painter.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        egui::Stroke::new(1.0, theme::FRAME_LO),
    );

    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        def.title,
        egui::FontId::proportional(12.0),
        theme::TEXT,
    );
    if locked {
        painter.text(
            rect.left_center() + egui::vec2(4.0, 0.0),
            egui::Align2::LEFT_CENTER,
            "🔒",
            egui::FontId::proportional(10.0),
            theme::TEXT_WEAK,
        );
    }

    let mut closed = false;
    if def.closeable {
        let box_size = 14.0;
        let close_rect = egui::Rect::from_center_size(
            egui::pos2(rect.max.x - box_size / 2.0 - 3.0, rect.center().y),
            egui::vec2(box_size, box_size),
        );
        let resp = ui.interact(
            close_rect,
            ui.id().with("close_box"),
            egui::Sense::click(),
        );
        let color = if resp.hovered() { theme::TEXT } else { theme::GOLD };
        let p = ui.painter();
        if resp.hovered() {
            p.rect_filled(close_rect, 2.0, egui::Color32::from_white_alpha(16));
        }
        p.text(
            close_rect.center(),
            egui::Align2::CENTER_CENTER,
            "✕",
            egui::FontId::proportional(11.0),
            color,
        );
        // Native behavior: the action fires on release over the same box.
        closed = resp.clicked();
    }
    ui.add_space(TITLE_STRIP_TRAILING_SPACE);
    closed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body shaped like the real culprit (`windows/map.rs`): a
    /// self-measuring bottom panel, then a central canvas that fills
    /// whatever's left via `ui.available_size()`. This is the exact pattern
    /// #613's root cause bit — the panel divides up the ui's `max_rect`
    /// (the container we handed it), not what the title strip already
    /// consumed by advancing the cursor — so it's the most faithful
    /// reproduction available for a mutation-discriminating test.
    fn self_filling_body(ui: &mut egui::Ui) {
        egui::TopBottomPanel::bottom(ui.id().with("footer"))
            .show_separator_line(false)
            .show_inside(ui, |ui| {
                ui.label("footer");
            });
        egui::CentralPanel::default().frame(egui::Frame::none()).show_inside(ui, |ui| {
            let size = ui.available_size().max(egui::vec2(50.0, 50.0));
            ui.allocate_space(size);
        });
    }

    fn test_def() -> WindowDef {
        WindowDef {
            id: "rt_test_win",
            title: "RT Test",
            hotkey: None,
            default_anchor: egui::Align2::LEFT_TOP,
            default_offset: [40.0, 40.0],
            default_size: [240.0, 240.0],
            resizable: true,
            closeable: true,
            default_open: true,
            transient: false,
        }
    }

    /// #613 regression: save -> load -> save (simulating a client restart,
    /// including a FRESH `egui::Context` each cycle — egui's own per-window
    /// Resize memory lives in `ctx.data`, which does not survive a real
    /// restart any more than our own `persist::Layout` file would if we
    /// didn't write it) must reproduce the EXACT same stored size, not
    /// merely "close to" it. Before the fix this drifted by exactly
    /// `TITLE_H + item_spacing` (21.0 px) per cycle, matching the owner's
    /// measured field data.
    #[test]
    fn round_trip_size_is_idempotent_across_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ui_layout_test.json");
        let def = test_def();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 720.0));
        let raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };

        // Seed a baseline "already used once" layout — a brand-new window
        // (no stored size at all) isn't where this bug lives; it only bites
        // once a size has actually been persisted at least once, exactly
        // like the owner's real files.
        {
            let mut sys = WinSys::new(Layout::from_path(path.clone()));
            sys.layout.set_geometry(def.id, [40.0, 40.0], Some([240.0, 261.0]));
            sys.layout.save_now();
        }

        let mut sizes = Vec::new();
        for _ in 0..6 {
            // Fresh Layout (reload from disk) AND fresh egui::Context
            // (simulated restart) each cycle.
            let mut sys = WinSys::new(Layout::from_path(path.clone()));
            let ctx = egui::Context::default();
            // A few settle frames per "session" — the layout saves
            // continuously while the app runs, not only on exit.
            for _ in 0..3 {
                let _ = ctx.run(raw.clone(), |ctx| {
                    eq_window(ctx, &mut sys, &def, screen, self_filling_body);
                });
            }
            sys.layout.save_now();
            let size = sys.layout.win(def.id).and_then(|w| w.size).expect("size not persisted");
            sizes.push(size);
        }

        for pair in sizes.windows(2) {
            assert_eq!(
                pair[0], pair[1],
                "persisted size drifted across a reload cycle — full history: {sizes:?}"
            );
        }
    }
}
