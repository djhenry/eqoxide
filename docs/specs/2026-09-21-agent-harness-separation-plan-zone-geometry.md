# Zone Geometry Crate Extraction — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract the geometry-grid construction/query logic that both eqoxide's own
client and the future agent-harness project need into a new, independent crate
(`eqoxide-zone-geometry`), with zero behavior change to eqoxide itself.

**Architecture:** `crates/eqoxide-nav/src/*.rs` splits by *investigated, confirmed usage*
(not by filename) into two destinations: geometry-grid construction and query logic that
real, in-workspace code already calls in production for reasons unrelated to A* pathing
moves into the new crate; A*-search-specific machinery stays in `eqoxide-nav` untouched,
to be relocated wholesale to the harness repo by a later plan. Every in-workspace consumer
of a moved symbol is repointed to import it from the new crate instead.

**Tech Stack:** Rust, Cargo workspace (no new external dependencies expected — the new
crate's dependency list mirrors whatever the moved modules already depend on today).

**Spec:** `docs/specs/2026-09-21-agent-harness-separation-design.md` (§8, "Shared Geometry
Layer"). This plan is Plan 1 of that spec's decomposition; see that spec's §8 for the
three-way split's rationale. It also implements one refinement beyond what §8 states
explicitly: §8 left `traversability.rs`'s split "not fully resolved... needs a closer
read during implementation planning" — that closer read happened during this plan's
research phase and its result is captured in Task 4 below.

## Global Constraints

- **No backwards-compatibility re-export shims.** Every consumer of a moved symbol gets
  its import path updated to the new crate directly. Do not leave a `pub use
  eqoxide_zone_geometry::foo as foo;` compatibility re-export in `eqoxide-nav` "just in
  case" — if a task's move leaves an unused re-export behind, delete it in the same task.
- **Delete dead code; don't document it.** If a task's move reveals a helper that turns
  out to have no remaining caller on either side of the split, delete it — a comment
  explaining why it's unused is never the right outcome.
- **No line-number citations in commit messages, code comments, or this plan's own
  future edits.** Reference symbols by name; they're greppable and don't drift.
- **The full workspace test suite (`cargo test --workspace`) and `cargo clippy
  --workspace --all-targets -- -D warnings` must be green at the end of every task.**
  This plan is a pure refactor — no task should change observable behavior, so a test
  failure or new clippy warning at any point means the move was incomplete or incorrect,
  not that a test needs updating.
- **Verbatim moves.** Every function/struct/const named in this plan moves with its
  existing implementation unchanged — only `use` paths and, where Rust requires it,
  visibility (`pub`/`pub(crate)`) change. If a task's implementer finds a genuine reason
  the body must change to compile after the move (e.g. a private helper the moved code
  depends on that itself needs to move too), that's expected and in scope for that same
  task — silently duplicating the helper instead of moving it is not.

---

## File Structure

**New crate**, added to the workspace `Cargo.toml`'s `members`:

```
crates/eqoxide-zone-geometry/
├── Cargo.toml
└── src/
    ├── lib.rs             — module declarations + crate-level doc
    ├── collision.rs        — Collision, SharedCollision, Hit, ClearanceField, and the
    │                         shared-bucket query/clearance/water-climb-accessor/
    │                         zone-line methods (Task 2)
    ├── diagnostics.rs       — SpokeReading, ProbeAnchor, CastZ, Placement,
    │                         ClearanceProbe, WaterDebug — the "live traversability
    │                         probe" types Collision::clearance_probe/body_placement
    │                         construct (discovered during Task 2; not in the original
    │                         investigation, since its producer wasn't flagged as
    │                         shared-bucket until this plan classified clearance_probe/
    │                         body_placement that way)
    ├── climb.rs             — moved wholesale (Task 3)
    ├── body.rs               — Body, PLAYER_BODY only — NOT Point/Tier, which stay in
    │                         eqoxide-nav (Task 4; see that task for why)
    ├── water_grid.rs        — WaterGrid/WaterColumn/ZoneWater/VRES + the corpus-sweep
    │                         coverage-test tooling (Task 5)
    └── zone_assets.rs       — moved wholesale, both read and write side (Task 6)
```

**Execution order note:** Tasks are numbered by conceptual weight (collision.rs is the
keystone the spec discusses first), not by required execution order.

Grep confirms `climb.rs` and `body.rs` have no *functional* dependency on `Collision` —
every mention of `Collision` in either file is inside a `//`/`///`/`//!` comment, added
for reader context, not a real call. So Task 3 (climb.rs) and Task 4 (body.rs) can land
before or after Task 2 with no compile-order constraint between them; do them first
(order: 1, 3, 4, ...) since they're the smallest, least risky moves and get them off the
board early.

Task 2 and Task 5 (collision.rs, water_grid.rs) are different: they're **mutually
dependent**, not just sequenced. `Collision::build_water_grid` (moving in Task 2) returns
`WaterGrid` by value — so `WaterGrid`'s definition (Task 5) must already be visible in the
new crate for Task 2's own move to compile. In the other direction, `water_grid.rs`'s
corpus-sweep tooling (`open_corpus_zone_with`, the `build32` test helper) calls
`Collision::build(...)` directly — so `Collision` (Task 2) must already be visible in the
new crate for Task 5's own move to compile. Neither task can reach a green build on its
own with the other still pending: this is a real two-file, same-crate cycle, which is
completely fine for Rust (modules in one crate compile as a unit) but means the two tasks
cannot each get their own independent "green at the end of the task" checkpoint as
Global Constraints would otherwise require.

**Resolution:** treat Task 2 and Task 5 as one combined execution unit. Do both files'
Steps 1-5 (the moves and internal-consumer fixups) back to back without expecting a green
build in between, then run Task 2's Step 6 and Task 5's Step 4 (build/test) once, against
the combined result. Commit once both are green — either as Task 2's commit followed
immediately by Task 5's commit (both from the same already-green tree), or squashed into
one commit if that reads more honestly; implementer's judgment. Full order: 1, 3, 4,
[2 and 5 together], 6, 7, 8.

**Modified**, `crates/eqoxide-nav/`:
- `Cargo.toml` — add a path dependency on `eqoxide-zone-geometry`
- `src/lib.rs` — module list shrinks as files empty out (Tasks 3, 5, 6 delete their
  source files entirely; Tasks 2, 4 leave a smaller file behind)
- `src/collision.rs` — shrinks to the A*-search/teleport-pad/climb-as-A*-edge machinery,
  now `use`-ing `Collision`/`ClearanceField` from the new crate instead of defining them
  (Task 2)
- `src/traversability.rs` — shrinks to `Traversability<'a>`/`HazardKind`/`Blockage`/
  `Point`/`Tier` (the latter two stay — see Task 4), now `use`-ing `Body`/`PLAYER_BODY`/
  `ClearanceField` from the new crate (Task 4)
- `src/diagnostics.rs` — loses its "live traversability probe" type cluster
  (`SpokeReading`, `ProbeAnchor`, `CastZ`, `Placement`, `ClearanceProbe`, `WaterDebug`) to
  the new crate; the `NavDebugSnapshot`/overlay-producing remainder stays (Task 2)
- `src/walker.rs`, `src/planner.rs`, `src/steering.rs` — untouched
  in substance; each gets its internal `use crate::...` paths updated for whichever
  symbols it references that moved (folded into the task that moves each symbol,
  not deferred to a separate cleanup task — see each task's Step list)
- `src/climb.rs`, `src/water_grid.rs`, `src/zone_assets.rs` — deleted (Tasks 3, 5, 6)

**Modified elsewhere in the workspace** (Task 7 — every in-workspace consumer outside
`eqoxide-nav` of a symbol that moved):
`src/lib.rs`, `src/movement.rs`, `src/camera_state.rs`, `src/hud.rs`, `src/zone_in.rs`,
`src/app.rs`, `src/model.rs`, `crates/eqoxide-net/src/action_loop.rs`,
`crates/eqoxide-agent-plugin-host/src/{lib.rs,session.rs,observation_builder.rs}`,
`crates/eqoxide-agent-vision-filter/src/lib.rs`, `tests/walker_sim.rs`.

## Interfaces (final shape, for reference across all tasks)

**`eqoxide-zone-geometry` public surface** (exact names, confirmed by grep against the
current `eqoxide-nav` source — an implementer should still verify each against the live
file before moving it, since a name can have gained/lost a sibling since this plan was
written):

```
collision:      Collision, SharedCollision, Hit, ClearanceField,
                 build, floor_z, nearest_floor, floor_beneath, ceiling_z,
                 column_surfaces, column_floors, nearest_hit_t, nearest_hit,
                 has_geometry, has_triangles, ground_below, descent_corridor_clear,
                 axis_scale, contact_tol, hit_accepted,
                 GROUND_DEPTH, GROUND_ORIGIN, GROUND_REACH_BELOW_FEET, MIN_RAY_LEN,
                 PLACEMENT_RING_DIRS,
                 footprint_clear, segment_blocked, line_clear, carrot_los_clear,
                 ground_continuous, edge_clear, path_clear, ground_margin_ok,
                 wall_clearance, ground_clearance, body_placement, clearance_probe,
                 clearance_field_for_test,
                 in_water, water_surface, build_water_grid, set_water_grid,
                 water_grid, climb_volumes, on_climbable, set_region_data, set_water,
                 region_data_absent,
                 zone_line_at, zone_line_at_standing, zone_line_indices,
                 find_zone_line_near, find_reachable_in_zone_line
climb:          CLIMB_SPEED, CLIMB_REACH, DISMOUNT_Z_TOL, is_climbable_name,
                 ClimbVolume, volumes_from_objects
body:           Body, PLAYER_BODY
diagnostics:    SpokeReading, ProbeAnchor, CastZ, Placement, ClearanceProbe, WaterDebug
water_grid:     VRES, WaterColumn, WaterGrid, ZoneWater
                 (test-only) WaterMeasurement, UNMEASURED, WaterRollup,
                 COMPOSITE_CLEAN, COMPOSITE_DIRTY, RollupReport, ZoneDropped,
                 open_corpus_zone, open_corpus_zone_with
zone_assets:    ZoneAssetStateShared, ZoneAssetState, NotUsable, usability,
                 usable_collision, lock_state, begin_zone_load, finish_zone_load
```

`ClearanceField` moves here (not "stays", despite `Traversability`/`HazardKind`/
`Blockage` all staying behind) because `Collision`'s own struct definition has a private
`clearance: ClearanceField` field, read by `Collision`'s own shared-bucket
`wall_clearance`/`ground_clearance` methods — and `ClearanceField::wall_at`/`ground_at`
both take `&Collision` as a parameter, so the two types must live in the same crate.
`Point`/`Tier` do NOT move, despite the original investigation listing them alongside
`Body`/`PLAYER_BODY` — see Task 4 for why.

**Stays in `eqoxide-nav`** (harness-bound, untouched by this plan — listed so an
implementer doesn't second-guess and move these by mistake):

```
collision:      Search, PlanCtx, PlanLimit, NoRoute, PlanOutcome, LocalOutcome,
                 find_path, find_path_res, find_path_ex, find_path_ex_tiered,
                 find_path_local, search_tiered_for_test, snap_goal_to_column_floor,
                 floating_goal_surface, goal_z_was_snapped, resolve_goal_floor,
                 walk_profile_ok, MAX_NODES, NET_TIER_NODE_CAP, PARTIAL_MIN_UNITS,
                 GOAL_TIER_TOL, GoalSnap,
                 PadEdge, resolve_teleport_pads, teleport_pad_footprints,
                 ClimbEdge, climb_edges, climb_plans, tight_plans,
                 facing_blind_surfaces
traversability: Traversability<'a>, HazardKind, Blockage, Point, Tier
diagnostics:    NavDebugSnapshot and everything else after the probe-type cluster (the
                 cluster itself — SpokeReading, ProbeAnchor, CastZ, Placement,
                 ClearanceProbe, WaterDebug — moves; see Task 2)
walker, planner, steering: unchanged in full
```

---

### Task 1: Scaffold the `eqoxide-zone-geometry` crate

**Files:**
- Create: `crates/eqoxide-zone-geometry/Cargo.toml`
- Create: `crates/eqoxide-zone-geometry/src/lib.rs`
- Modify: root `Cargo.toml` (add `crates/eqoxide-zone-geometry` to `[workspace] members`)

**Interfaces:**
- Consumes: nothing yet.
- Produces: an empty, compiling crate later tasks add modules to.

- [ ] **Step 1: Check `crates/eqoxide-nav/Cargo.toml`'s current dependency list**

Read it. The modules being extracted (`collision.rs`, `climb.rs`, `traversability.rs`,
`water_grid.rs`, `zone_assets.rs`) will need whatever subset of that dependency list they
actually use (almost certainly `eqoxide-assets` for `MeshData`/`TextureData`/
`ObjectModel`, `eqoxide-core` for coordinate types, and a math crate for vector types —
confirm exact crate names and versions from the file rather than guessing).

- [ ] **Step 2: Create the crate**

```toml
# crates/eqoxide-zone-geometry/Cargo.toml
[package]
name = "eqoxide-zone-geometry"
version = "0.1.0"
edition = "2021"

[dependencies]
# copy the exact entries eqoxide-nav uses for the modules moving in Tasks 2-6,
# with the same version pins
```

```rust
// crates/eqoxide-zone-geometry/src/lib.rs
//! Zone geometry construction and query logic shared between eqoxide's own client and
//! any external agent-harness project (docs/specs/2026-09-21-agent-harness-separation-design.md §8).
```

- [ ] **Step 3: Add to the workspace**

Add `"crates/eqoxide-zone-geometry"` to the root `Cargo.toml`'s `[workspace] members`.

- [ ] **Step 4: Verify it builds**

Run: `cargo build -p eqoxide-zone-geometry`
Expected: builds successfully (empty crate, nothing to fail on).

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-zone-geometry Cargo.toml
git commit -m "$(cat <<'EOF'
build: scaffold the eqoxide-zone-geometry crate

Empty crate, wired into the workspace. Tasks 2-7 of
docs/specs/2026-09-21-agent-harness-separation-plan-zone-geometry.md move
real content into it.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
```

---

### Task 2: Split `collision.rs` — the keystone move

This is the largest and riskiest task: `crates/eqoxide-nav/src/collision.rs` is ~11,400
lines (~4,900 production, the rest tests), and every other module in this plan depends on
the `Collision`/`Hit` types it defines. Every other task in this plan depends on this one
landing correctly first.

**Files:**
- Create: `crates/eqoxide-zone-geometry/src/collision.rs`
- Create: `crates/eqoxide-zone-geometry/src/diagnostics.rs`
- Modify: `crates/eqoxide-nav/src/collision.rs`
- Modify: `crates/eqoxide-nav/src/diagnostics.rs`
- Modify: `crates/eqoxide-nav/Cargo.toml` (add the new crate as a path dependency)
- Modify: `crates/eqoxide-nav/src/lib.rs` (module re-export adjustments if any; add
  `pub mod diagnostics;` in `eqoxide-zone-geometry`)
- Modify: `crates/eqoxide-nav/src/{walker,planner,steering,traversability,zone_assets}.rs`
  — only the ones that reference a moved `collision` symbol; update their `use` paths to
  `eqoxide_zone_geometry::collision::...`. (`climb.rs`, `water_grid.rs`, `zone_assets.rs`
  haven't moved yet at this point in the sequence — see the Execution order note above
  for climb.rs/water_grid.rs; `zone_assets.rs` still lives in `eqoxide-nav` and will need
  this same import fix now, then move itself in Task 6, carrying the already-correct
  import with it.)

**Interfaces:**
- Consumes: `ClimbVolume` (Task 3) and `Body`/`PLAYER_BODY` (Task 4) as return/parameter
  types on the moved methods — land Tasks 3 and 4 first (see the Execution order note
  above). Also mutually dependent on `WaterGrid` (Task 5) — `build_water_grid` returns it
  — see that note for why Task 2 and Task 5 must be executed as one combined unit rather
  than strictly sequenced.
- Produces: `eqoxide_zone_geometry::collision::{Collision, SharedCollision, Hit, ClearanceField, ...}` (full list above) — every later task in this plan, and every Task 7 consumer, imports from here.

- [ ] **Step 1: Locate `SharedCollision`'s definition**

The investigation that produced this plan's symbol lists didn't pin down exactly where
`SharedCollision` (the `Arc`/lock wrapper around `Collision` used throughout the
workspace — `docs/architecture.md`'s crate map names it as living in `eqoxide-nav`) is
defined. Grep `crates/eqoxide-nav/src/collision.rs` for `SharedCollision` to confirm its
exact definition and add it to the move list below — it belongs in the shared bucket
alongside `Collision` itself, since every one of its consumers (identified in Task 7) is
a shared-bucket consumer.

- [ ] **Step 2: Move the shared-bucket symbols verbatim**

Cut the following from `crates/eqoxide-nav/src/collision.rs` and paste into
`crates/eqoxide-zone-geometry/src/collision.rs`, unchanged:

- `Collision`, `SharedCollision` (per Step 1), `Hit`, `build`
- `floor_z`, `nearest_floor`, `floor_beneath`, `ceiling_z`, `column_surfaces`,
  `column_floors`, `nearest_hit_t`, `nearest_hit`, `has_geometry`, `has_triangles`,
  `ground_below`, `descent_corridor_clear`, `axis_scale`, `contact_tol`, `hit_accepted`
- `GROUND_DEPTH`, `GROUND_ORIGIN`, `GROUND_REACH_BELOW_FEET`, `MIN_RAY_LEN`,
  `PLACEMENT_RING_DIRS`
- `footprint_clear`, `segment_blocked`, `line_clear`, `carrot_los_clear`,
  `ground_continuous`, `edge_clear`, `path_clear`, `ground_margin_ok`, `wall_clearance`,
  `ground_clearance`, `body_placement`, `clearance_probe`, `clearance_field_for_test`
- `in_water`, `water_surface`, `build_water_grid`, `set_water_grid`, `water_grid`,
  `climb_volumes`, `on_climbable`, `set_region_data`, `set_water`, `region_data_absent`
- `zone_line_at`, `zone_line_at_standing`, `zone_line_indices`, `find_zone_line_near`,
  `find_reachable_in_zone_line`

Also cut `ClearanceField` (currently defined in `traversability.rs`, not `collision.rs`)
and move it into the new crate's `collision.rs` alongside `Collision`. It doesn't belong
in Task 4's `body.rs` move: `Collision`'s own struct definition has a private
`clearance: ClearanceField` field, read by the `wall_clearance`/`ground_clearance`
methods above, and `ClearanceField::wall_at`/`ground_at` both take `&Collision` as a
parameter — the two types are tightly coupled and must live in the same file/crate.
`eqoxide-nav`'s `traversability.rs` keeps its own, separate `Traversability<'a>`-scoped
`ClearanceField` field (per-plan memo, unrelated to `Collision`'s) — it just needs a `use
eqoxide_zone_geometry::collision::ClearanceField;` after this move instead of a local
definition.

`Collision`'s struct definition and impl blocks straddle both buckets (some of its
methods are A*-search-only and stay behind, per the "Stays in `eqoxide-nav`" list in
Interfaces above). Move the struct definition itself and only the listed methods; leave
the A*-only methods (`find_path*`, `resolve_teleport_pads`, `climb_edges`, etc.) as a
second `impl Collision` block in `eqoxide-nav`'s remaining `collision.rs`, which now
`use`s the struct from the new crate instead of defining it.

If a listed function calls a private (non-`pub`) helper not in this list, move that
helper too, keeping its original visibility. If the build reveals the reverse — a
still-in-`eqoxide-nav` A*-only method calling one of these newly-moved functions — that's
expected (this is exactly what "shared" means); the fix is an `eqoxide_zone_geometry::`
import in `eqoxide-nav`'s `collision.rs`, not moving the caller too.

- [ ] **Step 2b: Move `diagnostics.rs`'s probe-type cluster**

`Collision::body_placement`/`clearance_probe` (moved in Step 2 above) return
`Placement`/`ClearanceProbe` — types currently defined in `crates/eqoxide-nav/src/
diagnostics.rs`, not `collision.rs`. A crate can't define a method returning a foreign
downstream type, so this cluster has to move too, even though the rest of
`diagnostics.rs` (the `NavDebugSnapshot`/overlay-producing remainder) correctly stays in
`eqoxide-nav` per the Interfaces "stays" list above — the original investigation deferred
all of `diagnostics.rs` to a later plan, which was too broad a call for this one section.

Cut `SpokeReading`, `ProbeAnchor`, `CastZ`, `Placement`, `ClearanceProbe`, `WaterDebug`
(struct/enum/impl definitions, with their doc comments) from
`crates/eqoxide-nav/src/diagnostics.rs` into a new
`crates/eqoxide-zone-geometry/src/diagnostics.rs`, unchanged. Add `pub mod diagnostics;`
to `crates/eqoxide-zone-geometry/src/lib.rs`. In `eqoxide-nav`'s `diagnostics.rs`,
repoint every real call site that used these types to `eqoxide_zone_geometry::
diagnostics::{...}` directly — do not leave a `pub use eqoxide_zone_geometry::
diagnostics::{...};` re-export shim in `eqoxide-nav::diagnostics`, per the Global
Constraint against backwards-compat shims; every consumer (in this crate, and in Task 7's
list) gets its import repointed to the real new home.

- [ ] **Step 3: Wire the dependency**

Add to `crates/eqoxide-nav/Cargo.toml`:
```toml
eqoxide-zone-geometry = { path = "../eqoxide-zone-geometry" }
```

- [ ] **Step 4: Fix `eqoxide-nav`'s own remaining collision.rs**

Replace the removed definitions with:
```rust
use eqoxide_zone_geometry::collision::{
    Collision, SharedCollision, Hit,
    // ...full list from Step 2's move
};
```
Fix any A*-only method body that referenced a moved symbol without qualification.

- [ ] **Step 5: Fix `walker.rs`, `planner.rs`, `steering.rs`, `diagnostics.rs`**

Each references `collision::*` symbols today (walker.rs: 32 refs; steering.rs: 16;
planner.rs: 11; diagnostics.rs: indirectly via types it's handed). For each file, check
which of its `collision::` references are to shared-bucket symbols (now
`eqoxide_zone_geometry::collision::...`) versus A*-only symbols (still
`crate::collision::...` / `eqoxide_nav::collision::...`), and split the `use` statement
accordingly. `diagnostics.rs` additionally needs its own internal references to the
probe-type cluster it just lost (Step 2b) repointed to `eqoxide_zone_geometry::
diagnostics::{...}`.

- [ ] **Step 6: Build and test**

Run: `cargo build -p eqoxide-zone-geometry -p eqoxide-nav`
Expected: PASS. Fix compile errors by moving any additional private helpers Step 2
missed — do not duplicate.

Run: `cargo test -p eqoxide-zone-geometry -p eqoxide-nav`
Expected: PASS, same test count as before this task (tests moved with their subject
code; no test should have been dropped or newly failing).

- [ ] **Step 7: Commit**

```bash
git add crates/eqoxide-zone-geometry crates/eqoxide-nav Cargo.lock
git commit -m "$(cat <<'EOF'
refactor: extract Collision's shared-bucket surface into eqoxide-zone-geometry

Splits collision.rs by confirmed usage: the grid-construction, query,
clearance, water/climb-accessor, and zone-line-detection surface that
eqoxide's own client (movement, camera, hud, agent-plugin-host,
vision-filter) already depends on in production moves to the new
shared crate, along with ClearanceField (Collision's own struct field)
and diagnostics.rs's live-probe types (Placement/ClearanceProbe/etc.,
Collision::body_placement/clearance_probe's return types). A*-search,
teleport-pad-routing, and climb-as-A*-edge machinery stay in
eqoxide-nav, now depending on Collision as a foreign type from the new
crate instead of defining it locally.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
```

---

### Task 3: Move `climb.rs` wholesale

**Files:**
- Create: `crates/eqoxide-zone-geometry/src/climb.rs`
- Delete: `crates/eqoxide-nav/src/climb.rs`
- Modify: `crates/eqoxide-nav/src/lib.rs` (drop the `climb` module declaration)
- Modify: any `eqoxide-nav` file that referenced `crate::climb::*` (the investigation
  found `collision.rs` at 6 refs, `traversability.rs` at 1 ref — both already updated in
  Task 2/4's own steps if those land first; verify here regardless)

**Interfaces:**
- Consumes: nothing. Grep confirms every mention of `Collision`/`Hit` in this file is
  inside a comment (reader context on coordinate conventions), not a real call — so this
  task has no compile-order dependency on Task 2 (see the Execution order note above).
- Produces: `eqoxide_zone_geometry::climb::{CLIMB_SPEED, CLIMB_REACH, DISMOUNT_Z_TOL, is_climbable_name, ClimbVolume, volumes_from_objects}`.

- [ ] **Step 1: Move the file**

`git mv crates/eqoxide-nav/src/climb.rs crates/eqoxide-zone-geometry/src/climb.rs`,
then fix its internal `use` paths (anything it referenced via `crate::` or
`super::` now needs `eqoxide_zone_geometry::` or `crate::` depending on where the
referenced symbol itself now lives — its `collision`/`traversability` references should
already resolve via Task 2/4's moves).

- [ ] **Step 2: Update `eqoxide-nav/src/lib.rs`**

Remove the `pub mod climb;` (or equivalent) declaration.

- [ ] **Step 3: Update `eqoxide-zone-geometry/src/lib.rs`**

Add `pub mod climb;`.

- [ ] **Step 4: Fix remaining `eqoxide-nav` consumers**

`collision.rs`'s A*-only remainder (climb-as-A*-edge functions: `climb_edges`,
`climb_plans`, `tight_plans`, `facing_blind_surfaces`) likely still needs
`ClimbVolume`/`is_climbable_name` — import from `eqoxide_zone_geometry::climb`.

- [ ] **Step 5: Build and test**

Run: `cargo build -p eqoxide-zone-geometry -p eqoxide-nav`
Expected: PASS.

Run: `cargo test -p eqoxide-zone-geometry -p eqoxide-nav`
Expected: PASS, same test count as before this task.

- [ ] **Step 6: Commit**

```bash
git add crates/eqoxide-zone-geometry crates/eqoxide-nav
git commit -m "$(cat <<'EOF'
refactor: move climb.rs to eqoxide-zone-geometry

Ladder-volume derivation is pure geometry classification, needed both
by eqoxide's own real-time climb physics (src/movement.rs,
src/app.rs) and by A*'s climb-as-edge planning — a shared-bucket move
per docs/specs/2026-09-21-agent-harness-separation-design.md §8.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
```

---

### Task 4: Split `traversability.rs`'s `Body`/`PLAYER_BODY` into `body.rs`

The original investigation grouped `Body`, `PLAYER_BODY`, `Point`, and `Tier` together as
one shared-bucket move. Closer reading of the actual code (an authoritative in-code doc
comment, not a re-run of the investigation) shows `Point`/`Tier` don't belong: `Tier::
units()` calls `crate::collision::NAV_PREFERRED_CLEARANCE`, an A*-tuned constant, and an
existing comment on `Tier` explicitly ties it to "the controller-wiring phase" of legacy
`find_path*` plumbing. Both are planner/harness-bound, not client-shared geometry. Only
`Body`/`PLAYER_BODY` move — small enough, once trimmed, that the new file is named
`body.rs` rather than keeping `traversability.rs`'s name for a two-symbol subset.

**Files:**
- Create: `crates/eqoxide-zone-geometry/src/body.rs`
- Modify: `crates/eqoxide-nav/src/traversability.rs`
- Modify: `crates/eqoxide-nav/src/lib.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `eqoxide_zone_geometry::body::{Body, PLAYER_BODY}`.
  `eqoxide-nav::traversability::{Traversability<'a>, ClearanceField, HazardKind,
  Blockage, Point, Tier}` remains — `ClearanceField` is listed here as a *reference* to
  Task 2, which actually relocates it (see that task); `Point`/`Tier` genuinely stay and
  were never supposed to move. The remainder is now built on the new crate's `Body`.

- [ ] **Step 1: Move `Body`/`PLAYER_BODY` only**

Cut `Body` and `PLAYER_BODY` (the constant instance) from
`crates/eqoxide-nav/src/traversability.rs` into
`crates/eqoxide-zone-geometry/src/body.rs`, verbatim. Leave `Point` and `Tier` in place.

- [ ] **Step 2: Fix `eqoxide-nav`'s remaining traversability.rs**

`Traversability<'a>`, `HazardKind`, `Blockage`, `Point`, `Tier` stay (plus
`ClearanceField` until Task 2 relocates it — if Task 2 has already landed by the time
this task runs, per the Execution order note, `ClearanceField` is already gone from this
file; add `use eqoxide_zone_geometry::collision::ClearanceField;` instead). Add
`use eqoxide_zone_geometry::body::{Body, PLAYER_BODY};` for whatever this remainder still
references.

- [ ] **Step 3: Fix the confirmed production consumers outside eqoxide-nav**

Do NOT do the full external-consumer sweep here (that's Task 7) — but `collision.rs`
(within `eqoxide-nav`, already migrated by Task 2) references
`traversability::PLAYER_BODY.{near_horizontal,agent_height}` for its own
`NAV_NEAR_HORIZONTAL`/`NAV_AGENT_HEIGHT` constants. Fix that one in-crate reference now
(new path: `eqoxide_zone_geometry::body::PLAYER_BODY`) so `eqoxide-nav` keeps building;
Task 7 handles `src/movement.rs` and `eqoxide-agent-vision-filter`.

- [ ] **Step 4: Build and test**

Run: `cargo build -p eqoxide-zone-geometry -p eqoxide-nav`
Expected: PASS.

Run: `cargo test -p eqoxide-zone-geometry -p eqoxide-nav`
Expected: PASS, same test count as before this task.

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-zone-geometry crates/eqoxide-nav
git commit -m "$(cat <<'EOF'
refactor: move Body/PLAYER_BODY to eqoxide-zone-geometry

Body is the one shared geometry definition eqoxide's own real-time
character controller (src/movement.rs) and eqoxide-agent-vision-filter
must agree on with A*'s clearance probing (Traversability, staying in
eqoxide-nav) — this is the closer read
docs/specs/2026-09-21-agent-harness-separation-design.md §8 flagged as
needed before this file's split could be resolved. Point/Tier stay
behind: both are A*-tuned (Tier::units() reads
collision::NAV_PREFERRED_CLEARANCE) and tied to the legacy
find_path*-controller-wiring phase, not client-shared geometry.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
```

---

### Task 5: Move `water_grid.rs`'s runtime structure and test tooling

**Files:**
- Create: `crates/eqoxide-zone-geometry/src/water_grid.rs`
- Delete: `crates/eqoxide-nav/src/water_grid.rs`
- Modify: `crates/eqoxide-nav/src/lib.rs`

**Interfaces:**
- Consumes: `Collision::build` (called directly by this file's own corpus-sweep tooling,
  `open_corpus_zone_with`/the `build32` test helper) — and is in turn consumed BY
  `Collision::build_water_grid`'s return type. This is a genuine two-way coupling with
  Task 2, not a one-directional "already relocated" dependency — see the Execution order
  note above for why these two tasks must be executed as one combined unit.
- Produces: `eqoxide_zone_geometry::water_grid::{VRES, WaterColumn, WaterGrid,
  ZoneWater}`, plus (behind `#[cfg(test)]` or a `test-utils` feature — implementer's
  choice, consistent with how the crate handles other test-only exports, if any pattern
  already exists elsewhere in the workspace) `WaterMeasurement`, `UNMEASURED`,
  `WaterRollup`, `COMPOSITE_CLEAN`, `COMPOSITE_DIRTY`, `RollupReport`, `ZoneDropped`,
  `open_corpus_zone`, `open_corpus_zone_with`.

- [ ] **Step 1: Move the file**

`git mv crates/eqoxide-nav/src/water_grid.rs crates/eqoxide-zone-geometry/src/water_grid.rs`.

The corpus-sweep coverage tool (`WaterMeasurement`/`WaterRollup`/`open_corpus_zone`/
`RollupReport` family) moves with it rather than being split out — it exercises exactly
this file's own water-detection logic across the full zone corpus, so it belongs
alongside the code it tests, gated so it isn't part of the crate's normal public API
(check whether `eqoxide-nav` currently gates it with `#[cfg(test)]`, a `pub(crate)`
visibility, or a Cargo feature, and preserve whichever mechanism is already in use rather
than inventing a new one).

- [ ] **Step 2: Update module declarations**

Remove from `eqoxide-nav/src/lib.rs`, add to `eqoxide-zone-geometry/src/lib.rs`.

- [ ] **Step 3: Fix consumers within the crate**

`collision.rs`'s shared-bucket remainder (now in `eqoxide-zone-geometry`, per Task 2)
calls `build_water_grid`, `water_grid()`, `set_water_grid` — these already live in the
same new crate as of Task 2, so this should be a same-crate reference requiring no path
change; verify.

- [ ] **Step 4: Build and test**

Run: `cargo build -p eqoxide-zone-geometry -p eqoxide-nav`
Expected: PASS.

Run: `cargo test -p eqoxide-zone-geometry -p eqoxide-nav`
Expected: PASS, same test count as before this task.

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-zone-geometry crates/eqoxide-nav
git commit -m "$(cat <<'EOF'
refactor: move water_grid.rs to eqoxide-zone-geometry

WaterGrid/WaterColumn/ZoneWater are swim-auto-detection geometry, the
same shared-bucket reasoning as climb.rs. The corpus-sweep coverage
tool moves with it — it tests this file's own logic, not eqoxide- or
harness-specific behavior.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
```

---

### Task 6: Move `zone_assets.rs` wholesale

**Files:**
- Create: `crates/eqoxide-zone-geometry/src/zone_assets.rs`
- Delete: `crates/eqoxide-nav/src/zone_assets.rs`
- Modify: `crates/eqoxide-nav/src/lib.rs`

**Interfaces:**
- Consumes: `Collision` (already relocated by Task 2).
- Produces: `eqoxide_zone_geometry::zone_assets::{ZoneAssetStateShared, ZoneAssetState,
  NotUsable, usability, usable_collision, lock_state, begin_zone_load, finish_zone_load}`.

The whole file moves, including `begin_zone_load`/`finish_zone_load` (the write side),
even though today only `src/app.rs` calls them. The future harness runs its own
independent zone-load pipeline against the same on-disk asset cache
(`docs/specs/2026-09-21-agent-harness-separation-design.md §11`) and will need to
transition this same state machine itself, in its own process — there is no cross-process
publishing of `ZoneAssetState` involved; each side runs its own copy of this exact code
against its own load. This resolves the "cross-boundary tension" flagged during this
plan's research phase without introducing any new protocol capability.

- [ ] **Step 1: Check the `walker.rs` coupling first**

The investigation that produced this plan found one reference from `zone_assets.rs` to
`walker` (a single reference — most likely a `NAV_STATE_ZONE_LOADING`-style constant, but
confirm). If `zone_assets.rs` names a symbol defined in `walker.rs` (which is staying in
`eqoxide-nav`, harness-bound), moving `zone_assets.rs` wholesale would create a dependency
from the new shared crate back into `eqoxide-nav` — backwards, and something the harness
(which will depend on `eqoxide-zone-geometry` but not on `eqoxide-nav`) can't satisfy.
Resolve it before moving the file: most likely, the referenced constant itself is
shared-bucket in spirit (a load-state string both sides need) and should move into
`eqoxide-zone-geometry` alongside `zone_assets.rs`, with `walker.rs` updated to reference
it from there instead of defining it.

- [ ] **Step 2: Move the file**

`git mv crates/eqoxide-nav/src/zone_assets.rs crates/eqoxide-zone-geometry/src/zone_assets.rs`.

- [ ] **Step 3: Update module declarations**

Remove from `eqoxide-nav/src/lib.rs`, add to `eqoxide-zone-geometry/src/lib.rs`.

- [ ] **Step 4: Build and test**

Run: `cargo build -p eqoxide-zone-geometry -p eqoxide-nav`
Expected: PASS.

Run: `cargo test -p eqoxide-zone-geometry -p eqoxide-nav`
Expected: PASS, same test count as before this task.

- [ ] **Step 5: Commit**

```bash
git add crates/eqoxide-zone-geometry crates/eqoxide-nav
git commit -m "$(cat <<'EOF'
refactor: move zone_assets.rs to eqoxide-zone-geometry

The whole readiness-state machine, including its write side
(begin_zone_load/finish_zone_load), moves — not just the read side.
The future harness project runs its own independent zone-load
pipeline against the same on-disk asset cache and needs the same
state machine eqoxide uses today, not a cross-process publish of
eqoxide's own state.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
```

---

### Task 7: Repoint every remaining in-workspace consumer

Everything in `eqoxide-nav` itself was already fixed up as part of Tasks 2-6. This task
covers every OTHER workspace crate that imports a moved symbol via `crate::nav::...` or
`eqoxide_nav::...`.

**Files (all Modify):**
- `src/lib.rs` — the `pub use eqoxide_nav::traversability;` re-export stays pointed at
  `eqoxide_nav::traversability` — `Traversability<'a>`, `HazardKind`, `Blockage`,
  `Point`, `Tier` all still live there (only `Body`/`PLAYER_BODY` moved, into
  `eqoxide_zone_geometry::body`, per Task 4's correction). Check actual callers of
  `crate::traversability::Body`/`PLAYER_BODY` specifically and repoint just those to
  `eqoxide_zone_geometry::body`; everything else through this re-export is unaffected.
- `src/movement.rs` — `traversability::PLAYER_BODY` (9+ production call sites) →
  `eqoxide_zone_geometry::body::PLAYER_BODY` (not `traversability::PLAYER_BODY` — see
  Task 4), `collision::{Collision, Hit}`, `collision::{GROUND_DEPTH, GROUND_ORIGIN,
  GROUND_REACH_BELOW_FEET, PLACEMENT_RING_DIRS, MIN_RAY_LEN}` → `eqoxide_zone_geometry`.
  Its `#[cfg(test)]` module's `steering::carrot_along` and `water_grid::{WaterRollup,
  open_corpus_zone, COMPOSITE_CLEAN, COMPOSITE_DIRTY}` references split: `carrot_along`
  stays pointed at `eqoxide_nav::steering` (harness-only, unmoved), the `water_grid` test
  tooling repoints to `eqoxide_zone_geometry::water_grid`.
- `src/camera_state.rs`, `src/hud.rs` — `Collision` → `eqoxide_zone_geometry::collision`
- `src/zone_in.rs` — `Collision` → `eqoxide_zone_geometry::collision`
- `src/app.rs` — `zone_assets::{ZoneAssetState, lock_state, begin_zone_load,
  finish_zone_load}`, `climb::CLIMB_SPEED`, `collision` references →
  `eqoxide_zone_geometry`. Its `nav_debug_view: crate::nav::diagnostics::NavDebugView`
  field stays pointed at `eqoxide_nav::diagnostics` — `NavDebugView`/`NavDebugSnapshot`
  are the overlay-producing remainder that correctly stays behind (Task 2 only relocates
  the probe-type cluster — `SpokeReading`/`ProbeAnchor`/`CastZ`/`Placement`/
  `ClearanceProbe`/`WaterDebug` — not all of `diagnostics.rs`; verify this field doesn't
  itself construct one of the relocated types before assuming it's untouched).
- `src/model.rs` — its `#[cfg(test)] mod mock` (`MockModel`) uses `collision::{Collision,
  PlanCtx, PlanOutcome}`: `Collision` → `eqoxide_zone_geometry::collision`, `PlanCtx`/
  `PlanOutcome` stay `eqoxide_nav::collision` (A*-only, unmoved). This file ends up with
  both import paths — expected, not a sign something was classified wrong.
- `crates/eqoxide-net/src/action_loop.rs` — `zone_line_at_standing`/`find_zone_line_near`
  calls → `eqoxide_zone_geometry::collision`. Its `walker: eqoxide_nav::walker::Walker`
  field, and its `collision::{PlanOutcome, GoalSnap}`/`walker::{NAV_STATE_ZONE_LOADING,
  NAV_REASON_*}`/`diagnostics::NavDebugView` references, stay pointed at `eqoxide_nav`
  (all harness-bound, unmoved by this plan — Plan 2 relocates the `Walker`-owning
  `/goto`-driving logic itself, not this plan). This file straddles both buckets by
  design; both import paths are correct simultaneously.
- `crates/eqoxide-agent-plugin-host/src/{lib.rs,session.rs,observation_builder.rs}` —
  `SharedCollision`, `Collision::build` → `eqoxide_zone_geometry::collision`
- `crates/eqoxide-agent-vision-filter/src/lib.rs` — `SharedCollision`,
  `traversability::PLAYER_BODY.chest` → `eqoxide_zone_geometry::body::PLAYER_BODY.chest`
  (not `traversability::` — see Task 4)
- `tests/walker_sim.rs` — uses symbols from both buckets at once (`Collision`, `PlanCtx`,
  `PlanOutcome`, `steering::*`, `Traversability`, `PLAYER_BODY`,
  `water_grid::{open_corpus_zone, WaterRollup, RollupReport, ZoneWater}`): split its
  `use` block the same way `src/model.rs`'s does — `Collision`/`water_grid::*` →
  `eqoxide_zone_geometry::collision`/`water_grid`, `PLAYER_BODY` →
  `eqoxide_zone_geometry::body::PLAYER_BODY` specifically (not `traversability::` — see
  Task 4), `PlanCtx`/`PlanOutcome`/`steering::*`/`Traversability` stay `eqoxide_nav`. This
  test exercising the real `CharacterController` alongside harness-bound A* output as a
  fixture is a known, intentional coupling this plan doesn't try to resolve — just keep
  it compiling.
- **Verify, don't assume:** `crates/eqoxide-renderer/src/{nav_overlay.rs,scene.rs}` —
  the investigation that produced this plan confirmed these consume `diagnostics::*`, but
  didn't do a symbol-level check for any direct shared-bucket reference (e.g. a raw
  `Collision` query for the render pass itself) — and now that Task 2 relocates
  `diagnostics.rs`'s probe-type cluster (`SpokeReading`/`ProbeAnchor`/`CastZ`/`Placement`/
  `ClearanceProbe`/`WaterDebug`), check specifically whether either file constructs or
  matches on one of those six types directly (vs. only consuming `NavDebugSnapshot`,
  which stays). Grep both files for `eqoxide_nav::` / `crate::nav::` during this task and
  repoint anything found.

**Interfaces:**
- Consumes: every symbol produced by Tasks 2-6.
- Produces: nothing new — this task only repoints existing call sites.

- [ ] **Step 1: Grep for every remaining reference**

Run: `grep -rn "eqoxide_nav::\|crate::nav::\|crate::traversability::" src/ crates/ tests/ --include='*.rs' | grep -v 'crates/eqoxide-nav/'`

This should surface exactly the file list above (plus anything the investigation missed
— treat any surprise as real signal, not noise).

- [ ] **Step 2: Fix each file per the list above**

One `use` block at a time; this can be one commit per file or a single batched commit,
implementer's judgment — these are same-shape mechanical import fixes, not independent
judgment calls, so batching into one commit is reasonable here (see this project's usual
guidance on batching same-shape work).

- [ ] **Step 3: Build and test the full workspace**

Run: `cargo build --workspace`
Expected: PASS, zero errors.

Run: `cargo test --workspace`
Expected: PASS, full suite green, same test count as before Task 1 started (this whole
plan is a refactor — no test should have been added, removed, or changed in behavior).

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: zero warnings.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
refactor: repoint remaining workspace consumers at eqoxide-zone-geometry

Every in-workspace import of a symbol Tasks 2-6 relocated now points
at eqoxide-zone-geometry directly — no compatibility re-export left
behind in eqoxide-nav.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01UQTnMEjMF8Y7G5ZUeibYRE
EOF
)"
```

---

### Task 8: Final verification

**Files:** none (verification only).

- [ ] **Step 1: Full workspace build, test, and lint**

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: all three clean.

- [ ] **Step 2: Confirm no dead re-exports or orphaned modules**

Grep once more for `eqoxide_nav::` outside `crates/eqoxide-nav/` to confirm Task 7 caught
everything. Grep `crates/eqoxide-nav/src/lib.rs` for any module declaration whose file no
longer exists (Tasks 3, 5, 6 deleted `climb.rs`/`water_grid.rs`/`zone_assets.rs`
entirely).

- [ ] **Step 3: Confirm this plan's own claim — no behavior change**

`git diff <BASE>..HEAD --stat` against the commit before Task 1 started: every changed
file should be either inside `crates/eqoxide-zone-geometry/` (new) or an import-path-only
diff elsewhere. If any file's diff touches actual logic (not just `use` statements or
`mod` declarations), that's a plan violation worth flagging explicitly rather than
quietly committing — this task was scoped as a pure refactor.

- [ ] **Step 4: Report**

No commit for this task (verification only) — this is the plan's exit gate for a
finishing-a-development-branch-style review, or the handoff point to Plan 2 (client
scope reduction, which starts using this crate to actually change eqoxide's behavior).

---

## Notes for Plan 2 (not part of this plan)

- `diagnostics.rs`'s remaining fate (the nav-debug 3D overlay — `NavDebugSnapshot` and
  everything downstream of it, currently produced by `walker.rs` and consumed by
  `eqoxide-renderer`'s `nav_overlay.rs`/`scene.rs`) is untouched here. This is a narrower
  claim than an earlier draft of this plan made: the original investigation deferred ALL
  of `diagnostics.rs` to Plan 2, but Task 2 (this plan) already relocates its "live
  traversability probe" type cluster (`SpokeReading`, `ProbeAnchor`, `CastZ`, `Placement`,
  `ClearanceProbe`, `WaterDebug`) to `eqoxide-zone-geometry`, because `Collision::
  body_placement`/`clearance_probe` — themselves shared-bucket — construct and return
  them; a crate can't define a method returning a type from a crate downstream of it. Only
  the snapshot/overlay remainder still defers. `walker.rs` isn't moving in this plan, so
  its producer still exists and the overlay still works exactly as today. Plan 2, which
  does relocate `walker.rs` to the harness, needs to make an explicit call on this: drop
  the overlay from eqoxide entirely (it was always a developer tool, never something a
  human *player* sees, which fits this redesign's own "only what a player experiences"
  principle better than any alternative — this plan's author's recommendation, not a
  decision made here), rebuild it as a harness-side debug visualizer, or add a
  harness-to-eqoxide reverse channel the spec never designed. Flag this for explicit user
  sign-off in Plan 2's write-up rather than deciding it silently.
