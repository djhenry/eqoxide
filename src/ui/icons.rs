//! Lazy loaders for the native RoF2 UI texture atlases.
//!
//! Sources sit in the original client's `uifiles/default/` directory, located
//! via (in order) the `EQ_UI_DIR` env var, `renderer.eq_ui_dir` in config.yaml,
//! or `~/eq_assets/everquest_rof2/uifiles/default`. Everything degrades
//! gracefully to text-only UI when the files are absent.
//!
//! - **Item icons**: `dragitem1..178.dds`, 256×256 sheets of 6×6 40 px cells.
//!   Wire icon ids are offset by the classic base 500:
//!   `idx = icon - 500; sheet = idx/36 + 1; cell = idx%36`. Cells are **column-major**
//!   (RoF2 `A_DragItem` `<Vertical>true</Vertical>`) — see `cell_uv`.
//! - **Spell icons**: `spells01..07.tga`, sheets of 6×6 40 px cells, **row-major**
//!   (`A_SpellIcons` `<Vertical>false</Vertical>`; `crate::spells::icon_cell` maps a
//!   spell's icon id to sheet/cell).

use std::collections::HashMap;
use std::path::PathBuf;

pub const CELL: u32 = 40;
const SHEET_CELLS: u32 = 6; // 6×6 grid per 256×256 sheet
const ITEM_ICON_BASE: u32 = 500;

pub struct Icons {
    dir: Option<PathBuf>,
    /// dragitem sheets, keyed by 1-based sheet number; None = load failed.
    item_sheets: HashMap<u32, Option<egui::TextureHandle>>,
    /// spells0N.tga sheets, keyed by 1-based sheet number; None = load failed.
    spell_sheets: HashMap<u32, Option<egui::TextureHandle>>,
}

/// A drawable icon: a texture plus the UV sub-rect of its cell.
#[derive(Clone)]
pub struct IconRef {
    pub tex: egui::TextureId,
    pub uv: egui::Rect,
}

impl IconRef {
    pub fn image(&self, size: f32) -> egui::Image<'static> {
        egui::Image::new((self.tex, egui::vec2(size, size))).uv(self.uv)
    }
}

impl Icons {
    pub fn new(config_dir: Option<String>) -> Self {
        let dir = std::env::var("EQ_UI_DIR")
            .ok()
            .or(config_dir)
            .or_else(|| {
                // Back-compat with the old spell-icon env var.
                std::env::var("EQ_SPELL_ICONS_DIR").ok()
            })
            .map(|d| PathBuf::from(shellexpand::tilde(&d).to_string()))
            .or_else(|| {
                let default =
                    PathBuf::from(shellexpand::tilde("~/eq_assets/everquest_rof2/uifiles/default").to_string());
                default.is_dir().then_some(default)
            });
        if let Some(d) = &dir {
            tracing::info!("ui icons: using atlas dir {}", d.display());
        } else {
            tracing::info!("ui icons: no atlas dir found; icons fall back to text");
        }
        Icons { dir, item_sheets: HashMap::new(), spell_sheets: HashMap::new() }
    }

    fn load_sheet(
        ctx: &egui::Context,
        dir: &std::path::Path,
        name: &str,
    ) -> Option<egui::TextureHandle> {
        let path = dir.join(name);
        match image::open(&path) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                Some(ctx.load_texture(
                    format!("ui_atlas_{name}"),
                    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba),
                    egui::TextureOptions::NEAREST,
                ))
            }
            Err(e) => {
                tracing::debug!("ui icons: {} not loaded: {e}", path.display());
                None
            }
        }
    }

    /// UV rect of linear `cell` within a 6×6 sheet. RoF2 lays these two ways (per each
    /// `Ui2DAnimation`'s `<Vertical>` flag in EQUI_Animations.xml): item icons (`A_DragItem`,
    /// `Vertical=true`) advance **down a column first** (column-major); spell icons
    /// (`A_SpellIcons`, `Vertical=false`) advance **across a row first** (row-major). Applying the
    /// row-major layout to items transposes every icon (#184 — a short sword drew as a bottle).
    fn cell_uv(cell: u32, vertical: bool) -> egui::Rect {
        let (col, row) = if vertical {
            (cell / SHEET_CELLS, cell % SHEET_CELLS) // column-major (items)
        } else {
            (cell % SHEET_CELLS, cell / SHEET_CELLS) // row-major (spells)
        };
        let (col, row) = (col as f32, row as f32);
        // Cells are 40 px in a 256 px sheet (the last 16 px are padding).
        let unit = CELL as f32 / 256.0;
        egui::Rect::from_min_max(
            egui::pos2(col * unit, row * unit),
            egui::pos2((col + 1.0) * unit, (row + 1.0) * unit),
        )
    }

    /// Item icon for a wire `icon` id (as carried on `InvItem`/`MerchantItem`).
    pub fn item(&mut self, ctx: &egui::Context, icon_id: u32) -> Option<IconRef> {
        if icon_id < ITEM_ICON_BASE {
            return None;
        }
        let idx = icon_id - ITEM_ICON_BASE;
        let sheet = idx / (SHEET_CELLS * SHEET_CELLS) + 1;
        let cell = idx % (SHEET_CELLS * SHEET_CELLS);
        let dir = self.dir.clone()?;
        let tex = self
            .item_sheets
            .entry(sheet)
            .or_insert_with(|| Self::load_sheet(ctx, &dir, &format!("dragitem{sheet}.dds")))
            .as_ref()?;
        Some(IconRef { tex: tex.id(), uv: Self::cell_uv(cell, true) })
    }

    /// Spell icon by (1-based sheet, cell) — pair produced by `spells::icon_cell`.
    pub fn spell(&mut self, ctx: &egui::Context, sheet: u32, cell: u32) -> Option<IconRef> {
        let dir = self.dir.clone()?;
        let tex = self
            .spell_sheets
            .entry(sheet)
            .or_insert_with(|| Self::load_sheet(ctx, &dir, &format!("spells{sheet:02}.tga")))
            .as_ref()?;
        Some(IconRef { tex: tex.id(), uv: Self::cell_uv(cell, false) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_icon_sheet_math() {
        // icon 500 = first cell of sheet 1; icon 536 = first cell of sheet 2.
        assert_eq!((500u32 - ITEM_ICON_BASE) / 36 + 1, 1);
        assert_eq!((536u32 - ITEM_ICON_BASE) / 36 + 1, 2);
        assert_eq!((535u32 - ITEM_ICON_BASE) % 36, 35);
    }

    #[test]
    fn cell_uv_grid() {
        let uv = Icons::cell_uv(0, false);
        assert_eq!(uv.min, egui::pos2(0.0, 0.0));
        let uv7 = Icons::cell_uv(7, false); // row 1, col 1
        assert!((uv7.min.x - 40.0 / 256.0).abs() < 1e-6);
        assert!((uv7.min.y - 40.0 / 256.0).abs() < 1e-6);
    }

    #[test]
    fn item_cells_are_column_major_spells_row_major() {
        let unit = 40.0 / 256.0;
        // cell 1: spells (row-major) → col 1, row 0; items (column-major) → col 0, row 1 (the transpose).
        let spell1 = Icons::cell_uv(1, false);
        assert!((spell1.min.x - unit).abs() < 1e-6 && spell1.min.y.abs() < 1e-6);
        let item1 = Icons::cell_uv(1, true);
        assert!(item1.min.x.abs() < 1e-6 && (item1.min.y - unit).abs() < 1e-6);
        // Short Sword: icon 580 → sheet 3, cell 8. Column-major cell 8 = col 1, row 2 (the sword);
        // row-major would be col 2, row 1 (the bottle) — the #184 bug.
        let sword = Icons::cell_uv(8, true);
        assert!((sword.min.x - unit).abs() < 1e-6, "col 1");
        assert!((sword.min.y - 2.0 * unit).abs() < 1e-6, "row 2");
    }

    #[test]
    fn below_base_yields_none_math() {
        let below = 499u32;
        assert!(below < ITEM_ICON_BASE);
    }
}
