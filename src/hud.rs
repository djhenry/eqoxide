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
/// Death overlay (#284): while the player is slain the client HOLDS them dead (no auto-respawn), so
/// a HUMAN needs a way to revive. Show a centered panel naming the killer with a "Respawn at Bind"
/// button. Returns true the frame the button is clicked (the caller sets the respawn request, the
/// same flag POST /v1/lifecycle/respawn drives).
pub fn draw_death_overlay(ctx: &egui::Context, dead: bool, killed_by: &str) -> bool {
    if !dead { return false; }
    let mut clicked = false;
    egui::Area::new(egui::Id::new("death_overlay"))
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            egui::Frame::none()
                .fill(egui::Color32::from_rgba_unmultiplied(20, 0, 0, 220))
                .inner_margin(egui::Margin::symmetric(24.0, 18.0))
                .rounding(8.0)
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new("You have died.")
                            .size(22.0).strong().color(egui::Color32::from_rgb(230, 60, 60)));
                        if !killed_by.is_empty() {
                            ui.label(egui::RichText::new(format!("Slain by {killed_by}"))
                                .color(egui::Color32::from_rgb(220, 200, 200)));
                        }
                        ui.add_space(10.0);
                        if ui.button(egui::RichText::new("⚰  Respawn at Bind").size(16.0)).clicked() {
                            clicked = true;
                        }
                    });
                });
        });
    clicked
}

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

/// Zone-transition fade overlay (#286): a full-screen black rectangle at `alpha` (0.0 = clear,
/// 1.0 = opaque black). Drawn as a background layer so the loading text / HUD render on top of it.
/// A no-op at alpha 0 so it costs nothing outside a transition.
pub fn draw_fade(ctx: &egui::Context, alpha: f32) {
    if alpha <= 0.0 { return; }
    let a = (alpha.clamp(0.0, 1.0) * 255.0) as u8;
    let rect = ctx.screen_rect();
    ctx.layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("zone_fade")))
        .rect_filled(rect, 0.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, a));
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
    collision: Option<&crate::nav::collision::Collision>,
) {
    let ppp = ctx.pixels_per_point();

    // NPC labels
    for (i, b) in scene.billboards.iter().enumerate() {
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
                            .color(name_color));
                        ui.label(egui::RichText::new(&label_line2)
                            .size(11.0)
                            .color(egui::Color32::WHITE));
                    });
            });
    }
}

// (#608: `draw_nav_debug` + `NavDebugCache`/`NavCell` + their thread-local are GONE. That overlay
// re-derived nav state — it raycast the collision grid itself and re-ran the planner's clearance
// test to decide what to draw — and it was a screen-space painter with no depth test, so it drew
// through walls. Its replacement is the depth-tested 3D pass in `eqoxide_renderer::nav_overlay`,
// which draws the walker's PUBLISHED `NavDebugSnapshot` verbatim; the agent-readable form of the
// same snapshot is GET /v1/observe/nav_debug. No nav logic remains in this file.)

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
