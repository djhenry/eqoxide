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

// Crash/shutdown observability (#380: signal handlers, `exit`, `log_instance`, crash-dump logging)
// now lives in the `eqoxide-crash` workspace crate (#544 Step 2i), depending only on external
// crates (libc/dirs/tracing) — never on any workspace crate. Alias it as this crate's `crash`
// module so every existing `crate::crash::…` / `eqoxide::crash::…` call site (main.rs, app.rs,
// http/lifecycle.rs, http/mod.rs) keeps resolving unchanged. The subprocess-level tests
// (`tests/crash_signals.rs`, the `crash_probe` binary) moved WITH the module into that crate's own
// `tests/`/`src/bin/`, since they exercise only crash-module internals and need no app-crate code.
pub use eqoxide_crash as crash;

// The GPU/render core (the View's rendering layer — wgpu device/pipelines/passes, vertex+uniform
// structs, zone/character/billboard draw code, model+animation building, view/projection camera
// math, and the /frame PNG encoder) now lives in the `eqoxide-renderer` workspace crate (#544 Step
// 2n). It is a clean LOWER layer with ZERO up-refs into this app loop; it depends only on
// `eqoxide-core`/`eqoxide-assets` + the GPU/math externals (wgpu/glam/bytemuck/gltf/image) — never
// on eq_net/http/command/nav/ipc/app/movement/ui. Re-export its modules as this crate's own so every
// existing `crate::renderer::…` / `crate::scene::…` / `crate::gpu::…` etc. call site (app.rs, ui/*,
// hud.rs, model.rs, main.rs, the render_model bin) keeps resolving unchanged. The render-side WGSL
// validation test moved WITH the shaders into that crate's `tests/fog_shader.rs`.
pub use eqoxide_renderer::{
    anim, billboard, camera, frame_capture, gpu, head, models, pass, pipeline, renderer, scene,
};

pub mod app;
pub mod asset_sync;
pub mod debug_zone;
pub mod logging;
pub mod movement;
pub mod profiling;

// The agent-facing HTTP/REST API now lives in the `eqoxide-http` workspace crate (#544 Step 2l),
// depending only on the lower structural crates (`eqoxide-core`/`ipc`/`command`/`nav`/`telemetry`/
// `crash`/`protocol`) + axum/tokio/serde — never on this app crate, the renderer, gpu, or the
// eq_net transport. Alias it as this crate's `http` module so every existing `crate::http::…` /
// `eqoxide::http::…` call site (`main.rs`'s `http::spawn_camera_server`) keeps resolving unchanged.
// The two packet-apply gameplay fns its `/observe/debug` tests exercised (`eq_net::packet_handler::
// apply_consider`/`apply_death`) stay in THIS crate; those three tests moved up into
// `tests/http_observe_apply.rs`, where both `eqoxide_http` and `eq_net::packet_handler` are in scope
// (the app crate enables `eqoxide-http/test-fixtures` in its dev-deps for the shared `HttpState`
// builder). See the crate's module docs.
pub use eqoxide_http as http;

// The EQ network client / MVC Model (the net thread that decodes the RoF2 wire and is the SOLE
// writer of the shared GameState — login flow, zone gameplay loop, packet apply, server
// reconciliation, the nav/action thread draining the request slots) now lives in the `eqoxide-net`
// workspace crate (#544 Step 2m), depending only on the lower structural crates
// (`eqoxide-core`/`ipc`/`command`/`nav`/`protocol`/`telemetry`) + externals (tokio/des/…) — never on
// this app crate, the renderer, gpu, or ui. Its one app-side type, `MoveIntent`, it references from
// `eqoxide-ipc` directly. Alias it as this crate's `eq_net` module so every existing
// `crate::eq_net::…` / `eqoxide::eq_net::…` call site (main.rs spawns the net thread; app.rs, ui/*,
// model.rs, and `tests/http_observe_apply.rs`'s `eq_net::packet_handler` apply-fn calls) keeps
// resolving unchanged.
pub use eqoxide_net as eq_net;

// Modules only needed by the full client binary. `camera_state` (orbit/follow input state, driven by
// mouse/scroll in the app loop) and `profiling` (frame-timing instrumentation) stay in this app crate:
// each depends only on `eqoxide-ipc` (its inter-thread contract types) + std, the render core does not
// reference either, and moving them would push an ipc dep onto the renderer that its GPU code never uses.
pub mod camera_state;
pub mod hud;
pub mod model;

// The egui window system (registry/chrome/persist + per-window bodies, issue #162) now lives in the
// `eqoxide-ui` workspace crate (#544 Step 2o), depending only on `eqoxide-core`/`ipc`/`command`/
// `renderer` + egui — never on this app crate, eq_net, or http. Alias it as this crate's `ui` module
// so every existing `crate::ui::…` / `eqoxide::ui::…` call site (app.rs, main.rs, hud.rs) keeps
// resolving unchanged.
pub use eqoxide_ui as ui;
