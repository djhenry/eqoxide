//! Packet telemetry — the DEFAULT-OFF, low/zero-overhead capture-and-analysis rig for app-layer
//! packets (#525).
//!
//! Extracted into the `eqoxide-telemetry` crate (#544 Step 2k), sitting directly above
//! `eqoxide-protocol` (core ← protocol ← telemetry). Re-exported here so every existing
//! `crate::eq_net::packet_telemetry::…` path (`src/main.rs`, `src/eq_net/transport.rs`,
//! `src/http/observe.rs`) keeps resolving unchanged. See `crates/eqoxide-telemetry/src/lib.rs`
//! for the full module docs (zero-cost-when-off design, reliable-sequence-gap semantics).
pub use eqoxide_telemetry::*;
