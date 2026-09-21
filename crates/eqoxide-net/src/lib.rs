//! EQ network client — protocol, transport, login flow, and gameplay loop.
//!
//! Extracted into the `eqoxide-net` workspace crate (#544 Step 2m). This crate is the MVC **Model**:
//! `packet_handler` decodes + APPLIES the RoF2 wire into `GameState` (every position, spawn, HP,
//! zone, inventory the agent sees), and `action_loop` drives the net loop + server reconciliation —
//! it is the sole authoritative writer of the shared world state. It depends only on the lower
//! structural crates (`eqoxide-core`/`ipc`/`command`/`nav`/`protocol`/`telemetry`) + externals
//! (tokio/des/cbc/byteorder/miniz_oxide/rand/…) — never on the app crate, renderer, gpu, or UI.
//! The single app-side type it needs, `MoveIntent`, it references from `eqoxide-ipc` directly.
//!
//! The app crate re-exports this crate as `eq_net` (`pub use eqoxide_net as eq_net;`) so every
//! existing `crate::eq_net::…` / `eqoxide::eq_net::…` path (main.rs spawns the net thread; app.rs,
//! ui/*, model.rs read published state) keeps resolving unchanged.

pub mod gameplay;
pub mod item;
pub mod login;
pub mod action_loop;
pub mod packet_handler;
pub mod packet_telemetry;
pub mod transport;
pub mod ucs;

// `wire` (the `WireReader` cursor) and `protocol` (every RoF2 packet decoder/struct/const) were
// extracted into the `eqoxide-protocol` crate (#544 Step 2j) so `packet_telemetry` and
// `http/observe` can reach the decoders without dragging in the whole app crate. Re-exported here
// so every existing `crate::wire::…` / `crate::protocol::…` path keeps resolving
// unchanged.
pub use eqoxide_protocol::{protocol, wire};

pub use login::run_login_flow;
pub use transport::AppPacket;
