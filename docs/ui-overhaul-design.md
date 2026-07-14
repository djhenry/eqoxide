# UI Overhaul — Architecture Design

Issue: [#162](https://github.com/djhenry/eqoxide/issues/162) · Branch: `worktree-ui-overhaul` · Agent: ui-dev

Requirements (from #162): UI scales with window size; windows moveable + resizeable;
per-character persistence of window open/closed/pos/size **and** OS-window geometry;
a non-closeable window-control panel listing all windows; RoF2-like look & feel, but
better; native-client feature parity minus Sony/external services.

This design is grounded in research passes over (a) the current egui HUD
(`src/hud.rs`, `src/ui_layout.rs`, `src/app.rs`), (b) the native RoF2 UI definitions
(the client's own `uifiles/default/`, 164 `EQUI_*.xml`), and (c) observed
native-client window behavior (including the `UI_<char>_<server>.ini` schema).

---

## 1. What we keep, what we replace

**Keep** (proven in the current code):
- egui 0.29 composited over the wgpu frame in `App::egui_pass`.
- The zoom-factor scaling idea (`set_zoom_factor` derived from window size) — this is
  already "UI scales with the window"; we make it principled and user-adjustable.
- The debounced per-character JSON persistence machinery in `ui_layout.rs`
  (dirty flag, 1 s `maybe_save`, `save_now` on close, corrupt-file tolerance,
  serde-default forward compatibility).
- The `Arc<Mutex<Option<T>>>` request-slot pattern shared by HUD and HTTP API —
  windows stay dumb views over `GameState` + action slots.
- The headless egui smoke-test pattern (`hud.rs` tests) and `ui_layout` unit tests.

**Replace**:
- The 960×540 letterbox `canvas_off` hack and absolute-point persisted positions →
  anchor-relative persistence with the native client's edge-relative remap (§4).
- Ad-hoc unpersisted visibility bools (`show_inventory` etc. on `App`) → per-window
  `open` state in the persisted layout, driven by a window registry (§3).
- `title_bar(false)` + fake "☰ title" drag strips → real custom chrome: 18 px title
  strip, close box, per-window context menu (§5).
- Default egui dark theme + per-widget hardcoded colors → one RoF2-derived theme (§6).
- The duplicated fullscreen/small minimap code paths → one resizable map window.
- `egui_pass`'s ~30 positional parameters → a `UiCtx` bundle (§7).

## 2. Module layout

```
src/ui/
  mod.rs        // WindowManager, per-frame orchestration, UiCtx
  registry.rs   // WindowDef table — the single source of truth for all windows
  persist.rs    // LayoutFile v2 (supersedes ui_layout.rs, which is deleted)
  chrome.rs     // eq_window(): frame, title bar, close box, fades, context menu
  theme.rs      // RoF2 palette + egui Style/Visuals ("EQ but cleaner")
  widgets.rs    // eq_gauge (gradient fill), item_slot, coin_row, con-color helpers
  icons.rs      // TGA/DDS atlas loaders: dragitem*.dds, spells*.tga, gemicons, coins
  windows/      // one file per window, uniform signature
    player.rs target.rs group.rs chat.rs inventory.rs merchant.rs loot.rs
    spellgems.rs spellbook.rs casting.rs skills.rs trainer.rs pet.rs
    quest_journal.rs npc_dialogue.rs actions.rs map.rs selector.rs options.rs
    confirm.rs quantity.rs zone_info.rs help.rs
```

`hud.rs` shrinks to the non-window overlays only (nameplates, loading screen,
connection banner, fps/debug) and is renamed conceptually "overlays"; the window
draw functions migrate into `src/ui/windows/`.

## 3. Window registry + Window Selector (the control panel)

Every window is declared once:

```rust
pub struct WindowDef {
    pub id: &'static str,          // stable persistence key
    pub title: &'static str,
    pub hotkey: Option<egui::Key>, // I, P, G, B, M, K, J, ...
    pub default_anchor: egui::Align2,
    pub default_offset: [f32; 2],
    pub default_size: [f32; 2],
    pub resizable: bool,
    pub closeable: bool,           // Selector: false
    pub default_open: bool,
    pub transient: bool,           // gated by game state (merchant, loot, confirm…)
}
```

The **Window Selector** (native analog: `EQUI_SelectorWnd.xml`, which is itself
titlebar-on, closebox-off) iterates the registry and shows one toggle per
non-transient window with its hotkey, plus global controls: **Lock windows**,
**UI scale slider** (0.5–2.0×), **global opacity**, **fades on/off**, **Reset all**.
It is moveable but `closeable: false` — the registry makes it structurally
impossible to close, and it is exempt from "reset all closes".

Transient windows (merchant, loot, confirmation, quantity, trainer, task-offer)
open automatically from game state, appear greyed-out in the Selector, and their
`open` state is not persisted.

Hotkeys route through the registry (one match statement, no scattered `KeyCode`
arms). Typing focus guard unchanged: egui consumes keys first when a text field
is focused.

## 4. Scaling + persistence model

### Scaling
`zoom = user_scale × min(w/1280, h/720) / dpi_scale_factor`, applied via
`egui_ctx.set_zoom_factor` (design space normalized to **1280×720 points**;
the old 960×540 constants die with `canvas_off`). Result: the whole UI scales
with window size (requirement 1), text stays DPI-crisp, and `user_scale` is a
persisted per-character multiplier exposed in the Selector and Options windows.
`WindowEvent::ScaleFactorChanged` is now handled (currently it isn't — stale-zoom
bug on DPI change without resize).

### Per-window persistence (LayoutFile v2)
File stays `~/.config/eqoxide/ui_layout_<char>.json` (old files load unchanged via
serde defaults; we write `"version": 2`).

```jsonc
{
  "version": 2,
  "locked": false,
  "ui_scale": 1.0,
  "global_alpha": 255,
  "fades": true,
  "screen": [1280.0, 720.0],          // point-space size at last save (for remap)
  "os_window": { "size": [1920, 1080], "pos": [64, 32], "maximized": false },
  "windows": {
    "inventory": { "open": true, "pos": [8, 90], "size": [340, 420], "alpha": 230 }
  }
}
```

**Cross-resolution remap** (matching observed native-client behavior): on load,
for each axis — window in the left/top half of the old
screen keeps its absolute coordinate; in the right/bottom half keeps its distance
from that edge; straddling the center shifts by the center delta; then clamp
on-screen. This is why native EQ windows "stick to their corner" across
resolution changes, and it replaces both `canvas_off` and blind `constrain`.

### OS window geometry (requirement 3b)
- Restore: `App::new` loads the layout before `resumed` creates the window →
  `WindowAttributes::with_inner_size(saved)` `.with_maximized(saved)` and
  `.with_position(saved)` when a position was saved. Default when absent: 1600×900.
- Save: handle `WindowEvent::Resized` + `Moved` (+ `is_maximized()`) into the same
  debounced layout file. **Wayland caveat** (this host): `outer_position()` is
  unsupported and `Moved` never fires — position round-trips only on X11/XWayland;
  size + maximized work everywhere. Documented user-facing in
  `docs/ui-window-management.md`.
- Flush-on-exit gap fixed: the `POST /exit` / SIGTERM path (`about_to_wait` exit
  branch) now calls `save_now()` like `CloseRequested` already does.

## 5. Window chrome & behavior (imitating the native UI where it's good)

`chrome::eq_window(uictx, def, |ui| body)` wraps `egui::Window` with
`title_bar(false)` and draws its own chrome:

- **Title strip**: 18 pt, vertical gradient `#383631→#2A2A28`, 1 px underline,
  centered white title, 12 pt gold **✕** close box (only if `closeable`). Buttons
  fire on release-over-button (native behavior — lets you drag off to cancel).
- **Drag**: title strip drags; body drag-blank areas also drag when unlocked
  (egui default). **Resize**: egui's 8-direction handles for `resizable` defs.
  No clamping during drag (native behavior); clamp/repair happens at load-time
  remap, plus a "title strip must stay reachable" minimal constraint.
- **Lock** (global, Ctrl+L, persisted): blocks move + resize only; clicks still work.
- **Fades** (the signature EQ feel, matching the native client): when the pointer
  has been outside a window's rect for 2 s, animate its opacity to
  `fade_to_alpha` (~50%) over 0.5 s; animate back on re-enter. Global toggle +
  per-window alpha via right-click context menu.
- **Context menu** on the title strip only (not the whole body — fixes the
  right-click conflict): per-window opacity slider, fades toggle, reset window,
  lock all.
- **Z-order**: egui's natural click-to-front, with the Selector and modal dialogs
  on `egui::Order::Foreground` (the native ZLayer-band idea, two bands suffice).

## 6. Theme — "RoF2, but cleaner"

Palette measured from the shipped TGAs (see research; key values):
window bg `#131621` (light rock) / panels `#0D0E14` (dark rock), title
`#302F2B`, bevel hi `#898077` / lo `#3F3C30`, button face `#2A2A2F` with the
signature **blue hover `#46485E`**, brass outline `#9C9C8A`, gold glyphs
`#C5B976`, text `#F0F0F0`, recessed slots `#131313`. Gauge tints are the native
FillTints: HP `240,0,0`, mana `0,128,255`, endurance `240,240,0`, pet
`51,192,51`, XP `220,150,0` with AA overlay `0,80,220`.

- One `theme::apply(ctx)` sets `Visuals`/`Style` (rounding 3 px, 1 px strokes,
  tight 4×3 spacing, 24 px buttons). No per-widget hardcoded colors in windows.
- **Gauges** are the identity element: 12 pt trough `#1E1E1E` + fill drawn as a
  4-vertex mesh with a vertical white→36 % gradient multiplied by the tint
  (exactly emulating `A_GaugeFill`), overlay text left/right, and the native
  animated-fill (value eases toward target rather than jumping).
- **Icons from the real assets** (`icons.rs`): item icons from
  `dragitem1..178.dds` (256×256, 6×6 grid of 40 px cells, `idx = icon_id − 500`,
  sheet `idx/36 + 1`, cell `idx%36`), spell icons from `spells01..07.tga`
  (existing loader generalized), coin glyphs from `window_pieces01.tga`. Sheets
  load lazily, keyed off a new `renderer.eq_ui_dir` config (default
  `~/eq_assets/everquest_rof2/uifiles/default`, env `EQ_UI_DIR` override;
  degrades to text-only when absent). Flat-color chrome, not nine-slice
  textures — deliberate "better, not pixel-identical" call.
- Font: default egui sans at the RoF2 size ladder (10/12/14/16/20) via
  `TextStyle` overrides; Arial-metric clone embedding deferred (native uses GDI
  Arial; no font ships in uifiles).

## 7. App integration

- New `UiState` struct on `App` owns: `WindowManager` (registry + per-window
  runtime state + fades), `LayoutFile`, icon atlases, chat input state, transient
  dialog queues. `egui_pass` takes `&mut UiState` + a `UiCtx<'_>` view bundling
  the read-only refs (`GameState` snapshot fields, zone map, spell db) and the
  action slots grouped into one `Actions` struct of cloned request slots —
  killing the 30-parameter signature.
- Layout save I/O moves off the render thread hot path: `maybe_save` serializes
  only when dirty (unchanged) but writes via a `std::thread::spawn` fire-and-forget
  (tiny file, last-writer-wins, `save_now` stays synchronous for exit).
- Message ring in `GameState` grows 50 → 400 entries so the chat window has real
  scrollback (`VecDeque` cap bump; kinds already tagged).

## 8. Window set (phase 1 — this branch)

Buildable now against existing `GameState` + request slots (per capability
research). ✅ = new functionality vs old HUD.

| Window | Native analog | Notes |
|---|---|---|
| Window Selector | SelectorWnd | non-closeable control panel ✅ |
| Player | PlayerWindow | HP/mana/XP gauges, level/class, stats, coin |
| Target | TargetWindow | HP gauge, con-colored name; attack/consider buttons |
| Group | GroupWindow | roster + HP; invite accept/decline dialog ✅ |
| Chat | ChatWindow | tabs (All/Chat/Combat/System/Loot), scrollback, slash-command input (/say /tell /ooc /shout /gsay) ✅ |
| Inventory | Inventory | equipment grid + general slots, click-move/equip via OP_MoveItem ✅, coin |
| Merchant | MerchantWnd | buy/sell with item icons; sell quantity via Quantity window ✅ |
| Loot | LootWnd | shows pending/queued loot state (interactive pick blocked — see §9) |
| Spell Gems | CastSpellWnd | 9 gems w/ icons, cast on click |
| Casting bar | CastingWindow | progress gauge |
| Spellbook | SpellBookWnd | gems + scroll items only; full book blocked (§9) |
| Skills | SkillsWindow | 77 skills w/ values ✅ |
| Trainer | TrainWindow | transient; train buttons |
| Pet | PetInfoWindow | name + HP from entity, attack/back-off ✅ |
| Quest Journal | TaskWnd | objectives/progress, accept/decline offers |
| NPC Dialogue | (saylink flow) | clickable keywords, history |
| Actions | ActionsWindow | attack, sit, hail, target-nearest, camp (+countdown), who |
| Map | MapViewWnd | unified resizable map (replaces dual minimap), zoom slider |
| Zone/Compass | CompassWnd | heading strip, zone name, loc ✅ |
| Options | OptionsWindow | UI scale, opacity, fades, lock — the UiPrefs surface ✅ |
| Confirm / Quantity | ConfirmationDialog / QuantityWnd | shared modals ✅ |
| Help | (chathelp.txt) | hotkey + slash-command reference ✅ |

Excluded permanently (Sony/live-service, per requirement): marketplace/StationCash,
LoN, live mail, Vivox voice, votes, /petition //bug //feedback to CS, claim/veteran,
paid name/race change, server list/patcher, HTML browser, alert stack, heroic
templates, progression-server unlocks.

## 9. Blocked-on-protocol parity (follow-up issues, not this branch)

The capability research found these need `eq_net`/parser work before their windows
can be real; each gets a GitHub issue and a stub note in the Selector tooltip:

1. **Buff window** — no `OP_Buff` handling at all (profile buff array skipped).
2. **Bag/container windows** — item parser discards sub-items (`item.rs:185-195`).
3. **Bank window** — no bank slots/banker flow.
4. **Spellbook contents** — profile book region unparsed.
5. **Interactive loot** — gameplay loop auto-takes; needs loot-list state instead.
6. **PC trade window** — trade state machine is single-item fire-and-forget.
7. **Item tooltips/ItemDisplay** — stats parsed-past, not stored.
8. Hotbutton bar (needs socials/macros model), aggro meter, respawn-option picker,
   real max-mana (eqoxide#27).

## 10. Testing & verification

- Keep + extend `ui_layout`-style unit tests on `persist.rs` (remap math table
  tests: left-half/right-half/center/clamp cases) and headless
  `egui::Context::run` smoke tests over every registered window in both lock
  states and with empty/populated `GameState`.
- Live verify via the HTTP API: launch against local EQEmu, `/frame` screenshots
  at multiple window sizes, drag/resize/close/reopen, restart to prove
  persistence, resize to prove scaling + remap.
