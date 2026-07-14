# Item icon (`dragitem*.dds`) atlas mapping — RoF2

## Finding: item icon cells are COLUMN-MAJOR, not row-major

The RoF2 client's item-icon grid animation (`A_DragItem`) is defined with
`<Vertical>true</Vertical>`, which means the linear cell index advances **down
a column first, then to the next column** — the opposite of the row-major
order (`col = cell % cols; row = cell / cols`) eqoxide currently used.

### Evidence

- `everquest_rof2/uifiles/default/EQUI_Animations.xml:12722-12743` (and the
  full block through ~13400, one `<Frames>` per `dragitemN.dds`, 178 total,
  confirmed by `grep -c "<Texture>dragitem"` = 178): the `A_DragItem`
  `Ui2DAnimation` has
  ```
  <Cycle>false</Cycle>
  <Grid>true</Grid>
  <Vertical>true</Vertical>
  <CellHeight>40</CellHeight>
  <CellWidth>40</CellWidth>
  ```
  Each `<Frames>` entry is one whole `dragitemN.dds`, `Location=(0,0)`,
  `Size=(256,256)` — i.e. the Frame *is* the 256×256 grid sheet, subdivided
  into `256/40 = 6` (floor) columns × 6 rows = 36 cells/sheet.
- Contrast with `A_SpellIcons` at `EQUI_Animations.xml:11489-11495`:
  `<Vertical>false</Vertical>`, same 40×40 cell size — i.e. **spell icon
  sheets (`spellsNN.tga`) are row-major**, item icon sheets (`dragitemN.dds`)
  are column-major. These are two different conventions in the same client;
  do not assume they match.
- The XML property names (`Grid`, `Cycle`, `Vertical`, `CellWidth`,
  `CellHeight`) are real, live fields read by the animation loader, not
  dead/unused XML attributes. The actual per-frame cell-index → (row,col)
  arithmetic could not be isolated directly from client behavior alone; the
  ordering below was therefore confirmed empirically (see next section).
- **Empirical confirmation** (decisive): extracted `dragitem3.dds`,
  `dragitem7.dds`, `dragitem15.dds` from
  `everquest_rof2/uifiles/default/` with `magick` (DXT5-compressed 256×256
  DDS) and cropped candidate cells for several known `EQEmu` items DB icon
  values:
  - icon **580** ("Short Sword") → `idx=80` → `sheet=80/36+1=3`,
    `cell=80-72=8`. Row-major cell 8 (`row=1,col=2`) crops to a **light-blue
    bottle/flask** (wrong). Column-major cell 8 (`col=8/6=1,row=8%6=2`) crops
    to a plain **long straight sword with a cross-guard** — matches "Short
    Sword".
  - icon **728** ("Gloomingdeep Lantern") → `idx=228`, `sheet=7`, `cell=12`,
    column-major `col=2,row=0` → crops to a **lantern**. Correct.
  - icon **717** ("Skin of Milk") → `idx=217`, `sheet=7`, `cell=1`,
    column-major `col=0,row=1` → crops to a **waterskin/bag** shape. Correct.
  - icon **1021** ("Bread Cakes") → `idx=521`, `sheet=15`, `cell=17`,
    column-major `col=2,row=5` → crops to **pretzel/bread**-shaped icon.
    Correct.
  - Row-major placement was checked against the same four icons and only
    matches by coincidence when `cell % 6 == cell / 6` (the diagonal);
    everywhere else it lands on the wrong picture (e.g. icon 580 → bottle).
  - Scratch crops/scripts used for this: see session scratchpad
    `dragitem3.png`, `dragitem7.png`, `dragitem15.png`,
    `cand_rowmajor_r1c2.png` (wrong), `cand_colmajor_c1r2.png` (right),
    `check_lantern_728.png`, `check_milk_717.png`, `check_bread_1021.png`.
    (Scratch dir is session-local; regenerate with `magick <file>.dds
    <file>.png` + a PIL crop if needed again.)

### Formula (confirmed)

```
idx    = icon - 500                  // base offset; icon < 500 => no drag-item icon
sheet  = idx / 36 + 1                // 1-based dragitem<sheet>.dds, floor division
cell   = idx % 36                    // 0..35 within the sheet
cols   = 6, rows = 6                 // floor(256/40); 16px of the 256px sheet unused
col    = cell / 6                    // COLUMN-MAJOR: row varies fastest
row    = cell % 6
px_x   = col * 40
px_y   = row * 40
```

178 `dragitem*.dds` files ship in `everquest_rof2/uifiles/default/`
(178 × 36 = 6408 icon slots, icon 500..6907). Icons above the last populated
sheet fail to load (`dragitem179.dds` doesn't exist) — should degrade to
"no icon" the same way `icon < 500` does.

### Contrast: spell icons stay row-major

`A_SpellIcons` (`EQUI_Animations.xml:11489`) has `Vertical=false`, so
`spellsNN.tga` sheets are `col = cell % cols; row = cell / cols` — this is
what eqoxide's `src/spells.rs:95-100` (`icon_cell`) already implements
correctly. **Do not reuse one cell-math function for both** — they have
different `Vertical` flags in the source XML and therefore different
orderings.

## eqoxide bug (found while investigating)

`src/ui/icons.rs:88-97` (`Icons::cell_uv`) computes `col = cell % SHEET_CELLS;
row = cell / SHEET_CELLS` (row-major) and is shared by both `Icons::item()`
(icons.rs:113) and `Icons::spell()` (icons.rs:124). This is correct for
spells but **wrong for items** — it needs to be column-major for the
`dragitem*.dds` path. Recommended fix: split into two cell-UV helpers (e.g.
`item_cell_uv` doing `col = cell/6, row = cell%6`, keep `cell_uv`/rename for
spells doing `col = cell%6, row = cell/6`), or add a `vertical: bool`
parameter. The sheet-selection math already in `icons.rs:104-106`
(`idx/36+1`, `idx%36`, base 500) is correct and needs no change — only the
cell-to-(col,row) step is wrong for items.
