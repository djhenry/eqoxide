# Race Sizes / Spawn Size Mechanics

## How Spawn Size Is Determined

### Server side

`Spawn_Struct` (`EQEmu/common/patches/titanium_structs.h:265`) carries a `float size`
field at wire offset `0x075`.  `Mob::FillSpawnStruct` (`zone/mob.cpp:1344`) writes the
server's in-memory `size` field directly into it:

```c
ns->spawn.size = size;
```

For **NPCs**, `size` is set from the `npc_types` row; if it is `<= 0.0f` at NPC
construction time, the server falls back to `GetRaceGenderDefaultHeight(race, gender)`
(`zone/npc.cpp:149-150`).

For **player characters** (Clients), `Client::FillSpawnStruct` (`zone/client.cpp:2550`)
**overrides** the inherited Mob size with zero before sending:

```c
ns->spawn.size = 0; // Changing size works, but then movement stops! (wth?)
```

And the Client's own `Mob` constructor is called with `in_size = 0.0f`
(`zone/client.cpp:92`, `zone/client.cpp:399`).  The EQEmu comment notes this is
intentional for protocol reasons (movement breaks when size != 0 for clients in Titanium).

### What the value means

The `size` float on the wire is the **absolute rendered height in EQ world units
(feet)**.  It is NOT a multiplier over any per-race base.  The server stores it as
meters/EQ feet; the client's graphics layer calls `t3dSetActorScaleFactor` to resize
the actor so that it renders at exactly this height.

When `size == 0.0` is received for a player character, the Titanium client uses its
own internal default for that race (the client has its own race height table, matching
the server's `GetRaceGenderDefaultHeight` values).

### SpawnAppearance / ChangeSize

After the initial spawn, size can be changed via `OP_SpawnAppearance` with
`type = AppearanceType::Size` (= 29, `EQEmu/common/eq_constants.h:44`).  The parameter
is cast to `uint32(size)`.  `Mob::ChangeSize` (`zone/mob.cpp:4488-4498`) clamps to
`[1, 255]`, or `[3, 15]` for players/pets.

## GetRaceGenderDefaultHeight — Canonical Values

Source: `EQEmu/common/races.cpp:1398-1515`

The function returns `male_height[race]` or `female_height[race]` indexed by the race
integer constant (see `EQEmu/common/races.h:45-70`).  For all 15 playable races the
male and female tables are **identical** (no gender modifier on base height).

| Race        | Race ID | Default Height (EQ feet) |
|-------------|---------|--------------------------|
| Human       | 1       | 6.0                      |
| Barbarian   | 2       | 7.0                      |
| Erudite     | 3       | 6.0                      |
| Wood Elf    | 4       | 5.0                      |
| High Elf    | 5       | 6.0                      |
| Dark Elf    | 6       | 5.0                      |
| Half Elf    | 7       | 5.5                      |
| Dwarf       | 8       | 4.0                      |
| Troll       | 9       | 8.0                      |
| Ogre        | 10      | 9.0                      |
| Halfling    | 11      | 3.5                      |
| Gnome       | 12      | 3.0                      |
| Froglok     | 26      | 5.0                      |
| Iksar       | 128     | 6.0 (male) = 6.0 (female)|
| Vah Shir    | 130     | 7.0                      |

Index [0] = 6.0 (unused sentinel).  Index [13] = Dark Elf [6.0] at wrong offset? No —
check: index 5 = 6.0 (High Elf), index 6 = 5.0 (Dark Elf).  Confirmed correct above.

### Human vs Elf comparison (the reported discrepancy)

| Race      | Height | Ratio vs Human |
|-----------|--------|----------------|
| Human     | 6.0    | 1.000          |
| High Elf  | 6.0    | 1.000          |
| Half Elf  | 5.5    | 0.917          |
| Wood Elf  | 5.0    | 0.833          |
| Dark Elf  | 5.0    | 0.833          |

Humans and High Elves are **the same height**.  Wood Elves and Dark Elves are 83% of
Human height.  Half Elves are 92%.

## Client-Side Scaling Approach

The server sends `size = 0` for all player characters.  The client treats 0 as "use
race default".  The race default is the same table as the server's
`GetRaceGenderDefaultHeight`.

The `t3dSetActorScaleFactor` function in `EQGfx_Dx8.dll` (decompiled:
`decompiled/ghidra/EQGfx_Dx8.dll.c:8334`) accepts an actor pointer and a new scale
factor.  The scale factor is computed as `desired_height / model_intrinsic_height`.
There is no separate per-race base size table in the client; the `size` field IS the
target rendered height.

## Recommendation for eqoxide

**The spawn `size` field is the absolute target height in EQ world units.**

For player character spawns, the server sends `size = 0`.  The client should resolve
this by substituting `GetRaceGenderDefaultHeight(race, gender)` (which is gender-neutral
for all playable races):

```
fn default_height_for_race(race_id: u32) -> f32 {
    match race_id {
        1  => 6.0,  // Human
        2  => 7.0,  // Barbarian
        3  => 6.0,  // Erudite
        4  => 5.0,  // Wood Elf
        5  => 6.0,  // High Elf
        6  => 5.0,  // Dark Elf
        7  => 5.5,  // Half Elf
        8  => 4.0,  // Dwarf
        9  => 8.0,  // Troll
        10 => 9.0,  // Ogre
        11 => 3.5,  // Halfling
        12 => 3.0,  // Gnome
        26 => 5.0,  // Froglok
        128 => 6.0, // Iksar
        130 => 7.0, // Vah Shir
        _  => 6.0,  // fallback
    }
}
```

The resolved size should drive `archetype_target_height` override so that
`archetype_target_height("elf")` returns 5.0 for a Wood Elf rather than 12.0.

The current `archetype_target_height` in `src/models.rs:702` assigns equal heights to
"humanoid" and "elf" (both 12.0).  This causes Human and Wood Elf to render at the same
visual height even though Wood Elves should be 83% of Human height.  The fix is to
drive the rendered height from `spawn.size` (resolved via the race table above) rather
than from a fixed archetype constant.

## Coordinate Space: Characters vs Zone Geometry

**Confirmed**: EQ world units are feet throughout. Zone BSP geometry (WLD Fragment 36
vertices, `OpenEQ/LegacyFileReader/Wld.cs:504-505`) and character models both use the
same coordinate system with no multiplier between them. The `size` field on the wire
IS the final rendered world-space height in EQ feet; `t3dSetActorScaleFactor` applies
`scale = desired_height / model_intrinsic_height` directly
(`decompiled/ghidra/EQGfx_Dx8.dll.c:8334`). There is no separate player vs NPC render
path for size; both go through the same actor scale mechanism.

Typical doorway heights in human-scale zones (Qeynos, Freeport, etc.) are
approximately 7-8 EQ feet, i.e. just taller than a 6.0-unit Human. A door model's
world-space size is determined by its WLD geometry; the `.zon` placement record
carries an integer `size` that the Rust client applies as `size as f32 / 100.0`
(eqoxide `src/pass.rs:231`) — identical to how the real client scales door
placeables.

## The ×2 Bug in eqoxide (archetype_target_height)

### What the bug is

`archetype_target_height("humanoid") = 12.0` in `src/models.rs:706`.  A Human's
correct EQ height is **6.0 feet**.  The 12.0 value is exactly 2× the correct value.

### How it arose

The `archetype_scale` function (same file, line 678) was calibrated correctly:
- `"humanoid" => 3.55` with a comment `y_extent=1.6902 → 6.0 EQ (human adult)`
- That means `1.6902 × 3.55 ≈ 6.0 EQ feet` — the static model path gives correct height

`archetype_target_height` is used by the **skinned model path** for player/NPCs
rendered with joint animations.  Someone measured a Human character's apparent height
against a doorway and found it took a target of ~12 to match.  But the doorway itself
was being rendered using the static path (archetype_scale × y_extent), which gives 6.0
for a human-height door — so the doorway height was implicitly 6 EQ units.  At
`target=12` the skinned human rendered at 12, which is 2× the doorway.  What actually
happened is:

1. The skinned model's `true_height` was being measured or stored in a different unit
   (e.g. the bind-pose y_extent in raw GLB units before any conversion), making it read
   as ~half the correct value.
2. To compensate, the target was doubled to 12.
3. This doubled the player relative to NPCs/doors, since doors use the uncorrected path.

### Observable symptom

Player character renders ~2× the height of equivalent-race NPCs and ~2× taller than
doorways, because:
- Player/NPC skinned path: `dominant_mesh_scale = target / model.true_height * node_scale`
  with target=12 → scale 2× too large.
- Door/static path: `arch_scale * y_extent` calibrated to 6 EQ → correct size.
- NPC skinned path: same target=12 → NPCs also wrong, but player and NPCs match each
  other, so the mismatch is most visible against geometry (doors, room height).

### The fix

Set `archetype_target_height` values to the raw EQ feet values from the race table.
For the skinned model path, the final call is:

```
effective_size = if spawn.size > 0.0 { spawn.size } else { default_height_for_race(race_id) }
dominant_mesh_scale = effective_size / model.true_height * model.node_scale
```

This requires that `model.true_height` is measured in the same coordinate units as EQ
world space.  If `true_height` is currently stored in raw GLB/model units that do not
equal EQ feet, either fix the converter to write `eq_height` in EQ feet, or apply a
known conversion factor when reading it.  The `archetype_scale` calibration comments
(`y_extent=1.6902 → 6.0 EQ`) imply a factor of `3.55×` for humanoid models.  If
`true_height` (from `idle_extent` of the skinned pose) is similarly in raw model units,
multiply by the archetype_scale factor to convert to EQ feet before storing, or divide
the target by that factor.

Pragmatic near-term fix: change `archetype_target_height("humanoid")` from 12.0 to 6.0,
and `"elf"` from 12.0 to 5.0 for Wood Elf / Dark Elf, 6.0 for High Elf, etc.  This
will halve all skinned character heights relative to doorways and zone geometry.

## Current Status in eqoxide

- `archetype_target_height("humanoid") = 12.0`, `archetype_target_height("elf") = 12.0`
  (`src/models.rs:706`) — this is **wrong**; both are 2× the correct EQ feet values.
- Spawn `size` field is parsed into `Spawn_S.size: f32` (`src/eq_net/protocol.rs:547`)
  but is **never stored in `Entity`** (see `game_state.rs:20-42` — no size field) and
  **never passed to the renderer**.  All spawns use only the archetype constant.
- The `race_to_archetype` mapping collapses many races into "humanoid" or "elf" without
  applying per-race size, so all "humanoid" races render at the same height regardless
  of their EQ default (Barbarian=7.0, Halfling=3.5, Gnome=3.0 all get 12.0).
- Fix: (1) Add `size: f32` to `Entity`, populate it from spawn (resolving 0 to race
  default). (2) Pass `entity.size` through scene to the renderer. (3) Replace
  `archetype_target_height` lookup with `entity.size / model.true_height * node_scale`
  in `src/pass.rs`.
