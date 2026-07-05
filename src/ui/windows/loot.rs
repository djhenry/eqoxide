//! Loot window — native `LootWnd`, adapted to eqoxide's auto-loot flow.
//! The gameplay loop drives the corpse session (OP_LootRequest → take-all →
//! OP_EndLootRequest); this body shows the in-progress session, recent loot
//! messages, and a "loot nearest corpse" trigger.

use crate::ui::{theme, UiCtx};

/// Max distance (world units) at which "Loot nearest corpse" engages.
const LOOT_RANGE: f32 = 50.0;
/// How long a loot message stays in the recent-drops list.
const RECENT_SECS: u64 = 20;

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;

    // Active-session banner.
    if s.loot_active {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0).color(theme::CHAT_LOOT));
            ui.label(
                egui::RichText::new("Looting corpse…")
                    .color(theme::CHAT_LOOT)
                    .size(12.0),
            );
        });
        ui.add_space(2.0);
    }

    // Recent loot/coin messages (kind "loot", last 20 s).
    let recent: Vec<_> = s
        .messages
        .iter()
        .filter(|m| m.kind == "loot" && m.timestamp.elapsed().as_secs() < RECENT_SECS)
        .collect();
    if recent.is_empty() {
        if !s.loot_active {
            ui.label(
                egui::RichText::new("No recent loot.")
                    .color(theme::TEXT_WEAK)
                    .size(11.0),
            );
        }
    } else {
        egui::ScrollArea::vertical()
            .max_height(96.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for m in recent {
                    ui.label(
                        egui::RichText::new(&m.text)
                            .color(theme::CHAT_LOOT)
                            .size(11.0),
                    );
                }
            });
    }

    ui.add_space(4.0);

    // Nearest dead billboard within range → the loot request slot.
    let nearest = s
        .billboards
        .iter()
        .filter(|b| b.dead)
        .map(|b| {
            let dx = b.pos[0] - s.player_pos[0];
            let dy = b.pos[1] - s.player_pos[1];
            let dz = b.pos[2] - s.player_pos[2];
            (b, (dx * dx + dy * dy + dz * dz).sqrt())
        })
        .filter(|(_, d)| *d <= LOOT_RANGE)
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    ui.horizontal(|ui| {
        let label = egui::RichText::new("Loot nearest corpse").size(11.0);
        match nearest {
            Some((b, dist)) => {
                if ui
                    .button(label)
                    .on_hover_text(format!("{} ({dist:.0} units away)", b.name))
                    .clicked()
                {
                    *cx.acts.loot.lock().unwrap() = Some(b.id);
                }
            }
            None => {
                ui.add_enabled(false, egui::Button::new(label));
                ui.label(
                    egui::RichText::new("No corpse nearby")
                        .color(theme::TEXT_WEAK)
                        .size(10.0),
                );
            }
        }
    });

    ui.add_space(2.0);
    ui.separator();
    ui.label(
        egui::RichText::new("Items are auto-taken (interactive pick coming later)")
            .color(theme::TEXT_WEAK)
            .size(10.0)
            .italics(),
    );
}
