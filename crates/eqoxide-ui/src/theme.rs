//! RoF2-derived egui theme — "EQ, but cleaner".
//!
//! Palette measured from the native client's shipped TGA atlases
//! (`uifiles/default/window_pieces*.tga`, `wnd_bg_*_rock.tga`) and the FillTint
//! values in the EQUI window XML. See docs/ui-overhaul-design.md §6.

use egui::Color32;

// ── Palette ───────────────────────────────────────────────────────────────────
/// Window body fill (native `wnd_bg_light_rock.tga` average #131621).
pub const BG_WINDOW: Color32 = Color32::from_rgb(0x13, 0x16, 0x21);
/// Darker panel fill (chat / recessed wells, `wnd_bg_dark_rock.tga` #0D0E14).
pub const BG_PANEL: Color32 = Color32::from_rgb(0x0D, 0x0E, 0x14);
/// Recessed item-slot / text-edit background.
pub const BG_SLOT: Color32 = Color32::from_rgb(0x14, 0x14, 0x16);
/// Title-bar strip top / bottom (subtle top-lit vertical gradient).
pub const TITLE_TOP: Color32 = Color32::from_rgb(0x38, 0x36, 0x31);
pub const TITLE_BOTTOM: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x28);
/// Bevel strokes (outer highlight / dark inner) from the border frame pieces.
pub const FRAME_HI: Color32 = Color32::from_rgb(0x89, 0x80, 0x77);
pub const FRAME_LO: Color32 = Color32::from_rgb(0x3F, 0x3C, 0x30);
/// Warm brass outline used on native buttons.
pub const BRASS: Color32 = Color32::from_rgb(0x9C, 0x9C, 0x8A);
/// Gold used by native close/minimize glyphs.
pub const GOLD: Color32 = Color32::from_rgb(0xC5, 0xB9, 0x76);
/// Button faces; hover is the signature RoF2 *blue* shift, not a lighten.
pub const BTN_FACE: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x2F);
pub const BTN_HOVER: Color32 = Color32::from_rgb(0x46, 0x48, 0x5E);
pub const BTN_ACTIVE: Color32 = Color32::from_rgb(0x2B, 0x2B, 0x31);
/// Default text (native gauge-overlay white).
pub const TEXT: Color32 = Color32::from_rgb(0xF0, 0xF0, 0xF0);
pub const TEXT_WEAK: Color32 = Color32::from_rgb(0xA8, 0xA6, 0x9C);
/// Gauge trough.
pub const GAUGE_BG: Color32 = Color32::from_rgb(0x1E, 0x1E, 0x1E);

// Native FillTint gauge colors (EQUI_PlayerWindow.xml / EQUI_Inventory.xml).
pub const HP: Color32 = Color32::from_rgb(240, 0, 0);
pub const MANA: Color32 = Color32::from_rgb(0, 128, 255);
pub const ENDURANCE: Color32 = Color32::from_rgb(240, 240, 0);
pub const PET_HP: Color32 = Color32::from_rgb(51, 192, 51);
pub const XP: Color32 = Color32::from_rgb(220, 150, 0);
pub const XP_AA: Color32 = Color32::from_rgb(0, 80, 220);
/// Casting-bar tint (native casting gauge is the player-window grey-blue).
pub const CAST: Color32 = Color32::from_rgb(120, 150, 240);

// Chat channel colors (dark-background-legible takes on the classic set).
pub const CHAT_SAY: Color32 = Color32::from_rgb(0xE8, 0xE8, 0xE8);
pub const CHAT_TELL: Color32 = Color32::from_rgb(0xC0, 0x60, 0xC0);
pub const CHAT_GROUP: Color32 = Color32::from_rgb(0x60, 0xB0, 0xE0);
pub const CHAT_OOC: Color32 = Color32::from_rgb(0x60, 0xC0, 0x60);
pub const CHAT_SHOUT: Color32 = Color32::from_rgb(0xE0, 0x60, 0x60);
pub const CHAT_COMBAT: Color32 = Color32::from_rgb(0xE0, 0x80, 0x60);
pub const CHAT_SYSTEM: Color32 = Color32::from_rgb(0xC8, 0xC8, 0x60);
pub const CHAT_NPC: Color32 = Color32::from_rgb(0x80, 0xC8, 0xE8);
pub const CHAT_EXP: Color32 = Color32::from_rgb(0xE8, 0xD0, 0x40);
pub const CHAT_LOOT: Color32 = Color32::from_rgb(0x9C, 0xE0, 0x9C);

/// Consider colors (server con RGB is authoritative when present; these are
/// fallbacks / accents).
pub const CON_GREY: Color32 = Color32::from_rgb(0x80, 0x80, 0x80);

/// Message-kind → display color (chat window, message log).
pub fn kind_color(kind: &str) -> Color32 {
    match kind {
        "combat" => CHAT_COMBAT,
        "zone" | "system" | "door" => CHAT_SYSTEM,
        "exp" => CHAT_EXP,
        "loot" | "trade" | "merchant" => CHAT_LOOT,
        "npc" => CHAT_NPC,
        "tell" => CHAT_TELL,
        "group" => CHAT_GROUP,
        "ooc" => CHAT_OOC,
        "shout" => CHAT_SHOUT,
        "chat" | "say" => CHAT_SAY,
        _ => TEXT,
    }
}

/// Title-bar height in points (native strip is 16 px; +2 for breathing room).
pub const TITLE_H: f32 = 18.0;

/// Install the theme on the egui context. Idempotent; call once at startup.
pub fn apply(ctx: &egui::Context) {
    use egui::style::{Selection, WidgetVisuals, Widgets};
    use egui::{FontFamily, FontId, Rounding, Stroke, TextStyle, Visuals};

    let rounding = Rounding::same(2.0);
    let widget = |bg: Color32, stroke_c: Color32, fg: Color32| WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: Stroke::new(1.0, stroke_c),
        fg_stroke: Stroke::new(1.0, fg),
        rounding,
        expansion: 0.0,
    };

    let mut visuals = Visuals::dark();
    visuals.override_text_color = Some(TEXT);
    visuals.window_fill = BG_WINDOW;
    visuals.panel_fill = BG_PANEL;
    visuals.window_stroke = Stroke::new(1.0, FRAME_LO);
    visuals.window_rounding = Rounding::same(3.0);
    visuals.window_shadow = egui::epaint::Shadow {
        offset: egui::vec2(0.0, 2.0),
        blur: 8.0,
        spread: 0.0,
        color: Color32::from_black_alpha(96),
    };
    visuals.popup_shadow = visuals.window_shadow;
    visuals.extreme_bg_color = BG_SLOT;
    visuals.faint_bg_color = Color32::from_rgb(0x1A, 0x1C, 0x26);
    visuals.selection = Selection {
        bg_fill: Color32::from_rgba_unmultiplied(0x3C, 0x3B, 0x35, 110),
        stroke: Stroke::new(1.0, GOLD),
    };
    visuals.hyperlink_color = CHAT_NPC;
    visuals.widgets = Widgets {
        noninteractive: widget(BG_WINDOW, FRAME_LO, TEXT),
        inactive: widget(BTN_FACE, BRASS.gamma_multiply(0.55), TEXT),
        hovered: widget(BTN_HOVER, BRASS, TEXT),
        active: widget(BTN_ACTIVE, GOLD, TEXT),
        open: widget(BTN_ACTIVE, BRASS, TEXT),
    };
    visuals.slider_trailing_fill = true;

    let mut style = (*ctx.style()).clone();
    style.visuals = visuals;
    // RoF2 font ladder (Font 1/2/3/5 ≈ 10/12/14/16 px).
    style.text_styles = [
        (TextStyle::Small, FontId::new(10.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(16.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.0, FontFamily::Monospace)),
    ]
    .into();
    // EQ packs controls tight.
    style.spacing.item_spacing = egui::vec2(4.0, 3.0);
    style.spacing.button_padding = egui::vec2(8.0, 3.0);
    style.spacing.window_margin = egui::Margin::same(6.0);
    style.spacing.menu_margin = egui::Margin::same(6.0);
    ctx.set_style(style);
}
