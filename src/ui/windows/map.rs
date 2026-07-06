//! Map window — unified minimap / zone map (replaces the old small minimap +
//! fullscreen map pair in hud.rs). One resizable window: the painter fills the
//! body, centered on the player, north-up. Scroll or the bottom slider zooms.

use crate::ui::{theme, UiCtx};
use egui::{Color32, Pos2, Stroke, Vec2};

/// Zoom range (1.0 = whole zone fits the view).
const ZOOM_MIN: f32 = 0.5;
const ZOOM_MAX: f32 = 8.0;

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;

    // Footer (bottom panel) draws first and measures itself; the canvas then
    // takes the EXACT remainder. Never size the canvas as `available - <const>`
    // and draw a taller footer after it — the window grows to fit the overflow,
    // re-derives the canvas from the new size, and creeps forever (the
    // window-growth feedback loop).
    egui::TopBottomPanel::bottom(ui.id().with("map_footer"))
        .frame(egui::Frame::none().inner_margin(egui::Margin { top: 3.0, ..Default::default() }))
        .show_separator_line(false)
        .show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Zoom").color(theme::TEXT_WEAK).size(10.0));
                ui.spacing_mut().slider_width = (ui.available_width() - 40.0).max(60.0);
                ui.add(
                    egui::Slider::new(cx.minimap_zoom, ZOOM_MIN..=ZOOM_MAX)
                        .show_value(false)
                        .logarithmic(true),
                );
                let zoom = *cx.minimap_zoom;
                ui.label(egui::RichText::new(format!("{zoom:.1}x")).color(theme::TEXT_WEAK).size(10.0));
            });
            ui.horizontal(|ui| {
                let zone = if s.zone.is_empty() { "(no zone)" } else { s.zone.as_str() };
                ui.label(egui::RichText::new(zone).color(theme::GOLD).size(11.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{:.0}, {:.0}, {:.0}",
                            s.player_pos[0], s.player_pos[1], s.player_pos[2]
                        ))
                        .color(theme::TEXT_WEAK)
                        .size(11.0),
                    );
                });
            });
        });

    egui::CentralPanel::default().frame(egui::Frame::none()).show_inside(ui, |ui| {
    let canvas_size = ui.available_size().max(Vec2::new(120.0, 100.0));
    let (resp, painter) = ui.allocate_painter(canvas_size, egui::Sense::hover());
    let rect = resp.rect;
    let painter = painter.with_clip_rect(rect);

    // Scroll over the map zooms (no modifier needed).
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll.abs() > 0.5 {
            *cx.minimap_zoom = (*cx.minimap_zoom * (1.0 + scroll * 0.005)).clamp(ZOOM_MIN, ZOOM_MAX);
        }
    }
    let zoom = *cx.minimap_zoom;

    // ── Zone bounds: prefer the map file's own extents, else terrain bounds. ─
    let (zone_min, zone_max) = map_bounds(ui, cx);
    let zone_w = (zone_max[0] - zone_min[0]).max(1.0);
    let zone_h = (zone_max[1] - zone_min[1]).max(1.0);

    // scene.player_pos = [east, north, up] = [server_x, server_y, server_z].
    let player = [s.player_pos[0], s.player_pos[1]];

    // View extents: centered on the player, clamped inside the zone when the
    // view is smaller than the zone (degenerate bounds fall back to player).
    let view_w = zone_w / zoom;
    let view_h = zone_h / zoom;
    let (half_w, half_h) = (view_w * 0.5, view_h * 0.5);
    let cx_e = if zone_min[0] + half_w <= zone_max[0] - half_w {
        player[0].clamp(zone_min[0] + half_w, zone_max[0] - half_w)
    } else {
        player[0]
    };
    let cy_n = if zone_min[1] + half_h <= zone_max[1] - half_h {
        player[1].clamp(zone_min[1] + half_h, zone_max[1] - half_h)
    } else {
        player[1]
    };
    let view_left = cx_e - half_w;
    let view_bot = cy_n - half_h;

    // Map coord → screen. East (+) → right, north (+) → up (flip screen Y).
    let to_screen = |east: f32, north: f32| -> Pos2 {
        egui::pos2(
            rect.min.x + (east - view_left) / view_w * rect.width(),
            rect.max.y - (north - view_bot) / view_h * rect.height(),
        )
    };

    // ── Background + grid (100-unit ticks). ─────────────────────────────────
    painter.rect_filled(rect, 3.0, theme::BG_PANEL);
    // All grid + map segments are batched into one painter.extend: each
    // line_segment call takes egui's graphics lock, and ~4k locked pushes per
    // frame made the map the most expensive window in --profile.
    let mut shapes: Vec<egui::Shape> = Vec::with_capacity(1024);
    let tick = Stroke::new(0.5, Color32::from_white_alpha(16));
    let step = 100.0_f32;
    let mut ge = (view_left / step).ceil() * step;
    while ge <= view_left + view_w {
        shapes.push(egui::Shape::line_segment(
            [to_screen(ge, view_bot), to_screen(ge, view_bot + view_h)],
            tick,
        ));
        ge += step;
    }
    let mut gn = (view_bot / step).ceil() * step;
    while gn <= view_bot + view_h {
        shapes.push(egui::Shape::line_segment(
            [to_screen(view_left, gn), to_screen(view_left + view_w, gn)],
            tick,
        ));
        gn += step;
    }

    // ── Zone map line art (Brewall-style .map files). ───────────────────────
    if let Some(zm) = cx.zone_map {
        for line in &zm.lines {
            let p1 = to_screen(line.east1, line.north1);
            let p2 = to_screen(line.east2, line.north2);
            // Cull segments fully outside the view (clip rect handles partials).
            if (p1.x < rect.min.x && p2.x < rect.min.x)
                || (p1.x > rect.max.x && p2.x > rect.max.x)
                || (p1.y < rect.min.y && p2.y < rect.min.y)
                || (p1.y > rect.max.y && p2.y > rect.max.y)
            {
                continue;
            }
            let color = Color32::from_rgba_unmultiplied(line.r, line.g, line.b, 180);
            shapes.push(egui::Shape::line_segment([p1, p2], Stroke::new(0.8, color)));
        }
        // Flush before the POI labels so text draws on top of the line art.
        painter.extend(shapes.drain(..));
        // POI labels once zoomed in enough to read them.
        if zoom >= 2.0 {
            for label in &zm.labels {
                let p = to_screen(label.east, label.north);
                if !rect.contains(p) {
                    continue;
                }
                painter.text(
                    p,
                    egui::Align2::CENTER_CENTER,
                    &label.text,
                    egui::FontId::proportional(10.0),
                    theme::TEXT_WEAK,
                );
            }
        }
    }

    // No zone map: the grid ticks are still pending.
    painter.extend(shapes);

    // ── Entity dots — billboard.pos = [east, north, up]. ────────────────────
    for b in &s.billboards {
        let p = to_screen(b.pos[0], b.pos[1]);
        if !rect.contains(p) {
            continue;
        }
        let color = if b.dead {
            theme::CON_GREY
        } else if b.is_target {
            theme::HP
        } else {
            theme::CHAT_COMBAT
        };
        painter.circle_filled(p, 3.0, color);
        if b.is_target && !b.dead {
            painter.circle_stroke(p, 5.0, Stroke::new(1.0, theme::HP));
        }
    }

    // ── Player arrow: triangle rotated by heading (0 = north, clockwise). ───
    let pp = to_screen(player[0], player[1]);
    let hr = s.player_heading.to_radians();
    let dir = |ang: f32, len: f32| Vec2::new(ang.sin() * len, -ang.cos() * len);
    let tip = pp + dir(hr, 9.0);
    let back_l = pp + dir(hr + 2.5, 6.5);
    let back_r = pp + dir(hr - 2.5, 6.5);
    painter.add(egui::Shape::convex_polygon(
        vec![tip, back_l, back_r],
        theme::CHAT_GROUP,
        Stroke::new(1.0, Color32::from_black_alpha(200)),
    ));

    painter.rect_stroke(rect, 3.0, Stroke::new(1.0, theme::FRAME_LO));
    });
}

/// Zone extents in map coords: the map file's own line extents when loaded
/// (map art usually covers the whole zone), else the terrain bounds passed in.
/// The scan over ~4k line endpoints is cached per zone (recomputing it every
/// frame showed up in the --profile window timings).
fn map_bounds(ui: &egui::Ui, cx: &UiCtx) -> ([f32; 2], [f32; 2]) {
    if let Some(zm) = cx.zone_map {
        if !zm.lines.is_empty() {
            let key = egui::Id::new(("map_bounds", &cx.scene.zone, zm.lines.len()));
            if let Some(cached) =
                ui.ctx().data(|d| d.get_temp::<([f32; 2], [f32; 2])>(key))
            {
                return cached;
            }
            let (mut min, mut max) = ([f32::MAX; 2], [f32::MIN; 2]);
            for l in &zm.lines {
                min[0] = min[0].min(l.east1).min(l.east2);
                min[1] = min[1].min(l.north1).min(l.north2);
                max[0] = max[0].max(l.east1).max(l.east2);
                max[1] = max[1].max(l.north1).max(l.north2);
            }
            ui.ctx().data_mut(|d| d.insert_temp(key, (min, max)));
            return (min, max);
        }
    }
    (cx.zone_min, cx.zone_max)
}
