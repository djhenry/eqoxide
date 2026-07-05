//! Help window — hotkey reference, chat commands, and window-management tips.
//! The hotkey table is built live from the registry (via `cx.window_list`) so
//! it never drifts from the actual bindings.

use crate::ui::{theme, UiCtx};

/// Fixed (non-window) bindings that don't live in the registry.
const FIXED_KEYS: [(&str, &str); 8] = [
    ("WASD / QE", "Move / turn"),
    ("Space", "Jump"),
    ("R / F9", "Reset camera behind player"),
    ("F10", "Debug overlay"),
    ("Ctrl+L", "Lock / unlock windows"),
    ("Mouse drag", "Orbit camera"),
    ("Scroll", "Zoom camera"),
    ("Left click", "Target NPC / open door"),
];

const CHAT_CMDS: [(&str, &str); 7] = [
    ("/say <msg>", "Say to those nearby"),
    ("/tell <name> <msg>", "Private message a player"),
    ("/ooc <msg>", "Out-of-character (zone-wide)"),
    ("/shout <msg>", "Shout (zone-wide)"),
    ("/g <msg>", "Group chat"),
    ("/r <msg>", "Reply to the last tell"),
    ("/camp", "Sit and log out safely"),
];

fn key_row(ui: &mut egui::Ui, key: &str, what: &str) {
    ui.label(egui::RichText::new(key).color(theme::GOLD).size(11.0).monospace());
    ui.label(egui::RichText::new(what).size(11.0));
    ui.end_row();
}

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
        egui::CollapsingHeader::new(egui::RichText::new("Hotkeys").size(12.0))
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("help_keys")
                    .num_columns(2)
                    .spacing([12.0, 2.0])
                    .show(ui, |ui| {
                        for &(id, title, _open, hotkey) in cx.window_list {
                            let Some(k) = hotkey else { continue };
                            // Grid ids come from row content; keep rows unique by window id.
                            ui.push_id(id, |ui| {
                                ui.label(
                                    egui::RichText::new(format!("{k:?}"))
                                        .color(theme::GOLD)
                                        .size(11.0)
                                        .monospace(),
                                );
                            });
                            ui.label(egui::RichText::new(format!("Toggle {title}")).size(11.0));
                            ui.end_row();
                        }
                        for &(key, what) in &FIXED_KEYS {
                            key_row(ui, key, what);
                        }
                    });
            });

        egui::CollapsingHeader::new(egui::RichText::new("Chat commands").size(12.0))
            .default_open(false)
            .show(ui, |ui| {
                egui::Grid::new("help_chat")
                    .num_columns(2)
                    .spacing([12.0, 2.0])
                    .show(ui, |ui| {
                        for &(cmd, what) in &CHAT_CMDS {
                            key_row(ui, cmd, what);
                        }
                    });
            });

        egui::CollapsingHeader::new(egui::RichText::new("Windows").size(12.0))
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Drag the title bar to move, drag edges to resize, right-click the \
                         title for opacity/reset, ✕ closes. Reopen anything from the Window \
                         Selector.",
                    )
                    .color(theme::TEXT_WEAK)
                    .size(11.0),
                );
            });
    });
}
