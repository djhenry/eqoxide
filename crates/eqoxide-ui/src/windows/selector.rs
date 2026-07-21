//! Window Selector — the non-closeable control panel (native `SelectorWindow`).
//! One toggle per window plus the global UI controls. Requirement: this window
//! is always available; the registry marks it `closeable: false`.

use crate::{UiCmd, UiCtx};

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    ui.horizontal_wrapped(|ui| {
        for &(id, title, open, hotkey) in cx.window_list {
            let label = match hotkey {
                Some(k) => format!("{title} ({k:?})"),
                None => title.to_string(),
            };
            let mut is_open = open;
            if ui.toggle_value(&mut is_open, title).on_hover_text(label).changed() {
                cx.cmds.push(UiCmd::Toggle(id));
            }
        }
        // Transient windows open from game state, not from here — listed
        // greyed so the catalog is complete (native SelectorWindow parity).
        for def in crate::registry::REGISTRY.iter().filter(|d| d.transient) {
            ui.add_enabled(false, egui::SelectableLabel::new(false, def.title))
                .on_disabled_hover_text("Opens automatically in game (merchant, trainer, loot…)");
        }
    });
    ui.separator();
    ui.horizontal(|ui| {
        let mut locked = cx.locked;
        if ui.checkbox(&mut locked, "Lock").on_hover_text("Lock all windows (Ctrl+L)").changed() {
            cx.cmds.push(UiCmd::SetLocked(locked));
        }
        let mut fades = cx.fades;
        if ui.checkbox(&mut fades, "Fade").on_hover_text("Fade windows the mouse isn't over").changed() {
            cx.cmds.push(UiCmd::SetFades(fades));
        }
        let mut scale = cx.ui_scale;
        if ui
            .add(egui::Slider::new(&mut scale, 0.5..=2.0).text("UI scale").fixed_decimals(2))
            .on_hover_text("Multiplier on the window-size-based UI zoom")
            .changed()
        {
            cx.cmds.push(UiCmd::SetUiScale(scale));
        }
        if ui.button("Reset all").on_hover_text("Restore every window's default position/size").clicked() {
            cx.cmds.push(UiCmd::ResetAllWindows);
        }
    });
}
