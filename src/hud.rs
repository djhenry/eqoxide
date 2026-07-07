//! Non-window overlays drawn over the 3D scene: world-projected entity
//! nameplates (tinted by consider color), the loading screen, the
//! connection-lost banner, and the fps/profile/debug readouts.
//!
//! All interactive windows live in `crate::ui` (the window system, #162);
//! overlays here are fixed, non-interactive chrome.

use crate::camera::project_to_screen;
use crate::scene::SceneState;

/// A top-center red banner shown when the server connection has gone silent (#8), so a frozen/dead
/// session is visible to a human player instead of looking like a normal idle scene.
pub fn draw_connection_banner(ctx: &egui::Context, disconnected: bool) {
    if !disconnected { return; }
    egui::Area::new(egui::Id::new("connection_banner"))
        .anchor(egui::Align2::CENTER_TOP, [0.0, 6.0])
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::none()
                .fill(egui::Color32::from_rgb(140, 20, 20))
                .inner_margin(egui::Margin::symmetric(12.0, 6.0))
                .rounding(4.0)
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("⚠ Connection lost — server not responding")
                            .size(16.0)
                            .color(egui::Color32::WHITE)
                            .strong(),
                    );
                });
        });
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

/// `--profile` overlay: smoothed per-phase frame timings (update / 3D render / egui / submit) plus the
/// total CPU-side frame cost and the wall-clock frame interval. Anchored top-left under the fps line.
pub fn draw_profile(ctx: &egui::Context, p: &crate::profiling::FrameProfile) {
    egui::Area::new(egui::Id::new("profile_overlay"))
        .anchor(egui::Align2::LEFT_TOP, [8.0, 30.0])
        .interactable(false)
        .show(ctx, |ui| {
            let line = |ui: &mut egui::Ui, label: &str, ms: f32| {
                ui.label(
                    egui::RichText::new(format!("{label:<7}{ms:6.2} ms"))
                        .monospace()
                        .size(12.0)
                        .color(egui::Color32::from_rgb(180, 220, 255)),
                );
            };
            line(ui, "update", p.update_ms);
            line(ui, " scene", p.scene_ms);
            line(ui, " smooth", p.smooth_ms);
            line(ui, "render", p.render_ms);
            line(ui, "egui",   p.egui_ms);
            line(ui, "submit", p.submit_ms);
            line(ui, "cpu",    p.total_ms);
            ui.label(
                egui::RichText::new(format!("frame  {:6.2} ms", p.frame_ms))
                    .monospace()
                    .size(12.0)
                    .color(egui::Color32::from_rgb(255, 220, 120)),
            );
        });
}

pub fn draw_debug_overlay(
    ctx: &egui::Context,
    player_pos: [f32; 3],
    player_heading: f32,
    zone: &str,
    corrections: u32,
) {
    let h_cw = crate::eq_net::protocol::ccw_to_cw(player_heading);
    let info = format!(
        "DEBUG\nzone: {}\npos: ({:.1}, {:.1}, {:.1})\nheading CCW: {:.0}°  CW: {:.0}°\ncorrections: {}",
        zone, player_pos[0], player_pos[1], player_pos[2], player_heading, h_cw, corrections
    );
    egui::Area::new(egui::Id::new("debug_overlay"))
        .anchor(egui::Align2::LEFT_TOP, [8.0, 52.0])
        .interactable(false)
        .show(ctx, |ui| {
            ui.label(egui::RichText::new(&info)
                .monospace()
                .size(11.0)
                .color(egui::Color32::from_rgb(0, 255, 0)));
        });
}

pub fn draw_loading(ctx: &egui::Context, zone: &str, status: &str, progress: Option<f32>) {
    egui::Area::new(egui::Id::new("loading"))
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(format!("Loading zone: {zone}"))
                    .size(24.0)
                    .color(egui::Color32::WHITE));
                if !status.is_empty() {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(status)
                        .size(16.0)
                        .color(egui::Color32::from_gray(200)));
                }
                if let Some(frac) = progress {
                    ui.add_space(8.0);
                    ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0))
                        .desired_width(260.0));
                }
            });
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
        // Cull nameplates on the SAME distance+frustum test the model draw uses
        // (pass.rs), so a plate never shows for a spawn whose model isn't rendered —
        // and, unlike the occlusion test below, this is independent of whether zone
        // geometry is loaded. Without it, far-off spawns (past ENTITY_DRAW_DIST) and
        // — when collision is None (asset server down / mid-reload) — every on-screen
        // spawn showed a floating label with no model (#177).
        if !crate::camera::entity_in_view(b.pos, scene.player_pos, view_proj,
                                          crate::pass::ENTITY_DRAW_DIST, crate::pass::ENTITY_CULL_MARGIN) {
            continue;
        }
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

        // The current target's name is tinted by its consider color (set from the
        // OP_Consider reply); everyone else stays white.
        let name_color = match (b.is_target, scene.target_con) {
            (true, Some([r, g, bl])) => egui::Color32::from_rgb(r, g, bl),
            _ => egui::Color32::WHITE,
        };

        // Golden "!" over NPCs that have a quest (data from quests.json, synced from the asset server), MMO-style, so an
        // agent (or a human watching /frame) can SEE who to talk to. See src/quests.rs.
        let is_quest_giver =
            crate::quests::is_quest_giver(&scene.zone, &crate::http::clean_entity_name(&b.name));

        egui::Area::new(egui::Id::new(("npc_label", i)))
            .fixed_pos(egui::pos2(sx - 35.0, sy - 80.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(160))
                    .inner_margin(egui::Margin { left: 4.0, right: 4.0, top: 2.0, bottom: 2.0 })
                    .show(ui, |ui| {
                        if is_quest_giver {
                            ui.label(egui::RichText::new("❗ quest")
                                .size(13.0)
                                .strong()
                                .color(egui::Color32::from_rgb(255, 210, 40)));
                        }
                        ui.label(egui::RichText::new(&b.name)
                            .size(12.0)
                            .color(name_color));
                        ui.label(egui::RichText::new(&label_line2)
                            .size(11.0)
                            .color(egui::Color32::WHITE));
                    });
            });
    }
}

/// TEMP DEBUG (navmesh visualization): overlays the collision floor + the A* path near the
/// player, to expose where `find_path` (A*, with `MAX_WALK_GRADE`) and the walker's floor
/// (`nearest_floor`) disagree on steep slopes.
///  - floor dots: green ≈ level, blue = below player, red = above player, gray ring = no floor
///    in the step band (`nearest_floor` = the *walker's* notion of ground, no grade limit);
///  - yellow line + orange dots: the A* path to `goto_target` (find_path's exact decisions,
///    incl. the 1.2 grade limit). Where the yellow line stops short of the goal but green/blue
///    floor dots continue toward it = the A*/collision slope mismatch.
pub fn draw_nav_debug(
    ctx: &egui::Context,
    scene: &SceneState,
    view_proj: [[f32; 4]; 4],
    screen_w: u32,
    screen_h: u32,
    collision: Option<&crate::assets::Collision>,
    nav_goal: Option<[f32; 3]>,
) {
    let Some(col) = collision else { return; };
    let ppp = ctx.pixels_per_point();
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground, egui::Id::new("nav_debug")));
    let p = scene.player_pos;
    let on_screen = |w: [f32; 3]| -> Option<egui::Pos2> {
        let [sx, sy] = project_to_screen(w, view_proj, screen_w, screen_h)?;
        if sx < 0.0 || sy < 0.0 || sx > screen_w as f32 || sy > screen_h as f32 { return None; }
        Some(egui::pos2(sx / ppp, sy / ppp))
    };

    // (a) collision floor grid: ±R around the player, every STEP units.
    const R: i32 = 96;
    const STEP: i32 = 8;
    const STEP_UP: f32 = 20.0;
    const MAX_DROP: f32 = 100.0;
    let mut gx = -R;
    while gx <= R {
        let mut gy = -R;
        while gy <= R {
            let x = p[0] + gx as f32;
            let y = p[1] + gy as f32;
            match col.nearest_floor(x, y, p[2], STEP_UP, MAX_DROP) {
                Some(fz) => {
                    if let Some(sp) = on_screen([x, y, fz]) {
                        let dz = fz - p[2];
                        let c = if dz > 4.0 { egui::Color32::from_rgb(255, 90, 90) }
                                else if dz < -4.0 { egui::Color32::from_rgb(80, 160, 255) }
                                else { egui::Color32::from_rgb(80, 255, 120) };
                        painter.circle_filled(sp, 2.0, c);
                    }
                }
                None => {
                    if let Some(sp) = on_screen([x, y, p[2]]) {
                        painter.circle_stroke(sp, 2.0,
                            egui::Stroke::new(1.0, egui::Color32::from_gray(90)));
                    }
                }
            }
            gy += STEP;
        }
        gx += STEP;
    }

    // (b) A* path to the goal — reproduce find_path's exact decisions (incl. MAX_WALK_GRADE).
    if let Some(goal) = nav_goal {
        let pts: Vec<egui::Pos2> = match col.find_path(p, goal, 1.0, &[], true) {
            Some(path) => std::iter::once(p).chain(path.iter().copied())
                .filter_map(on_screen).collect(),
            None => Vec::new(),
        };
        for pair in pts.windows(2) {
            painter.line_segment([pair[0], pair[1]],
                egui::Stroke::new(2.5, egui::Color32::from_rgb(255, 230, 40)));
        }
        for wp in &pts {
            painter.circle_filled(*wp, 3.0, egui::Color32::from_rgb(255, 140, 0));
        }
        // goal marker: yellow ring, red-filled if A* found no path at all.
        if let Some(sp) = on_screen(goal) {
            if pts.is_empty() { painter.circle_filled(sp, 6.0, egui::Color32::RED); }
            painter.circle_stroke(sp, 9.0,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 255, 0)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::SceneState;

    /// Overlays must render headlessly without panicking on an empty scene.
    #[test]
    fn overlays_draw_headless() {
        let scene = SceneState::default();
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |ctx| {
            draw_fps(ctx, 60.0);
            draw_connection_banner(ctx, true);
            draw_loading(ctx, "qeynos", "syncing", Some(0.5));
            draw_debug_overlay(ctx, [1.0, 2.0, 3.0], 90.0, "qeynos", 4);
            draw_labels(
                ctx,
                &scene,
                [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]],
                800,
                600,
                [0.0; 3],
                None,
            );
        });
    }
}
