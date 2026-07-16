//! Navigation domain, extracted out of `eq_net` (cleanup step 2 — nav must not live inside net).
//!
//! `steering` holds the pure, net-independent walker math (pursuit carrots, replan/arrival
//! decisions, the fast-steering cursor). It takes positions/paths and depends only on `assets`
//! types — no `EqStream`, no packets. `planner` holds the pathfinding worker threads (cleanup
//! step 3). The `ActionLoop` god-struct and its `tick()`/`sync_*`/`apply_*plan` methods (the net
//! action loop) still live in `eq_net::action_loop`; moving those out is a later step.

pub mod collision;
pub mod planner;
pub mod steering;
