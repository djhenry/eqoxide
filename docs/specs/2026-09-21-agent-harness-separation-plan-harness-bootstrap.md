# Harness Repo Bootstrap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (recommended for
> this plan — see the note in Global Constraints on why subagent-driven-development is a poor fit
> here) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up `eqoxide-agent-harness-example-http-astar` as a genuinely separate, public
GitHub repository — a Cargo workspace of four crates that talk to a running eqoxide only over the
Agent Plugin API's Unix domain socket — with each crate carrying one small, real, tested vertical
slice rather than a stub, and CI proving the whole thing builds against eqoxide's current pinned
commit.

**Architecture:** A pure virtual Cargo workspace (`crates/harness-socket`, `crates/harness-nav`,
`crates/harness-combat`, `crates/harness-http`), each depending on eqoxide only through the three
crates eqoxide already publishes as ordinary crates.io-shaped dependencies — pinned as **git**
dependencies at one fixed commit, since none of this exists on eqoxide's `main` branch yet. No
crate in this workspace ever imports `eqoxide-core`, `eqoxide-ipc`, `eqoxide-nav`, `eqoxide-http`,
or any other in-process eqoxide type — the isolation boundary is the whole point of §10.

**Tech Stack:** Rust (edition 2021), `eqoxide-agent-protocol` (wire types), `eqoxide-assets` +
`eqoxide-zone-geometry` (zone geometry), `axum` + `tower` (harness-http), GitHub Actions (CI).

**Spec:** `docs/specs/2026-09-21-agent-harness-separation-design.md` §10 (New Project), §11 (Zone
Asset Access), §13 (Relationship to PR #1131 and Rollout). This plan implements §10's "Anticipated
shape" as a bootstrap only — see Global Constraints for exactly what is and isn't in scope.

## Global Constraints

- **Target repository:** public GitHub repo `djhenry/eqoxide-agent-harness-example-http-astar`,
  confirmed with the project owner (not a guess — creating a new repo is an external, hard-to-reverse
  action and was checked before this plan was written).
- **Local clone path:** `~/git/eqoxide-agent-harness-example-http-astar`, a sibling directory of
  this eqoxide checkout (`~/git/eqoxide`). Every task below assumes commands run from inside that
  clone unless stated otherwise.
- **The git-dependency pin.** Every crate in this workspace that depends on eqoxide-published
  crates does so via `git = "https://github.com/djhenry/eqoxide.git", rev =
  "d9075fe6b4652f84c66d6b58e0a85c4f6ca2a8ba"` — the commit this plan was written against
  (`worktree-rl-api-design` branch; `eqoxide-zone-geometry` does not exist on `main` yet, so pinning
  to `main` is not an option today). **Every single git dependency on that URL, across every crate
  in this workspace, MUST use this exact same `rev` string.** Cargo resolves a git URL as one source
  keyed by its rev; two crates in the same build pinning the same URL at two different revs is a
  hard `cargo` error ("failed to merge sources"), not a style issue. When eqoxide's branch gains
  commits the harness needs, bump every occurrence of this `rev` together, in one commit (Task 8
  documents the procedure).
- **Dependency allow-list.** No crate under `crates/` in this workspace may depend — directly or by
  adding a new git dependency — on any eqoxide crate other than `eqoxide-agent-protocol`,
  `eqoxide-assets`, or `eqoxide-zone-geometry`. This is what §10 means by "no in-process coupling";
  Task 9 greps for a violation as part of final verification. Note `eqoxide-zone-geometry` pulls in
  `eqoxide-core` as its own internal `path` dependency (geometry math needs eqoxide-core's
  coordinate types) — that crate will legitimately show up in `cargo tree`. The constraint is about
  what harness *source code* imports directly, not the full transitive build graph; Task 9's grep
  checks `crates/` (this workspace's own source), not `cargo tree` output.
- **Scope: bootstrap, not a port.** `eqoxide-nav`'s planner/walker/steering/traversability modules
  total ~19,000 lines; `eqoxide-http`'s route handlers total ~20,000 lines. Porting either into this
  workspace is explicitly **out of scope for this plan** — each of the four crates below gets one
  small, real, independently-testable vertical slice that proves its place in the dependency graph
  works, not a full reimplementation. Full porting is deferred to dedicated follow-up plans (one for
  `harness-nav`'s A* planner, one for `harness-http`'s route surface) that this plan does not write.
- **No LICENSE file.** The parent `eqoxide` repository ships none either; adding one here is a
  licensing decision for the project owner to make explicitly, not something to default into via a
  bootstrap plan.
- **Why executing-plans over subagent-driven-development for this plan specifically:**
  subagent-driven-development's setup step assumes a single git worktree that later tasks' diffs
  can be reviewed against. This plan's very first task creates an entirely different git repository
  in a different local directory — there is no shared worktree for a task reviewer to diff against,
  and every later task depends on exact file layout decisions the immediately preceding task made
  (crate names, module paths) in that same fresh repo. Executing inline, task by task, in this same
  session avoids re-deriving that context for a series of fresh subagents.

---

### Task 1: Create the repository and baseline scaffold

**Files:**
- Create (new repo root): `README.md`, `.gitignore`

**Interfaces:**
- Produces: the repository itself at `https://github.com/djhenry/eqoxide-agent-harness-example-http-astar`, cloned locally at `~/git/eqoxide-agent-harness-example-http-astar`, with an initial commit on its default branch.

- [ ] **Step 1: Create the repository and clone it**

Run from `~/git`:

```bash
cd ~/git
gh repo create djhenry/eqoxide-agent-harness-example-http-astar \
  --public \
  --description "Reference agent harness for eqoxide: A* navigation + combat over the Agent Plugin API socket, as a genuinely external process" \
  --clone
cd eqoxide-agent-harness-example-http-astar
```

- [ ] **Step 2: Add `.gitignore`**

```
/target
Cargo.lock
```

Wait — this workspace ships binaries indirectly used by others (a reference implementation people
run), so commit the lockfile instead of ignoring it. Use this `.gitignore` instead:

```
/target
```

- [ ] **Step 3: Write `README.md`**

```markdown
# eqoxide-agent-harness-example-http-astar

[![CI](https://github.com/djhenry/eqoxide-agent-harness-example-http-astar/actions/workflows/ci.yml/badge.svg)](https://github.com/djhenry/eqoxide-agent-harness-example-http-astar/actions/workflows/ci.yml)

A **reference** agent harness for [eqoxide](https://github.com/djhenry/eqoxide): A* navigation,
chase-and-face combat, and an optional HTTP convenience wrapper, all driving a running eqoxide
instance from a genuinely separate OS process over the [Agent Plugin
API](https://github.com/djhenry/eqoxide/blob/main/docs/agent-api.md)'s Unix domain socket only —
no shared memory, no in-process coupling. See
[`docs/specs/2026-09-21-agent-harness-separation-design.md`](https://github.com/djhenry/eqoxide/blob/main/docs/specs/2026-09-21-agent-harness-separation-design.md)
§10 in the eqoxide repo for the design this implements.

This is a reference implementation, not a privileged first-party client — any other agent-harness
project would integrate the same way: connect to `--agent-socket <path>`, handshake, and drive the
character with `Step`/`Observation`.

## Status

This repo is a **bootstrap**: each crate below has one small, real, tested vertical slice. Full
A* planner porting (`harness-nav`) and full HTTP route porting (`harness-http`) are tracked as
follow-up work — see [`docs/scope.md`](docs/scope.md) for exactly what's implemented today versus
deferred.

## Crates

| Crate | Purpose |
|---|---|
| `harness-socket` | Agent Plugin API client — handshake, send `Step`, receive `Observation` |
| `harness-nav` | Zone geometry loading, built on eqoxide's shared `eqoxide-zone-geometry` crate |
| `harness-combat` | Chase-and-face-while-engaged, computed from raw `Observation` data |
| `harness-http` | Optional HTTP convenience wrapper around `harness-socket` |

## Running against a live eqoxide instance

```bash
# in the eqoxide checkout
cargo run -- --testzone --agent-socket /tmp/eqoxide-agent.sock

# in this repo
cargo run -p harness-http
```

## Dependency pinning

This workspace depends on `eqoxide-agent-protocol`, `eqoxide-assets`, and `eqoxide-zone-geometry`
as **git** dependencies pinned to one commit on eqoxide's `worktree-rl-api-design` branch (that
work is not on `main` yet — see eqoxide spec §13). See
[`docs/scope.md`](docs/scope.md#bumping-the-eqoxide-pin) for the bump procedure.
```

- [ ] **Step 4: Commit and push**

```bash
git add README.md .gitignore
git commit -m "$(cat <<'EOF'
chore: initial repo scaffold

Reference agent harness for eqoxide (docs/specs/2026-09-21-agent-harness-separation-design.md
§10 in the eqoxide repo): a genuinely separate process driving eqoxide only over the Agent
Plugin API's Unix domain socket.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
git push
```

---

### Task 2: Cargo workspace skeleton with pinned git dependencies

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `crates/harness-socket/Cargo.toml`, `crates/harness-socket/src/lib.rs`
- Create: `crates/harness-nav/Cargo.toml`, `crates/harness-nav/src/lib.rs`
- Create: `crates/harness-combat/Cargo.toml`, `crates/harness-combat/src/lib.rs`
- Create: `crates/harness-http/Cargo.toml`, `crates/harness-http/src/lib.rs`, `crates/harness-http/src/main.rs`

**Interfaces:**
- Produces: four workspace member crates (`harness-socket`, `harness-nav`, `harness-combat`,
  `harness-http`) that all resolve and compile against eqoxide's pinned commit, with the exact
  dependency graph §10 specifies. Later tasks fill in real bodies; this task only needs to compile.

- [ ] **Step 1: Write the workspace root `Cargo.toml`**

```toml
[workspace]
resolver = "2"
members = [
    "crates/harness-socket",
    "crates/harness-nav",
    "crates/harness-combat",
    "crates/harness-http",
]
```

- [ ] **Step 2: `harness-socket`**

`crates/harness-socket/Cargo.toml`:

```toml
[package]
name = "harness-socket"
version = "0.1.0"
edition = "2021"

[lib]
name = "harness_socket"
path = "src/lib.rs"

[dependencies]
# Zero-eqoxide-internal-dependency wire types (eqoxide's own docs/agent-api.md: "zero dependency on
# any other eqoxide crate") — this is the one crate in this workspace that talks to eqoxide at all.
eqoxide-agent-protocol = { git = "https://github.com/djhenry/eqoxide.git", rev = "d9075fe6b4652f84c66d6b58e0a85c4f6ca2a8ba" }
serde_json = "1"
```

`crates/harness-socket/src/lib.rs`:

```rust
//! Agent Plugin API client (eqoxide spec §10) — connects to a running eqoxide instance's
//! `--agent-socket` Unix domain socket, performs the version handshake, and exchanges `Step`/
//! `Observation` over it. Filled in by a later task in this plan.
```

- [ ] **Step 3: `harness-nav`**

`crates/harness-nav/Cargo.toml`:

```toml
[package]
name = "harness-nav"
version = "0.1.0"
edition = "2021"

[lib]
name = "harness_nav"
path = "src/lib.rs"

[dependencies]
# The shared geometry-grid construction/query layer (eqoxide spec §8) plus the asset loader it's
# built from — both pinned to the exact same commit as every other git dependency on this URL in
# this workspace (see Global Constraints: Cargo treats one git URL as one source keyed by rev).
eqoxide-zone-geometry = { git = "https://github.com/djhenry/eqoxide.git", rev = "d9075fe6b4652f84c66d6b58e0a85c4f6ca2a8ba" }
eqoxide-assets = { git = "https://github.com/djhenry/eqoxide.git", rev = "d9075fe6b4652f84c66d6b58e0a85c4f6ca2a8ba" }
anyhow = "1"

[dev-dependencies]
# test-fixtures exposes ZoneAssetState::test_ready() — a real Ready state over a flat-floor
# Collision — to downstream test builds; it's #[cfg(any(test, feature = "test-fixtures"))] in
# eqoxide-zone-geometry, so invisible across the crate boundary otherwise (mirrors the exact
# pattern eqoxide-nav's own Cargo.toml uses for the same feature, in the eqoxide repo).
eqoxide-zone-geometry = { git = "https://github.com/djhenry/eqoxide.git", rev = "d9075fe6b4652f84c66d6b58e0a85c4f6ca2a8ba", features = ["test-fixtures"] }
```

`crates/harness-nav/src/lib.rs`:

```rust
//! Zone geometry loading (eqoxide spec §10) — builds an `eqoxide_zone_geometry::collision::Collision`
//! from a zone's asset files, using the same shared geometry-grid crate eqoxide itself uses. A* path
//! planning on top of this geometry is a follow-up plan (see docs/scope.md), not this crate today.

pub mod zone;
```

`crates/harness-nav/src/zone.rs` (create this file too — add it to the Files list mentally, it's
part of this task's deliverable):

```rust
//! Placeholder for Task 4, which fills in `load_zone`.
```

- [ ] **Step 4: `harness-combat`**

`crates/harness-combat/Cargo.toml`:

```toml
[package]
name = "harness-combat"
version = "0.1.0"
edition = "2021"

[lib]
name = "harness_combat"
path = "src/lib.rs"

[dependencies]
# Only the wire types (OwnState/VisibleEntity/AgentMovement) — chase-and-face computes range and
# bearing itself from raw Observation data (eqoxide spec §7.2), exactly what a human player derives
# by looking at their screen, with no special access to eqoxide internals.
eqoxide-agent-protocol = { git = "https://github.com/djhenry/eqoxide.git", rev = "d9075fe6b4652f84c66d6b58e0a85c4f6ca2a8ba" }
```

`crates/harness-combat/src/lib.rs`:

```rust
//! Chase-and-face-while-engaged (eqoxide spec §7.2) — reimplemented on raw AgentMovement +
//! Observation, no special access to eqoxide internals. Filled in by a later task in this plan.
```

- [ ] **Step 5: `harness-http`**

`crates/harness-http/Cargo.toml`:

```toml
[package]
name = "harness-http"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "harness-http"
path = "src/main.rs"

[lib]
name = "harness_http"
path = "src/lib.rs"

[dependencies]
harness-socket = { path = "../harness-socket" }
axum = "0.7"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[dev-dependencies]
# "util" gates tower::ServiceExt (the oneshot() call harness-http's health-check test uses below) —
# off by default, and axum 0.7's own testing docs use exactly this feature/version pairing.
tower = { version = "0.4", features = ["util"] }
```

`crates/harness-http/src/lib.rs`:

```rust
//! Optional HTTP convenience wrapper around harness-socket (eqoxide spec §10) — talks to
//! harness-socket, never to eqoxide directly. Filled in by a later task in this plan.
```

`crates/harness-http/src/main.rs`:

```rust
//! Placeholder binary entry point — Task 6 fills this in.
fn main() {}
```

- [ ] **Step 6: Verify the workspace builds**

```bash
cargo build --workspace
```

Expected: succeeds, fetching the three pinned crates from `https://github.com/djhenry/eqoxide.git`
at `d9075fe6b4652f84c66d6b58e0a85c4f6ca2a8ba`. This is the checkpoint that proves the whole
dependency graph in Global Constraints is consistent — if it fails with a source-merge error, some
crate's `rev` doesn't match the others exactly.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/ Cargo.lock
git commit -m "$(cat <<'EOF'
feat: workspace skeleton with pinned eqoxide git dependencies

Four crates (harness-socket, harness-nav, harness-combat, harness-http) matching the dependency
graph in eqoxide spec §10, all pinned to the same commit on eqoxide's worktree-rl-api-design
branch. No real logic yet — this task only needs cargo build --workspace to succeed.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
git push
```

---

### Task 3: `harness-socket` — a real Agent Plugin API client

**Files:**
- Modify: `crates/harness-socket/src/lib.rs`

**Interfaces:**
- Consumes: `eqoxide_agent_protocol::framing::{encode_line, decode_line}`,
  `eqoxide_agent_protocol::handshake::{Hello, HandshakeReply, PROTOCOL_VERSION}`,
  `eqoxide_agent_protocol::step::Step`, `eqoxide_agent_protocol::observation::Observation`.
- Produces: `harness_socket::AgentClient` with `connect(path: impl AsRef<std::path::Path>) ->
  Result<Self, ClientError>`, `send_step(&mut self, step: &Step) -> Result<(), ClientError>`,
  `recv_observation(&mut self) -> Result<Observation, ClientError>`. Later tasks (harness-http) use
  this exact API.

- [ ] **Step 1: Write the failing test**

Replace `crates/harness-socket/src/lib.rs` with:

```rust
//! Agent Plugin API client (eqoxide spec §10) — connects to a running eqoxide instance's
//! `--agent-socket` Unix domain socket, performs the version handshake, and exchanges `Step`/
//! `Observation` over it. Mirrors eqoxide's own
//! `crates/eqoxide-agent-protocol/examples/fixed_sequence_client.rs`, productized as a reusable
//! type instead of a one-shot script.

use eqoxide_agent_protocol::framing::{decode_line, encode_line};
use eqoxide_agent_protocol::handshake::{HandshakeReply, Hello, PROTOCOL_VERSION};
use eqoxide_agent_protocol::observation::Observation;
use eqoxide_agent_protocol::step::Step;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

#[derive(Debug)]
pub enum ClientError {
    Io(std::io::Error),
    Json(serde_json::Error),
    HandshakeRejected { server_protocol_version: u32, message: String },
    ConnectionClosed,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "io error: {e}"),
            ClientError::Json(e) => write!(f, "protocol json error: {e}"),
            ClientError::HandshakeRejected { server_protocol_version, message } => {
                write!(f, "handshake rejected (server_protocol_version={server_protocol_version}): {message}")
            }
            ClientError::ConnectionClosed => write!(f, "connection closed by server"),
        }
    }
}

impl std::error::Error for ClientError {}
impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self { ClientError::Io(e) }
}
impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self { ClientError::Json(e) }
}

/// A connected, handshaken Agent Plugin API session.
pub struct AgentClient {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl AgentClient {
    /// Connect to `path` and perform the version handshake. Returns `Err` on a `Rejected` reply,
    /// an I/O error, or a malformed line — matching the handshake contract in eqoxide's
    /// docs/agent-api.md.
    pub fn connect(_path: impl AsRef<Path>) -> Result<Self, ClientError> {
        unimplemented!("filled in by Step 3 of this task")
    }

    fn handshake(&mut self) -> Result<(), ClientError> {
        let hello = encode_line(&Hello { protocol_version: PROTOCOL_VERSION })?;
        self.writer.write_all(hello.as_bytes())?;

        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(ClientError::ConnectionClosed);
        }
        let reply: HandshakeReply = decode_line(&line)?;
        match reply {
            HandshakeReply::Accepted => Ok(()),
            HandshakeReply::Rejected { server_protocol_version, message } => {
                Err(ClientError::HandshakeRejected { server_protocol_version, message })
            }
        }
    }

    /// Send one `Step`. There is no per-`Step` acknowledgment — its effect shows up in the next
    /// `recv_observation()` call, not a direct reply (eqoxide's docs/agent-api.md).
    pub fn send_step(&mut self, step: &Step) -> Result<(), ClientError> {
        let line = encode_line(step)?;
        self.writer.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Block for the next `Observation` line.
    pub fn recv_observation(&mut self) -> Result<Observation, ClientError> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(ClientError::ConnectionClosed);
        }
        Ok(decode_line(&line)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_agent_protocol::movement::AgentMovement;
    use eqoxide_agent_protocol::observation::{LegalActionMask, OwnState};
    use std::os::unix::net::UnixListener;

    fn canned_observation(tick: u64) -> Observation {
        Observation {
            own: OwnState {
                pos: [1.0, 2.0, 3.0], heading: 0.0, hp: 100, hp_max: 100, hp_verified: true,
                mana: 0, mana_max: 0, endurance: 0, endurance_max: 0, endurance_confirmed: true,
                casting: None, buffs: vec![], zone_name: "qeynos".into(), target_id: None,
                target_name: None, auto_attack: false, sitting: false, held: false,
                player_class: "Warrior".into(), player_level: 1,
            },
            visible: vec![],
            legal_actions: LegalActionMask { gems: [false; 9], abilities: vec![] },
            dead: false, terminated: false, truncated: false, visibility_available: true, tick,
        }
    }

    /// A minimal fake server: accept one connection, handshake Accepted, then echo one
    /// Observation per Step received. Proves AgentClient round-trips against the real wire
    /// framing without needing a live eqoxide instance.
    #[test]
    fn connect_handshakes_and_round_trips_a_step_and_observation() {
        let dir = std::env::temp_dir().join(format!("harness-socket-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("agent.sock");
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = std::thread::spawn({
            let sock_path = sock_path.clone();
            move || {
                let (stream, _) = listener.accept().unwrap();
                let mut writer = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);

                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let _hello: Hello = decode_line(&line).unwrap();
                writer.write_all(encode_line(&HandshakeReply::Accepted).unwrap().as_bytes()).unwrap();

                let mut step_line = String::new();
                reader.read_line(&mut step_line).unwrap();
                let _step: Step = decode_line(&step_line).unwrap();
                writer.write_all(encode_line(&canned_observation(0)).unwrap().as_bytes()).unwrap();
                let _ = sock_path;
            }
        });

        let mut client = AgentClient::connect(&sock_path).expect("connect + handshake");
        client
            .send_step(&Step {
                movement: Some(AgentMovement { dir: [1.0, 0.0], up: 0.0, jump: false, wish_heading: None }),
                verb: None,
            })
            .expect("send step");
        let obs = client.recv_observation().expect("recv observation");
        assert_eq!(obs.tick, 0);
        assert_eq!(obs.own.zone_name, "qeynos");

        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 2: Run the test to see it fail**

```bash
cargo test -p harness-socket
```

Expected: `connect_handshakes_and_round_trips_a_step_and_observation ... FAILED`, panicking with
`not implemented: filled in by Step 3 of this task` — `AgentClient::connect` is the deliberate red
step (Step 1 wrote everything else for real; only `connect`'s body is stubbed).

- [ ] **Step 3: Implement `connect` for real**

Replace the `unimplemented!()` body:

```rust
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path)?;
        let writer = stream.try_clone()?;
        let reader = BufReader::new(stream);
        let mut client = AgentClient { writer, reader };
        client.handshake()?;
        Ok(client)
    }
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
cargo test -p harness-socket
```

Expected: `connect_handshakes_and_round_trips_a_step_and_observation ... ok`.

- [ ] **Step 5: Commit**

```bash
git add crates/harness-socket/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(harness-socket): real Agent Plugin API client

AgentClient::connect/send_step/recv_observation, mirroring eqoxide's own
fixed_sequence_client.rs example as a reusable type. Tested against a minimal in-process fake
server (no live eqoxide instance needed).

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
git push
```

---

### Task 4: `harness-nav` — zone geometry loader

**Files:**
- Create: `crates/harness-nav/src/zone.rs` (already stubbed in Task 2; this task fills it in)
- Modify: `crates/harness-nav/src/lib.rs` (no change needed — `pub mod zone;` already there)

**Interfaces:**
- Consumes: `eqoxide_assets::ZoneAssets::from_glb(path: &std::path::Path) ->
  anyhow::Result<ZoneAssets>`, `eqoxide_zone_geometry::collision::Collision::build(assets:
  &ZoneAssets, cell_size: f32) -> Collision`.
- Produces: `harness_nav::zone::load_zone(glb_path: &std::path::Path, cell_size: f32) ->
  anyhow::Result<eqoxide_zone_geometry::collision::Collision>`. A later A*-porting plan builds on
  top of the `Collision` this returns.

- [ ] **Step 1: Write the failing test**

Replace `crates/harness-nav/src/zone.rs` with:

```rust
//! Zone geometry loading (eqoxide spec §10, §11) — builds a `Collision` from a zone's asset GLB,
//! using the exact same construction path eqoxide's own client uses (`eqoxide_assets::ZoneAssets`
//! + `eqoxide_zone_geometry::collision::Collision::build`).
//!
//! §11 gives the harness the on-disk path to eqoxide's asset cache via the handshake's
//! `asset_cache_dir` field — but that field isn't implemented in the protocol yet (a separate,
//! not-yet-done piece of this redesign: eqoxide spec §9/§11). Until then, callers supply the GLB
//! path directly.

use eqoxide_assets::ZoneAssets;
use eqoxide_zone_geometry::collision::Collision;
use std::path::Path;

/// Load a zone's collision geometry from its asset GLB at `glb_path`, gridded at `cell_size`
/// (world units per collision-grid cell — eqoxide's own zone loader uses the same construction
/// path with its own cell size choice; callers of this function choose their own).
pub fn load_zone(glb_path: &Path, cell_size: f32) -> anyhow::Result<Collision> {
    let assets = ZoneAssets::from_glb(glb_path)?;
    Ok(Collision::build(&assets, cell_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_zone_on_a_nonexistent_path_is_an_error() {
        let result = load_zone(Path::new("/nonexistent/definitely-not-a-zone.glb"), 32.0);
        assert!(result.is_err(), "a missing GLB file must surface as an error, not panic or silently succeed");
    }

    /// Proves the pinned eqoxide-zone-geometry dependency actually builds a real, queryable
    /// Collision through the same ZoneAssets -> Collision::build path load_zone uses — using
    /// eqoxide's own test-only flat-floor fixture (test-fixtures feature) instead of a real GLB
    /// file, which this workspace has no fixture for yet. This test does not exercise
    /// `load_zone`'s own GLB-parsing step (`ZoneAssets::from_glb`) — that needs a real .glb file,
    /// which is follow-up-plan work once one is available to check into this repo (see
    /// docs/scope.md).
    #[test]
    fn the_pinned_zone_geometry_crate_builds_a_real_queryable_collision() {
        let state = eqoxide_zone_geometry::zone_assets::ZoneAssetState::test_ready();
        let collision = state.collision().expect("test_ready() always returns a Ready state");
        assert!(collision.has_geometry(), "the flat-floor fixture must produce real grid geometry");
        assert!(collision.has_triangles(), "the flat-floor fixture must produce real triangles to query");
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass**

```bash
cargo test -p harness-nav --features harness-nav/dummy 2>/dev/null; cargo test -p harness-nav
```

Expected: both tests pass —
`load_zone_on_a_nonexistent_path_is_an_error` and
`the_pinned_zone_geometry_crate_builds_a_real_queryable_collision`.

(The first command above is not needed — `harness-nav` has no such feature. Just run:)

```bash
cargo test -p harness-nav
```

Expected: `test result: ok. 2 passed`.

- [ ] **Step 3: Commit**

```bash
git add crates/harness-nav/src/zone.rs
git commit -m "$(cat <<'EOF'
feat(harness-nav): zone geometry loader over the pinned eqoxide-zone-geometry crate

load_zone(glb_path, cell_size) -> Collision, the same ZoneAssets::from_glb + Collision::build
path eqoxide's own client uses. A* planning on top of this Collision is out of scope for this
plan (see docs/scope.md) — this task only proves the loader and the pinned dependency work.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
git push
```

---

### Task 5: `harness-combat` — chase-and-face-while-engaged

**Files:**
- Create: `crates/harness-combat/src/chase.rs`
- Modify: `crates/harness-combat/src/lib.rs` (add `pub mod chase;`)

**Interfaces:**
- Consumes: `eqoxide_agent_protocol::observation::{OwnState, VisibleEntity}`,
  `eqoxide_agent_protocol::movement::AgentMovement`.
- Produces: `harness_combat::chase::chase_and_face(own: &OwnState, target: &VisibleEntity,
  engage_range: f32) -> AgentMovement`.

- [ ] **Step 1: Write the failing test**

`crates/harness-combat/src/chase.rs`:

```rust
//! Chase-and-face-while-engaged (eqoxide spec §7.2) — reads own position/heading and the target's
//! live position straight from `Observation`, computes range/bearing itself (exactly what a human
//! player derives by looking at their screen), and drives movement + wish_heading toward the
//! target. No special access to eqoxide internals.

use eqoxide_agent_protocol::movement::AgentMovement;
use eqoxide_agent_protocol::observation::{OwnState, VisibleEntity};

/// EQ heading in degrees (0..360) for a movement delta in server axes: heading 0 faces +Y
/// (north) and increases counter-clockwise (90 = west, 180 = south, 270 = east) — reimplemented
/// here from eqoxide's own `eqoxide_core::coord::eq_heading` (crates/eqoxide-core/src/coord.rs in
/// the eqoxide repo), which this workspace cannot depend on (Global Constraints: only
/// eqoxide-agent-protocol/eqoxide-assets/eqoxide-zone-geometry are allowed dependencies).
fn eq_heading(d_east: f32, d_north: f32) -> f32 {
    (-d_east).atan2(d_north).to_degrees().rem_euclid(360.0)
}

/// Compute the movement to close on and face `target`, given the agent's own current state.
/// Always faces the target (`wish_heading` is always `Some`); stops closing distance once within
/// `engage_range` but keeps facing it — the "while engaged" half of the behavior.
pub fn chase_and_face(own: &OwnState, target: &VisibleEntity, engage_range: f32) -> AgentMovement {
    let d_east = target.pos[0] - own.pos[0];
    let d_north = target.pos[1] - own.pos[1];
    let range = (d_east * d_east + d_north * d_north).sqrt();
    let heading = eq_heading(d_east, d_north);

    let dir = if range > engage_range && range > 1e-4 {
        [d_east / range, d_north / range]
    } else {
        [0.0, 0.0]
    };

    AgentMovement { dir, up: 0.0, jump: false, wish_heading: Some(heading) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_agent_protocol::observation::OwnState;

    fn own_at_origin() -> OwnState {
        OwnState {
            pos: [0.0, 0.0, 0.0], heading: 0.0, hp: 100, hp_max: 100, hp_verified: true,
            mana: 0, mana_max: 0, endurance: 0, endurance_max: 0, endurance_confirmed: true,
            casting: None, buffs: vec![], zone_name: "qeynos".into(), target_id: None,
            target_name: None, auto_attack: false, sitting: false, held: false,
            player_class: "Warrior".into(), player_level: 1,
        }
    }

    fn target_at(pos: [f32; 3]) -> VisibleEntity {
        VisibleEntity {
            spawn_id: 7, name: "a_rat00".into(), is_npc: true, level: 3, race: "Rat".into(),
            pos, heading: 0.0, hp_pct: 100.0, dead: false,
        }
    }

    #[test]
    fn a_far_target_due_east_is_chased_and_faced_at_heading_270() {
        let m = chase_and_face(&own_at_origin(), &target_at([100.0, 0.0, 0.0]), 20.0);
        assert!((m.dir[0] - 1.0).abs() < 1e-4 && m.dir[1].abs() < 1e-4, "must move straight east: {:?}", m.dir);
        assert!((m.wish_heading.unwrap() - 270.0).abs() < 1e-3, "east must be heading 270: {:?}", m.wish_heading);
    }

    #[test]
    fn a_far_target_due_north_is_chased_and_faced_at_heading_0() {
        let m = chase_and_face(&own_at_origin(), &target_at([0.0, 100.0, 0.0]), 20.0);
        assert!(m.dir[0].abs() < 1e-4 && (m.dir[1] - 1.0).abs() < 1e-4, "must move straight north: {:?}", m.dir);
        assert!(m.wish_heading.unwrap().abs() < 1e-3, "north must be heading 0: {:?}", m.wish_heading);
    }

    #[test]
    fn a_target_within_engage_range_stops_closing_but_keeps_facing() {
        let m = chase_and_face(&own_at_origin(), &target_at([5.0, 0.0, 0.0]), 20.0);
        assert_eq!(m.dir, [0.0, 0.0], "within engage_range: must stand still, not keep closing");
        assert!((m.wish_heading.unwrap() - 270.0).abs() < 1e-3, "must still face the target while engaged: {:?}", m.wish_heading);
    }
}
```

`crates/harness-combat/src/lib.rs`:

```rust
//! Chase-and-face-while-engaged (eqoxide spec §7.2) — reimplemented on raw AgentMovement +
//! Observation, no special access to eqoxide internals.

pub mod chase;
```

- [ ] **Step 2: Run the tests to verify they pass**

```bash
cargo test -p harness-combat
```

Expected: `test result: ok. 3 passed` (the three `chase` tests).

- [ ] **Step 3: Commit**

```bash
git add crates/harness-combat/src/lib.rs crates/harness-combat/src/chase.rs
git commit -m "$(cat <<'EOF'
feat(harness-combat): chase-and-face-while-engaged

chase_and_face(own, target, engage_range) -> AgentMovement, computed entirely from raw
Observation data (eqoxide spec §7.2) — no special access to eqoxide internals, matching how any
other agent-harness project would have to derive the same movement.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
git push
```

---

### Task 6: `harness-http` — minimal HTTP wrapper (health check)

**Files:**
- Modify: `crates/harness-http/src/lib.rs`
- Modify: `crates/harness-http/src/main.rs`

**Interfaces:**
- Consumes: nothing from `harness-socket` yet (see the step-3 note on why) — this task only proves
  the axum scaffolding and its wiring to the rest of the workspace.
- Produces: `harness_http::router() -> axum::Router`, mountable by `main.rs` or by a test.

- [ ] **Step 1: Write the failing test**

Replace `crates/harness-http/src/lib.rs` with:

```rust
//! Optional HTTP convenience wrapper around harness-socket (eqoxide spec §10) — talks to
//! harness-socket, never to eqoxide directly.
//!
//! This task only wires up a `/health` endpoint to prove the axum scaffolding and its place in
//! the workspace. Real socket-backed routes (movement, combat verbs, observation polling) are
//! out of scope for this plan — see docs/scope.md.

use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

pub fn router() -> Router {
    Router::new().route("/health", get(health))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_returns_200_and_ok_status() {
        let app = router();
        let response = app
            .oneshot(axum::http::Request::builder().uri("/health").body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let health: Health = serde_json::from_slice(&body).unwrap();
        assert_eq!(health.status, "ok");
    }
}
```

`crates/harness-http/src/main.rs`:

```rust
//! Convenience binary: binds harness_http::router() to a local port. See eqoxide spec §10 — this
//! is optional, and out-of-process from eqoxide either way.

#[tokio::main]
async fn main() {
    let app = harness_http::router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8090").await.unwrap();
    println!("harness-http listening on http://127.0.0.1:8090 (routes: GET /health)");
    axum::serve(listener, app).await.unwrap();
}
```

Update `crates/harness-http/Cargo.toml`'s `[dependencies]` block to add the `tokio`/`tower`-derived
pieces `main.rs`'s `#[tokio::main]` needs (the workspace-level `tokio = { features = ["full"] }`
already present in Task 2 covers this — no change needed there) plus `Serialize`'s
`derive` feature is already present from Task 2. No manifest changes needed for this task.

- [ ] **Step 2: Run the test to verify it passes**

```bash
cargo test -p harness-http
```

Expected: `health_returns_200_and_ok_status ... ok`.

- [ ] **Step 3: Verify the binary runs**

```bash
cargo run -p harness-http &
sleep 1
curl -s http://127.0.0.1:8090/health
kill %1
```

Expected output from `curl`: `{"status":"ok"}`.

(Note: `harness_http::router()` does not yet take a `harness-socket::AgentClient` — that wiring is
follow-up-plan work once `harness-http` grows routes that actually need a live connection. Keeping
`harness-socket` as a declared-but-unused-for-now dependency here, rather than removing it from
Task 2's manifest, is deliberate: it locks in the shape §10 specifies for the next plan to build on.)

- [ ] **Step 4: Commit**

```bash
git add crates/harness-http/src/lib.rs crates/harness-http/src/main.rs
git commit -m "$(cat <<'EOF'
feat(harness-http): minimal axum scaffold with a /health route

Proves the HTTP wrapper's place in the workspace end-to-end (router, binary, test). Real
socket-backed routes are out of scope for this plan (see docs/scope.md) — eqoxide-http alone is
~20,000 lines of route handlers, a full port is dedicated follow-up-plan work.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
git push
```

---

### Task 7: CI workflow

**Files:**
- Create: `.github/workflows/ci.yml`

**Interfaces:**
- Produces: a GitHub Actions workflow the README's badge (Task 1) already links to.

- [ ] **Step 1: Write the workflow**

`.github/workflows/ci.yml`:

```yaml
name: CI

on:
  push:
  pull_request:

jobs:
  build-test-lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - name: cargo build
        run: cargo build --workspace --all-targets
      - name: cargo test
        run: cargo test --workspace
      - name: cargo clippy
        run: cargo clippy --workspace --all-targets -- -D warnings
      - name: cargo fmt --check
        run: cargo fmt --all -- --check
```

- [ ] **Step 2: Commit and push, then watch the run**

```bash
git add .github/workflows/ci.yml
git commit -m "$(cat <<'EOF'
ci: build, test, clippy, and fmt-check the workspace on every push

Fetches the three pinned eqoxide crates from the public djhenry/eqoxide repo — no auth needed,
both repos are public.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
git push
gh run watch
```

Expected: the workflow run completes with conclusion `success`. If `cargo fmt --all -- --check`
fails because earlier tasks' code wasn't run through `cargo fmt`, run `cargo fmt --all` locally,
commit the formatting fixes, push, and re-watch.

---

### Task 8: A worked cross-crate example, and reference-implementation scope docs

**Files:**
- Modify: `crates/harness-combat/Cargo.toml` (add a dev-dependency)
- Create: `crates/harness-combat/examples/chase_nearest_npc.rs`
- Create: `docs/scope.md`

**Interfaces:**
- Consumes: `harness_socket::AgentClient` (Task 3), `harness_combat::chase::chase_and_face` (Task 5).
- Produces: the `examples/` directory the spec's §10 anticipated shape calls for (sibling of
  `crates/`, realized here as a per-crate `examples/` dir, Cargo's standard location); the doc
  `README.md` (Task 1) already links to.

- [ ] **Step 1: Add `harness-socket` as a dev-dependency of `harness-combat`**

The library itself stays free of any dependency on `harness-socket` (Global Constraints: each
crate's own vertical slice stays focused) — but a dev-dependency is available to `examples/` and
`tests/` without becoming part of the published lib's dependency graph. Add to
`crates/harness-combat/Cargo.toml`:

```toml
[dev-dependencies]
harness-socket = { path = "../harness-socket" }
```

- [ ] **Step 2: Write the example**

`crates/harness-combat/examples/chase_nearest_npc.rs`:

```rust
//! Reference example (eqoxide spec §10's anticipated `examples/` directory): connects to a live
//! eqoxide instance's Agent Plugin API socket and chases the nearest visible, living NPC for 20
//! ticks, printing what it sees. Run against a real eqoxide instance:
//!
//! ```text
//! # in the eqoxide checkout
//! cargo run -- --testzone --agent-socket /tmp/eqoxide-agent.sock
//! # in this repo
//! cargo run -p harness-combat --example chase_nearest_npc -- /tmp/eqoxide-agent.sock
//! ```

use eqoxide_agent_protocol::movement::AgentMovement;
use eqoxide_agent_protocol::step::Step;
use harness_combat::chase::chase_and_face;
use harness_socket::AgentClient;

fn main() {
    let socket_path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: chase_nearest_npc <agent-socket-path>");
        std::process::exit(1);
    });

    let mut client = AgentClient::connect(&socket_path).expect("connect + handshake");
    println!("connected to {socket_path}, chasing the nearest visible NPC for 20 ticks");

    for _ in 0..20 {
        let obs = client.recv_observation().expect("recv observation");
        let nearest = obs.visible.iter().filter(|e| e.is_npc && !e.dead).min_by(|a, b| {
            let da = (a.pos[0] - obs.own.pos[0]).hypot(a.pos[1] - obs.own.pos[1]);
            let db = (b.pos[0] - obs.own.pos[0]).hypot(b.pos[1] - obs.own.pos[1]);
            da.total_cmp(&db)
        });

        let movement = match nearest {
            Some(target) => {
                println!("tick {}: chasing {} (hp {:.0}%)", obs.tick, target.name, target.hp_pct);
                chase_and_face(&obs.own, target, 15.0)
            }
            None => {
                println!("tick {}: no NPC visible, standing still", obs.tick);
                AgentMovement { dir: [0.0, 0.0], up: 0.0, jump: false, wish_heading: None }
            }
        };

        client.send_step(&Step { movement: Some(movement), verb: None }).expect("send step");
    }
}
```

- [ ] **Step 3: Verify it builds**

```bash
cargo build -p harness-combat --examples
```

Expected: succeeds (this example needs a live eqoxide instance to actually run against, which
Task 9's automated verification does not set up — building it is the check at this stage).

- [ ] **Step 4: Commit**

```bash
git add crates/harness-combat/Cargo.toml crates/harness-combat/examples/chase_nearest_npc.rs
git commit -m "$(cat <<'EOF'
feat(harness-combat): worked cross-crate example — chase_nearest_npc

Combines harness-socket + harness-combat exactly as a real caller would (eqoxide spec §10's
anticipated examples/ directory). Builds against the pinned dependency graph; running it end to
end needs a live eqoxide instance, which this task doesn't set up.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
git push
```

- [ ] **Step 5: Write `docs/scope.md`**

```markdown
# Scope

This repo bootstraps the reference agent harness described in eqoxide's
[`docs/specs/2026-09-21-agent-harness-separation-design.md`](https://github.com/djhenry/eqoxide/blob/main/docs/specs/2026-09-21-agent-harness-separation-design.md)
§10. It intentionally does not implement everything that design describes yet.

## Implemented today

- **`harness-socket`** — a real Agent Plugin API client: connect, handshake, send `Step`, receive
  `Observation`. Tested against an in-process fake server, not yet exercised against a live
  eqoxide instance in CI.
- **`harness-nav`** — `load_zone(glb_path, cell_size)` builds a real `Collision` via the pinned
  `eqoxide-zone-geometry`/`eqoxide-assets` crates. No A* planner, walker, or steering yet.
- **`harness-combat`** — `chase_and_face(own, target, engage_range)`, a pure function computing
  movement + facing toward a visible target from raw `Observation` data.
- **`harness-http`** — a `GET /health` route proving the axum scaffolding's place in the
  workspace. No routes that actually drive `harness-socket` yet.
- **`crates/harness-combat/examples/chase_nearest_npc.rs`** — a worked, buildable example
  combining `harness-socket` + `harness-combat` the way a real caller would: connect, watch for
  the nearest visible living NPC, chase and face it. Needs a live eqoxide instance to actually run
  (`cargo run -p harness-combat --example chase_nearest_npc -- <socket-path>`).

## Deliberately deferred (follow-up plans, not started)

- **Full A* planner/walker/steering port** into `harness-nav`, from eqoxide's
  `crates/eqoxide-nav/src/{planner,walker,steering,traversability}.rs` (~19,000 lines combined in
  the eqoxide repo today). This is its own dedicated plan.
- **Full HTTP route port** into `harness-http`, from eqoxide's `crates/eqoxide-http/src/` (~20,000
  lines of route handlers). Also its own dedicated plan — and optional per spec §10 even then.
- **§11's `asset_cache_dir` handshake field.** eqoxide spec §11 proposes eqoxide disclosing its
  on-disk asset cache path during the handshake, so `harness-nav` can auto-discover zone files
  instead of a caller supplying a GLB path by hand. That protocol addition (eqoxide spec §9/§11)
  hasn't landed in `eqoxide-agent-protocol` yet — check `HandshakeReply::Accepted` in eqoxide's
  `crates/eqoxide-agent-protocol/src/handshake.rs`; as long as it's a unit variant with no fields,
  this hasn't happened, and `load_zone` needs an explicit path.
- **A checked-in test `.glb` fixture** for `harness-nav`'s `load_zone` itself. Today its own test
  only checks the not-found-path error case; the "does it actually build a real Collision" case is
  covered indirectly via eqoxide's `test_ready()` fixture, which bypasses `from_glb` entirely.

## Bumping the eqoxide pin

Every git dependency in this workspace on `https://github.com/djhenry/eqoxide.git` **must** share
one `rev`. To bump it (e.g. once eqoxide's `worktree-rl-api-design` branch gains new commits this
harness needs, or once that branch merges to `main`):

1. `git -C ~/git/eqoxide log --oneline -1` (or check the PR) to get the new commit sha.
2. Replace every occurrence of the old sha with the new one across every `Cargo.toml` in
   `crates/*/Cargo.toml` in this repo — `grep -rl 'rev = "' crates/*/Cargo.toml` lists every file
   that needs the edit.
3. `cargo build --workspace` to confirm the new commit still provides everything this workspace
   depends on.
4. Commit all the manifest changes together in one commit (a partial bump leaves some crates
   pinned to the old rev and others to the new one, which is the exact "two revs, one URL" error
   Global Constraints in the bootstrap plan warns about).
```

- [ ] **Step 6: Commit**

```bash
git add docs/scope.md
git commit -m "$(cat <<'EOF'
docs: scope.md — what's implemented vs. deferred, and the eqoxide-pin bump procedure

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
git push
```

---

### Task 9: Final verification

**Files:** none (verification only).

**Interfaces:** none.

- [ ] **Step 1: Full workspace build, test, lint, and format check**

```bash
cargo build --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: all four succeed with no errors or warnings.

- [ ] **Step 2: Confirm the dependency allow-list held**

```bash
grep -rn 'eqoxide_core\|eqoxide_ipc\|eqoxide_nav\|eqoxide_http\|eqoxide_command\|eqoxide_protocol\|eqoxide_telemetry\|eqoxide_crash\|eqoxide_net\|eqoxide_renderer\|eqoxide_ui\|eqoxide_agent_plugin_host\|eqoxide_agent_vision_filter' crates/ --include='*.rs'
```

Expected: no output. Any hit is a Global Constraints violation — a crate in this workspace has
coupled itself to an in-process eqoxide type instead of talking over the socket.

- [ ] **Step 3: Confirm the git-dependency pin is consistent**

```bash
grep -h 'rev = "' crates/*/Cargo.toml | sort -u
```

Expected: exactly one distinct line, all pinned to `d9075fe6b4652f84c66d6b58e0a85c4f6ca2a8ba` (or
whatever rev Task 8's bump procedure last updated it to, if this verification runs after a bump).
More than one distinct line is the "two revs, one URL" error Global Constraints describes.

- [ ] **Step 4: Confirm CI is green on the pushed branch**

```bash
gh run list --limit 1
```

Expected: the most recent run's conclusion is `success`.

- [ ] **Step 5: Report completion**

No commit needed for this task (it's verification-only) — if any check above failed and required a
fix, that fix was already committed within the relevant step. Report to the user: repo URL, what
each of the four crates does today, and the explicit deferred-work list from `docs/scope.md`.
