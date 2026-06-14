use crate::camera::project_to_screen;
use crate::scene::SceneState;
use crate::zone_map::ZoneMap;

// ── Shared helper: draw a labelled stat bar ───────────────────────────────────

fn stat_bar(
    ui: &mut egui::Ui,
    label: &str,
    pct: f32,
    width: f32,
    height: f32,
    fill: egui::Color32,
) {
    ui.label(label);
    let (resp, painter) = ui.allocate_painter(
        egui::Vec2::new(width, height),
        egui::Sense::hover(),
    );
    let rect = resp.rect;
    painter.rect_filled(rect, 2.0, egui::Color32::from_rgb(30, 30, 30));
    let filled = (pct / 100.0).clamp(0.0, 1.0);
    if filled > 0.0 {
        painter.rect_filled(
            egui::Rect::from_min_size(rect.min, egui::Vec2::new(rect.width() * filled, rect.height())),
            2.0,
            fill,
        );
    }
    ui.label(format!("{:.0}%", pct));
}

pub fn draw_fps(ctx: &egui::Context, fps: f32) {
    egui::Area::new(egui::Id::new("fps_counter"))
        .anchor(egui::Align2::LEFT_TOP, [8.0, 8.0])
        .interactable(false)
        .show(ctx, |ui| {
            let color = if fps >= 55.0 {
                egui::Color32::from_rgb(80, 220, 80)
            } else if fps >= 30.0 {
                egui::Color32::from_rgb(255, 200, 60)
            } else {
                egui::Color32::from_rgb(255, 80, 80)
            };
            ui.label(
                egui::RichText::new(format!("{:.0} fps", fps))
                    .monospace()
                    .size(14.0)
                    .color(color)
                    .strong(),
            );
        });
}

pub fn draw_hud(ctx: &egui::Context, scene: &SceneState, bot_id: &str) {
    egui::Window::new(format!("EQ Observer — {bot_id}"))
        .anchor(egui::Align2::LEFT_BOTTOM, [0.0, 0.0])
        .resizable(false)
        .collapsible(false)
        .min_width(640.0)
        .show(ctx, |ui| {
            // Row 1: Zone / player / HP / strategy / target
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!(
                    "Zone: {}",
                    if scene.zone.is_empty() { "connecting…" } else { &scene.zone }
                )).strong());
                ui.separator();
                ui.label(format!("{} (L{})", scene.player_name, scene.player_level));

                let hp = scene.player_hp_pct;
                let hp_color = if hp < 30.0 {
                    egui::Color32::from_rgb(248, 81, 73)
                } else if hp < 60.0 {
                    egui::Color32::from_rgb(255, 166, 87)
                } else {
                    egui::Color32::from_rgb(63, 185, 80)
                };
                stat_bar(ui, "HP:", hp, 120.0, 10.0, hp_color);

                ui.separator();
                ui.label(egui::RichText::new(&scene.strategy).italics().weak());

                if let (Some(name), Some(hp_pct)) = (&scene.target_name, scene.target_hp_pct) {
                    ui.separator();
                    ui.label(format!("→ {} ({:.0}%)", name, hp_pct));
                }
            });

            // Row 2: Mana bar + XP bar + coordinates
            ui.horizontal(|ui| {
                stat_bar(ui, "Mana:",
                    scene.player_mana_pct, 120.0, 8.0,
                    egui::Color32::from_rgb(58, 120, 220),
                );
                ui.add_space(12.0);
                stat_bar(ui, "XP:",
                    scene.player_xp_pct, 200.0, 8.0,
                    egui::Color32::from_rgb(200, 160, 40),
                );
                ui.add_space(12.0);
                // scene.player_pos is [server_y=map_x, server_x=map_y, server_z] after GPU swap
                let [sx, sy, sz] = scene.player_pos;
                ui.label(egui::RichText::new(format!("map ({:.1}, {:.1}, {:.1})", sx, sy, sz))
                    .weak()
                    .monospace());
            });
        });
}

pub fn draw_message_log(ctx: &egui::Context, scene: &SceneState) {
    let visible: Vec<_> = scene.messages.iter()
        .filter(|m| m.timestamp.elapsed().as_secs() < 20)
        .collect();
    if visible.is_empty() {
        return;
    }

    egui::Window::new("##msglog")
        .title_bar(false)
        .anchor(egui::Align2::LEFT_BOTTOM, [0.0, -80.0])  // just above the HUD
        .resizable(false)
        .collapsible(false)
        .min_width(480.0)
        .max_width(640.0)
        .frame(egui::Frame::none()
            .fill(egui::Color32::from_black_alpha(140))
            .inner_margin(egui::Margin::same(6.0)))
        .show(ctx, |ui| {
            for entry in &visible {
                let color = match entry.kind.as_str() {
                    "combat"  => egui::Color32::from_rgb(255, 90,  90),
                    "zone"    => egui::Color32::from_rgb(160, 160, 160),
                    "exp"     => egui::Color32::from_rgb(220, 175,  40),
                    "chat"    => egui::Color32::from_rgb(210, 210, 255),
                    _         => egui::Color32::from_rgb(200, 200, 200),
                };
                ui.label(egui::RichText::new(&entry.text).size(12.0).color(color));
            }
        });
}

pub fn draw_minimap(
    ctx:          &egui::Context,
    scene:        &SceneState,
    zone_min:     [f32; 2],  // [min_east, min_north] in map coords
    zone_max:     [f32; 2],  // [max_east, max_north] in map coords
    zoom:         &mut f32,
    fullscreen:   &mut bool,
    zone_map:     Option<&ZoneMap>,
) {
    let zone_w = (zone_max[0] - zone_min[0]).max(1.0);
    let zone_h = (zone_max[1] - zone_min[1]).max(1.0);

    // scene.player_pos is already [server_y=east, server_x=north, server_z] after GPU swap
    let player_map = [scene.player_pos[0], scene.player_pos[1]];

    let map_px = if *fullscreen { 580.0_f32 } else { 200.0_f32 };
    let map_py = if *fullscreen { 580.0_f32 } else { 200.0_f32 };
    let map_size = egui::Vec2::new(map_px, map_py);

    let (anchor, offset) = if *fullscreen {
        (egui::Align2::CENTER_CENTER, [0.0_f32, 0.0_f32])
    } else {
        (egui::Align2::RIGHT_TOP, [-10.0, 10.0])
    };

    egui::Window::new("##minimap")
        .title_bar(false)
        .anchor(anchor, offset)
        .resizable(false)
        .collapsible(false)
        .frame(egui::Frame::none())
        .show(ctx, |ui| {
            let (resp, painter) = ui.allocate_painter(map_size, egui::Sense::click());
            let rect = resp.rect;

            // Scroll to zoom (only when hovered)
            if resp.hovered() {
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll.abs() > 0.5 {
                    *zoom = (*zoom * (1.0 + scroll * 0.005)).clamp(0.25, 8.0);
                }
            }

            if resp.clicked() {
                *fullscreen = !*fullscreen;
            }

            // Dark background
            painter.rect_filled(rect, 4.0, egui::Color32::from_black_alpha(210));

            // Compute view extents: centre on player, scaled by zoom
            let view_w = zone_w / *zoom;
            let view_h = zone_h / *zoom;
            let half_w = view_w * 0.5;
            let half_h = view_h * 0.5;
            // Guard: when zone not yet loaded (min==max==0), the clamp bounds would
            // be inverted (min > max), causing a panic. Fall back to centering on player.
            let cx = if zone_min[0] + half_w <= zone_max[0] - half_w {
                player_map[0].clamp(zone_min[0] + half_w, zone_max[0] - half_w)
            } else {
                player_map[0]
            };
            let cy = if zone_min[1] + half_h <= zone_max[1] - half_h {
                player_map[1].clamp(zone_min[1] + half_h, zone_max[1] - half_h)
            } else {
                player_map[1]
            };
            let view_left  = cx - half_w;
            let view_bot   = cy - half_h;

            // Map coord → screen pos.
            // East (+) → right, North (+) → up (flip Y for screen).
            let to_screen = |east: f32, north: f32| -> egui::Pos2 {
                let nx = (east  - view_left) / view_w;
                let ny = (north - view_bot)  / view_h;
                egui::pos2(
                    rect.min.x + nx * rect.width(),
                    rect.max.y - ny * rect.height(),
                )
            };

            // EQ zone map lines
            if let Some(zm) = zone_map {
                for line in &zm.lines {
                    let p1 = to_screen(line.east1, line.north1);
                    let p2 = to_screen(line.east2, line.north2);
                    // Skip lines completely outside the view rect (both endpoints out)
                    if !rect.contains(p1) && !rect.contains(p2) { continue; }
                    let color = egui::Color32::from_rgba_unmultiplied(
                        line.r, line.g, line.b, 180,
                    );
                    painter.line_segment([p1, p2], egui::Stroke::new(0.8, color));
                }
            }

            // Zone border grid tick marks (every 100 units)
            let tick_stroke = egui::Stroke::new(0.5, egui::Color32::from_white_alpha(20));
            let step = 100.0_f32;
            let x_start = (view_left / step).ceil() * step;
            let mut gx = x_start;
            while gx <= view_left + view_w {
                let sp = to_screen(gx, view_bot);
                let ep = to_screen(gx, view_bot + view_h);
                painter.line_segment([sp, ep], tick_stroke);
                gx += step;
            }
            let y_start = (view_bot / step).ceil() * step;
            let mut gy = y_start;
            while gy <= view_bot + view_h {
                let sp = to_screen(view_left, gy);
                let ep = to_screen(view_left + view_w, gy);
                painter.line_segment([sp, ep], tick_stroke);
                gy += step;
            }

            // Entity dots
            for b in &scene.billboards {
                let em = [b.pos[0], b.pos[1]]; // b.pos is [east, north, height] after GPU swap
                let sp = to_screen(em[0], em[1]);
                if !rect.contains(sp) { continue; }
                let color = if b.dead {
                    egui::Color32::from_rgb(80, 80, 80)
                } else if b.is_target {
                    egui::Color32::from_rgb(255, 80, 80)
                } else {
                    egui::Color32::from_rgb(200, 100, 60)
                };
                let r = if *fullscreen { 4.0 } else { 3.0 };
                painter.circle_filled(sp, r, color);
            }

            // Player dot + heading arrow
            let pp = to_screen(player_map[0], player_map[1]);
            painter.circle_filled(pp, if *fullscreen { 6.0 } else { 5.0 }, egui::Color32::from_rgb(80, 180, 255));

            // EQ heading: 0 = north, clockwise. Screen: north = up (−screen_y).
            let hr = scene.player_heading.to_radians();
            let arrow_len = if *fullscreen { 16.0 } else { 10.0 };
            let arrow_tip = egui::pos2(
                pp.x + hr.sin() * arrow_len,
                pp.y - hr.cos() * arrow_len,
            );
            painter.line_segment(
                [pp, arrow_tip],
                egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 180, 255)),
            );

            // Border + hint
            painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.0, egui::Color32::from_rgb(90, 90, 120)));
            if !*fullscreen {
                painter.text(
                    egui::pos2(rect.min.x + 4.0, rect.max.y - 14.0),
                    egui::Align2::LEFT_BOTTOM,
                    "scroll=zoom  click=fullscreen",
                    egui::FontId::proportional(9.0),
                    egui::Color32::from_white_alpha(80),
                );
            }
        });
}

pub fn draw_loading(ctx: &egui::Context, zone: &str) {
    egui::Area::new(egui::Id::new("loading"))
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(egui::RichText::new(format!("Loading zone: {}…", zone))
                .size(24.0)
                .color(egui::Color32::WHITE));
        });
}

pub fn draw_labels(
    ctx: &egui::Context,
    scene: &SceneState,
    view_proj: [[f32; 4]; 4],
    screen_w: u32,
    screen_h: u32,
    cam_eye: [f32; 3],
    collision: Option<&crate::assets::Collision>,
) {
    let ppp = ctx.pixels_per_point();

    // NPC labels
    for (i, b) in scene.billboards.iter().enumerate() {
        if b.level == 0 { continue; } // level-0 placeholder spawns have no label
        let Some([sx, sy]) = project_to_screen(b.pos, view_proj, screen_w, screen_h) else {
            continue; // behind camera or outside depth range
        };
        // In view only: drop labels whose anchor projects off the visible viewport.
        if sx < 0.0 || sy < 0.0 || sx > screen_w as f32 || sy > screen_h as f32 {
            continue;
        }
        // Not behind a wall: skip if zone geometry occludes the line of sight. Aim a
        // little above the entity base (toward the head/label) so a knee-high lip of
        // floor in front doesn't hide an otherwise-visible NPC.
        if let Some(col) = collision {
            let head = [b.pos[0], b.pos[1], b.pos[2] + 4.0];
            if col.segment_blocked(cam_eye, head) {
                continue;
            }
        }
        let (sx, sy) = (sx / ppp, sy / ppp);
        let hp_str = if b.hp_pct == 0.0 && !b.dead {
            "??".to_string()
        } else {
            format!("{:.0}%", b.hp_pct)
        };
        let label_line2 = format!("L{}  {}", b.level, hp_str);

        egui::Area::new(egui::Id::new(("npc_label", i)))
            .fixed_pos(egui::pos2(sx - 35.0, sy - 80.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(160))
                    .inner_margin(egui::Margin { left: 4.0, right: 4.0, top: 2.0, bottom: 2.0 })
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(&b.name)
                            .size(12.0)
                            .color(egui::Color32::WHITE));
                        ui.label(egui::RichText::new(&label_line2)
                            .size(11.0)
                            .color(egui::Color32::WHITE));
                    });
            });
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{Billboard, SceneState};

    fn make_scene() -> SceneState {
        SceneState {
            zone: "qeynos".into(),
            player_pos: [0.0, 0.0, 0.0],
            player_name: "Aiquestbot".into(),
            player_level: 1,
            player_hp_pct: 100.0,
            billboards: vec![
                Billboard {
                    id: 1,
                    pos: [10.0, 10.0, 0.0],
                    level: 4,
                    hp_pct: 61.0,
                    name: "a gnoll".into(),
                    is_target: false,
                    dead: false,
                    race: "".into(),
                    action: "".into(),
                    heading: 0.0,
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn draw_labels_does_not_panic() {
        let ctx = egui::Context::default();
        let identity: [[f32; 4]; 4] = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            draw_labels(ctx, &make_scene(), identity, 800, 600, [0.0, 0.0, 0.0], None);
        });
    }
}
