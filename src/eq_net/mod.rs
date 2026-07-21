//! EQ network client — protocol, transport, login flow, and gameplay loop.

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
// so every existing `crate::eq_net::wire::…` / `crate::eq_net::protocol::…` path keeps resolving
// unchanged.
pub use eqoxide_protocol::{protocol, wire};

pub use login::run_login_flow;
pub use transport::AppPacket;
