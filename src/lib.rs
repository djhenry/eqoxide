//! `eqoxide` — a standalone Rust EverQuest (Titanium) observer/renderer.
//!
//! Connects directly to a local EQEmu server, renders the current zone in 3D (wgpu), and exposes
//! an HTTP API (port 8765) so an agent can drive the character. It runs either as a GM observer or
//! as an ordinary non-GM player that fights/levels/travels/buys. See `docs/architecture.md` and
//! `docs/autonomous-play.md`.
//!
//! Roughly three concurrent halves, glued by `Arc<Mutex<…>>` request slots + an mpsc packet channel
//! (wired up in `main.rs`):
//! - **render/app** (`app`, `renderer`, `pass`, `gpu`, `pipeline`, `camera`, `scene`, `models`,
//!   `anim`, `billboard`, `hud`) — the winit/wgpu event loop and everything drawn.
//! - **eq_net** (`eq_net::*`) — login flow, the zone gameplay loop, packet decode, and the
//!   navigation/action thread that drains the request slots.
//! - **http** (`http`) — the agent-facing REST API that writes those same request slots.
//!
//! `assets`/`game_state`/`camera_state`/`config`/`eqstr`/`zone_map` are shared support modules.

// The dependency-free leaf modules now live in the `eqoxide-core` workspace crate (#544 Step 2a/2b).
// Re-export them at this crate's root so existing `crate::<mod>` / `eqoxide::<mod>` paths across the
// tree keep resolving unchanged. Step 2b added `game_state`/`spells`/`ucs`: `UcsInfo` was relocated
// DOWN into core (`eq_net::ucs` re-exports it), which let `game_state` — and the `spells` it pins —
// follow it down without any up-reference into a higher layer.
pub use eqoxide_core::{config, coord, eqstr, game_state, region_map, skills, spells, zone_map};

// The inter-thread request-slot types now live in the `eqoxide-ipc` workspace crate (#544 Step 2c),
// which depends only on `eqoxide-core`. Alias it as this crate's `ipc` module so every existing
// `crate::ipc::…` / `eqoxide::ipc::…` path (and `command_state`'s `pub use crate::ipc::…`
// re-exports) keeps resolving unchanged. `movement`/`camera_state`/`profiling` re-export the pure
// type definitions that moved down into it (`MoveIntent`, `ControllerView`, `CameraCmd`/`CameraSnapshot`/
// `CameraMode`, `FrameProfile`/`FrameSample`) so their `crate::<mod>::<Type>` paths are unaffected too.
pub use eqoxide_ipc as ipc;

// The zone-geometry types + GLB loader (`ZoneAssets`, `MeshData`, `RenderMode`, `expand_objects`,
// `COLLISION_MESH_TAG`) now live in the `eqoxide-assets` workspace crate (#544 Step 2e), depending
// only on `eqoxide-core` + external crates (gltf/glam/anyhow/serde_json/tracing) — a clean leaf
// `eqoxide-nav` can depend on next. Alias it as this crate's `assets` module so every existing
// `crate::assets::…` / `eqoxide::assets::…` path keeps resolving unchanged.
pub use eqoxide_assets as assets;

// The navigation domain (`collision`, `planner`, `steering`, `walker`, `water_grid`) and the
// planner's `traversability` clearance abstraction now live in the `eqoxide-nav` workspace crate
// (#544 Step 2f), depending only on `eqoxide-core` + `eqoxide-ipc` + `eqoxide-assets` (+ `tracing`).
// Alias it as this crate's `nav` module and re-export `traversability` at the root so every existing
// `crate::nav::…` / `eqoxide::nav::…` and `crate::traversability::…` path keeps resolving unchanged.
// The nav+movement walker-sim integration tests (which step the app-layer `CharacterController` that
// stays in `movement`) live in this crate's `tests/walker_sim.rs`.
pub use eqoxide_nav as nav;
pub use eqoxide_nav::traversability;

// `CommandState` — the typed write-path facade over the view→model command IPC slots — now lives
// in the `eqoxide-command` workspace crate (#544 Step 2g), depending only on `eqoxide-core` +
// `eqoxide-ipc`. Alias it as this crate's `command_state` module so every existing
// `crate::command_state::…` / `eqoxide::command_state::…` call site (http/*, app.rs,
// eq_net/action_loop.rs, ui/*) keeps resolving unchanged.
pub use eqoxide_command as command_state;

pub mod anim;
pub mod app;
pub mod asset_sync;
pub mod billboard;
pub mod camera;
pub mod debug_zone;
pub mod gpu;
pub mod head;
pub mod logging;
pub mod models;
pub mod movement;
pub mod profiling;
pub mod pass;
pub mod pipeline;
pub mod renderer;
pub mod scene;

// Modules only needed by the full client binary.
pub mod camera_state;
pub mod crash;
pub mod eq_net;
pub mod frame_capture;
pub mod http;
pub mod hud;
pub mod model;
pub mod ui;
