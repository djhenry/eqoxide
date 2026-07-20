//! `eqoxide-core` — the dependency-free leaf modules peeled off the `eqoxide` crate as the first
//! member of the Cargo workspace (#544 Step 2a).
//!
//! These modules were verified to have **no up-reference** into any higher layer (net, render, ui,
//! command) before extraction, so this crate is a pure leaf that the app crate (`eqoxide`) depends
//! on — never the reverse. `crate::` inside this crate now resolves to `eqoxide_core`; the app crate
//! re-exports each module (`pub use eqoxide_core::…`) so existing `crate::<mod>` paths across the
//! tree keep resolving.
//!
//! - `config`      — client config (YAML) loading
//! - `coord`       — EQ coordinate-space math (deliberately dependency-free)
//! - `eqstr`       — EQ string-table (`eqstr_us.txt`) lookups
//! - `game_state`  — server-authoritative world snapshot the render/net/http layers share (#544 Step 2b)
//! - `physics`     — pure movement/physics constants + kinematics shared by movement/nav (#544 Step 2d)
//! - `region_map`  — zone `.wtr` region/water map parsing
//! - `skills`      — skill id ↔ name tables
//! - `spells`      — `spells_us.txt` id→{name,icon} tables (#544 Step 2b)
//! - `ucs`         — UCS (chat-server) connection-params POD (#544 Step 2b)
//! - `zone_map`    — zone short-name / id maps

pub mod config;
pub mod coord;
pub mod eqstr;
pub mod game_state;
pub mod physics;
pub mod region_map;
pub mod skills;
pub mod spells;
pub mod ucs;
pub mod zone_map;
