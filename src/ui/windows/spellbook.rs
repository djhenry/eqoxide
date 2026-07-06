//! Spellbook window — native `SpellBookWnd`, limited edition: the client does
//! not yet parse the profile book region (eqoxide#162 follow-up), so this shows
//! the memorized gems (click to cast) and any spell scrolls sitting in the
//! inventory, rather than the full 400-slot book.

use crate::ui::{theme, UiCtx};

/// Row icon size (small list rows, not the 36 px gem bar).
const ICON: f32 = 22.0;

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    section(ui, "Memorized");
    let cast_ok = cx.scene.casting.is_none();
    for (gem, &spell_id) in cx.scene.mem_spells.iter().enumerate() {
        let empty = spell_id == 0 || spell_id == u32::MAX;
        let info = if empty { None } else { cx.spells.get(spell_id) };
        let name = if empty {
            "\u{2014} empty \u{2014}".to_string()
        } else {
            info.map(|i| i.name.clone())
                .unwrap_or_else(|| format!("Spell {spell_id}"))
        };
        let icon = info.and_then(|i| {
            let (sheet0, col, row) = crate::spells::icon_cell(i.icon_id);
            cx.icons.spell(
                ui.ctx(),
                sheet0 as u32 + 1,
                (row * crate::spells::ICON_COLS + col) as u32,
            )
        });

        let resp = ui
            .add_enabled_ui(!empty && cast_ok, |ui| {
                gem_row(ui, gem, icon, &name, empty)
            })
            .inner;
        if !empty {
            let resp = resp.on_hover_text(format!("Cast {name} (gem {})", gem + 1));
            if resp.clicked() {
                *cx.acts.cast.lock().unwrap() = Some(crate::http::CastRequest {
                    gem: gem as u8,
                    target_id: None,
                    item_slot: None,
                });
            }
        }
    }

    ui.add_space(4.0);
    section(ui, "Spell scrolls in inventory");
    let scrolls: Vec<&crate::game_state::InvItem> = cx
        .scene
        .inventory
        .iter()
        .filter(|it| it.name.starts_with("Spell: "))
        .collect();
    if scrolls.is_empty() {
        ui.label(
            egui::RichText::new("(none)")
                .color(theme::TEXT_WEAK)
                .size(10.0),
        );
    } else {
        egui::ScrollArea::vertical()
            .id_salt("spellbook_scrolls")
            .max_height(140.0)
            .show(ui, |ui| {
                for it in scrolls {
                    ui.horizontal(|ui| {
                        let icon = cx.icons.item(ui.ctx(), it.icon);
                        match icon {
                            Some(ic) => {
                                ui.add(ic.image(ICON));
                            }
                            None => {
                                ui.label(
                                    egui::RichText::new("\u{1F4DC}")
                                        .size(11.0)
                                        .color(theme::TEXT_WEAK),
                                );
                            }
                        }
                        ui.label(egui::RichText::new(&it.name).size(11.0));
                        ui.label(
                            egui::RichText::new("(scribe via cursor \u{2014} see #11)")
                                .color(theme::TEXT_WEAK)
                                .size(10.0),
                        );
                    });
                }
            });
    }

    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(
            "Full spellbook requires profile book parsing \u{2014} tracked as a follow-up",
        )
        .color(theme::TEXT_WEAK)
        .size(10.0)
        .italics(),
    );
}

/// Section header: small gold caption + separator, EQ style.
fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .color(theme::GOLD)
            .size(11.0)
            .strong(),
    );
    ui.separator();
}

/// One memorized-gem row: `N  [icon] Spell Name`, clickable across its width.
fn gem_row(
    ui: &mut egui::Ui,
    gem: usize,
    icon: Option<crate::ui::icons::IconRef>,
    name: &str,
    empty: bool,
) -> egui::Response {
    let width = ui.available_width();
    let resp = ui
        .allocate_response(egui::vec2(width, ICON + 2.0), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let rect = resp.rect;
    if resp.hovered() && !empty {
        ui.painter()
            .rect_filled(rect, 2.0, theme::BTN_HOVER.gamma_multiply(0.5));
    }
    let painter = ui.painter();
    // Gem number.
    painter.text(
        rect.left_center() + egui::vec2(4.0, 0.0),
        egui::Align2::LEFT_CENTER,
        format!("{}", gem + 1),
        egui::FontId::proportional(10.0),
        theme::TEXT_WEAK,
    );
    // Icon (or a recessed placeholder square).
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 16.0 + ICON / 2.0, rect.center().y),
        egui::Vec2::splat(ICON),
    );
    match icon {
        Some(ic) => {
            egui::Image::new((ic.tex, egui::Vec2::splat(ICON)))
                .uv(ic.uv)
                .paint_at(ui, icon_rect);
        }
        None => {
            ui.painter().rect_filled(icon_rect, 2.0, theme::BG_SLOT);
            ui.painter().rect_stroke(
                icon_rect,
                2.0,
                egui::Stroke::new(1.0, egui::Color32::from_black_alpha(180)),
            );
        }
    }
    // Spell name.
    let color = if empty {
        theme::TEXT_WEAK.gamma_multiply(0.6)
    } else {
        theme::TEXT
    };
    ui.painter().text(
        egui::pos2(icon_rect.right() + 6.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        name,
        egui::FontId::proportional(11.0),
        color,
    );
    resp
}
