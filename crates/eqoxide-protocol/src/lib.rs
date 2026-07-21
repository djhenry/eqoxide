//! `eqoxide-protocol` — the RoF2 wire-format decode layer (#544 Step 2j).
//!
//! This crate is a low-level dependency sitting directly above `eqoxide-core`. It bundles the two
//! modules that turn raw RoF2 packet bytes into the typed structs an AI agent reads:
//!
//! * [`wire`] — the [`wire::WireReader`] cursor: the single, agent-honesty-preserving byte reader
//!   (a required read past the end PANICS loudly rather than decoding garbage).
//! * [`protocol`] — every packet decoder / struct / constant (`decode_position_update`,
//!   `parse_rof2_spawn`, `SpawnInfo`, `opcode_name`, `summarize`, and the chat/combat/group/guild/
//!   inventory/spells/trade/world submodule parsers).
//!
//! Extracting this layer unblocks `packet_telemetry` and `http/observe`, both of which need the
//! decoders without dragging in the whole app crate. The app crate re-exports both modules from
//! `eqoxide::eq_net` (`pub use eqoxide_protocol::{protocol, wire};`), so every existing
//! `crate::eq_net::wire::…` / `crate::eq_net::protocol::…` path keeps resolving unchanged.

pub mod wire;
pub mod protocol;
