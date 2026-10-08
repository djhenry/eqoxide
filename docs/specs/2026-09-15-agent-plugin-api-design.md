# Agent Plugin API — Design Spec

**Date:** 2026-09-15
**Status:** Draft, pending user review

## 1. Motivation

eqoxide already exposes an HTTP API (`docs/http-api.md`) that works well for
LLM-style agents: request/response, one action at a time, tolerant of
hundred-millisecond-plus round trips. It's the wrong shape for a
reinforcement-learning agent, which needs:

- A per-tick observation stream, not a polled/pulled snapshot.
- Latency well under HTTP's per-request connection/parse overhead,
  especially once multiple eqoxide instances are training in parallel.
- Game state filtered down to what a player character could actually
  perceive, not the full zone's worth of entity updates — this filtering
  is eqoxide's responsibility, not the agent's.
- Raw directional/heading control with eqoxide's own pathfinding disabled,
  so the agent learns movement itself rather than delegating to A*.

The immediate driver is a planned Hierarchical Reinforcement Learning (HRL)
system — Goal-Conditioned HRL (Feudal Networks) and Mixture-of-Experts/Gated
HRL are both on the roadmap, starting with combat and expanding to full
gameplay. This spec does **not** design that trainer. It designs the
boundary between eqoxide and whatever drives it — a boundary general enough
that it isn't RL-specific, because the same plumbing should serve any future
agent plugin, not just this one.

## 2. Goals

- A persistent, low-latency, bidirectional channel between eqoxide and an
  external agent process.
- Per-tick observation push, independent of when the agent's action arrives.
- Client-side visibility filtering (distance + line-of-sight occlusion).
- Raw movement/heading control with navigation disabled for agent-driven
  sessions.
- An action taxonomy that mirrors the HTTP API's group structure, so wiring
  new verbs later is additive, not a redesign.
- **Strict separation** between eqoxide (client + protocol + plugin host)
  and the agent itself — the agent is a wholly separate project, not a
  crate in this repo, not a thing eqoxide launches or manages.
- An observation/action space that doesn't foreclose cross-class
  generalization or particular HRL architectures later.

## 3. Non-goals

- Designing the trainer (Feudal/MoE network architecture, reward shaping,
  PPO/whatever algorithm).
- Character creation, leveling, or curriculum progression — out-of-band
  harness concerns.
- Orchestrating multiple training instances in parallel.
- Wiring every action verb immediately — most are typed and reserved.
- Extracting `eqoxide-nav` into its own agent-framework plugin module (see
  §4, §14) — anticipated, not designed here.

## 4. Architecture

Three new crates, plus a naming convention deliberately chosen to be
agent-general rather than RL-specific:

```
eqoxide-agent-protocol        (pure wire types; no dependency on eqoxide internals)
eqoxide-agent-vision-filter   (visibility; sibling to eqoxide-nav; no renderer/wgpu dep)
eqoxide-agent-plugin-host     (in-client plumbing; depends on both crates above
                                plus eqoxide-nav/eqoxide-ipc internals)
```

**`eqoxide-agent-protocol`** defines the wire format — `Step`, `Observation`,
`AgentVerb`, and the connection handshake/version types. It has no path
dependency back into eqoxide or any of its other crates. This is what makes
the separation real: an external agent project depends on this crate alone
(pinned via `git = "...", tag = "..."`, since it isn't published to
crates.io), and never touches eqoxide's codebase.

**`eqoxide-agent-vision-filter`** computes, each tick, the set of entities a
player at the character's position could actually perceive: distance cutoff
plus line-of-sight occlusion via `eqoxide-nav::collision::line_clear`
against the existing pathfinding BSP grid (`SharedCollision`). No GPU, no
renderer dependency — this reuses the *concept* of the renderer's
`entity_in_view()` distance cull but not its frustum-cull half, since that's
tied to the observer/orbit camera's view-projection, which has no bearing on
what a player character itself can perceive.

**`eqoxide-agent-plugin-host`** is thin, deliberately dumb, in-client
plumbing — it contains no policy or decision logic. It:
- owns the Unix domain socket server and performs the version handshake,
- disables `Navigator`'s auto-pathing for agent-driven sessions,
- latches the most recently received `Step`'s action into `MoveIntent` and
  the existing HTTP-mailbox types each tick,
- calls `eqoxide-agent-vision-filter` and serializes the result into an
  `Observation`,
- pushes that `Observation` out and deserializes inbound `Step` frames.

Everything with actual judgment in it — episode boundaries, reset timing,
the trainer, multi-instance orchestration — lives in the external agent
project. That project is not part of this repo and is out of scope for this
spec.

The `eqoxide-agent-*` prefix is chosen so a future crate (for example, a
`eqoxide-nav` extraction — see §11) can join this family later without a
rename; it does not imply more crates exist today than the three above.

## 5. Transport and cadence

A persistent Unix domain socket per eqoxide instance, not HTTP — HTTP's
per-request connection/parse overhead is too costly at a ~150ms tick
cadence, multiplied across however many instances are training at once.

**Step cadence = eqoxide's existing ~150ms nav/combat tick**, not the
~10ms network-drain pass — movement and combat decisions only actually
change at that slower cadence today, so ticking the protocol faster would
just repeat stale decisions.

**The protocol is async duplex, not lockstep request/reply.** eqoxide
pushes one `Observation` every tick, unconditionally, on its own clock — it
never waits for an action to arrive, because nothing else in the game world
pauses for the agent either. The agent sends `Step` frames whenever its
policy produces a new action, independently of eqoxide's tick. eqoxide
always applies the most recently received action at the next tick boundary
— like a human holding a key down. If the agent is briefly slower than
150ms (e.g. a GPU-bound inference spike), a tick or two just reuses the
last action; this is normal environment dynamics, not an error condition —
action latency is part of what the agent has to learn to work within.

**Version handshake at connect.** Because the agent is a genuinely separate,
independently-versioned project (no shared build, no guaranteed sync), the
handshake exchanges a `protocol_version`. A mismatch is rejected with a
clear, typed error at connect time — never a silent misparse downstream.

## 6. Visibility model

Distance cutoff (configurable; default mirrors the renderer's
`ENTITY_DRAW_DIST = 500.0`) plus line-of-sight occlusion via `line_clear`.
No facing/FOV cone — a human player can freely swivel the camera, so
visibility isn't gated on which way the character happens to be facing.

Character heading is still part of the **action space**, independent of
visibility: melee requires facing the target server-side (`IsFacingMob`,
~56° tolerance per `docs/autonomous-play.md`), so the agent needs explicit
`wish_heading` control. Heading is currently *derived* from movement
direction via `eq_heading()` (`crates/eqoxide-core/src/coord.rs:128`) — an
independently settable heading, decoupled from translation, is a genuine
new capability this spec introduces.

## 7. Movement

Navigation (A*/Goto/Follow) is disabled entirely for agent-driven sessions
— permanently excluded for the lifetime of such a session, not merely
unwired. The agent controls raw directional input by reusing the existing
`MoveIntent` primitive (`crates/eqoxide-ipc/src/lib.rs:60`: `wish_dir`,
`wish_vspeed`, `jump`, `want_swim`, `want_climb`, `speed`, `hop`) — the same
struct already driving WASD, nav, and combat auto-engage today — plus the
new `wish_heading` field from §6.

Movement is **continuous**, not a discrete verb: every `Step` carries a
movement payload (`MoveIntent` + `wish_heading`) that eqoxide latches and
keeps applying every tick until a newer one arrives — the same "held key"
semantics as §5's action-latching. This is deliberately separate from
`AgentVerb` (§8), which carries the *optional* discrete action layered on
top of that continuous movement in the same `Step` (attack, sit, respawn,
...). A `Step` can carry movement with no verb, a verb with no movement
change, or both.

## 8. Action space (`AgentVerb`)

An enum mirroring the HTTP API's group taxonomy (`/v1/<group>/<action>`),
so each verb reuses the mailbox plumbing that group's HTTP handlers already
use, and wiring a new verb later is additive. Raw translation/heading is
*not* in this enum — it's the always-present continuous movement payload
described in §7.

```
AgentVerb::Combat     — attack/cast/target (wired now)
AgentVerb::Interact   — Sit, Stand (wired now)
AgentVerb::Lifecycle  — Respawn (wired now; see §9 on death-as-state)
AgentVerb::Move       — ZoneCross (reserved, typed, not wired)
AgentVerb::Merchant   — reserved, typed, not wired
AgentVerb::Inventory  — reserved, typed, not wired
AgentVerb::Quests     — reserved, typed, not wired
AgentVerb::Chat       — reserved, typed, not wired
```

`AgentVerb::Move` exists only for discrete travel actions like zone
crossing — not part of the initial combat scope — and is unrelated to the
continuous per-tick movement payload every `Step` already carries.

## 9. Observation space

- **Own state** — position, heading, health/mana/endurance, buffs, casting
  state; mirrors what's already exposed via the HTTP observe endpoints.
- **Visible-entity list** — from `eqoxide-agent-vision-filter` (§6).
- **Legal-action mask** — alongside the observation, covering class- and
  level-dependent action validity generically, so the protocol doesn't need
  to encode class-specific rules itself.
- **Semantic per-ability features** — effect category, target type,
  resource cost, cooldown — plus class/role identity, rather than raw
  gem-slot indices. Raw slot indices would foreclose cross-class
  generalization later without a wire-format redesign; semantic features
  keep that door open. Whether to train one generalist model or per-class
  specialists is a trainer-side experiment this API doesn't decide — it
  connects naturally to either HRL architecture under consideration (an
  MoE gate can route by class/kit; a Feudal manager can stay class-agnostic
  while the worker uses the concrete kit).
- **Three distinct terminal-adjacent signals**, not one:
  - `dead: bool` — character death as ongoing *state*, not a terminal
    boundary. Death is routine and frequent in combat training; the agent
    keeps receiving observations while dead and must issue
    `Lifecycle::Respawn` to continue.
  - `terminated: bool` — true session end.
  - `truncated: bool` — an artificial, harness-imposed rollout cutoff,
    always safe to bootstrap past (matching Gymnasium's
    `terminated`/`truncated` distinction).
  - HRL option/goal boundaries (a Feudal manager's reissue cadence, an MoE
    gate's re-evaluation point) are **never** represented here — per the
    options framework and Feudal Networks literature, those are purely
    trainer-internal bookkeeping, not environment events.

## 10. Reset and session semantics

`reset()` means **a new session with the same character**, not a new
character. Character creation and leveling are out-of-band harness
concerns, reusing the DB-seeding recipes already documented in
`docs/autonomous-play.md` §1/§6 — scripting that is the harness's job, not
this protocol's.

A session survives brief disconnects: if the agent process crashes and
reconnects, it resumes the same session rather than forcing a `reset()`.
A fresh session means a real login/zone-in (seconds) — far too expensive to
pay for a transient disconnect.

## 11. Error handling and edge cases

- **Connection drop, agent process alive** — reconnect to the same session;
  no forced reset (§10).
- **Acting while dead** — not an error. Movement/combat verbs no-op until
  `Lifecycle::Respawn` succeeds; the tick keeps advancing and observations
  keep publishing.
- **Malformed or illegal action** (e.g. one the legal-action mask says is
  invalid, sent anyway) — reject just that action, apply nothing for that
  slot this tick, keep the connection alive. One bad action never tears
  down the session, matching the existing HTTP API's philosophy of
  validating without punishing the connection.
- **Protocol version mismatch at handshake** — reject the connection with a
  clear, typed error at connect time (§5); never a silent misparse.

## 12. Scope boundaries

**In scope, this repo, this spec:**
- `eqoxide-agent-protocol`, `eqoxide-agent-vision-filter`,
  `eqoxide-agent-plugin-host`.
- The continuous per-tick movement payload (`MoveIntent` + `wish_heading`)
  and `AgentVerb::Combat`, `Interact{Sit,Stand}`, `Lifecycle::Respawn`
  wired to real handlers.
- `AgentVerb::Merchant`, `Inventory`, `Quests`, `Chat`, `Move(ZoneCross)`
  present as typed, reserved `AgentVerb` variants with no handler yet.

**Explicitly out of scope:**
- The agent/session driver itself — a wholly separate project: the
  trainer (Feudal/MoE architecture, reward shaping), episode/reset policy,
  multi-instance orchestration, curriculum harness.
- Wiring the reserved `AgentVerb` variants.
- `A*`/`Goto`/`Follow` for agent-driven sessions — permanently excluded,
  not deferred.
- Extracting `eqoxide-nav` into its own agent-framework plugin module (see
  §4 naming note, §14 future work) — anticipated but not designed here.

## 13. Testing

Scaled to each crate's actual risk:

- **`eqoxide-agent-protocol`** — unit tests: round-trip (de)serialization
  of every `Step`/`Observation`/`AgentVerb` variant; handshake accepts a
  matching `protocol_version` and rejects a mismatched one with the
  documented error.
- **`eqoxide-agent-vision-filter`** — unit tests against fixture zones/
  collision data (reusing `eqoxide-nav`'s existing test fixtures where
  possible): an entity beyond the distance cutoff is excluded; an entity
  behind a wall is excluded via `line_clear`; an entity within distance and
  clear LOS is included. Pure logic, no socket involved — fast, no live
  client needed.
- **`eqoxide-agent-plugin-host`** — integration tests with a fake in-test
  client on a loopback socket: handshake success/rejection; `Observation`
  pushed every tick unconditionally; a latched `Step` shows up in
  `MoveIntent` at the next tick; disconnect-then-reconnect resumes the same
  session without forcing a `reset()`.
- **Live verification** — a minimal fixed-action-sequence example client
  (not the real agent — just enough to exercise the protocol end-to-end),
  run against a live eqoxide instance in a real zone, in the same spirit as
  how the HTTP API is manually verified per `docs/dev-workflow.md`. Worth
  keeping as a small `examples/` binary in `eqoxide-agent-protocol` so the
  protocol is testable before the external agent project exists.

## 14. Open questions / future work

- Extracting `eqoxide-nav` into its own agent-framework plugin module,
  alongside `eqoxide-agent-vision-filter`, once there's a second consumer
  that justifies it.
- Whether agent types other than the RL trainer end up reusing
  `eqoxide-agent-plugin-host` — this is why the crates are named
  generically rather than RL-specifically, but no second consumer exists
  yet.
