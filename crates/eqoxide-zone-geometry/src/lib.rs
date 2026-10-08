//! Zone geometry construction and query logic shared between eqoxide's own client and any
//! external agent-harness project (docs/specs/2026-09-21-agent-harness-separation-design.md §8).
//!
//! This crate holds only geometry-grid construction/query and the readiness state machine that
//! gates it — real-time collision *resolution* stays in eqoxide's own `src/movement.rs`, and
//! A*-search-specific planning/execution stays in `eqoxide-nav`, to be relocated to the harness
//! project by a later plan.

pub mod body;
pub mod climb;
pub mod collision;
pub mod diagnostics;
pub mod water_grid;
pub mod zone_assets;
