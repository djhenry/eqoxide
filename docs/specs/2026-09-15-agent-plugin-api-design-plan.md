# Agent Plugin API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a persistent, low-latency, bidirectional Unix-socket protocol (`eqoxide-agent-protocol`), client-side visibility filtering (`eqoxide-agent-vision-filter`), and the in-client plumbing that serves both (`eqoxide-agent-plugin-host`), so an external agent process can drive an eqoxide character at per-tick cadence.

**Architecture:** Three new crates layered `eqoxide-agent-protocol` (pure wire types, zero eqoxide-internal deps) ← `eqoxide-agent-vision-filter` (distance + line-of-sight filtering, depends on `eqoxide-core`/`eqoxide-nav`) ← `eqoxide-agent-plugin-host` (owns the socket server, depends on both plus `eqoxide-ipc`/`eqoxide-command`). NDJSON (newline-delimited JSON via `serde_json`) framing over a Unix domain socket. The plugin host pushes an `Observation` every ~150ms tick unconditionally and latches the most-recently-received `Step` into the existing `CameraSlots`/`CommandState` mailboxes — the same plumbing HTTP already writes into, so no new write path is invented, only a new writer.

**Tech Stack:** Rust, `serde`/`serde_json` (NDJSON framing, no new serialization crate), `tokio` (async socket I/O, already a workspace dependency), `std::os::unix::net::UnixStream` (blocking, for the verification example client only).

**Spec:** `docs/specs/2026-09-15-agent-plugin-api-design.md`

## Global Constraints

- Crate layering is one-directional: `eqoxide-agent-protocol` has **zero** dependency on any other eqoxide crate (only `serde`, `serde_json`). `eqoxide-agent-vision-filter` depends on `eqoxide-core` + `eqoxide-nav` + `eqoxide-agent-protocol` only — **never** `eqoxide-ipc` directly (it's pulled in transitively through `eqoxide-nav`, which is fine; the vision-filter's own code must never write `eqoxide_ipc::` paths). `eqoxide-agent-plugin-host` may depend on all of the above plus `eqoxide-ipc`, `eqoxide-command`, `tokio`, `tracing`. None of the three new crates may depend on `eqoxide-renderer`, `eqoxide-net`, `eqoxide-ui`, or the top-level `eqoxide` app crate (spec §4, §6).
- Navigation (A*/Goto/Follow) is **permanently excluded** for agent-driven sessions — not a runtime toggle. This is enforced structurally: `AgentVerb` has no Goto/Follow/wander variant at all, so the plugin host's verb dispatcher has no code path that could ever call `CommandState::request_goto`/`request_follow` (spec §7, §12).
- **GitHub issue [#1127](https://github.com/djhenry/eqoxide/issues/1127) has landed** (`bfd7d653`/#1128, on `main` as of this plan's rebase onto it): player endurance (`GameState.cur_endurance`/`max_endurance`/`endurance_pct`/`endurance_confirmed`), the full general buff list (`GameState.buffs: BTreeMap<u32, BuffSlot>`, `BuffSlot { spell_id: u32, duration_ticks: i32 }`), and per-spell mana cost/cast time/recast delay (`SpellInfo.mana_cost: i32`/`cast_time_ms: u32`/`recast_time_ms: u32`) are all now real, tracked data. The Observation-space types below (Task 7) include them — this plan no longer defers any field named in spec §9.
- Movement is driven via the existing `ManualMove` primitive (`crates/eqoxide-ipc/src/lib.rs:1715`), **not** `MoveIntent` — despite the spec's §7 wording naming `MoveIntent`. Research for this plan (see Task 10) confirmed `MoveIntent` is `src/app.rs`'s internal per-frame *output*, constructed locally from whichever input source is active (WASD, manual, or nav); no external caller — including today's HTTP `/v1/move/manual` — ever constructs one directly. `ManualMove` is the real external write-surface (`CameraSlots::request_manual_move`), and `want_swim`/`want_climb`/`speed`/`hop` (fields `ManualMove` doesn't carry) are auto-derived from environment state (`in_water`, `on_climbable`) by `src/app.rs` regardless of input source — verified at `src/app.rs:1979-1994`. Extending `ManualMove` with `wish_heading` is therefore the correct, spec-faithful mechanism.
- Every new public type that crosses the wire (`eqoxide-agent-protocol`) must round-trip through `serde_json` — this is the crate's core testing obligation (spec §13).
- The plugin host is "thin, deliberately dumb" (spec §4): it contains no policy/decision logic. Reserved-but-unwired `AgentVerb` variants (`Move(ZoneCross)`, and the four uninhabited families) are accepted at the wire level without error and simply produce no action — this is a deliberate no-op, not a placeholder (spec §8, §12).

---

## File Structure

**Create:**
- `crates/eqoxide-agent-protocol/Cargo.toml`
- `crates/eqoxide-agent-protocol/src/lib.rs` — module wiring + re-exports
- `crates/eqoxide-agent-protocol/src/framing.rs` — NDJSON encode/decode helpers
- `crates/eqoxide-agent-protocol/src/handshake.rs` — `PROTOCOL_VERSION`, `Hello`, `HandshakeReply`
- `crates/eqoxide-agent-protocol/src/movement.rs` — `AgentMovement`
- `crates/eqoxide-agent-protocol/src/verb.rs` — `AgentVerb` and every sub-enum
- `crates/eqoxide-agent-protocol/src/step.rs` — `Step`
- `crates/eqoxide-agent-protocol/src/observation.rs` — `CastingView`, `OwnState`, `VisibleEntity`, `AbilityFeature`, `LegalActionMask`, `Observation`
- `crates/eqoxide-agent-protocol/examples/fixed_sequence_client.rs` — live-verification client
- `crates/eqoxide-agent-vision-filter/Cargo.toml`
- `crates/eqoxide-agent-vision-filter/src/lib.rs` — `VISIBILITY_DIST`, `visible_entities()`
- `crates/eqoxide-agent-plugin-host/Cargo.toml`
- `crates/eqoxide-agent-plugin-host/src/lib.rs` — `spawn_agent_plugin_host()`, socket accept loop
- `crates/eqoxide-agent-plugin-host/src/session.rs` — per-connection handshake + tick loop + verb dispatch
- `crates/eqoxide-agent-plugin-host/src/observation_builder.rs` — `build_own_state()`, `build_observation()`
- `crates/eqoxide-agent-plugin-host/src/legal_actions.rs` — `build_legal_actions()`

**Modify:**
- `Cargo.toml` (workspace root) — add the three crates to `[workspace] members` and to the root binary's `[dependencies]`
- `crates/eqoxide-ipc/src/lib.rs:1715-1721` — add `wish_heading: Option<f32>` to `ManualMove`
- `crates/eqoxide-http/src/move_api.rs:88-91,102-105` — the two existing `ManualMove { .. }` construction sites need `wish_heading: None` added (compile-breaking otherwise)
- `src/movement.rs` — add `resolve_heading()` pure helper + test, near `manual_wish` (line 157)
- `src/app.rs:1963-1996` — the manual-move branch: use `resolve_heading(m.wish_heading, derived)` instead of the derived heading alone
- `src/main.rs` — add `--agent-socket` CLI flag, clone `net_thread_dead` before it's moved into `spawn_camera_server` (line 601), add the `spawn_agent_plugin_host(...)` call after the existing `http::spawn_camera_server(...)` call (after line 621)

---

## Task 1: `eqoxide-agent-protocol` scaffold + NDJSON framing

**Files:**
- Create: `crates/eqoxide-agent-protocol/Cargo.toml`
- Create: `crates/eqoxide-agent-protocol/src/lib.rs`
- Create: `crates/eqoxide-agent-protocol/src/framing.rs`
- Modify: `Cargo.toml` (workspace root)

**Interfaces:**
- Produces: `pub fn encode_line<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error>`, `pub fn decode_line<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, serde_json::Error>` — used by every later task that serializes a wire type, and by `eqoxide-agent-plugin-host`'s session loop (Tasks 15-17).

- [ ] **Step 1: Write the failing test**

Create `crates/eqoxide-agent-protocol/src/framing.rs`:

```rust
//! Newline-delimited JSON (NDJSON) framing over the agent socket. No I/O here — pure
//! encode/decode so the framing logic is testable without a live connection.

/// Serialize `value` to one JSON line, terminated by `\n`. The trailing newline is what lets the
/// reader split frames with a plain line reader instead of a length-prefixed protocol.
pub fn encode_line<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    Ok(s)
}

/// Deserialize one JSON line. `line` may or may not carry the trailing `\n` (tokio's `lines()`
/// strips it; callers reading raw bytes may not have), so both are accepted.
pub fn decode_line<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end_matches('\n'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Sample {
        a: u32,
        b: String,
    }

    #[test]
    fn encode_line_appends_exactly_one_trailing_newline() {
        let line = encode_line(&Sample { a: 1, b: "x".into() }).unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn decode_line_round_trips_with_and_without_trailing_newline() {
        let original = Sample { a: 42, b: "hello".into() };
        let encoded = encode_line(&original).unwrap();
        let decoded: Sample = decode_line(&encoded).unwrap();
        assert_eq!(decoded, original);
        let trimmed: Sample = decode_line(encoded.trim_end_matches('\n')).unwrap();
        assert_eq!(trimmed, original);
    }
}
```

Create `crates/eqoxide-agent-protocol/src/lib.rs`:

```rust
//! Wire protocol for the eqoxide Agent Plugin API (docs/specs/2026-09-15-agent-plugin-api-design.md).
//!
//! This crate has NO dependency on any other eqoxide crate — it is what an external agent project
//! pins directly (spec §4). Everything here is plain serde data; no I/O, no eqoxide internals.

pub mod framing;
```

Create `crates/eqoxide-agent-protocol/Cargo.toml`:

```toml
[package]
name = "eqoxide-agent-protocol"
version = "0.1.0"
edition = "2021"

[lib]
name = "eqoxide_agent_protocol"
path = "src/lib.rs"

[dependencies]
# Zero eqoxide-internal dependencies, deliberately (spec §4) — this is what an external agent
# project pins on its own, without touching eqoxide's codebase.
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-protocol` (from the workspace root)
Expected: FAIL — the crate isn't a workspace member yet, so cargo reports `error: package ID specification 'eqoxide-agent-protocol' did not match any packages`.

- [ ] **Step 3: Add the crate to the workspace**

In `Cargo.toml` (workspace root), change line 95's `members` list to include the new crate:

```toml
members = ["tools", "crates/eqoxide-core", "crates/eqoxide-ipc", "crates/eqoxide-assets", "crates/eqoxide-nav", "crates/eqoxide-command", "crates/eqoxide-crash", "crates/eqoxide-protocol", "crates/eqoxide-telemetry", "crates/eqoxide-http", "crates/eqoxide-net", "crates/eqoxide-renderer", "crates/eqoxide-ui", "crates/eqoxide-agent-protocol", "crates/eqoxide-agent-vision-filter", "crates/eqoxide-agent-plugin-host"]
```

(`eqoxide-agent-vision-filter` and `eqoxide-agent-plugin-host` don't exist on disk yet — that's fine, they're created in Tasks 8 and 11. Adding all three now avoids a second workspace-member edit later.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-protocol`
Expected: PASS — both `framing` tests green.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/eqoxide-agent-protocol
git commit -m "feat: scaffold eqoxide-agent-protocol crate with NDJSON framing"
```

---

## Task 2: Handshake types

**Files:**
- Create: `crates/eqoxide-agent-protocol/src/handshake.rs`
- Modify: `crates/eqoxide-agent-protocol/src/lib.rs`

**Interfaces:**
- Consumes: `framing::{encode_line, decode_line}` (Task 1).
- Produces: `pub const PROTOCOL_VERSION: u32`, `pub struct Hello { pub protocol_version: u32 }`, `pub enum HandshakeReply { Accepted, Rejected { server_protocol_version: u32, message: String } }` — consumed by `eqoxide-agent-plugin-host`'s session handshake (Task 15) and the example client (Task 19).

- [ ] **Step 1: Write the failing test**

Create `crates/eqoxide-agent-protocol/src/handshake.rs`:

```rust
//! Connect-time version handshake (spec §5, §11): the client sends `Hello` first, the server
//! replies with `HandshakeReply` and closes the connection on `Rejected` — never a silent
//! misparse downstream.

use serde::{Deserialize, Serialize};

/// Bumped whenever a wire type in this crate changes shape in a way that breaks an old client.
pub const PROTOCOL_VERSION: u32 = 1;

/// The client's first line on every connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
}

/// The server's second line. `Rejected` is followed by the server closing the connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum HandshakeReply {
    Accepted,
    Rejected { server_protocol_version: u32, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{decode_line, encode_line};

    #[test]
    fn hello_round_trips() {
        let h = Hello { protocol_version: PROTOCOL_VERSION };
        let line = encode_line(&h).unwrap();
        let back: Hello = decode_line(&line).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn handshake_reply_accepted_round_trips() {
        let r = HandshakeReply::Accepted;
        let line = encode_line(&r).unwrap();
        let back: HandshakeReply = decode_line(&line).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn handshake_reply_rejected_round_trips_with_fields() {
        let r = HandshakeReply::Rejected {
            server_protocol_version: PROTOCOL_VERSION,
            message: "client protocol_version 0 != server 1".into(),
        };
        let line = encode_line(&r).unwrap();
        let back: HandshakeReply = decode_line(&line).unwrap();
        assert_eq!(back, r);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-protocol handshake`
Expected: FAIL — `crate::framing` compiles, but `mod handshake;` isn't wired into `lib.rs` yet, so `cargo test` reports the test targets don't exist / unresolved module.

- [ ] **Step 3: Wire the module**

In `crates/eqoxide-agent-protocol/src/lib.rs`, add:

```rust
pub mod handshake;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-protocol handshake`
Expected: PASS — 3 tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-protocol/src/handshake.rs crates/eqoxide-agent-protocol/src/lib.rs
git commit -m "feat: add agent-protocol handshake types (Hello, HandshakeReply)"
```

---

## Task 3: `AgentMovement` and a movement-only `Step`

**Files:**
- Create: `crates/eqoxide-agent-protocol/src/movement.rs`
- Create: `crates/eqoxide-agent-protocol/src/step.rs`
- Modify: `crates/eqoxide-agent-protocol/src/lib.rs`

**Interfaces:**
- Produces: `pub struct AgentMovement { pub dir: [f32; 2], pub up: f32, pub jump: bool, pub wish_heading: Option<f32> }`, `pub struct Step { pub movement: Option<AgentMovement>, pub verb: Option<AgentVerb> }` (the `verb` field is added in Task 4 — this task leaves it as `Option<()>` is wrong; instead this task defines `Step` with only `movement`, and Task 4 adds `verb` once `AgentVerb` exists — see Step 3 note below on why `Step` and `verb.rs` are sequenced this way).

- [ ] **Step 1: Write the failing test**

Create `crates/eqoxide-agent-protocol/src/movement.rs`:

```rust
//! Continuous per-tick movement payload (spec §6, §7): the agent's raw directional/heading control,
//! mirroring `eqoxide_ipc::ManualMove`'s fields plus the new `wish_heading`. eqoxide latches this and
//! keeps applying it every tick until a newer one arrives — "held key" semantics, not a discrete verb.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AgentMovement {
    /// World `(east, north)` direction. Any magnitude; the plugin host normalizes it. Zero = stand
    /// in place (e.g. a jump with no movement) — same convention as `ManualMove::dir`.
    pub dir: [f32; 2],
    /// Vertical axis, `-1..1`. Only has an effect while swimming or on a climbable (auto-detected
    /// from the character's position, not something the agent selects) — same as `ManualMove::up`.
    pub up: f32,
    pub jump: bool,
    /// Independently-settable heading (spec §6): melee requires facing the target server-side, so
    /// this can be set even while strafing sideways or standing still. `None` falls back to the
    /// direction-derived heading (facing the way you walk), matching the pre-existing WASD/manual
    /// behavior when the agent doesn't care.
    pub wish_heading: Option<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{decode_line, encode_line};

    #[test]
    fn agent_movement_round_trips_with_heading() {
        let m = AgentMovement { dir: [1.0, 0.0], up: 0.0, jump: false, wish_heading: Some(90.0) };
        let line = encode_line(&m).unwrap();
        let back: AgentMovement = decode_line(&line).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn agent_movement_round_trips_without_heading() {
        let m = AgentMovement { dir: [0.0, 0.0], up: 1.0, jump: true, wish_heading: None };
        let line = encode_line(&m).unwrap();
        let back: AgentMovement = decode_line(&line).unwrap();
        assert_eq!(back, m);
    }
}
```

Create `crates/eqoxide-agent-protocol/src/step.rs`:

```rust
//! `Step` — one frame of agent input (spec §7). Movement is continuous and always present or
//! absent as a whole; the discrete verb (Task 4/5/6) layers on top independently.

use crate::movement::AgentMovement;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub movement: Option<AgentMovement>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{decode_line, encode_line};

    #[test]
    fn step_with_movement_round_trips() {
        let s = Step {
            movement: Some(AgentMovement { dir: [1.0, 0.0], up: 0.0, jump: false, wish_heading: None }),
        };
        let line = encode_line(&s).unwrap();
        let back: Step = decode_line(&line).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn step_with_no_movement_round_trips() {
        let s = Step { movement: None };
        let line = encode_line(&s).unwrap();
        let back: Step = decode_line(&line).unwrap();
        assert_eq!(back, s);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-protocol movement:: step::`
Expected: FAIL — neither module is wired into `lib.rs`.

- [ ] **Step 3: Wire the modules**

In `crates/eqoxide-agent-protocol/src/lib.rs`, add:

```rust
pub mod movement;
pub mod step;
```

Note: `Step` gains its `verb: Option<AgentVerb>` field in Task 4, once `AgentVerb` exists — adding a field to a struct defined in this task is a normal, expected edit in the next task, not a placeholder here (this task's `Step` is real and complete for what it can express so far: movement alone).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-protocol movement:: step::`
Expected: PASS — 4 tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-protocol/src/movement.rs crates/eqoxide-agent-protocol/src/step.rs crates/eqoxide-agent-protocol/src/lib.rs
git commit -m "feat: add AgentMovement and movement-only Step"
```

---

## Task 4: `AgentVerb::Combat` and `CastRequest`

**Files:**
- Create: `crates/eqoxide-agent-protocol/src/verb.rs`
- Modify: `crates/eqoxide-agent-protocol/src/step.rs` (add `verb` field)
- Modify: `crates/eqoxide-agent-protocol/src/lib.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub struct CastRequest { pub gem: u8, pub target_id: Option<u32>, pub item_slot: Option<u32> }` (own type — mirrors `eqoxide_ipc::CastRequest` at `crates/eqoxide-ipc/src/lib.rs:2514-2520` field-for-field, but is NOT a re-export, since this crate has zero eqoxide dependency), `pub enum CombatVerb { Target { spawn_id: u32 }, Attack { on: bool }, Consider { spawn_id: u32 }, Cast(CastRequest) }`, `pub enum AgentVerb { Combat(CombatVerb) }` (more variants added in Tasks 5-6), `Step.verb: Option<AgentVerb>` — the dispatcher built in Task 14 matches on every `CombatVerb` variant by exact name.

- [ ] **Step 1: Write the failing test**

Create `crates/eqoxide-agent-protocol/src/verb.rs`:

```rust
//! `AgentVerb` — the discrete action layered on top of the continuous movement payload every `Step`
//! carries (spec §7, §8). Mirrors the HTTP API's `/v1/<group>/<action>` taxonomy so wiring a new
//! verb later is additive.

use serde::{Deserialize, Serialize};

/// Mirrors `eqoxide_ipc::CastRequest` (`crates/eqoxide-ipc/src/lib.rs:2514`) field-for-field. A
/// separate type, not a re-export — this crate has zero dependency on eqoxide-ipc (spec §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CastRequest {
    pub gem: u8,
    pub target_id: Option<u32>,
    pub item_slot: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", content = "data")]
pub enum CombatVerb {
    Target { spawn_id: u32 },
    Attack { on: bool },
    Consider { spawn_id: u32 },
    Cast(CastRequest),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", content = "data")]
pub enum AgentVerb {
    Combat(CombatVerb),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{decode_line, encode_line};

    fn round_trips(v: AgentVerb) {
        let line = encode_line(&v).unwrap();
        let back: AgentVerb = decode_line(&line).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn combat_target_round_trips() {
        round_trips(AgentVerb::Combat(CombatVerb::Target { spawn_id: 42 }));
    }

    #[test]
    fn combat_attack_round_trips() {
        round_trips(AgentVerb::Combat(CombatVerb::Attack { on: true }));
    }

    #[test]
    fn combat_consider_round_trips() {
        round_trips(AgentVerb::Combat(CombatVerb::Consider { spawn_id: 7 }));
    }

    #[test]
    fn combat_cast_round_trips() {
        round_trips(AgentVerb::Combat(CombatVerb::Cast(CastRequest {
            gem: 0,
            target_id: Some(42),
            item_slot: None,
        })));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-protocol verb::`
Expected: FAIL — `mod verb;` isn't wired into `lib.rs` yet.

- [ ] **Step 3: Wire the module and add `verb` to `Step`**

In `crates/eqoxide-agent-protocol/src/lib.rs`, add:

```rust
pub mod verb;
```

In `crates/eqoxide-agent-protocol/src/step.rs`, change:

```rust
use crate::movement::AgentMovement;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub movement: Option<AgentMovement>,
}
```

to:

```rust
use crate::movement::AgentMovement;
use crate::verb::AgentVerb;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub movement: Option<AgentMovement>,
    pub verb: Option<AgentVerb>,
}
```

and update `step.rs`'s two existing tests to add `verb: None`:

```rust
    #[test]
    fn step_with_movement_round_trips() {
        let s = Step {
            movement: Some(AgentMovement { dir: [1.0, 0.0], up: 0.0, jump: false, wish_heading: None }),
            verb: None,
        };
        let line = encode_line(&s).unwrap();
        let back: Step = decode_line(&line).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn step_with_no_movement_round_trips() {
        let s = Step { movement: None, verb: None };
        let line = encode_line(&s).unwrap();
        let back: Step = decode_line(&line).unwrap();
        assert_eq!(back, s);
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-protocol`
Expected: PASS — all tests across the crate green (verb tests plus the updated step tests).

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-protocol/src/verb.rs crates/eqoxide-agent-protocol/src/step.rs crates/eqoxide-agent-protocol/src/lib.rs
git commit -m "feat: add AgentVerb::Combat and wire it into Step"
```

---

## Task 5: `AgentVerb::Interact`, `Lifecycle`, `Move(ZoneCross)`

**Files:**
- Modify: `crates/eqoxide-agent-protocol/src/verb.rs`

**Interfaces:**
- Consumes: `AgentVerb` (Task 4).
- Produces: `pub enum InteractVerb { Sit, Stand }`, `pub enum LifecycleVerb { Respawn }`, `pub enum MoveVerb { ZoneCross }`, and three new `AgentVerb` variants (`Interact`, `Lifecycle`, `Move`) — the dispatcher (Task 14) matches `InteractVerb::Sit`/`Stand` to `CommandState::request_sit(bool)`, `LifecycleVerb::Respawn` to `CommandState::request_respawn()`, and leaves `MoveVerb::ZoneCross` as an accepted, deliberate no-op.

- [ ] **Step 1: Write the failing test**

In `crates/eqoxide-agent-protocol/src/verb.rs`, add below `CombatVerb`:

```rust
/// Both variants translate to the single real `CommandState::request_sit(bool)` — `Sit` → `true`,
/// `Stand` → `false` (spec §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum InteractVerb {
    Sit,
    Stand,
}

/// `Respawn` dispatches to `CommandState::request_respawn()`, which returns `()` — unlike the other
/// `request_*` methods this dispatcher calls, there is no accepted/refused signal to report back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum LifecycleVerb {
    Respawn,
}

/// Reserved for discrete travel actions like zone crossing — not part of the initial combat scope
/// (spec §8, §12). Typed and constructible now; the plugin host accepts it and does nothing (no
/// handler wired yet) rather than rejecting it as malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum MoveVerb {
    ZoneCross,
}
```

and change the `AgentVerb` enum to:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", content = "data")]
pub enum AgentVerb {
    Combat(CombatVerb),
    Interact(InteractVerb),
    Lifecycle(LifecycleVerb),
    Move(MoveVerb),
}
```

and add tests below the existing `Combat*` tests in the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn interact_sit_round_trips() {
        round_trips(AgentVerb::Interact(InteractVerb::Sit));
    }

    #[test]
    fn interact_stand_round_trips() {
        round_trips(AgentVerb::Interact(InteractVerb::Stand));
    }

    #[test]
    fn lifecycle_respawn_round_trips() {
        round_trips(AgentVerb::Lifecycle(LifecycleVerb::Respawn));
    }

    #[test]
    fn move_zone_cross_round_trips() {
        round_trips(AgentVerb::Move(MoveVerb::ZoneCross));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-protocol verb::`
Expected: FAIL — `InteractVerb`/`LifecycleVerb`/`MoveVerb` and the new `AgentVerb` variants don't exist yet (compile error), so the whole test binary fails to build.

- [ ] **Step 3: Implementation is already written above (Step 1) — nothing further to add.**

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-protocol verb::`
Expected: PASS — 8 verb tests green (4 combat + 4 new).

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-protocol/src/verb.rs
git commit -m "feat: add AgentVerb::Interact, Lifecycle, Move(ZoneCross)"
```

---

## Task 6: Reserved uninhabited verb families

**Files:**
- Modify: `crates/eqoxide-agent-protocol/src/verb.rs`

**Interfaces:**
- Consumes: `AgentVerb` (Task 5).
- Produces: `pub enum MerchantVerb {}`, `pub enum InventoryVerb {}`, `pub enum QuestsVerb {}`, `pub enum ChatVerb {}` (uninhabited — structurally unconstructable), plus `AgentVerb::Merchant/Inventory/Quests/Chat` variants wrapping them. The dispatcher (Task 14) matches these arms with an empty/unreachable body, since no value can ever exist to dispatch.

- [ ] **Step 1: Write the failing test**

In `crates/eqoxide-agent-protocol/src/verb.rs`, add below `MoveVerb`:

```rust
/// Reserved, typed, not wired (spec §8, §12). An uninhabited enum — no variant exists to
/// construct — which is a more honest "nothing to see here" than a stub with unread fields: a
/// value of this type can never exist, so there is nothing for a future task to forget to wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MerchantVerb {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InventoryVerb {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuestsVerb {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatVerb {}
```

and change `AgentVerb` to:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", content = "data")]
pub enum AgentVerb {
    Combat(CombatVerb),
    Interact(InteractVerb),
    Lifecycle(LifecycleVerb),
    Move(MoveVerb),
    Merchant(MerchantVerb),
    Inventory(InventoryVerb),
    Quests(QuestsVerb),
    Chat(ChatVerb),
}
```

and add these tests (an uninhabited variant can never be round-tripped, so the test instead asserts that the JSON shape such a value WOULD have fails to deserialize — the honest inverse of a round-trip test):

```rust
    #[test]
    fn merchant_verb_cannot_be_deserialized_from_any_action_tag() {
        // No `MerchantVerb` value can ever exist to serialize, so this asserts the negative: no
        // JSON shape deserializes into one, because the enum has no variants to match against.
        let result: Result<AgentVerb, _> =
            crate::framing::decode_line(r#"{"verb":"Merchant","data":{"action":"Buy"}}"#);
        assert!(result.is_err(), "an uninhabited enum must reject every input: {result:?}");
    }

    #[test]
    fn inventory_verb_cannot_be_deserialized_from_any_action_tag() {
        let result: Result<AgentVerb, _> =
            crate::framing::decode_line(r#"{"verb":"Inventory","data":{"action":"Move"}}"#);
        assert!(result.is_err(), "an uninhabited enum must reject every input: {result:?}");
    }

    #[test]
    fn quests_verb_cannot_be_deserialized_from_any_action_tag() {
        let result: Result<AgentVerb, _> =
            crate::framing::decode_line(r#"{"verb":"Quests","data":{"action":"Accept"}}"#);
        assert!(result.is_err(), "an uninhabited enum must reject every input: {result:?}");
    }

    #[test]
    fn chat_verb_cannot_be_deserialized_from_any_action_tag() {
        let result: Result<AgentVerb, _> =
            crate::framing::decode_line(r#"{"verb":"Chat","data":{"action":"Say"}}"#);
        assert!(result.is_err(), "an uninhabited enum must reject every input: {result:?}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-protocol verb::`
Expected: FAIL — `MerchantVerb`/`InventoryVerb`/`QuestsVerb`/`ChatVerb` and the new `AgentVerb` variants don't exist yet (compile error).

- [ ] **Step 3: Implementation is already written above (Step 1) — nothing further to add.**

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-protocol verb::`
Expected: PASS — 12 verb tests green (8 existing + 4 new rejection tests).

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-protocol/src/verb.rs
git commit -m "feat: add reserved uninhabited AgentVerb families (Merchant/Inventory/Quests/Chat)"
```

---

## Task 7: Observation types

**Files:**
- Create: `crates/eqoxide-agent-protocol/src/observation.rs`
- Modify: `crates/eqoxide-agent-protocol/src/lib.rs`

**Interfaces:**
- Consumes: nothing new (pure data types).
- Produces: `pub struct CastingView { pub spell_id: u32, pub elapsed_ms: u32, pub cast_ms: u32 }`, `pub struct BuffView { pub slot: u32, pub spell_id: u32, pub duration_ticks: i32 }`, `pub struct OwnState { pub pos: [f32; 3], pub heading: f32, pub hp: i32, pub hp_max: i32, pub hp_verified: bool, pub mana: i32, pub mana_max: i32, pub endurance: i32, pub endurance_max: i32, pub endurance_confirmed: bool, pub casting: Option<CastingView>, pub buffs: Vec<BuffView>, pub zone_name: String }`, `pub struct VisibleEntity { pub spawn_id: u32, pub name: String, pub is_npc: bool, pub level: u32, pub race: String, pub pos: [f32; 3], pub heading: f32, pub hp_pct: f32, pub dead: bool }`, `pub struct AbilityFeature { pub gem: u8, pub spell_id: u32, pub target_type: u8, pub effects: Vec<i32>, pub mana_cost: i32, pub cast_time_ms: u32, pub recast_time_ms: u32 }`, `pub struct LegalActionMask { pub gems: [bool; 9], pub abilities: Vec<AbilityFeature> }`, `pub struct Observation { pub own: OwnState, pub visible: Vec<VisibleEntity>, pub legal_actions: LegalActionMask, pub dead: bool, pub terminated: bool, pub truncated: bool }`. Consumed by `eqoxide-agent-vision-filter` (Task 9, `VisibleEntity`) and `eqoxide-agent-plugin-host`'s `observation_builder` (Task 12-13).

**Scope note (updated after rebasing onto #1127):** `OwnState.endurance`/`endurance_max`/`endurance_confirmed` mirror `GameState.cur_endurance`/`max_endurance`/`endurance_confirmed` directly; `OwnState.buffs` mirrors `GameState.buffs: BTreeMap<u32, BuffSlot>`; `AbilityFeature.mana_cost`/`cast_time_ms`/`recast_time_ms` mirror the three new `SpellInfo` fields — all landed via #1127 (`bfd7d653`/#1128). `OwnState.hp_verified` is added alongside them for the same reason, even though it predates #1127: `GameState` already exposes `hp_verified()` under the identical "false until the server has actually confirmed it" contract as `endurance_confirmed`, and exposing `hp`/`hp_max` without it while exposing `endurance_confirmed` beside `endurance`/`endurance_max` would be an inconsistent honesty contract within the same struct. `AbilityFeature.effects` and `.target_type` remain raw SPA ids / raw EQ `u8` codes rather than a hand-curated enum, mirroring `eqoxide_core::spells::SpellInfo` itself (which only names `ST_SELF = 6`, no enum).

- [ ] **Step 1: Write the failing test**

Create `crates/eqoxide-agent-protocol/src/observation.rs`:

```rust
//! Per-tick push from eqoxide to the agent (spec §9). Pushed unconditionally every tick,
//! independent of whether a `Step` arrived (spec §5).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CastingView {
    pub spell_id: u32,
    pub elapsed_ms: u32,
    pub cast_ms: u32,
}

/// Mirrors `eqoxide_core::game_state::BuffSlot` plus its map key (spec §9, #1127). `duration_ticks`
/// stays signed straight through the wire: EQEmu writes `-1000` for a permanent buff, and treating
/// that as unsigned would publish a fabricated-looking ~4.29 billion tick count instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuffView {
    pub slot: u32,
    pub spell_id: u32,
    pub duration_ticks: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OwnState {
    pub pos: [f32; 3],
    pub heading: f32,
    pub hp: i32,
    pub hp_max: i32,
    /// Mirrors `GameState::hp_verified()`: false during the estimate-only window before the
    /// server's first self `OP_HPUpdate`, so the agent never mistakes a guess for a confirmed
    /// reading — same "false, not a confident 0/guess" contract as `endurance_confirmed` below.
    pub hp_verified: bool,
    pub mana: i32,
    pub mana_max: i32,
    /// #1127: mirrors `GameState.cur_endurance`/`max_endurance`.
    pub endurance: i32,
    pub endurance_max: i32,
    /// #1127: mirrors `GameState.endurance_confirmed` — false (not a confident 0) until at least
    /// one `OP_EnduranceUpdate` has been seen.
    pub endurance_confirmed: bool,
    pub casting: Option<CastingView>,
    /// #1127: every occupied buff slot, mirroring `GameState.buffs`. Always present (empty when no
    /// buffs are active), sorted by slot (the source `BTreeMap`'s natural iteration order).
    pub buffs: Vec<BuffView>,
    pub zone_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisibleEntity {
    pub spawn_id: u32,
    pub name: String,
    pub is_npc: bool,
    pub level: u32,
    pub race: String,
    pub pos: [f32; 3],
    pub heading: f32,
    pub hp_pct: f32,
    pub dead: bool,
}

/// Raw SPA effect ids (`SPA_BLANK = 254` slots already filtered out by whoever builds this) and a
/// raw EQ target-type code, rather than a hand-curated enum — matches the precedent set by
/// `eqoxide_core::spells::SpellInfo` itself (which only names `ST_SELF = 6`, no enum).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AbilityFeature {
    pub gem: u8,
    pub spell_id: u32,
    pub target_type: u8,
    pub effects: Vec<i32>,
    /// #1127: mirrors `SpellInfo.mana_cost` — signed; some clicks/procs cost negative mana.
    pub mana_cost: i32,
    /// #1127: mirrors `SpellInfo.cast_time_ms`.
    pub cast_time_ms: u32,
    /// #1127: mirrors `SpellInfo.recast_time_ms`.
    pub recast_time_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegalActionMask {
    pub gems: [bool; 9],
    pub abilities: Vec<AbilityFeature>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub own: OwnState,
    pub visible: Vec<VisibleEntity>,
    pub legal_actions: LegalActionMask,
    /// Character death as ongoing STATE, not a terminal boundary (spec §9) — routine in combat
    /// training; the agent keeps receiving observations while dead and must issue
    /// `Lifecycle::Respawn` to continue.
    pub dead: bool,
    /// True session end — eqoxide can only ever honestly assert this when the net thread has died
    /// for good (see `eqoxide-agent-plugin-host`'s `observation_builder`, Task 13).
    pub terminated: bool,
    /// An artificial, harness-imposed rollout cutoff. eqoxide never imposes one itself in v1 — this
    /// is always `false` from eqoxide's side; an external driver may still truncate its own
    /// rollout without needing this field to say so.
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{decode_line, encode_line};

    fn round_trips(o: &Observation) {
        let line = encode_line(o).unwrap();
        let back: Observation = decode_line(&line).unwrap();
        assert_eq!(&back, o);
    }

    #[test]
    fn observation_with_casting_and_visible_entities_round_trips() {
        let o = Observation {
            own: OwnState {
                pos: [1.0, 2.0, 3.0],
                heading: 45.0,
                hp: 100,
                hp_max: 100,
                hp_verified: true,
                mana: 50,
                mana_max: 100,
                endurance: 80,
                endurance_max: 100,
                endurance_confirmed: true,
                casting: Some(CastingView { spell_id: 12, elapsed_ms: 500, cast_ms: 3000 }),
                buffs: vec![
                    BuffView { slot: 3, spell_id: 90, duration_ticks: -1000 },
                    BuffView { slot: 5, spell_id: 12, duration_ticks: 42 },
                ],
                zone_name: "qeynos".into(),
            },
            visible: vec![VisibleEntity {
                spawn_id: 7,
                name: "a_rat00".into(),
                is_npc: true,
                level: 3,
                race: "Rat".into(),
                pos: [4.0, 5.0, 6.0],
                heading: 0.0,
                hp_pct: 75.0,
                dead: false,
            }],
            legal_actions: LegalActionMask {
                gems: [true, false, false, false, false, false, false, false, false],
                abilities: vec![AbilityFeature {
                    gem: 0,
                    spell_id: 12,
                    target_type: 5,
                    effects: vec![79],
                    mana_cost: 25,
                    cast_time_ms: 3000,
                    recast_time_ms: 0,
                }],
            },
            dead: false,
            terminated: false,
            truncated: false,
        };
        round_trips(&o);
    }

    #[test]
    fn observation_with_no_casting_and_no_visible_entities_round_trips() {
        let o = Observation {
            own: OwnState {
                pos: [0.0, 0.0, 0.0],
                heading: 0.0,
                hp: 0,
                hp_max: 100,
                hp_verified: false,
                mana: 0,
                mana_max: 0,
                endurance: 0,
                endurance_max: 0,
                endurance_confirmed: false,
                casting: None,
                buffs: vec![],
                zone_name: "qeynos".into(),
            },
            visible: vec![],
            legal_actions: LegalActionMask { gems: [false; 9], abilities: vec![] },
            dead: true,
            terminated: false,
            truncated: false,
        };
        round_trips(&o);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-protocol observation::`
Expected: FAIL — `mod observation;` isn't wired into `lib.rs` yet.

- [ ] **Step 3: Wire the module**

In `crates/eqoxide-agent-protocol/src/lib.rs`, add:

```rust
pub mod observation;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-protocol observation::`
Expected: PASS — 2 tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-protocol/src/observation.rs crates/eqoxide-agent-protocol/src/lib.rs
git commit -m "feat: add Observation, OwnState, VisibleEntity, LegalActionMask types"
```

This completes `eqoxide-agent-protocol`'s core types. Run `cargo test -p eqoxide-agent-protocol` once more here to confirm the whole crate (all 7 tasks' tests) is green before moving on.

---

## Task 8: `eqoxide-agent-vision-filter` scaffold + distance cutoff

**Files:**
- Create: `crates/eqoxide-agent-vision-filter/Cargo.toml`
- Create: `crates/eqoxide-agent-vision-filter/src/lib.rs`

**Interfaces:**
- Consumes: `eqoxide_core::game_state::Entity` (fields: `spawn_id: u32, name: String, level: u32, is_npc: bool, x/y/z: f32, hp_pct: f32, race: String, heading: f32, dead: bool` — confirmed at `crates/eqoxide-core/src/game_state.rs:125-155`), `eqoxide_agent_protocol::observation::VisibleEntity` (Task 7).
- Produces: `pub const VISIBILITY_DIST: f32 = 500.0;`, `pub fn visible_entities(entities: &std::collections::HashMap<u32, eqoxide_core::game_state::Entity>, from: [f32; 3], max_dist: f32) -> Vec<eqoxide_agent_protocol::observation::VisibleEntity>` (distance-only in this task; Task 9 adds the LOS/occlusion parameter) — consumed by `eqoxide-agent-plugin-host`'s `observation_builder` (Task 13).

- [ ] **Step 1: Write the failing test**

Create `crates/eqoxide-agent-vision-filter/Cargo.toml`:

```toml
[package]
name = "eqoxide-agent-vision-filter"
version = "0.1.0"
edition = "2021"

[lib]
name = "eqoxide_agent_vision_filter"
path = "src/lib.rs"

[dependencies]
# core: the Entity type this crate filters. nav: SharedCollision + PLAYER_BODY.chest for the
# line-of-sight check (Task 9). agent-protocol: VisibleEntity, the type this crate produces.
# Deliberately NOT eqoxide-ipc directly (spec §4/§6) — it arrives transitively through eqoxide-nav.
# No renderer/wgpu dependency (spec §4): ENTITY_DRAW_DIST is mirrored as VISIBILITY_DIST below,
# not imported.
eqoxide-core = { path = "../eqoxide-core" }
eqoxide-nav = { path = "../eqoxide-nav" }
eqoxide-agent-protocol = { path = "../eqoxide-agent-protocol" }

[dev-dependencies]
# Test-fixture construction only (Task 9's LOS tests): MeshData/ZoneAssets/RenderMode to build a
# synthetic Collision with known wall geometry, mirroring the pattern eqoxide-nav's own tests use
# (crates/eqoxide-nav/src/collision.rs:7359, `slotted_wall`). Never a normal dependency — this
# crate has no renderer/asset-loading need outside tests.
eqoxide-assets = { path = "../eqoxide-assets" }
```

Create `crates/eqoxide-agent-vision-filter/src/lib.rs`:

```rust
//! Client-side visibility filtering (spec §6): distance cutoff plus line-of-sight occlusion (Task
//! 9), computed each tick from the character's actual position — not the full zone's worth of
//! entity updates. No renderer/GPU dependency; `VISIBILITY_DIST` mirrors the renderer's
//! `ENTITY_DRAW_DIST` (`crates/eqoxide-renderer/src/pass.rs:367`, value 500.0) rather than
//! importing it, since this crate must not depend on eqoxide-renderer.

use eqoxide_agent_protocol::observation::VisibleEntity;
use eqoxide_core::game_state::Entity;
use std::collections::HashMap;

pub const VISIBILITY_DIST: f32 = 500.0;

fn to_visible_entity(e: &Entity) -> VisibleEntity {
    VisibleEntity {
        spawn_id: e.spawn_id,
        name: e.name.clone(),
        is_npc: e.is_npc,
        level: e.level,
        race: e.race.clone(),
        pos: [e.x, e.y, e.z],
        heading: e.heading,
        hp_pct: e.hp_pct,
        dead: e.dead,
    }
}

fn within_distance(e: &Entity, from: [f32; 3], max_dist: f32) -> bool {
    let d = [e.x - from[0], e.y - from[1], e.z - from[2]];
    let dist2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
    dist2 <= max_dist * max_dist
}

/// Entities within `max_dist` of `from` — no line-of-sight check yet (Task 9 adds occlusion).
pub fn visible_entities(entities: &HashMap<u32, Entity>, from: [f32; 3], max_dist: f32) -> Vec<VisibleEntity> {
    entities.values().filter(|e| within_distance(e, from, max_dist)).map(to_visible_entity).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity_at(spawn_id: u32, pos: [f32; 3]) -> Entity {
        Entity {
            spawn_id,
            name: format!("entity{spawn_id}"),
            level: 1,
            is_npc: true,
            x: pos[0],
            y: pos[1],
            z: pos[2],
            hp_pct: 100.0,
            cur_hp: 100,
            max_hp: 100,
            race: "Rat".into(),
            heading: 0.0,
            dead: false,
            equipment: [0; 9],
            equipment_tint: [[0; 3]; 9],
            gender: 0,
            helm: 0,
            showhelm: 0,
            face: 0,
            hairstyle: 0,
            haircolor: 0,
            pose: String::new(),
            gait: None,
        }
    }

    #[test]
    fn entity_within_distance_is_included() {
        let mut entities = HashMap::new();
        entities.insert(1, entity_at(1, [10.0, 0.0, 0.0]));
        let result = visible_entities(&entities, [0.0, 0.0, 0.0], VISIBILITY_DIST);
        assert_eq!(result.len(), 1, "an entity 10 units away is well within the 500-unit cutoff");
        assert_eq!(result[0].spawn_id, 1);
    }

    #[test]
    fn entity_beyond_distance_is_excluded() {
        let mut entities = HashMap::new();
        entities.insert(1, entity_at(1, [1000.0, 0.0, 0.0]));
        let result = visible_entities(&entities, [0.0, 0.0, 0.0], VISIBILITY_DIST);
        assert!(result.is_empty(), "an entity 1000 units away must be excluded by the 500-unit cutoff");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-vision-filter` (from the workspace root)
Expected: FAIL — the crate directory exists but isn't yet a real package cargo can resolve until this exact commit lands (the workspace member entry was already added in Task 1, so this is really "not yet compiling" rather than "not found" — run it once with just the `Cargo.toml` + empty stub to confirm it currently fails to build, or skip straight to writing both files together and rely on Step 4 as the first real pass/fail signal). If the two files above are written before this step runs, treat this step as validating pre-image state by temporarily reverting `src/lib.rs` to `pub const VISIBILITY_DIST: f32 = 500.0;` with no `visible_entities` — the test then fails to compile with "cannot find function `visible_entities`".

- [ ] **Step 3: Implementation is already written above (Step 1) — nothing further to add.**

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-vision-filter`
Expected: PASS — 2 tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-vision-filter
git commit -m "feat: scaffold eqoxide-agent-vision-filter with distance-cutoff filtering"
```

---

## Task 9: Line-of-sight occlusion

**Files:**
- Modify: `crates/eqoxide-agent-vision-filter/src/lib.rs`
- Modify: `crates/eqoxide-core/src/game_state.rs` (remove now-stale `#[allow(dead_code)]` on `Entity.is_npc`, line 129 — `is_npc` becomes genuinely read once `to_visible_entity` consumes it)

**Interfaces:**
- Consumes: `eqoxide_nav::collision::SharedCollision` (`Arc<RwLock<Option<Arc<Collision>>>>`), `eqoxide_nav::traversability::PLAYER_BODY.chest` (`f32`, value 4.0), `eqoxide_core::physics::PLAYER_RADIUS`.
- Produces: `visible_entities()`'s signature changes to add a `collision: &eqoxide_nav::collision::SharedCollision` parameter — this is the final signature `eqoxide-agent-plugin-host`'s `observation_builder` (Task 13) calls.

**Design decision:** occlusion calls `Collision::line_clear` directly with the vision-filter's own chest-height ray offset, rather than reusing `eqoxide_nav::collision::carrot_los_clear` (`crates/eqoxide-nav/src/collision.rs:2263`) — that helper's doc comment scopes it entirely to the nav planner's pure-pursuit carrot clamp, a different concern from "can the agent see this entity." Reusing the same *technique* (chest-height ray via `line_clear`, matching `PLAYER_BODY.chest`) with this crate's own rationale keeps vision-filter semantics independent of a nav-internal helper's naming.

- [ ] **Step 1: Write the failing test**

In `crates/eqoxide-agent-vision-filter/src/lib.rs`, change the top-of-file imports and `visible_entities` to:

```rust
use eqoxide_agent_protocol::observation::VisibleEntity;
use eqoxide_core::game_state::Entity;
use eqoxide_nav::collision::SharedCollision;
use std::collections::HashMap;

pub const VISIBILITY_DIST: f32 = 500.0;

fn to_visible_entity(e: &Entity) -> VisibleEntity {
    VisibleEntity {
        spawn_id: e.spawn_id,
        name: e.name.clone(),
        is_npc: e.is_npc,
        level: e.level,
        race: e.race.clone(),
        pos: [e.x, e.y, e.z],
        heading: e.heading,
        hp_pct: e.hp_pct,
        dead: e.dead,
    }
}

fn within_distance(e: &Entity, from: [f32; 3], max_dist: f32) -> bool {
    let d = [e.x - from[0], e.y - from[1], e.z - from[2]];
    let dist2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
    dist2 <= max_dist * max_dist
}

/// A CHEST-HEIGHT ray between the two positions, using the character's own body dimensions — a
/// foot-height ray false-trips on ground undulation; chest height rides above it while a real wall
/// still blocks it (same reasoning `eqoxide_nav::collision::carrot_los_clear` documents for its own,
/// unrelated use case — see this task's design note in the plan). `false` (occluded) when no zone
/// geometry is loaded at all, matching agent-honesty: no visibility claim without real geometry.
fn has_line_of_sight(collision: &SharedCollision, from: [f32; 3], to: [f32; 3]) -> bool {
    let chest = eqoxide_nav::traversability::PLAYER_BODY.chest;
    let radius = eqoxide_core::physics::PLAYER_RADIUS;
    let guard = collision.read().unwrap();
    let Some(col) = guard.as_ref() else { return false };
    let eye = [from[0], from[1], from[2] + chest];
    let target = [to[0], to[1], to[2] + chest];
    col.line_clear(eye, target, radius)
}

/// Entities within `max_dist` of `from` with clear line of sight (spec §6). No facing/FOV cone.
pub fn visible_entities(
    entities: &HashMap<u32, Entity>,
    from: [f32; 3],
    max_dist: f32,
    collision: &SharedCollision,
) -> Vec<VisibleEntity> {
    entities
        .values()
        .filter(|e| within_distance(e, from, max_dist))
        .filter(|e| has_line_of_sight(collision, from, [e.x, e.y, e.z]))
        .map(to_visible_entity)
        .collect()
}
```

Update the existing two tests' calls to pass a collision argument, and add the two new LOS tests, in `crates/eqoxide-agent-vision-filter/src/lib.rs`'s `#[cfg(test)] mod tests`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};
    use eqoxide_nav::collision::Collision;
    use std::sync::{Arc, RwLock};

    fn entity_at(spawn_id: u32, pos: [f32; 3]) -> Entity {
        Entity {
            spawn_id,
            name: format!("entity{spawn_id}"),
            level: 1,
            is_npc: true,
            x: pos[0],
            y: pos[1],
            z: pos[2],
            hp_pct: 100.0,
            cur_hp: 100,
            max_hp: 100,
            race: "Rat".into(),
            heading: 0.0,
            dead: false,
            equipment: [0; 9],
            equipment_tint: [[0; 3]; 9],
            gender: 0,
            helm: 0,
            showhelm: 0,
            face: 0,
            hairstyle: 0,
            haircolor: 0,
            pose: String::new(),
            gait: None,
        }
    }

    fn floor() -> MeshData {
        MeshData {
            positions: vec![[0.0, 0.0, 0.0], [40.0, 0.0, 0.0], [40.0, 0.0, 40.0], [0.0, 0.0, 40.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0; 3],
            render_mode: RenderMode::Opaque,
            anim: None,
        }
    }

    // A full-width wall panel at north = `n` (GLB axes -> world: east = p[2], north = p[0], height
    // = p[1] — same convention as eqoxide-nav's own `slotted_wall` test fixture).
    fn wall_at(n: f32) -> MeshData {
        MeshData {
            positions: vec![[0.0, 0.0, n], [40.0, 0.0, n], [40.0, 10.0, n], [0.0, 10.0, n]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0; 3],
            render_mode: RenderMode::Opaque,
            anim: None,
        }
    }

    fn shared(col: Collision) -> SharedCollision {
        Arc::new(RwLock::new(Some(Arc::new(col))))
    }

    fn open_collision() -> SharedCollision {
        shared(Collision::build(&ZoneAssets { terrain: vec![floor()], objects: vec![], textures: vec![] }, 2.0))
    }

    fn walled_collision() -> SharedCollision {
        shared(Collision::build(
            &ZoneAssets { terrain: vec![floor(), wall_at(20.0)], objects: vec![], textures: vec![] },
            2.0,
        ))
    }

    #[test]
    fn entity_within_distance_and_clear_los_is_included() {
        let mut entities = HashMap::new();
        entities.insert(1, entity_at(1, [10.0, 5.0, 0.0]));
        let result = visible_entities(&entities, [10.0, 0.0, 0.0], VISIBILITY_DIST, &open_collision());
        assert_eq!(result.len(), 1, "an entity 5 units away with no obstruction must be visible");
        assert_eq!(result[0].spawn_id, 1);
    }

    #[test]
    fn entity_beyond_distance_is_excluded() {
        let mut entities = HashMap::new();
        entities.insert(1, entity_at(1, [1000.0, 5.0, 0.0]));
        let result = visible_entities(&entities, [10.0, 0.0, 0.0], VISIBILITY_DIST, &open_collision());
        assert!(result.is_empty(), "an entity 1000 units away must be excluded by the 500-unit cutoff");
    }

    #[test]
    fn entity_behind_a_wall_is_excluded_despite_being_in_range() {
        let mut entities = HashMap::new();
        // Player at north=10, entity at north=30 — the wall at north=20 sits directly between them.
        entities.insert(1, entity_at(1, [10.0, 5.0, 30.0]));
        let result = visible_entities(&entities, [10.0, 5.0, 10.0], VISIBILITY_DIST, &walled_collision());
        assert!(result.is_empty(), "a wall between the two points must occlude the entity");
    }

    #[test]
    fn entity_in_front_of_the_wall_with_clear_los_is_included() {
        let mut entities = HashMap::new();
        // Both player and entity are on the SAME side (north < 20) of the wall at north=20.
        entities.insert(1, entity_at(1, [10.0, 5.0, 15.0]));
        let result = visible_entities(&entities, [10.0, 5.0, 10.0], VISIBILITY_DIST, &walled_collision());
        assert_eq!(result.len(), 1, "nothing obstructs a line that never crosses the wall");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-vision-filter`
Expected: FAIL to compile — the two pre-existing tests still call `visible_entities` with 3 arguments (the old signature); once the source's signature above is in place but before the test module is updated, this is a type-mismatch compile error. (In practice, since both the implementation and the tests are edited together in Step 1, run this BEFORE editing `src/lib.rs` for the true red state — the existing 2 tests fail to compile against a hand-reverted 3-argument `visible_entities` stub if you want to observe a literal red run; otherwise treat the mismatch between old test calls and the new 4-argument signature, if edited in two separate saves, as the fail signal.)

- [ ] **Step 3: Implementation is already written above (Step 1) — nothing further to add.**

Also remove the now-stale attribute in `crates/eqoxide-core/src/game_state.rs`. Change:

```rust
    #[allow(dead_code)]
    pub is_npc: bool,
```

to:

```rust
    pub is_npc: bool,
```

(`is_npc` is genuinely read now, by `to_visible_entity` above — the attribute would otherwise claim something false.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-vision-filter && cargo test -p eqoxide-core`
Expected: PASS — 4 vision-filter tests green; `eqoxide-core` still builds clean with the attribute removed (no other code relied on the `#[allow]` suppressing a real warning elsewhere — `is_npc` is a plain struct field, not shared across an `#[allow]` scope).

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-vision-filter/src/lib.rs crates/eqoxide-core/src/game_state.rs
git commit -m "feat: add line-of-sight occlusion to eqoxide-agent-vision-filter"
```

---

## Task 10: `ManualMove.wish_heading`

**Files:**
- Modify: `crates/eqoxide-ipc/src/lib.rs:1715-1721`
- Modify: `crates/eqoxide-http/src/move_api.rs:88-91,102-105`
- Modify: `src/movement.rs` (add `resolve_heading` near line 157)
- Modify: `src/app.rs:1963-1996`

**Interfaces:**
- Produces: `ManualMove.wish_heading: Option<f32>` (new field), `pub fn resolve_heading(wish_heading: Option<f32>, derived: Option<f32>) -> Option<f32>` in `src/movement.rs` — consumed by `src/app.rs`'s manual-move branch and, indirectly, by whatever the plugin host writes into `ManualMove` (Task 14).

This task has no agent-plugin-host code yet — it lands the primitive both HTTP (unchanged behavior) and the future plugin host will share.

- [ ] **Step 1: Write the failing test**

In `src/movement.rs`, near the existing `manual_wish` function (line 157) and its `#[cfg(test)] mod tests` block (line 2101), add a new test inside that same `mod tests` block:

```rust
    #[test]
    fn resolve_heading_prefers_explicit_wish_heading_over_derived() {
        assert_eq!(resolve_heading(Some(90.0), Some(180.0)), Some(90.0));
    }

    #[test]
    fn resolve_heading_falls_back_to_derived_when_no_explicit_heading() {
        assert_eq!(resolve_heading(None, Some(180.0)), Some(180.0));
    }

    #[test]
    fn resolve_heading_is_none_when_neither_is_set() {
        assert_eq!(resolve_heading(None, None), None);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test resolve_heading` (from the workspace root; this is the top-level `eqoxide` binary crate's lib target)
Expected: FAIL — `resolve_heading` doesn't exist yet (compile error: `cannot find function 'resolve_heading' in this scope`).

- [ ] **Step 3: Write minimal implementation**

In `src/movement.rs`, add this function just after `manual_wish` (line 157's function, ending around line 167):

```rust
/// Resolve the heading to face this tick: an explicit `wish_heading` (agent-driven, spec §6) wins
/// over the direction-derived heading `manual_wish` computes, so a caller can face independently of
/// travel direction (e.g. strafing while facing a target). `None` only when neither is set.
pub fn resolve_heading(wish_heading: Option<f32>, derived: Option<f32>) -> Option<f32> {
    wish_heading.or(derived)
}
```

Add `wish_heading: Option<f32>` to `ManualMove` in `crates/eqoxide-ipc/src/lib.rs`. Change (lines 1714-1721):

```rust
#[derive(Clone, Copy)]
pub struct ManualMove {
    pub dir:   [f32; 2],
    pub up:    f32,
    pub jump:  bool,
    pub until: std::time::Instant,
}
```

to:

```rust
#[derive(Clone, Copy)]
pub struct ManualMove {
    pub dir:   [f32; 2],
    pub up:    f32,
    pub jump:  bool,
    /// Independently-settable heading (spec `docs/specs/2026-09-15-agent-plugin-api-design.md` §6):
    /// `None` means "face the direction of travel" (the pre-existing WASD/HTTP-manual behavior via
    /// `manual_wish`'s derived heading) — see `crate::movement::resolve_heading` in the app crate.
    pub wish_heading: Option<f32>,
    pub until: std::time::Instant,
}
```

Fix the two now-broken construction sites in `crates/eqoxide-http/src/move_api.rs`. Change (line 88-91):

```rust
    s.camera.request_manual_move(ManualMove {
        dir, up, jump,
        until: std::time::Instant::now() + std::time::Duration::from_millis(ms),
    });
```

to:

```rust
    s.camera.request_manual_move(ManualMove {
        dir, up, jump,
        wish_heading: None,
        until: std::time::Instant::now() + std::time::Duration::from_millis(ms),
    });
```

and change (line 102-105):

```rust
    s.camera.request_manual_move(ManualMove {
        dir: [0.0, 0.0], up: 0.0, jump: true,
        until: std::time::Instant::now() + std::time::Duration::from_millis(400),
    });
```

to:

```rust
    s.camera.request_manual_move(ManualMove {
        dir: [0.0, 0.0], up: 0.0, jump: true,
        wish_heading: None,
        until: std::time::Instant::now() + std::time::Duration::from_millis(400),
    });
```

Wire the resolution into `src/app.rs`'s manual-move branch. Change (lines 1963-1968):

```rust
            } else if let Some(m) = manual {
                // Like WASD, manual drive cancels any in-progress /goto so it doesn't fight us.
                self.acts.command.request_cancel_goto();
                *self.nav_intent.lock().unwrap() = None;
                let (wish, heading) = crate::movement::manual_wish(m.dir);
                if let Some(h) = heading { self.heading_target = h; } // face where we walk
```

to:

```rust
            } else if let Some(m) = manual {
                // Like WASD, manual drive cancels any in-progress /goto so it doesn't fight us.
                self.acts.command.request_cancel_goto();
                *self.nav_intent.lock().unwrap() = None;
                let (wish, derived_heading) = crate::movement::manual_wish(m.dir);
                // An explicit wish_heading (agent-driven) wins over the direction-derived one, so
                // an agent can face independently of travel direction (e.g. strafe while facing a
                // target) — falls back to "face where we walk" when unset, same as before.
                if let Some(h) = crate::movement::resolve_heading(m.wish_heading, derived_heading) {
                    self.heading_target = h;
                }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test resolve_heading && cargo build`
Expected: PASS — the 3 new `resolve_heading` tests are green, and `cargo build` succeeds across the whole workspace (confirming the `ManualMove` field addition didn't leave any other construction site broken — `move_api.rs`'s two sites were the only ones outside `app.rs`'s read site).

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-ipc/src/lib.rs crates/eqoxide-http/src/move_api.rs src/movement.rs src/app.rs
git commit -m "feat: add ManualMove.wish_heading for independently-settable facing"
```

---

## Task 11: `eqoxide-agent-plugin-host` scaffold

**Files:**
- Create: `crates/eqoxide-agent-plugin-host/Cargo.toml`
- Create: `crates/eqoxide-agent-plugin-host/src/lib.rs`

**Interfaces:**
- Consumes: `eqoxide_ipc::{CameraSlots, GameStateSnapshot, NetThreadDeadShared}`, `eqoxide_command::CommandState`, `eqoxide_nav::collision::SharedCollision`, `eqoxide_core::spells::SpellDb`.
- Produces: `pub fn spawn_agent_plugin_host(camera: eqoxide_ipc::CameraSlots, command: eqoxide_command::CommandState, game_state: eqoxide_ipc::GameStateSnapshot, shared_collision: eqoxide_nav::collision::SharedCollision, spells: std::sync::Arc<eqoxide_core::spells::SpellDb>, net_thread_dead: eqoxide_ipc::NetThreadDeadShared, socket_path: std::path::PathBuf)` — the entry point `src/main.rs` calls (Task 18).

This task lands a real, minimal, compiling entry point: it binds the socket and logs, with no session handling yet (Tasks 15-17 add that). The "test" for a thread-spawning entry point is a smoke test that it doesn't panic and produces a bound socket file.

- [ ] **Step 1: Write the failing test**

Create `crates/eqoxide-agent-plugin-host/Cargo.toml`:

```toml
[package]
name = "eqoxide-agent-plugin-host"
version = "0.1.0"
edition = "2021"

[lib]
name = "eqoxide_agent_plugin_host"
path = "src/lib.rs"

[features]
# Forwards to eqoxide-nav/eqoxide-ipc so downstream integration-test builders (this crate's own
# session/observation_builder tests, Tasks 12-17) can construct real fixtures — same pattern as
# eqoxide-http's test-fixtures feature.
test-fixtures = ["eqoxide-nav/test-fixtures", "eqoxide-ipc/test-fixtures"]

[dependencies]
eqoxide-core = { path = "../eqoxide-core" }
eqoxide-ipc = { path = "../eqoxide-ipc" }
eqoxide-command = { path = "../eqoxide-command" }
eqoxide-nav = { path = "../eqoxide-nav" }
eqoxide-agent-protocol = { path = "../eqoxide-agent-protocol" }
eqoxide-agent-vision-filter = { path = "../eqoxide-agent-vision-filter" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
tracing = "0.1"

[dev-dependencies]
eqoxide-core = { path = "../eqoxide-core", features = ["test-fixtures"] }
eqoxide-ipc = { path = "../eqoxide-ipc", features = ["test-fixtures"] }
eqoxide-command = { path = "../eqoxide-command", features = ["test-fixtures"] }
tokio = { version = "1", features = ["full", "test-util"] }
```

Create `crates/eqoxide-agent-plugin-host/src/lib.rs`:

```rust
//! In-client plumbing for the Agent Plugin API (spec §4): owns the Unix domain socket server,
//! performs the version handshake, latches each `Step` into the existing `CameraSlots`/
//! `CommandState` mailboxes, and pushes an `Observation` every tick. Deliberately thin — no
//! policy/decision logic lives here (spec §4, §12).

use eqoxide_command::CommandState;
use eqoxide_core::spells::SpellDb;
use eqoxide_ipc::{CameraSlots, GameStateSnapshot, NetThreadDeadShared};
use eqoxide_nav::collision::SharedCollision;
use std::path::PathBuf;
use std::sync::Arc;

/// Bind the agent socket and start accepting connections on its own thread (mirrors
/// `eqoxide_http::spawn_camera_server`'s own-thread-plus-own-tokio-runtime pattern). One
/// connection is served at a time — see `src/lib.rs`'s accept loop, added in Task 17.
pub fn spawn_agent_plugin_host(
    camera: CameraSlots,
    command: CommandState,
    game_state: GameStateSnapshot,
    shared_collision: SharedCollision,
    spells: Arc<SpellDb>,
    net_thread_dead: NetThreadDeadShared,
    socket_path: PathBuf,
) {
    std::thread::Builder::new()
        .name("agent-plugin-host".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("agent-plugin-host tokio runtime");
            rt.block_on(async move {
                if socket_path.exists() {
                    let _ = std::fs::remove_file(&socket_path);
                }
                let listener = match tokio::net::UnixListener::bind(&socket_path) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!("agent-plugin-host: failed to bind {}: {e}", socket_path.display());
                        return;
                    }
                };
                tracing::info!("agent-plugin-host: listening on {}", socket_path.display());
                let _ = (&camera, &command, &game_state, &shared_collision, &spells, &net_thread_dead, &listener);
            });
        })
        .expect("spawn agent-plugin-host thread");
}
```

(The trailing `let _ = (...)` line silences unused-parameter warnings for arguments this task doesn't consume yet — Task 17 replaces it with the real accept loop that uses every one of them. This is a deliberate one-line placeholder for warnings only, not for logic; it's fully removed by Task 17's Step 3.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo build -p eqoxide-agent-plugin-host`
Expected: FAIL — the crate isn't in the workspace member list under this exact name yet if Task 1's Step 3 edit didn't already include it. (It does — Task 1 added all three crate paths up front. So this step's actual red state is: the crate directory has no `Cargo.toml`/`src/lib.rs` yet. Confirm with `cargo build -p eqoxide-agent-plugin-host` before writing the two files above; expect `error: package ID specification... did not match`.)

- [ ] **Step 3: Implementation is already written above (Step 1) — nothing further to add.**

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo build -p eqoxide-agent-plugin-host`
Expected: PASS — compiles clean, no warnings (the `let _ = (...)` line accounts for every otherwise-unused parameter).

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-plugin-host
git commit -m "feat: scaffold eqoxide-agent-plugin-host with a binding entry point"
```

---

## Task 12: `legal_actions::build_legal_actions`

**Files:**
- Create: `crates/eqoxide-agent-plugin-host/src/legal_actions.rs`
- Modify: `crates/eqoxide-agent-plugin-host/src/lib.rs`

**Interfaces:**
- Consumes: `eqoxide_core::game_state::gem_is_empty(spell_id: u32) -> bool`, `eqoxide_core::spells::{SpellDb, SpellInfo, SPA_BLANK}`, `eqoxide_agent_protocol::observation::{AbilityFeature, LegalActionMask}`.
- Produces: `pub fn build_legal_actions(mem_spells: &[u32; 9], spell_db: &SpellDb) -> LegalActionMask` — consumed by `observation_builder::build_observation` (Task 13).

- [ ] **Step 1: Write the failing test**

Create `crates/eqoxide-agent-plugin-host/src/legal_actions.rs`:

```rust
//! Builds the wire `LegalActionMask` from the character's memorized spells (spec §9). Real data:
//! `GameState.mem_spells` (9 gem slots) plus `SpellDb`, including the per-spell mana cost/cast
//! time/recast delay landed by GitHub issue #1127 (`SpellInfo.mana_cost`/`cast_time_ms`/`recast_time_ms`).

use eqoxide_agent_protocol::observation::{AbilityFeature, LegalActionMask};
use eqoxide_core::game_state::gem_is_empty;
use eqoxide_core::spells::{SpellDb, SPA_BLANK};

pub fn build_legal_actions(mem_spells: &[u32; 9], spell_db: &SpellDb) -> LegalActionMask {
    let mut gems = [false; 9];
    let mut abilities = Vec::new();
    for (i, &spell_id) in mem_spells.iter().enumerate() {
        if gem_is_empty(spell_id) {
            continue;
        }
        gems[i] = true;
        if let Some(info) = spell_db.get(spell_id) {
            let effects: Vec<i32> = info.effects.iter().copied().filter(|&e| e != SPA_BLANK).collect();
            abilities.push(AbilityFeature {
                gem: i as u8,
                spell_id,
                target_type: info.target_type,
                effects,
                mana_cost: info.mana_cost,
                cast_time_ms: info.cast_time_ms,
                recast_time_ms: info.recast_time_ms,
            });
        }
    }
    LegalActionMask { gems, abilities }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_core::game_state::EMPTY_GEM;
    use eqoxide_core::spells::SpellInfo;

    fn db_with(id: u32, info: SpellInfo) -> SpellDb {
        let mut db = SpellDb::default();
        db.insert_for_test(id, info);
        db
    }

    #[test]
    fn empty_gems_produce_no_abilities_and_all_false_gems() {
        let mem_spells = [EMPTY_GEM; 9];
        let db = SpellDb::default();
        let mask = build_legal_actions(&mem_spells, &db);
        assert_eq!(mask.gems, [false; 9]);
        assert!(mask.abilities.is_empty());
    }

    #[test]
    fn a_memorized_spell_in_the_db_produces_a_true_gem_and_an_ability_with_blank_effects_filtered() {
        let mut mem_spells = [EMPTY_GEM; 9];
        mem_spells[2] = 12;
        let db = db_with(
            12,
            SpellInfo {
                name: "Minor Healing".into(),
                icon_id: 1,
                good_effect: 1,
                target_type: 5,
                effects: [79, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK],
                mana_cost: 25,
                cast_time_ms: 3000,
                recast_time_ms: 0,
            },
        );
        let mask = build_legal_actions(&mem_spells, &db);
        assert!(mask.gems[2], "gem 2 holds a real spell id, so it must be legal");
        assert!(!mask.gems[0] && !mask.gems[1], "empty gems must stay false");
        assert_eq!(mask.abilities.len(), 1);
        assert_eq!(mask.abilities[0].gem, 2);
        assert_eq!(mask.abilities[0].spell_id, 12);
        assert_eq!(mask.abilities[0].target_type, 5);
        assert_eq!(mask.abilities[0].effects, vec![79], "SPA_BLANK slots must be filtered out");
        assert_eq!(mask.abilities[0].mana_cost, 25, "#1127: mana_cost must pass through from SpellInfo");
        assert_eq!(mask.abilities[0].cast_time_ms, 3000, "#1127: cast_time_ms must pass through from SpellInfo");
        assert_eq!(mask.abilities[0].recast_time_ms, 0, "#1127: recast_time_ms must pass through from SpellInfo");
    }

    #[test]
    fn a_memorized_spell_not_in_the_db_still_marks_the_gem_legal_with_no_ability_feature() {
        let mut mem_spells = [EMPTY_GEM; 9];
        mem_spells[0] = 999;
        let db = SpellDb::default();
        let mask = build_legal_actions(&mem_spells, &db);
        assert!(mask.gems[0]);
        assert!(mask.abilities.is_empty(), "no SpellInfo means no ability feature, but the gem is still legal");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-plugin-host legal_actions::`
Expected: FAIL — `mod legal_actions;` isn't wired into `lib.rs`, and (separately) `SpellDb::insert_for_test` / `EMPTY_GEM` may not yet be `pub` test-fixture helpers on `eqoxide-core` — confirm with a `cargo test -p eqoxide-agent-plugin-host --features test-fixtures legal_actions::` and adjust import paths per whatever compiler errors name (see Step 3 note).

- [ ] **Step 3: Wire the module; add a `SpellDb` test-fixture constructor if missing**

In `crates/eqoxide-agent-plugin-host/src/lib.rs`, add:

```rust
pub mod legal_actions;
```

Check whether `eqoxide_core::spells::SpellDb` already exposes a test-only insertion method. If `cargo test -p eqoxide-agent-plugin-host legal_actions::` reports `no method named 'insert_for_test' found`, add one in `crates/eqoxide-core/src/spells.rs` next to `SpellDb::get`:

```rust
    /// Test-only direct insertion — production code only ever populates `by_id` from the parsed
    /// `spells_us.txt` (`SpellDb::load`). Gated the same way `eqoxide-ipc`'s `Roster::insert_for_test`
    /// is: invisible outside `#[cfg(test)]`/`test-fixtures` builds.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn insert_for_test(&mut self, id: u32, info: SpellInfo) {
        self.by_id.insert(id, info);
    }
```

and confirm `crates/eqoxide-core/Cargo.toml` already declares a `test-fixtures` feature (it does, per the workspace's established pattern used by `eqoxide-ipc`/`eqoxide-nav`/`eqoxide-command`); if `eqoxide-core`'s own `[features]` section doesn't list `test-fixtures = []`, add it there.

Also confirm `EMPTY_GEM` is `pub` in `crates/eqoxide-core/src/game_state.rs` (it's referenced as a documented constant in this plan's research — if the compiler reports it's private, change its declaration to `pub const EMPTY_GEM: u32 = 0xFFFF_FFFF;`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-plugin-host --features test-fixtures legal_actions::`
Expected: PASS — 3 tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-plugin-host/src/legal_actions.rs crates/eqoxide-agent-plugin-host/src/lib.rs crates/eqoxide-core/src/spells.rs crates/eqoxide-core/src/game_state.rs crates/eqoxide-core/Cargo.toml
git commit -m "feat: build LegalActionMask from mem_spells + SpellDb"
```

---

## Task 13: `observation_builder`

**Files:**
- Create: `crates/eqoxide-agent-plugin-host/src/observation_builder.rs`
- Modify: `crates/eqoxide-agent-plugin-host/src/lib.rs`

**Interfaces:**
- Consumes: `eqoxide_core::game_state::GameState` (fields: `player_x/y/z: f32`, `player_heading: f32`, `cur_hp/max_hp/cur_mana/max_mana: i32`, `cur_endurance/max_endurance: i32`, `endurance_confirmed: bool`, `buffs: BTreeMap<u32, BuffSlot>`, `casting: Option<CastState>`, `player_dead: bool`, `mem_spells: [u32; 9]`, `world.entities: HashMap<u32, Entity>`, `world.zone_name: String`), `eqoxide_core::game_state::hp_verified(&self) -> bool`, `eqoxide_core::game_state::BuffSlot { spell_id: u32, duration_ticks: i32 }` (#1127), `eqoxide_core::game_state::CastState { spell_id: u32, started: std::time::Instant, cast_ms: u32 }`, `eqoxide_ipc::NetThreadDeadShared`, `eqoxide_nav::collision::SharedCollision`, `eqoxide_agent_vision_filter::{visible_entities, VISIBILITY_DIST}` (Task 9), `legal_actions::build_legal_actions` (Task 12).
- Produces: `pub fn build_observation(gs: &eqoxide_core::game_state::GameState, collision: &eqoxide_nav::collision::SharedCollision, spells: &eqoxide_core::spells::SpellDb, net_thread_dead: &eqoxide_ipc::NetThreadDeadShared) -> eqoxide_agent_protocol::observation::Observation` — consumed by `session`'s tick loop (Task 16).

**Design decisions this task encodes:**
- `Observation.dead` maps to `gs.player_dead` specifically (confirmed OP_Death), not any broader HP-halted-but-unconfirmed state — matching `eqoxide-http`'s own documented distinction that a `MoveGate`/`require_alive` "halted_hp_zero" state is NOT remedied by respawn ("nothing has died"). Using the broader signal would wrongly tell the agent to respawn when respawn isn't the fix.
- `Observation.terminated` maps to `net_thread_dead`'s underlying `Option<NetThreadDeath>` being `Some` — the one signal eqoxide can honestly call "the MDP has reached a terminal state" (the connection to the game world is unrecoverably gone). `Observation.truncated` is always `false` from eqoxide's side (v1 imposes no rollout cutoff of its own).

- [ ] **Step 1: Write the failing test**

Create `crates/eqoxide-agent-plugin-host/src/observation_builder.rs`:

```rust
//! Composes the per-tick `Observation` from `GameState` + the vision filter + the legal-action
//! mask (spec §9). Pure function of its inputs — no I/O, so it's directly unit-testable.

use crate::legal_actions::build_legal_actions;
use eqoxide_agent_protocol::observation::{BuffView, CastingView, Observation, OwnState};
use eqoxide_core::game_state::GameState;
use eqoxide_core::spells::SpellDb;
use eqoxide_ipc::NetThreadDeadShared;
use eqoxide_nav::collision::SharedCollision;

fn build_own_state(gs: &GameState) -> OwnState {
    OwnState {
        pos: [gs.player_x, gs.player_y, gs.player_z],
        heading: gs.player_heading,
        hp: gs.cur_hp,
        hp_max: gs.max_hp,
        hp_verified: gs.hp_verified(),
        mana: gs.cur_mana,
        mana_max: gs.max_mana,
        endurance: gs.cur_endurance,
        endurance_max: gs.max_endurance,
        endurance_confirmed: gs.endurance_confirmed,
        casting: gs.casting.as_ref().map(|c| CastingView {
            spell_id: c.spell_id,
            elapsed_ms: c.started.elapsed().as_millis() as u32,
            cast_ms: c.cast_ms,
        }),
        buffs: gs
            .buffs
            .iter()
            .map(|(&slot, b)| BuffView { slot, spell_id: b.spell_id, duration_ticks: b.duration_ticks })
            .collect(),
        zone_name: gs.world.zone_name.clone(),
    }
}

pub fn build_observation(
    gs: &GameState,
    collision: &SharedCollision,
    spells: &SpellDb,
    net_thread_dead: &NetThreadDeadShared,
) -> Observation {
    let visible = eqoxide_agent_vision_filter::visible_entities(
        &gs.world.entities,
        [gs.player_x, gs.player_y, gs.player_z],
        eqoxide_agent_vision_filter::VISIBILITY_DIST,
        collision,
    );
    Observation {
        own: build_own_state(gs),
        visible,
        legal_actions: build_legal_actions(&gs.mem_spells, spells),
        // Confirmed OP_Death, not the broader "HP halted but unconfirmed" state — respawn is only
        // ever the correct remedy for THIS signal (mirrors eqoxide-http's require_alive distinction).
        dead: gs.player_dead,
        // The one signal eqoxide can honestly assert as a true terminal state: the net thread is
        // gone for good.
        terminated: net_thread_dead.lock().unwrap().is_some(),
        // eqoxide imposes no rollout cutoff of its own in v1; an external driver may still truncate
        // its own rollout without needing this field to say so.
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_core::game_state::{make_entity, GameState};
    use eqoxide_core::spells::SpellDb;
    use std::sync::{Arc, Mutex, RwLock};

    fn empty_collision() -> SharedCollision {
        Arc::new(RwLock::new(None))
    }

    fn no_death() -> NetThreadDeadShared {
        Arc::new(Mutex::new(None))
    }

    #[test]
    fn own_state_reflects_position_health_and_zone() {
        let mut gs = GameState::default();
        gs.player_x = 1.0;
        gs.player_y = 2.0;
        gs.player_z = 3.0;
        gs.player_heading = 45.0;
        gs.cur_hp = 80;
        gs.max_hp = 100;
        gs.world.zone_name = "qeynos".into();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert_eq!(obs.own.pos, [1.0, 2.0, 3.0]);
        assert_eq!(obs.own.heading, 45.0);
        assert_eq!(obs.own.hp, 80);
        assert_eq!(obs.own.hp_max, 100);
        assert_eq!(obs.own.zone_name, "qeynos");
        assert!(obs.own.casting.is_none());
    }

    #[test]
    fn endurance_reflects_game_state_confirmed_values() {
        let mut gs = GameState::default();
        gs.set_endurance(80, 100);
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert_eq!(obs.own.endurance, 80);
        assert_eq!(obs.own.endurance_max, 100);
        assert!(obs.own.endurance_confirmed, "set_endurance is the authoritative OP_EnduranceUpdate path");
    }

    #[test]
    fn endurance_confirmed_is_false_before_any_endurance_update_is_seen() {
        let gs = GameState::default();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert!(
            !obs.own.endurance_confirmed,
            "agent-honesty: no confident endurance claim before the server has said anything"
        );
    }

    #[test]
    fn buffs_mirror_game_state_buffs_sorted_by_slot() {
        let mut gs = GameState::default();
        gs.buff_slot_set(5, 12, 42);
        gs.buff_slot_set(1, 90, -1000);
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert_eq!(
            obs.own.buffs,
            vec![
                BuffView { slot: 1, spell_id: 90, duration_ticks: -1000 },
                BuffView { slot: 5, spell_id: 12, duration_ticks: 42 },
            ],
            "buffs come from a BTreeMap, so iteration order is already sorted by slot"
        );
    }

    #[test]
    fn dead_maps_to_player_dead_specifically() {
        let mut gs = GameState::default();
        gs.player_dead = true;
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert!(obs.dead);
    }

    #[test]
    fn terminated_is_false_when_net_thread_dead_is_none() {
        let gs = GameState::default();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert!(!obs.terminated);
    }

    #[test]
    fn terminated_is_true_when_net_thread_dead_is_some() {
        use eqoxide_ipc::{NetThreadDeath, NetThreadEnd};
        let gs = GameState::default();
        let dead: NetThreadDeadShared =
            Arc::new(Mutex::new(Some(NetThreadDeath::new(NetThreadEnd::Panicked, "test"))));
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &dead);
        assert!(obs.terminated);
    }

    #[test]
    fn truncated_is_always_false() {
        let gs = GameState::default();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert!(!obs.truncated);
    }

    #[test]
    fn visible_entities_come_from_world_entities_via_the_vision_filter() {
        let mut gs = GameState::default();
        gs.player_x = 0.0;
        gs.player_y = 0.0;
        gs.player_z = 0.0;
        gs.world.entities.insert(7, make_entity(7, "a_rat00", [5.0, 0.0, 0.0]));
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        // No zone geometry loaded (empty_collision) -> has_line_of_sight returns false for everyone
        // (agent-honesty: no visibility claim without real geometry) -> visible is empty. This
        // confirms the wiring reaches the vision filter at all, which is what this test checks.
        assert!(obs.visible.is_empty(), "no collision grid loaded means no LOS can be confirmed");
    }
}
```

Note: `make_entity` is presumed to be an existing `eqoxide-core` test-fixture constructor (per this plan's research summary, referenced as `game_state::make_entity` in `eqoxide-ipc/Cargo.toml`'s own doc comments). If `cargo test` reports it doesn't exist with this exact signature, adjust the call in `visible_entities_come_from_world_entities_via_the_vision_filter` to match whatever `make_entity`'s real signature is (check `crates/eqoxide-core/src/game_state.rs` for `#[cfg(any(test, feature = "test-fixtures"))] pub fn make_entity`), or construct an `Entity` literal directly using the field list from Task 8/9's test fixtures if no such constructor exists.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-plugin-host --features test-fixtures observation_builder::`
Expected: FAIL — `mod observation_builder;` isn't wired into `lib.rs` yet.

- [ ] **Step 3: Wire the module**

In `crates/eqoxide-agent-plugin-host/src/lib.rs`, add:

```rust
pub mod observation_builder;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-plugin-host --features test-fixtures observation_builder::`
Expected: PASS — 9 tests green. If `make_entity`'s real signature differs from what Step 1 assumed, fix the one call site per Step 1's note and re-run.

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-plugin-host/src/observation_builder.rs crates/eqoxide-agent-plugin-host/src/lib.rs
git commit -m "feat: compose Observation from GameState + vision filter + legal actions"
```

---

## Task 14: Verb dispatch

**Files:**
- Create: `crates/eqoxide-agent-plugin-host/src/session.rs`
- Modify: `crates/eqoxide-agent-plugin-host/src/lib.rs`

**Interfaces:**
- Consumes: `eqoxide_command::CommandState::{request_target(spawn_id: u32) -> bool, request_attack(on: bool) -> bool, request_consider(spawn_id: u32) -> bool, request_cast(req: eqoxide_ipc::CastRequest) -> bool, request_sit(sit: bool) -> bool, request_respawn(&self)}` (all confirmed at `crates/eqoxide-command/src/{combat,interact,lifecycle}.rs`), `eqoxide_agent_protocol::verb::{AgentVerb, CombatVerb, InteractVerb, LifecycleVerb}`.
- Produces: `pub fn dispatch_verb(verb: &AgentVerb, command: &eqoxide_command::CommandState)` — consumed by `apply_step` (Task 15).

- [ ] **Step 1: Write the failing test**

Create `crates/eqoxide-agent-plugin-host/src/session.rs`:

```rust
//! Per-connection handshake, tick loop, and action dispatch (spec §5, §7, §8).

use eqoxide_agent_protocol::verb::{AgentVerb, CombatVerb, InteractVerb, LifecycleVerb};
use eqoxide_command::CommandState;

/// Translate one `AgentVerb` into the real `CommandState` call it mirrors. Reserved-but-unwired
/// arms (`Move(ZoneCross)` and the four uninhabited families) are accepted and deliberately no-op —
/// not malformed, just not wired yet (spec §8, §12).
pub fn dispatch_verb(verb: &AgentVerb, command: &CommandState) {
    match verb {
        AgentVerb::Combat(CombatVerb::Target { spawn_id }) => {
            command.request_target(*spawn_id);
        }
        AgentVerb::Combat(CombatVerb::Attack { on }) => {
            command.request_attack(*on);
        }
        AgentVerb::Combat(CombatVerb::Consider { spawn_id }) => {
            command.request_consider(*spawn_id);
        }
        AgentVerb::Combat(CombatVerb::Cast(c)) => {
            command.request_cast(eqoxide_ipc::CastRequest {
                gem: c.gem,
                target_id: c.target_id,
                item_slot: c.item_slot,
            });
        }
        AgentVerb::Interact(InteractVerb::Sit) => {
            command.request_sit(true);
        }
        AgentVerb::Interact(InteractVerb::Stand) => {
            command.request_sit(false);
        }
        AgentVerb::Lifecycle(LifecycleVerb::Respawn) => {
            command.request_respawn();
        }
        // Reserved, typed, not wired (spec §8, §12) — accepted, deliberately does nothing yet.
        AgentVerb::Move(_) => {}
        // Uninhabited — no value of these types can ever be constructed, so these arms are
        // unreachable in practice; kept for exhaustiveness so a future inhabited variant is a
        // compile error here until dispatched.
        AgentVerb::Merchant(v) => match *v {},
        AgentVerb::Inventory(v) => match *v {},
        AgentVerb::Quests(v) => match *v {},
        AgentVerb::Chat(v) => match *v {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_agent_protocol::verb::{CastRequest, MoveVerb};
    use eqoxide_command::CommandState;

    #[test]
    fn combat_target_writes_to_the_target_slot() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Combat(CombatVerb::Target { spawn_id: 42 }), &command);
        assert_eq!(command.take_target(), Some(42));
    }

    #[test]
    fn combat_attack_writes_to_the_attack_slot() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Combat(CombatVerb::Attack { on: true }), &command);
        assert_eq!(command.take_attack(), Some(true));
    }

    #[test]
    fn combat_cast_writes_to_the_cast_slot() {
        let command = CommandState::default();
        dispatch_verb(
            &AgentVerb::Combat(CombatVerb::Cast(CastRequest { gem: 0, target_id: Some(9), item_slot: None })),
            &command,
        );
        let cast = command.take_cast().expect("cast queued");
        assert_eq!(cast.gem, 0);
        assert_eq!(cast.target_id, Some(9));
    }

    #[test]
    fn interact_sit_requests_sit_true() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Interact(InteractVerb::Sit), &command);
        assert_eq!(command.take_sit(), Some(true));
    }

    #[test]
    fn interact_stand_requests_sit_false() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Interact(InteractVerb::Stand), &command);
        assert_eq!(command.take_sit(), Some(false));
    }

    #[test]
    fn lifecycle_respawn_does_not_panic() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Lifecycle(LifecycleVerb::Respawn), &command);
    }

    #[test]
    fn reserved_move_zone_cross_is_a_deliberate_no_op() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Move(MoveVerb::ZoneCross), &command);
        assert_eq!(command.take_target(), None, "a reserved verb must not touch any real slot");
    }
}
```

Note: this task assumes `CommandState` exposes `take_target`/`take_attack`/`take_cast`/`take_sit` drain methods matching the `request_*` writers (per the crate's own documented naming convention: `take_<thing>() -> Option<T>` mirrors each `request_<verb>`, `crates/eqoxide-command/src/lib.rs:66`). If any exact method name differs from this guess, adjust the test call to match what `cargo test`'s compiler errors name — the writer methods (`request_target`, etc.) are confirmed exact; only the drain-side names are inferred from the documented convention rather than directly read.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-plugin-host session::`
Expected: FAIL — `mod session;` isn't wired into `lib.rs` yet.

- [ ] **Step 3: Wire the module**

In `crates/eqoxide-agent-plugin-host/src/lib.rs`, add:

```rust
pub mod session;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-plugin-host session::`
Expected: PASS — 7 tests green. If a `take_*` method name doesn't match, fix the test call site per Step 1's note (the implementation in `dispatch_verb` itself only calls `request_*` methods, which are confirmed exact and need no adjustment).

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-plugin-host/src/session.rs crates/eqoxide-agent-plugin-host/src/lib.rs
git commit -m "feat: dispatch AgentVerb to CommandState"
```

---

## Task 15: Movement dispatch (`apply_step`)

**Files:**
- Modify: `crates/eqoxide-agent-plugin-host/src/session.rs`

**Interfaces:**
- Consumes: `eqoxide_ipc::{CameraSlots, ManualMove}`, `eqoxide_agent_protocol::step::Step`, `dispatch_verb` (Task 14).
- Produces: `pub fn apply_step(step: &Step, camera: &eqoxide_ipc::CameraSlots, command: &eqoxide_command::CommandState)` — consumed by the tick loop (Task 16).

**Design decision (from this plan's Global Constraints):** the tick loop re-issues `ManualMove` every tick with a short deadline (`now + 300ms`, ~2x the 150ms tick), matching "held key" semantics and providing a natural fail-safe if the agent stops sending Steps.

- [ ] **Step 1: Write the failing test**

In `crates/eqoxide-agent-plugin-host/src/session.rs`, add near the top (after the `use` block) and extend the test module:

```rust
use eqoxide_agent_protocol::step::Step;
use eqoxide_ipc::{CameraSlots, ManualMove};
use std::time::{Duration, Instant};

/// Re-issued every tick (~2x the 150ms tick, `MOVE_LATCH`), so movement naturally stops within
/// ~300ms of the agent going quiet — the same fail-safe deadline mechanism `ManualMove` already
/// gives HTTP's `/v1/move/manual`, applied here every tick instead of once per request.
const MOVE_LATCH: Duration = Duration::from_millis(300);

pub fn apply_step(step: &Step, camera: &CameraSlots, command: &CommandState) {
    if let Some(m) = &step.movement {
        camera.request_manual_move(ManualMove {
            dir: m.dir,
            up: m.up,
            jump: m.jump,
            wish_heading: m.wish_heading,
            until: Instant::now() + MOVE_LATCH,
        });
    }
    if let Some(verb) = &step.verb {
        dispatch_verb(verb, command);
    }
}
```

and add to the `#[cfg(test)] mod tests` block:

```rust
    use eqoxide_agent_protocol::movement::AgentMovement;

    #[test]
    fn apply_step_with_movement_writes_manual_move() {
        let camera = CameraSlots::default();
        let command = CommandState::default();
        let step = Step {
            movement: Some(AgentMovement { dir: [1.0, 0.0], up: 0.0, jump: false, wish_heading: Some(90.0) }),
            verb: None,
        };
        apply_step(&step, &camera, &command);
        let m = camera.manual_move.lock().unwrap().expect("manual move queued");
        assert_eq!(m.dir, [1.0, 0.0]);
        assert_eq!(m.wish_heading, Some(90.0));
        assert!(m.until > Instant::now(), "the deadline must be in the future");
    }

    #[test]
    fn apply_step_with_no_movement_leaves_manual_move_slot_untouched() {
        let camera = CameraSlots::default();
        let command = CommandState::default();
        let step = Step { movement: None, verb: None };
        apply_step(&step, &camera, &command);
        assert!(camera.manual_move.lock().unwrap().is_none());
    }

    #[test]
    fn apply_step_with_both_movement_and_verb_applies_both() {
        let camera = CameraSlots::default();
        let command = CommandState::default();
        let step = Step {
            movement: Some(AgentMovement { dir: [0.0, 1.0], up: 0.0, jump: false, wish_heading: None }),
            verb: Some(AgentVerb::Combat(CombatVerb::Attack { on: true })),
        };
        apply_step(&step, &camera, &command);
        assert!(camera.manual_move.lock().unwrap().is_some());
        assert_eq!(command.take_attack(), Some(true));
    }
```

If `CameraSlots` does not derive `Default`, this test will fail to compile; use `CameraSlots { cmd_tx: Default::default(), snapshot: Default::default(), frame_req: Default::default(), manual_move: Default::default() }` explicitly instead of `CameraSlots::default()`, matching the exact field list confirmed at `crates/eqoxide-ipc/src/lib.rs:3012-3017`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-plugin-host session::apply_step`
Expected: FAIL — `apply_step` doesn't exist yet (compile error).

- [ ] **Step 3: Implementation is already written above (Step 1) — nothing further to add.**

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-plugin-host session::`
Expected: PASS — all session tests (7 from Task 14 + 3 new) green.

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-plugin-host/src/session.rs
git commit -m "feat: latch Step movement into ManualMove with a 300ms fail-safe deadline"
```

---

## Task 16: Handshake accept/reject

**Files:**
- Modify: `crates/eqoxide-agent-plugin-host/src/session.rs`

**Interfaces:**
- Consumes: `eqoxide_agent_protocol::{handshake::{Hello, HandshakeReply, PROTOCOL_VERSION}, framing::{encode_line, decode_line}}`, `tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, AsyncBufReadExt}`.
- Produces: `pub async fn handshake<R: tokio::io::AsyncBufRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(reader: &mut R, writer: &mut W) -> bool` (returns `true` on `Accepted`, `false` on rejection or malformed first line — either way the caller closes the connection on `false`) — consumed by the connection-handling entry point (Task 17).

- [ ] **Step 1: Write the failing test**

In `crates/eqoxide-agent-plugin-host/src/session.rs`, add:

```rust
use eqoxide_agent_protocol::framing::{decode_line, encode_line};
use eqoxide_agent_protocol::handshake::{HandshakeReply, Hello, PROTOCOL_VERSION};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Read the client's `Hello`, reply `Accepted`/`Rejected`, and report which happened. The caller
/// closes the connection immediately on `false` (spec §5, §11).
pub async fn handshake<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(reader: &mut R, writer: &mut W) -> bool {
    let mut line = String::new();
    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return false; // connection closed before sending anything
    }
    let hello: Hello = match decode_line(&line) {
        Ok(h) => h,
        Err(_) => {
            let reply = HandshakeReply::Rejected {
                server_protocol_version: PROTOCOL_VERSION,
                message: "malformed Hello".into(),
            };
            let _ = writer.write_all(encode_line(&reply).unwrap().as_bytes()).await;
            return false;
        }
    };
    if hello.protocol_version != PROTOCOL_VERSION {
        let reply = HandshakeReply::Rejected {
            server_protocol_version: PROTOCOL_VERSION,
            message: format!(
                "client protocol_version {} != server {PROTOCOL_VERSION}",
                hello.protocol_version
            ),
        };
        let _ = writer.write_all(encode_line(&reply).unwrap().as_bytes()).await;
        return false;
    }
    let _ = writer.write_all(encode_line(&HandshakeReply::Accepted).unwrap().as_bytes()).await;
    true
}
```

and add to `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn handshake_accepts_matching_protocol_version() {
        let hello = encode_line(&Hello { protocol_version: PROTOCOL_VERSION }).unwrap();
        let mut reader = tokio::io::BufReader::new(hello.as_bytes());
        let mut writer: Vec<u8> = Vec::new();
        let accepted = handshake(&mut reader, &mut writer).await;
        assert!(accepted);
        let reply: HandshakeReply = decode_line(std::str::from_utf8(&writer).unwrap()).unwrap();
        assert_eq!(reply, HandshakeReply::Accepted);
    }

    #[tokio::test]
    async fn handshake_rejects_mismatched_protocol_version() {
        let hello = encode_line(&Hello { protocol_version: PROTOCOL_VERSION + 1 }).unwrap();
        let mut reader = tokio::io::BufReader::new(hello.as_bytes());
        let mut writer: Vec<u8> = Vec::new();
        let accepted = handshake(&mut reader, &mut writer).await;
        assert!(!accepted);
        let reply: HandshakeReply = decode_line(std::str::from_utf8(&writer).unwrap()).unwrap();
        match reply {
            HandshakeReply::Rejected { server_protocol_version, .. } => {
                assert_eq!(server_protocol_version, PROTOCOL_VERSION);
            }
            HandshakeReply::Accepted => panic!("a version mismatch must not be accepted"),
        }
    }

    #[tokio::test]
    async fn handshake_rejects_malformed_first_line() {
        let mut reader = tokio::io::BufReader::new("not json\n".as_bytes());
        let mut writer: Vec<u8> = Vec::new();
        let accepted = handshake(&mut reader, &mut writer).await;
        assert!(!accepted);
    }

    #[tokio::test]
    async fn handshake_returns_false_on_immediate_eof() {
        let mut reader = tokio::io::BufReader::new(&b""[..]);
        let mut writer: Vec<u8> = Vec::new();
        let accepted = handshake(&mut reader, &mut writer).await;
        assert!(!accepted);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-plugin-host session::handshake`
Expected: FAIL — `handshake` doesn't exist yet (compile error).

- [ ] **Step 3: Implementation is already written above (Step 1) — nothing further to add.**

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-plugin-host session::`
Expected: PASS — all session tests (10 from Tasks 14-15 + 4 new) green.

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-plugin-host/src/session.rs
git commit -m "feat: add connect-time protocol_version handshake accept/reject"
```

---

## Task 17: Tick loop

**Files:**
- Modify: `crates/eqoxide-agent-plugin-host/src/session.rs`

**Interfaces:**
- Consumes: `handshake` (Task 16), `apply_step` (Task 15), `observation_builder::build_observation` (Task 13), `tokio::net::UnixStream`, `tokio::time::interval`.
- Produces: `pub async fn run(stream: tokio::net::UnixStream, camera: eqoxide_ipc::CameraSlots, command: eqoxide_command::CommandState, game_state: eqoxide_ipc::GameStateSnapshot, shared_collision: eqoxide_nav::collision::SharedCollision, spells: std::sync::Arc<eqoxide_core::spells::SpellDb>, net_thread_dead: eqoxide_ipc::NetThreadDeadShared)` — consumed by the accept loop (Task 18).

**Design decisions this task encodes:** the reader runs as an independent tokio task that continuously latches the most-recently-received `Step` into a shared slot (async duplex, not lockstep — spec §5); the tick loop pushes an `Observation` every `TICK_MS` unconditionally, regardless of whether a new `Step` arrived, applying whatever `Step` is currently latched (or none).

- [ ] **Step 1: Write the failing test**

In `crates/eqoxide-agent-plugin-host/src/session.rs`, add:

```rust
use crate::observation_builder::build_observation;
use eqoxide_core::game_state::GameState;
use eqoxide_core::spells::SpellDb;
use eqoxide_ipc::{GameStateSnapshot, NetThreadDeadShared};
use eqoxide_nav::collision::SharedCollision;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncBufReadExt;
use tokio::net::UnixStream;

/// Mirrors `eqoxide_net::action_loop`'s private `NAV_TICK_MS = 150` (`crates/eqoxide-net/src/
/// action_loop.rs:9`) — movement/combat decisions only actually change at that cadence today, so
/// ticking faster would just repeat stale decisions (spec §5). Mirrored, not imported: this crate
/// must not depend on eqoxide-net.
const TICK_MS: u64 = 150;

/// Serve one connection end-to-end: handshake, then the async-duplex tick loop (spec §5, §10). A
/// disconnect ends this function; the caller's accept loop then serves the next connection —
/// reconnect resumes the same underlying session because nothing here is per-connection state
/// beyond the socket itself (the character/game state is the one shared, persistent thing).
pub async fn run(
    stream: UnixStream,
    camera: CameraSlots,
    command: CommandState,
    game_state: GameStateSnapshot,
    shared_collision: SharedCollision,
    spells: Arc<SpellDb>,
    net_thread_dead: NetThreadDeadShared,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);
    if !handshake(&mut reader, &mut write_half).await {
        return;
    }

    let latest_step: Arc<Mutex<Option<Step>>> = Arc::new(Mutex::new(None));
    let reader_step = latest_step.clone();
    let reader_task = tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break, // EOF or read error — connection is done
                Ok(_) => {
                    if let Ok(step) = decode_line::<Step>(&line) {
                        *reader_step.lock().unwrap() = Some(step);
                    }
                    // A malformed line rejects just that one Step, keeping the connection alive
                    // (spec §11) — there is nothing to reply with here since Observation, not an
                    // ack, is the only outbound message shape.
                }
            }
        }
    });

    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(TICK_MS));
    loop {
        ticker.tick().await;
        if reader_task.is_finished() {
            break;
        }
        if let Some(step) = latest_step.lock().unwrap().clone() {
            apply_step(&step, &camera, &command);
        }
        let gs: arc_swap::Guard<Arc<GameState>> = game_state.load();
        let obs = build_observation(&gs, &shared_collision, &spells, &net_thread_dead);
        let line = encode_line(&obs).expect("Observation always serializes");
        if write_half.write_all(line.as_bytes()).await.is_err() {
            break;
        }
    }
    reader_task.abort();
}
```

and add to `#[cfg(test)] mod tests`:

```rust
    use eqoxide_agent_protocol::handshake::{HandshakeReply, Hello};
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn run_pushes_an_observation_every_tick_after_handshake() {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let camera = CameraSlots::default();
        let command = CommandState::default();
        let game_state: GameStateSnapshot = Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default()));
        let shared_collision: SharedCollision = Arc::new(std::sync::RwLock::new(None));
        let spells = Arc::new(SpellDb::default());
        let net_thread_dead: NetThreadDeadShared = Arc::new(Mutex::new(None));

        let server_task = tokio::spawn(run(
            server, camera, command, game_state, shared_collision, spells, net_thread_dead,
        ));

        let (read_half, mut write_half) = client.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        write_half
            .write_all(encode_line(&Hello { protocol_version: PROTOCOL_VERSION }).unwrap().as_bytes())
            .await
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let reply: HandshakeReply = decode_line(&line).unwrap();
        assert_eq!(reply, HandshakeReply::Accepted);

        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let _obs: eqoxide_agent_protocol::observation::Observation = decode_line(&line).unwrap();

        drop(write_half);
        drop(reader);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), server_task).await;
    }

    #[tokio::test]
    async fn run_closes_immediately_on_handshake_rejection() {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let camera = CameraSlots::default();
        let command = CommandState::default();
        let game_state: GameStateSnapshot = Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default()));
        let shared_collision: SharedCollision = Arc::new(std::sync::RwLock::new(None));
        let spells = Arc::new(SpellDb::default());
        let net_thread_dead: NetThreadDeadShared = Arc::new(Mutex::new(None));

        let server_task = tokio::spawn(run(
            server, camera, command, game_state, shared_collision, spells, net_thread_dead,
        ));

        let (read_half, mut write_half) = client.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        write_half
            .write_all(encode_line(&Hello { protocol_version: PROTOCOL_VERSION + 1 }).unwrap().as_bytes())
            .await
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let reply: HandshakeReply = decode_line(&line).unwrap();
        assert!(matches!(reply, HandshakeReply::Rejected { .. }));

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), server_task).await;
        assert!(result.is_ok(), "run() must return promptly after a rejected handshake");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-plugin-host --features test-fixtures session::run`
Expected: FAIL — `run` doesn't exist yet (compile error).

- [ ] **Step 3: Implementation is already written above (Step 1) — nothing further to add.**

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-plugin-host --features test-fixtures session::`
Expected: PASS — all session tests (14 from Tasks 14-16 + 2 new) green. Add `arc-swap = "1"` to `crates/eqoxide-agent-plugin-host/Cargo.toml`'s `[dependencies]` if the compiler reports it's missing (it's needed for `GameStateSnapshot`'s `.load()` call and the test's `ArcSwap::from_pointee`).

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-plugin-host/src/session.rs crates/eqoxide-agent-plugin-host/Cargo.toml
git commit -m "feat: add the per-connection async-duplex tick loop"
```

---

## Task 18: Socket accept loop

**Files:**
- Modify: `crates/eqoxide-agent-plugin-host/src/lib.rs`

**Interfaces:**
- Consumes: `session::run` (Task 17), `tokio::net::UnixListener`.
- Produces: `spawn_agent_plugin_host`'s body is replaced with the real accept loop — this is the crate's finished public entry point, unchanged signature from Task 11.

**Design decision:** one connection served at a time, sequentially — `listener.accept()` is only called again after the previous `session::run` returns. A brief disconnect-then-reconnect (spec §10) is served by the next `accept()` once the old connection's read hits EOF/error and `run` returns; there is no per-connection state to hand off because the character/game state itself is the one persistent thing, not anything session.rs owns.

- [ ] **Step 1: Write the failing test**

Change `crates/eqoxide-agent-plugin-host/src/lib.rs` to:

```rust
//! In-client plumbing for the Agent Plugin API (spec §4): owns the Unix domain socket server,
//! performs the version handshake, latches each `Step` into the existing `CameraSlots`/
//! `CommandState` mailboxes, and pushes an `Observation` every tick. Deliberately thin — no
//! policy/decision logic lives here (spec §4, §12).

pub mod legal_actions;
pub mod observation_builder;
pub mod session;

use eqoxide_command::CommandState;
use eqoxide_core::spells::SpellDb;
use eqoxide_ipc::{CameraSlots, GameStateSnapshot, NetThreadDeadShared};
use eqoxide_nav::collision::SharedCollision;
use std::path::PathBuf;
use std::sync::Arc;

/// Bind the agent socket and start accepting connections on its own thread (mirrors
/// `eqoxide_http::spawn_camera_server`'s own-thread-plus-own-tokio-runtime pattern). One
/// connection is served at a time (spec §10) — see this module's doc comment / this task's plan
/// entry for why that's the right choice for a single-character session.
pub fn spawn_agent_plugin_host(
    camera: CameraSlots,
    command: CommandState,
    game_state: GameStateSnapshot,
    shared_collision: SharedCollision,
    spells: Arc<SpellDb>,
    net_thread_dead: NetThreadDeadShared,
    socket_path: PathBuf,
) {
    std::thread::Builder::new()
        .name("agent-plugin-host".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("agent-plugin-host tokio runtime");
            rt.block_on(async move {
                if socket_path.exists() {
                    let _ = std::fs::remove_file(&socket_path);
                }
                let listener = match tokio::net::UnixListener::bind(&socket_path) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!("agent-plugin-host: failed to bind {}: {e}", socket_path.display());
                        return;
                    }
                };
                tracing::info!("agent-plugin-host: listening on {}", socket_path.display());
                loop {
                    let (stream, _addr) = match listener.accept().await {
                        Ok(pair) => pair,
                        Err(e) => {
                            tracing::warn!("agent-plugin-host: accept failed: {e}");
                            continue;
                        }
                    };
                    tracing::info!("agent-plugin-host: agent connected");
                    session::run(
                        stream,
                        camera.clone(),
                        command.clone(),
                        game_state.clone(),
                        shared_collision.clone(),
                        spells.clone(),
                        net_thread_dead.clone(),
                    )
                    .await;
                    tracing::info!("agent-plugin-host: agent disconnected");
                }
            });
        })
        .expect("spawn agent-plugin-host thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_core::game_state::GameState;

    #[tokio::test]
    async fn spawn_agent_plugin_host_binds_a_socket_file_that_accepts_a_connection() {
        let dir = std::env::temp_dir().join(format!("eqoxide-agent-host-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("test.sock");

        spawn_agent_plugin_host(
            CameraSlots::default(),
            CommandState::default(),
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default())),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(SpellDb::default()),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            socket_path.clone(),
        );

        // Give the spawned thread a moment to bind. Polling instead of a fixed sleep so this isn't
        // flaky under load.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !socket_path.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(socket_path.exists(), "the socket file must exist once the thread has bound it");

        let connected = tokio::net::UnixStream::connect(&socket_path).await;
        assert!(connected.is_ok(), "a client must be able to connect to the bound socket");

        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_dir(&dir);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p eqoxide-agent-plugin-host --features test-fixtures spawn_agent_plugin_host`
Expected: FAIL — this exact test doesn't exist yet against the Task-11 stub body (the stub never actually reaches a bound-and-listening state observable this way — confirm red by running against the pre-edit `lib.rs` from Task 11, which has no accept loop for a client to connect to, so the `connected.is_ok()` assertion fails).

- [ ] **Step 3: Implementation is already written above (Step 1) — nothing further to add.**

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p eqoxide-agent-plugin-host --features test-fixtures`
Expected: PASS — every test in the crate (legal_actions, observation_builder, session, and this new lib-level test) green.

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-agent-plugin-host/src/lib.rs
git commit -m "feat: add the sequential socket accept loop"
```

---

## Task 19: `main.rs` wiring

**Files:**
- Modify: `src/main.rs`
- Modify: `Cargo.toml` (workspace root) — add `eqoxide-agent-plugin-host` to the root binary's `[dependencies]`

**Interfaces:**
- Consumes: `eqoxide_agent_plugin_host::spawn_agent_plugin_host` (Task 18).
- Produces: a running `eqoxide` binary that also serves the agent socket. No new public interface — this is the final integration point.

- [ ] **Step 1: Add the dependency**

In `Cargo.toml` (workspace root), add to `[dependencies]` (near the other `eqoxide-*` path deps, e.g. after line 30's `eqoxide-ui`):

```toml
eqoxide-agent-plugin-host = { path = "crates/eqoxide-agent-plugin-host" }
```

- [ ] **Step 2: Run to verify the binary still builds with the new dependency present but unused**

Run: `cargo build`
Expected: PASS (with an "unused import" style situation only once Step 3 partially wires it — at this exact point nothing references the new crate yet, so this step just confirms the dependency itself resolves).

- [ ] **Step 3: Add the CLI flag**

In `src/main.rs`, add `--agent-socket` to `USAGE` (after the `--api-port` block, before `-h, --help`):

```rust
    --agent-socket <PATH>  Bind the Agent Plugin API to this Unix socket path, instead of the
                           default `$TMPDIR/eqoxide-agent-<pid>.sock`.
```

Add the field to `CliArgs` (line 42-48):

```rust
struct CliArgs {
    testzone: bool,
    profile: bool,
    nav_debug: bool,
    config: Option<String>,
    api_port: Option<u16>,
    agent_socket: Option<std::path::PathBuf>,
}
```

Add parsing in `parse_cli` (after the `--api-port` arm, line 89-108):

```rust
            // accept both "--agent-socket <value>" and "--agent-socket=<value>"
            _ if arg == "--agent-socket" || arg.starts_with("--agent-socket=") => {
                let value = if let Some(v) = arg.strip_prefix("--agent-socket=") {
                    v.to_string()
                } else {
                    match args.get(idx + 1) {
                        Some(v) if !v.starts_with('-') => { idx += 1; v.clone() }
                        _ => {
                            eprintln!("error: --agent-socket requires a value (a filesystem path)\n\n{USAGE}");
                            eqoxide::crash::exit("bad-args", 2);
                        }
                    }
                };
                if value.is_empty() {
                    eprintln!("error: --agent-socket requires a non-empty value\n\n{USAGE}");
                    eqoxide::crash::exit("bad-args", 2);
                }
                agent_socket_arg = Some(std::path::PathBuf::from(value));
            }
```

and add the local variable declaration alongside `api_port_arg` (line 60):

```rust
    let mut agent_socket_arg: Option<std::path::PathBuf> = None;
```

and add it to the `CliArgs { ... }` construction at the end of `parse_cli` (line 116-122):

```rust
    CliArgs {
        testzone: testzone_mode,
        profile: profile_flag,
        nav_debug: nav_debug_flag,
        config: login_cfg_arg,
        api_port: api_port_arg,
        agent_socket: agent_socket_arg,
    }
```

- [ ] **Step 4: Run to verify it fails to compile until the call site is added**

Run: `cargo build`
Expected: FAIL — `CliArgs` now has an `agent_socket` field with no corresponding use, which is fine (no error from an unused struct field alone), but the real red signal here is functional, not compiler-enforced: without Step 5's call, `--agent-socket` silently does nothing. Confirm the CLI parses by running `cargo run -- --agent-socket /tmp/test.sock --testzone --help` and checking `--agent-socket` appears in the printed `USAGE` text (it will, from Step 3's edit) — this step is a manual smoke check, not an automated test, since `main()` itself has no test harness in this codebase.

- [ ] **Step 5: Wire the spawn call**

In `src/main.rs`, just before the existing `http::spawn_camera_server(` call (line 593), add a clone of `net_thread_dead` (which is otherwise moved into that call at line 601 and would no longer exist afterward) and resolve the socket path:

```rust
    // Cloned here because `net_thread_dead` (below) is MOVED into `http::spawn_camera_server`.
    let net_thread_dead_for_agent = net_thread_dead.clone();
    let agent_socket_path = cli.agent_socket.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("eqoxide-agent-{}.sock", std::process::id()))
    });
```

Then, immediately after the existing `http::spawn_camera_server(...)` call closes (after line 621's `);`), add:

```rust
    eqoxide_agent_plugin_host::spawn_agent_plugin_host(
        camera.clone(),
        command.clone(),
        game_state_snapshot.clone(),
        shared_collision.clone(),
        spells.clone(),
        net_thread_dead_for_agent,
        agent_socket_path,
    );
```

- [ ] **Step 6: Run to verify it passes**

Run: `cargo build && cargo run -- --testzone --agent-socket /tmp/eqoxide-agent-plan-check.sock &` then, after a few seconds, check the socket file exists and accepts a raw connection:

```bash
sleep 3
test -S /tmp/eqoxide-agent-plan-check.sock && echo "socket bound"
kill %1
rm -f /tmp/eqoxide-agent-plan-check.sock
```

Expected: `cargo build` succeeds across the whole workspace; the running `--testzone` instance binds the socket file (confirmed by `test -S` reporting it's a socket).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml src/main.rs
git commit -m "feat: wire --agent-socket CLI flag and spawn the agent plugin host"
```

---

## Task 20: Live-verification example client

**Files:**
- Create: `crates/eqoxide-agent-protocol/examples/fixed_sequence_client.rs`
- Modify: `crates/eqoxide-agent-protocol/Cargo.toml` (dev-dependency needed only for the example's blocking socket I/O — none, `std::os::unix::net::UnixStream` is stdlib)

**Interfaces:**
- Consumes: every public type in `eqoxide-agent-protocol` (`handshake`, `movement`, `verb`, `step`, `observation`, `framing`).
- Produces: a runnable binary exercising the protocol end-to-end against a live `eqoxide --testzone --agent-socket <path>` instance (spec §13's "Live verification").

This task's "test" is the manual verification run itself, matching how `docs/dev-workflow.md` already documents manually verifying the HTTP API — there is no automated assertion here because it requires a live client instance with a real zone loaded.

- [ ] **Step 1: Write the client**

Create `crates/eqoxide-agent-protocol/examples/fixed_sequence_client.rs`:

```rust
//! Minimal fixed-action-sequence client — NOT the real agent, just enough to exercise the protocol
//! end-to-end against a live eqoxide instance (spec §13). Run:
//!
//! ```text
//! cargo run -p eqoxide-agent-protocol --example fixed_sequence_client -- /path/to/agent.sock
//! ```
//!
//! against an instance started with `cargo run -- --testzone --agent-socket /path/to/agent.sock`.
//! Sends a short scripted sequence of `Step`s and prints each `Observation` received in between.

use eqoxide_agent_protocol::framing::{decode_line, encode_line};
use eqoxide_agent_protocol::handshake::{HandshakeReply, Hello, PROTOCOL_VERSION};
use eqoxide_agent_protocol::movement::AgentMovement;
use eqoxide_agent_protocol::observation::Observation;
use eqoxide_agent_protocol::step::Step;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

fn read_line(reader: &mut impl BufRead) -> String {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read line from socket");
    line
}

fn main() {
    let socket_path = std::env::args().nth(1).expect("usage: fixed_sequence_client <socket path>");
    let stream = UnixStream::connect(&socket_path).expect("connect to agent socket");
    let mut writer = stream.try_clone().expect("clone stream for writing");
    let mut reader = BufReader::new(stream);

    writer
        .write_all(encode_line(&Hello { protocol_version: PROTOCOL_VERSION }).unwrap().as_bytes())
        .expect("send Hello");

    let reply: HandshakeReply = decode_line(&read_line(&mut reader)).expect("parse HandshakeReply");
    match reply {
        HandshakeReply::Accepted => println!("handshake accepted"),
        HandshakeReply::Rejected { server_protocol_version, message } => {
            eprintln!("handshake rejected: server={server_protocol_version} message={message}");
            std::process::exit(1);
        }
    }

    // A short scripted sequence: stand still (read one Observation), walk east for a beat, stop.
    let sequence: Vec<Step> = vec![
        Step { movement: None, verb: None },
        Step {
            movement: Some(AgentMovement { dir: [1.0, 0.0], up: 0.0, jump: false, wish_heading: None }),
            verb: None,
        },
        Step { movement: Some(AgentMovement { dir: [0.0, 0.0], up: 0.0, jump: false, wish_heading: None }), verb: None },
    ];

    for (i, step) in sequence.iter().enumerate() {
        writer.write_all(encode_line(step).unwrap().as_bytes()).expect("send Step");
        let obs: Observation = decode_line(&read_line(&mut reader)).expect("parse Observation");
        println!(
            "step {i}: pos={:?} hp={}/{} zone={} dead={} terminated={} visible={}",
            obs.own.pos, obs.own.hp, obs.own.hp_max, obs.own.zone_name, obs.dead, obs.terminated, obs.visible.len()
        );
    }
}
```

- [ ] **Step 2: Confirm it builds**

Run: `cargo build -p eqoxide-agent-protocol --examples`
Expected: PASS — the example compiles (it cannot be functionally exercised without a live instance, so there is no automated pass/fail beyond compilation).

- [ ] **Step 3: Manually verify against a live instance**

In one terminal:

```bash
cargo run -- --testzone --agent-socket /tmp/eqoxide-agent-verify.sock
```

In a second terminal, once the first has finished loading:

```bash
cargo run -p eqoxide-agent-protocol --example fixed_sequence_client -- /tmp/eqoxide-agent-verify.sock
```

Expected: the client prints `handshake accepted` followed by three `step N: ...` lines, each with sensible `pos`/`hp`/`zone` values reflecting the `--testzone` character. Confirm manually — this is the spec's "Live verification" (§13), not an automated test.

- [ ] **Step 4: Commit**

```bash
git add crates/eqoxide-agent-protocol/examples
git commit -m "feat: add fixed_sequence_client example for live protocol verification"
```

---

## Self-Review

**Spec coverage** (against `docs/specs/2026-09-15-agent-plugin-api-design.md`):
- §4 Architecture (3 crates, layering) — Tasks 1, 8, 11.
- §5 Transport/cadence (Unix socket, ~150ms tick, async duplex, version handshake) — Tasks 2, 16, 17.
- §6 Visibility (distance + LOS, `wish_heading`) — Tasks 8, 9, 10.
- §7 Movement (continuous `ManualMove`+`wish_heading`, held-key latching) — Tasks 10, 15.
- §8 Action space (`AgentVerb`, all 8 groups, 3 wired + 5 reserved) — Tasks 4, 5, 6, 14.
- §9 Observation space (own state, visible entities, legal-action mask, dead/terminated/truncated) — Tasks 7, 12, 13. Endurance, buffs, and per-ability mana cost/cast time/recast delay are included (#1127 landed as `bfd7d653`/#1128 before this plan's rebase onto main) — no field named in spec §9 is deferred.
- §10 Reset/session semantics (reconnect resumes the same session) — Task 18 (sequential accept loop; no per-connection state to lose).
- §11 Error handling (malformed action rejected per-slot, connection stays alive; version mismatch rejected at handshake) — Tasks 16, 17 (reader task drops a bad line and keeps looping; handshake rejects and returns).
- §12 Scope boundaries (Goto/Follow permanently excluded, reserved verbs typed-not-wired) — enforced structurally by `AgentVerb`'s shape (Tasks 5, 6) and `dispatch_verb`'s no-op arms (Task 14); stated explicitly in Global Constraints.
- §13 Testing (protocol round-trips, vision-filter fixtures, plugin-host integration tests, live example) — Tasks 1-9 (unit), 14-18 (integration), 20 (live).
- §14 Open questions — no task needed; explicitly future work, not this plan's scope.

**Placeholder scan:** no `TBD`/`TODO`/"add error handling" phrasing appears in any task's Steps. The one deliberate no-op (`AgentVerb::Move(_) => {}` in Task 14) is fully specified behavior (spec §12), not an unfinished stub — it's explained inline and its test (`reserved_move_zone_cross_is_a_deliberate_no_op`) asserts the no-op explicitly rather than leaving it untested. Task 11's `let _ = (...)` line is flagged as temporary and is explicitly replaced by Task 18's real accept loop.

**Type consistency:** `ManualMove` (Task 10) → `AgentMovement`'s fields (Task 3) match 1:1 (`dir`, `up`, `jump`, `wish_heading`). `Step.verb: Option<AgentVerb>` (Task 4) is consumed identically in `dispatch_verb` (Task 14) and `apply_step` (Task 15). `Observation`/`OwnState`/`VisibleEntity`/`LegalActionMask`/`AbilityFeature` (Task 7) are constructed with matching field names in `observation_builder` (Task 13) and `eqoxide-agent-vision-filter` (Tasks 8-9). `spawn_agent_plugin_host`'s signature is identical across Task 11 (initial) and Task 18 (final) — only the body changes. `CommandState::request_target/request_attack/request_consider/request_cast/request_sit/request_respawn` signatures used in Task 14 are copied verbatim from `crates/eqoxide-command/src/{combat,interact,lifecycle}.rs`, confirmed by direct grep during this plan's research (not inferred).

**Known follow-ups an implementer should watch for** (named explicitly rather than left implicit, since they depend on exact method names not fully confirmed during planning):
- Task 12 assumes `SpellDb` needs a `insert_for_test` test-fixture constructor added — check first whether one already exists under a different name.
- Task 13 assumes a `make_entity` test-fixture constructor exists in `eqoxide-core::game_state` — confirm its real signature before use.
- Task 14 infers `CommandState`'s `take_target`/`take_attack`/`take_cast`/`take_sit` drain-method names from the crate's own documented `take_<thing>()` convention (`crates/eqoxide-command/src/lib.rs:66`) rather than from a direct read of each one — confirm exact names against the source before writing the test calls.

---

**Plan complete and saved to `docs/specs/2026-09-15-agent-plugin-api-design-plan.md`.** Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints

**Which approach?**
