//! Navigation domain, extracted out of `eq_net` (cleanup step 2 — nav must not live inside net).
//!
//! `steering` holds the pure, net-independent walker math (pursuit carrots, replan/arrival
//! decisions, the fast-steering cursor). It takes positions/paths and depends only on `assets`
//! types — no `EqStream`, no packets. `planner` holds the pathfinding worker threads (cleanup
//! step 3). `walker` holds the path-walker state machine itself (`Walker`) — the `/goto` route,
//! stall/backoff/oscillation recovery, and arrival, driven by `eq_net::action_loop::ActionLoop`
//! once per tick (M1 walker extraction, #eq-dev-process). `Walker` moves the player ONLY by
//! writing a `MoveIntent` into the shared `nav_intent` slot — the SAME slot native WASD input
//! writes — never the position or controller directly (see `walker`'s module doc for the full
//! intent-only boundary). The `ActionLoop` god-struct and its remaining `tick()`/`sync_*`/net-I/O
//! methods still live in `eq_net::action_loop`.

pub mod collision;
pub mod planner;
pub mod steering;
pub mod walker;
