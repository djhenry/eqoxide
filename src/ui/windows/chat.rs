//! Chat window — tabbed scrollback over `scene.messages` plus the say/command
//! input line (native `ChatWindow`). Slash commands route to the EQ chat
//! channels via the shared `chat_send` queue; bare text goes out on Say.

use crate::http::ChatSend;
use crate::ui::{theme, UiCtx};

/// Tab order matches the native default chat filters.
const TABS: [&str; 5] = ["All", "Chat", "Combat", "System", "Loot"];

/// Does a message kind belong on the given tab?
fn tab_matches(tab: usize, kind: &str) -> bool {
    match tab {
        1 => matches!(kind, "chat" | "say" | "tell" | "ooc" | "shout" | "group" | "npc"),
        2 => kind == "combat",
        3 => matches!(kind, "system" | "zone" | "door" | "exp"),
        4 => matches!(kind, "loot" | "trade" | "merchant"),
        // 0 (All) and anything out of range show everything.
        _ => true,
    }
}

/// Parse one submitted input line and queue the matching outbound action.
/// EQ ChatChannel numbers: 2=group, 3=shout, 5=ooc, 7=tell.
fn submit(line: &str, cx: &mut UiCtx) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let Some(rest) = line.strip_prefix('/') else {
        // No slash → plain Say (also triggers quest keywords server-side).
        *cx.acts.say.lock().unwrap() = Some(line.to_string());
        return;
    };
    let (cmd, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let arg = arg.trim();
    let send = |chan: u32, to: &str, text: &str| {
        if !text.is_empty() {
            cx.acts.chat_send.lock().unwrap().push(ChatSend {
                chan,
                to: to.to_string(),
                text: text.to_string(),
            });
        }
    };
    match cmd.to_ascii_lowercase().as_str() {
        "tell" | "t" => {
            if let Some((name, msg)) = arg.split_once(char::is_whitespace) {
                send(7, name, msg.trim());
            }
        }
        "r" | "reply" => {
            let to = cx.chat.reply_to.clone();
            if !to.is_empty() {
                send(7, &to, arg);
            }
        }
        "ooc" => send(5, "", arg),
        "shout" => send(3, "", arg),
        "g" | "gsay" | "group" => send(2, "", arg),
        // /camp — same toggle the Actions window's Camp button uses.
        "camp" => {
            *cx.acts.camp.lock().unwrap() = Some(crate::http::CampCmd::Toggle);
        }
        "say" if !arg.is_empty() => {
            *cx.acts.say.lock().unwrap() = Some(arg.to_string());
        }
        // Unknown slash command: swallow it rather than shouting gibberish.
        _ => {}
    }
}

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    // ── Tab row ──────────────────────────────────────────────────────────────
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for (i, name) in TABS.iter().enumerate() {
            if ui
                .selectable_label(cx.chat.tab == i, egui::RichText::new(*name).size(11.0))
                .clicked()
            {
                cx.chat.tab = i;
            }
        }
    });

    // ── Scrollback (fills everything above the input row) ───────────────────
    let input_h = 24.0;
    let scroll_h = (ui.available_height() - input_h).max(30.0);
    let tab = cx.chat.tab;
    egui::Frame::none()
        .fill(theme::BG_PANEL)
        .rounding(egui::Rounding::same(2.0))
        .inner_margin(egui::Margin::same(3.0))
        .show(ui, |ui| {
            ui.set_min_height(scroll_h - 6.0);
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .max_height(scroll_h - 6.0)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    for m in cx.scene.messages.iter().filter(|m| tab_matches(tab, &m.kind)) {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&m.text)
                                    .size(12.0)
                                    .color(theme::kind_color(&m.kind)),
                            )
                            .wrap(),
                        );
                    }
                });
        });

    // ── Input row ────────────────────────────────────────────────────────────
    ui.horizontal(|ui| {
        let send_w = 40.0;
        let resp = ui.add(
            egui::TextEdit::singleline(&mut cx.chat.input)
                .id(egui::Id::new("eq_chat_input")) // stable ID so focus persists
                .desired_width((ui.available_width() - send_w).max(60.0))
                .font(egui::FontId::proportional(12.0))
                .hint_text(
                    egui::RichText::new("say... (/tell name msg, /ooc, /shout, /g)").size(11.0),
                ),
        );
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let clicked = ui.button(egui::RichText::new("Send").size(11.0)).clicked();
        if (enter || clicked) && !cx.chat.input.trim().is_empty() {
            let line = std::mem::take(&mut cx.chat.input);
            submit(&line, cx);
            // Keep typing without re-clicking the box.
            resp.request_focus();
        }
    });
}
