# 3D water-volume navigation — genuine 3D A* inside water (water design Phase 3d)

**Status:** DESIGN ONLY — for owner sign-off before any implementation. No code accompanies this
document.
**Directed by the owner:** the current water planner is 2D-with-water-hacks; it cannot express a
mid-water goal or route through a flooded underwater passage (qcat). Water is a 3D medium; the
planner must be 3D — but ONLY within water. Land stays surface-following; a whole-zone 3D grid is
the memory trap to avoid.
**This is** the "Option 4 / Phase 3d — 3D navigable water volume" explicitly deferred by
`docs/specs/2026-07-16-water-navigation-design.md` §3-R3/§9, now opened against two concrete owner
cases (§7 below).
**Scope:** `src/nav/collision.rs` (the shared two-tier A*), `src/region_map.rs` (`.wtr` reader —
one new query, no format change), `src/nav/walker.rs` + `src/nav/steering.rs` (3D carrot),
`src/movement.rs` (vertical-wish rule only — the collided swim primitives are already in),
`src/traversability.rs` (no new fields expected; `Body` already carries the swim geometry).

---

## 0. Provenance rules

Every load-bearing claim is labelled:

* **[cited]** — traceable to `file:line` on `main @ 950d32f` (2026-07-17), read for this document.
* **[measured]** — produced by a scratch scanner run for this document over the real shipped
  `.wtr` files (`~/.local/share/eqoxide/assets/models/maps/water/`). Method stated inline;
  re-runnable.
* **[derived]** — arithmetic from cited/measured values.
* **[inferred]** — believed from structure but NOT directly verified. Each one states what would
  verify it.

---

## 1. Verified current state (this has moved since the 2026-07-16 doc)

The July-16 water design's Phase 3a/3b landed. What exists on `main` today:

| mechanism | where [cited] | status |
|---|---|---|
| One A* for both tiers | `collision.rs:1824` (`astar`), called with `cell=8` (coarse) and `cell=2` (fine, `steering.rs:22`) | the water edge families live in the **shared** neighbour loop — anything added there serves both tiers |
| Node identity is already (cell, z-tier) | `collision.rs:2145-2151` — "MULTI-FLOOR A*: the node is (cell, floor), not just cell", `Key = (i32, i32, i32)` with z quantized to **2u buckets** (`qf`, :2150) | the search is a layered 2.5D graph, not a flat 2D grid |
| Heuristic is horizontal-only | `collision.rs:2162` — `h = 2D cell distance × cell` | matters for 3D admissibility, §6.4 |
| WATER DESCENT edge | `collision.rs:2330-2362`; per-depth cost `(cz−nf)×4.0` :2352 | dive from a walkway to a **submerged floor** only |
| WATER ASCENT / haul-out edge | `collision.rs:2364-2421`; the #359 E3 contract at :2385-2403 — exit admitted iff `nf ≤ surface + PLAYER_BODY.haul_out_up` (:2400-2403), chest ray from swim height :2409-2410 | the water→land contract, **kept intact** by this design |
| WATER SURFACE TRAVERSAL edge | `collision.rs:2424-2461`; node keyed at `qf(surf)` :2443; deliberately no chest ray :2444-2449; `|surf − cz| ≤ STEP_H` window :2450 | surface-only crossing |
| Floating START anchor | `collision.rs:1973-1978` (wet + no footing within `FOOTING` (:506) → anchor at `surface_z`) | |
| Floating GOAL anchor | `collision.rs:1915-1921` via `floating_goal_surface` :1394-1414 | goal in water anchors to the **surface** |
| Goal accommodation to surface | `goal_z_was_snapped` :1423-1453 (`GoalSnap::ToWaterSurface`), `resolve_goal_floor` :1473-1490 | a goal deeper than `GOAL_TIER_TOL` (8, :518) below the surface is **rewritten to the surface** — the projection hack this design subsumes |
| Shared swim geometry on `Body` | `traversability.rs:134` `float_depth` (=2.0, :167), :142 `haul_out_up` (=`STEP_UP`=2.0, :172); `height`=6.0 :159, `radius`=1.0 (:151, = `movement::PLAYER_RADIUS`), `chest`=4.0 :157 | #359 Phase 3a landed |
| Collided vertical swim | `movement.rs:526-534` `swim_rise` (sweeps the body TOP, stops `SKIN` short of ceiling), :539-545 `swim_sink` (sweeps the feet down) | the primitives 3D execution needs **already exist** |
| Upward wish clamped at surface | `movement.rs:336-341` | feet never leave the water column mid-swim |
| Buoyancy = float to `surface − float_depth` | `movement.rs:356-366` (and the not-swimming branch :367-390), `BUOY_RATE`=30 :79 | fires **only when `wish_vspeed == 0`** (:325 branch structure) — load-bearing for §8.3 |
| Walker vertical wish is UP-ONLY | `walker.rs:843-844` — `wish_vspeed = if swim && target.2 > player_z + 1.0 { 20.0 } else { 0.0 }` | a mid-water hold is **inexpressible** today |
| Walker carrot / pursuit is 2D | `steering.rs:192-215` (`carrot_along` projects in XY; carries the segment **end** z, :199/:203, not a lerp), :245-260 (`advance_cursor`: a segment with `l2 < 1e-6` in XY — i.e. **purely vertical** — reads `t=1.0` and is skipped instantly, :257), `walker.rs:606-612`, distance :658-660 is XY-only | |
| Walker-sim water handling | `collision.rs:4925-4929` `DRIFT_INCLUDE_WATER`; :4814-4819 backoff branch drives `want_swim: false`; :5082 `WAT-ROUTE` = quarantined column, not gated | the self-grading gap the verification plan must not lean on |

**The Increment-3 finding this design answers** [cited `collision.rs:1899-1911`, the anchor-order
comment]: before the floating anchors, A* resolved deep-water endpoints to the **pool bottom** and
routed crossings along it, while the controller's buoyancy lifted the swimmer to
`surface − float_depth` — planner-z and controller-z fought, and the walker wedged (the
halas-pool / blackburrow-lake `[WAT-ROUTE]` drift-corpus wedges). The anchors fixed the
**endpoints** by projecting them to the surface; the interior of water remained expressible only
as "pool bottom floors" (descent edges) or "the surface plane" (traversal edges). Nothing between
exists as a node — that is precisely why a mid-water goal and a flooded passage cannot be planned.

---

## 2. Why the two owner cases are impossible today (not just broken — inexpressible)

**(a) Mid-water goal.** A goal below the surface and above the bottom hits `resolve_goal_floor`
clause 2 and is **rewritten to the surface** (`collision.rs:1484-1487`; reported as
`GoalSnap::ToWaterSurface`, :1442-1445). If the caller's z is within `GOAL_TIER_TOL` of the pool
**bottom**, it resolves to the bottom (:1478-1482) — which the walker floats off of. There is no
node at (x, y, mid-depth) for A* to finish on, and no walker mechanism to hold that depth
(`walker.rs:843-844` drives up only; buoyancy then lifts to the swim plane, `movement.rs:356-366`).

**(b) Flooded passage (qcat).** An underwater tunnel whose ceiling is below the surface cannot be
crossed at the surface (the SURFACE TRAVERSAL edge requires surface continuity between adjacent
columns within `STEP_H`, `collision.rs:2438-2450`), and can only be entered along its **floor**
via descent edges — but the interior route would then be planned on the tunnel floor while the
controller's buoyancy pulls the swimmer to the tunnel **ceiling** (the same planner-z vs
controller-z fight, now with rock above). The `.wtr` and the collision mesh both know the tunnel
is there; the planner has no vocabulary for "swim through the middle of it."

---

## 3. What the `.wtr` and the collision mesh actually give us

* **The `.wtr` is a full volumetric classifier, not a surface sheet** [cited]. It is a BSP over
  3D points (`region_map.rs:144-157` `leaf_at`; `is_water` :165-167 answers for **any** (x,y,z)).
  Water regions are bounded below as well as above: the `water_slab` test helper's doc comment
  states the real shape — "a water volume's lower bound need not meet the floor beneath it (the
  qcat spawn shaft's floor is at −69.97 while its water starts at −69.5)" (`region_map.rs:78-94`),
  and the controller's body-probe fix exists precisely because of that gap
  (`movement.rs:231-246`). So the volume's extent — top, bottom, and lateral — is queryable
  point-wise from the `.wtr` alone.
* **`surface_z` exists; `bottom_z` does not** [cited `region_map.rs:331-342`]. The surface is
  found by upward binary search (24 iterations, 200u cap). The design needs the symmetric
  downward query (§5.2); it is the same ~10-line shape. No format change.
* **Submerged solid geometry (floors, walls, tunnel bores) lives in the ordinary zone collision
  mesh** — the same triangles `column_hits` raycasts (`collision.rs:900-993`) and the controller
  collides with. Evidence it includes underwater geometry: the qcat spawn-shaft work was entirely
  about a **submerged** floor at −69.97 and a ceiling flush with the waterline that the
  (previously uncollided) rise embedded into (`movement.rs:326-334` recounts it; the collided
  `swim_rise` now sweeps against that ceiling, :526-534). **[inferred, one gap]:** that qcat's
  flooded *tunnels specifically* have interior collision (bore walls/ceiling) in the shipped GLB.
  Verify before the live acceptance run with the existing offline probe endpoints
  (`column_surfaces` at a few points along the tunnel line); if the bore is uncollided, #423-class
  mesh work is a prerequisite for the qcat live case (fixtures are unaffected).
* **Zone-bounded water discovery without scanning the zone**: the BSP can be walked once,
  carrying AABBs tightened by axis-aligned split planes — exactly what
  `zone_line_region_points` already does for zone-line leaves (`region_map.rs:208-263`).
  Generalizing that walk to **water** leaves yields the set of water-region AABBs, which is how
  the builder (§5.3) finds candidate columns without a whole-zone lattice sweep.

---

## 4. The owner's question: "why not 3D A* on land as well?" — algorithm vs representation

The question deserves a precise answer, because half of it is already true in the code.

### 4.1 The ALGORITHM is already unified — there is one A*, and its nodes already have 3D positions

The land planner is **not** a flat 2D grid. Verified: the single `astar`
(`collision.rs:1824`) runs a lazy, implicitly-generated graph whose node identity is
`(col, row, z-bucket)` with z quantized at 2u (`Key`/`qf`, :2150-2151), expressly so "a single
cell can be visited at several heights" (:2145-2149). Every node carries a real float z
(`floor_of`, `Node.fz`), edges are generated on the fly by families (walk, jump :2287-2328,
water descent/ascent/surface :2330-2461, controlled fall :2463-2492), and **water-surface nodes
already coexist with floor nodes in the same heap** (the surface-traversal edge keys its node at
`qf(surf)`, :2443). Both the 8u coarse tier and the 2u fine tier run this one function.

So "one graph-based A* with 3D-positioned nodes and per-medium node density" is not a rework
proposal — it is a description of the current architecture plus one missing node **generator**.
Consequently the "land(2D) ↔ water(3D) stitching seam" this task was briefed to design mostly
dissolves: there are not two searches to stitch. There are new **edge families** in the one
neighbour loop, joining nodes of the one key type — the same way jump edges, fall edges, and the
existing three water families joined. §7 specifies those families; none of them is a
graph-to-graph adapter.

### 4.2 The REPRESENTATION must differ by medium — do not voxelize land

* **Land is a gravity-constrained 2D manifold.** Off a surface, a land character is *falling* —
  the controller has exactly one non-swimming vertical mode, gravity + ground clamp
  (`movement.rs:432-463`); there is no hover. A mid-air land node is not a holdable state, so any
  3D land path is instantly projected back onto the walkable surface. Voxelizing land buys zero
  expressible states.
* **The memory/branching math is the trap the brief names.** Everfrost's whole-zone close is
  **1.12M nodes** on the current surface graph, and `MAX_NODES` = 8M is the absolute backstop
  ([cited `collision.rs:2082-2083`, :74]). A 2u voxelization of a 6400×6400u zone with a modest
  200u vertical band is 3200·3200·100 = **1.0×10⁹ voxels** [derived] — ~10³× the surface graph
  and 128× the backstop, with a 26-way branching factor on top. Water, by contrast, is small and
  bounded: the **measured** volumes (§5.4) put whole pools at 10³-10⁵ nodes.
* **The known land bugs are floor-model quality, not dimensionality.** The #375/#420 line
  (probe heights, headroom classification, facing-blindness — `collision.rs:891-993`,
  `traversability.rs:78-126`) are bugs in *classifying the surface*, which a voxel grid would
  need to solve identically (every voxel column still must decide "is this standable").
  Multi-level land — bridges, building floors, walkways-over-sewers — is already handled as
  **layered 2.5D**: multiple z-tier nodes at one (x,y), which is exactly what `Key`'s third
  component exists for (:2145-2151). That is the navmesh-flavoured answer to overhangs; voxels
  add nothing to it.
* **Water genuinely is a 3D medium**: the swimmer holds arbitrary depth (collided `swim_rise`/
  `swim_sink` + suppressed buoyancy, §8.3), so interior volume nodes are *real, holdable states*
  — the exact property land air lacks.

### 4.3 The fork, stated for decision

* **(a) As briefed: keep the land planner untouched, bolt on a separate 3D water graph with an
  explicit stitching layer.** Rejected as *more* complex than the codebase's own shape: it would
  build a second search + a seam abstraction the current single-search design doesn't need, and
  the seam is where the bugs would live.
* **(b) Full rework onto an explicit navmesh/waypoint graph for everything.** Unnecessary: §4.1
  shows the algorithmic unification already exists; a representation rework of mature,
  heavily-tested land nav (the confounding domain) buys no expressiveness this task needs.
* **(c) RECOMMENDED — extend the ONE existing search with a water-volume node generator.** One
  A*, one node key, one heap; node *density* varies by medium (land: surface tiers, sparse;
  water: volume lattice from the span grid, §5); land↔water transitions are edge families like
  every other transition. Land code paths are untouched except where §7/§8 explicitly says
  otherwise. This is (b)'s cleanliness at (a)'s risk profile.

---

## 5. Water-volume representation: the sparse **water-span grid**

### 5.1 Shape

A per-zone, water-only structure — call it `WaterGrid`:

* A sparse map keyed by a **4u XY column lattice** aligned to the coarse grid origin (each 8u
  nav cell = 2×2 water columns; each fine 2u cell maps to a quarter of one). Only columns whose
  water test hits are stored; dry land stores nothing.
* Each column stores its **navigable interval(s)**, not voxels: a small vec (inline capacity 2)
  of `(nav_lo: f32, nav_hi: f32)` plus the column's `surface_z` (f32) — with, per interval:
  * `nav_hi = min(surface_z − float_depth, first_solid_above − Body::height − SKIN)` — the
    highest z a swimmer's **feet** may occupy: at most the swim plane (the buoyancy rest,
    `movement.rs:361`), and low enough that the body top clears the ceiling exactly as the
    collided `swim_rise` will enforce at run time (`movement.rs:526-534`).
  * `nav_lo = max(water_bottom, nearest_solid_floor_below) + ε` — feet stay in water and above
    the collision floor.
  * An interval with `nav_hi < nav_lo` (water shallower than the body, or a slab thinner than
    clearance) stores nothing — the column is unnavigable-3D (it may still support the wading /
    walk edges, unchanged).
* **Vertical is intervals, horizontal is sparse** — that is the whole compression story. Open
  water costs 2 floats per column regardless of depth; geometry (a tunnel under a floor under a
  pool) shows up as a second interval only where it exists.

**Node identity within the interval:** 3D water nodes are *implicit*, materialized during
expansion: `z ∈ { nav_hi, nav_hi − VRES, nav_hi − 2·VRES, … ≥ nav_lo } ∪ { nav_lo }`, with
**VRES = 2.0** — deliberately equal to the existing `qf` bucket (`collision.rs:2150`), so a water
node's key is the **same `(col, row, qf(z))` `Key` the search already uses**, no key change at
all. Anchoring the lattice at `nav_hi` (not at a global z=0 phase) makes the top node of every
column exactly the swim plane, which keeps the haul-out and surface-swim semantics aligned with
the existing edges by construction.

A node is navigable **iff** it lies in a stored interval — which encodes, by construction:
in-water (both `.wtr` bounds), body-height ceiling clearance, and above-floor. Lateral (wall)
clearance is *not* encoded in the node — as on land, it is enforced per-edge (§6.3) with the same
`edge_clear` machinery, keeping one clearance philosophy across media.

### 5.2 Inputs

* `RegionMap::surface_z` (exists, `region_map.rs:331-342`) and a new symmetric
  **`RegionMap::bottom_z`** (downward binary search from a wet probe; same 24-iteration shape,
  ~200u cap). No `.wtr` format change.
* `Collision::column_surfaces` (facing-blind, both windings — `collision.rs:882-886`) for the
  solid floor/ceiling bounds inside the water AABB. This reuses the exact triangles and raycast
  the controller and planner already share; there is no second collision truth.
* `Body` fields: `height`, `float_depth` (`traversability.rs:159/:167`). No new fields needed for
  the representation itself.

### 5.3 Build + caching

* **Discovery:** DFS the `.wtr` BSP once collecting water-leaf AABBs, exactly the
  `collect_zone_line_leaves` pattern (`region_map.rs:240-263`) with `special ∈ {1,7}` instead of
  zone-line. Iterate the 4u lattice inside each AABB only; confirm each column with
  point `is_water` probes (an AABB from oblique planes is a superset — probes make loose AABBs
  cost time, never correctness, same argument as :202-207).
* **Per wet column:** one `surface_z`, one `bottom_z` (≈24 BSP point walks each), one
  `column_surfaces` raycast over the water z-band, then interval arithmetic. [derived] at the
  measured column counts (§5.4: 4.6k-7.3k columns/zone) this is a few hundred thousand BSP walks
  + a few thousand column raycasts — the same order as the existing zone-line precompute that
  runs at zone load with a 250ms warn threshold (`collision.rs:709-731`). **Estimate: tens of ms
  per zone; unmeasured** — the Slice-1 harness measures it (§11).
* **Caching:** built once per zone, stored on `Collision` beside `water`
  (set in `set_water`, `collision.rs:702-707`), shared by both tiers and the walker via the
  existing `Arc<Collision>` clones. **Lazy vs eager:** recommended **eager at zone load, off the
  net thread** — same slot as the zone-line precompute — because the first water plan should not
  eat the build; if the Slice-1 measurement says >100ms for some zone, flip to lazy-once behind a
  `OnceLock`. No serialization/baking to disk: a bake is a second truth that can drift from the
  `.wtr` + GLB it derives from (the #386 argument, and the July-16 doc's R1/R2 verdict).

### 5.4 Memory budget — measured on the real shipped `.wtr` files

Method [measured]: a scratch Python reimplementation of `leaf_at` (same (y,x,z) swap,
`region_map.rs:17-19`) scanned ±3200u XY, −500..300 z: coarse 64×64×16u pass for the water AABB,
then a fine 4×4×2u lattice inside it. Caveat: slabs thinner than the coarse step or pools outside
the padded AABB could be missed; the numbers are a floor, believed close for these zones.

| zone | water AABB (x·y·z, u) | water volume | wet 4u columns | 4×4×2 voxels |
|---|---|---|---|---|
| halas (the pool + river) | [−128,128]·[−384,−64]·[−116,−4] | ~10.5M u³ | 7,301 | 327k |
| blackburrow (lake) | [−128,448]·[−192,192]·[−212,−148] | ~2.0M u³ | 5,618 | 63k |
| qcat (canal + flooded net) | [−192,256]·[−128,832]·[−72,−32] | ~0.9M u³ | 4,612 | 28k |

**Span-grid storage** [derived]: per column ≈ 4B surface + 2×8B inline intervals + len/flags ≈
28B payload; sparse-hash overhead ≈ ×2 → ~56B/column:

* halas ≈ **0.4 MB**, blackburrow ≈ **0.3 MB**, qcat ≈ **0.26 MB**. Compare: the intervals never
  pay per-voxel, so even halas's 327k-voxel volume stores as 7.3k interval records.
* Stress upper bound [inferred — kedge unmeasured, scan it the same way before gating on it]: a
  fully-flooded keep ~1500×1500×150u ≈ 3.4×10⁸ u³ → ~140k columns → **~8 MB**. Bounded and
  acceptable; nothing scales with the zone's dry extent.
* **The avoided trap** [derived]: a whole-zone 2u voxel grid at everfrost scale is ~10⁹ nodes
  (§4.2) — five to six orders above the span grid, and 128× `MAX_NODES`.

**Transient search cost** [derived]: a full flood of halas's volume at coarse water-node density
(8×8u XY within the search, VRES 2u ⇒ volume/(8·8·2)) ≈ **82k nodes** — 7% of everfrost's
routine 1.12M-node land close [cited :2082]. qcat ≈ 7k. Water search cost disappears into the
existing budget; `node_cap` semantics unchanged.

**Octree — considered, rejected.** An octree also bounds memory to water, but: the span grid
reuses `column_hits`/`surface_z` verbatim; `.wtr` water regions are BSP half-space intersections
(near-boxes) whose vertical structure intervals capture losslessly; refinement-near-geometry is
unnecessary because clearance is enforced by exact raycasts at node/edge granularity, not by grid
subdivision; and the octree's non-uniform neighbours complicate the one thing that must stay
simple — sharing the `(col,row,qf(z))` key with the land search. Revisit only if a measured zone
breaks the budget (none of the three did).

---

## 6. The 3D A* inside water

### 6.1 Node

`(col, row, qf(z))` — **unchanged key type** (`collision.rs:2150-2151`), where the z comes from a
span-grid interval (§5.1). A node knows it is a water node because the span grid says so at its
(x, y, z); no medium tag is stored. Start/goal in water become real nodes: nearest lattice z in
the containing interval at the character's / caller's column (§7.3) — **no surface or bottom
projection**.

### 6.2 Connectivity: 26-neighbour (8 XY × {−VRES, 0, +VRES} + 2 vertical)

For each of the 8 horizontal neighbours the search already visits, connect to that column's node
at `z' ∈ {z − VRES, z, z + VRES}` (clamped into the neighbour's interval), plus straight
up/down within the own column. Justification:

* A swimmer has full 3 DOF (collided `swim_rise`/`swim_sink` + horizontal slide); 6-connectivity
  would stair-step every diagonal descent into axis moves, inflate path length up to ×1.73
  against the Euclidean heuristic's estimate, and — worse here — produce vertical zigzags the
  depth-hold controller (§8) would visibly hunt along. 26-connectivity's smoother chords cost ~3×
  the branching of 6 but on volumes measured at 10³-10⁵ nodes (§5.4) that is noise.
* The diagonal set stays within ±VRES per step (grade ≤ ~35° on 4u XY), which keeps each edge
  short enough that the per-edge lateral clearance check (§6.3) is honest; long steep chords are
  composed of steps, not single edges.

### 6.3 Edge validity + cost

* Both endpoints navigable per the span grid (which already encodes vertical body clearance,
  §5.1). Lateral clearance per edge: `edge_clear(from, to, radius, cell)`
  (`collision.rs:1211-1214`) at **two heights** — feet-level (z + `feet_clr`) and head-level
  (z + `height` − margin) — because a swimmer's blocking band is its whole 6u body, not the
  standing chest band. On the fine tier this is the swept-feeler `path_clear` (:1249+); note its
  documented limits: purely-vertical segments fall through to the centre ray (:1254) and feelers
  cannot see walls the segment runs *alongside* (:1270-1277) — same limits land lives with; the
  wall-hug **cost** (not filter) pattern (:2140-2144) extends to water at swim heights.
* **Cost = full 3D Euclidean length** of the edge. No depth penalty, no surface bonus.
* The existing `aggro_cost` (:2117-2126) applies unchanged (XY-based).

### 6.4 Heuristic — the admissibility trap, decided explicitly

The current heuristic is horizontal-only (`collision.rs:2162`). It is tempting to "upgrade" to 3D
Euclidean — **do not, not globally**: land edges cost horizontal distance only (a 20u climb over
an 8u cell costs 8u + penalties), so a 3D `h` can exceed true remaining cost across stacked
floors (goal 40u above via 30u of ramps ⇒ `h`=40 > cost=30) — inadmissible, breaks optimality
quietly. **Decision: keep `h` horizontal-only.** It remains admissible for water too, since every
water edge costs ≥ its horizontal projection. The price is vertical un-informedness — A* explores
depth bands it doesn't need — bounded by (water depth / VRES) extra expansions per column, ~50×
worst-case on a 100u-deep pool, against measured volumes of 10³-10⁵ nodes: acceptable, and the
Slice-2 harness measures actual expansion counts. (Optional later refinement: cost land climb
edges by 3D length so a consistent 3D `h` becomes legal — a route-shape change across all land
nav; out of scope here.)

### 6.5 Why it neither bottom-crawls nor surface-hugs

With interior nodes real and cost purely geometric, the straight 3D chord to a mid-water goal is
optimal by construction; a bottom detour or surface detour is strictly longer. The legacy shaping
that existed to fight bottom-crawling — the descent edge's `(cz−nf)×4.0` [cited :2352] — was
compensating for the interior being inexpressible; inside the volume it is **not carried over**.
It survives only on the land→water *entry* dive (§7.1), where "prefer wading in at the surface
over leaping into the deep" is still the right bias. The surface keeps a *natural* advantage —
top nodes have haul-out edges and no ceiling — not an artificial cost one.

---

## 7. Land ↔ water boundary: edge families, not a seam

All within the one neighbour loop; the three legacy water families are **replaced** as follows.
(Migration note: tests `find_path_swims_up_out_of_a_flooded_pit` :2734 and
`find_path_swims_across_a_surface_pool_instead_of_diving` :4296 must stay green under the new
families — they pin behaviour this design preserves, now emergent rather than special-cased.)

1. **Entry (land → water).** From a floor node, to the neighbour column's **top navigable node**
   (= its swim plane):
   * *Wade/step-in*: when `|nav_hi − cz| ≤ STEP_H` — subsumes today's surface-traversal-as-entry
     (:2438-2450), same window.
   * *Dive-in from a ledge*: when the water lies below (today's descent trigger, :2334-2337),
     target the **surface node** (not the pool bottom) with the fall being the safe water landing
     the walker already recognizes (`walker.rs:669-671`); keep a per-depth entry cost so A*
     prefers wading entries (§6.5). Descents *below* the surface are then ordinary 3D edges.
2. **Exit (water → land): the haul-out contract, verbatim.** Only from a column's **top node**,
   and only under the existing #359 E3 predicate: `nf ≤ surface + PLAYER_BODY.haul_out_up` with
   the swim-height chest ray — the current code at `collision.rs:2385-2421` moves, unmodified in
   meaning, onto top nodes. Interior nodes have **no** land edges: a route out of water always
   goes up-column to the swim plane first, which is exactly what the controller can execute.
   (#359's planner-promise ≡ controller-capability equivalence is untouched.)
3. **Start/goal in water = real 3D nodes.** Replace the floating **anchors**:
   * Start: if the span grid has an interval containing (or near) the character's z at its
     column, the start node is the nearest lattice z in it — a swimmer mid-dive starts mid-dive.
     The current surface anchor (:1973-1978) becomes the special case z ≈ swim plane.
   * Goal: same resolution; a mid-water goal resolves to its own depth. `resolve_goal_floor`
     (:1473-1490) gains a clause **before** the ToWaterSurface accommodation: goal-in-navigable-
     interval → arrival z = the goal node's z. `goal_z_was_snapped` (:1423-1453) mirrors it (the
     accommodation now fires only when the asked z is *not* navigable — e.g. inside the floor, in
     an unbounded column, or in a zone with no water map), so what is promised, what is planned,
     and what arrival demands stay one chain, per that function's own contract (:1455-1472).
4. **A full path is then land-2D → water-3D → land-2D** with no mode switch in the search — just
   waypoints, each carrying its real z, exactly as today's `Vec<[f32;3]>` routes already do.

---

## 8. Walker / controller execution — the crux

The plan must live at the depth the controller holds. Three changes, all gated on being in water
(land steering untouched):

### 8.1 The carrot becomes genuinely 3D

* `carrot_along` (`steering.rs:192-215`): interpolate z along the segment (today it carries the
  segment **end** z, :199/:203 — fine for floors, a stair-step for a diving chord).
* `advance_cursor` / the pursuit projection (`steering.rs:245-260`, `walker.rs:606-612`): for
  segments whose endpoints are in water, project the character's **[x,y,z]** onto the segment in
  3D. This is load-bearing, not polish: today a purely **vertical** segment has `l2 < 1e-6` in XY
  ⇒ `t = 1.0` ⇒ the cursor skips it instantly (:257) — a "descend the shaft, then go through the
  tunnel" route would have its descent leg consumed on frame one and the walker would drive
  horizontally into the shaft wall. The same 3D-in-water rule applies to the steering distance
  (`walker.rs:658-660`) and to arrival: `gdz` already anchors to `resolve_goal_floor`
  (`walker.rs:694-697`), which §7.3 makes depth-aware, so `arrival_action`'s existing
  `Z_ARRIVAL_TOL` gate (`steering.rs:173-186`) needs no change.

### 8.2 Vertical wish from the carrot — replacing the up-only rule

Replace `walker.rs:843-844` with a signed depth controller (in water, when the active carrot is a
water waypoint):

```
err = carrot.z − player_z            // feet-frame, as all nav z's are
wish_vspeed = 0                       if carrot is at/above the swim plane at (x,y)   // §8.3
            = clamp(err / τ, ±SWIM_VRATE) otherwise, with deadband |err| ≤ DEAD
```

Proposed `SWIM_VRATE` = 20 (the existing value, :843), `τ` ≈ 0.25s, `DEAD` ≈ 0.75u (< VRES/2, so
depth-hold cannot oscillate between node buckets). The controller side **already does the right
thing with this input**: a positive wish is collided and surface-clamped
(`movement.rs:336-341` / `swim_rise` :526-534), a negative wish is collided against the floor
(`swim_sink` :539-545) and feeds the #444 `swim_sinking` bookkeeping (:342-355) — no controller
changes are required for depth-hold. The haul-out approach behaviour (rise as the exit lip
nears) is preserved because the last water waypoint before a haul-out edge **is** the swim-plane
node (§7.2), so the carrot rises and the rule drives up exactly as the current SWIM_UP trigger
does.

### 8.3 Buoyancy: assist surface-ward, never fight a hold — the rule, decided

Buoyancy fires **only when `wish_vspeed == 0`** — that is the existing branch structure
(`movement.rs:325` vs :356-366), not a new mechanism. The rule above exploits it deliberately:

* **Carrot at/above the swim plane** → send `wish_vspeed = 0` and let buoyancy do the lift at
  `BUOY_RATE` = 30 (:79) — faster than `SWIM_VRATE` = 20, collided via the same `swim_rise`, and
  it settles exactly at `surface − float_depth`, which **is** the top node's z (§5.1). Buoyancy
  assists; the walker never races it.
* **Carrot below the swim plane** (mid-water hold, diving, tunnel transit) → nonzero wish
  suppresses buoyancy for the frame, so the hold is not a fight — it is buoyancy switched off,
  by the controller's own existing contract. The planner-z vs controller-z war (§1) is dissolved
  because both sides now agree on which z's are holdable: the span-grid intervals.

### 8.4 The fine tier and `want_swim`

The fine 2u tier runs the same `astar`, so it gains the same water nodes/edges with no extra work
(§4.1); its 40u bound keeps 3D windows tiny. `want_swim` stays as today
(`walker.rs:826-829`, body-probe). One deliberate deferral: the downhill-backoff recovery drives
`want_swim: false` (`walker.rs:751-761`) — in deep water that briefly turns swim physics off
(buoyancy branch still catches it, `movement.rs:367-390`). Backoff-in-water semantics are
reviewed in Slice 3, not silently redefined here.

---

## 9. The two owner cases, as acceptance tests

**(a) Mid-water goal.** Fixture: a 60×60u pool, surface −4, floor −44 (walled, haul-out ledge on
one side). Goal at (centre, −24) — 20u under the surface, 20u off the bottom.
* *Planner property*: the route's final waypoint z = −24 ± VRES; **no** waypoint deeper than
  −24 − VRES (no bottom detour) and the route's last leg is not via the surface (no projection);
  `goal_z_was_snapped` reports **no** accommodation.
* *Walker-sim*: the stepped `CharacterController` following the route comes to rest with
  `|z − (−24)| ≤ DEAD + VRES/2` and **holds** it ≥ 5 simulated seconds (buoyancy suppressed by
  the hold wish, §8.3).
* *Live (halas pool, release binary)*: `/goto` with an explicit mid-water z; observe hold depth
  and `nav_state = arrived`.

**(b) Underwater passage.** Fixture: two pools connected only by a flooded tube (bore 8u,
ceiling below both surfaces), dry-land route walled off. Goal on the far shore.
* *Planner property*: a FULL route exists; it descends in pool A, traverses nodes whose z lies
  inside the tube's interval band, ascends in pool B, and exits via a haul-out edge obeying
  `nf ≤ surface_B + haul_out_up`. Clearance property: for **every** in-water waypoint,
  `z + Body::height ≤ (first solid above, from the fixture's analytic geometry) − SKIN` and
  `z ≥ (fixture floor)` — checked against the *hand-built fixture geometry*, not against the span
  grid (see §10 on circularity).
* *Walker-sim*: transit end-to-end; zero depenetration recoveries; position never inside fixture
  solids.
* *Live (qcat, release binary)*: route through a real flooded tunnel (canal→sewer family,
  [cited :2333]) — **contingent on the §3 tunnel-collision check**, and run as a targeted repro
  with #423 (walk-through-walls near water, a separate open collision bug) explicitly on the
  suspect list if anomalies appear.

---

## 10. Non-circular verification (what each tier proves)

The standing warning (nav QA memory): the drift gate samples start/goal **from the floor model
under test** and the walker-sim is effectively opt-out for water (`want_swim: false` hardcode
:4814-4819; `WAT-ROUTE` quarantined not gated :5082). This plan does not lean on that gate.

1. **Planner-only property tests on the emitted route's z-profile** (no walker in the loop; the
   assertions compare against **hand-authored fixture geometry** — analytic pool/tube bounds
   written in the test — never against the span grid or `column_floors` under test, which is what
   makes them non-circular):
   * P-3D-1 *in-volume*: every in-water waypoint is inside the fixture's water AND clears the
     fixture's solids by `Body::height` above / ε below (the §9b property).
   * P-3D-2 *mid-water goal fidelity*: terminal z equals the asked z (±VRES); no ToWaterSurface
     accommodation for a navigable asked z. **Makes the projection hack's return
     unrepresentable.**
   * P-3D-3 *haul-out contract retained*: every water→land step in any emitted route satisfies
     the E3 predicate — sweep ledge height h as in #359's P1; planner-legal ≡ h ≤ haul_out_up.
   * P-3D-4 *no-regression*: existing water tests (:2734, :4296) green; zones with no `.wtr`
     plan exactly as today (span grid absent ⇒ no water nodes ⇒ current dry behaviour).
   * These prove: **the planner cannot emit** a bottom-crawl, a surface-projection of a mid-water
     goal, a ceiling-clipping tunnel route, or an illegal exit. They prove nothing about
     execution.
2. **Faithful walker-sim in water** (extend the drift sim to drive `want_swim` from body-in-water
   and the §8.2 vspeed rule — i.e. the *real* walker's intent path, removing the :4814-4819
   hardcode and the corpus swim-exclusion in water mode): step the actual `CharacterController`
   against the actual collision mesh along the planned route in the §9 fixtures and in
   halas/blackburrow/qcat geometry. Proves: **plan ∘ controller converges** — the buoyancy-vs-plan
   wedge is structurally gone, depth-holds hold, transits transit — under real physics but
   simulated time. `WAT-ROUTE` then flips from quarantine column to a gated count (target 0) in
   clean water zones; qcat stays visibility-only until #423 closes.
3. **Live on the release binary** (per the validate-on-release rule and the verification
   hierarchy): §9a in halas, §9b in qcat, plus one full land→water→land `zone_cross` (halas→
   everfrost re-run). Proves the **premises**: real `.wtr`, real GLB, real timing, real server —
   live runs validate premises, never universals; the universals are tier 1's job.

---

## 11. Incremental build plan

* **Slice 1 — the span grid, alone.** `RegionMap::bottom_z`; water-leaf AABB enumeration;
  `WaterGrid` build + storage on `Collision`; a measurement harness printing per-zone column
  count / bytes / build-ms for the water-zone corpus. Zero behaviour change (nothing reads it).
  *Verifiable by itself*: unit tests on fixture BSPs (`water_slab`, `box_below`) + the measured
  budget table vs §5.4's predictions.
* **Slice 2 — 3D A* inside one pool, planner-only.** Water node generator + 26-connectivity +
  entry/exit families replacing the legacy three; anchor subsumption (§7.3);
  `resolve_goal_floor`/`goal_z_was_snapped` depth clauses. Tests P-3D-1..4. The walker still
  executes surface routes exactly as today (surface-dominant routes remain the optimum for all
  currently-passing cases, so live behaviour should be ~unchanged — the drift sim's water
  visibility mode, run before/after, is the check). **A mid-water goal is already plannable at
  the end of this slice** — the first owner case, offline.
* **Slice 3 — 3D execution.** §8.1 carrot/cursor 3D-in-water, §8.2 vspeed rule, §8.3 rule (no
  controller change expected), backoff-in-water review; walker-sim water gate (tier 2). The
  mid-water hold and fixture tunnel transit pass end-to-end.
* **Slice 4 — live + honesty.** qcat tunnel-collision check (§3), live runs (tier 3), water-aware
  refusal diagnostics (a failed 3D water plan says *why* in water terms — extends the July-16
  §5/§7 honesty work to interior nodes), issue closures.

**Risks, named:**
* *Build cost / zone-load timing* — believed tens of ms, unmeasured until Slice 1; mitigation:
  the eager→lazy flip (§5.3).
* *3D clearance correctness* — `path_clear`'s feelers are horizontal-perpendicular; slanted
  water edges sweep a slanted ribbon and purely-vertical hops degrade to a centre ray (:1254).
  Short edges (§6.2) and the two-height sweep (§6.3) bound the error; the walker-sim tier exists
  precisely to catch what the rays miss. If it does, the durable fix is the clearance-field
  route (:1270-1277's own advice), not more feelers.
* *Walker rework destabilizing land nav* — the highest-blast-radius change is `carrot_along`
  z-lerp (shared with land). Mitigation: 3D projection/vspeed strictly gated on in-water
  segments; land drift-gate run before/after Slice 3; the z-lerp's only land consumer is the
  fall-guard `drop_to_target` (`walker.rs:667-671`) — verify its FALL_TRIGGER behaviour is
  unchanged on the existing corpus.
* *qcat tunnel collision presence* — [inferred] §3; checked in Slice 4 before the live case;
  fixtures carry the acceptance regardless.
* *`.wtr` fidelity* — sliver/thin water regions under the scanner's coarse pass; suspended slabs
  (`water_slab`, movement.rs §444) give intervals whose `nav_lo` is the water's own bottom —
  exiting through a volume's bottom is deliberately NOT a planner edge (only entry/exit via
  §7.1/7.2), matching the controller's fall-re-arm semantics.
* *Route-shape drift in Slice 2* — replacing cost-shaped legacy edges with geometric costs can
  change which shore a crossing hauls out on; the before/after drift-sim diff is the review
  artifact.

**Uncertain / to be confirmed live:** actual expansion overhead of the horizontal-only heuristic
in deep water (§6.4, measured in Slice 2); server-side tolerance of sustained mid-water hover
(no known server correction against it — [inferred] from the halas float park; the §9a live run
confirms); kedge-class budget ([inferred], scan before adding such a zone to any gate).

---

## 12. Open decisions for the owner

1. **The fork (§4.3):** confirm (c) — extend the one existing search with per-medium node
   density — over (a) separate-graph-with-seam and (b) navmesh rework. This is the load-bearing
   direction call this document exists for.
2. **Resolution:** 4u water columns / VRES 2u (= the existing `qf` bucket) — confirm, or ask for
   a finer column lattice at the memory cost table in §5.4 (linear in columns).
3. **Heuristic (§6.4):** accept horizontal-only `h` (admissible, mildly less informed
   vertically) vs re-costing land climbs to legalize a 3D `h` (route-shape change on land —
   recommended NO for this phase).
4. **Slice 2 landing shape:** replace the legacy descent/ascent-interior/surface families
   outright (recommended — one vocabulary) vs keeping them alongside 3D edges behind a flag for
   one release (safer-looking, but two truths for the same water).
5. **Eager vs lazy grid build (§5.3)** — recommended eager-at-zone-load pending the Slice-1
   measurement.
6. **Gate zones for the water walker-sim tier** (halas, blackburrow, + qcat visibility-only until
   #423) — approve the list.

---

*Prepared for owner sign-off; no implementation started. Code read on `main @ 950d32f`
(2026-07-17). Water-volume measurements from the shipped `.wtr` assets on the same date; scanner
method in §5.4.*
