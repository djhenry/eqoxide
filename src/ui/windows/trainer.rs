//! Trainer window — native `TrainWindow`. Transient: forced open while
//! `scene.trainer_open` holds the trainer NPC's spawn id.
//!
//! `scene.trainer_skills[id]` is the cap this trainer offers for skill `id`
//! (same indexing as `scene.player_skills`); a skill is trainable when the
//! offered cap exceeds the player's current value. Clicking Train stores the
//! skill id into the shared `trainer_train` slot (→ OP_GMTrainSkill).

use crate::ui::{theme, UiCtx};

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;

    // Header: who is training us.
    let trainer_name = s.trainer_open.and_then(|id| {
        s.billboards
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.name.clone())
    });
    ui.label(
        egui::RichText::new(trainer_name.as_deref().unwrap_or("Trainer"))
            .strong()
            .size(13.0),
    );
    ui.separator();

    // Trainable skills: offered cap > current value, and a known skill id.
    let trainable: Vec<(u32, u32, u32)> = s
        .trainer_skills
        .iter()
        .enumerate()
        .filter_map(|(id, &cap)| {
            let id = id as u32;
            crate::skills::skill_name(id)?;
            let cur = s.player_skills.get(id as usize).copied().unwrap_or(0);
            (cap > cur).then_some((id, cur, cap))
        })
        .collect();

    if trainable.is_empty() {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Nothing to train here")
                .color(theme::TEXT_WEAK)
                .size(11.0),
        );
    } else {
        egui::ScrollArea::vertical()
            .max_height(ui.available_height() - 24.0)
            .show(ui, |ui| {
                egui::Grid::new("trainer_skills")
                    .num_columns(3)
                    .spacing([10.0, 3.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for (id, cur, cap) in &trainable {
                            let name =
                                crate::skills::skill_name(*id).unwrap_or("Unknown Skill");
                            ui.label(egui::RichText::new(name).size(11.0));
                            ui.label(
                                egui::RichText::new(format!("{cur} \u{2192} {cap}"))
                                    .color(theme::GOLD)
                                    .size(11.0),
                            );
                            if ui
                                .add(egui::Button::new(
                                    egui::RichText::new("Train").size(10.0),
                                ))
                                .clicked()
                            {
                                cx.acts.command.request_train_skill(*id);
                            }
                            ui.end_row();
                        }
                    });
            });
    }

    ui.add_space(2.0);
    ui.label(
        egui::RichText::new("Training costs are applied server-side.")
            .color(theme::TEXT_WEAK)
            .size(10.0),
    );
}
