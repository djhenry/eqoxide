# Agent Harness Separation — Design Spec

**Date:** 2026-09-21
**Status:** Draft, pending user review

## 1. Motivation

`docs/specs/2026-09-15-agent-plugin-api-design.md` (implemented on this branch, PR #1131)
gave eqoxide a low-latency socket API for agent control, alongside the pre-existing HTTP
API. That spec's own §14 flagged, as anticipated future work: *"Extracting `eqoxide-nav`
into its own agent-framework plugin module... once there's a second consumer that
justifies it."* This spec is that extraction, and it goes further: it draws a hard line
around what eqoxide is for, and moves everything on the wrong side of that line out.

Two things made this line-drawing necessary rather than a nice-to-have:

- **`eqoxide-http`'s ~70 routes** span far more than navigation — merchant, inventory,
  quests, chat, group/guild, trainer, lifecycle — all of it living in-process, coupled
  to eqoxide's shared-Arc IPC types (`docs/architecture.md`'s `GotoTarget`, `HailReq`,
  `TargetReq`, etc.). Every one of those routes is a second front door with its own
  mailbox-vs-agent-socket interaction to reason about (PR #1131's review finding I5).
- **`drive_auto_engage_melee`** (`crates/eqoxide-net/src/action_loop.rs`, function of the
  same name) walks the character into melee range and faces the target automatically
  while auto-attack is on. Native RoF2 does no such thing: toggling auto-attack is a pure
  client-side boolean: the server independently re-checks range (`Mob::CombatRange`) and
  facing (`Mob::IsFacingMob`, ~80/512 heading units ≈ ±56° — both in EQEmu's
  `zone/client_process.cpp`/`zone/mob.cpp`, the same facing gate `docs/autonomous-play.md`
  already documents) every attack tick, but never moves or turns the character for you. Chasing
  and facing while meleeing is something a *human player* actively does. eqoxide having
  it built in means eqoxide is not just a client, but a client with one particular
  player's playstyle baked in.

Both are symptoms of the same underlying problem: eqoxide had no principled boundary
between "what the client does for you" and "what a player, or something standing in for
a player, decides to do." This spec establishes that boundary and relocates everything
that falls on the wrong side of it into a new, separate project.

## 2. Guiding Principle

**eqoxide is scope-limited to being an RoF2-compatible client with agent-friendly plugin
hooks into the commands and information a human player would have in native RoF2.**
Anything that is normally a player's own responsibility — deciding *how* to get
somewhere, *when* to re-engage a moving target, *what* to do next — is out of eqoxide's
scope. It's a plugin's responsibility, exercised through the Agent Plugin API.

Concretely:

1. A* navigation and any HTTP-based convenience API move to a standalone agent-harness
   *example* project, separate from the eqoxide client.
2. Every navigation-specific eqoxide issue gets copied to the new project's tracker and
   closed here, with a comment pointing to the new issue and explaining the refactor.
3. Other agent-harness projects are free to reuse eqoxide's A* implementation, write
   their own, or use no path planning at all — eqoxide doesn't care.
4. `/follow` in the eqoxide client becomes exactly as dumb as native RoF2's `/follow` —
   no smarter.
5. Chasing and facing an enemy while auto-attack is active is an agent-harness
   responsibility, not something eqoxide does automatically.
6. Information and observation coming out of eqoxide's APIs are limited to what a human
   player could observe through chat windows or client menus — never omniscient state.
7. The plugin interface gives plugins access to zone asset files eqoxide has already
   downloaded, without expanding eqoxide's own scope to serve them over a new protocol.
8. The plugin interface exposes only player-observable game state — no positions or
   information a human player couldn't have gotten some other way.

**The test for "does this automation stay in eqoxide" is fidelity to native behavior, not
"is it automation."** `/follow` stays, even though it's automated character movement,
because native RoF2's client genuinely does this for a human player who types `/follow`.
`drive_auto_engage_melee` leaves, even though it's mechanically similar (walk toward a
target, turn to face it), because native RoF2 has no such feature — nothing in the real
client does this for you. The same test applies to every future feature request: does
native RoF2 do this for the player, or does the player do this for themselves?

## 3. Goals

- Draw a durable, principled line between "eqoxide" and "agent harness," so future
  feature requests have an obvious answer for which side they belong on.
- Reduce eqoxide's own surface area and the number of things that can interact with each
  other inside it (fewer front doors onto shared state — directly resolves PR #1131
  review finding I5).
- Produce a working reference implementation (`eqoxide-agent-harness-example-http-astar`)
  demonstrating that the pattern is viable: A* pathing, engage-and-chase combat
  positioning, and an HTTP convenience layer, all built purely on top of the Agent Plugin
  API's socket protocol — no special access to eqoxide internals.
- Keep the Agent Plugin API general enough that a harness project can implement *any*
  approach to navigation or combat positioning it wants, not just the example's.

## 4. Non-Goals

- Redesigning the trainer/RL side of the harness — out of scope for both projects, same
  as the prior spec's §3.
- Fully specifying the expanded `AgentVerb` wire protocol needed to reach HTTP-era
  parity (merchant, inventory, quests, chat, group/guild, trainer, etc.) — real work,
  deferred to its own dedicated planning pass (§9).
- A full field-by-field audit of `Observation` against the human-observability principle
  — a starting assessment is in §12, but the audit itself is future work.
- Deciding `eqoxide-agent-harness-example-http-astar`'s trainer/RL architecture, or
  whether it becomes anything more than a reference example.

## 5. Scope Boundary Overview

| Capability | Today | After this spec |
|---|---|---|
| A* pathing / `/v1/move/goto` | `eqoxide-nav` (`planner.rs`, `walker.rs`, `steering.rs`) + `eqoxide-http` | Harness-only |
| `/follow` | A*-routed, re-resolves a moving target every tick, `avoid_aggro`/`aggro_buffer` (`crates/eqoxide-http/src/move_api.rs`'s `post_follow`) | eqoxide: dumb straight-line seek at native fidelity (§7.1). Smart following (the current behavior) becomes a harness capability, built the same way `goto` is |
| Auto-attack chase/face (`drive_auto_engage_melee`) | `eqoxide-net` tick loop (`action_loop.rs`, function `drive_auto_engage_melee`) | Removed from eqoxide; ported to the harness as an example agent behavior (§7.2, §10) |
| Auto-attack range/facing legitimacy | Server-enforced (EQEmu), client passive either way | Unchanged |
| Explicit zone-cross (`take_zone_cross`/`resolve_zone_cross`, `/v1/move/zone_cross`) | `eqoxide-net` (`action_loop.rs`, function `drain_zone_cross`) + `eqoxide-http` | Removed from eqoxide; an equivalent, if wanted, is a harness capability built on raw movement + `Observation` |
| Automatic DRNTP-proximity zone-cross | `eqoxide-net`, passive, native-matching | Unchanged (§7.3) |
| Physical collision resolution (wall-sliding) | `eqoxide-nav/src/collision.rs` | Stays in eqoxide — a client concern natively too (§8) |
| Swim/climb surface detection | `eqoxide-nav/src/water_grid.rs`, `climb.rs` | Extracted into a shared geometry crate (§8); stays wired into eqoxide, also consumable by the harness for A* costing |
| `eqoxide-http` (~70 routes) | In-process axum server in eqoxide | Entire crate moves to the harness as an optional HTTP convenience wrapper (§10) |
| Agent Plugin API (socket) | Combat/Interact/Lifecycle wired; Merchant/Inventory/Quests/Chat reserved, uninhabited | Becomes eqoxide's *only* programmatic surface; expands toward HTTP-era parity (§9) |

## 6. What Moves to the Harness

- **The A* stack**: `crates/eqoxide-nav/src/planner.rs` (A* search), `walker.rs` and
  `steering.rs` (path-following/pure-pursuit execution). These implement *deciding how to
  get somewhere* — the player-responsibility half of navigation — as opposed to *not
  clipping through a wall while getting there*, which is a client concern (§8).
- **The explicit zone-cross ticket path**: `take_zone_cross`/`resolve_zone_cross`, called
  from `action_loop.rs`'s `drain_zone_cross`, and the `/v1/move/zone_cross` HTTP route.
  This resolves a named zone-point index to its DRNTP trigger region and walks the
  character there — itself a wayfinding decision, not a passive mechanic (contrast with
  §7.3).
- **`drive_auto_engage_melee`**: the chase-and-face-while-attacking behavior
  (`action_loop.rs`'s `drive_auto_engage_melee` and its `melee_chase_plausible` helper) is
  removed from eqoxide's tick loop and reimplemented in the harness (§10) on top of raw
  `AgentMovement` + `Observation` — the same information and controls a human player has,
  not a shortcut into eqoxide internals.
- **All of `eqoxide-http`**: every route currently under `crates/eqoxide-http/src/*.rs`
  (combat, merchant, inventory, trainer, pet, group, guild, quests, chat/social,
  interact, lifecycle, observe, camera, move, events — see `docs/http-api.md`). It's
  rebuilt in the harness as a convenience wrapper over the Agent Plugin API socket,
  serving the same purpose it serves today: a low-effort way for LLM-based agents (or
  anything else) to drive a live eqoxide session over plain HTTP, without every consumer
  needing to speak the NDJSON socket protocol directly.

## 7. What Stays in eqoxide, Redefined to Native Fidelity

### 7.1 `/follow`

Native RoF2's `/follow` is a straight-line seek with no path planning: it re-reads the
target's current position every tick, turns the character's heading toward it at a
clamped max turn rate, and synthesizes move-forward/stop purely from straight-line
distance versus combined collision radii. It auto-cancels on target death or on
exceeding a maximum follow distance. There's no waypoint queue, no obstacle or door
awareness, no aggro-avoidance — which is exactly why native RoF2's own UI warns "auto-
follow works best in wide open areas with low lag; twisty areas, lag, and other factors
may cause auto-follow to fail" (`eqstr_us.txt` id 13232).

eqoxide's `/follow` is rebuilt to match: straight-line seek, clamped turn rate, max-
distance cancel, no A*, no aggro-avoidance. The current smart implementation (full A*
routing through `move_api.rs`'s `post_follow`, re-resolving a moving target every
tick with `avoid_aggro`/`aggro_buffer`) is exactly the kind of "player skill" this spec
moves out — it becomes a harness capability built the same way `goto` is, not a special
case.

Native `/follow` was also observed restricted to a current group member as a target (also
refusing self-target, a can't-move/rooted state, sitting, and disguise/illusion) in the
client build investigated. This may be version-specific behavior worth reconfirming
before eqoxide hard-codes it; flagged here rather than resolved.

### 7.2 Auto-attack

Toggling auto-attack becomes a pure client-side boolean with **no client-driven
movement or facing** — matching native exactly. The server already independently
enforces range and facing every attack tick regardless of what the client does
(§1), so removing `drive_auto_engage_melee` changes nothing about combat *legitimacy* —
it only removes eqoxide's own automatic positioning assistance. An agent (or a human
player) that wants to stay in range and facing while meleeing has to move and turn
itself, exactly as a human player does — using the harness, or their own hands.

### 7.3 Automatic zone-crossing

The passive mechanism — standing in a DRNTP zone-line region baked into the zone BSP
triggers `OP_ZONE_CHANGE` via the resolved `OP_SendZonepoints` destination, on a 10-second
cooldown (`ZONE_CROSS_COOLDOWN_MS`, `action_loop.rs`), independent of *how* the character
got there — is unchanged. This matches a human player simply walking through a zone
line; it involves no wayfinding decision, so it isn't in scope to remove.

## 8. Shared Geometry Layer

Because the harness runs as a genuinely separate OS process with no shared memory
(confirmed direction, §9), it can't reach into eqoxide's in-process `SharedCollision`
grid the way `eqoxide-nav`'s A* planner does today. It needs its own copy of zone
geometry, built independently from the same on-disk assets (§11).

Rather than duplicate that geometry logic in two places (eqoxide's client and the
harness) and let them drift, the parts of `eqoxide-nav` that *build and query* a spatial
representation of the zone — as opposed to the parts that *search* it for a path — become
a shared crate both projects depend on:

- **Stays eqoxide-only, no sharing needed**: real-time collision *resolution* during
  movement (wall-sliding, physical movement constraints) — this only matters to
  something that is actually moving a character frame-by-frame inside the render loop.
- **Extracted into a shared crate** (tentative name `eqoxide-zone-geometry`), consumed
  in-process by eqoxide and as a git dependency by the harness (same mechanism
  `eqoxide-agent-protocol` already uses): the collision-grid *construction* logic
  eqoxide needs anyway for its own swim/climb auto-detection (`AgentMovement.up`'s
  "auto-detected, not agent-selected" contract, `docs/agent-api.md`), and that the
  harness needs for A* traversal costing. `water_grid.rs` and `climb.rs` fall here.
- **Moves to the harness outright**: A*-specific cost modeling and search
  (`planner.rs`). `traversability.rs`'s exact split between "geometry query" (shared)
  and "A* cost model" (harness-only) isn't fully resolved by this spec — it needs a
  closer read during implementation planning, flagged here rather than guessed at.

## 9. The Agent Plugin API Becomes eqoxide's Sole Programmatic Interface

With `eqoxide-http` gone, the Unix-socket Agent Plugin API (`eqoxide-agent-protocol` /
`eqoxide-agent-plugin-host`, `docs/agent-api.md`) is the only way anything external talks
to a running eqoxide instance — including the harness's own HTTP convenience layer,
which becomes a client of this socket rather than an in-process peer.

This requires expanding `AgentVerb` well past its current wired set (`Combat`,
`Interact{Sit,Stand}`, `Lifecycle::Respawn`) toward the surface area HTTP used to cover:
`Merchant`, `Inventory`, `Quests`, `Chat` are already reserved, uninhabited enum variants
in `crates/eqoxide-agent-protocol/src/verb.rs` waiting to be filled in; group/guild
management and the various `observe`-family read endpoints (`/entities`, `/inventory`,
`/skills`, `/spells`, `/doors`, `/who`, …) have no `AgentVerb`/`Observation` analog yet at
all. A `MoveVerb::Follow { spawn_id }` verb also needs adding, to trigger the native-
fidelity `/follow` of §7.1 (distinct from `AgentMovement`'s continuous per-tick payload,
same way `Move::ZoneCross` is already distinct from it).

**This mapping is real, substantial work and is explicitly not fully specified here** —
consistent with the prior spec's own Non-Goals ("wiring every action verb immediately —
most are typed and reserved"), it gets a dedicated follow-up planning pass once this
spec is approved, enumerating each HTTP route's `AgentVerb`/`Observation` equivalent.

## 10. New Project: `eqoxide-agent-harness-example-http-astar`

A separate repository, not a workspace member of eqoxide. It runs as a genuinely
separate OS process and talks to a running eqoxide instance **only** over the Agent
Plugin API's Unix domain socket — no shared memory, no in-process coupling. This mirrors
how any other agent-harness project would have to integrate, which is the point: it's a
reference implementation, not a privileged first-party client.

Anticipated shape:

```
eqoxide-agent-harness-example-http-astar/
├── crates/
│   ├── harness-nav/      — ported A* planner + walker + steering (§6); depends on the
│   │                        shared eqoxide-zone-geometry crate (§8) and eqoxide-assets
│   │                        for zone/asset parsing, both pinned as git dependencies
│   ├── harness-combat/   — chase-and-face-while-engaged (§7.2), reimplemented on raw
│   │                        AgentMovement + Observation: reads own position/heading and
│   │                        the target's live position from Observation.visible, computes
│   │                        range/bearing itself (exactly what a human player derives by
│   │                        looking at their screen), and drives movement + wish_heading
│   │                        toward the target — no special access to eqoxide internals
│   ├── harness-socket/   — Agent Plugin API client: handshake, Step send, Observation
│   │                        receive; depends on eqoxide-agent-protocol (pinned git dep,
│   │                        already zero-dependency on eqoxide by design)
│   └── harness-http/     — optional HTTP convenience wrapper, ported from eqoxide-http;
│                            talks to harness-socket, never to eqoxide directly
└── examples/, docs/
```

It depends on eqoxide only through crates eqoxide already publishes as pinned git
dependencies (`eqoxide-agent-protocol`, `eqoxide-assets`, the new `eqoxide-zone-geometry`)
— the same mechanism the original Agent Plugin API spec designed for exactly this kind
of external consumer (§4 of `2026-09-15-agent-plugin-api-design.md`).

## 11. Zone Asset Access

The harness needs to read the same zone geometry eqoxide has already downloaded, to
build its own copy of the shared geometry layer (§8). Rather than build a new binary-
transfer protocol capability into the Agent Plugin API, eqoxide discloses the filesystem
path to its existing on-disk asset cache during the handshake.

eqoxide already maintains this cache (`src/asset_sync.rs`'s `CacheDirs`: `root` resolved
via `dirs::data_dir()`, with `models_dir()` holding assembled, ready-to-read zone/model
artifacts and `cas_dir()` the underlying content-addressed store). `HandshakeReply`'s
`Accepted` variant (`docs/agent-api.md`'s handshake section) gains an `asset_cache_dir:
String` field carrying `CacheDirs::models_dir()`'s path. The harness reads zone files
directly off disk from there — no new transfer protocol, no polling, and it stays correct
automatically as eqoxide's own asset sync keeps that directory up to date.

This assumes the harness runs on the same machine/filesystem as the eqoxide instance it
drives, which matches every other assumption in this design (single Unix domain socket,
no network transport) and is the reasonable default for a reference implementation and
for local RL training.

## 12. Observation / Information-Parity Principle

Guiding principle bullets 6 and 8 (§2): eqoxide's APIs expose only what a human player
could observe — no positions or state a player couldn't have gotten some other way.

`eqoxide-agent-vision-filter` already does most of the work for `Observation.visible`: a
distance cutoff plus a real line-of-sight occlusion check against zone collision
geometry (`docs/agent-api.md`'s Receiving Observation section), rather than the zone's
full entity list. That substantially satisfies the principle for the visible-entity list
specifically.

A full field-by-field audit of the rest of `Observation` (`OwnState`, `LegalActionMask`)
against this principle hasn't been done and is explicitly deferred (§4) — nothing
currently in `docs/agent-api.md`'s shape looks like an obvious violation, but "looks fine
on a skim" isn't the same claim as "audited," and this needs a deliberate pass rather
than being asserted here.

## 13. Relationship to PR #1131 and Rollout

This is **one combined architecture change**, not a foundation-plus-follow-up. PR #1131
(currently: the Agent Plugin API implementation, `worktree-rl-api-design` branch) stays
open and unmerged; the work described in this spec lands as further commits on the same
branch, and the PR merges once, as a whole, when this redesign is complete — not before.
The Agent Plugin API's protocol expansion (§9) is what makes the socket capable of
replacing `eqoxide-http` at all, so treating the two as separable pieces would leave the
client with no working programmatic interface in between.

## 14. Issue Migration

Existing eqoxide issues that are specifically about navigation (A* planning, goto/follow
routing, zone-cross ticket handling) get copied into
`eqoxide-agent-harness-example-http-astar`'s tracker and closed here, each with a comment
pointing to the new issue and explaining the refactor. This is mechanical housekeeping,
done last, once the code move it's describing has actually happened — a closed issue
pointing at code that doesn't exist yet in the new repo would be worse than not migrating
it yet.

## 15. Testing Implications

- **eqoxide**: `drive_auto_engage_melee`'s existing test suite (`action_loop.rs`,
  e.g. `drive_auto_engage_melee_declines_an_unreachable_ledge_target_1119` and siblings)
  is deleted along with the code, not ported as-is — the harness's reimplementation
  needs its own tests against its own interface (raw `AgentMovement` + `Observation`),
  not a transplant of tests written against eqoxide-internal state. The *behaviors* those
  tests encode (ledge/z-gap handling, pet-mode standoff, etc.) are useful reference for
  what the harness's port should still handle correctly.
- **eqoxide**: `/follow`'s existing regression coverage (e.g.
  `goto_and_follow_resolve_an_ambiguous_name_to_the_same_spawn`) gets replaced with
  coverage of the new native-fidelity behavior (straight-line seek, turn-rate clamp,
  max-distance cancel) — most of it moves to the harness along with `goto`, but the
  slice of `/follow` that stays in eqoxide (§7.1) needs its own new tests.
- **`eqoxide-zone-geometry`** (§8): unit tests against fixture zone data, likely reusing
  `eqoxide-nav`'s existing fixtures where the split allows — construction/query logic
  only, no socket or process boundary involved.
- **Harness project**: its own test suite, out of scope for this repo's CI. Live
  verification the same way the current Agent Plugin API's `fixed_sequence_client.rs`
  example is used (`docs/agent-api.md`'s Minimal Example section) — run the harness
  against a live eqoxide instance in a real zone.

## 16. Open Questions / Future Work

- The exact `traversability.rs` split between shared geometry-query logic and
  harness-only A* cost modeling (§8) — needs a closer read during implementation
  planning, not guessed at here.
- The full `AgentVerb`/`Observation` expansion mapping every remaining `eqoxide-http`
  route to its socket-protocol equivalent (§9) — its own dedicated planning pass.
- The `Observation` field-by-field audit against the human-observability principle
  (§12).
- Whether native `/follow`'s group-member-only target restriction (§7.1) is
  version-specific and needs reconfirming before eqoxide hard-codes it.
- PR #1131's still-open Important/Minor findings from its independent code review
  (socket-permission TOCTOU, pre-existing-socket-file removal safety, movement-field
  validation, `deny_unknown_fields`, a silent-dead-socket bind failure, and several
  Minor items) are independent of this redesign and remain unaddressed. One review
  finding (I5, "nav exclusion for agent sessions was emergent rather than structural")
  is structurally resolved by this redesign, since there's no goto/A* left in eqoxide to
  conflict with an agent session at all. The rest need their own pass before merge.
