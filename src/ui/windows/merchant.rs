//! Merchant window — native `MerchantWnd`. Transient: opened/closed by the
//! game session (`scene.merchant_open`). Left panel: the merchant's wares with
//! icon + price + Buy; right panel: the player's general-inventory slots with a
//! quantity picker + Sell. Footer: coin row + Done. All buttons write the same
//! request slots the `/v1/merchant/*` HTTP API uses, so HUD and API stay in sync.

use crate::ui::{theme, widgets, UiCtx};

/// RoF2 wire slots for the general inventory (rof2_limits.h): 23..=32.
const GENERAL_SLOTS: std::ops::RangeInclusive<i32> = 23..=32;

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let s = cx.scene;
    let Some(merchant_id) = s.merchant_open else {
        // Transient gating should prevent this, but never panic on a race.
        ui.label(egui::RichText::new("(no merchant session)").weak().size(11.0));
        return;
    };

    let footer_h = 26.0;
    let body_h = (ui.available_height() - footer_h).max(80.0);

    ui.columns(2, |cols| {
        draw_buy_panel(&mut cols[0], cx, merchant_id, body_h);
        draw_sell_panel(&mut cols[1], cx, merchant_id, body_h);
    });

    ui.separator();
    ui.horizontal(|ui| {
        widgets::coin_row(ui, cx.scene.coin);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(egui::RichText::new("Done").size(11.0)).clicked() {
                *cx.acts.trade.lock().unwrap() = Some(crate::http::TradeCmd::Close);
            }
        });
    });
}

/// Merchant wares: icon, name, price (copper → p/g/s/c), stock, Buy.
fn draw_buy_panel(ui: &mut egui::Ui, cx: &mut UiCtx, merchant_id: u32, height: f32) {
    ui.label(egui::RichText::new("For Sale").strong().size(12.0).color(theme::GOLD));
    egui::ScrollArea::vertical()
        .id_salt("merchant_buy")
        .max_height(height)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if cx.scene.merchant_items.is_empty() {
                ui.label(
                    egui::RichText::new("(no items / loading…)")
                        .color(theme::TEXT_WEAK)
                        .size(11.0),
                );
                return;
            }
            for it in &cx.scene.merchant_items {
                ui.horizontal(|ui| {
                    let icon = cx.icons.item(ui.ctx(), it.icon);
                    widgets::item_slot(ui, icon, &it.name, &it.name, false);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(egui::RichText::new(&it.name).size(11.0));
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(widgets::fmt_copper(it.price))
                                    .color(theme::GOLD)
                                    .size(10.0),
                            );
                            // Small positive stock counts are limited supply;
                            // -1 / huge values mean effectively unlimited.
                            if it.quantity > 0 && it.quantity < 1000 {
                                ui.label(
                                    egui::RichText::new(format!("({} left)", it.quantity))
                                        .color(theme::TEXT_WEAK)
                                        .size(10.0),
                                );
                            }
                        });
                        if ui.button(egui::RichText::new("Buy").size(10.0)).clicked() {
                            *cx.acts.buy.lock().unwrap() = Some((merchant_id, it.merchant_slot));
                        }
                    });
                });
            }
        });
}

/// Player general-slot items (wire slots 23..=32): name + qty picker + Sell.
fn draw_sell_panel(ui: &mut egui::Ui, cx: &mut UiCtx, merchant_id: u32, height: f32) {
    ui.label(egui::RichText::new("Your Items").strong().size(12.0).color(theme::GOLD));
    egui::ScrollArea::vertical()
        .id_salt("merchant_sell")
        .max_height(height)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let sellable: Vec<_> = cx
                .scene
                .inventory
                .iter()
                .filter(|i| GENERAL_SLOTS.contains(&i.slot))
                .collect();
            if sellable.is_empty() {
                ui.label(
                    egui::RichText::new("(nothing to sell)")
                        .color(theme::TEXT_WEAK)
                        .size(11.0),
                );
                return;
            }
            for it in sellable {
                let stack = it.charges.max(1) as u32;
                ui.horizontal(|ui| {
                    let icon = cx.icons.item(ui.ctx(), it.icon);
                    widgets::item_slot(ui, icon, &it.name, &it.name, false);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        let label = if stack > 1 {
                            format!("{} x{}", it.name, stack)
                        } else {
                            it.name.clone()
                        };
                        ui.label(egui::RichText::new(label).size(11.0));
                        ui.horizontal(|ui| {
                            // Per-slot quantity, kept in egui temp memory so
                            // the dumb-view redraw doesn't reset it each frame.
                            let qty_id = ui.id().with(("sell_qty", it.slot));
                            let mut qty: u32 = ui
                                .ctx()
                                .data_mut(|d| *d.get_temp_mut_or(qty_id, 1u32))
                                .clamp(1, stack);
                            if stack > 1 {
                                ui.add(
                                    egui::DragValue::new(&mut qty)
                                        .range(1..=stack)
                                        .speed(0.1),
                                );
                            }
                            ui.ctx().data_mut(|d| d.insert_temp(qty_id, qty));
                            if ui.button(egui::RichText::new("Sell").size(10.0)).clicked() {
                                *cx.acts.sell.lock().unwrap() =
                                    Some((merchant_id, it.slot as u32, qty));
                            }
                        });
                    });
                });
            }
        });
}
