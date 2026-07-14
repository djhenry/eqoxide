# The Window System (UI overhaul, #162)

The in-game UI is a registry-driven window system (`src/ui/`) themed after the
native RoF2 client. Every interactive element lives in a window; windows are
moveable, most are resizeable, and everything persists per character.
Architecture details: `docs/ui-overhaul-design.md`.

## The Window Selector

The **Windows** panel (top center by default) lists every window with a toggle
button and hosts the global controls: **Lock**, **Fade**, **UI scale**, and
**Reset all**. It can be moved but **never closed** — if you lose your layout,
the Selector is always there to recover from.

## Windows

| Window | Hotkey | Notes |
|---|---|---|
| Player | — | HP/mana/XP gauges, coin, stats |
| Target | — | con-colored name, HP, Attack/Consider |
| Group | G | roster, HP bars, invite Accept/Decline, leader tools |
| Chat | — | tabs (All/Chat/Combat/System/Loot), scrollback, slash commands |
| Inventory | I | worn grid + general slots, click-to-move, stack counts |
| Spell Gems | — | 9 gems, click to cast |
| Spellbook | B | memorized gems + scrolls (full book: follow-up) |
| Skills | K | trained skills, "show untrained" toggle |
| Pet | — | pet name/HP + commands |
| Quest Journal | T | active tasks w/ objectives, offers, history |
| Actions | — | attack, sit, hail, target-nearest, camp |
| Map | M | resizable zone map, zoom slider + scroll |
| Compass | — | heading tape + /loc |
| Options | O | UI scale, fades, lock, reset |
| Help | H | hotkeys, slash commands, window how-to |

**Transient windows** (Merchant, Loot, Trainer, Casting bar, NPC Dialogue,
Confirm/Quantity) open automatically from game state and close with it. Their
✕ dismisses them for the current session (and ends the session where a
protocol path exists — merchant close, trainer end-training).

## Moving, resizing, closing

- **Drag** anywhere on a window that isn't a control (title strip included).
  Windows drag freely — you can tuck them partly off-screen like the native
  client; they're pulled back on-screen at next load if the resolution changed.
- **Resize** from any edge/corner of resizable windows.
- **✕** in the title strip closes; reopen from the Selector or hotkey.
- **Right-click** a window for per-window opacity, fade toggle, reset, lock.
- **Ctrl+L** (or the Selector/Options checkbox) locks all windows: no move, no
  resize, clicks still work.
- **Fades**: windows the mouse hasn't been over for ~2 s dim to ~40 %
  opacity and restore on approach (native window behavior). Global toggle in
  the Selector; per-window opacity in the right-click menu.

## Scaling

The whole UI scales with the OS window: the design canvas is 1280×720 points
and the zoom is `ui_scale × min(w/1280, h/720) / dpi`. The per-character
**UI scale** multiplier (0.5–2×) is in the Selector and Options windows.

## Persistence

`~/.config/eqoxide/ui_layout_<Character>.json` (version 2), saved debounced
(1 s) and flushed on every exit path:

- per-window: open/closed, position, size (content), opacity
- global: lock, fades, UI scale
- **OS window geometry**: inner size + maximized (+ position where the
  platform allows reading it — X11 yes, **Wayland no**: compositors don't
  expose window position, so on Wayland only size/maximized restore)

Positions are stored with the screen size they were saved under; loading at a
different size runs the native client's edge-relative remap (windows keep
their corner/edge relationship instead of drifting). Old v1 layout files are
migrated by dropping their (incompatible) geometry once; settings survive.

Delete the file to reset everything for that character.

## Chat commands

`/say` (default), `/tell <name> <msg>` (`/t`), `/r` (reply), `/ooc`,
`/shout`, `/g`|`/gsay`|`/group`, `/camp`.

## For agent developers

Windows read the per-frame `SceneState` snapshot and write the same
request slots the HTTP API uses — anything a window does, an agent can do via
`/v1/...` and vice versa. Screenshot via `GET /v1/observe/frame` (1024 px).
