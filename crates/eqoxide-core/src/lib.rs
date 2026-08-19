//! `eqoxide-core` — the dependency-free leaf modules peeled off the `eqoxide` crate as the first
//! member of the Cargo workspace (#544 Step 2a).
//!
//! These modules were verified to have **no up-reference** into any higher layer (net, render, ui,
//! command) before extraction, so this crate is a pure leaf that the app crate (`eqoxide`) depends
//! on — never the reverse. `crate::` inside this crate now resolves to `eqoxide_core`; the app crate
//! re-exports each module (`pub use eqoxide_core::…`) so existing `crate::<mod>` paths across the
//! tree keep resolving.
//!
//! - `charname`    — character-name normalization + the locally-decidable subset of the server's
//!   OP_ApproveName name rules (#1092)
//! - `config`      — client config (YAML) loading
//! - `coord`       — EQ coordinate-space math (deliberately dependency-free)
//! - `eqstr`       — EQ string-table (`eqstr_us.txt`) lookups
//! - `game_state`  — server-authoritative world snapshot the render/net/http layers share (#544 Step 2b)
//! - `pet`         — RoF2 `PetCommand_Struct` command constants (#544 Step 2h)
//! - `physics`     — pure movement/physics constants + kinematics shared by movement/nav (#544 Step 2d)
//! - `race_class`  — EQ class-id→name / race-id→archetype-code lookup tables (#544 Step 2h)
//! - `region_map`  — zone `.wtr` region/water map parsing
//! - `skills`      — skill id ↔ name tables
//! - `spells`      — `spells_us.txt` id→{name,icon} tables (#544 Step 2b)
//! - `ucs`         — UCS (chat-server) connection-params POD (#544 Step 2b)
//! - `zone_cross`  — zone-line crossing decisions shared by the net thread and the HTTP surface
//!   (the #679/#683 unresolved-cross gate, the #713 attempt bound + best-effort marker)
//! - `zone_map`    — zone short-name / id maps

pub mod afloat;
pub mod charname;
pub mod config;
pub mod coord;
pub mod eqstr;
pub mod game_state;
pub mod pet;
pub mod physics;
pub mod race_class;
pub mod region_map;
pub mod skills;
pub mod sky;
pub mod spells;
pub mod ucs;
pub mod weather;
pub mod zone_cross;
pub mod zone_map;
