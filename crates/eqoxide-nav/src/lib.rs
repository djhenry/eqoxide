//! `eqoxide-nav` — the navigation domain, extracted into its own workspace crate (#544 Step 2f).
//!
//! Depends ONLY on `eqoxide-core` (physics constants, `region_map`, `coord`, `game_state`),
//! `eqoxide-ipc` (`MoveIntent` + the `Nav*`/`World*` slot views), and `eqoxide-assets`
//! (`ZoneAssets`/`MeshData`/`RenderMode`) — never on any app-layer crate. The app crate re-exports
//! this crate as `crate::nav` (and `traversability` at its own root) so every existing
//! `crate::…` / `crate::traversability::…` path across the tree keeps resolving unchanged.
//!
//! `steering` holds the pure, net-independent walker math (pursuit carrots, replan/arrival
//! decisions, the fast-steering cursor). It takes positions/paths and depends only on `assets`
//! types — no `EqStream`, no packets. `planner` holds the pathfinding worker threads. `walker`
//! holds the path-walker state machine itself (`Walker`) — the `/goto` route, stall/backoff/
//! oscillation recovery, and arrival, driven once per tick by the app's `ActionLoop`. `Walker`
//! moves the player ONLY by writing a `MoveIntent` into the shared `nav_intent` slot — the SAME
//! slot native WASD input writes — never the position or controller directly (see `walker`'s
//! module doc for the full intent-only boundary).
//!
//! `traversability` is the planner's clearance abstraction (#378) — it lives here too because the
//! planner and the walker both consume it. The walker-sim tests that step the REAL app-layer
//! `CharacterController` are nav+movement integration tests and live in the app crate's
//! `tests/walker_sim.rs`, not here (that controller is the one dependency this crate must not have).

pub mod collision;
pub mod diagnostics;
pub mod planner;
pub mod steering;
pub mod traversability;
pub mod walker;
pub mod water_grid;
pub mod zone_assets;
