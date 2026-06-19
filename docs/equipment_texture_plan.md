# Equipment Texture Rendering Plan

> **SUPERSEDED (2026-06-19).** This plan was written against an incorrect model of the
> renderer (it assumed runtime `_chr.s3d` loading + per-frame filename construction; the
> renderer actually uses prebaked GLB loaded once at startup). It also has the arm/wrist/
> hand slot mapping wrong and proposes selecting one primitive per slot (primitives are
> disjoint regions and must all be drawn). Use instead:
> - Verified facts: `docs/equipment-textures-findings.md`
> - Corrected design: `docs/superpowers/specs/2026-06-19-equipment-textures-design.md`

## Problem

Characters are rendered "naked" because `Spawn_S.equipment[36]` (9 slots × u32 LE) is parsed but discarded. EQ renders armor as **texture replacements** on the character body mesh — not separate meshes.

## How EQ Equipment Textures Work

From inspecting `globalhum_chr.s3d`:
- Character mesh `HUM_DMSPRITEDEF` has **25 primitives**, each named by body slot
- Material names encode slot: `HUMCH0001_MDF` = chest, `HUMLG0001_MDF` = legs, `HUMFT0001_MDF` = feet, `HUMHN0001_MDF` = upper arms, `HUMFA0001_MDF` = forearms, `HUMHE0001_MDF` = head
- Each primitive references a texture like `humch0001.dds` (naked) or `humch0101.dds` (armored)
- The `equipment` u32 per slot IS the armor type number (e.g., `10` → swap chest textures to `humch10XX.dds`)
- `global*_amr.s3d` files provide additional armor textures (BMP only, no WLD) using 2-char race codes
- Tint colors (`equipment_tint`) are RGB multipliers applied to the texture

### Texture Slot Mapping

| Index | EQEmu Enum     | Body Position | Slot Code |
|-------|---------------|---------------|-----------|
| 0     | armorHead     | Head/Helmet   | `he`      |
| 1     | armorChest    | Chest/Torso   | `ch`      |
| 2     | armorArms     | Upper Arms    | `hn`      |
| 3     | armorWrist    | Wrists        | `hn`      |
| 4     | armorHands    | Hands/Gloves  | `fa`      |
| 5     | armorLegs     | Legs/Pants    | `lg`      |
| 6     | armorFeet     | Feet/Boots    | `ft`      |
| 7     | weaponPrimary | Primary weapon| (later)   |
| 8     | weaponSecondary| Off-hand     | (later)   |

### Texture Naming Convention

- `_chr.s3d`: `{race_3char}{slot_2char}{armor_2d}{variant_2d}.dds` — e.g., `humch0101.dds`
- `_amr.s3d`: `{race_2char}{gender_1char}{slot_2char}{armor_2d}{variant_2d}.bmp` — e.g., `homch2102.bmp`

---

## Implementation Steps

### Step 1: Add Equipment Fields to Entity and Billboard

**Files:** `src/game_state.rs`, `src/scene.rs`

**Entity struct** — add:
```rust
pub equipment: [u32; 9],          // material IDs per slot
pub equipment_tint: [[u8; 3]; 9], // RGB tint per slot
pub equip_chest2: u8,             // alternate chest variant
pub helm: u8,                     // helm graphic
pub showhelm: u8,                 // helm visibility
pub gender: u8,                   // 0=male, 1=female
pub class_id: u8,                 // class
pub bodytype: u8,                 // body type variant
```

**Billboard struct** — add:
```rust
pub equipment: [u32; 9],
pub equipment_tint: [[u8; 3]; 9],
pub gender: u8,
pub helm: u8,
pub showhelm: u8,
```

**`register_spawn()`** in `src/eq_net/packet_handler.rs` — parse equipment bytes:
```rust
let equipment: [u32; 9] = std::array::from_fn(|i| {
    u32::from_le_bytes(spawn.equipment[i*4..(i+1)*4].try_into().unwrap())
});
let equipment_tint: [[u8; 3]; 9] = std::array::from_fn(|i| {
    [spawn.equipment_tint[i*4], spawn.equipment_tint[i*4+1], spawn.equipment_tint[i*4+2]]
});
```

Store on `Entity`, propagate through `SceneState::from_game_state()` → `Billboard`.

---

### Step 2: Load Equipment Textures at Startup

**Files:** `src/assets.rs`, `src/renderer.rs`

Create a new `EquipmentTextures` struct:
```rust
pub struct EquipmentTextures {
    textures: HashMap<String, TextureData>,  // keyed by lowercase filename
    bind_groups: Vec<(String, wgpu::BindGroup)>,
}
```

**Loading logic:**
1. For each archetype, load the `_chr.s3d` archive → extract ALL textures (BMP/DDS)
2. Also load textures from `_amr.s3d` archives (global17-23_amr.s3d)
3. Index all textures by lowercase filename
4. Upload each unique texture to GPU and create a bind group

**Which S3D files to load:**

| Archetype  | _chr.s3d files                    | _amr.s3d files        |
|------------|-----------------------------------|-----------------------|
| humanoid   | globalhum_chr.s3d, globalhuf_chr.s3d | global17-23_amr.s3d |
| elf        | globalelf_chr.s3d, globalelf_chr2.s3d | global17-23_amr.s3d |
| dwarf      | globaldwf_chr.s3d, globaldwf_chr2.s3d | global17-23_amr.s3d |
| gnoll      | globalgnm_chr.s3d                 | global17-23_amr.s3d  |
| frog       | globalfroglok_chr.s3d             | global17-23_amr.s3d  |

---

### Step 3: Map Primitives to Body Slots

**File:** `src/models.rs`

When loading a character model, record which primitive index maps to which body slot from the WLD material name:

```rust
fn material_name_to_slot(name: &str) -> Option<usize> {
    let upper = name.to_uppercase();
    if upper.contains("CH") { Some(1) }      // Chest
    else if upper.contains("LG") { Some(5) } // Legs
    else if upper.contains("FT") { Some(6) } // Feet
    else if upper.contains("HN") { Some(2) } // Arms (upper)
    else if upper.contains("UA") { Some(2) } // Arms (upper alt)
    else if upper.contains("FA") { Some(4) } // Hands (forearm)
    else if upper.contains("HE") { Some(0) } // Head
    else { None }
}
```

Store `Vec<Option<usize>>` on `GpuStaticModel` / `GpuSkinnedModel` mapping each mesh index to its equipment slot.

---

### Step 4: Dynamic Texture Binding at Render Time

**Files:** `src/pass.rs`, `src/renderer.rs`

Modify `encode_entity_pass()` and `encode_player_pass()`:

1. For each entity, determine archetype → look up `GpuModel`
2. For each mesh primitive:
   - Get body slot from primitive-to-slot mapping (Step 3)
   - Look up `equipment[slot]` material ID
   - Construct texture filename: `{race}{slot_code}{material_id:02d}01.dds`
   - Look up in `EquipmentTextures` map
   - If found, use that texture's bind group; else fall back to default
   - Apply tint from `equipment_tint[slot]` via `EntityUniform.tint`

**Texture filename construction:**
```rust
fn equipment_texture_name(race: &str, slot: usize, material_id: u32) -> String {
    let slot_code = match slot {
        0 => "he", 1 => "ch", 2 => "hn", 3 => "hn",
        4 => "fa", 5 => "lg", 6 => "ft", _ => "",
    };
    format!("{}{}{:02}01.dds", race.to_lowercase(), slot_code, material_id)
}
```

**Tint application:** The existing `EntityUniform.tint` already multiplies texture color in the shader. Set from `equipment_tint[slot]` when tint is non-zero.

---

### Step 5: Handle Race Code Mapping

**File:** `src/models.rs`

Map numeric EQ race ID + gender to 3-char texture prefix:

```rust
fn eq_race_to_texture_prefix(race_id: u32, gender: u8) -> Option<&'static str> {
    match (race_id, gender) {
        (1, 0) => Some("hum"),   // Human Male
        (1, 1) => Some("huf"),   // Human Female
        (2, 0) => Some("bam"),   // Barbarian Male
        (2, 1) => Some("baf"),   // Barbarian Female
        (3, 0) => Some("elm"),   // Wood Elf Male
        (3, 1) => Some("elf"),   // Wood Elf Female
        (4, 0) => Some("him"),   // High Elf Male
        (4, 1) => Some("hif"),   // High Elf Female
        (5, 0) => Some("dam"),   // Dark Elf Male
        (5, 1) => Some("daf"),   // Dark Elf Female
        (6, 0) => Some("erm"),   // Erudite Male
        (6, 1) => Some("erf"),   // Erudite Female
        (7, 0) => Some("twm"),   // Troll Male
        (7, 1) => Some("twf"),   // Troll Female
        (8, 0) => Some("ogm"),   // Ogre Male
        (8, 1) => Some("ogf"),   // Ogre Female
        (9, 0) => Some("gnm"),   // Gnome Male
        (9, 1) => Some("gnf"),   // Gnome Female
        (10, 0) => Some("hom"),  // Half-Elf Male
        (10, 1) => Some("hof"),  // Half-Elf Female
        (11, 0) => Some("dwm"),  // Dwarf Male
        (11, 1) => Some("dwf"),  // Dwarf Female
        (12, 0) => Some("ikm"),  // Iksar Male
        (12, 1) => Some("ikf"),  // Iksar Female
        (13, 0) => Some("frm"),  // Froglok Male
        (13, 1) => Some("frf"),  // Froglok Female
        _ => None,
    }
}
```

---

### Step 6: WearChange Packet Handler (Dynamic Updates)

**Files:** `src/eq_net/protocol.rs`, `src/eq_net/packet_handler.rs`

Add WearChange opcode and struct:
```rust
pub const OP_WEAR_CHANGE: u16 = 0x6427; // Titanium — verify against EQEmu patches

#[repr(C, packed)]
pub struct WearChange_S {
    pub spawn_id: u16,
    pub material: u32,
    pub unknown06: u32,
    pub elite_material: u32,
    pub hero_forge_model: u32,
    pub unknown18: u32,
    pub color: u32,       // 0x00RRGGBB
    pub wear_slot_id: u8,
}
```

Handler updates `Entity.equipment[slot]` and `Entity.equipment_tint[slot]` in `GameState`.

---

## Implementation Order

| Phase | Steps | Deliverable |
|-------|-------|-------------|
| 1     | Step 1 | Equipment data parsed and stored (no visual change) |
| 2     | Steps 2-5 | Characters show equipped armor textures |
| 3     | Step 6 | Runtime equip/unequip updates |

---

## Verification

1. Connect to a real EQ server with a character wearing armor
2. Characters should display armor textures instead of naked models
3. Tint colors should be applied correctly
4. NPCs with equipment data should also show armor
5. Use `s3d_to_gltf --list` to verify texture filenames match constructed names

---

## Key Files Reference

| File | Role |
|------|------|
| `src/eq_net/protocol.rs:446-520` | Spawn_S struct with equipment fields |
| `src/eq_net/packet_handler.rs:507` | register_spawn() — where equipment is parsed |
| `src/game_state.rs:19-35` | Entity struct — needs equipment fields |
| `src/scene.rs:5-16` | Billboard struct — needs equipment fields |
| `src/models.rs:12-35` | ModelAsset — needs primitive-to-slot mapping |
| `src/gpu.rs:55-94` | GpuStaticModel/GpuSkinnedModel — needs slot mapping |
| `src/pass.rs:344-404` | render_static_model() — needs texture override |
| `src/renderer.rs:212-300` | load_character_models() — needs texture loading |
| `src/assets.rs:240-389` | ZoneAssets::load() — S3D texture extraction |
