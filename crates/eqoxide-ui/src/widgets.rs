//! Shared EQ-flavored widgets: gauges, item slots, coin row.

use super::icons::IconRef;
use super::theme;
use egui::{Color32, Rect, Sense, Ui, Vec2};

/// Native gauge geometry: ~12 pt tall bar with a 1 px inset trough.
pub const GAUGE_H: f32 = 12.0;

/// Draw an EQ gauge: dark trough + tinted fill with the native vertical
/// gradient (emulating `A_GaugeFill`: bright top → dark bottom, multiplied by
/// the tint), overlay text, and animated fill that eases toward the target
/// value like the native `CGaugeWnd`.
///
/// `frac` is 0..=1. `label` draws left-aligned inside the gauge; the percent
/// draws right-aligned when `show_pct`.
pub fn gauge(ui: &mut Ui, id: impl std::hash::Hash, label: &str, frac: f32, tint: Color32, show_pct: bool) {
    let frac = frac.clamp(0.0, 1.0);
    let width = ui.available_width().max(40.0);
    let (resp, painter) = ui.allocate_painter(Vec2::new(width, GAUGE_H), Sense::hover());
    let rect = resp.rect;

    // Ease the displayed value toward the target (native animated fill).
    let id = ui.id().with(id);
    let shown = ui.ctx().animate_value_with_time(id, frac, 0.25);

    painter.rect_filled(rect, 2.0, theme::GAUGE_BG);
    painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0, Color32::from_black_alpha(180)));

    if shown > 0.001 {
        let fill = Rect::from_min_size(
            rect.min + Vec2::splat(1.0),
            Vec2::new((rect.width() - 2.0) * shown, rect.height() - 2.0),
        );
        gradient_fill(&painter, fill, tint);
    }

    let font = egui::FontId::proportional(10.0);
    if !label.is_empty() {
        painter.text(
            rect.left_center() + Vec2::new(4.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            font.clone(),
            theme::TEXT,
        );
    }
    if show_pct {
        painter.text(
            rect.right_center() + Vec2::new(-4.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            format!("{:.0}%", frac * 100.0),
            font,
            theme::TEXT,
        );
    }
}

/// The native `A_GaugeFill` look: a vertical gradient (≈90% white multiply at
/// the top → ≈36% at the bottom) times the tint color, drawn as one quad mesh
/// with per-vertex colors.
fn gradient_fill(painter: &egui::Painter, rect: Rect, tint: Color32) {
    let mul = |c: Color32, f: f32| {
        Color32::from_rgb(
            (c.r() as f32 * f) as u8,
            (c.g() as f32 * f) as u8,
            (c.b() as f32 * f) as u8,
        )
    };
    let top = mul(tint, 0.95);
    let bottom = mul(tint, 0.40);
    let mut mesh = egui::Mesh::default();
    let i = mesh.vertices.len() as u32;
    mesh.vertices.push(egui::epaint::Vertex { pos: rect.left_top(), uv: egui::epaint::WHITE_UV, color: top });
    mesh.vertices.push(egui::epaint::Vertex { pos: rect.right_top(), uv: egui::epaint::WHITE_UV, color: top });
    mesh.vertices.push(egui::epaint::Vertex { pos: rect.right_bottom(), uv: egui::epaint::WHITE_UV, color: bottom });
    mesh.vertices.push(egui::epaint::Vertex { pos: rect.left_bottom(), uv: egui::epaint::WHITE_UV, color: bottom });
    mesh.indices.extend_from_slice(&[i, i + 1, i + 2, i, i + 2, i + 3]);
    painter.add(egui::Shape::mesh(mesh));
}

/// A recessed 40×40 item slot. Draws the icon when given, else the fallback
/// text (abbreviated). Returns the interaction response (click = act).
pub fn item_slot(ui: &mut Ui, icon: Option<IconRef>, fallback: &str, tooltip: &str, selected: bool) -> egui::Response {
    let size = Vec2::splat(40.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, Color32::from_rgb(0x13, 0x13, 0x13));
    painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0, Color32::from_black_alpha(200)));
    match icon {
        Some(ic) => {
            egui::Image::new((ic.tex, size)).uv(ic.uv).paint_at(ui, rect.shrink(1.0));
        }
        None if !fallback.is_empty() => {
            let short: String = fallback.chars().take(10).collect();
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                short,
                egui::FontId::proportional(8.0),
                theme::TEXT_WEAK,
            );
        }
        None => {}
    }
    if selected {
        painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.5, theme::GOLD));
    } else if resp.hovered() {
        painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0, theme::BRASS));
    }
    if !tooltip.is_empty() {
        resp.clone().on_hover_text(tooltip);
    }
    resp
}

/// Coin readout: `12p 3g 45s 6c` with the native metal tints.
/// `coin` is [plat, gold, silver, copper].
pub fn coin_row(ui: &mut Ui, coin: [u32; 4]) {
    let colors = [
        Color32::from_rgb(0xE0, 0xE2, 0xE8), // platinum
        theme::GOLD,
        Color32::from_rgb(0xB8, 0xB8, 0xC0), // silver
        Color32::from_rgb(0xC0, 0x80, 0x50), // copper
    ];
    let tags = ["p", "g", "s", "c"];
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for i in 0..4 {
            ui.label(
                egui::RichText::new(format!("{}{}", coin[i], tags[i]))
                    .color(colors[i])
                    .size(12.0),
            );
        }
    });
}

/// Consider-color helper: server con RGB when known, grey otherwise.
pub fn con_color(con: Option<[u8; 3]>) -> Color32 {
    match con {
        Some([r, g, b]) => Color32::from_rgb(r, g, b),
        None => theme::CON_GREY,
    }
}

/// Format a copper amount as p/g/s/c (merchant prices arrive in copper).
pub fn fmt_copper(total: u32) -> String {
    let (p, r) = (total / 1000, total % 1000);
    let (g, r) = (r / 100, r % 100);
    let (s, c) = (r / 10, r % 10);
    let mut out = String::new();
    if p > 0 {
        out.push_str(&format!("{p}p "));
    }
    if g > 0 {
        out.push_str(&format!("{g}g "));
    }
    if s > 0 {
        out.push_str(&format!("{s}s "));
    }
    if c > 0 || out.is_empty() {
        out.push_str(&format!("{c}c"));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_copper_breaks_denominations() {
        assert_eq!(fmt_copper(0), "0c");
        assert_eq!(fmt_copper(9), "9c");
        assert_eq!(fmt_copper(1234), "1p 2g 3s 4c");
        assert_eq!(fmt_copper(1000), "1p");
    }

    #[test]
    fn con_color_falls_back_grey() {
        assert_eq!(con_color(None), theme::CON_GREY);
        assert_eq!(con_color(Some([1, 2, 3])), Color32::from_rgb(1, 2, 3));
    }
}
