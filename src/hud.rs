//! The egui HUD: player stat bars, world-projected entity nameplates/labels (tinted by consider
//! color), the NPC dialogue panel with clickable `[keyword]`s, the minimap, and the control bar
//! (hail/say/target buttons + say box). Buttons write the same shared request slots the HTTP API
//! uses. See `docs/npc-interaction.md`.

use crate::camera::project_to_screen;
use crate::scene::SceneState;
use crate::zone_map::ZoneMap;
use crate::ui_layout::{managed_window, UiLayout, WindowSpec};

/// HUD design reference (points). The layout is authored for 1920x1080; using a HALF reference here
/// makes `set_zoom_factor` twice as large, so all HUD text/widgets render at 2x scale. Shared with
/// the zoom calc in `app.rs::egui_pass`. Tune this single pair to change the global HUD size.
pub const HUD_REF_W: f32 = 960.0;
pub const HUD_REF_H: f32 = 540.0;

/// Letterbox: adjust an anchored element's offset so HUD chrome sits on a HUD_REF_W x HUD_REF_H
/// canvas centered in the window, instead of spreading to the window edges. The margin is the slack
/// on the non-constraining axis (read from the zoomed screen rect, in points); when the window is
/// narrower than the canvas the margin clamps to 0 (graceful fall back to edge-anchoring). On a
/// triple-monitor/ultrawide it keeps the HUD on the center region. Nameplates do NOT use this —
/// they track 3D mobs across the whole window. `align` is the element's own anchor alignment.
pub(crate) fn canvas_off(ctx: &egui::Context, align: egui::Align2, base: [f32; 2]) -> [f32; 2] {
    let sr = ctx.screen_rect();
    let mx = (sr.width() - HUD_REF_W).max(0.0) * 0.5;
    let my = (sr.height() - HUD_REF_H).max(0.0) * 0.5;
    let dx = match align.0[0] {
        egui::Align::Min => mx, egui::Align::Max => -mx, egui::Align::Center => 0.0,
    };
    let dy = match align.0[1] {
        egui::Align::Min => my, egui::Align::Max => -my, egui::Align::Center => 0.0,
    };
    [base[0] + dx, base[1] + dy]
}

/// All managed (movable) HUD windows. Anchors/offsets reproduce the previous
/// fixed layout, retuned so the windows no longer overlap on a 960x540 canvas.
pub static WINDOW_SPECS: [WindowSpec; 9] = [
    WindowSpec { id: "status_hud",   title: "Status",       default_anchor: egui::Align2::LEFT_BOTTOM,   default_offset: [0.0, 0.0],   default_size: None,                resizable: false },
    WindowSpec { id: "message_log",  title: "Messages",     default_anchor: egui::Align2::LEFT_BOTTOM,   default_offset: [0.0, -64.0], default_size: Some([480.0, 120.0]), resizable: true  },
    WindowSpec { id: "minimap",      title: "Map",          default_anchor: egui::Align2::RIGHT_TOP,     default_offset: [-10.0, 10.0],default_size: Some([200.0, 200.0]), resizable: true  },
    WindowSpec { id: "control_bar",  title: "Controls",     default_anchor: egui::Align2::RIGHT_BOTTOM,  default_offset: [-8.0, -8.0], default_size: None,                resizable: false },
    WindowSpec { id: "action_grid",  title: "Actions",      default_anchor: egui::Align2::CENTER_BOTTOM, default_offset: [0.0, -8.0],  default_size: None,                resizable: false },
    WindowSpec { id: "inventory",    title: "Inventory",    default_anchor: egui::Align2::LEFT_TOP,      default_offset: [8.0, 90.0],  default_size: Some([340.0, 420.0]), resizable: true  },
    WindowSpec { id: "group_roster", title: "Group",        default_anchor: egui::Align2::RIGHT_TOP,     default_offset: [-10.0, 220.0], default_size: Some([220.0, 160.0]), resizable: true  },
    WindowSpec { id: "npc_dialogue", title: "NPC Dialogue", default_anchor: egui::Align2::CENTER_TOP,    default_offset: [0.0, 36.0],  default_size: Some([460.0, 140.0]), resizable: true  },
    WindowSpec { id: "merchant",     title: "Merchant",     default_anchor: egui::Align2::CENTER_CENTER, default_offset: [0.0, 0.0],   default_size: Some([420.0, 460.0]), resizable: true  },
];

fn spec(id: &str) -> &'static WindowSpec {
    WINDOW_SPECS.iter().find(|s| s.id == id).expect("unknown window id")
}

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

/// Load spells01..07.tga as egui textures. Returns [] if the directory/files are absent
/// (graceful — gems fall back to text labels).
pub fn load_spell_icons(ctx: &egui::Context) -> Vec<egui::TextureHandle> {
    // Spell-gem icons (spells01..07.tga) come from the EQ UI files. Point
    // EQ_SPELL_ICONS_DIR at a directory containing them; absent that, gems fall
    // back to text labels.
    let Ok(dir) = std::env::var("EQ_SPELL_ICONS_DIR") else { return Vec::new(); };
    let mut out = Vec::new();
    for n in 1..=7 {
        let path = format!("{dir}/spells0{n}.tga");
        match image::open(&path) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                let tex = ctx.load_texture(
                    format!("spellicons{n}"),
                    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba),
                    egui::TextureOptions::NEAREST,
                );
                out.push(tex);
            }
            Err(_) => break,
        }
    }
    out
}

/// A top-center red banner shown when the server connection has gone silent (#8), so a frozen/dead
/// session is visible to a human player instead of looking like a normal idle scene.
pub fn draw_connection_banner(ctx: &egui::Context, disconnected: bool) {
    if !disconnected { return; }
    egui::Area::new(egui::Id::new("connection_banner"))
        .anchor(egui::Align2::CENTER_TOP, canvas_off(ctx, egui::Align2::CENTER_TOP, [0.0, 6.0]))
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
        .anchor(egui::Align2::LEFT_TOP, canvas_off(ctx, egui::Align2::LEFT_TOP, [8.0, 8.0]))
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
        .anchor(egui::Align2::LEFT_TOP, canvas_off(ctx, egui::Align2::LEFT_TOP, [8.0, 30.0]))
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

pub fn draw_hud(ctx: &egui::Context, layout: &mut UiLayout, scene: &SceneState, _bot_id: &str) {
    let base = egui::Frame::window(&ctx.style());
    managed_window(ctx, layout, spec("status_hud"), base, |ui| {
        ui.set_min_width(640.0);
        // Row 1: Zone / player / HP / strategy / target
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!(
                "Zone: {}",
                if scene.zone.is_empty() { "connecting…" } else { &scene.zone }
            )).strong());
            ui.separator();
            let class_suffix = if scene.player_class.is_empty() {
                String::new()
            } else {
                format!(" {}", scene.player_class)
            };
            ui.label(format!("{} (L{}{})", scene.player_name, scene.player_level, class_suffix));

            // Coin on hand (only once a profile has been received).
            let [pp, gp, sp, cp] = scene.coin;
            if pp | gp | sp | cp != 0 {
                ui.separator();
                ui.label(egui::RichText::new(format!("{}p {}g {}s {}c", pp, gp, sp, cp))
                    .color(egui::Color32::from_rgb(225, 205, 120)));
            }

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

        // Row 3: character stats (only once a player profile has arrived).
        if scene.stats.iter().any(|&s| s != 0) {
            let [str_, sta, cha, dex, int_, agi, wis] = scene.stats;
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!(
                    "STR {}  STA {}  AGI {}  DEX {}  WIS {}  INT {}  CHA {}",
                    str_, sta, agi, dex, wis, int_, cha))
                    .weak().monospace());
            });
        }
    });
}

/// Small top-left "UI" menu: global Lock toggle + Reset all windows.
/// Not a managed window (it must always be reachable).
pub fn draw_ui_menu(ctx: &egui::Context, layout: &mut UiLayout) {
    egui::Area::new(egui::Id::new("ui_menu"))
        .anchor(egui::Align2::LEFT_TOP, canvas_off(ctx, egui::Align2::LEFT_TOP, [8.0, 34.0]))
        .show(ctx, |ui| {
            ui.menu_button("\u{2699} UI", |ui| {
                let mut locked = layout.locked;
                if ui.checkbox(&mut locked, "Lock windows (Ctrl+L)").clicked() {
                    layout.locked = locked; layout.set_dirty_locked();
                }
                if ui.button("Reset all windows").clicked() {
                    layout.reset_all(); ui.close_menu();
                }
            });
        });
}

/// Split dialogue text into `(segment, is_keyword)` runs, where keywords are the
/// `[bracketed]` phrases EQ NPCs use for quest responses. Pure (no egui) so it can be
/// unit-tested; the dialogue panel renders keyword runs highlighted.
pub fn split_keywords(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        if open > 0 {
            out.push((rest[..open].to_string(), false));
        }
        if let Some(close_rel) = rest[open..].find(']') {
            let close = open + close_rel;
            out.push((rest[open..=close].to_string(), true));
            rest = &rest[close + 1..];
        } else {
            out.push((rest[open..].to_string(), false));
            rest = "";
            break;
        }
    }
    if !rest.is_empty() {
        out.push((rest.to_string(), false));
    }
    out
}

/// The NPC billboard nearest the player. Skips level-0 placeholder spawns and the
/// off-map zone controller. Pure, so it can be unit-tested.
pub fn nearest_npc(scene: &SceneState) -> Option<&crate::scene::Billboard> {
    let p = scene.player_pos; // [east, north, height]
    scene.billboards.iter()
        .filter(|b| b.level > 0 && !b.name.contains("zone_controller"))
        .map(|b| {
            let de = b.pos[0] - p[0];
            let dn = b.pos[1] - p[1];
            (b, de * de + dn * dn)
        })
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(b, _)| b)
}

/// Cleaned display name of the nearest NPC (for the Hail button).
#[allow(dead_code)]
pub fn nearest_npc_name(scene: &SceneState) -> Option<String> {
    nearest_npc(scene).map(|b| crate::http::clean_entity_name(&b.name))
}

/// Dedicated panel for NPC dialogue (kind "npc"), e.g. quest-giver responses to a hail.
/// Bracketed [keywords] are highlighted and clickable — clicking one says it back so the
/// player can follow a quest conversation without typing.
pub fn draw_quest_dialogue(ctx: &egui::Context, layout: &mut UiLayout, scene: &SceneState, say: &crate::http::SayReq) {
    let visible: Vec<_> = scene.messages.iter()
        .filter(|m| m.kind == "npc" && m.timestamp.elapsed().as_secs() < 45)
        .collect();
    if visible.is_empty() {
        return;
    }
    let base = egui::Frame::none()
        .fill(egui::Color32::from_black_alpha(200))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(180, 150, 60)))
        .inner_margin(egui::Margin::same(8.0));
    managed_window(ctx, layout, spec("npc_dialogue"), base, |ui| {
        for entry in &visible {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (seg, is_kw) in split_keywords(&entry.text) {
                    if is_kw {
                        let label = egui::Label::new(egui::RichText::new(&seg).size(13.0)
                            .strong().color(egui::Color32::from_rgb(255, 225, 90)))
                            .sense(egui::Sense::click());
                        if ui.add(label).on_hover_text("Click to say this keyword").clicked() {
                            let kw = seg.trim_start_matches('[').trim_end_matches(']').to_string();
                            *say.lock().unwrap() = Some(kw);
                        }
                    } else {
                        ui.label(egui::RichText::new(&seg).size(13.0)
                            .color(egui::Color32::from_rgb(225, 225, 205)));
                    }
                }
            });
        }
    });
}

/// Titanium worn-equipment slot ids → display labels (0-21).
const WORN_SLOTS: [(i32, &str); 22] = [
    (0, "Charm"), (1, "Ear"), (2, "Head"), (3, "Face"), (4, "Ear"), (5, "Neck"),
    (6, "Shoulders"), (7, "Arms"), (8, "Back"), (9, "Wrist"), (10, "Wrist"), (11, "Range"),
    (12, "Hands"), (13, "Primary"), (14, "Secondary"), (15, "Finger"), (16, "Finger"),
    (17, "Chest"), (18, "Legs"), (19, "Feet"), (20, "Waist"), (21, "Ammo"),
];

/// Inventory/equipment window + a toggle button (top-left). `show` is owned by the App (toggled
/// here or by the I key). Data comes from `scene.inventory` (decoded from OP_CharInventory).
pub fn draw_inventory(ctx: &egui::Context, layout: &mut UiLayout, scene: &SceneState, show: &mut bool) {
    // Top-left under the UI menu, so it doesn't overlap it. Moved to [8.0, 60.0] (UI menu is at 34.0).
    egui::Area::new(egui::Id::new("inv_toggle"))
        .anchor(egui::Align2::LEFT_TOP, canvas_off(ctx, egui::Align2::LEFT_TOP, [8.0, 60.0]))
        .show(ctx, |ui| {
            if ui.button("🎒 Inventory (I)").clicked() {
                *show = !*show;
            }
        });
    if !*show {
        return;
    }
    let base = egui::Frame::window(&ctx.style());
    managed_window(ctx, layout, spec("inventory"), base, |ui| {
        let inv = &scene.inventory;
        ui.label(egui::RichText::new("Equipped").strong().size(14.0));
        egui::Grid::new("equip_grid").num_columns(2).striped(true).show(ui, |ui| {
            for (slot, label) in WORN_SLOTS {
                let item = inv.iter().find(|i| i.slot == slot);
                ui.label(label);
                match item {
                    Some(i) => ui.label(egui::RichText::new(&i.name).color(egui::Color32::from_rgb(220, 220, 120))),
                    None => ui.label(egui::RichText::new("—").weak()),
                };
                ui.end_row();
            }
        });
        ui.separator();
        ui.label(egui::RichText::new("Inventory").strong().size(14.0));
        let mut bag: Vec<_> = inv.iter().filter(|i| i.slot >= 22).collect();
        bag.sort_by_key(|i| i.slot);
        if bag.is_empty() {
            ui.label(egui::RichText::new("(empty)").weak());
        }
        for i in &bag {
            let qty = if i.charges > 1 { format!(" x{}", i.charges) } else { String::new() };
            ui.label(format!("• {}{}", i.name, qty));
        }
        ui.separator();
        ui.label(format!("Coin: {}p {}g {}s {}c",
            scene.coin[0], scene.coin[1], scene.coin[2], scene.coin[3]));
        if inv.is_empty() {
            ui.label(egui::RichText::new("(waiting for inventory from server…)").weak());
        }
    });
}

/// Always-on group roster panel (RIGHT_TOP, below the minimap). Hidden entirely when not
/// grouped. Shows each member's name, level, leader/role badges, and an HP% bar — HP comes from
/// the same OP_MobHealth pathway used for targetable mobs (see GameState.group_members /
/// Navigator::sync_group), so it lags real-time HP by up to one server health-update tick, same
/// as any other entity's HP bar in this client.
pub fn draw_group_roster(ctx: &egui::Context, layout: &mut UiLayout, scene: &SceneState) {
    if scene.group_members.is_empty() {
        return;
    }
    let base = egui::Frame::window(&ctx.style());
    managed_window(ctx, layout, spec("group_roster"), base, |ui| {
        for m in &scene.group_members {
            let mut label = m.name.clone();
            if m.is_leader { label.push_str(" 👑"); }
            if m.tank { label.push_str(" [T]"); }
            if m.assist { label.push_str(" [A]"); }
            if m.puller { label.push_str(" [P]"); }
            if m.offline { label.push_str(" (offline)"); }
            ui.label(format!("{label} (lvl {})", m.level));
            let hp_pct = if m.name == scene.player_name { scene.player_hp_pct } else {
                scene.billboards.iter().find(|b| b.name == m.name).map(|b| b.hp_pct).unwrap_or(0.0)
            };
            stat_bar(ui, "HP", hp_pct, 140.0, 10.0, egui::Color32::from_rgb(180, 40, 40));
        }
    });
}

/// Floating control bar (bottom-right): Hail the nearest NPC and a say box for
/// chatting / quest replies. Buttons write shared request slots the nav thread drains.
pub fn draw_control_bar(
    ctx:        &egui::Context,
    layout:     &mut UiLayout,
    scene:      &SceneState,
    hail:       &crate::http::HailReq,
    say:        &crate::http::SayReq,
    target:     &crate::http::TargetReq,
    say_buffer: &mut String,
    camp:       &crate::http::CampReq,
    camp_until: &crate::http::CampUntil,
) {
    let base = egui::Frame::none()
        .fill(egui::Color32::from_black_alpha(170))
        .inner_margin(egui::Margin::symmetric(8.0, 4.0));
    managed_window(ctx, layout, spec("control_bar"), base, |ui| {
        ui.horizontal(|ui| {
            // Resolve the nearest NPC once (id for targeting, clean name for labels).
            let nearest = nearest_npc(scene)
                .map(|b| (b.id, crate::http::clean_entity_name(&b.name)));

            // Target nearest → OP_TargetCommand + auto-consider.
            if ui.add_enabled(nearest.is_some(), egui::Button::new("Target nearest")).clicked() {
                if let Some((id, _)) = &nearest {
                    *target.lock().unwrap() = Some(*id);
                }
            }

            let hail_label = match &nearest {
                Some((_, n)) => format!("Hail {}", n),
                None => "Hail nearest".to_string(),
            };
            if ui.add_enabled(nearest.is_some(), egui::Button::new(hail_label)).clicked() {
                if let Some((_, n)) = nearest {
                    *hail.lock().unwrap() = Some(n);
                }
            }

            // Camp button. While a camp is in progress its label shows the countdown and a second
            // click cancels it (Toggle); otherwise it starts a camp. The gameplay loop owns the
            // actual OP_Camp / cancel + the clean shutdown once the timer elapses.
            let remaining = camp_until.lock().unwrap()
                .map(|d| d.saturating_duration_since(std::time::Instant::now()).as_secs());
            let camp_label = match remaining {
                Some(secs) => format!("Cancel camp ({}s)", secs),
                None       => "Camp".to_string(),
            };
            if ui.button(camp_label).clicked() {
                *camp.lock().unwrap() = Some(crate::http::CampCmd::Toggle);
            }

            ui.separator();
            ui.label("Say:");
            let resp = ui.add(egui::TextEdit::singleline(say_buffer)
                .id(egui::Id::new("say_box"))   // stable ID so focus persists across frames
                .desired_width(260.0)
                .hint_text("message / quest keyword"));
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("Send").clicked() || enter) && !say_buffer.trim().is_empty() {
                *say.lock().unwrap() = Some(say_buffer.trim().to_string());
                say_buffer.clear();
            }
        });
    });
}

/// Bottom-center action grid: attack toggle, sit/stand toggle, target/consider, and the 9
/// memorized spell gems. Buttons write the same request slots the HTTP API uses.
pub fn draw_action_grid(
    ctx:      &egui::Context,
    layout:   &mut UiLayout,
    scene:    &SceneState,
    spells:   &crate::spells::SpellDb,
    icons:    &[egui::TextureHandle],
    attack:   &crate::http::AttackReq,
    cast:     &crate::http::CastReq,
    sit:      &crate::http::SitReq,
    target:   &crate::http::TargetReq,
    consider: &crate::http::ConsiderReq,
) {
    let base = egui::Frame::none()
        .fill(egui::Color32::from_black_alpha(170))
        .inner_margin(egui::Margin::symmetric(8.0, 4.0));
    managed_window(ctx, layout, spec("action_grid"), base, |ui| {
        if let Some(c) = &scene.casting {
            let frac = (c.started.elapsed().as_secs_f32()
                / (c.cast_ms.max(1) as f32 / 1000.0)).clamp(0.0, 1.0);
            let label = spells.get(c.spell_id).map(|s| s.name.clone())
                .unwrap_or_else(|| format!("Spell {}", c.spell_id));
            ui.add(egui::ProgressBar::new(frac).text(format!("Casting {label}")));
        }
        ui.horizontal(|ui| {
            let atk = egui::Button::new("\u{2694} Attack")
                .fill(if scene.auto_attack { egui::Color32::from_rgb(150, 40, 40) }
                      else { egui::Color32::from_gray(50) });
            if ui.add(atk).clicked() {
                *attack.lock().unwrap() = Some(!scene.auto_attack);
            }
            let sit_label = if scene.sitting { "Stand" } else { "Sit" };
            if ui.button(sit_label).clicked() {
                *sit.lock().unwrap() = Some(!scene.sitting);
            }
            let nearest = nearest_npc(scene).map(|b| b.id);
            if ui.add_enabled(nearest.is_some(), egui::Button::new("Target")).clicked() {
                if let Some(id) = nearest { *target.lock().unwrap() = Some(id); }
            }
            if ui.add_enabled(scene.target_id.is_some(), egui::Button::new("Con")).clicked() {
                if let Some(id) = scene.target_id { *consider.lock().unwrap() = Some(id); }
            }
        });
        ui.horizontal(|ui| {
            for (gem, &spell_id) in scene.mem_spells.iter().enumerate() {
                let empty = spell_id == 0 || spell_id == 0xFFFF_FFFF;
                // Icon path: non-empty gem + a loaded sheet for this spell's icon.
                if !empty {
                    if let Some(info) = spells.get(spell_id) {
                        let (sheet, col, row) = crate::spells::icon_cell(info.icon_id);
                        if let Some(tex) = icons.get(sheet) {
                            let cw = 1.0 / crate::spells::ICON_COLS as f32;
                            let ch = 1.0 / crate::spells::ICON_ROWS as f32;
                            let uv = egui::Rect::from_min_size(
                                egui::pos2(col as f32 * cw, row as f32 * ch),
                                egui::vec2(cw, ch));
                            let img = egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                                .uv(uv)
                                .fit_to_exact_size(egui::vec2(36.0, 36.0));
                            let resp = ui.add(egui::ImageButton::new(img))
                                .on_hover_text(&info.name);
                            if resp.clicked() {
                                *cast.lock().unwrap() = Some(crate::http::CastRequest {
                                    gem: gem as u8, target_id: None,
                                });
                            }
                            continue;
                        }
                    }
                }
                // Text fallback: empty gem, or no icon sheet loaded.
                let label = if empty { "\u{2014}".to_string() }
                    else { spells.get(spell_id).map(|s| s.name.clone())
                             .unwrap_or_else(|| format!("{spell_id}")) };
                let btn = egui::Button::new(egui::RichText::new(label).size(11.0))
                    .min_size(egui::vec2(56.0, 28.0));
                if ui.add_enabled(!empty, btn).clicked() {
                    *cast.lock().unwrap() = Some(crate::http::CastRequest {
                        gem: gem as u8, target_id: None,
                    });
                }
            }
        });
    });
}

/// Merchant window — shown only while a merchant session is open (`scene.merchant_open`). Left
/// column: the merchant's wares with a Buy button each; right column: the player's general/bag
/// inventory with a Sell button each. Buttons write the same request slots the `/trade/*` HTTP API
/// uses (buy/sell/trade), so HUD and API stay in sync. A Close button ends the session.
pub fn draw_merchant(
    ctx:    &egui::Context,
    layout: &mut UiLayout,
    scene:  &SceneState,
    buy:    &crate::http::BuyReq,
    sell:   &crate::http::SellReq,
    trade:  &crate::http::TradeReq,
) {
    let Some(merchant_id) = scene.merchant_open else { return };
    let base = egui::Frame::none()
        .fill(egui::Color32::from_black_alpha(220))
        .inner_margin(egui::Margin::same(8.0));
    managed_window(ctx, layout, spec("merchant"), base, |ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Merchant").strong().size(15.0));
            if ui.button("Close").clicked() {
                *trade.lock().unwrap() = Some(crate::http::TradeCmd::Close);
            }
        });
        let coin = scene.coin;
        ui.label(egui::RichText::new(format!(
            "Your coin: {}p {}g {}s {}c", coin[0], coin[1], coin[2], coin[3]
        )).size(11.0).color(egui::Color32::LIGHT_YELLOW));
        ui.separator();
        ui.columns(2, |cols| {
            // ── Buy column: merchant wares ──
            cols[0].label(egui::RichText::new("For sale").strong());
            egui::ScrollArea::vertical().id_salt("merch_buy").show(&mut cols[0], |ui| {
                if scene.merchant_items.is_empty() {
                    ui.label(egui::RichText::new("(no items / loading…)").weak().size(11.0));
                }
                for it in &scene.merchant_items {
                    ui.horizontal(|ui| {
                        let label = format!("{} ({}p{}g{}s{}c)",
                            it.name,
                            it.price / 1000, (it.price / 100) % 10,
                            (it.price / 10) % 10, it.price % 10);
                        if ui.add(egui::Button::new(egui::RichText::new("Buy").size(10.0))).clicked() {
                            *buy.lock().unwrap() = Some((merchant_id, it.merchant_slot));
                        }
                        ui.label(egui::RichText::new(label).size(11.0));
                    });
                }
            });
            // ── Sell column: player's general/bag inventory (slot >= 22) ──
            cols[1].label(egui::RichText::new("Your items").strong());
            egui::ScrollArea::vertical().id_salt("merch_sell").show(&mut cols[1], |ui| {
                let sellable: Vec<_> = scene.inventory.iter().filter(|i| i.slot >= 22).collect();
                if sellable.is_empty() {
                    ui.label(egui::RichText::new("(nothing to sell)").weak().size(11.0));
                }
                for it in sellable {
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(egui::RichText::new("Sell").size(10.0))).clicked() {
                            *sell.lock().unwrap() = Some((merchant_id, it.slot as u32, it.charges.max(1) as u32));
                        }
                        let qty = if it.charges > 1 { format!(" x{}", it.charges) } else { String::new() };
                        ui.label(egui::RichText::new(format!("{}{}", it.name, qty)).size(11.0));
                    });
                }
            });
        });
    });
}

pub fn draw_message_log(ctx: &egui::Context, layout: &mut UiLayout, scene: &SceneState) {
    let visible: Vec<_> = scene.messages.iter()
        // NPC dialogue has its own panel (draw_quest_dialogue); keep it out of here.
        .filter(|m| m.kind != "npc" && m.timestamp.elapsed().as_secs() < 30)
        .collect();
    if visible.is_empty() {
        return;
    }

    let base = egui::Frame::none()
        .fill(egui::Color32::TRANSPARENT)
        .inner_margin(egui::Margin::same(4.0));
    managed_window(ctx, layout, spec("message_log"), base, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        for entry in &visible {
            let color = match entry.kind.as_str() {
                "combat"  => egui::Color32::from_rgb(220, 110, 110),
                "zone"    => egui::Color32::from_rgb(160, 160, 160),
                "exp"     => egui::Color32::from_rgb(220, 175,  40),
                "chat"    => egui::Color32::from_rgb(210, 210, 255),
                _         => egui::Color32::from_rgb(200, 200, 200),
            };
            ui.label(egui::RichText::new(&entry.text).size(11.0).color(color));
        }
    });
}

pub fn draw_minimap(
    ctx:          &egui::Context,
    layout:       &mut UiLayout,
    scene:        &SceneState,
    zone_min:     [f32; 2],  // [min_east, min_north] in map coords
    zone_max:     [f32; 2],  // [max_east, max_north] in map coords
    zoom:         &mut f32,
    fullscreen:   &mut bool,
    zone_map:     Option<&ZoneMap>,
    show:         &mut bool,
) {
    // Toggle button (top-left), stacked below the UI menu (34) and Inventory (60).
    egui::Area::new(egui::Id::new("map_toggle"))
        .anchor(egui::Align2::LEFT_TOP, canvas_off(ctx, egui::Align2::LEFT_TOP, [8.0, 86.0]))
        .show(ctx, |ui| {
            if ui.button("\u{1f5fa} Map").clicked() {
                *show = !*show;
            }
        });
    if !*show {
        return;
    }

    let zone_w = (zone_max[0] - zone_min[0]).max(1.0);
    let zone_h = (zone_max[1] - zone_min[1]).max(1.0);

    // scene.player_pos = [east, north, height] = [server_x, server_y, server_z].
    let player_map = [scene.player_pos[0], scene.player_pos[1]]; // [east, north]

    let map_px = if *fullscreen { 580.0_f32 } else { 200.0_f32 };
    let map_py = if *fullscreen { 580.0_f32 } else { 200.0_f32 };
    let map_size = egui::Vec2::new(map_px, map_py);

    // Fullscreen: use a NON-managed centered window (not draggable/persisted)
    if *fullscreen {
        let offset = canvas_off(ctx, egui::Align2::CENTER_CENTER, [0.0_f32, 0.0_f32]);
        egui::Window::new("##minimap_full")
            .title_bar(false)
            .anchor(egui::Align2::CENTER_CENTER, offset)
            .resizable(false)
            .collapsible(false)
            .frame(egui::Frame::none())
            .show(ctx, |ui| {
                let (resp, painter) = ui.allocate_painter(map_size, egui::Sense::click());
                let rect = resp.rect;

                if resp.hovered() {
                    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                    if scroll.abs() > 0.5 {
                        *zoom = (*zoom * (1.0 + scroll * 0.005)).clamp(0.25, 8.0);
                    }
                }

                if resp.clicked() {
                    *fullscreen = !*fullscreen;
                }

                painter.rect_filled(rect, 4.0, egui::Color32::from_black_alpha(210));

                let view_w = zone_w / *zoom;
                let view_h = zone_h / *zoom;
                let half_w = view_w * 0.5;
                let half_h = view_h * 0.5;
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

                let to_screen = |east: f32, north: f32| -> egui::Pos2 {
                    let nx = (east  - view_left) / view_w;
                    let ny = (north - view_bot)  / view_h;
                    egui::pos2(
                        rect.min.x + nx * rect.width(),
                        rect.max.y - ny * rect.height(),
                    )
                };

                if let Some(zm) = zone_map {
                    for line in &zm.lines {
                        let p1 = to_screen(line.east1, line.north1);
                        let p2 = to_screen(line.east2, line.north2);
                        if !rect.contains(p1) && !rect.contains(p2) { continue; }
                        let color = egui::Color32::from_rgba_unmultiplied(
                            line.r, line.g, line.b, 180,
                        );
                        painter.line_segment([p1, p2], egui::Stroke::new(0.8, color));
                    }
                }

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

                for b in &scene.billboards {
                    let sp = to_screen(b.pos[0], b.pos[1]);
                    if !rect.contains(sp) { continue; }
                    let color = if b.dead {
                        egui::Color32::from_rgb(80, 80, 80)
                    } else if b.is_target {
                        egui::Color32::from_rgb(255, 80, 80)
                    } else {
                        egui::Color32::from_rgb(200, 100, 60)
                    };
                    painter.circle_filled(sp, 4.0, color);
                }

                let pp = to_screen(player_map[0], player_map[1]);
                painter.circle_filled(pp, 6.0, egui::Color32::from_rgb(80, 180, 255));

                let hr = scene.player_heading.to_radians();
                let arrow_len = 16.0;
                let arrow_tip = egui::pos2(
                    pp.x + hr.sin() * arrow_len,
                    pp.y - hr.cos() * arrow_len,
                );
                painter.line_segment(
                    [pp, arrow_tip],
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 180, 255)),
                );

                painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.0, egui::Color32::from_rgb(90, 90, 120)));
            });
        return;
    }

    // Small map: route through managed_window. The canvas tracks the window's
    // available width (square), so dragging the resize handle actually grows the map.
    let base = egui::Frame::none();
    managed_window(ctx, layout, spec("minimap"), base, |ui| {
        let side = ui.available_width().clamp(120.0, 1024.0);
        let (resp, painter) = ui.allocate_painter(egui::Vec2::splat(side), egui::Sense::click());
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

        // Entity dots — billboard.pos = [east, north, height] in GPU space.
        for b in &scene.billboards {
            let sp = to_screen(b.pos[0], b.pos[1]);
            if !rect.contains(sp) { continue; }
            let color = if b.dead {
                egui::Color32::from_rgb(80, 80, 80)
            } else if b.is_target {
                egui::Color32::from_rgb(255, 80, 80)
            } else {
                egui::Color32::from_rgb(200, 100, 60)
            };
            painter.circle_filled(sp, 3.0, color);
        }

        // Player dot + heading arrow
        let pp = to_screen(player_map[0], player_map[1]);
        painter.circle_filled(pp, 5.0, egui::Color32::from_rgb(80, 180, 255));

        // EQ heading: 0 = north, clockwise. Screen: north = up (−screen_y).
        let hr = scene.player_heading.to_radians();
        let arrow_len = 10.0;
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
        painter.text(
            egui::pos2(rect.min.x + 4.0, rect.max.y - 14.0),
            egui::Align2::LEFT_BOTTOM,
            "scroll=zoom  click=fullscreen",
            egui::FontId::proportional(9.0),
            egui::Color32::from_white_alpha(80),
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
        .anchor(egui::Align2::LEFT_TOP, canvas_off(ctx, egui::Align2::LEFT_TOP, [8.0, 28.0]))
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

        // Golden "!" over NPCs that have a quest (data from data/quests.json), MMO-style, so an
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{Billboard, SceneState};

    #[test]
    fn split_keywords_marks_bracketed_runs() {
        let parts = split_keywords("Greetings. Are you [my contact]? Tell me about the [shipment].");
        let kws: Vec<&str> = parts.iter().filter(|(_, k)| *k).map(|(s, _)| s.as_str()).collect();
        assert_eq!(kws, vec!["[my contact]", "[shipment]"]);
        // Reassembling the segments reproduces the original text exactly.
        let joined: String = parts.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(joined, "Greetings. Are you [my contact]? Tell me about the [shipment].");
    }

    fn bb(id: u32, name: &str, level: u32, pos: [f32; 3]) -> Billboard {
        Billboard {
            id, pos, level, hp_pct: 100.0, is_target: false, dead: false,
            name: name.to_string(), race: String::new(), action: String::new(), heading: 0.0,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 0,
            face: 0, hairstyle: 0, haircolor: 0, helm: 0, showhelm: 0,
        }
    }

    #[test]
    fn nearest_npc_name_picks_closest_and_cleans_name() {
        let mut scene = SceneState::default();
        scene.player_pos = [0.0, 0.0, 0.0]; // [east, north, height]
        scene.billboards = vec![
            bb(1, "Far_Guard001", 5, [100.0, 0.0, 0.0]),
            bb(2, "Guard_Phaeton000", 20, [5.0, 5.0, 0.0]), // closest
            bb(3, "zone_controller000", 1, [1.0, 1.0, 0.0]), // skipped (controller)
            bb(4, "Placeholder000", 0, [0.5, 0.5, 0.0]),     // skipped (level 0)
        ];
        assert_eq!(nearest_npc_name(&scene).as_deref(), Some("Guard Phaeton"));
    }

    #[test]
    fn nearest_npc_name_none_when_no_real_npcs() {
        let mut scene = SceneState::default();
        scene.billboards = vec![bb(1, "zone_controller000", 1, [1.0, 1.0, 0.0])];
        assert_eq!(nearest_npc_name(&scene), None);
    }

    #[test]
    fn split_keywords_handles_plain_and_unclosed() {
        assert_eq!(split_keywords("plain text"), vec![("plain text".to_string(), false)]);
        // An unclosed '[' is treated as literal text, not a keyword.
        let parts = split_keywords("a [b");
        assert!(parts.iter().all(|(_, k)| !*k), "unclosed bracket must not be a keyword");
        let joined: String = parts.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(joined, "a [b");
    }

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
                    equipment: [0; 9],
                    equipment_tint: [[0; 3]; 9],
                    gender: 0,
                    face: 0,
                    hairstyle: 0,
                    haircolor: 0,
                    helm: 0,
                    showhelm: 0,
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

    #[test]
    fn managed_hud_windows_do_not_panic() {
        let ctx = egui::Context::default();
        let scene = make_scene();
        for locked in [false, true] {
            let mut layout = crate::ui_layout::UiLayout::from_path(
                std::env::temp_dir().join(format!("ui_layout___hud_smoke_{locked}.json")));
            layout.locked = locked;
            let say: crate::http::SayReq = std::sync::Arc::new(std::sync::Mutex::new(None));
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                draw_hud(ctx, &mut layout, &scene, "x");
                draw_message_log(ctx, &mut layout, &scene);
                draw_quest_dialogue(ctx, &mut layout, &scene, &say);
            });
        }
    }
}
