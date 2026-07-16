//! Pet window — minimal take on the native `PetInfoWindow`.
//!
//! Shows the current pet's name, level and HP (native pet-green FillTint), plus
//! the manual command buttons (Attack / Back Off / Follow / Guard / Sit). The
//! buttons write the shared `pet_cmd` slot (same as POST /v1/pet/command); the
//! nav thread drains it and sends OP_PetCommands.

use crate::eq_net::protocol::{PET_ATTACK, PET_BACKOFF, PET_FOLLOWME, PET_GUARDHERE, PET_SIT};
use crate::ui::{theme, widgets, UiCtx};

/// Compact button with an 11 pt label (matches the Actions window's bar style).
fn btn(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).size(11.0))
}

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

    ui.add_space(4.0);

    // Manual pet commands. Each button queues one OP_PetCommands command byte;
    // Attack needs a current target (the nav thread aims it at scene.target_id).
    let send = |cmd: u32| cx.acts.command.request_pet_command(cmd as u8);
    ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
    ui.horizontal_wrapped(|ui| {
        let atk_hover = if s.target_id.is_some() {
            "Send the pet at the current target"
        } else {
            "Target something first"
        };
        if ui
            .add_enabled(s.target_id.is_some(), btn("\u{2694} Attack"))
            .on_hover_text(atk_hover)
            .clicked()
        {
            send(PET_ATTACK);
        }
        if ui.add(btn("Back Off")).on_hover_text("Stop attacking and return").clicked() {
            send(PET_BACKOFF);
        }
        if ui.add(btn("Follow")).on_hover_text("Follow me").clicked() {
            send(PET_FOLLOWME);
        }
        if ui.add(btn("Guard")).on_hover_text("Guard this spot").clicked() {
            send(PET_GUARDHERE);
        }
        if ui.add(btn("Sit")).on_hover_text("Toggle sit").clicked() {
            send(PET_SIT);
        }
    });
}
