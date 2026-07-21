//! Player window — native `PlayerWindow` + the stats block of `Inventory`.
//! HP/mana/XP gauges with the native FillTints, name/level/class, STR–WIS, coin.

use crate::{theme, widgets, UiCtx};

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(&s.player_name).strong().size(14.0));
        ui.label(
            egui::RichText::new(format!("{} {}", s.player_level, s.player_class))
                .color(theme::TEXT_WEAK)
                .size(11.0),
        );
        if s.sitting {
            ui.label(egui::RichText::new("(sitting)").color(theme::TEXT_WEAK).size(10.0));
        }
    });
    widgets::gauge(ui, "hp", "HP", s.player_hp_pct / 100.0, theme::HP, true);
    widgets::gauge(ui, "mana", "Mana", s.player_mana_pct / 100.0, theme::MANA, true);
    widgets::gauge(ui, "xp", "XP", s.player_xp_pct / 100.0, theme::XP, true);
    ui.add_space(2.0);
    widgets::coin_row(ui, s.coin);

    // STR..WIS block (collapsed by default to keep the window tight).
    egui::CollapsingHeader::new(egui::RichText::new("Stats").size(11.0))
        .default_open(false)
        .show(ui, |ui| {
            const NAMES: [&str; 7] = ["STR", "STA", "CHA", "DEX", "INT", "AGI", "WIS"];
            egui::Grid::new("stat_grid").num_columns(4).spacing([10.0, 2.0]).show(ui, |ui| {
                for (i, name) in NAMES.iter().enumerate() {
                    ui.label(egui::RichText::new(*name).color(theme::TEXT_WEAK).size(11.0));
                    ui.label(egui::RichText::new(format!("{}", s.stats[i])).size(11.0));
                    if i % 2 == 1 {
                        ui.end_row();
                    }
                }
            });
        });
}
