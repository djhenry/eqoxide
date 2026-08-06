# Design: the contact-probe blind band (#854)

**Date:** 2026-08-05
**Status:** design only — no fix in this PR.
**Issue:** #854 (`agent-honesty`). Adjacent: #855 (the `nearest_hit` epsilon cliff — being worked
concurrently, **no PR open at the time of writing**; see §5, which is written as a conditional
rather than as a reading of their diff), #423, #329, #661, #649, #359.

---

## 0. Provenance — what was measured and what was reasoned

Everything in §1–§3 tagged **MEASURED** was produced in this worktree at `origin/main` (7b1d87d),
with `cargo test` (dev profile), on hand-authored geometry built through the helpers in
`tests/synthetic_scenes/mod.rs`, driving the real `CharacterController::step` at `dt = 1/60`. The
scratch harness and the three candidate patches were reverted before this document was committed;
the fixture is re-specified in §5 so the numbers can be re-derived rather than trusted.

Claims tagged **REASONED** are read off the code and were *not* run. They are marked inline every
time. This project's dominant defect class is a mechanism sentence nobody measured
(`[[eq-docs-are-the-honesty-surface]]`), so the tags are load-bearing, not decoration.

Per `tests/synthetic_scenes/mod.rs`'s own warning: a synthetic scene that agrees with a shipped
zone is evidence the **mechanism** is understood. It is not a second measurement of the shipped
geometry, and the numeric agreement below with #854's qcat figures is arithmetic (the constants
were copied), not corroboration.

---

## 1. The defect, re-measured

`slide()` (`src/movement.rs:1185`, `1195-1202`) casts horizontal contact rays at exactly two
heights above the body origin — `Body::foot` = 0.5 and `Body::chest` = 4.0
(`crates/eqoxide-nav/src/traversability.rs:152,157`). Nothing between the feet and +0.5 is ever
tested horizontally.

**MEASURED — the raw query.** A 2.0-long horizontal ray at the swim-clamped feet height
(`surface − SKIN` = −55.978 − 0.05 = −56.02800) against a wall whose top is −55.99, i.e. 0.038
above the feet:

| probe height above feet | `Collision::nearest_hit` |
|---|---|
| +0.00 | `Some(t = 0.750, n = [−1, 0, 0])` |
| +0.01 | `Some(t = 0.750, n = [−1, 0, 0])` |
| +0.05 | `None` |
| +0.10 … +0.45 | `None` |
| **+0.50 (shipped)** | **`None`** |
| +1.00, **+4.00 (shipped)** | `None` |

**MEASURED — at the controller.** Driving `wish_dir = [1,0]`, `wish_vspeed = +10`,
`want_swim = true`, `speed = 35`, `dt = 1/60`, the frame that crosses the wall plane, instrumented
inside `step()`:

```
pos=[-0.41666, 0, -56.02799]  low_hit=false  low_prog=0.5833  wish=0.5833
                              stepped=false  ducked=false  applied=[0.16667, 0, -56.02799]
```

`low_hit = false` and full progress: the wall plane is crossed with **no contact of any kind**, at
a z 0.038 **below** the wall's top. The body then reads dry and lands on the slab at −55.96875.

**MEASURED — reach control.** Same scene, same drive, the east wall raised to full height: the body
stops at east −0.41666 and never crosses. The crossing is produced by the wall's *top height* and
by nothing else in the fixture.

**Fixture limitation, recorded rather than buried.** #854's falsification control — a zero-pitch
drive stalls wet for ever — did **not** reproduce here. A zero-pitch drive from the buoyancy plane
(−57.978) also exits east, but by a different mechanism: it stops embedded at east −0.41666 and is
then moved to the slab by the depenetration net (z −57.978 → −55.96875 in one frame, with no
vertical wish). That is the #649 lid push-out. My fixture puts a lid over the pocket, so it is a
#649 scene as well as an #854 scene. This is **not** a refutation of #854's control against baked
qcat; it is a statement that *this* fixture cannot carry that control, and §6 does not pin one.

---

## 2. Why a foot-plane probe is not the patch — the `STEP_UP` tension, measured

**MEASURED — the ramp.** Walking a grade-0.8 ramp (`flat_run_into_a_walkable_ramp`), sampling the
contact ray at three heights every frame:

| ray height | on the flat | at the ramp toe | during the whole ascent |
|---|---|---|---|
| +0.00 | `none` | `t = 1.000, nz = 0.824` | `none` |
| +0.05 | `none` | — | `t = 0.125, nz = 0.824` **every frame** |
| +0.50 | `none` | `none` | `none` |

So a naive foot-plane ray fires on the floor the body is standing on. But the ramp's contact normal
has `|nz| = 0.824`, `Body::near_horizontal` is 0.64, and a vertical wall's is 0.000 (§1). **A
standability veto on the contact normal separates the two cleanly.** That single measurement is
what makes a foot-plane probe viable at all.

It is not sufficient. Three variants were built and measured.

### Variant 1 — walls-only feet-plane probe in *every* slide (low, step-up's raised, duck's lowered)

**MEASURED.** Three existing unit tests go RED:

- `movement::tests::steps_up_a_2u_ledge`
- `movement::tests::a_swimmer_at_a_solid_bank_still_hauls_out_the_duck_does_not_override_191`
- `movement::tests::a_swimmer_hauling_out_at_a_legitimate_bank_never_raises_the_afloat_stall`

and the step ladder moves: a flat run east into a vertical face with floor beyond at the same
height crosses at h = 0.30 / 0.45 / 1.00 / 1.90 / **2.40** on `main`, but **2.40 is refused** under
variant 1. The mechanism is direct: `try_step_up` raises the body by `STEP_UP` = 2.0 and re-slides,
so its feet plane sits *exactly at* a 2 u ledge's top — and the probe blocks the step-up's own
escape route. The controller's real climb capability drops from 2.5 to 2.0, which is precisely the
`STEP_UP + GROUND_SNAP_TOL = 2.5` figure `traversability.rs` sizes `haul_out_up` against.

**MEASURED — and it does not even close #854:** the low slide blocks, and `try_step_up` then
carries the body over the rim to the same final position. Blocking the wall is not the same as
closing the exit.

### Variant 2 — walls-only feet-plane probe on the **low** slide only

**MEASURED.** 85/85 movement unit tests green. Step ladder identical to baseline. Ramp climb
unchanged. And the #854 crossing frame becomes:

```
pos=[-0.41666, 0, -56.02799]  low_hit=true  low_prog=0.0000  wish=0.5833
                              stepped=true  ducked=false  applied=[0.16667, 0, -55.96875]
```

The wall is contacted, horizontal progress is **zero**, and the crossing is performed by
`try_step_up` mounting the rim — the body goes **over** the wall instead of through it.

**MEASURED — but it introduces a land-wide wedge class.** A lip of height 0.30 or 0.45 with the
floor beyond dropped to −2 or −10 becomes *permanently impassable* (baseline crosses and falls;
variant 2 parks the body at east −0.16667 for ever, `on_ground = true`). `try_step_up` refuses
because there is no floor in its landing band, and there is no step-**over** path. The entire
existing suite stayed green through this.

**MEASURED — a tangency hazard, in both directions.** A bare cliff edge whose face top is *exactly*
at the feet z still crosses under variant 2; the 2 u ledge whose face top is *exactly* at the
raised feet z did *not* under variant 1. Exact tangency is decided by floating-point boundary
behaviour in the triangle test and must not be relied on in either direction.

### Variant 3 — walls-only feet-plane probe on the low slide only, **and only while not grounded**

**MEASURED.**

| check | result |
|---|---|
| #854 crossing frame | `low_hit=true low_prog=0.0000 stepped=true` — pass-through closed |
| lip-with-drop (0.30 / 0.45, drop −2 / −10) | all crossed, identical to baseline — regression gone |
| step ladder (0.30 … 2.40 cross, 3.00 / 6.00 refuse) | identical to baseline |
| `cargo test --lib movement::` | 85 passed, 0 failed |
| `tests/synthetic_water_capability.rs` | 6 passed |
| `tests/synthetic_walk_profile.rs` | 4 passed |
| `tests/synthetic_goal_append.rs` | 5 passed |

---

## 3. Option space

| # | Option | Fixes | Breaks | Cost/frame | vs `STEP_UP` |
|---|---|---|---|---|---|
| A | Feet-plane probe, unconditional | the band everywhere | **MEASURED:** 3 unit tests RED; climb capability 2.5 → 2.0; 2.40 curb impassable | +1 ray per slide iteration in all 3 roles | fatal — the step-up blocks its own raised slide |
| A′ | + standability veto, low slide only (variant 2) | **MEASURED:** the pass-through | **MEASURED:** lip-with-drop wedges on land | +1 ray per low-slide iter (≤ 3 casts) | compatible; ladder unchanged |
| B | Swept band / capsule contact replacing the rays | the whole class, incl. hanging slabs (see §7) | the step-up's raised slide would contact the very ledge it exists to clear; A's failures return in a stronger form | several × a ray cast per candidate triangle, every frame (**REASONED** — not profiled) | requires re-expressing the step-up as a band-clearance query — a different design |
| C | Probe height derived from the remaining step budget | nothing #854 has | — | as A′ | the budget is `STEP_UP` = 2.0 whenever the body can step at all, so the derived height is ≥ 0.5 in exactly the failing case; where the budget is 0 it degenerates into E with a misleading name |
| D | Stop the swim clamp parking feet in the band (clamp to `surface − float_depth`) | the measured qcat trigger | the #359 haul-out geometry: `haul_out_up` = 2.0 is measured **from the surface**, the swimming step-up reaches `STEP_UP + GROUND_SNAP_TOL` = 2.5 **from the feet**; parking feet at `surface − 2.0` makes the tallest admissible lip 4.0 against a 2.5 reach (**REASONED** from `traversability.rs`'s field docs, arithmetic not re-run) | nil | leaves the defect: the band is a body-model defect, and every non-water entrance (ballistic, levitating, a #543 pad landing) is untouched |
| **E** | **A′ + support gate (variant 3)** | **MEASURED:** the pass-through, with no measured regression | see §8 | +1 ray per low-slide iter, **only while not grounded**; 0 when grounded | compatible; ladder byte-identical to baseline |

**Recommendation: E.**

A is refuted by measurement. A′ trades a water-only pass-through for a land-wide wedge — a strictly
worse bug in a project whose backlog is mostly wedges. B is the right long-term contact model and
the wrong response to #854: it is a rewrite of the contact resolver and of `try_step_up` together,
on the code path that runs every movement frame, to fix a defect that E closes with one gated ray.
C is E wearing a label that says "step budget" while meaning "who owns the body's z" — and a
misleading name in this codebase is how #386/#312-class drift starts. D re-opens #359 and fixes one
door of a room with several.

---

## 4. The bad state, and why E makes it unrepresentable

The `foot = 0.5` probe is not a mistake. It is a **free-step allowance**: the body's XY is admitted
wherever the space at `feet + 0.5` is clear, and the vertical is corrected *afterwards* by the
ground clamp, which lands the feet on the lip. The allowance is sound exactly when a floor clamp
will consume it. When the body's z is owned by the water-surface clamp — or by buoyancy, or
ballistically, or by levitate — nothing consumes it, and the allowance becomes a licence to
translate through solid geometry.

> **The bad state is not a probe height. It is a free-step allowance granted to a body whose
> vertical is not owned by a floor.**

Today `slide()` takes a bare `from: [f32; 3]` and cannot tell (a) whether it is resolving the real
body or evaluating a hypothetical raised/lowered one, nor (b) what owns the body's z this frame.
Both facts exist at all three call sites (`src/movement.rs:776`, `:1256`, `:1317`) and are thrown
away. E passes them as types:

```rust
/// What owns the body's vertical THIS frame. Exhaustive on purpose: adding a variant is a
/// compile error at `free_step` until its allowance is stated. (Same discipline as #826/#838.)
enum VerticalOwner { Floor, WaterSurfaceClamp, Buoyant, Ballistic, Levitating }

impl VerticalOwner {
    /// How much lip this body may cross without the collide-and-slide seeing it.
    fn free_step(self) -> FreeStep {
        match self {
            // a floor clamp will consume the allowance in the same frame
            VerticalOwner::Floor => FreeStep::UpTo(PLAYER_BODY.foot),
            // nothing consumes it — the lowest contact ray must sit at the feet
            VerticalOwner::WaterSurfaceClamp | VerticalOwner::Buoyant
            | VerticalOwner::Ballistic   | VerticalOwner::Levitating => FreeStep::None,
        }
    }
}

/// Which body a slide is resolving.
enum SlideRole { RealBody(VerticalOwner), Hypothetical }   // step-up raised / duck lowered
```

Two states stop being writable:

1. **`SlideRole::Hypothetical` carries no `VerticalOwner`**, so the step-up's raised slide and the
   duck's lowered slide *cannot* be given a feet contact by accident. Variant 1's measured
   regression is not guarded against — it is unrepresentable.
2. **`free_step()` is a `match` with no `_` arm.** A sixth vertical mode (a mount, a knockback, a
   #543 pad) cannot silently inherit the grounded body's free step; the compiler demands its
   allowance be stated.

And the probe height stops being an independent number: the lowest contact ray's height **is**
`free_step`'s height, so "the probe sits above the allowance" has no representation either.

`FreeStep::None` additionally selects the walls-only veto on that ray (`|n_z| < near_horizontal`),
because §2 measured a coplanar floor hit at the feet plane. **UNMEASURED:** whether the veto is
load-bearing *under the support gate* — the gated probe only runs when no floor owns the body's z,
so there may be no coplanar floor to reject. Variant 3 was measured **with** the veto. The
implementation must include a run with the veto removed and record the result; a filter nothing
needs is a future false explanation, and this repo has shipped several.

**Placement.** `VerticalOwner` and `SlideRole` are controller concepts and belong in
`src/movement.rs`. Only the free-step *height* is read from `traversability::PLAYER_BODY.foot`, so
the shared crate's public surface does not widen and the #420 `feet_clr = foot + step_up`
derivation the planner depends on is untouched.

**One honest wrinkle.** The horizontal move runs *before* the vertical branch in `step()`, so
`self.on_ground` there is last frame's resolution — the owner is one frame stale on a support
transition. `swimming`, `in_water` and `levitating` are all current at that point, so only the
`Floor`/`Ballistic` distinction is stale, and the cost is at most one frame of free step on the
frame a body leaves the ground. Variant 3 was measured with exactly this staleness. Restructuring
`step()` to resolve support first is out of scope and should not be smuggled in.

---

## 5. Composition with #855 — a conditional, not a reading of their fix

**What this section is NOT.** At the time of writing, **#855 has no open PR**. Nothing below was
verified against an implementation; every statement here is **REASONED from #855's issue text and
from the current source of `Collision::nearest_hit`**, and each is written as a conditional on what
#855 turns out to do. If the eventual fix differs in shape, the conditionals — not the conclusions —
are what should be re-checked. Naming a mechanism in someone else's unwritten diff as a known
consequence is precisely the habit this repository has spent several issues unlearning.

#855 reports an acceptance cliff at `t > 1e-3` in `Collision::nearest_hit`
(`crates/eqoxide-nav/src/collision.rs:1674`), whose blind radius is `1e-3 ×` ray length. E's new
feet-plane ray is a caller of that function, so it inherits the cliff in its own units:
`1e-3 × speed·dt` = 5.8e-4 at 35 u/s and `dt = 1/60`. #855's own blind-radius inventory lists
`slide`'s contact rays at 0.6–44 mm across the speed range.

**Conditional 1 — if #855 tightens or removes the epsilon** (the shape its title implies):

- **E does not depend on it.** E closes the pass-through with the epsilon exactly as it stands
  today — that is what §2's variant-3 row measures, on unmodified `collision.rs`. MEASURED.
- **E is strengthened, not endangered.** A smaller epsilon can only let `nearest_hit` return hits it
  previously missed, which for a contact probe can only *add* contacts, never remove one. REASONED
  from the filter's form (`if t > 1e-3 && t <= 1.0 && …`), not run against a patched epsilon.
- **The regression that would have scaled with it is gone by construction.** A′'s lip-with-drop
  wedge got worse the more eagerly the probe fired. E's support gate means grounded bodies never run
  the probe at all, so no epsilon change can reintroduce it on land. This is a property of the gate,
  which is measured; the inference to "therefore any epsilon" is REASONED.

**Conditional 2 — if #855 instead fixes the caller** (e.g. a floor guard on `swim_sink`, which its
"Not the duck" section leaves open as a possibility) then E is untouched: it shares no caller with
the swim-descent path.

**Conditional 3 — if #855 changes `nearest_hit`'s signature or return type**, E's call site must
follow. This is the only way the two can conflict, and it is a mechanical conflict, not a semantic
one. E adds no code *inside* `nearest_hit`.

**Not affected either way — the tangency results.** The exact-coincidence outcomes in §2 (a face top
exactly at the ray's z, decided oppositely in two scenes) fall out of Möller–Trumbore's
`|det| < 1e-6` degeneracy test and its barycentric bounds, not out of the `t > 1e-3` cliff.
REASONED from reading the traversal; not isolated experimentally.

**Merge order.** E touches `src/movement.rs` only; #855's issue points at
`crates/eqoxide-nav/src/collision.rs`. Under conditionals 1 and 2 the two changes touch disjoint
files and either order is safe. `[[eq-cross-pr-semantic-conflict]]` is the reason this is stated
rather than assumed: two PRs each green on their own base turned main red once here, because the
gate reviews PRs and not merge *order*. The behavioural overlap to re-check before the second merge
is exactly one line — whether E's feet probe still reports contact at the #854 rim under the merged
epsilon. §6.2's example test is that check, and it should be re-run on the merge commit, not only on
each branch.

**Deliberate non-dependency.** E uses a *post-hoc* normal veto (`|n_z| < near_horizontal` applied to
whatever `nearest_hit` returns) rather than a filtered traversal (`nearest_hit_where`). The known
weakness of the post-hoc form is that a nearer standable face can hide a wall behind it inside one
frame's travel. Under the support gate that nearer standable face is uncommon — the body is not
floor-owned — and building a filtered sibling would put this change **inside the function #855 is
working in**. If the hiding case turns out to be reachable it is a follow-up issue, filed *after*
#855 lands. Not concurrent surgery on a function someone else is mid-fix in.

---

## 6. Test plan

### 6.1 Property test — the universal

**Statement:** *a body whose vertical is not floor-owned cannot translate its XY across a face
plane at a z below that face's top.*

Generate a vertical face at east 0 whose top height sweeps `wt ∈ [feet − 1, feet + 1]`, and drive
the body horizontally into it under each non-`Floor` `VerticalOwner`. Assert the XY never crosses
the plane while `z < wt`.

- **Coverage assertion, not just cases.** The harness must record that at least one generated `wt`
  landed in `(feet, feet + 0.5)` *and* at least one above `feet + 0.5`, and **fail** if the band was
  not reached. A generator that silently misses the band is `[[eq-guard-reach-control]]` again — a
  scanner that covered 12 % of its corpus and looked green.
- **Mutation check (deletion):** remove the feet probe → RED on the `(feet, feet + 0.5)` cases,
  GREEN above. This is a *known* RED, not a hoped-for one: §1 measures `low_hit = false` at
  `wt = feet + 0.038` on today's code.
- **Mutation check (wrap), the one that matters:** wrap the probe in `if false { … }`, and
  *separately* weaken the veto to `|n_z| < 0.0` (never fires). Each must go RED **independently**.
  A probe that is written but never reached passes a deletion test that only greps.
  (`[[eq-source-text-pins-prove-written-not-reached]]` — seven measured evasions to date.)

### 6.2 Example test — the repro

A new builder in `tests/synthetic_scenes/mod.rs`,
`flooded_pocket_with_a_rim_under_the_waterline()`: pocket east [−40, 0] × north [−20, 20], floor
−70, a slab at −55.96875 spanning east [−40, 40] (lid over the pocket, tile floor beyond it), a
water box over the pocket from −69.5 to surface −55.978, three full-height walls, and the **east
wall running only to −55.99**. Paired reach control: the identical builder with the east wall at
full height.

Assertions: driven at `wish_vspeed = +10`, the body's east coordinate never advances while its z is
below the rim; and the frame that does advance it does so with `on_ground` newly true at the slab
height — it **mounted** the rim.

- **Mutation, direction 1:** revert the probe → RED, with the body crossing at constant
  z = −56.02799 (the trace in §1).
- **Mutation, direction 2:** use the full-height-wall control → the test must still pass **and** the
  body must never cross at all. This is what distinguishes "mounted the rim" from "the scene has no
  exit", and it is the check the #852 "sealed on all six sides" retraction existed for.

### 6.3 Negative controls that must not move

- **The step ladder:** 0.30 / 0.45 / 1.00 / 1.90 / 2.40 crossed; 3.00 / 6.00 refused. Pin it —
  variant 1 moved the 2.40 boundary and nothing in the existing suite noticed.
- **Lip-with-drop:** 0.30 and 0.45 lips with the floor beyond at −2 and −10 still crossed. Pin it —
  variant 2 broke exactly this and the whole suite stayed green.
- **Named existing tests that must stay green**, because they are the ones that actually caught
  variant 1: `steps_up_a_2u_ledge`,
  `a_swimmer_at_a_solid_bank_still_hauls_out_the_duck_does_not_override_191`,
  `a_swimmer_hauling_out_at_a_legitimate_bank_never_raises_the_afloat_stall`.
- **The falsification control — the zero-pitch drive — must be built, and must NOT be pinned to
  this fixture.** #854's control is that the same horizontal drive with *zero* pitch, from the
  buoyancy plane, stalls wet for ever; the escape requires the up-pitch clamp. That control is what
  makes the pitch coupling (`src/app.rs:1648`, `wish_vspeed = dz · MOVE_SPEED`) the trigger rather
  than a coincidence, and without it the example test cannot distinguish "the probe closed the
  pass-through" from "this scene has no exit for anyone".

  It **does not hold in the §6.2 fixture** — MEASURED, §1: a zero-pitch drive there also exits, via
  the #649 depenetration push-out onto the lid, because that fixture has a lid over the pocket and
  is therefore a #649 scene as well as an #854 one. Pinning a control that does not hold is worse
  than having none.

  The implementation must therefore build a **second, lid-less** builder — same pocket, same rim at
  −55.99, same water surface, **no slab over the water**, dry standable floor only beyond the rim —
  and pin the control there: `wish_vspeed = 0` from `surface − float_depth` must leave the body wet,
  un-grounded, and west of the rim for the whole run, both **before and after** the fix. If that
  control also fails to hold in the lid-less scene, the pitch-coupling premise is not reproduced
  synthetically at all and that fact must be reported, not worked around.

### 6.4 What no test here claims

No live run discharges "cannot pass through". A live pass in the shipped zone proves the *premise*
(the geometry is as described); it cannot prove the universal, because a race that usually wins is
indistinguishable from one that cannot lose. `[[eq-verification-hierarchy]]`.

---

## 7. Bound the blast radius

`slide()` runs on every movement frame carrying a horizontal wish. Under E:

- **Grounded bodies: nothing changes.** The probe is gated off. MEASURED: step ladder and
  lip-with-drop ladder identical to baseline; 85/85 movement unit tests; 6/6, 4/4, 5/5 on the three
  synthetic integration binaries.
- **Swimming / buoyant / ballistic / levitating bodies with a horizontal wish** gain a contact at
  the feet plane against non-standable faces. **If the recommendation is wrong, this is where it
  shows:** a swimmer that used to slide along a submerged lip now stops at it; a falling body that
  used to clip a ledge corner now catches on it.
- **The failure mode is extra work, not a new stall** — the step-up and duck branches *arm* on
  `low_hit` (`if (self.on_ground || swimming) && low_hit && …`), so more contacts means more
  step-up/duck attempts, not fewer. **REASONED** from the branch structure, not measured.
- **Cost:** +1 ray cast per low-slide iteration, ≤ `MAX_SLIDE_ITERS` = 3 per frame, and only while
  not grounded; zero when grounded. Arithmetic from the loop bound, **not** a profile.
- **CI reality check:** `tests/walker_sim.rs`'s P1 admission-matches-execution test is the sharpest
  existing guard on the haul-out contract and it is `#[ignore]`d (asset-gated). It will **not** run
  in CI. Say so in the PR rather than counting it as coverage.

**Out of scope, recorded so it is not mistaken for covered:** a hanging slab that touches no
ground — say a lintel spanning `feet + 1.0` to `feet + 3.0` — is invisible to *both* shipped probes
and remains invisible under E. It is the same defect class, and option B is what fixes it.
**REASONED** from the probe heights; not measured.

---

## 8. Agent-honesty statement

**Removed:** the client no longer reports a position reached by translating through a wall's
interior as an ordinary position. MEASURED: the crossing becomes a mount (`low_hit` false → true,
`low_prog` 0.5833 → 0.0000, `stepped` false → true).

**NOT removed, stated plainly:** a **grounded** body still crosses any face whose top is within
`Body::foot` = 0.5 of its feet, and reports the result as ordinary. The design's claim is that this
is not a falsehood, because the ground clamp resolves the body onto the lip in the same frame, so
the reported end-of-frame position is one a step could have reached. That claim is **REASONED**
from the clamp's ordering, supported only by the measured equivalence of the two ladders — it is
not a proof. The residual it leaves is concrete: a grounded body crossing a 0.4 lip whose far side
has *no* floor within the clamp's reach ends up airborne past a wall it never contacted. The
lip-with-drop fixture is exactly that shape, and it is crossed both on `main` and under E. Whether
"step over a curb at a cliff edge and fall" is legal player behaviour or a second instance of #854
is an **owner call**, and this design does not make it — it should be filed as a follow-up rather
than silently absorbed into a fix that claims to close the class.

**Do not read this as closing the qcat escape.** After E, leaving the pocket is a `try_step_up`
whose legality is decided by the haul-out contract (`haul_out_up` = 2.0 from the surface), and it
is reported truthfully — MEASURED, the body ends `on_ground` on the slab having mounted the rim.
The **pass-through** is closed; the **exit** is not, and it belongs to #329 / #661 / #543. Anyone
reading #854 as "the character can no longer get out" will be wrong.

**Two prior diagnoses of #854 were retracted publicly** ("the camera has no collision test"; "the
chamber is sealed on all six sides"). Neither is restated here, and §6.2's paired reach control
exists specifically so the second cannot recur in this fix's own tests.
