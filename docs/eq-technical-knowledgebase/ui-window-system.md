# Titanium UI Window System

Investigated 2026-06-23. Sources: the original Titanium game client's UI files
(`uifiles/default/`) and observed behavior of the original Titanium game client
(`eqgame.exe`).

---

## 1. Window definition system (SIDL/XML)

The UI is defined entirely by XML files in `uifiles/<skin>/`.  The schema is
`SIDL.xml` (from the original Titanium game client).

Key XML element types from SIDL.xml:
- `Screen` — a top-level window.  Has Style_* flags (see below).
- `ScreenPiece` (supertype of all child controls) — carries `Location`,
  `Size`, `RelativePosition`, and four anchor fields.
- `Control` — adds Style_VScroll, Style_HScroll, Style_Transparent, Style_Border, DrawTemplate.
- `SuiteDefaults` — declares the five resize cursors and the drag cursor used
  globally: CursorResizeNS, CursorResizeEW, CursorResizeNESW, CursorResizeNWSE,
  CursorDrag  (SIDL.xml:347-352).

### Screen style flags (SIDL.xml:334-342)
```
Style_Titlebar    — draw / allow title bar
Style_Closebox    — X button
Style_Qmarkbox    — ? help button
Style_Minimizebox — _ minimize button
Style_Sizable     — allow edge/corner drag to resize
```
There is NO `Style_Movable` flag in the schema.  Movability is implicit: any
window that has a title bar (Style_Titlebar true) can be dragged by it.
Windows without a title bar can still be moved by dragging their border area
(observed in game — e.g. PlayerWindow, HotButtonWnd, CompassWindow all have
`Style_Titlebar false` but respond to drag).

### Anchor / relative positioning (SIDL.xml:125-156, child elements)
Child pieces inside a Screen use:
- `RelativePosition true/false` — when true, Location is offset from parent.
- `AutoStretch true` — piece stretches to fill parent when it resizes.
- Four offset fields: `TopAnchorOffset`, `BottomAnchorOffset`,
  `LeftAnchorOffset`, `RightAnchorOffset`.
- Four direction flags: `TopAnchorToTop`, `BottomAnchorToTop`,
  `RightAnchorToLeft`, `LeftAnchorToLeft` — when false the anchor is to the
  opposite edge (bottom/right of parent), enabling right/bottom-relative
  layout.

This gives a CSS-like anchor layout: `BottomAnchorToTop=false` +
`BottomAnchorOffset=22` means the piece's bottom edge is 22px from the
parent's bottom.  Used extensively in ChatWindow for the input box
(EQUI_ChatWindow.xml:9-16) and output area (EQUI_ChatWindow.xml:23-29).

---

## 2. Which HUD windows were movable / sizable

Confirmed from XML files (Screen element Style_ flags):

| Window (Screen item name) | Style_Titlebar | Style_Sizable | Notes |
|---|---|---|---|
| ChatWindow | false (WDT_RoundedNoTitle) | **true** | Movable+sizable |
| PlayerWindow | false | false | Movable, not sizable |
| GroupWindow | false | false | Movable, not sizable |
| TargetWindow | **true** | false | Has title, not sizable |
| HotButtonWnd (x4) | false | false | Has closebox, movable |
| BuffWindow | **true** | **true** | Both |
| ShortDurationBuffWindow | (confirmed in defaults.ini as tracked) | |
| CastSpellWnd | false | false | Spell gems, movable only |
| CastingWindow | false | false | Cast bar, movable only |
| CompassWindow | false | false | Movable only |
| SelectorWindow (main menu bar) | false | false | Movable only |
| InventoryWindow | false | false | Movable only |
| SpellBookWnd | false | false | Movable only |
| MapViewWnd | **true** | **true** | Both |
| MerchantWnd | **true** | **true** | Both |
| LootWnd | false | false | Movable only |
| TradeWnd | false | false | Movable only |
| PetInfoWindow | (tracked in defaults.ini) | |
| ActionsWindow | (tracked in defaults.ini) | |

Other sizeable windows (non-HUD / situational): AAWindow, BankWnd, BuffWindow,
SkillsWindow, TrackingWnd, HelpWnd, JournalCatWnd, FriendsWnd, RaidWindow,
TaskWnd, GuildManagementWnd, GuildBankWnd, BarterWnd, BazaarSearchWnd,
MerchantWnd, ItemDisplay, BodyTintWnd, etc.

---

## 3. User interaction model

- **Move**: drag anywhere on the window background (or title bar if present).
  No title bar required — the border/background area acts as the drag target.
- **Resize**: drag the 4 edges or 4 corners when Style_Sizable=true.
  Five resize cursors are declared globally in SuiteDefaults: N/S, E/W,
  NE/SW, NW/SE diagonals (SIDL.xml:347-351).
- **No snap-to-grid** at the window level — the XML has no snap/grid for
  Screen elements.  (`Grid=true` in Ui2DAnimation is sprite sheet layout only.)
- **No explicit min/max size properties** exist in the SIDL schema.  Min
  enforcement happens in code only (eqgame.exe shows a
  hard-coded `if (local_318 - local_328 < 4)` guard — window can't be dragged
  past 4px from screen edge; other minimum-size checks not confirmed by name).
- **No aspect-ratio constraint** confirmed.

---

## 4. Persistence

Two-tier system:

### Tier 1: `defaults.ini` (skin-level defaults)
Path: `uifiles/<skin>/defaults.ini`.  Provides fallback positions per
resolution.  Key format:
```ini
[WindowName]
XPos1024x768=<px>
YPos1024x768=<px>
Width1024x768=<px>   ; only for sizable windows
Height1024x768=<px>  ; only for sizable windows
```
Resolution keys are `{width}x{height}` of the game viewport.
Confirmed resolutions present: 800x600, 1024x768, 1152x864, 1280x720,
1280x1024, 1600x900, 1600x1200.

### Tier 2: Per-window INI sections (runtime state)
Each window writes its own section to a per-login-session INI.  The per-window
keys are resolution-qualified the same way.  Observed in `eqlsUIConfig.ini`
(login screen) and inferred for in-game windows (eqgame.exe contains the
save/load code).

Per-window INI section fields (eqgame.exe):
```ini
[WindowName]
INIVersion=<int>
XPos{W}x{H}=<px>
YPos{W}x{H}=<px>
Width{W}x{H}=<px>    ; if bit 2 of flags set
Height{W}x{H}=<px>   ; if bit 2 of flags set
BGTint.red=<0-255>    ; if bit 4 of flags set
BGTint.green=<0-255>
BGTint.blue=<0-255>
BGType=<1 or 2>
Fades=true/false      ; if bit 3 of flags set
Delay=<ms>
Duration=<ms>
Alpha=<0-255>
FadeToAlpha=<0-255>
Locked=true/false     ; always written
```

### Per-character file naming (eqgame.exe)
The per-character file has the prefix `.\UI_<CharName>_` and then appends
server name and `.ini`.  Full format inferred as:
`UI_<CharacterName>_<ServerShortName>.ini`

HotButton pages are saved separately:
`userdata\HB_<page>_<CharName>_<ServerName>.ini` (eqgame.exe).

---

## 5. Global state flags in eqclient.ini

```ini
[Defaults]
LockWindows=FALSE        ; globally lock all window positions (eqgame.exe)
HidePlayerWin=FALSE      ; hide/show per major window (eqgame.exe)
HidePartyWin=FALSE
HideTargetWin=FALSE
HideSpellsWin=FALSE
HideBuffWin=FALSE
HideHotboxWin=FALSE
HideChatWin=FALSE
HideMainMenuWin=FALSE
UISkin=default           ; which uifiles/ subdirectory to load (eqgame.exe)
```

---

## 6. Alpha / fading system

Global settings stored in the UI config object:
- `GlobalAlpha` — default 0xFF (eqgame.exe)
- `GlobalFadeDelay` — default 2000 ms (eqgame.exe)
- `GlobalFadeDuration` — default 500 ms (eqgame.exe)
- `GlobalFadeToAlpha` — default 0x80 (eqgame.exe)

Sliders in EQUI_OptionsWindow.xml expose:
- `ODP_WindowAlphaSlider` — "Window Transparency" (line 1450)
- `ODP_FadeDelaySlider` — "Fade Delay (seconds)" (line 1338)
- `ODP_FadeDurationSlider` — "Fade Duration (seconds)" (line 1394)
- `ODP_FadeToAlphaSlider` — "Fade-to Transparency" (line 1527)

Per-window: each window's INI section stores its own `Alpha`, `FadeToAlpha`,
`Delay`, `Duration`, `Fades=true/false`.

Fading behavior: when mouse moves away from a window, after `Delay` ms it
fades over `Duration` ms to `FadeToAlpha` (0=fully transparent, 255=opaque).
Moving the mouse back instantly restores `Alpha`.

---

## 7. Per-window Locked flag

Each window stores `Locked=true/false` in its INI section (eqgame.exe).
This is per-window lock, distinct from the global `LockWindows` flag in
eqclient.ini.  The global lock flag is checked in eqgame.exe.

There is also a chat-specific `ChatManager/LockedActiveWindow` key
(eqgame.exe).

---

## 8. Context menus / right-click title bar

There is a `Style_Qmarkbox` (SIDL.xml:338) that places a `?` button on the
title bar for help popups.  The `CascadeMenu` system (eqgame.exe) is
used for in-game popup menus (e.g. right-clicking NPCs/players), not directly
for window management.  A global `ContextMenus` option (eqgame.exe)
controlled whether these context menus were enabled.  No confirmed
right-click-title-bar-to-get-window-options behavior (would need more tracing
of mouse handling to fully confirm).

---

## Recommendation for eq_client_lite

Minimum viable HUD window system to match Titanium behavior:

1. **All windows are movable** — drag by background (not just title bar).
   Only some have a drawn title bar (Style_Titlebar=true).

2. **~10 HUD windows are sizable**: ChatWindow, BuffWindow, MapViewWnd,
   MerchantWnd, and most secondary windows.  Core HUD (Player/Group/Target/
   HotButton/Spells/Compass) are fixed size, movable only.

3. **Persist per window, per resolution**: INI section `[WindowName]` with
   `XPos{W}x{H}`, `YPos{W}x{H}`, optionally `Width{W}x{H}` /
   `Height{W}x{H}`.  Fall back to `uifiles/default/defaults.ini` on first run.

4. **Global LockWindows** flag in eqclient.ini — when TRUE, ignore all drag
   and resize input.

5. **Per-window alpha + fade** — store `Alpha` (0-255), `FadeToAlpha`,
   `Delay` (ms), `Duration` (ms), `Fades` (bool) per window in INI.

6. **Anchored children** — when a window resizes, children with
   `RightAnchorToLeft=false` and/or `BottomAnchorToTop=false` track the
   right/bottom edge (CSS right/bottom anchoring).  This is required for
   ChatWindow to work correctly (input box must stay at the bottom).

7. **Resolution-qualified keys** — use screen pixel size `{W}x{H}` as suffix.
   This lets the player move between resolutions and maintain sensible defaults.

8. **No snap-to-grid, no aspect constraints** — none exist in Titanium.
