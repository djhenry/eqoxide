//! NPC Dialogue window (transient) — quest-giver conversation panel.
//!
//! Shows recent NPC speech (message kind `"npc"`, younger than 45 s) with
//! `[bracketed]` keywords rendered as clickable answers, then the parsed
//! saylink choices as link-style buttons, and a Hail button aimed at the
//! nearest living NPC. Ported from the old HUD's `draw_quest_dialogue`.

use crate::scene::{Billboard, SceneState};
use crate::ui::{theme, UiCtx};

/// How long an NPC message stays on the panel (seconds).
const MSG_TTL_SECS: u64 = 45;
/// Max distance (2D, zone units) at which the Hail button will pick an NPC.
const HAIL_RANGE: f32 = 30.0;

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;
    ui.set_min_width(220.0);

    let visible: Vec<_> = s
        .messages
        .iter()
        .filter(|m| m.kind == "npc" && m.timestamp.elapsed().as_secs() < MSG_TTL_SECS)
        .collect();

    // ── NPC speech, keywords clickable ────────────────────────────────────
    if visible.is_empty() {
        ui.label(
            egui::RichText::new("No recent NPC dialogue. Hail someone!")
                .color(theme::TEXT_WEAK)
                .size(11.0),
        );
    } else {
        egui::ScrollArea::vertical()
            .max_height(180.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for entry in &visible {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        for (seg, is_kw) in split_keywords(&entry.text) {
                            if is_kw {
                                keyword_link(ui, cx, &seg);
                            } else {
                                ui.label(
                                    egui::RichText::new(seg).size(12.0).color(theme::CHAT_NPC),
                                );
                            }
                        }
                    });
                    ui.add_space(3.0);
                }
            });
    }

    // ── Parsed saylink choices ────────────────────────────────────────────
    if !s.dialogue_choices.is_empty() {
        ui.separator();
        ui.label(egui::RichText::new("Responses").color(theme::TEXT_WEAK).size(10.0));
        for choice in &s.dialogue_choices {
            let link = egui::Label::new(
                egui::RichText::new(format!("• {}", choice.text))
                    .size(12.0)
                    .color(theme::GOLD)
                    .underline(),
            )
            .sense(egui::Sense::click());
            let resp = ui.add(link).on_hover_text("Click to answer the NPC");
            if resp.clicked() {
                cx.acts.command.request_dialogue_click(choice.clone());
            }
        }
    }

    // ── Hail the nearest NPC ──────────────────────────────────────────────
    ui.separator();
    match nearest_hailable(s) {
        Some(b) => {
            let name = crate::http::clean_entity_name(&b.name);
            if ui
                .button(egui::RichText::new(format!("Hail, {name}")).size(12.0))
                .on_hover_text("Greet the nearest NPC")
                .clicked()
            {
                cx.acts.command.request_hail(name, Some(b.id));
            }
        }
        None => {
            ui.label(
                egui::RichText::new("(no NPC within hail range)")
                    .color(theme::TEXT_WEAK)
                    .size(10.0),
            );
        }
    }
}

/// Render one `[keyword]` run as a clickable answer. Prefers the proper
/// saylink click (works even for "silent" links whose sent phrase differs from
/// the label, #120); falls back to plain /say for bracketed text that isn't an
/// actual saylink.
fn keyword_link(ui: &mut egui::Ui, cx: &mut UiCtx, seg: &str) {
    let label = egui::Label::new(
        egui::RichText::new(seg).size(12.0).strong().color(theme::GOLD),
    )
    .sense(egui::Sense::click());
    if ui.add(label).on_hover_text("Click to answer the NPC").clicked() {
        let kw = seg.trim_start_matches('[').trim_end_matches(']').to_string();
        if let Some(choice) = cx
            .scene
            .dialogue_choices
            .iter()
            .find(|c| c.text.eq_ignore_ascii_case(&kw))
        {
            cx.acts.command.request_dialogue_click(choice.clone());
        } else {
            cx.acts.command.request_say(kw);
        }
    }
}

// `[keyword]` parsing lives in game_state::split_keywords (shared with the
// HTTP message feed).
use crate::game_state::split_keywords;

/// The nearest living NPC within [`HAIL_RANGE`] (2D distance). Skips corpses and
/// the off-map zone controller.
fn nearest_hailable(scene: &SceneState) -> Option<&Billboard> {
    let p = scene.player_pos; // [east, north, height]
    scene
        .billboards
        .iter()
        .filter(|b| !b.dead && !b.name.contains("zone_controller"))
        .map(|b| {
            let de = b.pos[0] - p[0];
            let dn = b.pos[1] - p[1];
            (b, de * de + dn * dn)
        })
        .filter(|(_, d2)| *d2 <= HAIL_RANGE * HAIL_RANGE)
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(b, _)| b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keywords_marks_bracket_runs() {
        let runs = split_keywords("I need [bone chips] and [rat ears].");
        assert_eq!(
            runs,
            vec![
                ("I need ".to_string(), false),
                ("[bone chips]".to_string(), true),
                (" and ".to_string(), false),
                ("[rat ears]".to_string(), true),
                (".".to_string(), false),
            ]
        );
    }

    #[test]
    fn split_keywords_unterminated_bracket_is_plain() {
        let runs = split_keywords("broken [link");
        assert_eq!(
            runs,
            vec![("broken ".to_string(), false), ("[link".to_string(), false)]
        );
    }

    #[test]
    fn split_keywords_plain_text_passthrough() {
        assert_eq!(split_keywords("hello"), vec![("hello".to_string(), false)]);
        assert!(split_keywords("").is_empty());
    }

    fn bb(id: u32, name: &str, pos: [f32; 3], level: u32, dead: bool) -> Billboard {
        Billboard {
            id,
            pos,
            level,
            hp_pct: 100.0,
            is_target: false,
            dead,
            name: name.to_string(),
            race: String::new(),
            action: String::new(),
            heading: 0.0,
            equipment: [0; 9],
            equipment_tint: [[0; 3]; 9],
            gender: 0,
            face: 0,
            hairstyle: 0,
            haircolor: 0,
            helm: 0,
            showhelm: 0,
            floating: false,
        }
    }

    #[test]
    fn nearest_hailable_picks_closest_living_in_range() {
        let mut scene = SceneState { player_pos: [0.0, 0.0, 0.0], ..Default::default() };
        scene.billboards = vec![
            bb(1, "Far_Guard000", [100.0, 0.0, 0.0], 5, false), // out of range
            bb(2, "Dead_Guy000", [2.0, 0.0, 0.0], 5, true),     // corpse
            bb(3, "zone_controller", [1.0, 0.0, 0.0], 5, false),
            bb(5, "Guard_Phaeton000", [10.0, 5.0, 0.0], 5, false),
            bb(6, "Guard_Hobble000", [3.0, 3.0, 0.0], 5, false),
        ];
        assert_eq!(nearest_hailable(&scene).map(|b| b.id), Some(6));
    }

    #[test]
    fn nearest_hailable_none_when_empty_or_far() {
        let mut scene = SceneState::default();
        assert!(nearest_hailable(&scene).is_none());
        scene.billboards = vec![bb(1, "Far_Guard000", [50.0, 50.0, 0.0], 5, false)];
        assert!(nearest_hailable(&scene).is_none());
    }
}
