//! Actions window (native `ActionsWindow`) — the main button bar.
//!
//! One wrapped row of compact buttons: Attack toggle, Sit/Stand, Target
//! nearest, Hail, Consider, and Camp (with in-progress countdown). Ported from
//! the old HUD control bar + action grid; buttons write the same shared
//! request slots the HTTP API uses.

use crate::scene::{Billboard, SceneState};
use crate::ui::{theme, UiCtx};

/// The nearest *living* NPC billboard. Skips corpses and the off-map zone
/// controller.
fn nearest_living_npc(scene: &SceneState) -> Option<&Billboard> {
    let p = scene.player_pos; // [east, north, height]
    scene
        .billboards
        .iter()
        .filter(|b| !b.dead && !b.name.contains("zone_controller"))
        .map(|b| {
            let de = b.pos[0] - p[0];
            let dn = b.pos[1] - p[1];
            (b, de * de + dn * dn)
        })
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(b, _)| b)
}

/// Compact button with an 11 pt label (the bar packs six of these).
fn btn(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).size(11.0))
}

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;
    ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);

    // Resolve the nearest living NPC once (id for targeting, clean name for labels).
    let nearest = nearest_living_npc(s).map(|b| (b.id, crate::http::clean_entity_name(&b.name)));

    ui.horizontal_wrapped(|ui| {
        // Attack toggle — highlighted red while auto-attack is on.
        let attack_on = s.auto_attack;
        let atk = btn("\u{2694} Attack").fill(if attack_on {
            egui::Color32::from_rgb(150, 40, 40)
        } else {
            theme::BTN_FACE
        });
        let hover = if attack_on { "Auto-attack ON — click to stop" } else { "Toggle auto-attack" };
        if ui.add(atk).on_hover_text(hover).clicked() {
            *cx.acts.attack.lock().unwrap() = Some(!attack_on);
        }

        // Sit / Stand.
        let sit_label = if s.sitting { "Stand" } else { "Sit" };
        if ui.add(btn(sit_label)).clicked() {
            *cx.acts.sit.lock().unwrap() = Some(!s.sitting);
        }

        // Target nearest living NPC.
        let target_hover = match &nearest {
            Some((_, n)) => format!("Target {n}"),
            None => "No NPC in range".to_string(),
        };
        if ui
            .add_enabled(nearest.is_some(), btn("Target nearest"))
            .on_hover_text(target_hover)
            .clicked()
        {
            if let Some((id, _)) = &nearest {
                *cx.acts.target.lock().unwrap() = Some(*id);
            }
        }

        // Hail — the current target when we have one, else the nearest NPC.
        // Passing the id too makes the nav thread target first (the server
        // only fires EVENT_SAY on the current target, #130).
        let hail_who = match (s.target_id, &s.target_name) {
            (Some(id), Some(name)) => Some((id, crate::http::clean_entity_name(name))),
            _ => nearest.clone(),
        };
        let hail_label = match &hail_who {
            Some((_, n)) => format!("Hail {n}"),
            None => "Hail".to_string(),
        };
        if ui.add_enabled(hail_who.is_some(), btn(hail_label)).clicked() {
            if let Some((id, name)) = hail_who {
                *cx.acts.hail.lock().unwrap() = Some((name, Some(id)));
            }
        }

        // Consider the current target.
        if ui
            .add_enabled(s.target_id.is_some(), btn("Consider"))
            .on_hover_text("/consider the current target")
            .clicked()
        {
            if let Some(id) = s.target_id {
                *cx.acts.consider.lock().unwrap() = Some(id);
            }
        }

        // Camp. While a camp is in progress the label shows the countdown and
        // a click cancels (Toggle); otherwise a click starts one. The gameplay
        // loop owns the actual OP_Camp / cancel / shutdown.
        let remaining = cx
            .acts
            .camp_until
            .lock()
            .unwrap()
            .map(|d| d.saturating_duration_since(std::time::Instant::now()).as_secs());
        match remaining {
            Some(secs) => {
                let camp = btn(format!("Camping\u{2026} {secs}s (cancel)"))
                    .fill(egui::Color32::from_rgb(0x50, 0x44, 0x20));
                if ui.add(camp).on_hover_text("Click to cancel camping").clicked() {
                    *cx.acts.camp.lock().unwrap() = Some(crate::http::CampCmd::Toggle);
                }
                // Keep the countdown ticking even when nothing else repaints.
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            }
            None => {
                if ui.add(btn("Camp")).on_hover_text("Sit and camp to desktop").clicked() {
                    *cx.acts.camp.lock().unwrap() = Some(crate::http::CampCmd::Toggle);
                }
            }
        }
    });
}
