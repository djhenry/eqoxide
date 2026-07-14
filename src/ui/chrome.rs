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

    // Stored size is the CONTENT size (what default_size/fixed_size control);
    // persisting the outer rect here would grow windows by the chrome overhead
    // every session.
    let size = stored
        .as_ref()
        .and_then(|s| s.size)
        .filter(|_| def.resizable)
        .unwrap_or(def.default_size);
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
        add_contents(ui);
        // Content size — the space default_size/fixed_size actually control.
        content_size = ui.min_rect().size();
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
    ui.add_space(2.0);
    closed
}
