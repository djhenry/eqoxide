//! Spell Gems window — native `CastSpellWnd`: a vertical bar of the 9
//! memorized spell gems. Click a gem to cast it (writes the same request slot
//! the `/cast` HTTP API uses). Gems grey out while a cast is in flight.

use crate::ui::{theme, UiCtx};

/// Native gem size (the RoF2 gem art is ~34 px; we match the old HUD's 36).
const GEM: f32 = 36.0;

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let enabled = cx.scene.casting.is_none();
    ui.spacing_mut().item_spacing.y = 3.0;

    for (gem, &spell_id) in cx.scene.mem_spells.iter().enumerate() {
        let empty = spell_id == 0 || spell_id == u32::MAX;
        if empty {
            empty_gem(ui, gem);
            continue;
        }

        let info = cx.spells.get(spell_id);
        let name = info
            .map(|i| i.name.clone())
            .unwrap_or_else(|| format!("Spell {spell_id}"));
        let icon = info.and_then(|i| {
            let (sheet0, col, row) = crate::spells::icon_cell(i.icon_id);
            cx.icons.spell(
                ui.ctx(),
                sheet0 as u32 + 1,
                (row * crate::spells::ICON_COLS + col) as u32,
            )
        });

        let resp = match icon {
            Some(ic) => ui
                .add_enabled(
                    enabled,
                    egui::ImageButton::new(ic.image(GEM)).rounding(egui::Rounding::same(2.0)),
                )
                .on_hover_text(&name),
            None => {
                // No atlas: small text button with the first 8 chars of the name.
                let short: String = name.chars().take(8).collect();
                ui.add_enabled(
                    enabled,
                    egui::Button::new(egui::RichText::new(short).size(10.0))
                        .min_size(egui::vec2(GEM + 20.0, 24.0)),
                )
                .on_hover_text(&name)
            }
        };
        if resp.clicked() {
            cx.acts.command.request_cast(crate::http::CastRequest {
                gem: gem as u8,
                target_id: None,
                item_slot: None,
            });
        }
    }
}

/// A recessed empty gem socket (native empty `SpellGem` look).
fn empty_gem(ui: &mut egui::Ui, gem: usize) {
    let (rect, resp) =
        ui.allocate_exact_size(egui::Vec2::splat(GEM), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, theme::BG_SLOT);
    painter.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0, egui::Color32::from_black_alpha(200)),
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{}", gem + 1),
        egui::FontId::proportional(9.0),
        theme::TEXT_WEAK.gamma_multiply(0.5),
    );
    resp.on_hover_text("Empty gem — memorize a spell from the Spellbook");
}
