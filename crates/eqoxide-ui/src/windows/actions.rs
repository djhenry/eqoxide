//! Actions window (native `ActionsWindow`) — the main button bar.
//!
//! One wrapped row of compact buttons: Attack toggle, Sit/Stand, Target
//! nearest, Hail, Consider, and Camp (with in-progress countdown). Ported from
//! the old HUD control bar + action grid; buttons write the same shared
//! request slots the agent API uses.

use eqoxide_renderer::scene::{Billboard, SceneState};
use crate::{theme, UiCtx};

/// The nearest *living* NPC billboard. Skips corpses, untargetable spawns and the off-map zone
/// controller.
fn nearest_living_npc(scene: &SceneState) -> Option<&Billboard> {
    let p = scene.player_pos; // [east, north, height]
    scene
        .billboards
        .iter()
        .filter(|b| !b.dead && !b.untargetable && !b.name.contains("zone_controller"))
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
    let nearest = nearest_living_npc(s).map(|b| (b.id, eqoxide_core::game_state::clean_entity_name(&b.name)));

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
            cx.acts.command.request_attack(!attack_on);
        }

        // Sit / Stand.
        let sit_label = if s.sitting { "Stand" } else { "Sit" };
        if ui.add(btn(sit_label)).clicked() {
            cx.acts.command.request_sit(!s.sitting);
        }

        // Run / Walk (#625). Label shows the action the click performs, matching Sit/Stand above.
        // Sends OP_SetRunMode — see `run_mode` on
        // GameState/SceneState for the honesty caveat (send-time intent, no server ack).
        let run_label = if s.run_mode { "Walk" } else { "Run" };
        if ui.add(btn(run_label)).on_hover_text(if s.run_mode {
            "Running — click to walk"
        } else {
            "Walking — click to run"
        }).clicked() {
            cx.acts.command.request_run_mode(!s.run_mode);
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
                cx.acts.command.request_target(*id);
            }
        }

        // Hail — the current target when we have one, else the nearest NPC.
        // Passing the id too makes the net thread target first (the server
        // only fires EVENT_SAY on the current target, #130).
        let hail_who = match (s.target_id, &s.target_name) {
            (Some(id), Some(name)) => Some((id, eqoxide_core::game_state::clean_entity_name(name))),
            _ => nearest.clone(),
        };
        let hail_label = match &hail_who {
            Some((_, n)) => format!("Hail {n}"),
            None => "Hail".to_string(),
        };
        if ui.add_enabled(hail_who.is_some(), btn(hail_label)).clicked() {
            if let Some((id, name)) = hail_who {
                cx.acts.command.request_hail(name, Some(id));
            }
        }

        // Consider the current target.
        if ui
            .add_enabled(s.target_id.is_some(), btn("Consider"))
            .on_hover_text("/consider the current target")
            .clicked()
        {
            if let Some(id) = s.target_id {
                cx.acts.command.request_consider(id);
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
                    cx.acts.command.request_camp(eqoxide_ipc::CampCmd::Toggle);
                }
                // Keep the countdown ticking even when nothing else repaints.
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            }
            None => {
                if ui.add(btn("Camp")).on_hover_text("Sit and camp to desktop").clicked() {
                    cx.acts.command.request_camp(eqoxide_ipc::CampCmd::Toggle);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bb(id: u32, name: &str, pos: [f32; 3], untargetable: bool) -> Billboard {
        Billboard {
            id, pos, level: 5, hp_pct: 100.0, is_target: false, dead: false,
            name: name.to_string(), race: String::new(), action: String::new(), heading: 0.0,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 0, face: 0, hairstyle: 0,
            haircolor: 0, helm: 0, showhelm: 0, floating: false, gait: None, untargetable,
        }
    }

    #[test]
    fn nearest_living_npc_skips_an_untargetable_boat() {
        let mut scene = SceneState { player_pos: [0.0, 0.0, 0.0], ..Default::default() };
        scene.billboards = vec![
            bb(1, "SirensBane000", [2.0, 2.0, 0.0], true),
            bb(2, "Guard_Phaeton000", [30.0, 30.0, 0.0], false),
        ];
        assert_eq!(nearest_living_npc(&scene).map(|b| b.id), Some(2));
    }

    #[test]
    fn nearest_living_npc_none_when_only_an_untargetable_boat() {
        let mut scene = SceneState { player_pos: [0.0, 0.0, 0.0], ..Default::default() };
        scene.billboards = vec![bb(1, "SirensBane000", [2.0, 2.0, 0.0], true)];
        assert!(nearest_living_npc(&scene).is_none());
    }
}
