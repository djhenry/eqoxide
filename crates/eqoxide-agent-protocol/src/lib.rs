//! Wire protocol for the eqoxide Agent Plugin API (docs/specs/2026-09-15-agent-plugin-api-design.md).
//!
//! This crate has NO dependency on any other eqoxide crate — it is what an external agent project
//! pins directly (spec §4). Everything here is plain serde data; no I/O, no eqoxide internals.

pub mod framing;
