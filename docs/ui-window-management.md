# HUD Window Management: Movable, Resizable, Persistent Layouts

Most HUD windows in the egui client are now **movable** and can be **resized** (some windows). Window
positions and sizes are saved and restored **per-character** between sessions.

---

## Movable Windows

When windows are **unlocked**, these HUD windows can be dragged:

- Status bar (player stats, HP/mana/etc.)
- Messages (log panel)
- Map (minimap)
- Controls (navigation buttons)
- Actions (action grid, spell gems, auto-attack)
- Inventory
- NPC Dialogue (quest dialogue panel)

**Transient overlays** remain fixed in place (not movable):

- FPS counter (top-right)
- Loading screen
- 3D nameplates (floating NPC/player labels)
- Debug overlay

---

## Resizable Windows

These windows support both **dragging (moving) and resizing** via edge/corner handles:

- Messages (left/right/bottom edges, corners)
- Map (left/right/bottom edges, corners)
- Inventory (left/right/bottom edges, corners)
- NPC Dialogue (left/right/bottom edges, corners)

Other movable windows support **dragging only** (fixed size).

---

## How to Move a Window

When windows are **unlocked**:

1. A thin **header strip** (the window's name) appears at the top of each movable window.
2. **Click and drag** the header to move the window anywhere on screen.
3. Windows are constrained so they cannot be dragged fully off-screen.

---

## Locking/Unlocking Windows

**Press `Ctrl+L`** to toggle the lock state.

Alternatively, use the **`⚙ UI` menu** (top-left corner):
- Click the gear icon to open the UI menu
- Select **"Lock windows (Ctrl+L)"** to toggle

**When locked:**
- All windows are frozen in place (prevents accidental moves/resizes)
- Drag headers disappear
- Right-click context menus are still available

---

## Window Context Menu

**Right-click any window** to open a context menu with:

- **Opacity slider** — adjust the per-window transparency (0–255 alpha)
- **Reset this window** — restore the window to its default position and size
- **Lock all windows** — checkbox to toggle the global lock state (same as `Ctrl+L`)

---

## UI Menu (Top-Left Gear Icon)

The **`⚙ UI`** menu provides:

- **Lock windows (Ctrl+L)** — toggle the global lock state
- **Reset all windows** — restore all windows to their default positions and sizes

---

## Persistence and Storage

Window layouts are **saved to disk** per character:

**File location:** `ui_layout_<CharacterName>.json` in the process working directory.

Non-alphanumeric characters in the character name are **stripped** from the filename (e.g.,
a character named "Cleric-Alt" saves to `ui_layout_ClericAlt.json`).

**Format** (JSON):

```json
{
  "locked": true,
  "windows": {
    "status_hud": {
      "pos": [100, 50],
      "size": null,
      "alpha": 255
    },
    "message_log": {
      "pos": [400, 200],
      "size": [300, 400],
      "alpha": 200
    },
    "minimap": {
      "pos": null,
      "size": null,
      "alpha": 255
    }
  }
}
```

**Window IDs:**

- `status_hud` — player stats bar
- `message_log` — messages/chat panel
- `minimap` — map window
- `control_bar` — navigation/movement buttons
- `action_grid` — spells, auto-attack, sit/stand
- `inventory` — character inventory
- `npc_dialogue` — NPC quest dialogue panel

**Field meanings:**

- `locked` — whether all windows are locked (boolean)
- `pos` — `[x, y]` screen coordinates (top-left), or `null` to use default
- `size` — `[width, height]`, or `null` for default (non-resizable windows always show `null`)
- `alpha` — opacity 0–255 (255 = opaque, 0 = invisible)

**To reset all layouts:** Delete the `ui_layout_<CharacterName>.json` file. Next login will
restore default positions.

---

## Saving Behavior

Window positions and sizes are **saved automatically**:

- **Debounced** (~1 second) while dragging or resizing (avoid excessive disk writes)
- **Flushed to disk** when the application window closes

Changes take effect on the next login for that character.
