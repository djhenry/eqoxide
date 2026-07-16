# Water Navigation — Phase 3 of the traversability line (design shape)

**Status:** DESIGN ONLY — for owner sign-off before any implementation. No code accompanies this
document.
**Scope:** `src/nav/collision.rs` (the coarse+fine A*), `src/traversability.rs` (the one authority),
`src/movement.rs` (the controller), `src/eq_net/action_loop.rs` (the nav driver),
`src/region_map.rs` (the `.wtr` reader — read-only, no format change).
**Fixes as part of the model:** #359 (haul-out reference-plane mismatch), #197 (swimmer strands —
close-out), and thereby unblocks #329 (qcat spawn pocket).
**Explicitly NOT designed here:** #423 (walk-through-walls-into-water) — a collision-mesh/walker
bug, addressed only as a *scope decision* in §8.

---

## 0. Provenance rules for this document

Every number and claim below is one of three kinds, and it is labelled:

* **[cited]** — traceable to an issue, a PR, or a `file:line` on `main @ 3d39536`.
* **[derived]** — arithmetic done here from cited constants. Reproducible, not measured.
* **[guess]** — not traced, not measured. **Re-verify before relying on it.**

There are no unlabelled numbers. If you find one, that is a bug in this document.

---

## 1. What is actually true today (verified on `main @ 3d39536`)

**The premise this phase was briefed with — "the planner has NO water layer" — is out of date.**
That was true when #197's key comment was written, but PR #353 and its follow-ups landed
substantial water machinery. An honest design has to start from what exists:

| mechanism | where [cited] | what it does |
|---|---|---|
| `.wtr` region map | `src/region_map.rs` (`is_water` :165, `surface_z` :331) | BSP point query: is this point water; where is the surface above a submerged point (binary search, 24 iterations, 200 u ceiling) |
| water attach | `src/nav/collision.rs:669` (`set_water`), `:702` (`in_water`), `:708` (`water_surface`) | zone-load wiring; planner and controller query the same map |
| **WATER DESCENT** edge | `collision.rs:2178-2210` | A* may drop/dive from a walkway into water below, past `MAX_STEP_DOWN` (60 [cited :1860]), landing on a submerged floor; steep per-depth cost `(cz−nf)×4.0` [cited :2200] so diving is a last resort |
| **WATER ASCENT** edge | `collision.rs:2212-2257` | from a submerged/floating cell, swim up the contiguous water column to the surface and haul out onto a neighbour floor at `nf ≤ surface + WATER_EXIT_UP`, `WATER_EXIT_UP = STEP_UP + 0.5 = 2.5` [cited :2235] |
| **WATER SURFACE TRAVERSAL** edge | `collision.rs:2259-2296` | swim ACROSS a pool at its surface: neighbour water cell connects at `surface_z`, node keyed at that height; deliberately no chest-clearance ray [cited :2279-2284] |
| floating **start** anchor (#353) | `collision.rs:1820-1826` | a character in water with no footing within 4 u (`FOOTING` [cited :1820]) anchors A* to the **water surface**, not the pool bottom/ceiling |
| floating plans in `Traversability` | `traversability.rs:404` (`floating`), `:493-497` (`occupy_floor_ok` asks the water), `:522` (margin exempt) | a swim plan is exempt from ground margins; water counts as its "floor" |
| `HazardKind::Water` diagnosis | `traversability.rs:199-200`, `:540-546` | cold-path refusals can already name Water |
| controller swim/buoyancy | `movement.rs:263-299` | floats to `surface − FLOAT_DEPTH` (`FLOAT_DEPTH = 2.0` [cited :275 and :288]) at `BUOY_RATE = 30` u/s; body-probe water detection (#329 fix, :180-201); step-up allowed while swimming (:223-227) |
| nav swim-up drive (#329) | `action_loop.rs:2673-2674` | when swimming and the waypoint is >1 u above, drive `wish_vspeed = SWIM_UP_RATE = 20` u/s |
| partial-route water trim | `collision.rs:2472-2482` | a PARTIAL route pops trailing in-water waypoints — stops at the dry edge instead of walking into a pit |
| flooded-pit unit test | `collision.rs:2547-2578` | `find_path_swims_up_out_of_a_flooded_pit` pins the ascent edge |
| drift-sim water visibility | `collision.rs:4385-4516` | `DRIFT_INCLUDE_WATER=1` keeps water journeys and counts waterline wedges in a `water #423` column; DRY mode (default gate) skips them |

**Live evidence the machinery works when its premises hold:** the halas pool crossing — parked
floating at `(1, −256, −3.1)`, `zone_cross` to everfrost crossed the pool **at z = −3.0 the whole
way** in 30 s; offline the same query is a 60-waypoint surface route in 24 ms [cited #197 final
comment]. So "swim across at the surface" is real, planner and controller both.

### 1b. What is actually broken — the real gaps

1. **The haul-out reference-plane mismatch (#359, OPEN, severity:high).** A* sizes the water→land
   exit from the **water surface** (`nf ≤ surface + 2.5` [cited collision.rs:2235-2238]); the
   controller parks a swimmer at **`surface − FLOAT_DEPTH` = surface − 2.0** [cited
   movement.rs:275-276]. The real riser the controller faces is up to
   `2.5 + 2.0 = 4.5 u` **[derived]** against a step-up of `STEP_UP = 2.0` [cited movement.rs:23]
   (+ `GROUND_SNAP_TOL = 0.5` slack in `try_step_up` [cited movement.rs:66, :403-404] → 2.5 u
   effective **[derived]**). Every haul-out A* approves is out of reach; the character bobs at the
   waterline forever [cited #359].

2. **The nav-driven vertical swim is UNCOLLIDED (#359, second mechanism).**
   `if intent.wish_vspeed != 0.0 { self.pos[2] += intent.wish_vspeed * dt; }` [cited
   movement.rs:267-269] is a raw position write — no sweep, no ceiling clamp, no surface clamp.
   The #329 swim-up fix (`SWIM_UP_RATE` above) put this path on the agent's hot loop. In water
   flush with a ceiling — the qcat spawn corridor, water line −56.0 vs ceiling −55.97 [cited #359
   comment] — the rise embeds the character in rock; the depenetration net recovers it to the last
   good *grounded* position, the shaft floor at −69.97 [cited #359 comment]. Rising can therefore
   *cause* the strand it is meant to fix.

3. **The goal side has no floating anchor (asymmetry).** The start anchors to the water surface
   when floating (#353) [cited collision.rs:1820-1826], but a goal **in** open water resolves via
   `nearest_floor` / `floor_beneath` / `snap_goal_to_column_floor` [cited collision.rs:1769-1791]
   — i.e. to the **pool bottom**. "Swim to X" over deep water plans a dive to the bottom, and
   arrival is judged on the bottom tier (`GOAL_TIER_TOL = 8` [cited :1740]).

4. **The water edges bypass the one authority.** The walk edge goes through
   `Traversability::can_traverse_fast` [cited collision.rs:2115-2117]; the three water edge
   families are bespoke inline code that consult the `RegionMap` directly [cited :2182, :2217,
   :2266]. #378's "a hazard cannot be silently omitted" property does not hold for water yet, and
   the cold diagnosis path hardcodes `floating: false` — "planner water nav is an unimplemented
   gap (#359/#197/#423)" [cited collision.rs:1457-1460]. A failed water plan today diagnoses as if
   it were a dry plan.

5. **Nothing proves a route out of water it routes into.** The partial-route trim [cited
   :2472-2482] protects *partials* only. A FULL route may legally end in water even when the goal
   was dry-land intent, and no test anywhere asserts "every water segment a route contains has an
   executable exit." The walker-sim structurally cannot check it: it always drives
   `want_swim: false` [cited collision.rs:4374] and filters swim routes out of its corpus [cited
   :4442-4460] — water wedges are *quarantined* (the `water #423` column), never *gated*.

6. **`FLOAT_DEPTH` is declared twice** (two local consts in two branches of `movement.rs`, :275
   and :288 [cited]) **and the planner assumes it is zero.** This is a textbook instance of the
   #386 disease — one physical truth, multiple declarations, no compiler forcing agreement — the
   exact bug class `Body`/`PLAYER_BODY` was built to make unrepresentable [cited
   traversability.rs:65-129].

### 1c. Issue status this design asserts

* **#197** — part 1 (zone-line approach) fixed/not-reproduced; part 2 (strand on pool bottom)
  fixed by #353's floating anchor, live-verified [cited #197 comments]. What keeps it honest to
  leave open is the haul-out family (#359). This phase closes it.
* **#359** — OPEN; two stacked mechanisms (1b.1 + 1b.2 above). This phase's centrepiece.
* **#329** — OPEN; the spawn-shaft escape is exactly a haul-out: floor −69.97, water −69.5…−43.0,
  exit ledge −41.97 [cited #359 comment] → ledge sits `−41.97 − (−43.0) = 1.03 u` above the
  surface **[derived]** — legal under today's `WATER_EXIT_UP = 2.5` cap, unreachable from
  `surface − 2.0`.
* **#423** — OPEN; pre-existing (bisected to before `f820c6c`, owner-verified) [cited #423];
  collision/walker, **not** planner. §8.

---

## 2. The core idea: ONE swim plane, declared once

Everything in this design reduces to one sentence:

> **The planner must plan on the plane the controller actually swims at, and the water→land step
> must be one number, declared once, that both sides read.**

Today three A* edge families measure from `surface_z`, the controller floats at
`surface_z − FLOAT_DEPTH`, and `FLOAT_DEPTH` exists only as two duplicated local constants the
planner has never heard of. That is the whole of #359's first mechanism. The fix follows the
repo's own precedent (`Body`/`PLAYER_BODY`, #386): move the water constants into the shared body
and derive both sides from them.

**New fields on `Body` (`src/traversability.rs`):**

```rust
pub struct Body {
    // ... existing fields ...
    /// Where a swimmer's feet rest relative to the water surface. The controller's buoyancy
    /// target AND the planner's swim-node height are both surface_z - float_depth.
    pub float_depth: f32,      // = 2.0, the current FLOAT_DEPTH [cited movement.rs:275]
    /// The tallest ledge a swimmer can mount, measured from the WATER SURFACE. The planner's
    /// haul-out cap AND the controller's haul-out capability are both this number.
    pub haul_out_up: f32,      // proposed 2.0 = STEP_UP; see §4 options and §10 decision 1
}
```

Derived, single-sourced (replacing today's scattered constants):

* **swim plane** `swim_z(x, y) = surface_z(x, y) − float_depth` — the z every swimming A* node
  carries, and the z buoyancy settles to.
* **haul-out condition** `nf ≤ surface_z + haul_out_up` — the planner's exit test, and (by the §4
  controller change) exactly what the walker can execute.

With this, the #359 drift is not just fixed — re-introducing it requires adding a new hardcoded
height, same as #386.

---

## 3. Design area 1 — water representation for navigation

**INPUTS:** the zone's `.wtr` BSP (`RegionMap`), already loaded at zone entry and attached via
`Collision::set_water` [cited collision.rs:669]; the collision mesh (floors/walls) already in
`Collision`; `PLAYER_BODY` with the new §2 fields.

**LOGIC — the three options considered:**

* **Option R1 — keep the implicit query-time model (RECOMMENDED).** Water stays a *function*
  (`is_water`, `surface_z`) consulted inside the A* neighbour loop, exactly as today — no new
  baked structure. The change is *normalisation*, not representation: every water edge and anchor
  computes heights from the §2 swim plane instead of raw `surface_z`, through one new helper on
  `Collision` (e.g. `swim_plane(x, y, probe_z) -> Option<f32>`).
  * For: it already works (halas crossing, flooded-pit test); zero new memory; **one source of
    truth** — a baked copy of the `.wtr` is a second truth that can drift, the #386 disease again.
  * Against: `surface_z` costs ~24 `is_water` BSP walks per call [cited region_map.rs:337-341],
    and the surface-traversal edge can call it per neighbour per expansion. Whether that is hot
    enough to matter is **unmeasured [guess]** — if profiling says yes, memoise per 8 u cell at
    zone load, following the `ClearanceField` MemoField precedent (a memo of the SAME query, pure
    function of its key — degrades speed, never truth [cited traversability.rs:251-270]).

* **Option R2 — bake a surface layer into the grid at zone load.** For every grid cell over
  water, precompute the swim-plane height and store it as an extra "floor tier" the ordinary walk
  edges connect.
  * For: water cells become ordinary nodes; the bespoke edge code shrinks.
  * Against: walk-edge semantics (chest rays, ledge margins, grade limits) are wrong for swimming
    and would need per-tier special-casing anyway — the bespoke logic moves, it does not vanish.
    And the bake is a second truth. Cost with no matching benefit at the zones we have bugs in.

* **Option R3 — full 3D navigable water volume (voxelised).** True underwater routing beneath
  ceilings and through flooded tunnels.
  * For: the only model that can route the qcat *flooded tunnels* and kedge-keep-class zones
    **[guess** that kedge is the canonical 3D-water zone — verify before citing as motivation**]**.
  * Against: every currently-filed water bug (#197, #329, #359) is a **surface or vertical-shaft**
    case — descend, float up, cross at the top, haul out. The controller has no 3D path-following
    (it drives a horizontal `wish_dir` plus a scalar `wish_vspeed` [cited action_loop.rs:2686-2694]),
    and the client models no breath/drowning. 3D volume is a large build for zero currently-filed
    bugs. Deferred to an optional Phase 3d (§9), gated on evidence.

**OUTPUTS:** unchanged public planner API (`find_path*`, `PlanOutcome`); internally, every
water-related height in the A* loop and the anchors is `swim_z` or `surface_z + haul_out_up`,
never a raw `surface_z ± local-const`. No new files, no format changes, no rebake.

**RECOMMENDATION: R1** — normalise on the swim plane, keep the `.wtr` as the single live truth,
memoise only if measurement demands it.

---

## 4. Design area 2 — planner routing through water

How a route ENTERS, CROSSES, and EXITS water, stated per edge family.

### 4a. Entering (shore/ledge → water; the qcat canal→sewer case [cited collision.rs:2181])

**INPUTS:** current node `(cell, floor_z = cz)` on dry land; neighbour cell centre `b`; the water
map.

**LOGIC:** two existing entry forms, both kept, both re-based on the swim plane:
1. **Surface step-in** (today's SURFACE TRAVERSAL doubles as entry): if the neighbour column has
   water within `STEP_H = 20` below the current floor [cited :2272-2275], connect to a node at
   `swim_z` (today: at `surface_z` — the 2 u change). Condition `|swim_z − cz| ≤ STEP_H` (today
   `|surf − cz| ≤ STEP_H` [cited :2285]).
2. **Dive** (WATER DESCENT, unchanged in shape): connect to submerged floors below, keeping the
   steep `(cz − nf) × 4.0` per-depth cost [cited :2200] so A* prefers the surface.

**OUTPUTS:** a swim node keyed `(nc, nr, qf(swim_z))` or a submerged floor node, with edge costs
as today.

### 4b. Crossing (water → water)

**INPUTS:** a swim node at `swim_z`; neighbour water columns.

**LOGIC:** today's SURFACE TRAVERSAL, at `swim_z`. Two honesty notes carried into the design:
* The edge deliberately casts **no chest ray** [cited :2279-2284] — a shore-height ray would snag
  the pool lip. Consequence, stated plainly: a wall standing IN the water (a fence across a canal)
  is invisible to the coarse swim edge; the controller collide-and-slides off it and the fine tier
  re-threads. Keep this, but add a **wall probe at swimmer chest height (`swim_z + chest`) as a
  COST, not a filter** — the same hug-cost pattern as dry routing — so crossings prefer open water.
  (Filter vs cost is a §10 decision; the non-negotiable is: never a hard filter that creates false
  `no_path` across an open pool.)
* Submerged floor→floor movement along a pool bottom remains what it is today: ordinary walk edges
  where floors connect, dive edges where they don't. No new underwater lateral edges in this phase
  (that is Phase 3d).

**OUTPUTS:** swim-node chains at `swim_z` across the pool, cost = horizontal distance (+ optional
wall-hug cost).

### 4c. Exiting — the haul-out (#359's heart)

**INPUTS:** a swim node at `swim_z` (or a submerged node below it); neighbour column's standable
floors (`column_floors`); `surface_z`; `haul_out_up`; `chest`.

**LOGIC — the "can I get out here?" predicate**, one function, used by the ascent edge and (cold)
by diagnosis — proposed home `Traversability::can_haul_out(a: swim-point, b: Point)`:

1. `b` must be standable ground (`is_standable` via `column_floors`, unchanged — water never makes
   a floor).
2. **Height:** `b.floor_z ≤ surface_z + haul_out_up`, sized from the surface with the shared
   constant — replacing today's `WATER_EXIT_UP = STEP_UP + 0.5` local [cited :2235].
3. **Clearance:** chest ray from swim height over the lip to `b` (today's ray from
   `max(surface, nf − STEP_H) + CHEST` [cited :2244-2245], re-based on `swim_z`).
4. **Rise cap into the edge:** the swim-up from a submerged start to the surface stays part of the
   same ascent edge (contiguous water column walk, today's 2 u stepping [cited :2224-2228]).

The three options for making the planner's promise executable — **the owner's central decision**:

| | planner exit cap | controller change | qcat ledge (surface + 1.03 u [derived §1c]) | verdict |
|---|---|---|---|---|
| **E1: planner moves down** | `nf ≤ swim_z + STEP_UP` = surface + 0.5 [derived] | none | **becomes `no_path`** — honest but #329 stays unfixed | rejected as sole fix |
| **E2: controller moves up** | keep surface + 2.5 | rise to surface before the lip | riser 2.5 u from surface vs 2.5 u max step-up [derived] — zero margin | fragile; and the rise MUST be collided (§6) |
| **E3: explicit haul-out contract (RECOMMENDED)** | `nf ≤ surface + haul_out_up`, `haul_out_up = 2.0` proposed | collided surface-approach + step-up mantle (§6) | riser 2.0 u from surface ≤ 2.5 u capability [derived] — 0.5 u margin | fixes #359+#329 with margin; one shared constant |

**E3 in one line:** the planner promises only exits ≤ `haul_out_up` above the surface; the
controller is *made able* to execute exactly those (swim collided to the surface, then the
existing swimming step-up [cited movement.rs:223-227] mounts the lip); a property test sweeps the
ledge height and pins that the two sets are identical (§7). This is #359's own fix-sketch option
three ("give the water→land transition its own explicit allowance") [cited #359], with the
allowance shared instead of duplicated.

### 4d. Anchors and arrival

* **Start:** unchanged (#353's floating anchor), except anchored to `swim_z` instead of
  `surface_z` [cited collision.rs:1820-1826] — so the plan starts where the character actually
  floats.
* **Goal (new):** mirror the start. If the goal point is in water with no footing within
  `FOOTING = 4.0` [cited :1820], anchor the goal to `swim_z` at the goal XY and judge arrival on
  that tier — closing gap §1b.3. A goal on the pool *bottom* (explicitly asked-for z near the
  bottom) still resolves to the bottom via the existing tier logic.

**OUTPUTS:** routes whose water waypoints all carry `swim_z` heights; every water segment either
terminates in a `can_haul_out`-approved exit or at a floating goal; `PlanOutcome` unchanged in
shape.

---

## 5. Design area 3 — integration with the `Traversability` abstraction

**INPUTS:** the existing façade (`can_occupy/_fast`, `can_traverse/_fast`, `Blockage`,
`HazardKind::Water`, the `floating` flag, `ClearanceField`).

**LOGIC:**

* **Water becomes a first-class *support*, not just a hazard.** Minimal shape (recommended for
  this phase): keep `Point { xy, floor_z }` and add two façade methods the water edges route
  through — `can_swim_traverse(a, b)` (crossing, §4b) and `can_haul_out(a, b)` (exit, §4c), each
  in the established hot/cold pair (`_fast: bool` for the A* loop; diagnostic
  `Result<(), BlockedBy>` for failed plans). This ends the §1b.4 bypass: every edge family —
  walk, jump, fall, descend, cross, haul out — consults the one authority.
  * A larger refactor (`enum Support { Floor(f32), Swim { plane_z } }` replacing `floor_z`) was
    considered and REJECTED for this phase: it touches every `Point` call site for no behavioural
    gain over the two-method shape. Re-open it if Phase 3d (3D water) happens.
* **The `floating` flag becomes per-query, not per-plan.** Today it is set from the START only
  [cited collision.rs:1943-1953], so a dry-start route that enters water mid-way evaluates its
  water cells with dry-land rules (they only work because the bespoke edges bypass the façade).
  With the water edges routed through the façade, the swim methods carry their own water
  semantics and the plan-global flag reduces to the start/goal anchor choice.
* **`is_standable` / `ClearanceField`: unchanged.** A water surface is never a standable floor;
  `ground_at`'s "waterline reads as edge" behaviour [cited traversability.rs:260-262] stays
  correct for dry routing. Swim nodes are exempt from ground margins (already true via `floating`
  [cited :522]) and from `ground_at`; `wall_at` may serve the §4b hug cost at swim height —
  whether to extend the field's key with a medium bit or compute uncached is an implementation
  detail, decided by measurement.
* **Diagnosis becomes water-aware.** `diagnose_unreachable` drops its hardcoded
  `floating: false` [cited collision.rs:1457-1460]. New cold-path refusals name what actually
  stopped the route: `goal_blocked_by { hazard: Water }` for a goal in water no plan can exit to;
  a frontier blockage at the waterline when every rim cell fails `can_haul_out` — surfaced through
  the existing `nav_reason`/`goal_blocked_by` channel on `/v1/observe/debug` [cited
  collision.rs:271, :358]. Hot path stays allocation-free; the fast/cold agreement property test
  extends to the two new method pairs.

**OUTPUTS:** the same façade, two methods richer; water hazards impossible to silently omit from a
new edge type; failed water plans that say *why* in water terms.

---

## 6. Design area 4 — controller alignment (#359)

**INPUTS:** `MoveIntent` (unchanged shape); the shared `Body::{float_depth, haul_out_up}`; the
collision mesh; the water map.

**LOGIC — three changes, all in `movement.rs` (plus one relocation decision):**

1. **Single-source the constants.** Delete both local `FLOAT_DEPTH` consts [cited movement.rs:275,
   :288] and the local `BUOY_RATE` pair [cited :274, :287]; read `PLAYER_BODY.float_depth` (and a
   single `BUOY_RATE`). Buoyancy behaviour is numerically identical — this is the
   drift-unrepresentable move.
2. **Collide the vertical swim.** Replace the raw write `self.pos[2] += wish_vspeed * dt` [cited
   :267-269] with a swept vertical move: cast up from the body top (`pos + height`, `height = 6.0`
   [cited traversability.rs:123]) and clamp the rise short of the first solid hit — the same
   ray+radius discipline the horizontal `slide` uses [cited movement.rs:349-379]. Also clamp the
   rise so the feet do not leave the water column mid-swim (the lip itself is mounted by step-up,
   not by flying out of the pool). This kills #359's second mechanism (rise-into-ceiling →
   depenetration slam-back, §1b.2). Downward `wish_vspeed` gets the same sweep against the floor.
3. **Surface-approach for haul-out (E3's controller half).** When swimming and the active waypoint
   is standable ground above the swim plane, the swimmer rises (collided, #2) to the surface as it
   closes on the lip, so the residual riser is `≤ haul_out_up = 2.0` from the surface — within the
   existing swimming step-up capability (`STEP_UP + GROUND_SNAP_TOL = 2.5` [derived §1b.1]).
   Today's version of this lives in the nav driver (`SWIM_UP_RATE`, `action_loop.rs:2673-2674`
   [cited]) and works by aiming `wish_vspeed` at high waypoints. **Decision for the owner
   (§10.3):** keep that driver-side shape (smaller change; controller stays dumb) or move
   haul-out approach into the controller (a `MoveIntent`-visible "waypoint is a haul-out" hint or
   an internal rule; planner-independent, WASD benefits too). This design recommends the minimal
   version: keep the driver-side trigger, make the vertical collided (#2), and let the shared
   constants guarantee the sizes match.

**OUTPUTS:** a swimmer that floats at `surface − float_depth`, rises without embedding in
ceilings, and can mount exactly the lips the planner promised — with the promise and the
capability derived from the same two `Body` fields.

**Regression tests (from #359's own sketch [cited]):** a swimmer at `surface − float_depth`
adjacent to a ledge at `surface + h` must exit for every `h ≤ haul_out_up` and must be refused by
the *planner* for every `h > haul_out_up` — the drift-apart property, pinned (§7 P1).

---

## 7. Design area 5 — honesty (the agent-honesty invariant applied to water)

The planner must distinguish water-crossable from water-uncrossable, and say which — never a
silent wedge, never a false `no_path`, never a confident route into water it cannot exit.

* **No route into an inescapable pocket.** With §4c, a swim node's outgoing land edges exist only
  where `can_haul_out` passes. A pool whose entire rim exceeds `haul_out_up` is *topologically*
  a dead end for full routes to dry goals — A* cannot emit a crossing that strands. The existing
  partial-route trim [cited collision.rs:2472-2482] stays as the belt for partials.
* **No false `no_path`.** The flip side: `haul_out_up` sized too small re-breaks legal exits (E1's
  failure mode — it would have turned the qcat spawn shaft into an honest-but-wrong
  `unreachable`, §4c table). The P1 property test pins both directions, and the E3 margin (0.5 u
  [derived]) absorbs quantisation.
* **Failed water plans explain themselves in water terms** (§5): `goal_blocked_by: water`,
  frontier blockage at the waterline, and a cold-path-only rim diagnosis ("no haul-out on this
  pool's rim within `haul_out_up`") — computed at most once per failed plan, never on success,
  preserving the zero-diagnosis-on-success guarantee [cited traversability.rs:958-970].
* **Degraded-mode visibility.** If a zone has no `.wtr` (water map absent), swim edges don't
  exist and water reads as plain missing-floor. That is today's behaviour [cited
  collision.rs:701-703]; keep it, but surface it — a plan refused at a waterline in a zone with
  no water map should say so (`nav_support`-style counter or a `nav_reason` qualifier), so an
  agent is TOLD it is navigating water-blind rather than being quietly handed dry-land answers.
  (Same principle as `facing_blind_hits` [cited collision.rs:424-428].)
* **Provenance in the plan log.** The `find_path` anchor log already tags floating starts
  ("WATER SURFACE — floating") [cited collision.rs:2337-2341]; extend to floating goals.

---

## 8. #423 — scope recommendation (not a fix design)

**What it is:** walls are passable where the far side is water; the character walks through solid
geometry and starts swimming (qcat dry tunnels over flooded tunnels). Bisected pre-`f820c6c`,
owner-reproduced; a collide-and-slide / collision-mesh bug, explicitly NOT planner work and NOT
agent-honesty (position reporting stays truthful) [cited #423].

**Recommendation: keep it a separate track.** Rationale:
* It shares no code path with this design's changes (planner edges, anchors, buoyancy targets).
  Candidate causes are in the walker's wall handling near water or the mesh itself [cited #423].
* It does **not** block Phase 3 validation, because the acceptance gate (§9) runs in clean water
  zones and qcat is excluded from gating [cited collision.rs:4391-4396 — "qcat is deliberately
  NOT a dry gate zone… the owner's call"].
* It DOES block *qcat-wide* acceptance and any zone-wide qcat walker runs. The one qcat check
  this phase still wants — the #329 spawn-shaft escape at `(-43, 1009, -69.97)` [cited #359
  comment] — is a fixed-location repro whose route does not obviously cross a #423 wall spot;
  run it as a *targeted* live check, labelled as such, and treat any anomaly as possibly-#423
  before blaming the water work. **[guess** that the spawn-escape route avoids #423 geometry —
  the fixer must confirm on the nav-debug overlay**]**
* One ordering note: §6.2 (collided vertical swim) plausibly *reduces* some #423 symptoms if any
  involve the uncollided rise, but no claim is made — #423 reproduces with manual WASD driving
  [cited #423], which never sets `wish_vspeed` upward through geometry. Do not let this phase
  claim #423 progress without its own bisect-grade evidence.

---

## 9. Design area 6+7 — validation, acceptance gate, and phasing

### The gate (what "done" means)

1. **Unit/property tests (tier-1, prove "cannot")** — all offline fixtures, no live play needed:
   * **P1 — the drift-apart property (from #359's sketch [cited]):** fixture pool, ledge at
     `surface + h`, `h` swept in 0.25 u steps over `[0, 2 × haul_out_up]`. For every `h ≤
     haul_out_up`: planner emits the haul-out AND a stepped `CharacterController` starting at
     `surface − float_depth` exits onto the ledge. For every `h > haul_out_up`: planner refuses
     (and diagnosis names Water/no-haul-out). Planner-legal ≡ controller-executable, by test.
   * **P2 — never route into an inescapable pocket:** flooded pit, rim everywhere above
     `haul_out_up`, dry goal beyond: `PlanOutcome` must be `Unreachable` (or a partial trimmed at
     the dry edge), NEVER a route containing a pit water cell. Extends the existing flooded-pit
     test [cited collision.rs:2547-2578] with its negation.
   * **P3 — ceiling-flush water:** the qcat spawn shaft as a fixture (floor −69.97, water
     −69.5…−43.0, ledge −41.97, ceiling flush with a mid-column water line [cited #359]) — the
     full plan+step escape succeeds; the collided rise never embeds (position never inside
     geometry, no depenetration recoveries during the swim).
   * **P4 — hot/cold agreement extended** to `can_swim_traverse`/`can_haul_out` (the existing
     property-test pattern [cited traversability.rs:836-868]).
2. **Walker-sim water gate.** Prerequisite: teach the drift sim to drive swim legs the way the
   real driver does — `want_swim` on body-in-water and the swim-up vspeed — instead of hardcoded
   `want_swim: false` [cited collision.rs:4374], and remove the swim-route exclusion from its
   corpus filter [cited :4442-4460] in water mode. Then `DRIFT_INCLUDE_WATER=1` changes meaning:
   water crossings in CLEAN zones must **succeed** (walk the route end-to-end), not be quarantined;
   the `water` column becomes a regression count with target 0 in gate zones. qcat stays
   visibility-only until #423 and #329 are closed (it is triply-confounded: #423 + #329 + this
   phase's own subject [cited collision.rs:4391-4396]).
3. **Gate zones:** halas (pool crossing, live-verified once already [cited #197]) and qeynos2
   (the moat, the ascent edge's motivating case [cited collision.rs:2215-2216]). Both must be
   confirmed to have real water regions in their `.wtr` before being written into the gate
   (every zone ships a `.wtr` file; file presence ≠ water presence) — **[guess** until a one-line
   region scan verifies**]**. Add 1-2 more clean water zones from that scan.
4. **Targeted live checks (prove "can", per the verification hierarchy — live runs validate
   premises, never universals):** halas pool `zone_cross` re-run; qeynos2 moat in-and-out; the
   #329 spawn-shaft escape (with the §8 #423 caveat). On the release binary, per the standing
   rule.

### Phasing (each sub-phase = one PR-able unit with its own gate slice)

* **Phase 3a — the haul-out contract (fixes #359, unblocks #329).** §2 Body fields; §6.1
  single-sourcing; §6.2 collided vertical swim; §4c E3 planner cap re-based on the shared
  constants; §4a/4b swim-plane re-basing (the 2 u node-height shift). Tests P1, P3. Smallest
  diff, highest value; everything else layers on it.
* **Phase 3b — honesty and symmetry.** §4d floating goal anchor; §5 façade methods
  (`can_swim_traverse`/`can_haul_out`) with water edges routed through them; water-aware
  `diagnose_unreachable`; §7 diagnostics and degraded-mode visibility. Tests P2, P4.
* **Phase 3c — the validation gate.** Walker-sim swim capability; `DRIFT_INCLUDE_WATER`
  promotion from visibility to acceptance in the verified clean zones; live checks; close #359,
  #197, and (if the targeted repro passes) #329.
* **Phase 3d (OPTIONAL — owner decision, default: not now).** 3D submerged routing (under-ceiling
  swims, flooded tunnels, breath modelling). No currently-filed bug requires it (§3 R3); open it
  only against a concrete zone/bug.

---

## 10. OPEN DECISIONS FOR THE OWNER

1. **The haul-out contract (§4c): E1 / E2 / E3, and the value of `haul_out_up`.** Recommended:
   **E3 with `haul_out_up = 2.0`** (= `STEP_UP`; 0.5 u executable margin [derived]; covers the
   qcat ledge at surface + 1.03 u [derived]). A larger value (e.g. 3-4 u, closer to "what real EQ
   effectively does" [cited #359 sketch]) buys more routable exits but requires a genuine mantle
   capability the controller does not have — that would be its own design.
2. **Water representation (§3): R1 implicit queries (recommended) vs R2 baked surface layer.**
   And within R1: memoise `surface_z` per cell now, or only after profiling (recommended: only
   after).
3. **How much the controller changes (§6.3):** keep the haul-out approach in the nav driver
   (recommended, minimal) vs move surface-approach logic into `CharacterController`. Either way
   §6.1/§6.2 (single-sourcing + collided vertical) are treated as non-optional bug fixes —
   confirm.
4. **#423 scope (§8):** separate track (recommended) — and specifically, whether the targeted
   #329 spawn-shaft live check may run before #423 is fixed, or must wait.
5. **Crossing-edge wall handling (§4b):** add the swim-height wall hug COST (recommended) or keep
   crossings cost-blind to in-water walls as today.
6. **Phasing (§9):** land 3a alone first (recommended) vs 3a+3b together; and whether Phase 3d
   (3D water) is wanted on the roadmap at all now.
7. **Gate zone list (§9.3):** approve halas + qeynos2 pending the `.wtr` region verification, and
   how many additional clean water zones the gate should carry.

---

*Prepared for owner sign-off; no implementation has been started. Everything cited was read on
`main @ 3d39536` (2026-07-16); the issue quotes are from #197, #329, #359, #423 as of the same
date.*
