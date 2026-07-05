//! Inventory window — native `Inventory`. Worn-equipment grid (RoF2 wire
//! slots 0-22), general inventory (23-32), cursor (33), coin footer.
//! Click-to-move: first click selects a slot (gold ring), second click sends
//! the pair into the shared `move_item` request slot (same path as the HTTP
//! `/inventory/move` API). Bag *contents* (sub-slots ≥ 251) aren't modeled by
//! the client yet, so bags can't be opened from here.

use crate::game_state::InvItem;
use crate::ui::{theme, widgets, UiCtx};

/// RoF2 worn-equipment wire slots → display labels (rof2_limits.h 0-22).
const WORN_SLOTS: [(i32, &str); 23] = [
    (0, "Charm"),
    (1, "Ear"),
    (2, "Head"),
    (3, "Face"),
    (4, "Ear"),
    (5, "Neck"),
    (6, "Shoulders"),
    (7, "Arms"),
    (8, "Back"),
    (9, "Wrist"),
    (10, "Wrist"),
    (11, "Range"),
    (12, "Hands"),
    (13, "Primary"),
    (14, "Secondary"),
    (15, "Finger"),
    (16, "Finger"),
    (17, "Chest"),
    (18, "Legs"),
    (19, "Feet"),
    (20, "Waist"),
    (21, "Power Source"),
    (22, "Ammo"),
];

/// General-inventory wire slots (the 10 main bag/item slots).
const GENERAL_FIRST: i32 = 23;
const GENERAL_LAST: i32 = 32;
/// Cursor wire slot.
const CURSOR_SLOT: i32 = 33;
/// First bag-content sub-slot (not modeled client-side yet).
const BAG_SLOTS_BEGIN: i32 = 251;

fn sel_id() -> egui::Id {
    egui::Id::new("inv_sel")
}

fn selected_slot(ui: &egui::Ui) -> Option<i32> {
    ui.ctx().data_mut(|d| d.get_temp(sel_id()))
}

/// One inventory slot: icon/label, stack-count overlay, tooltip, and the
/// click-to-move selection protocol.
fn inv_slot(ui: &mut egui::Ui, cx: &mut UiCtx, slot: i32, label: &str, item: Option<&InvItem>) {
    let selected = selected_slot(ui) == Some(slot);
    let (icon, fallback, tooltip) = match item {
        Some(it) => {
            let mut tip = it.name.clone();
            if it.charges > 1 {
                tip.push_str(&format!("\nQty: {}", it.charges));
            }
            if slot >= BAG_SLOTS_BEGIN {
                tip.push_str("\nbag contents not yet supported");
            }
            (cx.icons.item(ui.ctx(), it.icon), it.name.clone(), tip)
        }
        None => (None, label.to_string(), label.to_string()),
    };
    let resp = widgets::item_slot(ui, icon, &fallback, &tooltip, selected);

    // Stack/charge count overlaid bottom-right when > 1.
    if let Some(it) = item {
        if it.charges > 1 {
            let pos = resp.rect.right_bottom() - egui::vec2(3.0, 2.0);
            let font = egui::FontId::proportional(9.0);
            let p = ui.painter();
            // 1 px shadow so the count reads over bright icons.
            p.text(
                pos + egui::vec2(1.0, 1.0),
                egui::Align2::RIGHT_BOTTOM,
                format!("{}", it.charges),
                font.clone(),
                egui::Color32::from_black_alpha(220),
            );
            p.text(pos, egui::Align2::RIGHT_BOTTOM, format!("{}", it.charges), font, theme::TEXT);
        }
    }

    if resp.clicked() {
        match selected_slot(ui) {
            // Click the selected slot again → deselect.
            Some(s) if s == slot => ui.ctx().data_mut(|d| d.remove::<i32>(sel_id())),
            // Second click elsewhere → request the move, clear selection.
            Some(s) => {
                *cx.acts.move_item.lock().unwrap() = Some((s as u32, slot as u32));
                ui.ctx().data_mut(|d| d.remove::<i32>(sel_id()));
            }
            // First click → select.
            None => ui.ctx().data_mut(|d| d.insert_temp(sel_id(), slot)),
        }
    }
}

pub fn draw(ui: &mut egui::Ui, cx: &mut UiCtx) {
    let inv = cx.scene.inventory.clone();
    let find = |slot: i32| inv.iter().find(|i| i.slot == slot);

    // ── Equipment ────────────────────────────────────────────────────────
    ui.label(egui::RichText::new("Equipment").strong().size(12.0).color(theme::TEXT_WEAK));
    egui::Grid::new("inv_equip_grid").spacing([3.0, 3.0]).show(ui, |ui| {
        for (i, (slot, label)) in WORN_SLOTS.iter().enumerate() {
            inv_slot(ui, cx, *slot, label, find(*slot));
            if i % 6 == 5 {
                ui.end_row();
            }
        }
    });

    ui.add_space(4.0);
    ui.separator();

    // ── General inventory (2 × 5) + cursor ───────────────────────────────
    ui.label(egui::RichText::new("Inventory").strong().size(12.0).color(theme::TEXT_WEAK));
    egui::Grid::new("inv_general_grid").spacing([3.0, 3.0]).show(ui, |ui| {
        for slot in GENERAL_FIRST..=GENERAL_LAST {
            let n = slot - GENERAL_FIRST;
            inv_slot(ui, cx, slot, &format!("Slot {}", n + 1), find(slot));
            if n % 5 == 4 {
                ui.end_row();
            }
        }
    });
    ui.label(
        egui::RichText::new("Bags can't be opened yet (contents not modeled).")
            .weak()
            .size(10.0),
    );

    if let Some(cur) = find(CURSOR_SLOT) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Cursor:").size(11.0).color(theme::TEXT_WEAK));
            inv_slot(ui, cx, CURSOR_SLOT, "Cursor", Some(cur));
        });
    }

    // Anything outside the modeled 0-33 range (e.g. bag sub-slots the server
    // streamed anyway) — show it so items never silently vanish.
    let mut other: Vec<&InvItem> = inv
        .iter()
        .filter(|i| !(0..=CURSOR_SLOT).contains(&i.slot))
        .collect();
    if !other.is_empty() {
        other.sort_by_key(|i| i.slot);
        ui.add_space(2.0);
        ui.label(egui::RichText::new("Bag contents").strong().size(12.0).color(theme::TEXT_WEAK));
        ui.horizontal_wrapped(|ui| {
            for it in other {
                inv_slot(ui, cx, it.slot, &it.name, Some(it));
            }
        });
    }

    if inv.is_empty() {
        ui.label(egui::RichText::new("(waiting for inventory from server…)").weak().size(11.0));
    }

    // Selection hint — makes the two-click move protocol discoverable.
    if let Some(s) = selected_slot(ui) {
        let label = WORN_SLOTS
            .iter()
            .find(|(w, _)| *w == s)
            .map(|(_, l)| l.to_string())
            .unwrap_or_else(|| format!("slot {s}"));
        ui.label(
            egui::RichText::new(format!("Moving from {label} — click a destination slot"))
                .color(theme::GOLD)
                .size(10.0),
        );
    }

    ui.add_space(2.0);
    ui.separator();
    widgets::coin_row(ui, cx.scene.coin);
}
