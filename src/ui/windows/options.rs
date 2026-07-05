//! Options window — global UI settings plus a small info readout.
//! Interface: UI scale, window lock, window fading, reset-all. Info: fps,
//! zone, character, and where the per-character layout persists.

use crate::ui::{theme, UiCmd, UiCtx};

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(title)
            .color(theme::GOLD)
            .size(11.0)
            .strong(),
    );
    ui.separator();
}

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    section(ui, "Interface");

    let mut scale = cx.ui_scale;
    if ui
        .add(
            egui::Slider::new(&mut scale, 0.5..=2.0)
                .text("UI scale")
                .fixed_decimals(2),
        )
        .on_hover_text("Multiplier on the window-size-based UI zoom")
        .changed()
    {
        cx.cmds.push(UiCmd::SetUiScale(scale));
    }

    let mut locked = cx.locked;
    if ui
        .checkbox(&mut locked, egui::RichText::new("Lock windows").size(12.0))
        .on_hover_text("Prevent windows from being moved or resized (Ctrl+L)")
        .changed()
    {
        cx.cmds.push(UiCmd::SetLocked(locked));
    }

    let mut fades = cx.fades;
    if ui
        .checkbox(&mut fades, egui::RichText::new("Window fading").size(12.0))
        .on_hover_text("Fade windows the mouse isn't over")
        .changed()
    {
        cx.cmds.push(UiCmd::SetFades(fades));
    }

    ui.add_space(2.0);
    if ui
        .button(egui::RichText::new("Reset all windows").size(12.0))
        .on_hover_text("Restore every window's default position and size")
        .clicked()
    {
        cx.cmds.push(UiCmd::ResetAllWindows);
    }

    ui.add_space(4.0);
    section(ui, "Info");

    let s = cx.scene;
    egui::Grid::new("options_info")
        .num_columns(2)
        .spacing([10.0, 2.0])
        .show(ui, |ui| {
            let row = |ui: &mut egui::Ui, k: &str, v: &str| {
                ui.label(egui::RichText::new(k).color(theme::TEXT_WEAK).size(11.0));
                ui.label(egui::RichText::new(v).size(11.0));
                ui.end_row();
            };
            row(ui, "FPS", &format!("{:.0}", cx.fps));
            row(ui, "Zone", if s.zone.is_empty() { "(none)" } else { &s.zone });
            row(
                ui,
                "Character",
                if s.player_name.is_empty() { "(none)" } else { &s.player_name },
            );
        });

    ui.add_space(2.0);
    ui.label(
        egui::RichText::new("Window layout is saved per character in ~/.config/eqoxide/")
            .color(theme::TEXT_WEAK)
            .size(10.0),
    );
}
