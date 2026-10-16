//! Zone geometry construction and query logic shared between eqoxide's own client and any
//! external agent-harness project (docs/specs/2026-09-21-agent-harness-separation-design.md §8).
//!
//! This crate holds only geometry-grid construction/query and the readiness state machine that
//! gates it. Real-time collision *resolution* lives in eqoxide's own `src/movement.rs`, and route
//! planning lives in the agent harness.

pub mod body;
pub mod climb;
pub mod collision;
pub mod diagnostics;
pub mod water_grid;
pub mod zone_assets;
