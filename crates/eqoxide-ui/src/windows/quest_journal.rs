//! Quest Journal — the native Task-system quest log (`TaskWnd`).
//!
//! Active tab: one collapsing entry per task (title, reward, description,
//! objectives with progress gauges, Abandon). Completed tab: quest history
//! with completion dates. Pending selector offers (`OP_TaskSelectWindow`)
//! surface in a highlighted "Offered" strip with Accept/Decline — decline is
//! the accept slot with task_id 0, mirroring POST /v1/quests/decline.

use eqoxide_core::game_state::{ActivityProgress, TaskActivity};
use crate::{theme, widgets, UiCtx};

/// Objective-complete green (matches the old HUD task window).
const DONE_GREEN: egui::Color32 = egui::Color32::from_rgb(120, 220, 120);

/// Format one objective as "target  done/goal" (e.g. "Kill a rat  3/10").
/// Single-step objectives (goal ≤ 1, e.g. "Speak to X") show just the target;
/// completion is conveyed by color. A *locked* objective renders as `???` —
/// the same thing the real RoF2 client shows, and the truthful rendering of a
/// short-form OP_TaskActivity, which carries no target and no counts (#889).
/// Pure/unit-testable.
fn objective_label(a: &TaskActivity) -> String {
    match &a.progress {
        ActivityProgress::Known { target, description, done_count, goal_count, .. } => {
            let text = if description.is_empty() { target } else { description };
            if *goal_count > 1 {
                format!("{}  {}/{}", text, done_count.min(goal_count), goal_count)
            } else {
                text.clone()
            }
        }
        ActivityProgress::Locked { .. } => "???".to_string(),
        ActivityProgress::Undecodable { .. } => "(objective could not be read)".to_string(),
    }
}

/// An objective is complete once its done-count reaches its goal (a
/// single-step objective has goal 1). Pure/unit-testable. An objective whose
/// progress the server has not disclosed is never reported as done.
fn objective_done(a: &TaskActivity) -> bool {
    match &a.progress {
        ActivityProgress::Known { done_count, goal_count, .. } => *done_count >= (*goal_count).max(1),
        ActivityProgress::Locked { .. } | ActivityProgress::Undecodable { .. } => false,
    }
}

/// The progress fraction to draw a gauge for, or `None` when there is no
/// honest one to draw (single-step, locked, or undecodable).
fn objective_fraction(a: &TaskActivity) -> Option<f32> {
    match &a.progress {
        ActivityProgress::Known { done_count, goal_count, .. } if *goal_count > 1 =>
            Some(*done_count.min(goal_count) as f32 / *goal_count as f32),
        _ => None,
    }
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
            .stroke(egui::Stroke::new(1.0_f32, theme::GOLD))
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
                            cx.acts.command.request_accept_task(o.task_id);
                        }
                        if ui
                            .small_button("Decline")
                            .on_hover_text("Declines all pending offers")
                            .clicked()
                        {
                            // task_id 0 = decline-all, same as POST /v1/quests/decline.
                            cx.acts.command.request_accept_task(0);
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
                    if let Some(frac) = objective_fraction(a) {
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
                    cx.acts.command.request_cancel_task(t.task_id);
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
            activity_id: 0,
            progress: ActivityProgress::Known {
                activity_type: 2,
                target: target.into(),
                description: String::new(),
                done_count: done,
                goal_count: goal,
                optional: false,
            },
        }
    }

    fn locked() -> TaskActivity {
        TaskActivity { activity_id: 1, progress: ActivityProgress::Locked { optional: false } }
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

    /// #889: a short-form (locked) activity carries NO target and NO counts. It must render as
    /// the client's own `???`, never as a completed or zero-progress real objective, and it must
    /// not draw a progress gauge it has no numbers for.
    #[test]
    fn locked_objective_renders_as_unknown_and_is_never_done() {
        assert_eq!(objective_label(&locked()), "???");
        assert!(!objective_done(&locked()));
        assert_eq!(objective_fraction(&locked()), None);
    }

    #[test]
    fn undecodable_objective_says_so_and_is_never_done() {
        let a = TaskActivity {
            activity_id: 2,
            progress: ActivityProgress::Undecodable { reason: "test".into() },
        };
        assert_eq!(objective_label(&a), "(objective could not be read)");
        assert!(!objective_done(&a));
        assert_eq!(objective_fraction(&a), None);
    }

    #[test]
    fn objective_fraction_only_for_multi_step_known() {
        assert_eq!(objective_fraction(&act("x", 3, 10)), Some(0.3));
        assert_eq!(objective_fraction(&act("x", 30, 10)), Some(1.0), "done clamps at goal");
        assert_eq!(objective_fraction(&act("x", 0, 1)), None, "single-step has no gauge");
    }

    /// `description_override` is the server's own wording for the objective; when it is set the
    /// client's auto-generated target text is not what the player sees.
    #[test]
    fn description_override_wins_over_target() {
        let a = TaskActivity {
            activity_id: 0,
            progress: ActivityProgress::Known {
                activity_type: 4, target: "Guard Hollings".into(),
                description: "Report to the guard at the gate".into(),
                done_count: 0, goal_count: 1, optional: false,
            },
        };
        assert_eq!(objective_label(&a), "Report to the guard at the gate");
    }

    #[test]
    fn fmt_epoch_day_formats_utc_dates() {
        assert_eq!(fmt_epoch_day(0), "");
        assert_eq!(fmt_epoch_day(86_400), "1970-01-02");
        assert_eq!(fmt_epoch_day(1_700_000_000), "2023-11-14");
    }
}
