//! Skills window (native `SkillsWindow`) — alphabetical name/value list of the
//! player's skills, hiding untrained (0) entries unless "Show untrained" is on.

use crate::ui::{theme, UiCtx};

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let skills = &cx.scene.player_skills;

    // Named skills only; the raw vec is indexed by skill id and padded.
    let mut rows: Vec<(&'static str, u32)> = skills
        .iter()
        .enumerate()
        .filter_map(|(id, &val)| crate::skills::skill_name(id as u32).map(|n| (n, val)))
        .collect();
    rows.sort_unstable_by_key(|&(name, _)| name);
    let trained = rows.iter().filter(|&&(_, v)| v > 0).count();

    // "Show untrained" persists as egui temp data (per-session, not saved).
    let show_id = ui.id().with("show_untrained");
    let mut show_untrained = ui.ctx().data_mut(|d| *d.get_temp_mut_or(show_id, false));

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!("{trained} trained"))
                .color(theme::TEXT_WEAK)
                .size(11.0),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .checkbox(&mut show_untrained, egui::RichText::new("Show untrained").size(10.0))
                .changed()
            {
                ui.ctx().data_mut(|d| d.insert_temp(show_id, show_untrained));
            }
        });
    });
    ui.separator();

    if !show_untrained {
        rows.retain(|&(_, v)| v > 0);
    }
    if rows.is_empty() {
        ui.label(
            egui::RichText::new("No trained skills.")
                .color(theme::TEXT_WEAK)
                .size(11.0),
        );
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Grid::new("skills_grid")
                .num_columns(2)
                .striped(true)
                .spacing([12.0, 2.0])
                .min_col_width(90.0)
                .show(ui, |ui| {
                    for (name, val) in rows {
                        ui.label(egui::RichText::new(name).size(11.0));
                        let color = if val > 0 { theme::TEXT } else { theme::TEXT_WEAK };
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    egui::RichText::new(format!("{val}")).color(color).size(11.0),
                                );
                            },
                        );
                        ui.end_row();
                    }
                });
        });
}
