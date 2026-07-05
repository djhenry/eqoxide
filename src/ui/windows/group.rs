//! Group window — native `GroupWindow`.
//!
//! Member rows (name, level, role badges, per-member HP gauge), a pending
//! invite banner with Accept/Decline, and Leave/Kick/Make-Leader controls.
//! Member HP comes from the same OP_MobHealth pathway as any other entity
//! (matched into `scene.billboards` by name), so it lags real HP by up to one
//! server health tick; the player's own row uses `player_hp_pct` directly.

use crate::ui::{theme, widgets, UiCtx};

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;

    // ── Pending invite banner ────────────────────────────────────────────────
    if let Some(inviter) = &s.pending_invite {
        egui::Frame::none()
            .fill(theme::BG_PANEL)
            .stroke(egui::Stroke::new(1.0, theme::GOLD))
            .rounding(egui::Rounding::same(2.0))
            .inner_margin(egui::Margin::same(6.0))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!("{inviter} invites you to join"))
                        .color(theme::GOLD)
                        .size(12.0),
                );
                ui.horizontal(|ui| {
                    if ui.button(egui::RichText::new("Accept").size(11.0)).clicked() {
                        *cx.acts.group_accept.lock().unwrap() = Some(());
                    }
                    if ui.button(egui::RichText::new("Decline").size(11.0)).clicked() {
                        *cx.acts.group_decline.lock().unwrap() = Some(());
                    }
                });
            });
        ui.add_space(4.0);
    }

    if s.group_members.is_empty() {
        if s.pending_invite.is_none() {
            ui.label(egui::RichText::new("Not in a group").color(theme::TEXT_WEAK).size(11.0));
        }
        return;
    }

    // ── Member rows ──────────────────────────────────────────────────────────
    // Kick / Make Leader are leader-only server-side; hide them from non-leaders
    // so the UI can't fire requests the nav layer's validation would reject
    // (and can't disband the clicker's own membership by accident).
    let self_is_leader = s.group_leader.eq_ignore_ascii_case(&s.player_name)
        || s.group_members.iter().any(|m| m.is_leader && m.name.eq_ignore_ascii_case(&s.player_name));
    for m in &s.group_members {
        let is_self = m.name.eq_ignore_ascii_case(&s.player_name);
        let is_leader = m.is_leader || (!s.group_leader.is_empty() && m.name.eq_ignore_ascii_case(&s.group_leader));

        let row = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let name_color = if m.offline { theme::TEXT_WEAK } else { theme::TEXT };
            ui.label(egui::RichText::new(&m.name).color(name_color).strong().size(12.0));
            ui.label(
                egui::RichText::new(format!("{}", m.level))
                    .color(theme::TEXT_WEAK)
                    .size(10.0),
            );
            if is_leader {
                badge(ui, "LDR", theme::GOLD);
            }
            if m.tank {
                badge(ui, "TNK", theme::CHAT_COMBAT);
            }
            if m.assist {
                badge(ui, "AST", theme::CHAT_GROUP);
            }
            if m.puller {
                badge(ui, "PUL", theme::CHAT_SYSTEM);
            }
            if m.offline {
                badge(ui, "offline", theme::TEXT_WEAK);
            }

            // Small ✕ on the right of the row kicks a non-self member (leader only).
            if !is_self && self_is_leader {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let x = ui
                        .small_button(egui::RichText::new("✕").size(9.0))
                        .on_hover_text(format!("Kick {} from the group", m.name));
                    if x.clicked() {
                        *cx.acts.group_kick.lock().unwrap() = Some(m.name.clone());
                    }
                });
            }
        });

        // Right-click a non-self member row: Kick / Make Leader (leader only).
        if !is_self && self_is_leader {
            row.response.interact(egui::Sense::click()).context_menu(|ui| {
                if ui.button(egui::RichText::new("Kick").size(11.0)).clicked() {
                    *cx.acts.group_kick.lock().unwrap() = Some(m.name.clone());
                    ui.close_menu();
                }
                if ui.button(egui::RichText::new("Make Leader").size(11.0)).clicked() {
                    *cx.acts.group_make_leader.lock().unwrap() = Some(m.name.clone());
                    ui.close_menu();
                }
            });
        }

        // HP gauge: self from the player snapshot, others matched into the
        // billboard list by name (case-insensitive).
        let hp_pct = if is_self {
            s.player_hp_pct
        } else {
            s.billboards
                .iter()
                .find(|b| b.name.eq_ignore_ascii_case(&m.name))
                .map(|b| b.hp_pct)
                .unwrap_or(0.0)
        };
        let tint = if m.offline { theme::CON_GREY } else { theme::HP };
        widgets::gauge(ui, ("grp_hp", m.name.as_str()), "HP", hp_pct / 100.0, tint, true);
        ui.add_space(2.0);
    }

    // ── Footer ───────────────────────────────────────────────────────────────
    ui.separator();
    ui.horizontal(|ui| {
        if ui
            .button(egui::RichText::new("Leave").size(11.0))
            .on_hover_text("Leave the group")
            .clicked()
        {
            *cx.acts.group_leave.lock().unwrap() = Some(());
        }
        ui.label(
            egui::RichText::new("right-click a member for options")
                .color(theme::TEXT_WEAK)
                .size(9.0),
        );
    });
}

/// Tiny role badge: bracketed colored tag text.
fn badge(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    ui.label(egui::RichText::new(format!("[{text}]")).color(color).size(9.0));
}
