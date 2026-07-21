//! Compass window — native `CompassWnd`.
//!
//! A horizontal heading strip: the center of the strip is the direction the
//! player faces; tick marks and cardinal letters slide across as you turn.
//! Below it, the zone name and a `/loc`-style position readout.
//!
//! Heading convention (same as the minimap arrow in the old HUD):
//! `scene.player_heading` is in **degrees**, 0 = north, increasing clockwise
//! (screen-x = sin, screen-y = −cos).

use crate::{theme, UiCtx};
use egui::{Align2, FontId, Pos2, Rounding, Sense, Stroke, Vec2};

/// Height of the heading strip in points.
const STRIP_H: f32 = 22.0;
/// Degrees of heading visible across the full strip width.
const SPAN_DEG: f32 = 180.0;
/// Minor tick pitch in degrees.
const TICK_DEG: i32 = 15;

const CARDINALS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;
    let heading = s.player_heading.rem_euclid(360.0);

    // ── Heading strip ─────────────────────────────────────────────────────
    let width = ui.available_width().max(120.0);
    let (resp, painter) = ui.allocate_painter(Vec2::new(width, STRIP_H), Sense::hover());
    let rect = resp.rect;
    let painter = painter.with_clip_rect(rect);

    painter.rect_filled(rect, Rounding::same(2.0), theme::BG_PANEL);
    painter.rect_stroke(rect, Rounding::same(2.0), Stroke::new(1.0, theme::FRAME_LO));

    let px_per_deg = rect.width() / SPAN_DEG;
    let center_x = rect.center().x;

    // Ticks + labels for every TICK_DEG within the visible window (plus one
    // pitch of margin on each side so labels slide in smoothly).
    let first = (((heading - SPAN_DEG / 2.0) / TICK_DEG as f32).floor() as i32 - 1) * TICK_DEG;
    let last = first + SPAN_DEG as i32 + 2 * TICK_DEG;
    let mut a = first;
    while a <= last {
        let x = center_x + (a as f32 - heading) * px_per_deg;
        let norm = a.rem_euclid(360);
        if norm % 45 == 0 {
            // Cardinal / intercardinal: letter on top, brass tick below.
            let label = CARDINALS[(norm / 45) as usize];
            let (color, size) = if norm == 0 {
                (theme::GOLD, 12.0) // N stands out, native-style
            } else if norm % 90 == 0 {
                (theme::TEXT, 12.0)
            } else {
                (theme::TEXT_WEAK, 9.0)
            };
            painter.text(
                Pos2::new(x, rect.top() + 2.0),
                Align2::CENTER_TOP,
                label,
                FontId::proportional(size),
                color,
            );
            painter.line_segment(
                [Pos2::new(x, rect.bottom() - 6.0), Pos2::new(x, rect.bottom() - 1.0)],
                Stroke::new(1.0, theme::BRASS),
            );
        } else {
            painter.line_segment(
                [Pos2::new(x, rect.bottom() - 4.0), Pos2::new(x, rect.bottom() - 1.0)],
                Stroke::new(1.0, theme::FRAME_LO),
            );
        }
        a += TICK_DEG;
    }

    // Center needle: thin gold line + a small pointer at the bottom edge.
    painter.line_segment(
        [
            Pos2::new(center_x, rect.top() + 1.0),
            Pos2::new(center_x, rect.bottom() - 1.0),
        ],
        Stroke::new(1.0, theme::GOLD.gamma_multiply(0.55)),
    );
    let base = rect.bottom() - 1.0;
    painter.add(egui::Shape::convex_polygon(
        vec![
            Pos2::new(center_x - 4.0, base),
            Pos2::new(center_x + 4.0, base),
            Pos2::new(center_x, base - 5.0),
        ],
        theme::GOLD,
        Stroke::NONE,
    ));
    resp.on_hover_text(format!("heading {heading:.0}\u{00B0}"));

    // ── Zone + loc row ────────────────────────────────────────────────────
    ui.horizontal(|ui| {
        let zone = if s.zone.is_empty() { "(no zone)" } else { s.zone.as_str() };
        ui.label(egui::RichText::new(zone).size(11.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let [x, y, z] = s.player_pos;
            ui.label(
                egui::RichText::new(format!("loc: {x:.1}, {y:.1}, {z:.1}"))
                    .color(theme::TEXT_WEAK)
                    .size(10.0),
            );
        });
    });
}
