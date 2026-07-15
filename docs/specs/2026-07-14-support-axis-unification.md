# The traversability framework: where the character can go, and how PR-D's `is_standable` fits in

**Status:** design for owner approval. No `src/` code is cut until signed off.
**PR-D umbrella issue:** #375 (support-axis drift, reopened with the live qcat evidence). Must not reintroduce
#329 (ceiling-as-floor).
**Framework umbrella:** #378 (unify the mutually-blind notions of "walkable"). PR-D is **one mode** of it.
**Where the code will go:** `src/assets.rs` (the floor/column model + A\* edges), a test suite, and later a
re-bake criterion for `eqoxide_asset_server`.

---

## What this document is

The owner's point, correctly made: **"can the character be here / go there" is not one question — it is FIVE**,
and PR-D's `is_standable` answers only the first (standing on flat floor). This doc first states the whole
**traversability framework** — the five ways a character moves through a zone, how each is represented today,
and (critically) **proves `is_standable` as designed does not preclude any of the other four** — and then gives
the PR-D support-axis algorithm in full. Support is Section A of the framework; the algorithm that was this
doc's original subject is unchanged, just placed in context.

The framework is the shape behind #378. `is_standable` is a **cell/floor property**. The other four modes are
**edges** layered on top of the cell model, or **dynamic mechanisms** that the static cell model structurally
cannot hold. Getting the layering right is what lets the owner approve `is_standable` knowing exactly where it
sits — and knowing it doesn't quietly break swimming, dropping, teleporting, or riding a lift.

---

## The five traversal modes at a glance

| # | mode | representation | existing machinery (cited) | tracked issue | status | does `is_standable` preclude it? |
|---|---|---|---|---|---|---|
| 1 | **Support** (stand on flat floor) | **cell property** = `is_standable` | `column_hits`/`ground_below` (`assets.rs:1259`, `:1497`) | **#375** (PR-D) | the fix | it **IS** this mode |
| 2 | **3D water volume** (swim) | **edge types** (surface / ascent / descent) + buoyancy | `assets.rs:2470-2588`; controller buoyancy `movement.rs` | #197, #309, #359 | partial | **No** — proof in §B |
| 3 | **Drop from a ledge** (asymmetric fall) | **edge type** (controlled fall) | `assets.rs:2590-2612`; `MAX_STEP_DOWN=60`, `MAX_FALL=120` | #313 | partial | **No** — proof in §C |
| 4 | **Teleport pad** (intra-zone discontinuous link) | **edge type** (region → destination) | modeled on zone-line `DRNTP` (`assets.rs:1092`, `:2315`) | **#403 (NEW)** | gap | **No** — proof in §D |
| 5 | **Moving / moveable geometry** (lift, boat, door) | **dynamic** (time-varying) — NOT a static cell | doors bypass collision; boats/lifts unmodeled | #240, #194 | deep gap | **No, but it can't SOLVE it either** — §E |

"Partial" = the edges exist but have known holes (the cited issues). "Gap" = not represented. The one row that
matters most for approval is the last: mode 5 is the deepest gap, and `is_standable` neither helps nor hurts it —
§E is explicit that PR-D does **not** solve moving floors and must not be read as if it does.

**The headline result:** `is_standable` is a *more permissive AND better-anchored* floor test than today's
facing filter. Every other mode's edges are built on top of "is there floor at the two ends?" — and PR-D makes
that underlying question **more** correct (it stops deleting real floor), so it **strengthens** the foundation
those edges stand on rather than fighting them. The proofs (§B-§E) show this mode by mode. **I found no mode
that `is_standable` precludes.** The one place it needs a one-line guard is called out in §C (anchoring must be
a support property, not an A\* edge filter — it already is, but the code must keep it that way).

---

# Section A — Support (mode 1): the PR-D algorithm

## The problem, in three sentences

The planner and the walker use **different tests for "is there floor here?"**, and they disagree. The walker
accepts **any** near-flat surface under its feet; the planner throws away surfaces whose art faces **down**
before it even looks. So in a zone whose real floor was built from down-facing ("inverted") art, the planner
believes there is no floor where the walker is standing — and wedges the character with a full route in hand.

**Live proof (qcat, 2026-07-14):** a character wedged terminally at `(4.0, 809.8, -43.0)`. The walker's floor
test (`ground_below`, `assets.rs:1497`) found solid ground at **z = −42.97** and stood on it. The planner's
floor test (`column_floors`) found **only z = −55.97** and never once anchored to the surface the walker was
actually on, so it looped. No wall was involved — this is purely the two sides disagreeing about the floor.

---

## THE ALGORITHM

The whole fix: replace the planner's "floor" test and the walker's "ground" test with **one shared predicate**,
`is_standable(...)`, that both call. Because there is only one test, they cannot disagree.

### Inputs

| input | meaning |
|---|---|
| `(x, y)` + a candidate surface at height `z` | the surface we're asking about, in one vertical column |
| `nz` | how flat the surface is: `1.0` = perfectly flat, `0.0` = a vertical wall. Its **sign** says which way the art faces (up = floor-art, down = ceiling-art) — **we deliberately ignore the sign.** |
| `AGENT_HEIGHT` | how tall the character is (from the one shared `Body`; PR-A's `Body::height`) |
| `NEAR_HORIZONTAL` | the "flat enough to stand on" cutoff, derived from the walk-grade limit `MAX_WALK_GRADE = 1.2` (`assets.rs:2154`) |

### Logic

A surface is **standable ground** when **all three** are true:

1. **Flat enough (facing-blind):** `|nz| >= NEAR_HORIZONTAL`.
   We take the *absolute* value — a floor and a ceiling are equally flat, and we do **not** trust the art's
   winding to tell us which is which. This is the change: today the planner discards down-facing surfaces
   (`nz <= 0`) *before* this test (`assets.rs:1259-1260`); we stop doing that.

2. **Headroom:** there is at least `AGENT_HEIGHT` of open space **above** the surface before the next solid
   thing. A real floor has metres of air above it; a ceiling has rock right above it and **fails** this test.
   **Headroom is what replaces "the art must face up"** — instead of trusting the winding, we ask "is there
   room to stand?"

3. **Anchoring:** the character can only reach this surface from a neighbouring surface at a *similar height*.
   A\* follows the terrain step by step; it cannot teleport the character onto a slab with nothing under it.

### Output

- `true`  → "a character can stand here." The planner treats it as floor; the walker clamps to it.
- `false` → "not standable" — it's a wall (fails #1) or a ceiling (fails #2 or #3).

### Where it plugs in

`is_standable` becomes the single body of **both** `column_hits(filter=true)` (the planner's floor lookup) and
`ground_below` (the walker's clamp). Today those are two different tests; after PR-D they are one — that is the
entire "one source of truth" idea. `AGENT_HEIGHT`/`NEAR_HORIZONTAL` come from the same shared `Body` as PR-A, so
there is no second copy of the number. No navmesh, no baking — this is one extra winding-blind walk up the
column, cached per cell.

---

## Why the obvious shortcuts are WRONG (the #329 trap)

Two tempting simplifications both reintroduce the ceiling-as-floor bug (#329), which is why the cancelled
navmesh (#372) failed:

- **"Just drop the facing filter" (`|nz|` alone):** now accepts **ceilings** as floors — they're flat, just
  upside down. This is exactly #329: at qcat the column is `[roof 391.8, floor −70.0]`, and a facing-blind test
  with no headroom check stood a character on the roof and routed through rock (`assets.rs:1293-1295`).
- **"Just check for air above" (headroom alone):** fails on a lone ceiling slab that happens to have open sky
  above it — there *is* air above, so a naive headroom test admits it.

**The correct test needs headroom AND anchoring together.** A ceiling either has rock above it (fails headroom),
or — if it's a lone slab with open sky — is only reachable from thin air (fails anchoring, because there's no
floor under it to step from). Neither defence alone is enough; both are required.

---

## The test that proves it's safe (this gates the merge)

> ### ⚠️ CORRECTED at D-2 implementation — the original open-air-ceiling fixture below is RETRACTED.
>
> The D-1 fixture (kept verbatim below for provenance) asserted that a **down-facing surface with OPEN SKY
> above it** (floor z=0, ceiling z=8, nothing on top) is a ceiling `nearest_floor` must never return.
> **The D-2 shape probe (`probe_qcat_column_vs_fixture`, measured 2026-07-14) FALSIFIED that premise:** the
> character's walkable qcat surface at **−42.97 is DOWN-facing, has NOTHING solid above it, and an up-facing
> floor 13u below** — geometrically *identical* to the fixture's z=8. So "down-facing + open sky above =
> ceiling" is **false**; qcat proves such a surface is walkable floor (the #375 fix). A classifier that
> rejected the fixture's z=8 would also delete qcat's walkway. The owner reviewed the measurement and
> **replaced the open-air fixture with two gates that hold** (see below).
>
> **The real discriminator is not "open sky above" but "a ROOF close above."** A ceiling is a ceiling because
> it has a roof — that is what `headroom` measures. An open-sky down-facing plane is a *platform*, and per
> qcat, walkable.
>
> **The corrected #329 gate (both, owner-approved):**
> 1. **Close-roof** (`close_roof_ceiling_is_rejected_by_headroom`, synthetic, mutation-checked): a down-facing
>    ceiling with a solid roof **within `NAV_AGENT_HEIGHT` above** → `headroom < NAV_AGENT_HEIGHT` → rejected.
>    This is NOT the #372 decorative cheat: there the slab was cosmetic while the classifier still used
>    winding; here the roof-above IS the classifier's real input. Mutation: drop the headroom test → the
>    ceiling is wrongly admitted → RED.
> 2. **Far-ceiling** (`qcat_pocket_nearest_floor_is_never_the_ceiling`, asset): the qcat-pocket roof at 391.8
>    (457u above the −66 floor) is never returned at a REALISTIC `ref_z`, because the `ref_z ± window`
>    excludes it. (The retracted `fallback_never_admits…` queried AT roof height — a position no character is
>    in; the window is the real defence.)
>
> **Q1 answered (measured, `q1_headroom_seal_measurement`, 2026-07-14):** does the headroom test re-delete
> legit inverted ledges? Over the inverted-art zones, `is_standable` **RECOVERS 91–95%** of footprint-fitting
> surfaces (highpass 4978/5364, permafrost 5773/6321, neriakc 1893/1984, qcat 4578/4833); only **3–6%** are
> headroom-rejected (real close-roof ceilings), and **corpus route-success did NOT drop (99.50% → 99.54%)** —
> so those rejects are ceilings, not legit floors being sealed. No stop-and-report.

### (retracted D-1 fixture, kept for provenance — FALSIFIED by qcat, see the box above)

A deliberately adversarial synthetic column:

- a **floor** at `z = 0`,
- a **down-facing ceiling** at `z = 8` **with open sky above it** (nothing on top).

~~This is the hard case: the ceiling's only difference from a floor is its winding *plus* the fact that there's a
floor 8u below it and no rock above it. A `|nz|`-only classifier admits it (wrong). An "air-above-only"
classifier admits it too (wrong). **The correct classifier rejects it** (you'd have to stand at z=8 with
nothing under your feet) and returns the **floor at z=0**.~~ **← FALSIFIED: qcat's −42.97 walkway IS exactly
this shape and is walkable. There is no per-surface way to reject z=8 while accepting −42.97; the discriminator
is a close roof, not open sky. Replaced by the two gates in the box above.**

The fixture asserted `nearest_floor` at that XY returns **z=0, never z=8** — RETRACTED.

> **Q1 — RESOLVED (see the ⚠️ box above).** The original recommendation ("a standable surface within one step
> below") is superseded: `headroom` = distance up to the next SOLID surface (either winding), and a surface is
> a ceiling iff that roof is within `NAV_AGENT_HEIGHT`. Measured: 91–95% recovery, 3–6% (ceiling) rejects,
> route-success flat. This is what the reviewer should
> attack hardest.

---

## What could go wrong, and the guard against it

Every past tightening of the planner's floor test **sealed zones** (the coarse capsule sweep cost −29% route
success in akanon, `assets.rs:1559-1564`; the winding filter deleted 65% of highpass's floor, #375). PR-D
mostly opens things up (it *admits* floor the filter was deleting), but the new headroom test could newly
**reject** a low tunnel the character technically fits under. So it can cut both ways, and the gate is
non-negotiable:

1. **Corpus route-success must not drop.** Baseline this session: **2388/2400 = 99.50%**. Any regression blocks
   the merge (stop-and-report, same protocol as PR-B).
2. **A swim-capable scanner variant.** The current drift scanner *skips water*, which is exactly where the qcat
   wedge lives — so it literally cannot see this bug today. PR-D must extend it to drive swim/buoyancy, classify
   a "support-drift" wedge (walker finds floor the planner doesn't), and prove that count drops to **0** on
   qcat and the other inverted-art zones (#375 names highpass, permafrost, neriakc).
   > **⚠️ SUPERSEDED at D-1 implementation — see the Addendum below.** The swim-capable *corpus* scanner was
   > NOT built as written; a static disagreement scan + the focused qcat pair replaced it, deliberately. The
   > D-2 gate is **restated** in the Addendum. This bullet is kept verbatim only to show what the original
   > design said before the deviation.
3. **The #329 fixtures above stay green and mutation-checked.**

---

## ADDENDUM (D-1 implementation, 2026-07-14): the swim-scanner deferral and the restated D-2 gate

This addendum records a deliberate deviation from the design above, made while implementing D-1 (PR #405), so
the owner sees that "the swim-capable scanner" (gate item #2 above, and the D-1 plan-table row) became
"static disagreement scan + focused qcat pair" — and why. **No code changed to accommodate this; it is a
scoping/gate clarification.**

### 1. What was built instead, and WHY the swim *corpus* scanner is deferred

D-1 ships, in place of a swim-capable corpus scanner:
- **`qcat_support_floor_is_visible_to_the_planner`** — a focused, deterministic RED-on-main assertion at the
  live wedge XY: the planner's `column_floors` omits the −42.97 floor the controller's `ground_below` stands on.
- **`floor_model_disagreement_scan`** — a STATIC corpus scan of planner-vs-controller floor-model disagreement.
- **`faithful_walker_drift_corpus`** — the dry (water-skipping) per-tick-recovery dynamic harness.

The support-axis drift is a **static property of the floor model**: at the wedge point the two floor *queries*
disagree **whether or not anyone is swimming**. So it is provable and gate-able deterministically, without
simulating buoyancy. A swim-simulation *corpus* scanner would add real **buoyancy-fidelity risk** (float rate,
haul-out sizing, surface-vs-body probes — themselves the subjects of open bugs #197/#309/#359) **without adding
discrimination at the qcat point** — the focused pair already proves the fix there. Building a fragile swim sim
to re-derive a fact a two-line static assertion pins would be motion, not progress. The dry faithful scanner is
kept because per-tick *steering* recovery genuinely needs simulation (that is dynamic, not static); *support*
does not.

### 2. The restated D-2 gate (this replaces "swim scanner → 0")

Since "the swim scanner's qcat support-drift count → 0" no longer exists as written, **D-2 passes iff ALL of:**
- **(a)** `qcat_support_floor_is_visible_to_the_planner` flips **RED → GREEN** (the planner now sees the floor
  the controller stands on);
- **(b)** `floor_model_disagreement_scan` **drops on the inverted-art zones** (qcat / highpass / permafrost /
  neriakc) — and, because after D-2 both sides call one `is_standable`, is **0 by construction** everywhere
  (a nonzero result means the two sides were not actually unified — a blunt but real regression catch);
- **(c)** corpus **route-success ≥ 99.50%** (`fine_tier_corpus`), stop-and-report on ANY drop;
- **(d)** the **#329 gates stay GREEN and mutation-checked** — `open_air_ceiling_is_never_returned_as_floor`
  and `qcat_pocket_nearest_floor_is_never_the_ceiling`, with the D-2 mutation (swap in either naive shortcut)
  making the open-air fixture return z=8 → RED.

That is the unambiguous bar for D-2's reviewer.

### 3. The water axis is not silently dropped

The live qcat wedge's start anchor carried a `(WATER SURFACE — floating)` component, so the water axis is
genuinely relevant to this bug. It is covered as follows, and the split is deliberate:
- **The FIX is proven at the water-adjacent point:** the qcat pair (RED→GREEN) sits exactly on the flooded
  walkway, so D-2 is verified where the water actually matters.
- **The swim-capable *corpus breadth* (how many water-adjacent points across all zones) is DEFERRED** to a
  tracked follow-up, to be built only if the static scan + focused pair prove insufficient in practice. It is
  **not** a D-2 gate. If it is built, it extends `faithful_walker_drift_corpus` with the swim/buoyancy drive
  (`want_swim` + `wish_vspeed`, mirroring `navigation.rs`) and un-skips water journeys — the same harness, one
  more intent. Until then, the water-axis *correctness* is gated (the qcat pair); only its *corpus breadth* is
  outstanding.

## Staying honest

Today a counter `nav_degraded{reason:"inverted_floor_art"}` fires when the old column-bottom patch recovers
inverted ground. That patch goes away with PR-D, but the signal must not vanish — replace it with
`nav_support{reason:"facing_blind_ground", queries:N}`, a per-zone counter (reset on zone change, never
silently absent) telling an agent "this zone's floor is partly inverted art; pathing here is on winding-blind
ground." A degraded/fallback mode must never be silent.

## The client-vs-art split (your "Both" decision)

The client fix lands **first and unconditionally** — a shipped client meets zones nobody re-baked, so it must
be robust on its own. The asset re-bake (correcting the inverted winding at the source, in
`eqoxide_asset_server`) is then an **optional cleanliness pass** that reduces how often `nav_support` fires, not
a correctness dependency. The swim scanner's per-zone support-drift count is exactly the list of zones whose art
most wants re-baking — that list is what I'll hand the asset effort once this rule is settled.

## The plan — three small, independently-mergeable steps

| step | what | gate |
|---|---|---|
| **D-1** | Tests only: **[shipped in PR #405]** the focused qcat RED-on-main repro + the #329 open-air-ceiling & qcat-pocket gates + a STATIC `floor_model_disagreement_scan` + the dry `faithful_walker_drift_corpus` harness. The swim-capable *corpus* scanner is **deferred** (see Addendum §1/§3) — the drift is static, so the qcat pair proves it deterministically. **RED on `main`** (proves the bug before any fix). | the qcat support-drift repro is RED on main; #329 gates green + mutation-checked |
| **D-2** | The shared `is_standable` predicate used by both the planner and the walker; delete the old column-bottom patch. | route-success ≥ 99.50%; all #329 tests green + mutation-checked; qcat support-drift → 0 |
| **D-3** | The `nav_support` honesty counter; `docs/http-api.md` updated. | a test that it fires on inverted art and is null on clean art |

D-1 is tests-only and safe to write on approval. D-2 is the behaviour change and gets its own independent
review.

---

# Section B — 3D water volume (mode 2): swim

**What it is.** Water is a **volume you move through in 3D**, not a surface you stand on. You enter it, sink or
swim, cross at the surface, and haul out onto a ledge. It is the one mode that is genuinely not "floor."

**How it's represented today (edges, cited).** A\* has four water edge types, all in `astar` (`assets.rs`):
- **WATER DESCENT** (`assets.rs:2470`): drop/swim DOWN into water below the current floor (upper walkway → flooded
  lower level).
- **WATER ASCENT** (`assets.rs:2504`): swim UP a submerged column and haul out onto a ledge within
  `WATER_EXIT_UP = STEP_UP + 0.5` of the surface (`assets.rs:2527`).
- **WATER SURFACE TRAVERSAL** (`assets.rs:2551`): swim ACROSS a body at its surface instead of diving.
- plus the **floating start anchor** (`assets.rs:2107`): a character with no footing under it is anchored to the
  water **surface**, not a slab, so A\* plans from the tier it is actually floating on.
The controller side is buoyancy + swim intents in `movement.rs` (`want_swim`, `wish_vspeed`, the qcat body-probe
`WATER_BODY`).

**Known holes (partial, cited):** #197 (swimmer strands on a pool bottom / partial reachability past a pool),
#309 (ladders don't let you climb OUT of water — climbable surfaces non-functional), #359 (haul-out sizing: A\*
sizes the exit from the surface but the controller floats 2u below it, eating the whole `STEP_UP`).

**Honest limit:** there is **no true open-volume 3D swim graph** — no free 3D pathfinding through a water body.
Water is navigated by these *edges between floors/surfaces* plus the controller's buoyancy, which is enough for
"cross the pool / haul out / drop into the flooded level" but not for, e.g., threading a 3D underwater cave with
no floor reference. That is a deeper gap, unfiled as a distinct issue; the edge model has carried every shipped
case so far.

**Proof `is_standable` does not preclude it.** `is_standable`'s **headroom** test is "distance to the next
**SOLID** surface above." **Water is not solid.** So a wade/pool floor with water (not rock) above it still has
its `AGENT_HEIGHT` of headroom and is **still classified standable** — exactly as `ground_below` treats it today.
The floating-start anchor and the water edges query floors/surfaces at *both ends*; PR-D makes those floor
queries *more* correct (it stops deleting inverted pool-bottom floor), so it **helps** water nav, doesn't fight
it.

- **Fixture (D-1):** a column with a floor at `z=0` and a **water region** (not a solid triangle) from `z=2`
  upward. Assert `is_standable(z=0)` is **true** (water above ≠ rock above → headroom passes). Mutation: make the
  headroom test count water as "solid above" → the floor is wrongly rejected → RED. This pins that headroom is
  *solid-surface* distance, never *water-surface* distance.

---

# Section C — Drop from a ledge (mode 3): the asymmetric vertical transition

**What it is.** You can step DOWN off a ledge much farther than you can step UP onto one. Up is capped at
`STEP_UP = 2.0` (a hard native limit, `movement.rs:23`); down can be many units (walk off, or a controlled
fall). The transition is **asymmetric** — that asymmetry is the whole mode.

**How it's represented today (edges, cited).** Two mechanisms in `astar`:
- **Walk-down edge:** the neighbour loop admits a lower floor up to `MAX_STEP_DOWN = 60` below the current cell
  (`assets.rs:2148`, `:2376-2377`) while climbs are capped at the feet-ray/`STEP_UP` — the asymmetry, in the edge
  test.
- **CONTROLLED FALL edge** (`assets.rs:2590`): step off into open air and fall to a floor up to `MAX_FALL = 120`
  below, at a huge `FALL_PENALTY = 50_000` (`assets.rs:2608`) so it is a last resort, with the walker refusing a
  lethal drop (`fall_would_be_lethal`, navigation.rs).

**Known weak spot (cited):** #313 — nav routes UP a steep slope it then cannot descend, stranding the character
high (Butcherblock). That is an edge-asymmetry/grade bug, in the drop/climb edges — **its own fix, separate from
PR-D.** Do **not** fold a drop-edge fix into the support predicate.

**Proof `is_standable` does not preclude it — and the one guard that must hold.** A drop connects two surfaces.
`is_standable` classifies **both** the high ledge and the low landing as ground (each has its own headroom); the
DROP itself is the **edge decision** (walk-down or controlled-fall), made in `astar`, **not** in `is_standable`.
So the support predicate cannot block a legitimate descent — it never sees the pair, only each surface alone.

- **The subtle trap the owner flagged — anchoring must NOT read as "no descending."** Logic rule #3 (anchoring)
  says a surface is standable only if reachable from a neighbour at a *similar height*. Read carelessly, that
  sounds like it forbids dropping to a far-lower floor. **It does not, and the code must keep it that way:**
  anchoring is a **support property of a single column query** — "is the reference-z I'm asking about actually
  over/near this surface, or am I asking about a slab in the sky?" — it is about the **query's own z vs the
  surface**, evaluated per surface. It is **not** an A\* edge filter and says nothing about neighbour-to-neighbour
  height change. The walk-down/fall edges (`MAX_STEP_DOWN`, `MAX_FALL`) own descent, and they run *after*
  `is_standable` has independently confirmed the landing is ground. **`is_standable` must never be given the
  source cell's z as the reference for the destination surface** — it is asked about the destination column on
  its own terms. This is a one-line contract, and D-2's review must check it.
- **Fixture (D-1):** two floors, `z=0` (high) and `z=-40` (low), no wall between. Assert `is_standable` is
  **true** at *both*, independently. Then assert A\* still emits the controlled-fall edge from high→low (the drop
  is not blocked). Mutation: make `is_standable` take the neighbour's z as reference and reject the low floor →
  the drop edge vanishes → RED. This pins that anchoring is per-column, not a descent filter.

---

# Section D — Teleport pad (mode 4): the intra-zone discontinuous link

**What it is.** Step on a pad and you are instantly relocated elsewhere in the **same** zone (the North Qeynos
paladin-guild pad; the Temple-of-Life bind pocket). Terrain-follow A\* has **no concept** of a discontinuous
link, so it floods a reachable component that doesn't contain the goal and returns a **false `no_path`**.

**Status: GAP — newly filed as #403** (agent-honesty: it produces a false definitive "no route" for a reachable
goal). I hit it myself in Stage-1: from the lvltest bind pocket in `qeynos2` (~`(-677,-187,-14)`), a `goto` to a
normal street point returned `no_path / search_closed`; a manual walk out of the pocket then let a fresh `goto`
route fine. The pocket is sealed to terrain-follow A\* and exits via a pad the planner can't represent.

**Precedent (cited):** zone-line crossings are the SAME shape — discontinuous links — and are already modeled as
`DRNTP` BSP regions carrying a destination index, with a special A\* arrival test (`zone_line_at`,
`assets.rs:1092`; ZONE-LINE ARRIVAL `assets.rs:2315`; #174/#229). An intra-zone pad is a zone-line minus the zone
change: a region that relocates you. The fix is an **edge type modeled on that machinery**, out of scope for PR-D.

**Proof `is_standable` does not preclude it.** `is_standable` classifies the pad floor and the destination floor
as ordinary ground — correctly. What is missing is the **link edge**, which lives in the region/edge layer, not
the floor model. PR-D neither adds nor blocks it; it only makes the two endpoint floors classify correctly. When
the teleport edge is built (#403), it will query `is_standable` at both ends and benefit from the more-correct
floor test.

---

# Section E — Moving / moveable geometry (mode 5): the deepest gap, and the honest limit

**What it is.** A **lift** (Kelethin/gfaydark platform, Qeynos boat) whose floor moves in **time**; a **door**
that flips blocking↔open. The character must ride a floor that translates, or pass an opening that changes state.

**Why `is_standable` structurally CANNOT absorb it.** `is_standable` is a **static column query** — "at this
`(x,y)`, is the surface at height `z` ground *right now, as baked*?" A lift's floor is at a **different z at a
different time**; a boat's floor **translates in x/y**; a door is **present or absent depending on state**. A
static, time-independent predicate has no axis to express any of that. This is not a tuning gap — it is a
representation gap. **PR-D does not solve mode 5 and must not be read as if it does.**

**How the pieces sit today (cited):**
- **Doors:** deliberately **not in the collision mesh** — they are server-side entities published to
  `doors_shared` and actuated via `build_click_door` (`navigation.rs:362`, `:902`), so A\* plans *through* door
  openings and the door state is handled out-of-band (nav memory `navpath-floor-fix`: "doors aren't in collision
  (don't affect A\*)"). This works for the common "closed door you can open" case and is why doors are not a
  live nav blocker — but it also means a *permanently* blocked/locked door is invisible to routing.
- **Boats / lifts:** **unmodeled** — #240 (navigation over moving objects: Kelethin lift + Qeynos boat), #194
  (boats render as placeholder humanoids, sink below the waterline, no rider/vehicle mechanic). These need a
  **time-varying mechanism** — platform-riding (parent the controller to a moving floor), dynamic collision, or
  door-state edges — which is **its own architecture**, the #240 family, entirely separate from PR-D.

**Proof `is_standable` does not preclude it (and does not pretend to solve it).** Because mode 5 needs a
*dynamic* layer that does not exist yet, `is_standable` simply operates on the static baked geometry as it does
today — it neither helps nor blocks a future platform-riding mechanism, which would sit *above* the cell model
(re-parenting the controller, or injecting time-varying edges) and would call `is_standable` for the *static*
parts of the world around the moving object. The honest statement for the owner: **mode 5 is out of scope for
PR-D and for the whole traversability-predicate idea; it is a dynamic-collision problem tracked in #240/#194.**

---

# Section F — What's settled, and the framework claim to attack

(Support-mode gates, the honesty counter, and the D-1/D-2/D-3 plan are in Section A above; this is the wrap-up.)

## What's settled vs what's my call (attack this)

- **Settled (owner-decided / live-measured):** the support axis is a real, live-confirmed terminal wedge; the
  fix must be facing-blind **plus** headroom/anchoring, never `|nz|` alone; the open-air-ceiling fixture must
  gate it; route-success must not regress; client-robust first, then fix the art.
- **My proposal (open to change):** the exact `headroom` definition (Q1 above); whether to delete the
  column-bottom patch or keep it as belt-and-braces; the `nav_support` field name; the D-1/D-2/D-3 split.
- **The framework claim (attack this hardest):** that `is_standable` precludes none of modes 2-5. The proofs are
  §B (water = non-solid headroom), §C (drop = separate edge + per-column anchoring contract), §D (teleport =
  separate edge), §E (moving = separate dynamic layer). **The one contract that could break if ignored** is §C's:
  `is_standable` must be asked about a destination column on its **own** reference-z, never the source cell's z,
  or anchoring would wrongly veto legitimate drops. It already works this way; D-2's review must keep it so.

---

*Note: this file began as the PR-D support-axis design (clarity-rewritten by the coordinator), then was expanded
into the full traversability framework at the owner's request — the five traversal modes, each with its
representation, existing machinery, tracked issue, and a proof that `is_standable` does not preclude it. No
load-bearing constraint, citation, or gate from the support-axis design was dropped; Section A is that design in
context. The filename is retained for link stability even though the scope is now the framework (#378). Earlier
drafts are in this file's git history on `worktree-nav-drift`.*
