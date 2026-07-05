//! Quest Journal — the native Task-system quest log (`TaskWnd`).
//!
//! Active tab: one collapsing entry per task (title, reward, description,
//! objectives with progress gauges, Abandon). Completed tab: quest history
//! with completion dates. Pending selector offers (`OP_TaskSelectWindow`)
//! surface in a highlighted "Offered" strip with Accept/Decline — decline is
//! the accept slot with task_id 0, mirroring POST /v1/quests/decline.

use crate::game_state::TaskActivity;
use crate::ui::{theme, widgets, UiCtx};

/// Objective-complete green (matches the old HUD task window).
const DONE_GREEN: egui::Color32 = egui::Color32::from_rgb(120, 220, 120);

/// Format one objective as "target  done/goal" (e.g. "Kill a rat  3/10").
/// Single-step objectives (goal ≤ 1, e.g. "Speak to X") show just the target;
/// completion is conveyed by color. Pure/unit-testable.
fn objective_label(a: &TaskActivity) -> String {
    if a.goal_count > 1 {
        format!("{}  {}/{}", a.target, a.done_count.min(a.goal_count), a.goal_count)
    } else {
        a.target.clone()
    }
}

/// An objective is complete once its done-count reaches its goal (a
/// single-step objective has goal 1). Pure/unit-testable.
fn objective_done(a: &TaskActivity) -> bool {
    a.done_count >= a.goal_count.max(1)
}

/// Format a unix-epoch second as a `YYYY-MM-DD` UTC date (no date-lib
/// dependency; Howard Hinnant's civil-from-days). `0` renders empty.
fn fmt_epoch_day(secs: u32) -> String {
    if secs == 0 {
        return String::new();
    }
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}")
}

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;

    // ── Offered tasks (selector window open) — highlighted, above the tabs ──
    if !s.task_offers.is_empty() {
        egui::Frame::none()
            .fill(theme::BG_PANEL)
            .stroke(egui::Stroke::new(1.0, theme::GOLD))
            .rounding(egui::Rounding::same(2.0))
            .inner_margin(egui::Margin::same(5.0))
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Offered").strong().size(12.0).color(theme::GOLD));
                for o in &s.task_offers {
                    ui.label(egui::RichText::new(&o.title).strong().size(12.0));
                    if !o.description.trim().is_empty() {
                        ui.label(
                            egui::RichText::new(o.description.trim())
                                .size(10.0)
                                .color(theme::TEXT_WEAK),
                        );
                    }
                    if o.has_rewards {
                        ui.label(
                            egui::RichText::new("This task has rewards.")
                                .size(10.0)
                                .color(theme::CHAT_LOOT),
                        );
                    }
                    ui.horizontal(|ui| {
                        if ui.small_button("Accept").clicked() {
                            *cx.acts.accept_task.lock().unwrap() = Some(o.task_id);
                        }
                        if ui
                            .small_button("Decline")
                            .on_hover_text("Declines all pending offers")
                            .clicked()
                        {
                            // task_id 0 = decline-all, same as POST /v1/quests/decline.
                            *cx.acts.accept_task.lock().unwrap() = Some(0);
                        }
                    });
                }
            });
        ui.add_space(3.0);
    }

    // ── Tabs: Active | Completed ─────────────────────────────────────────
    let tab_id = ui.id().with("qj_tab");
    let mut tab: u8 = ui.ctx().data_mut(|d| *d.get_temp_mut_or(tab_id, 0u8));
    ui.horizontal(|ui| {
        if ui
            .selectable_label(tab == 0, format!("Active ({})", s.tasks.len()))
            .clicked()
        {
            tab = 0;
        }
        if ui
            .selectable_label(tab == 1, format!("Completed ({})", s.completed_tasks.len()))
            .clicked()
        {
            tab = 1;
        }
    });
    ui.ctx().data_mut(|d| d.insert_temp(tab_id, tab));
    ui.separator();

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if tab == 0 {
            draw_active(ui, cx);
        } else {
            draw_completed(ui, cx);
        }
    });
}

fn draw_active(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;
    if s.tasks.is_empty() {
        ui.label(
            egui::RichText::new("(no active tasks)")
                .size(11.0)
                .color(theme::TEXT_WEAK),
        );
        return;
    }
    for t in &s.tasks {
        let header = egui::RichText::new(&t.title).strong().size(12.0).color(theme::GOLD);
        egui::CollapsingHeader::new(header)
            .id_salt(("task", t.task_id))
            .default_open(true)
            .show(ui, |ui| {
                // Reward line (xp / coin / item, whichever are present).
                let mut reward = Vec::new();
                if t.xp_reward > 0 {
                    reward.push("experience".to_string());
                }
                if t.coin_reward > 0 {
                    reward.push(widgets::fmt_copper(t.coin_reward));
                }
                if !t.reward_item_text.is_empty() {
                    reward.push(t.reward_item_text.clone());
                }
                if !reward.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("Reward: {}", reward.join(", ")))
                            .size(10.0)
                            .color(theme::CHAT_LOOT),
                    );
                }
                if !t.description.trim().is_empty() {
                    ui.label(
                        egui::RichText::new(t.description.trim())
                            .size(10.0)
                            .color(theme::TEXT_WEAK),
                    );
                }
                ui.add_space(2.0);

                // Objectives: checkbox-style line + thin progress gauge for
                // multi-count steps.
                for a in &t.activities {
                    let done = objective_done(a);
                    ui.label(
                        egui::RichText::new(format!(
                            "{} {}",
                            if done { "\u{2714}" } else { "\u{2022}" },
                            objective_label(a)
                        ))
                        .size(11.0)
                        .color(if done { DONE_GREEN } else { theme::TEXT }),
                    );
                    if a.goal_count > 1 {
                        let frac = a.done_count.min(a.goal_count) as f32 / a.goal_count as f32;
                        ui.indent(("task_act", t.task_id, a.activity_id), |ui| {
                            widgets::gauge(
                                ui,
                                ("task_gauge", t.task_id, a.activity_id),
                                "",
                                frac,
                                if done { DONE_GREEN } else { theme::XP },
                                false,
                            );
                        });
                    }
                }

                ui.add_space(2.0);
                if ui
                    .small_button(egui::RichText::new("Abandon").size(10.0))
                    .on_hover_text("Abandon this task (cannot be undone)")
                    .clicked()
                {
                    *cx.acts.cancel_task.lock().unwrap() = Some(t.task_id);
                }
            });
    }
}

fn draw_completed(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;
    if s.completed_tasks.is_empty() {
        ui.label(
            egui::RichText::new("(no completed tasks)")
                .size(11.0)
                .color(theme::TEXT_WEAK),
        );
        return;
    }
    // Newest first.
    let mut done: Vec<_> = s.completed_tasks.iter().collect();
    done.sort_by_key(|c| std::cmp::Reverse(c.completed_time));
    for c in done {
        ui.horizontal(|ui| {
            let when = fmt_epoch_day(c.completed_time);
            if !when.is_empty() {
                ui.label(egui::RichText::new(when).size(10.0).color(theme::TEXT_WEAK));
            }
            ui.label(egui::RichText::new(&c.title).size(11.0).color(DONE_GREEN));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(target: &str, done: u32, goal: u32) -> TaskActivity {
        TaskActivity {
            target: target.into(),
            done_count: done,
            goal_count: goal,
            ..Default::default()
        }
    }

    #[test]
    fn objective_label_shows_counts_only_for_multi_step() {
        assert_eq!(objective_label(&act("Kill a rat", 3, 10)), "Kill a rat  3/10");
        assert_eq!(objective_label(&act("Speak to Guard", 0, 1)), "Speak to Guard");
        // done clamps at goal.
        assert_eq!(objective_label(&act("Kill a rat", 12, 10)), "Kill a rat  10/10");
    }

    #[test]
    fn objective_done_handles_zero_goal() {
        assert!(!objective_done(&act("x", 0, 1)));
        assert!(objective_done(&act("x", 1, 1)));
        assert!(objective_done(&act("x", 1, 0))); // goal 0 treated as 1
        assert!(!objective_done(&act("x", 3, 10)));
    }

    #[test]
    fn fmt_epoch_day_formats_utc_dates() {
        assert_eq!(fmt_epoch_day(0), "");
        assert_eq!(fmt_epoch_day(86_400), "1970-01-02");
        assert_eq!(fmt_epoch_day(1_700_000_000), "2023-11-14");
    }
}
