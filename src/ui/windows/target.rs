//! Target window — native `TargetWindow`. Name colored by consider color,
//! HP gauge (`-?-` when unknown, like the native client), level when the
//! target's spawn is known, and Attack / Consider actions.

use crate::ui::{theme, widgets, UiCtx};

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;

    let (Some(target_id), Some(name)) = (s.target_id, s.target_name.as_ref()) else {
        ui.label(egui::RichText::new("No target").color(theme::TEXT_WEAK).size(11.0));
        return;
    };

    // Level (and dead-ness) comes from the target's billboard when the spawn
    // is in view; the native window shows just the name otherwise.
    let bb = s.billboards.iter().find(|b| b.is_target);
    let level = bb.map(|b| b.level).filter(|&l| l > 0);
    let dead = bb.map(|b| b.dead).unwrap_or(false);

    // Name row, tinted by the server consider color.
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(name.as_str())
                .color(widgets::con_color(s.target_con))
                .strong()
                .size(13.0),
        );
        if let Some(level) = level {
            ui.label(
                egui::RichText::new(format!("Lvl {level}"))
                    .color(theme::TEXT_WEAK)
                    .size(10.0),
            );
        }
        if dead {
            ui.label(egui::RichText::new("(dead)").color(theme::TEXT_WEAK).size(10.0));
        }
    });

    // HP gauge: native shows `-?-` until the server reports target HP.
    match s.target_hp_pct {
        Some(pct) => widgets::gauge(ui, "target_hp", "", pct / 100.0, theme::HP, true),
        None => widgets::gauge(ui, "target_hp", "-?-", 0.0, theme::HP, false),
    }

    ui.add_space(2.0);
    ui.horizontal(|ui| {
        // Attack toggle mirrors auto-attack state (gold outline while on).
        let attack_label = egui::RichText::new("Attack").size(11.0);
        let attack_btn = if s.auto_attack {
            egui::Button::new(attack_label).stroke(egui::Stroke::new(1.0, theme::GOLD))
        } else {
            egui::Button::new(attack_label)
        };
        let resp = ui.add(attack_btn).on_hover_text(if s.auto_attack {
            "Auto-attack ON — click to stop"
        } else {
            "Start auto-attack"
        });
        if resp.clicked() {
            cx.acts.command.request_attack(!s.auto_attack);
        }

        if ui
            .add(egui::Button::new(egui::RichText::new("Consider").size(11.0)))
            .on_hover_text("Consider the target")
            .clicked()
        {
            cx.acts.command.request_consider(target_id);
        }
    });
}
