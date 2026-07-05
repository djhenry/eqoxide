//! Pet window — minimal take on the native `PetInfoWindow`.
//!
//! Shows the current pet's name, level and HP (native pet-green FillTint).
//! Display-only for now: the nav/gameplay thread auto-drives pet attack and
//! there is no manual pet-command request slot yet (see integration notes).

use crate::ui::{theme, widgets, UiCtx};

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;

    let Some(pet_id) = s.pet_id else {
        ui.label(egui::RichText::new("No pet").color(theme::TEXT_WEAK).size(11.0));
        return;
    };

    let pet = s.billboards.iter().find(|b| b.id == pet_id);
    match pet {
        Some(b) => {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&b.name).strong().size(13.0));
                ui.label(
                    egui::RichText::new(format!("Lv {}", b.level))
                        .color(theme::TEXT_WEAK)
                        .size(10.0),
                );
                if b.dead {
                    ui.label(egui::RichText::new("(dead)").color(theme::HP).size(10.0));
                }
            });
            let frac = if b.dead { 0.0 } else { b.hp_pct / 100.0 };
            widgets::gauge(ui, "pet_hp", "HP", frac, theme::PET_HP, true);
        }
        None => {
            // Pet exists but its spawn isn't in this frame's snapshot
            // (e.g. wandered out of view or not yet spawned in).
            ui.label(egui::RichText::new("Pet").strong().size(13.0));
            ui.label(
                egui::RichText::new("(pet out of view)")
                    .color(theme::TEXT_WEAK)
                    .size(10.0),
            );
            widgets::gauge(ui, "pet_hp", "HP", 0.0, theme::PET_HP, false);
        }
    }

    ui.add_space(2.0);
    ui.label(
        egui::RichText::new("Pet commands are automatic (manual commands: follow-up)")
            .color(theme::TEXT_WEAK)
            .size(10.0),
    );
}
