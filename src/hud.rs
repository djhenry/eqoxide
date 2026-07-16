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

/// TEMP DEBUG (navmesh visualization): overlays the collision floor + the A* path near the
/// player, to expose where `find_path` (A*, with `MAX_WALK_GRADE`) and the walker's floor
/// (`nearest_floor`) disagree on steep slopes.
///  - floor dots: green ≈ level, blue = below player, red = above player, gray ring = no floor
///    in the step band (`nearest_floor` = the *walker's* notion of ground, no grade limit);
///  - yellow line + orange dots: the A* path to `goto_target` (find_path's exact decisions,
///    incl. the 1.2 grade limit). Where the yellow line stops short of the goal but green/blue
///    floor dots continue toward it = the A*/collision slope mismatch.
/// One cached A* grid cell (8u): the walker's floor here + whether the step to the E/N neighbor is
/// walkable. Computed once from static geometry and reused across frames (see `NavDebugCache`).
struct NavCell { floor: Option<f32>, edge_e: Option<(f32, bool)>, edge_n: Option<(f32, bool)> }

/// Persistent cache for the nav-debug overlay so it doesn't re-raycast the whole grid and re-run a
/// full A* every frame (that recompute-per-frame is what made the overlay tank the frame rate). The
/// floor/edge queries are functions of STATIC geometry, so each cell is computed once and reused;
/// only cells newly in range as the player walks are raycast. Invalidated on a big teleport/level
/// change. The A* path is NO LONGER recomputed here — the overlay draws the walker's ACTUAL committed
/// plan, published by the nav thread via `nav_path_view`, so what you see is what it follows (#246).
#[derive(Default)]
struct NavDebugCache {
    last_pos: [f32; 3],
    ref_z:    f32,
    valid:    bool,
    coarse:   std::collections::HashMap<(i32, i32), NavCell>, // 8u A* grid: floors + edges
    fine:     std::collections::HashMap<(i32, i32), Option<f32>>, // 4u near-player floor dots
}
thread_local! {
    static NAV_CACHE: std::cell::RefCell<NavDebugCache> = std::cell::RefCell::new(NavDebugCache::default());
}

pub fn draw_nav_debug(
    ctx: &egui::Context,
    scene: &SceneState,
    view_proj: [[f32; 4]; 4],
    screen_w: u32,
    screen_h: u32,
    collision: Option<&crate::nav::collision::Collision>,
    nav_goal: Option<[f32; 3]>,
    nav_paths: &(Vec<[f32; 3]>, Vec<[f32; 3]>), // walker's live (coarse, fine) plan (#246)
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

    // Nav grid + A* path, CACHED so we don't re-raycast the whole grid and re-run a full A* EVERY
    // frame — that per-frame recompute is what tanked the frame rate (and made the walker crawl under
    // the overlay). Floor/edge queries are functions of STATIC geometry, so each cell is computed
    // once and reused; only newly-in-range cells get raycast as the player walks. Floor dots use a
    // FINER grid near the player for extra fidelity; edges + the A* path stay at the 8u NAV_CELL
    // (A*'s real resolution, so the edges keep reflecting A*'s actual connectivity).
    const R: i32 = 96;              // coarse (8u) context radius
    const CELL: i32 = 8;            // = NAV_CELL — edges + A* work at this resolution
    const FINE_R: i32 = 32;         // fine (4u) floor-dot radius for near-player detail
    const FINE: i32 = 4;
    const STEP_UP: f32 = 20.0;
    const MAX_DROP: f32 = 100.0;
    // A* per-edge constants (mirror find_path @ src/assets.rs):
    const STEP_H: f32 = 20.0;
    const MAX_STEP_DOWN: f32 = 60.0;
    const MAX_WALK_GRADE: f32 = 1.2;
    const CHEST: f32 = 3.0;
    let walk_col  = egui::Color32::from_rgba_unmultiplied(60, 230, 90, 170);
    let block_col = egui::Color32::from_rgba_unmultiplied(255, 60, 60, 220);
    let dot_col = |fz: f32| {
        let dz = fz - p[2];
        if dz > 4.0 { egui::Color32::from_rgb(255, 90, 90) }
        else if dz < -4.0 { egui::Color32::from_rgb(80, 160, 255) }
        else { egui::Color32::from_rgb(80, 255, 120) }
    };

    NAV_CACHE.with(|slot| {
        let cache = &mut *slot.borrow_mut();
        // Invalidate on first use / a big teleport / a level (z) change — cached floors were sampled
        // relative to the old reference height and would be wrong on a different level.
        let jumped = (p[0] - cache.last_pos[0]).hypot(p[1] - cache.last_pos[1]) > 200.0
            || (p[2] - cache.ref_z).abs() > 20.0;
        if !cache.valid || jumped {
            cache.coarse.clear();
            cache.fine.clear();
            cache.ref_z = p[2];
            cache.valid = true;
        }
        cache.last_pos = p;
        let ref_z = cache.ref_z;

        // ── Coarse 8u cells (floor + E/N walkable edges): raycast only cells not already cached. ──
        let (pcx, pcy) = ((p[0] / CELL as f32).round() as i32, (p[1] / CELL as f32).round() as i32);
        let chalf = R / CELL;
        for ci in -chalf..=chalf {
            for cj in -chalf..=chalf {
                let key = (pcx + ci, pcy + cj);
                if cache.coarse.contains_key(&key) { continue; }
                let (cx, cy) = (key.0 as f32 * CELL as f32, key.1 as f32 * CELL as f32);
                let floor = col.nearest_floor(cx, cy, ref_z, STEP_UP, MAX_DROP);
                let edge = |bx: f32, by: f32, cz: f32| -> Option<(f32, bool)> {
                    let mut chosen: Option<(f32, bool)> = None;
                    for nf in col.column_floors(bx, by, cz, STEP_H, MAX_STEP_DOWN) {
                        if nf - cz > STEP_H || cz - nf > MAX_STEP_DOWN { continue; }
                        let rise = nf - cz;
                        let run = ((bx - cx).hypot(by - cy)).max(1e-3);
                        let grade_ok = !(rise > 0.0 && rise / run > MAX_WALK_GRADE);
                        // The SAME clearance test A* runs, at the SAME radius and cell — never a
                        // hand-rolled copy. An overlay that disagrees with the planner is an
                        // overlay that lies about the planner (#358).
                        let clear = col.edge_clear([cx, cy, cz + CHEST], [bx, by, nf + CHEST],
                                                   crate::movement::PLAYER_RADIUS, CELL as f32);
                        if grade_ok && clear { chosen = Some((nf, true)); break; }
                        else if chosen.is_none() { chosen = Some((nf, false)); }
                    }
                    chosen
                };
                let (edge_e, edge_n) = match floor {
                    Some(fz) => (edge(cx + CELL as f32, cy, fz), edge(cx, cy + CELL as f32, fz)),
                    None => (None, None),
                };
                cache.coarse.insert(key, NavCell { floor, edge_e, edge_n });
            }
        }
        // Edges (green walkable / red blocked), from the cache.
        for ci in -chalf..=chalf {
            for cj in -chalf..=chalf {
                let key = (pcx + ci, pcy + cj);
                let Some(nc) = cache.coarse.get(&key) else { continue; };
                let Some(fz) = nc.floor else { continue; };
                let (cx, cy) = (key.0 as f32 * CELL as f32, key.1 as f32 * CELL as f32);
                let a = [cx, cy, fz];
                for (e, bx, by) in [(&nc.edge_e, cx + CELL as f32, cy), (&nc.edge_n, cx, cy + CELL as f32)] {
                    if let Some((nf, walk)) = e {
                        if let (Some(sa), Some(sb)) = (on_screen(a), on_screen([bx, by, *nf])) {
                            let (c, w) = if *walk { (walk_col, 1.0) } else { (block_col, 1.6) };
                            painter.line_segment([sa, sb], egui::Stroke::new(w, c));
                        }
                    }
                }
            }
        }

        // ── Floor dots: FINE (4u) near the player, COARSE (8u) for the outer context ring. ──
        let (pfx, pfy) = ((p[0] / FINE as f32).round() as i32, (p[1] / FINE as f32).round() as i32);
        let fhalf = FINE_R / FINE;
        for fi in -fhalf..=fhalf {
            for fj in -fhalf..=fhalf {
                let key = (pfx + fi, pfy + fj);
                let (fx, fy) = (key.0 as f32 * FINE as f32, key.1 as f32 * FINE as f32);
                let fz = *cache.fine.entry(key).or_insert_with(|| col.nearest_floor(fx, fy, ref_z, STEP_UP, MAX_DROP));
                if let Some(fz) = fz {
                    if let Some(sp) = on_screen([fx, fy, fz]) { painter.circle_filled(sp, 1.6, dot_col(fz)); }
                }
            }
        }
        // Coarse dots outside the fine window (context).
        for ci in -chalf..=chalf {
            for cj in -chalf..=chalf {
                let key = (pcx + ci, pcy + cj);
                let (cx, cy) = (key.0 as f32 * CELL as f32, key.1 as f32 * CELL as f32);
                if (cx - p[0]).abs() <= FINE_R as f32 && (cy - p[1]).abs() <= FINE_R as f32 { continue; }
                if let Some(Some(fz)) = cache.coarse.get(&key).map(|c| c.floor) {
                    if let Some(sp) = on_screen([cx, cy, fz]) { painter.circle_filled(sp, 1.6, dot_col(fz)); }
                }
            }
        }

        // ── A* paths: drawn from the WALKER'S ACTUAL committed plan (published via nav_path_view), NOT
        // a separate recompute — so the overlay shows exactly what the walker follows, including a
        // stale/awkward segment, instead of an idealized fresh route (#246). No A* runs here, so a long
        // route no longer hitches the frame rate; we still bound the DRAWN coarse line to a near-range
        // window so a cross-zone route isn't a screen-spanning tangle.
        //   • COARSE (yellow): the walker's global 8u route (`ActionLoop::path`), drawn ≤COARSE_VIS ahead.
        //   • FINE (cyan): the fine 2u local plan (`ActionLoop::local_path`) the walker steers along.
        const COARSE_VIS: f32 = 160.0;
        let (coarse_plan, fine_plan) = nav_paths;
        if let Some(goal) = nav_goal {
            // Draw the coarse route from the point nearest the player forward, capping the drawn
            // length at COARSE_VIS so a distant goal stays a reasonable near-range line.
            let near_i = coarse_plan.iter().enumerate()
                .min_by(|(_, a), (_, b)| {
                    let da = (a[0]-p[0]).hypot(a[1]-p[1]);
                    let db = (b[0]-p[0]).hypot(b[1]-p[1]);
                    da.total_cmp(&db)
                }).map(|(i, _)| i).unwrap_or(0);
            let mut coarse_vis: Vec<[f32; 3]> = Vec::new();
            let mut acc = 0.0f32;
            let mut prev = p;
            for w in coarse_plan.iter().skip(near_i) {
                acc += (w[0]-prev[0]).hypot(w[1]-prev[1]);
                if acc > COARSE_VIS { break; }
                coarse_vis.push(*w);
                prev = *w;
            }
            // Coarse (yellow).
            let cp: Vec<egui::Pos2> = std::iter::once(p).chain(coarse_vis.iter().copied()).filter_map(on_screen).collect();
            for pair in cp.windows(2) {
                painter.line_segment([pair[0], pair[1]], egui::Stroke::new(2.0, egui::Color32::from_rgba_unmultiplied(255, 230, 40, 150)));
            }
            for wp in &cp { painter.circle_filled(*wp, 2.5, egui::Color32::from_rgb(255, 140, 0)); }
            // Fine local plan (cyan) — the line the walker actually follows.
            let lp: Vec<egui::Pos2> = std::iter::once(p).chain(fine_plan.iter().copied()).filter_map(on_screen).collect();
            for pair in lp.windows(2) {
                painter.line_segment([pair[0], pair[1]], egui::Stroke::new(3.0, egui::Color32::from_rgb(80, 230, 255)));
            }
            if let Some(sp) = on_screen(goal) {
                if coarse_plan.is_empty() { painter.circle_filled(sp, 6.0, egui::Color32::RED); }
                painter.circle_stroke(sp, 9.0, egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 255, 0)));
            }
        }
    });
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
