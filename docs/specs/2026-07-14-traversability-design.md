# Traversability: one abstraction, pluggable hazards (#378)

**Status:** approved; Phase 1 built (`src/traversability.rs` — the `Body`/`Tier`, the
`Traversability` façade, and the `ClearanceField` `MemoField`). Phase 2 (controller wiring, divergent-
copy deletion, `BlockedBy` → `/v1/observe/debug`, wall-aware inset) is the next branch.
**Scope:** `src/assets.rs` (A*, clearance tests), `src/movement.rs` (the controller),
`src/eq_net/navigation.rs` + `src/eq_net/nav_planner.rs` (the two planner tiers), and a new
`src/traversability.rs`. It does **NOT** touch `src/navmesh.rs`.

> **The navmesh line is CANCELLED — do not re-import it.** #372 (the `worktree-navmesh-harness`
> navmesh + Recast-style bake) is closed, its "Phase 2" cancelled. This abstraction was built with
> **zero navmesh code** and does not depend on it. Where this document cites #372's *measurements*
> (bake times, cache sizes, the `TARGET_COLUMNS` clamp) it does so as **historical data** to size the
> clearance field, NOT as an instruction to lift `navmesh.rs`. The optional Phase-4 `BakedField`
> (§3d, §7 PR-5) is an eager bake **of this same `ClearanceField`** — same graded wall/ground
> distances, same query surface as the shipped `MemoField` — **not** a revival of the navmesh harness.
> Any file:line reference to `navmesh.rs` below is a pointer into that dead branch for provenance
> only; a future agent must re-derive the equivalent, not resurrect the harness.
**Closes / unblocks:** #378 (this), #358 (residual), #379, #381, #375, #312's half-fix. Does *not* close #382.

---

## 0. Provenance rules for this document

Every number below is one of three kinds, and it is labelled:

* **[cited]** — traceable to an issue, a PR body, or a `file:line`.
* **[derived]** — arithmetic I did here from cited constants. Reproducible, but not measured.
* **[guess]** — I could not trace it and did not measure it. **Re-derive before relying on it.**

There are no unlabelled numbers. If you find one, it is a bug in this document.

---

## 1. What is actually true today (read from `main` @ `950d32f`)

Two things have changed since #378 was written, and both change the design.

### 1a. Half of #378 already shipped

PR #376 and #377 landed the parts #378 listed as blockers, and they landed *more* of #378 than the
issue anticipated:

| #378 asks for | status on `main` today |
|---|---|
| Honest plan outcome (`PlanOutcome`) | **done** — `assets.rs:621` (`Route` / `Unreachable(NoRoute)` / `Exhausted{limit, progress}`) |
| Tiered clearance, generous → minimum | **done** — `search_tiered`, `assets.rs:1649-1681` |
| `PLAYER_RADIUS` as a hard floor | **done** — `assets.rs:1652` (`radius.max(PLAYER_RADIUS)`) |
| "If a route required the tight tier, say so" | **partially** — a *zone-lifetime counter*, not a per-route fact (see §4c) |
| Volume-sweep clearance instead of a ray | **done for the fine tier** — `path_clear`, `assets.rs:1451`; `edge_clear` picks sweep-vs-ray by cell size (`assets.rs:1419`) |
| One type answers "can I be here / go there" | **not started** — this is what remains |
| `BlockedBy(hazard, position)` | **not started** |
| Static hazards baked into a clearance field | **not started** (exists only in #372, on a branch) |
| Dynamic hazards as first-class | **not started** — still `avoid: &[[f32;2]]` + `aggro_buffer`, `nav_planner.rs:46-48` |

So this refactor is **not** "build tiered clearance." It is: **collapse the remaining four
mutually-blind predicates into one, and give the refusal a name and a position.**

### 1b. The four predicates, as they exist right now

| # | predicate | `file:line` | sees | blind to |
|---|---|---|---|---|
| 1 | cell walkability | `assets.rs:2128` (`column_floors` per neighbour) | is there *a floor* in the neighbour column at a reachable height | the character's radius, entirely |
| 2 | `edge_clear` → `path_clear` / `line_clear` | `assets.rs:1419`, `:1451`, `:1385` | walls the segment **crosses**, swept at 5 feelers × the caller's *one* height | walls the segment runs **parallel** to (#381); **anything above 3.0u** (§1c) |
| 3 | `ground_margin_ok` (ledge margin) + `edge_ok` (the #312 inset) | `assets.rs:1500`, `:2445` | floor **running out** — drops, bridge lips, waterlines | **walls** — `column_hits` discards every tri with `tri_nz <= 0` (`assets.rs:684`), so a vertical face can never be a `nearest_floor` hit. The code says so itself at `assets.rs:2436-2441` and `:2461-2470`. |
| 4 | `avoid` / `aggro_cost` | `assets.rs:2023-2032` | mob XY positions, as a soft cost | it is not part of walkability at all; and the **fine tier passes `&[]`** (`navigation.rs:2730`) — the tier that actually steers the character is **blind to every mob** |

Predicate 3's blindness is the one #378 called out as "documented as fixed, not fixed." It is worse
than the issue says: it is blind to walls **and** the fine local tier never even asks it —
`ledge_margin` is `0.0` at `PLAYER_RADIUS` (`assets.rs:2006-2010`), and the fine tier always plans at
exactly `PLAYER_RADIUS` (`navigation.rs:2730`). **The tier the walker steers along has no ledge safety
whatsoever.** That is the direct mechanism of "walking near edges is how you fall off them."

### 1c. A drift I found while reading — and it is live on `main`

> **This is the single most load-bearing finding in this document.** It should be filed as its own
> issue (a ready-to-file body is in Appendix A). I was not authorised to file it; the owner should.

The planner and the controller probe **different heights** and use **different feeler patterns**:

| | lateral feelers | probe heights above the floor | radius |
|---|---|---|---|
| **planner** (`path_clear`, `assets.rs:1451`) | **5**, at `[-r, -r/2, 0, +r/2, +r]` (`assets.rs:1480`) | A* calls it at **`cz+2.5`** (`FEET_CLR = STEP_UP + 0.5`, `assets.rs:2144`) and **`cz+3.0`** (`CHEST`, `assets.rs:1984`) | the plan clearance (1.0 or 2.0) |
| **controller** (`CharacterController::slide`, `movement.rs:349-386`) | **0** — one centre ray, then back off `PLAYER_RADIUS / ndot` along the hit normal (`movement.rs:375`) | **`foot+0.5`** and **`foot+4.0`** (`movement.rs:350-351`) | `PLAYER_RADIUS` = 1.0 |

Consequences:

1. **The planner never probes above 3.0u.** The character's cylinder is ~6u tall (`assets.rs:1330`).
   Geometry occupying only **z ∈ (3.0, 4.5]** above the floor — an overhead beam, a chest-height
   railing, the underside of a low arch — is **clear to the planner and solid to the walker**. The
   planner hands the walker a route it cannot follow. That is #358's exact signature, in the height
   axis, still live. **[cited: the two constant sets above]**
2. **`Collision::sweep` (`assets.rs:1321`) has zero production callers.** `grep -rn '\.sweep('` over
   `src/` returns only `assets.rs:4006` and `assets.rs:4012` — both unit tests. `slide` implements its
   own thing.
3. Therefore `path_clear`'s doc comment (`assets.rs:1440-1445`) — *"the exact feeler pattern
   `Collision::sweep` … uses — the planner and the controller now share one collision model, so …
   cannot drift apart"* — **is false on both halves.** `sweep` is not the mover's model, and the
   patterns differ anyway (3 feelers × {0.5, 4.0} vs 5 feelers × one caller height).

This is #312's failure mode repeating: **a comment that documents a fix broader than the code makes.**
It is exactly the class of thing this refactor exists to make impossible.

The lateral axis is *less* dangerous than it looks — the planner (5 feelers at `radius ≥ 1.0`) is
**more** conservative than the controller (centre ray + back-off), so planner-clear mostly implies
controller-passable laterally, modulo #381. **The height axis is the unsafe direction**, and it is
unsafe in the fatal orientation: the planner is the *permissive* one.

---

## 2. The type shape

### 2a. Critique of the owner's sketch

```rust
// The owner's proposal (#378)
trait Hazard {
    fn blocks(&self, pos: [f32; 3], r: f32) -> bool;
    fn name(&self) -> &'static str;
}
struct Traversability { hazards: Vec<...> }
impl Traversability {
    fn can_occupy(&self, pos: [f32;3], clearance: f32) -> Result<(), BlockedBy>;
    fn can_traverse(&self, from: [f32;3], to: [f32;3], clearance: f32) -> Result<(), BlockedBy>;
}
```

Four objections, each with a fix.

**(i) `blocks() -> bool` cannot produce `BlockedBy(hazard, position)`, and patching it to try is the
wrong move.** You could recover the *hazard* by looping detectors and calling `name()` on the one that
returned `true` — but you cannot recover the **position**, especially for `can_traverse`, where the
blockage is somewhere *along* a segment and a `bool` has thrown that away. The tempting fix is to
change the return to `Option<Blockage>`. **Do not.** That puts an `Option<struct>` — and the work to
populate it — on a path that runs ~10⁷ times per plan (§3a) *and it is wasted every time the route
succeeds*, which is the common case.

**The reconciliation: split hot from cold.** The WHY is never computed on the hot path. It is
**reconstructed** afterwards, by re-running the detectors at the one or two points we actually want to
explain (§4).

```rust
pub trait Hazard {
    /// HOT. Called ~10^7 times per plan. bool, no allocation, no diagnosis, monomorphised.
    fn clear(&self, body: &Body, at: Point, tier: Tier) -> bool;

    /// COLD. Called at most twice per FAILED plan, never on a successful one.
    /// MUST agree with `clear`: `clear(..) == diagnose(..).is_none()`. Property-tested (§5c).
    fn diagnose(&self, body: &Body, at: Point, tier: Tier) -> Option<Blockage>;

    fn kind(&self) -> HazardKind;
}
```

This is the whole answer to "give me `BlockedBy` for free": it *is* free, because it is not computed
until something has already failed.

**(ii) `dyn` is the wrong dispatch, but so is "enum instead of dyn" as a blanket rule.** The right
answer is: **static hazards do not exist at query time at all.** Floor / Wall / Water are *compiled
away* into a clearance field at zone load (§3). What survives to runtime is a small, closed set of
dynamic hazards — so:

```rust
pub struct Traversability {
    /// Floor + Wall + Water, already resolved into a lookup. THE hot path.
    field:   Arc<ClearanceField>,
    /// Mobs / players / danger. A CLOSED set — an enum, not `dyn`. Evaluated at plan-request
    /// cadence, not per A* edge.
    dynamic: Vec<DynamicHazard>,
}

pub enum DynamicHazard {
    Mobs   { pts: Arc<[[f32; 2]]>, radius: f32, severity: Severity },
    Players{ pts: Arc<[[f32; 2]]>, radius: f32, severity: Severity },
    Danger { zones: Arc<[DangerZone]>,          severity: Severity },
}

/// A dynamic hazard is normally a COST, not a wall — today's aggro avoidance is soft and "never
/// becomes no route" (assets.rs:2012-2020), and that is correct: a rat standing on a corridor must
/// not make a zone unroutable. `Block` exists only for a danger zone the AGENT declared.
pub enum Severity { Cost(f32), Block }
```

`dyn Hazard` buys extensibility nobody needs (no third-party hazard plugins exist or will), and costs
a vtable hop plus a pointer chase in the hottest loop in the client. **Use `dyn` only on the cold
`diagnose` path**, where it runs twice per failed plan and the ergonomics are worth it.

**(iii) `clearance: f32` is the type that let #310 happen.** A free float is exactly what permitted a
`0.5 × PLAYER_RADIUS` fallback to plan routes the character could not fit through. Per the repo's
verification hierarchy (tier 1: *make the bad state unrepresentable*), do not guard it — **remove the
representation**:

```rust
/// The only two clearances that exist. There is no third, and no float constructor.
/// #310: a sub-radius plan is not a "lower tier", it is a lie. It is not expressible here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// PLAYER_RADIUS * 2.0 (assets.rs:741). One radius to fit, one radius of standing room.
    Preferred,
    /// Exactly PLAYER_RADIUS (movement.rs:13). The character fits, with nothing to spare.
    /// THE HARD FLOOR. Below this the honest answer is `no_path`.
    Minimum,
}
impl Tier {
    pub fn units(self) -> f32 {
        match self {
            Tier::Preferred => crate::assets::NAV_PREFERRED_CLEARANCE,
            Tier::Minimum   => crate::movement::PLAYER_RADIUS,
        }
    }
    /// The retry ladder, in order. Exactly two rungs, forever.
    pub const LADDER: [Tier; 2] = [Tier::Preferred, Tier::Minimum];
}
```

`search_tiered` currently gets this right *by a guard* (`radius.max(PLAYER_RADIUS)`, `assets.rs:1652`).
A guard can be removed by a future edit; a type cannot.

**(iv) `Traversability` must own the BODY, not take a bare radius.** §1c exists because the character's
volume is re-declared in four places. One definition, consumed by planner *and* controller:

```rust
/// The character's collision volume. THE single source of truth. Planner and controller both
/// derive their probes from this — that is the whole point (#358, and the height drift in §1c).
pub struct Body {
    /// movement.rs:13 — native RoF2 wall-collision sphere radius.
    pub radius: f32,
    /// Heights above the feet at which the volume is probed. Today these are FOUR different
    /// hardcoded sets (movement.rs:350-351, assets.rs:1331-1332, assets.rs:1984, assets.rs:2144).
    /// After this change there is ONE, and both the sweep and the slide read it.
    pub probes: &'static [f32],
    /// Total height. The planner's tallest probe today is 3.0u against a ~6u cylinder (§1c).
    pub height: f32,
}
pub const PLAYER_BODY: Body = Body {
    radius: crate::movement::PLAYER_RADIUS,
    probes: &[0.5, 2.5, 4.0],   // <- to be SET BY MEASUREMENT, not by me. See Q6.
    height: 6.0,
};
```

### 2b. The recommended shape, entire

```rust
pub struct Point { pub xy: [f32; 2], pub floor_z: f32 }   // a standing position, not a free 3-vec

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HazardKind { Floor, Wall, Water, Mob, Player, Danger }

/// WHY a position or a segment was refused, and WHERE. The agent-honesty payload.
#[derive(Clone, Copy, Debug)]
pub struct Blockage { pub hazard: HazardKind, pub at: [f32; 3] }

/// The refusal, as returned to a caller. `Result<(), BlockedBy>` — the Ok case is a ZST, so a
/// successful query allocates nothing and branches once.
pub type BlockedBy = Blockage;

impl Traversability {
    // ── HOT: what A* calls. No Result, no Option, no diagnosis. ──
    pub fn can_occupy_fast (&self, p: Point, t: Tier) -> bool;
    pub fn can_traverse_fast(&self, a: Point, b: Point, t: Tier) -> bool;

    // ── COLD: what the failure path calls. At most twice per failed plan. ──
    pub fn can_occupy (&self, p: Point, t: Tier) -> Result<(), BlockedBy>;
    pub fn can_traverse(&self, a: Point, b: Point, t: Tier) -> Result<(), BlockedBy>;
}
```

The `_fast` / diagnostic pair is the only load-bearing asymmetry in the design, and §5c specifies the
property test that stops them drifting.

**Why `Point` and not `[f32;3]`:** a free 3-vector is what let #229 happen (a zone-line region's
*interior volume point* was treated as a floor height). `Point { xy, floor_z }` says, in the type, that
the z is a **surface you stand on**, not an arbitrary altitude. The conversion from a caller's sloppy
`[f32;3]` goes through `snap_goal_to_column_floor` (`assets.rs:1553`) — which already reports the snap
rather than performing it silently.

---

## 3. The perf split, concretely

### 3a. What the inner loop costs today  **[derived, from cited constants]**

Per **expanded node**, A* (`assets.rs:2116-2164`) does, for each of 8 neighbours:
* one `column_floors` (a vertical ray sweep of the column, `assets.rs:1513`), then
* per surface in that column: **two `edge_clear` calls** (`assets.rs:2145-2146`), each of which —
  on the fine tier, where `cell = 2.0 ≤ SWEPT_EDGE_MAX_CELL = 4.0` (`navigation.rs:561`,
  `assets.rs:773`) — is a `path_clear` = **5 ray casts** (`assets.rs:1480`).

So ≥ **8 × 2 × 5 = 80 ray casts per node** on the fine tier (16 on the coarse, where `edge_clear`
degrades to a single `line_clear`), *before* counting the jump / water-descent / water-ascent /
water-surface / controlled-fall edges, each of which adds its own `column_floors` and `edge_clear`
calls (`assets.rs:2166-2365`).

`MAX_NODES = 1_000_000` (`assets.rs:1993`). Worst case ≈ **8 × 10⁷ ray casts per plan**. The owner's
"millions of evaluations" is, if anything, an understatement. **[derived]**

### 3b. Which budget still binds — precisely

| plan | thread | budget | binding? |
|---|---|---|---|
| **coarse** (8u, whole zone) | worker (`nav_planner.rs`) | `WORKER_PLAN_BUDGET_MS = 5_000` (`assets.rs:508`) — an explicit *safety net*, "nothing real-time waits on it" | **no** |
| **fine** (2u cell, 40u bound, **every nav tick**) | **NETWORK THREAD** (`navigation.rs:2729`) | `NET_TIER_BUDGET_MS = 150` (`assets.rs:522`), re-armed each tick; `NAV_TICK_MS = 150` (`navigation.rs:7`) | **YES** |

#377 removed the coarse tier's budget. **It did not remove the fine tier's, and #382 is open about
exactly that.** So the constraint is *relaxed, not lifted*:

* The **fine tier's 150 ms, on the net thread, every 150 ms** is the budget this design must live
  inside. #382 measures it at **mean 15.3 ms, worst 358 ms (release, akanon)** **[cited: #382]** —
  note the worst case *already exceeds its own budget*, because the deadline is only checked every 32
  expansions (`assets.rs:2101`).
* The coarse tier's 5 s is slack. A design that is fast enough for the fine tier is trivially fast
  enough for the coarse one.

**Therefore the static-hazard query must be O(1) with a small constant. Not "a few rays." A lookup.**

### 3c. The baked artifact — what it is, exactly

**A clearance field: for each (cell, surface), the horizontal distance to the nearest thing you cannot
stand in.** Not a boolean.

```rust
pub struct ClearanceField {
    origin: [f32; 2],
    cell:   f32,                 // 2.0 in dungeons; coarsened in huge outdoor zones (see below)
    keys:   Vec<u64>,            // sorted (col,row) — CSR, exactly navmesh.rs's layout
    offsets:Vec<u32>,
    surf_z: Vec<f32>,            // the standable height
    /// Clearance in 0.25u quanta, saturating at 63.75u. 0 = you cannot stand here at all.
    /// A GRADED DISTANCE, deliberately not #372's boolean FLAG_EDGE.
    clear:  Vec<u8>,
    flags:  Vec<u8>,             // WALK | SWIM | STEEP  (navmesh.rs:121-139)
}
```

The query the whole design rests on:

```rust
// can_occupy_fast, in full:
field.clearance_at(cell, floor) >= tier.units()
```

One binary search on the column key, one array index, one compare. **That is the inner loop.**

**Why a graded distance and not #372's `FLAG_EDGE` boolean.** #372 marks `FLAG_EDGE` on any surface
within `agent_radius` of a wall/ledge/waterline (`navmesh.rs:535-570`, branch
`worktree-navmesh-harness`) and uses it as a **penalty**, `EDGE_PENALTY = 6.0` (`navmesh.rs:747`,
`:1001`). A boolean at *one* radius cannot answer the tiered question — you would need a second bake
per tier. But look at `mark_edges`: it already runs a **multi-source BFS that computes `dist[si]` in
cells** and then *throws the distance away* to make a bool (`navmesh.rs:565` (`dist[si] < rad_cells` → bool)). **Keep the distance.**
Tiered clearance then becomes a compare against a *number*, and the retry ladder costs nothing extra —
no second bake, no second field.

**Why voxel occupancy unifies Floor and Wall (the thing that makes this whole issue tractable).** In
the span-grid model, a surface is standable iff it has `agent_height` of open air above it. A wall's
footprint column therefore has **no standable surface at floor height** (the wall's own voxels fill the
headroom), so — from a neighbouring floor cell — a **wall reads exactly like a missing floor**.
`mark_edges`'s test *"does my 4-neighbour column have a surface within `max_climb` of me?"*
(`mark_edges`, `navmesh.rs:535-570`) is therefore **one test that catches walls, ledges, drops, and waterlines at
once.** That is the unification #378 is asking for, and it falls out of the representation rather than
being assembled from four predicates. This is the strongest single argument for the field.

**Cost.**

* **Bake time:** #372 measured its full bake (voxelise → filter → edge-mark → components) at
  **median 0.5–0.9 s, max 5.6 s (everfrost)** across 34 cached zones **[cited: PR #372 body]**. The
  clearance field is stages 1–3 of that, so its bake is a *subset* of the cited figure. Zone GLB load
  is already ~10 s, so this is not a new user-visible wait.
* **Disk:** #372's cache is **mean ~900 KB/zone, max 3.1 MB (~180 MB for ~200 zones)**
  **[cited: PR #372 body]**. The on-disk record is 5 bytes/surface + 12 bytes/column, deflated
  (`navmesh.rs:923-941`). Adding one `u8` of clearance is **+1 byte/surface, pre-compression**
  **[derived]** — a ~20 % growth of the surface array, well inside the cited envelope.
* **RAM:** dominated by the same arrays. **[derived]** — not separately measured; measure it.
* **Big outdoor zones:** #372 found gfaydark at a fixed 2 u cell bakes **5.9 M columns → 29 s bake,
  33 MB cache, 30 s worst-case query — all unacceptable**, and solves it by *coarsening the cell* to
  hold ~`TARGET_COLUMNS = 300_000` (`navmesh.rs:88-108`; `TARGET_COLUMNS = 300_000` at `:102`). **[cited: navmesh.rs comments on
  branch `worktree-navmesh-harness`]** Any bake in this design **must** inherit that clamp, or it
  reintroduces a 30 s zone-load stall.

**Where it is cached, and how it is invalidated.** Exactly as #372 already does
(`navmesh.rs:868`, `:947-960`): on disk, keyed by `blake3(zone GLB bytes ‖ bake params)`. A changed
asset or a retuned parameter yields a different digest, the cached blob is **rejected**, and the zone
re-bakes. Nothing silently paths on a stale field. Dynamic hazards are **not** in the field and are
**never** cached — they are snapshotted per plan request from the spawn list, as `avoid` already is
(`navigation.rs:2602`).

### 3d. The no-bake path — and why I recommend shipping it FIRST

**The design does not require a bake, and it does not require the navmesh.** A `ClearanceField` is an
*interface*, and there are two implementations:

* **`MemoField` (no bake).** `clearance_at(cell, floor)` computed on demand by a **radial disc probe**
  (rays *outward* from the standing point at increasing radii — `footprint_clear`, `assets.rs:1361`,
  is already exactly this shape), memoised per `(col, row, floor_bucket)` for the zone's lifetime.
  * It **closes #381 for free**: #381 is specifically that `path_clear`'s feelers run *parallel* to
    travel and so never intersect a wall alongside the path. A **radial** probe casts rays in every
    direction, so a parallel wall is crossed by a spoke. The blind spot moves from "any wall parallel
    to travel" (a plane) to "a needle thinner than the spoke spacing" (measure-zero in real zone art —
    the same approximation family `path_clear` already accepts, `assets.rs:1466-1470`).
  * The memo turns the inner loop's per-*edge* cost (10 rays) into a per-*cell* cost paid once
    (`assets.rs:2011`'s `margin_ok` map already does this, but only *per plan* and only for the ledge
    probe — generalise it to per-zone and to all static hazards).
  * **The risk, stated plainly:** an unbounded per-zone memo is a memory leak in a big outdoor zone.
    gfaydark has **5.9 M columns at a 2 u cell** **[cited: `navmesh.rs:92`, branch `worktree-navmesh-harness`]**; a `HashMap` entry per
    visited (cell, floor) at ~16 B is **~95 MB if the character walks the whole zone** **[derived]**.
    Mitigation: a fixed-capacity table with eviction, cleared on zone change, plus a hit-rate metric.
    **This is Q1 for the owner.**

* **`BakedField` (the #372 path).** The array above, baked at zone load, `O(1)` query, no memory
  surprise. Lift stages 1–3 of `navmesh.rs` — **not** its planner.

**Recommendation: ship `MemoField` first (PR-4), measure, and take `BakedField` only if the numbers
demand it (PR-5).** That keeps "bank the working grid and drop the navmesh" a live option for the
owner right up to the last PR, which is what he asked for.

---

## 4. Tiered clearance

### 4a. The ladder

```rust
for tier in Tier::LADDER {                 // [Preferred, Minimum]. Exactly two. No third rung.
    match search(start, goal, tier) {
        Route(p) => return (Route(p), tier),
        _        => continue,
    }
}
return Unreachable(SearchClosed { .. });   // the honest no_path (#310)
```

This is what `search_tiered` (`assets.rs:1649`) already does — the change is that the rungs become a
`Tier`, not an `f32`, so a third rung *below `Minimum`* stops being expressible.

Two existing subtleties must be preserved, both of which the current code gets right and which a naive
rewrite would break:

1. **The generous pass may never starve the minimum pass.** They share the caller's *one* deadline; the
   generous pass gets a slice (`GENEROUS_BUDGET_SHARE = 0.4`, `assets.rs:751`, `generous_deadline`,
   `:764`). Arming a fresh budget per rung makes one plan cost two budgets — on the net thread, that is
   the #302 stall disease. **Keep this exactly as it is.**
2. **The fine tier does not tier.** `chooses_a_route = max_search.is_none()` (`assets.rs:1662-1663`) —
   the bounded local plan follows a coarse route *already chosen with room*, so a second pass buys
   nothing and, measured, "DOUBLES the plans that overrun the budget (blackburrow 17 → 30 of 240)"
   (`assets.rs:1658-1660`) **[cited: that comment; provenance is PR #376]**. **Keep this.** With a
   clearance field the second pass gets cheap enough to reconsider — but reconsider it *with a
   measurement*, not on this document's say-so.

### 4b. The ledge margin at the tight tier — a gap the owner should know about

Today `ledge_margin` is **`0.0` whenever `radius == PLAYER_RADIUS`** (`assets.rs:2006-2010`). Since the
fine tier always plans at `PLAYER_RADIUS` (`navigation.rs:2730`), **the tier that steers the character
has no drop-safety at all**, and neither does *any* route that fell back to `Minimum`.

The owner's requirement — *"walking near edges is a good way to fall off of them"* — is therefore
**not** satisfied on the tight tier.

**This is safe to fix and it does NOT re-open #310.** #310 was about *fitting through gaps* (wall
clearance below the body radius). A **ground** margin from a drop is a different axis: keeping 0.5u of
standing room from a cliff lip does not thread the character through anything it cannot fit through. I
recommend a **reduced but non-zero** ledge margin at `Tier::Minimum`. But it **will** cost routability
on catwalks and gangplanks, which is exactly the class of route the minimum tier exists to keep. **Q5:
measure it over the corpus before choosing the value.**

### 4c. Surfacing "this route required the tight tier" — today it is a lie by aggregation

`nav_tight` is a **zone-lifetime counter** (`assets.rs:701-704`, surfaced at `observe.rs:83-95`):
*"how many routes since zone load only existed at the minimum clearance."*

The agent's actual question is **"is the route I am walking right now a tight one?"** — and the counter
cannot answer it. Once *any* route in the zone has been tight, `nav_tight` is non-null forever. It is
the same shape as the `connected: true` bug (#343): a field with no per-instance writer.

**Fix (PR-3):** carry the tier on the plan itself.

```rust
pub struct PlanReply {                 // nav_planner.rs:54
    pub gen: u64,
    pub outcome: PlanOutcome,
    pub plan_ms: u128,
    pub goal_snapped_z: Option<f32>,
    pub tier: Tier,                    // <-- NEW. Which rung actually answered.
}
```

and publish `"nav_tier": "preferred" | "minimum"` on `/v1/observe/debug` alongside `nav_state`.
Keep the counter as a *zone-health* statistic; add the per-route fact, which is the one that is true.

---

## 5. The honesty channel

### 5a. What to report — and the trap

A* closes a frontier of up to `MAX_NODES = 1_000_000` (`assets.rs:1993`) and refuses a multiple of that
many edges. **You cannot report them all**, and a dump of the closed set's boundary is not something an
agent can act on. Reporting *one arbitrary* refusal is worse: it is a confident, well-formed, unhelpful
answer — precisely the failure the `agent-honesty` label exists for.

**Decision: report exactly two facts, both derived on the COLD path, both actionable.**

| field | how it is computed | answers |
|---|---|---|
| `goal_blocked_by` | **one** `can_occupy(goal, Tier::Minimum)` — the *diagnostic* form | "Is my goal itself impossible?" This is the highest-value answer and it is *definitive*: if the goal cannot be occupied, no search could ever have succeeded. It explains `Unreachable(GoalNotWalkable)` |
| `frontier_blocked_by` | **one** `can_traverse(best_toward → the next cell toward the goal)` — diagnostic form | "I got as close as *here*; the thing between me and you is a **Wall** at (x,y,z)." This explains `Unreachable(SearchClosed)`, the case where the goal is fine but the character's component does not contain it |

`best_toward` is **already tracked by A***, for the partial-route fallback (`assets.rs:2063-2064`,
`:2107-2108`). Nothing new is computed during the search. The two diagnoses run **after** the search
has failed, cost two detector evaluations, and run **zero times on a successful plan.**

**Why the frontier and not the goal alone:** a sealed component is the common wedge (#329, #205), and
there the goal is perfectly walkable — `goal_blocked_by` would be `None` and the agent learns nothing.
The frontier fact names *the actual obstruction that ended the journey*, at the point nearest the goal.

**Why not "the hazard that refused the most cells":** it is a popularity contest over a set the agent
cannot see, it costs a counter in the hot loop (which is the thing we may not pay for), and the modal
refusal in an open zone is "no floor" at the zone boundary — true, and useless.

**Honesty about the honesty channel:** `frontier_blocked_by` is **one** blocking fact, not *the* cause.
It must be *named* as such in the API (`frontier_blocked_by`, not `reason`), and `docs/http-api.md` must
say: *"the hazard that stopped the search's closest approach — not necessarily the only one, and not
necessarily the one to fix."* A field that over-claims is the same lie in a different hat.

### 5b. Propagation

```rust
pub enum PlanOutcome {                          // assets.rs:621 — extended, not replaced
    Route(Vec<[f32; 3]>),
    Unreachable {
        reason: NoRoute,                        // unchanged (assets.rs:585)
        goal_blocked_by:     Option<Blockage>,  // NEW — cold path
        frontier_blocked_by: Option<Blockage>,  // NEW — cold path
    },
    Exhausted { limit: PlanLimit, progress: Option<Vec<[f32; 3]>> },
}
```

`Exhausted` deliberately carries **no** blockage: the search did not close its frontier, so it does not
*know* what stopped it, and inventing a blockage there would be a fabrication. **"I don't know" stays
"I don't know."** That is the #337/#356 discipline, and this must not erode it.

Then: `PlanOutcome` → `PlanReply` (`nav_planner.rs:54`) → `Navigator::apply_plan`
(`navigation.rs:1405`) → `nav.reason` → `/v1/observe/debug` (`observe.rs:150`), which is a path that
**already exists end-to-end**. This adds two nullable fields to it, nothing structural.

### 5c. The zero-cost proof

Two tests, and they are the reason the hot/cold split is safe rather than a second drift waiting to
happen:

1. **Agreement (property).** For random bodies, points, segments and tiers over the fixture zones:
   `assert_eq!(t.can_occupy_fast(p, tier), t.can_occupy(p, tier).is_ok())` — and the same for
   `can_traverse`. **This is the invariant that makes `_fast` safe.** Mutation-check it: perturb one
   detector's `diagnose` and the test must go RED.
2. **Zero cost on success (example, mutation-checked).** Instrument `diagnose` with a counter behind
   `#[cfg(test)]`; plan a route that succeeds; assert the counter is **0**.

---

## 6. The drift invariant — the honest answer

### 6a. Can the planner and the controller truly share ONE predicate?

**No.** Do not promise this, and do not let the acceptance criterion be read as promising it.

* The planner asks a **discrete** question — *is this (cell, surface) occupiable? is this lattice edge
  traversable?* — up to 10⁷ times, and needs a `bool` in nanoseconds.
* The controller asks a **continuous** question at 60 Hz — *where does my cylinder first make contact,
  what is the normal, how do I resolve the penetration?* (`slide`, `movement.rs:349`;
  `depenetrate`, `:428`). It is not a predicate at all; it is a solver.

Forcing them into one function ruins one of them.

### 6b. What CAN be unified — one source of truth, three views

| unified | how |
|---|---|
| **The body** | one `PLAYER_BODY` const (§2a-iv). The planner's probes and the controller's probes are **the same array**. §1c is then unrepresentable, not merely fixed. **This is the tier-1 (unrepresentable) half of the fix.** |
| **The hazard set** | one `ClearanceField` + one `DynamicHazard` list. "What counts as an obstruction" has exactly one definition. |
| **The direction of conservatism** | the load-bearing invariant, below. |

Three **views** of that one truth:

1. **Coarse planner (8u)** — a **max-pool** over the field: *does **any** point in this 8u cell have
   clearance ≥ `tier`?* This is the *optimistic corridor selector* #379 says the coarse tier must
   remain — but it is now optimistic **about the same field**, not via a *different predicate*.
2. **Fine planner (2u)** — the **exact** field lookup.
3. **Controller** — the continuous sweep, whose *contact* test is derived from the same `Body`.

**This is what dissolves #379.** #379's re-plan loop exists because the coarse tier (ray) and the fine
tier (capsule) answer *different questions*, so coarse can commit to a corridor fine will always
refuse, forever. Under max-pool: **if the coarse tier selects a cell, that cell provably contains a
point the character fits at** — so the fine tier's job stops being "veto the corridor" and becomes
"find the fitting line through it," which is what a fine tier is *for*. The pathological re-proposal
loop becomes structurally impossible, rather than being patched with a blacklist.

And it does so **without** the −29 % akanon routability collapse that killed the "just capsule-sweep
the coarse lattice" idea (`assets.rs:1413-1415` **[cited]**), because max-pool never rejects a corridor
that has *any* fitting point — it only rejects corridors with *none*, which are exactly the corridors
that should be rejected.

> **Caveat, honestly:** max-pool is an *approximation of an approximation*. A coarse cell whose only
> fitting point is a far corner will be selected, and the fine tier then has to find that corner within
> its 40u window. Whether that is better than today's behaviour is **[guess]** until measured on the
> corpus. It is the single most important thing for the reviewer of PR-4 to attack.

### 6c. The invariant, stated formally

> **Soundness (the one that matters):** for every segment `s` the planner emits,
> `controller.walk(s)` reaches `s.end` without a depenetration event and without cross-track error
> exceeding ε.
>
> Equivalently: **planner-clear ⇒ controller-passable.** The converse is *not* required — the
> controller may pass things the planner refuses. That costs routes, not correctness, and it is the
> right direction to be wrong in.

### 6d. The test that proves they have not drifted

**A property test, not an example test** (verification hierarchy tier 2 — "the controller can walk
every segment the planner emits" is a *universal*, and no number of live runs discharges a universal):

```
for (start, goal) in random_pairs(zone):            # the #372 harness already enumerates
    plan = planner.plan(start, goal)                # 1700 pairs across 34 cached zones [cited: PR #372]
    if plan is Route:
        ctrl = CharacterController::new(start)
        drive ctrl along plan with the REAL step()  # movement.rs:173 — the real mover, not a mock
        assert ctrl reached the last waypoint
        assert depenetrate() never fired            # movement.rs:428
        assert max cross-track error <= EPS         # xte(), movement.rs:528
```

The example-shaped ancestor of this test already exists —
`nav_walker_hugs_a_bending_path_without_straying` (`movement.rs:547`) plans with `find_path` and drives
the real controller along the result. **Generalise it into a property test over generated geometry, and
run it as an offline harness over the cached-zone corpus.** That single test is the acceptance criterion
of #378, made falsifiable.

**It must be written BEFORE the refactor and must FAIL on `main`** — on the §1c beam fixture, if nothing
else. A drift test that is green on the buggy code is worthless, and this repo has shipped several.

---

## 7. Migration — a reviewable PR ladder

Each rung is independently mergeable, independently green, and independently *revertable*. No big bang.

| PR | change | closes / unblocks | the regression that protects it |
|---|---|---|---|
| **PR-1** | **`Body`: one collision volume.** One `PLAYER_BODY` const; `path_clear` **and** `CharacterController::slide` both derive feelers + probe heights from it. Delete the dead `Collision::sweep` (`assets.rs:1321`, 0 production callers). Correct the false doc comment (`assets.rs:1440-1445`). | **#358 (residual, height axis — §1c)** | (a) a test asserting planner probes == controller probes, mutation-checked by perturbing one; (b) the **beam fixture** (Appendix A) — must be RED on `main`. **⚠ This CHANGES ROUTES** (the planner gains a 4.0u probe). Corpus-measure route parity before merge — every prior tightening sealed zones. |
| **PR-2** | **`Traversability` façade + `Tier`.** Wraps *today's* tests unchanged (Floor = `nearest_floor`, Wall = `path_clear`, Water = `RegionMap`). Route all four predicates through it: A*'s edge tests (`assets.rs:2145`), the ledge margin (`:2153`), the waypoint inset (`:2445`), the controller. **Pure re-plumbing — byte-identical routes.** | **#378's "one type" criterion**; **#312's half-fix** (the inset's `edge_ok` becomes Floor **and** Wall) | a route-hash parity test over the fixture zones: the emitted waypoints must be **byte-identical** to `main`'s. If they are not, PR-2 did something it was not supposed to. |
| **PR-3** | **`Blockage` / `BlockedBy` + the cold `diagnose` path.** Extend `PlanOutcome::Unreachable` with `goal_blocked_by` / `frontier_blocked_by`; thread through `PlanReply` → `nav_state` → `/v1/observe/debug`. Add per-route **`nav_tier`** (§4c). | **#378's honesty criterion**; the residual half of **#356** | (a) the §5c agreement property test; (b) the §5c zero-diagnose-on-success counter test; (c) a test that `Exhausted` carries **no** blockage (the #337 discipline). |
| **PR-4** | **`ClearanceField` (MemoField — no bake, no navmesh).** Radial-disc point clearance, memoised per zone with a **bounded** table. A* inner loop becomes a lookup. Coarse tier becomes a **max-pool** view (§6b). | **#381** (radial probe has no parallel-wall hole); **#379** (max-pool ⇒ coarse cannot select a corridor with no fitting point) | (a) the §6d **walkability property test** — the whole point; (b) an inner-loop benchmark, before/after — **#378 requires "no perf regression in the A* inner loop (measure it)"**; (c) a memo-size ceiling test in gfaydark (the ~95 MB **[derived]** leak risk). |
| **PR-5** *(optional — only if PR-4's numbers demand it)* | **`BakedField`.** An eager, zone-load bake **of the very `ClearanceField` PR-4 ships** — same graded wall/ground query surface, computed up front instead of on demand. **NOT** a lift of `navmesh.rs` (that harness is cancelled, #372 closed); re-derive the voxelise → filter → clearance-BFS from scratch if the numbers ever demand it. Digest-keyed disk cache; inherit a `TARGET_COLUMNS`-style cell clamp (the historical `300_000` figure sized gfaydark, [cited: #372 measurements]) or reintroduce a 30 s zone-load stall. | zone-load-time O(1) clearance; the memory ceiling | bake determinism (same GLB ⇒ same digest ⇒ same field); a stale-cache-rejection test; field-vs-MemoField agreement on the corpus. |
| **PR-6** | **Dynamic hazards first-class.** `DynamicHazard` enum replaces `avoid: &[[f32;2]]` + `aggro_buffer` (`nav_planner.rs:46-48`). **Give the fine tier the hazard list it does not have today** (`navigation.rs:2730` passes `&[]`). | **#378 closes here** | a test that a mob on the fine tier's carrot is *skirted*, not walked through; and that a `Severity::Cost` hazard can **never** turn a route into `no_path` (`assets.rs:2012-2017`'s guarantee). |
| **PR-7** | **Floor detector: `\|nz\|` + headroom, retiring the winding-sign filter.** | **#375** | see §8 — this is the rung most able to hurt you. |

**Why this order.** Honesty before performance (PR-3 before PR-4): a fast planner that cannot say why it
refused is the bug we already have. Re-plumbing before behaviour change (PR-2 before PR-4): if PR-2's
route hashes match, every later route change is *attributable*. And #375's classifier is **last**,
because its safety net (headroom) is a thing PR-4/5 build.

---

## 8. Interaction with #372 (navmesh, CANCELLED) and #375

### 8a. Does this design require the navmesh? **No — and #372 is now cancelled outright.**

What it requires is a **clearance oracle keyed by (point, surface)**. `MemoField` (PR-4, shipped in
Phase 1) satisfies that with **zero** navmesh code. #372 (the `worktree-navmesh-harness`) is closed
and its Phase 2 cancelled; the owner banked the working grid and dropped it. **PR-1 through PR-4, PR-6
and PR-7 all land unchanged regardless.** The optional PR-5 `BakedField` is an eager bake of *this
design's own* `ClearanceField` (a span grid → filter → clearance-BFS re-derived from scratch), **not**
a lift of `navmesh.rs`. The `navmesh.rs` file:line references in this section are provenance pointers
into that dead branch — cited for the measurements they recorded, not as code to resurrect.

That is the design's answer to "the owner may bank the grid and drop the navmesh": **it must not be a
decision this refactor forces, and it isn't.**

### 8b. Does the Floor detector adopt #372's `|nz|` + clearance? **Yes — but last, and with both
defences intact.**

The evidence is strong and it is not mine: #375 measures that the #353 winding-sign filter deletes
**806 of 1240 EQEmu-walkable spots (65 %)** in highpass **[cited: #375]**, that PR #374's
column-bottom fallback recovers **0 of 40,422** of them **[cited: #375]**, and that #372's
`|nz|`-plus-clearance classifier takes highpass from **39.3 % → 99.2 %** oracle coverage
**[cited: PR #372 body]**. The conclusion — *EQ face winding is not a reliable signal for outdoor
terrain* — is measured, not argued.

**But `|nz|` alone reintroduces #329** (A* stood on qcat's ceiling and planned routes through rock).
#372's own fix is the pair of defences, and **both** are required:

1. **Anchoring** — you stand on a surface **below your feet**, never above your head.
2. **Headroom** — a ceiling has rock above it; ground does not. And the headroom test must run on
   **both windings**, "so the defence cannot silently depend on the art" (`navmesh.rs` /
   PR #372 body **[cited]**).

Headroom is exactly the `agent_height` clearance the field already computes. **That is why PR-7 comes
after PR-4/5** — before the field exists, the #329 defence would have to be re-implemented ad hoc, and
an ad-hoc #329 defence is how #329 happened.

### 8c. The tests that prove PR-7 does not reintroduce #329

Three, in increasing strength — and the strongest is the one that must gate the merge:

1. **Example, mutation-checked.** `nearest_floor_never_returns_a_ceiling` (`assets.rs:2810`) already
   exists. Revert the headroom test → it **must** go RED. *(If it does not, it never protected
   anything, and that is a finding in itself.)*
2. **The #329 location, end-to-end.** Plan from the qcat spawn pocket (**`[-48, 1058, -66]`**,
   **[cited: #329]**) and assert **every** waypoint's z is at or below the character's floor tier —
   never the ceiling. #372 reports qcat anchoring to **−69.97** with "the ceiling is never walkable"
   **[cited: PR #372 body]**; this test pins that.
3. **Property, over the corpus — this is the gate.** For **every** surface the classifier calls
   walkable, in every cached zone: `assert!(headroom(surface) >= agent_height)`. A ceiling, *by
   construction*, has none. This is a universal, and #329 is a universal claim ("A* must never stand
   on a ceiling"), so only a universal test discharges it. Tests 1 and 2 are existence proofs over one
   trajectory; **test 3 is the one that cannot be passed by luck.**

Additionally, **#375's own warning must be honoured**: the metric used to validate the classifier must
be able to **see mid-column ground**. Both `winding_is_consistent()` (which voted on the *lowest*
surface, scored highpass 98.4 %, and sailed past its 85 % bar while the filter ate the *middle* of the
column) and the "ground = the column's lowest surface" metric proposed during #374's review are
**structurally blind to this loss** **[cited: #375]**. Validate against the EQEmu-navmesh oracle at
sampled *walkable spots*, not against a per-column summary.

---

## 9. Risks

**The biggest risk, by a distance: this design changes what "walkable" means, everywhere, at once —
and every previous attempt to tighten planner walkability sealed zones.**

The receipts:

* Capsule-sweeping the **coarse** lattice: routable pairs **876 → 813 (−7 %)**; **akanon 90/120 →
  55/120 (−29 %)** — "sealing a third of a city is a worse bug than the one being fixed"
  (`assets.rs:1413-1416`) **[cited]**.
* Recast's **hard erosion** at our cell size: **−15.5 %** of the routes the legacy grid still finds —
  it deletes narrow stairs and bridges outright (`navmesh.rs:527-529`, branch) **[cited]**.
* **#310** removed a sub-radius fallback for the mirror-image reason.

A clearance field **thresholded at `Tier::Preferred`** is precisely a 2.0u erosion, and it will do this
again. **Therefore, non-negotiably:**

> **The field is a threshold at `Tier::Minimum` and a COST above it — never a hard filter at
> `Tier::Preferred`.** `Preferred` is a *first rung of a retry ladder*, and the ladder always falls back
> to `Minimum`. It is never a filter that deletes geometry from the graph.

This is the invariant the PR-4 reviewer should be briefed to attack, and the corpus routability count
(PR #372's 1700-pair harness) is the instrument. **A routability regression on the corpus is a
merge-blocker, not a tuning note.**

Second risk: **PR-1 changes routes** (the planner starts probing at 4.0u and will refuse routes it
currently emits — *correctly*, but that is the same sentence every zone-sealing change came with).
Measure it on the corpus. Third: the `MemoField` memory ceiling in gfaydark (§3d).

---

## 10. Open questions — my recommendation on each

| # | question | my recommendation | confidence |
|---|---|---|---|
| **Q1** | **Bake or memoise?** `BakedField` (0.5–0.9 s median bake **[cited: #372]**, O(1) query, needs #372's voxeliser) vs `MemoField` (no bake, no navmesh, but a ~95 MB **[derived]** memory ceiling risk in gfaydark). | **Memoise first (PR-4), measure, bake only if forced (PR-5).** It keeps "drop the navmesh" a live option to the very end, and it is a smaller, more reviewable diff. | high |
| **Q2** | **Does the coarse tier become a max-pool view of the fine field?** | **Yes.** It is what structurally dissolves #379 (coarse can no longer select a corridor with no fitting point) *without* the −29 % akanon collapse a coarse capsule sweep caused. But the "is it better in practice" half is **[guess]** — gate it on the corpus. | medium |
| **Q3** | **Per-route `nav_tier`, or keep the zone-lifetime `nav_tight` counter?** | **Per-route, in `PlanReply`.** The counter cannot answer "is *my* route tight" — it is `connected: true` (#343) again: a field with no per-instance writer. Keep the counter as zone health; add the fact. | high |
| **Q4** | **`Tier` enum, or an `f32` clamped at `PLAYER_RADIUS`?** | **`Tier`.** An f32 is the type that permitted #310. The clamp is a *guard*; the enum is *unrepresentability*, which the verification hierarchy ranks strictly above it. | high |
| **Q5** | **Should `Tier::Minimum` keep a non-zero LEDGE margin?** Today it is `0.0` (`assets.rs:2006`) — so the fine tier, which steers the character, has **no drop-safety at all**. | **Yes, a reduced one.** It does **not** re-open #310 (that was *wall* clearance below body radius; this is *ground* margin from a drop — a different axis). But it **will** cost catwalk/gangplank routability. **Pick the value by measurement, not by argument.** | medium |
| **Q6** | **What are the body's true probe heights?** I will not invent them. The controller uses {0.5, 4.0}; the planner uses {2.5, 3.0}; the cylinder is ~6u. | **Take the union {0.5, 2.5, 4.0} as the starting point and MEASURE route parity on the corpus.** Adding the 4.0 probe to the planner is *correct* and *route-reducing*; the corpus decides whether the reduction is real geometry or a sealed zone. | low — this is the number I most want measured |
| **Q7** | **Dynamic hazards: hard blocks or costs?** | **`Severity::Cost` by default; `Severity::Block` only when the AGENT explicitly declares a danger zone.** A mob must never make a zone unroutable — today's aggro penalty "never becomes no route" (`assets.rs:2012-2017`) and that is right. | high |
| **Q8** | **Does the CONTROLLER consult dynamic hazards?** | **No.** The controller must never refuse to move because a mob is standing there — bodies do not block in EQ, and a walker that freezes because a rat wandered onto it is a new wedge class. Dynamic hazards are **planner-only**, and `Traversability` should make that explicit in its API (the controller gets a handle that exposes only the static field). | high |
| **Q9** | **Should the fine tier come off the net thread (#382) as part of this?** | **No — keep it separate.** It is a threading change, not a traversability one, and bundling it makes both unreviewable. But note PR-4 *lowers* the fine plan's cost, which relaxes #382's pressure. #382 stays open and is **not** closed by this design. | high |

---

## Appendix A — the drift found while writing this (ready to file)

> Not filed: I was scoped to a design document. **The owner should file this**, or authorise it.
> It is the concrete, live instance of the drift #378's acceptance criterion is written to prevent, and
> it is the fixture PR-1 needs.

**Title:** Planner probes to 3.0u, the controller collides to 4.0u: a chest-height beam is invisible to
A* and solid to the walker — #358's drift, still live, in the height axis
**Labels:** `bug`, `severity:medium`, `agent-honesty`

**Body:** the table and the two consequences in §1c of this document, plus:

**Repro (deterministic, unit-level — no live client):** a `Collision` fixture with a floor slab at
z = 0 and a horizontal slab spanning a corridor from z = 3.2 to z = 3.6 (an overhead beam).
1. `col.path_clear([0,0,2.5], [0,20,2.5], 1.0)` → **true**; at 3.0 → **true**.
2. Drive `CharacterController::step` along the same segment → the CHEST ray at 4.0 hits the beam,
   `slide` reports a hit, the walker makes no forward progress.
3. `col.find_path` across the beam returns a route **straight through it**.

*(Asserted from reading the code, not yet executed — writing this fixture and watching it go RED on
`main` is the first commit of PR-1.)*

**Root cause (high confidence — structural):** there is no single definition of the character's
collision volume. `PLAYER_RADIUS` is shared (`movement.rs:13`), but the probe *heights* are re-declared
independently in four places (`movement.rs:350-351`, `assets.rs:1331-1332`, `assets.rs:1984`,
`assets.rs:2144`) and the sweep *pattern* is implemented twice. Nothing forces them to agree — so they
don't.
