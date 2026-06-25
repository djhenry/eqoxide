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

pub mod anim;
pub mod app;
pub mod assets;
pub mod water_map;
pub mod asset_sync;
pub mod billboard;
pub mod camera;
pub mod debug_zone;
pub mod gpu;
pub mod logging;
pub mod models;
pub mod profiling;
pub mod pass;
pub mod pipeline;
pub mod renderer;
pub mod scene;

// Modules only needed by the full client binary.
pub mod camera_state;
pub mod config;
pub mod eq_net;
pub mod eqstr;
pub mod frame_capture;
pub mod game_state;
pub mod http;
pub mod hud;
pub mod quests;
pub mod spells;
pub mod ui_layout;
pub mod zone_map;
