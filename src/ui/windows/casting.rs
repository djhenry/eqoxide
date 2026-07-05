//! Casting bar — native `CastingWindow`. Transient: only shown while
//! `scene.casting` is `Some` (see `UiState::transient_open`).
//!
//! Spell icon + name, time remaining, and a CAST-tinted progress gauge that
//! fills as `started.elapsed()` approaches `cast_ms`.

use crate::ui::{theme, widgets, UiCtx};

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let Some(c) = &cx.scene.casting else {
        // Shouldn't happen (transient gate), but never panic in a draw fn.
        ui.label(egui::RichText::new("Not casting").color(theme::TEXT_WEAK).size(10.0));
        return;
    };

    let info = cx.spells.get(c.spell_id);
    let name = info
        .map(|i| i.name.clone())
        .unwrap_or_else(|| "Casting…".to_string());

    let cast_ms = c.cast_ms.max(1) as f32;
    let elapsed_ms = c.started.elapsed().as_secs_f32() * 1000.0;
    let frac = (elapsed_ms / cast_ms).clamp(0.0, 1.0);
    let remain_s = ((cast_ms - elapsed_ms).max(0.0)) / 1000.0;

    ui.horizontal(|ui| {
        // Spell icon when the atlas is available; the name is the fallback.
        if let Some(ic) = info.and_then(|i| {
            let (sheet0, col, row) = crate::spells::icon_cell(i.icon_id);
            cx.icons.spell(
                ui.ctx(),
                sheet0 as u32 + 1,
                (row * crate::spells::ICON_COLS + col) as u32,
            )
        }) {
            ui.add(egui::Image::new((ic.tex, egui::Vec2::splat(16.0))).uv(ic.uv));
        }
        ui.label(egui::RichText::new(&name).strong().size(12.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(format!("{remain_s:.1}s"))
                    .color(theme::TEXT_WEAK)
                    .size(10.0),
            );
        });
    });
    widgets::gauge(ui, "cast", "", frac, theme::CAST, true);

    // Keep the bar filling smoothly while the cast is in flight.
    ui.ctx().request_repaint();
}
