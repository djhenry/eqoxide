//! TODO(#162): window body — being implemented by the ui-dev window fan-out.

use crate::ui::UiCtx;

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let _ = &cx.scene;
    ui.label("(under construction)");
}
