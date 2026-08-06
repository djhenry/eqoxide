# Design: the contact-probe blind band (#854)

**Date:** 2026-08-05
**Status:** design only — no fix in this PR.
**Issue:** #854 (`agent-honesty`). Adjacent: #855 / **PR #866** (the `nearest_hit` epsilon cliff —
now open and read; §5 is verified against it, not conditional on it), #423, #329, #649, #359.

---

## 0. Provenance — what was measured and what was reasoned

Everything tagged **MEASURED** was produced in this worktree at `origin/main` (7b1d87d), with
`cargo test` (dev profile), on hand-authored geometry built through the helpers in
`tests/synthetic_scenes/mod.rs`, driving the real `CharacterController::step` at `dt = 1/60`. The
scratch harnesses and every candidate patch were reverted before this document was committed; the
fixtures are re-specified with their **start states** in §6.2 so the numbers can be re-derived
rather than trusted.

Claims tagged **REASONED** are read off the code and were *not* run. They are marked inline every
time. This project's dominant defect class is a mechanism sentence nobody measured
(`[[eq-docs-are-the-honesty-surface]]`), so the tags are load-bearing, not decoration.

**Round 2.** An independent reviewer rebuilt every fixture and candidate patch from scratch at the
same commit and reproduced all four of round 1's measured claims. It then falsified a **REASONED**
sentence in §7 — that E's failure mode was "extra work, not a new stall" — by driving a population
round 1 never drove: bodies that are neither grounded nor swimming. That was correct, and it
changed the recommendation. Round 2 re-ran the elimination with that population included; §2's
variant 4, §3's E′ row, §7 and §8 are the result. Anything the reviewer measured and this document
now repeats is marked **MEASURED (round 2)** and was re-run here rather than copied.

**Round 3.** The same reviewer falsified **two more sentences**, both of which were mine and neither
of which had been measured:

1. "E′ restores every crossing wherever a landing exists." False. The round-2 tables sampled the
   floor beyond the lip at exactly two heights (level, and −10). Sweeping the continuum between them
   shows E′'s residual is **"any landing more than `GROUND_SNAP_TOL` = 0.5 u below the feet"** — an
   ordinary shape, not a bottomless drop — and that the wedge is **silent** (`hold()` is `None`).
2. "The controller has no levitate mode; the levitating body is not measurable today." False.
   `levitating` ships (`src/movement.rs:195`, `:423`, hover branch `:1004`, two green tests). Driven,
   it turns out levitators **already pass through blind-band lips on `main`** — they are inside
   #854's population, not a hypothetical one — and they inherit the residual in full.

Both are corrected below from **runs made here**, not from the review text. The reviewer's root cause
for (1) — `try_step_up` is a step *up* with no downward half — implies a candidate this document did
not contain; §2 works it up as **two** distinct shapes (variants 5 and 6) and §3 records what each
costs. Round 3 sentences are tagged **MEASURED (round 3)**.

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

**MEASURED — the raw query.** Feet at the swim-surface clamp, `surface − SKIN` = −55.978 − 0.05 =
**−56.02800**; wall plane at east 0 with its top at **−55.99**, i.e. **0.038 above the feet**. Each
ray runs **from east −1.50 to east +0.50** (length 2.0) at the stated height, so a hit at the plane
is `t = 1.5 / 2.0 = 0.750`. (Endpoints stated because round 1 omitted them and `t` alone is
unverifiable — reviewer finding 6.)

| probe height above feet | `Collision::nearest_hit(from, to)` |
|---|---|
| +0.00 | `Some((0.75000006, [−1, −0, −0]))` |
| +0.01 | `Some((0.75000006, [−1, −0, −0]))` |
| +0.05 | `None` |
| +0.10, +0.45 | `None` |
| **+0.50 (shipped `Body::foot`)** | **`None`** |
| +1.00, **+4.00 (shipped `Body::chest`)** | `None` |

**MEASURED — at the controller, with its start state.** Fixture as §6.2. Body created at
**east −5.0, north 0, z = surface − SKIN = −56.028** — i.e. already *at* the clamp, which is the
state #854 describes. Drive `wish_dir = [1,0]`, `wish_vspeed = +10`, `want_swim = true`,
`speed = 35`, `dt = 1/60`. Frames 6–9, `pre` = position entering the frame:

```
f= 6 pre=[ -1.5000, -56.0280] post=[ -0.9167, -56.0280] dE= 0.5833 ground=false wet=true
f= 7 pre=[ -0.9167, -56.0280] post=[ -0.3333, -56.0280] dE= 0.5833 ground=false wet=true
f= 8 pre=[ -0.3333, -56.0280] post=[  0.2500, -56.0280] dE= 0.5833 ground=false wet=true   <-- crossing
f= 9 pre=[  0.2500, -56.0280] post=[  0.8333, -55.9690] dE= 0.5833 ground=true  wet=false
```

Frame 8 carries the body's origin from east −0.333 to +0.250 — **across the rim plane** — at
z −56.028, which is 0.038 *below* the rim's top, at full unimpeded speed. The body ends the frame
inside the rim's material band and under the lid; frame 9 reports it dry and grounded on the slab.

**MEASURED — reach control.** The identical builder with the rim wall run to full height: the body
stops at east −0.167 and never crosses, still wet and un-grounded at −56.028 after 900 frames. The
crossing is produced by the rim's *top height* and by nothing else in the fixture.

**MEASURED — the start state is load-bearing.** The same drive started at the **buoyancy plane**
(`surface − float_depth` = −57.978) instead crosses at frame 8 while still *rising*, at z −56.645,
with the rim 0.676 *above* its feet — and is carried over by `try_step_up`, i.e. a legitimate
haul-out, not a pass-through. That run is identical on `main` and under every candidate patch. A
#854 fixture that does not start the body at the clamp measures a different phenomenon.

**Fixture limitation, recorded rather than buried.** #854's falsification control — a zero-pitch
drive stalls wet for ever — does **not** reproduce in this fixture family, and §6.3 now carries the
arithmetic showing that no member of the family can carry it.

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

**MEASURED.** **Four** existing tests go RED (round 1 listed three and missed the fourth; the
reviewer found it and it is re-run here — `cargo test --lib movement::` 82 passed / **3 failed**,
`cargo test --test walker_sim` 10 passed / **1 failed**):

- `movement::tests::steps_up_a_2u_ledge`
- `movement::tests::a_swimmer_at_a_solid_bank_still_hauls_out_the_duck_does_not_override_191`
- `movement::tests::a_swimmer_hauling_out_at_a_legitimate_bank_never_raises_the_afloat_stall`
- **`walker_sim::p1_haul_out_admission_matches_controller_execution`** — which, contrary to what
  round 1's §7 said, is a bare `#[test]` at `tests/walker_sim.rs:116-117`, builds its own quads
  (`:120-135`), is **not** `#[ignore]`d, is **not** asset-gated, and **runs in CI**. It is the
  sharpest guard in the repo on the haul-out contract this design leans on, and it is sensitive to
  exactly the change variant 1 makes.

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
| `cargo test --lib movement::` | 85 passed, 0 failed, **1 ignored** (`depenetration_corpus_over_baked_zones`) |
| `cargo test --test walker_sim` | 11 passed, 0 failed, 5 ignored |
| `tests/synthetic_water_capability.rs` | 6 passed |
| `tests/synthetic_walk_profile.rs` | 4 passed |
| `tests/synthetic_goal_append.rs` | 5 passed |

**MEASURED (round 2) — and this is where variant 3 fails.** Every driver above is grounded or
`want_swim = true`. Driving the population that is *neither* — the one §4's table sends to
`FreeStep::None` alongside the swimmer — variant 3 stops bodies that `main` carries across.

*Buoyant body*: water surface at 2.1, `want_swim = false`, `wish_vspeed = 0`, so buoyancy holds the
feet at `surface − float_depth` = **0.10**; driven east at 35 u/s for 900 frames into a lip at
east 0. Two shapes: **drop** (floor beyond at −10, so a step-up has no landing) and **curb** (floor
beyond at 0, so it does). Final east coordinate; `−0.167` means stopped dead at the lip.

| lip height | drop: `main` | drop: v3 | curb: `main` | curb: v3 |
|---|---|---|---|---|
| 0.30 | 99.58 | **−0.167** | 99.58 | **−0.167** |
| 0.45 | 99.58 | **−0.167** | 99.58 | **−0.167** |
| 0.55 | 99.58 | **−0.167** | — | — |
| 0.60 | 99.58 | **−0.167** | 99.58 | **−0.167** |
| 1.00 | **−0.167** | −0.167 | **−0.167** | −0.167 |
| 2.00 | **−0.167** | −0.167 | — | — |

Two things are in that table. The regression is real: v3 stops a floating body at a 0.30 lip that
`main` crosses, and it is permanent (3600 frames, east −0.167, unchanged). **And so is the
context**: `main` *already* stops the identical body, permanently, at any lip above
`feet + Body::foot` = 0.60 — including the **curb** case at 1.00, where a walkable floor sits at
the same height just beyond and a step-up would trivially reach it. The wedge is not a class v3
invents; v3 lowers an already-present threshold from 0.61 to 0.

*Ballistic body*: no water, driven east from z = 6.0 and from z = 0.25 into the same shapes.
**Identical in every mode**, `main` and v3 alike (0.30 lip: crosses to 99.15; 0.60 lip: stops at
−0.167). A falling body reaches the floor and becomes floor-owned before it reaches the lip, so
`FreeStep::None` is not in force at contact. There is **no measured ballistic regression** — the
reviewer named this owner but did not drive it.

*Levitating body*: **RETRACTED — round 2 said "not measurable today; the controller has no levitate
mode". That was false.** `levitating: bool` is a controller field (`src/movement.rs:195`),
`set_levitating` is public (`:423`), the gravity-off hover branch ships (`:1004`, #529) and two unit
tests cover it. Driving it is one extra call. **MEASURED (round 3)** — levitator hovering at z 0.10
over a floor at 0, driven east at 35 u/s for 900 frames into a lip at east 0, final east (`−0.167` =
stopped dead):

| lip | 0.30 | 0.45 | 0.60 | 0.61 | 1.00 | 2.00 |
|---|---|---|---|---|---|---|
| `main`, curb (floor beyond at 0) | 99.15 | 99.15 | **−0.167** | −0.167 | −0.167 | −0.167 |
| `main`, drop (floor beyond at −10) | 99.15 | 99.15 | **−0.167** | −0.167 | −0.167 | −0.167 |
| v3, either shape | **−0.167** | **−0.167** | −0.167 | −0.167 | −0.167 | −0.167 |

Two consequences, and the second is why the retraction matters more than the arithmetic:

- The levitator is **already in #854's defect population on `main`**, not a future one. It crosses
  0.30 and 0.45 lips by translating through them, on the same `feet + Body::foot` threshold as every
  other owner. `VerticalOwner::Levitating` in §4 is therefore a *live* variant, not a placeholder.
- The frame trace makes the pass-through explicit. **MEASURED (round 3)**, `main`, lip 0.45 topping
  at 0.45, floor beyond at −1.0, levitator hovering at 0.10:

  ```
  f= 7 pre=[ -0.9167, 0.1000] post=[ -0.3333, 0.1000] ground=false
  f= 8 pre=[ -0.3333, 0.1000] post=[  0.2500, 0.1000] ground=false   <-- crossing
  f= 9 pre=[  0.2500, 0.1000] post=[  0.8333, 0.1000] ground=false
  ```

  The reported z is 0.1000 on every frame. The body ends east of a face whose top is 0.45 without its
  reported height ever reaching it. Under v3 frame 8's post is `[-0.3333, 0.1000]` and stays there.

This population matters more than the buoyant one for a *predicament* argument: levitate ignores
vertical input by design (`:1004`'s own doc — "free-floats over land, gaps, and water"; jump and
`wish_vspeed` are discarded), so a levitating player wedged by v3 has no manual escape at all.

### Variant 4 (**E′**) — variant 3, **plus** arming the existing recovery for `FreeStep::None` bodies

The root cause of variant 3's regression, read straight off the branch that was mis-cited in round
1's §7: the step-up/duck recovery arms on `if (self.on_ground || swimming) && low_hit && …`
(`src/movement.rs:790`). Variant 3's probe fires precisely when `on_ground` is false, so outside the
`swimming` subset it adds a contact **with no recovery attached**. The probe and the arm must move
together: *if a body is denied the free step, it must be offered the step.*

Variant 4 extends the arm to those bodies and changes nothing else. MEASURED (round 2), same runs:

| check | `main` | v3 | **v4 (E′)** |
|---|---|---|---|
| buoyant, curb 0.30 / 0.45 / 0.60 | cross | **stop** | **cross** |
| buoyant, curb 1.00 | **stop** | stop | **cross** — better than `main` |
| buoyant, drop 0.30 … 2.00 | cross ≤ 0.60, stop ≥ 1.00 | stop | stop |
| ballistic, both shapes | — | identical | identical |
| grounded ladder (0.30 / 0.45 / 1.00 / 2.40 / 3.00, drop −10) | — | identical | identical |
| #854 crossing frame | pass-through | mount | **mount** |
| `cargo test --lib movement::` | 85 / 0 / 1 ign | 85 / 0 / 1 ign | **85 / 0 / 1 ign** |
| `cargo test --test walker_sim` | 11 / 0 / 5 ign | 11 / 0 / 5 ign | **11 / 0 / 5 ign** |
| synthetic water / walk-profile / goal-append | 6 / 4 / 5 | 6 / 4 / 5 | **6 / 4 / 5** |

#### E′'s residual, measured at its true size — **RETRACTED and replaced**

Round 2 wrote: *"E′ restores every crossing v3 removed **wherever a landing exists**"*, and §3's E′
row said *"Breaks: nothing in the driven population"*. **Both are false**, and the error is visible
in the table above: it tests the floor beyond the lip at exactly two heights, 0.0 and −10. That
binary hides the whole continuum, and the boundary lives inside it.

**MEASURED (round 3)** — buoyant body (feet held at 0.10), lip fixed at **0.45** (inside the band),
sweeping the height of the floor beyond it. Final east after 900 frames:

| floor beyond | +0.45 | +0.20 | 0.00 | −0.25 | −0.38 | **−0.40** | −0.50 | −1.00 | −1.90 | −2.40 | −10.0 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `main` | 99.58 | 99.58 | 99.58 | 99.58 | 99.58 | 99.58 | 99.58 | 99.58 | 99.58 | 99.58 | 99.58 |
| **E′ (v4)** | 99.58 | 99.58 | 99.58 | 99.58 | 99.58 | **−0.167** | **−0.167** | **−0.167** | **−0.167** | **−0.167** | **−0.167** |

The cutoff is exactly `feet − GROUND_SNAP_TOL` = 0.10 − 0.50 = **−0.40**, and it is not a coincidence:
`try_step_up` (`src/movement.rs:1258-1266`) both *searches* and *accepts* only down to
`self.pos[2] − GROUND_SNAP_TOL` —

```rust
let f = col.ground_below(hi[0], hi[1], self.pos[2] + max_step + GROUND_ORIGIN,
                         max_step + GROUND_ORIGIN + GROUND_SNAP_TOL)?;
if f >= self.pos[2] - GROUND_SNAP_TOL && f - self.pos[2] <= max_step + GROUND_SNAP_TOL {
```

— so the recovery E′ arms is a step **up** with no downward half. E′ offers the denied body a step up
and never a step over-and-down. The same sweep on a **levitator** gives the same shape one sample
later (crosses to −0.40, stops at −0.50 and below), because its feet sit at the same 0.10.

So the residual is **not** "a lip with no landing beyond". It is **"any landing more than
`GROUND_SNAP_TOL` below the feet"** — e.g. a submerged 0.3 kerb with the bed a unit lower, or a rock
on a gently falling seabed. That is ordinary geometry.

**And it is silent. MEASURED (round 3):** at every wedged sample above, `CharacterController::hold()`
returns `None` at frame 3600, and the position is unchanged from frame 8. The client reports an
ordinary stationary body to the driving agent. `main`'s own >0.60 wedge is equally silent, so this is
pre-existing *in kind* — but E′ enlarges the set of positions where the client is stuck and says
nothing, and that is the `agent-honesty` class this project ranks first
(`[[eq-agent-honesty-invariant]]`). This is not a mobility nuisance and §8 no longer describes it as
one.

### Variant 5 (**F1**) — E′, plus a **downward half** on the landing gate

The reviewer's root cause names the fix: give the recovery a downward half. The narrowest form keeps
`try_step_up`'s structure and only widens its landing window for the bodies E′ newly arms — search
and accept out to the same envelope the step reaches upward, `STEP_UP + GROUND_SNAP_TOL` = 2.5:

```rust
let reach = max_step + GROUND_SNAP_TOL;                       // 2.5
let f = col.ground_below(hi[0], hi[1], self.pos[2] + max_step + GROUND_ORIGIN,
                         max_step + GROUND_ORIGIN + reach)?;  // was + GROUND_SNAP_TOL
f >= self.pos[2] - reach && f - self.pos[2] <= max_step + GROUND_SNAP_TOL
```

**MEASURED (round 3).** It moves the boundary and does not remove it:

| | buoyant, lip 0.45, floor beyond | levitator, lip 0.45, floor beyond | levitator, lip 0.30/0.45, floor beyond −10 |
|---|---|---|---|
| `main` | crosses all | crosses all | crosses |
| E′ (v4) | stops ≤ −0.40 | stops ≤ −0.50 | **stops** |
| **F1 (v5)** | stops ≤ **−2.40** | stops ≤ **−2.60** | **stops** |

Three findings, all measured:

- **The residual survives, six times further out.** −0.40 becomes −2.40 (= `feet − 2.5`). Every shape
  with a landing deeper than that is still wedged, still silently.
- **It does nothing at all for the case levitate exists for.** A levitator gliding at a gap or a
  −10 drop has no landing within any finite envelope, so F1 refuses exactly as E′ does. That is the
  paradigmatic levitate geometry, and `main` carries it.
- **Where F1 does cross, it costs the hover.** MEASURED, levitator at 0.10, lip 0.45, floor beyond at
  −1.0: frame 8 post is `[0.2500, −1.0000] ground=true`. The body is *landed* on the lower floor —
  a 1.1 u drop out of a hover that `:1004` documents as holding altitude over exactly this shape.
  The step-up's contract is "land on a floor", and handing it to a body whose vertical it does not
  own contradicts §4's own type: `FreeStep::None` means *the owner keeps the z*, and F1 takes it.

### Variant 6 (**F2**) — E′, plus a **step-over** that does not land

The other shape of "a downward half": don't land at all. Raise, sweep, require the destination's foot
plane to be clear — the highest floor at or below the raised probe start must be **at or below the
feet**, which is what proves the obstruction does not extend to where we are going — and then apply
the horizontal move only, leaving the z to whoever owns it:

```rust
fn try_step_over(&self, wish, max_step, col) -> Option<[f32; 3]> {
    let raised = [self.pos[0], self.pos[1], self.pos[2] + max_step];
    let (hi, _) = self.slide(raised, wish, col);
    match col.ground_below(hi[0], hi[1], self.pos[2] + max_step + GROUND_ORIGIN, GROUND_DEPTH) {
        // a floor ABOVE the feet at the destination = the obstruction extends there: that is a
        // MOUNT, leave it to try_step_up (this clause is what keeps #854 fixed)
        Some(z) if z > self.pos[2] + SKIN => None,
        _ => Some([hi[0], hi[1], self.pos[2]]),   // z unchanged: the owner keeps it
    }
}
```

Tried **before** `try_step_up`, only for `FreeStep::None` bodies. **MEASURED (round 3)** — F2 wins
every mobility comparison in this document:

| check | `main` | E′ (v4) | F1 (v5) | **F2 (v6)** |
|---|---|---|---|---|
| buoyant, lip 0.45, floor beyond +0.45 … −10 | crosses all | stops ≤ −0.40 | stops ≤ −2.40 | **crosses all** |
| levitator, same sweep | crosses all | stops ≤ −0.50 | stops ≤ −2.60 | **crosses all** |
| levitator, lip 0.30 … 2.00, drop −10 | crosses ≤ 0.45 | stops all | stops all | **crosses all** |
| levitator hover z across the crossing frame | 0.1000 | (wedged) | **−1.0000** | **0.1000** |
| buoyant, curb 0.30 … 2.00 | crosses ≤ 0.60 | crosses all | crosses all | crosses all |
| grounded ladder, both shapes, 0.30 … 2.80 | — | identical | identical | **identical** |
| #854 crossing frame (§6.2 fixture) | pass-through | mount | mount | **mount** |
| `--lib movement::` / `--test walker_sim` / synthetics | 85·0·1 / 11·0·5 / 6·4·5 | same | same | **same** |

**And the discriminating control that decides against it. MEASURED (round 3).** Two 0.45-topped lips
0.30 apart, floor beyond at −1.0, levitator at 0.10:

```
v6   f= 8 pre=[ -0.3333, 0.1000] post=[  0.2500, 0.1000] ground=false
```

One admitted step carries the body from west of the **first** face to east of the **second**, with
its reported z pinned at 0.1000 the whole way. F2's clearance evidence is taken at
`feet + STEP_UP`; the body never goes there. Nothing in the frame discharges the fiction, because
refusing to write a z is the entire point of the variant.

That is the same sentence #854 is about — *the client reports a position reached by translating
through a face it never went over* — with a bound of `STEP_UP` = 2.0 instead of `Body::foot` = 0.5,
and now **sanctioned** rather than accidental. F1 at least discharges the raise with a real landing
write, which is precisely the fiction the grounded step-up already ships and the ground clamp already
consumes. F2 discharges nothing.

**This is the tradeoff, stated so the owner can take it rather than inherit it:**

| | crossings a `main` body keeps | truthfulness of the crossings it does make | new silent wedges |
|---|---|---|---|
| **E′** | loses those with a landing > 0.5 u below the feet | every crossing is a real mount, z written | **yes** — measured, silent, ordinary geometry |
| **F1** | loses those with a landing > 2.5 u below the feet, and all gap/drop glides | crossings land on a real floor; a levitator loses its hover | **yes** — smaller set, same silence |
| **F2** | loses none | crossings are asserted, not performed; up to 2.0 u of unmodelled rise, multiple faces per step | **no** |

---

## 3. Option space

| # | Option | Fixes | Breaks | Cost/frame | vs `STEP_UP` |
|---|---|---|---|---|---|
| A | Feet-plane probe, unconditional | the band everywhere | **MEASURED:** 3 unit tests RED; climb capability 2.5 → 2.0; 2.40 curb impassable | +1 ray per slide iteration in all 3 roles | fatal — the step-up blocks its own raised slide |
| A′ | + standability veto, low slide only (variant 2) | **MEASURED:** the pass-through | **MEASURED:** lip-with-drop wedges on land | +1 ray per low-slide iter (≤ 3 casts) | compatible; ladder unchanged |
| B | Swept band / capsule contact replacing the rays | the whole class, incl. hanging slabs (see §7) | the step-up's raised slide would contact the very ledge it exists to clear; A's failures return in a stronger form | several × a ray cast per candidate triangle, every frame (**REASONED** — not profiled) | requires re-expressing the step-up as a band-clearance query — a different design |
| C | Probe height derived from the remaining step budget | nothing #854 has | — | as A′ | the budget is `STEP_UP` = 2.0 whenever the body can step at all, so the derived height is ≥ 0.5 in exactly the failing case; where the budget is 0 it degenerates into E′ with a misleading name |
| D | Stop the swim clamp parking feet in the band (clamp to `surface − float_depth`) | the measured qcat trigger | the #359 haul-out geometry: `haul_out_up` = 2.0 is measured **from the surface**, the swimming step-up reaches `STEP_UP + GROUND_SNAP_TOL` = 2.5 **from the feet**; parking feet at `surface − 2.0` makes the tallest admissible lip 4.0 against a 2.5 reach (**REASONED** from `traversability.rs`'s field docs, arithmetic not re-run) | nil | leaves the defect: the band is a body-model defect, and every non-water entrance (ballistic, levitating, a #543 pad landing) is untouched |
| E | A′ + support gate (variant 3) | **MEASURED:** the pass-through | **MEASURED (round 2):** stops a buoyant, non-swimming body at any lip in the band, permanently, because the probe fires where the recovery does not arm | +1 ray per low-slide iter while not grounded | compatible; ladder byte-identical to baseline |
| **E′** | **E + arm the existing recovery for `FreeStep::None` bodies (variant 4)** | **MEASURED:** the pass-through; and one pre-existing `main` wedge (buoyant body, 1.00 curb) | **MEASURED (round 3):** every crossing whose landing is more than `GROUND_SNAP_TOL` = 0.5 u below the feet — buoyant *and* levitating bodies, ordinary geometry, **silently** (`hold()` = `None` at 3600 frames). Not "no landing"; see §2 and §8 residual 2 | as E, plus at most one extra `try_step_up` on frames where the new contact fires | compatible; ladder byte-identical to baseline |
| F1 | E′ + a downward half on `try_step_up`'s landing gate (variant 5) | **MEASURED:** the pass-through; moves E′'s cutoff from −0.40 to −2.40 | **MEASURED (round 3):** the residual survives at `feet − 2.5`; **no** improvement at all for gap/drop glides (levitate's own use case); where it does cross it **lands** the body, dropping a levitator out of its documented hover (0.10 → −1.00, `ground=true`) | as E′; the widened `ground_below` window is one column query with a larger depth, same call count | compatible; grounded ladder byte-identical |
| F2 | E′ + a `try_step_over` that translates and leaves the z to its owner (variant 6) | **MEASURED:** the pass-through; **and every mobility case in this document** — no measured wedge remains, hover preserved, grounded byte-identical, all suites green | **MEASURED (round 3):** it does not remove the falsehood, it enlarges and sanctions it — clearance is checked at `feet + STEP_UP` and the body never goes there; one admitted step was measured crossing **two** band-height faces with reported z pinned at 0.1000 | as E′, plus one extra `slide` (≤ 3 iters × 2 rays) + one `ground_below`, on blocked `FreeStep::None` frames only; it runs *before* `try_step_up` and replaces it when it succeeds (**REASONED** from the call structure — not profiled) | compatible; grounded ladder byte-identical |

**Recommendation: E′ — but this is now a genuine owner call, and §2's three-row tradeoff table is
the thing to decide from, not this sentence.**

**Round 1 recommended E and that was wrong** — not in its mechanism but in its scope. §7 claimed,
REASONED and unmeasured, that E's failure mode was "extra work, not a new stall", citing the arming
branch. The branch says the opposite: it arms on `self.on_ground || swimming` while E's probe fires
when `on_ground` is false. Round 2 drove the gap and found the stall. The fix is not to retreat
from the gate; it is to notice that **the gate and the arm are one decision**. A body denied the
free step must be offered the step. E′ is that, and it is measured.

A is refuted by measurement, on four tests including one that runs in CI. A′ trades a water-only
pass-through for a land-wide wedge that hits **grounded** bodies — the population with an
established 2.5 u climb capability that A′ takes away. That is the disqualifying difference from E′,
and it is the argument round 1 owed and did not make: E′'s residual falls only on bodies for which
`main` already refuses lips above 0.60, so E′ moves a threshold; A′ removes a capability. B is the
right long-term contact model and the wrong response to #854 — a rewrite of the contact resolver
and of `try_step_up` together, on the path that runs every movement frame, to fix a defect E′ closes
with one gated ray and one extended condition. C is E′ wearing a label that says "step budget" while
meaning "who owns the body's z"; a misleading name in this codebase is how #386/#312-class drift
starts. D re-opens #359 and fixes one door of a room with several.

**F1 is eliminated by measurement, not by argument.** It was the obvious reading of "give the gate a
downward half", and it fails on its own terms: it keeps a residual of exactly the same kind six times
further out, it does nothing for the gap/drop glide that is levitate's whole purpose, and the
crossings it does buy are paid for by landing a body whose z it does not own. Nothing recommends it
over either neighbour.

**F2 is not eliminated, and I am not going to pretend it is.** On every mobility measurement in this
document F2 dominates E′: no wedge survives, the hover is preserved, the grounded ladder is
byte-identical, #854 stays fixed, all five suites stay green. If the deciding question were "which
option leaves the fewest bodies stuck", F2 wins outright and E′ should be dropped.

I still recommend **E′**, on one ground, and it is a ranking not a refutation: **F2 buys its mobility
by re-admitting the exact translation #854 objects to.** The step-over asserts a 2.0 u vertical
excursion the body never performs and never reports — measured crossing two faces in a single frame
with z pinned — where E′'s crossings are all real mounts with a written z. Under this project's
stated first priority, a client that quietly reports having got somewhere it did not go is worse than
a client that stops (`[[eq-agent-honesty-invariant]]`). E′'s cost is a *stall*; F2's cost is a
*confident falsehood*, only a wider one than today's.

That ranking depends entirely on that priority ordering, so it is the owner's to overturn:

- If a **checked** step-over counts as a legitimate crossing for a body whose vertical is externally
  owned — i.e. "cleared at `feet + STEP_UP` and the destination's foot plane is clear" is a
  physically meaningful admission rule rather than a fiction — then **F2 is the better option and
  should be taken**, and #854's fix becomes "make the crossing checked", not "make it impossible".
  It would then want a companion issue for the multi-face case measured above (an admitted step
  should probably be limited to one face, or re-swept at the feet plane on arrival).
- If it does not, E′ is the answer and its residual must be made **loud**, which is the next bullet.

**E′ is only defensible with the silence fixed.** The residual's worst property is not the stall, it
is that `hold()` returns `None` while the body cannot move under any driver — measured. A
`ControllerHoldReason` raised when a `FreeStep::None` body is blocked at the feet plane *and* the
recovery finds nothing would convert a silent wedge into a reported predicament, which is the shape
this repo already uses for the other two hold cases (`enter_hold`, §724/#679). **UNMEASURED** — no
hold variant was implemented or driven here — and it should be a stated part of the implementation
issue rather than a footnote, because without it E′'s honesty advantage over F2 is smaller than the
paragraph above claims.

---

## 4. The bad state, and what E′ makes unrepresentable

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
away. E′ passes them as types:

```rust
/// What owns the body's vertical THIS frame. Exhaustive on purpose: adding a variant is a
/// compile error at `free_step` until its allowance is stated. (Same discipline as #826/#838.)
/// Every variant here is LIVE and drivable today — `Levitating` reads `self.levitating`
/// (`src/movement.rs:195`), whose hover branch is at `:1004`. Round 2 called it a placeholder;
/// §2 retracts that and measures it.
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

**What this actually achieves — stated at the size it is** (reviewer finding 5, accepted). The two
misuses variant 1 *exhibited* become unwritable:

1. **`SlideRole::Hypothetical` carries no `VerticalOwner`**, so the step-up's raised slide and the
   duck's lowered slide *cannot* be given a feet contact by accident. Variant 1's measured
   regression is not guarded against — it is unrepresentable.
2. **`free_step()` is a `match` with no `_` arm.** A sixth vertical mode (a mount, a knockback, a
   #543 pad) cannot silently inherit the grounded body's free step; the compiler demands its
   allowance be stated.

And the probe height stops being an independent number: the lowest contact ray's height **is**
`free_step`'s height, so "the probe sits above the allowance" has no representation either.

**What it does not achieve.** `VerticalOwner` is an ordinary value computed at the call site.
Nothing in the type stops a body no floor owns being handed `VerticalOwner::Floor`, and the
compiler forces the *allowance* decision for a new variant but not the *classification* decision —
a future `Mounted` can be added, its allowance stated, and every call site still compute `Floor` for
a mounted body. The **owner → physics mapping stays a runtime obligation**. Round 1's "the bad state
stops being writable" overstated this; the accurate claim is the numbered pair above.

The one-frame staleness below is a live instance of that gap, not a rounding error, and it is
recorded as such.

**A third thing that must move with the type, or the type lies.** `free_step()` is not only a probe
height — it is a *capability statement*, and the recovery must honour it. Concretely
(§2, variant 4): wherever `free_step()` returns `FreeStep::None`, the step-up/duck arming condition
at `src/movement.rs:790` must also arm. Leaving that condition as `self.on_ground || swimming` while
the probe keys off `FreeStep::None` puts two different answers to "which bodies are being denied the
free step" in two places three hundred lines apart — precisely the drift the type exists to prevent.
The implementation should derive the arm from the same `free_step()` call, not from a second
predicate that happens to agree today.

**And a fourth, added in round 3: arming the recovery is not the same as the recovery being able to
help.** E′ arms `try_step_up` for `FreeStep::None` bodies, and `try_step_up`'s landing gate then
refuses every landing more than `GROUND_SNAP_TOL` below the feet (§2, measured). So the type says
"this body is denied the free step, so offer it the step" while the recovery it is offered can only
answer *up*. The type does not close that gap and this document should not pretend it does: a
`FreeStep::None` body needs a **stated policy for what happens when the step refuses**, and there are
exactly three coherent ones — refuse the move and *say so* (E′ + a hold reason), land it lower (F1),
or translate it without landing (F2). §3 costs all three. What must not happen is the current
implicit fourth: refuse, and report nothing.

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

## 5. Composition with #855 / PR #866

**Status change since round 1.** Round 1 was written with no #855 PR open and stated three
conditionals. **PR #866 is now open** and its body was read. The conditionals are resolved below.
What is **VERIFIED** here is verified *against PR #866's stated content*, which is a stronger
warrant than round 1's reasoning and a weaker one than running the merged tree; nothing below is
a claim about behaviour I executed on their branch.

**Citation correction** (reviewer finding 7). The acceptance cliff is at
`crates/eqoxide-nav/src/collision.rs:1775` inside `nearest_hit` (which begins at `:1744`), and its
twin at `:1711` inside `nearest_hit_t` (which begins at `:1676`). Round 1 cited `:1674`, inherited
verbatim from #855's issue body where it is also stale; `:1674` is inside a doc comment. The quoted
*form* — `if t > 1e-3 && t <= 1.0 && …` — was correct.

The blind radius is `1e-3 ×` ray length. E′'s new feet-plane ray is a caller of `nearest_hit`, so it
inherits the cliff in its own units: `1e-3 × speed·dt` = 5.8e-4 at 35 u/s and `dt = 1/60`.

**Resolved 1 — #866 replaces the epsilon with a shared `HIT_WINDOW = 0.0..=1.0` in both scans.**

- **E′ does not depend on it.** E′ closes the pass-through with the epsilon exactly as it stands on
  `main` — §2's variant 3 and 4 rows are measured on **unmodified** `collision.rs`. MEASURED. (The
  reviewer confirmed the same independently.)
- **E′ is not endangered by it.** #866's own §"Effect on `slide()`" reports a sweep of
  speed × `dt` × centre-to-wall gap driving into a wall, with and without the old epsilon, and
  states the resolved positions are **identical in every row**, because the resolver keeps the body
  centre `radius` (1.0) + `SKIN` clear of any detected face and 1.0 dwarfs the largest blind band.
  It states explicitly that "the bottom-0.5 u blind band described in #854 is unaffected — it is a
  probe-placement gap, not a `t`-window gap". VERIFIED against PR #866's body; not re-run here.
- **The regression that would have scaled with it is gone by construction.** A′'s lip-with-drop
  wedge got worse the more eagerly the probe fired. E′'s support gate means grounded bodies never
  run the probe at all, so no window change can reintroduce it on land. The gate is measured; the
  inference to "therefore any window" remains REASONED.

**Resolved 2 — #866 also adds a caller-side guard** (`swim_sink` clamps against a new
`Collision::obstruction_below`). E′ is untouched by it: E′ shares no caller with the swim-descent
path, and `obstruction_below` is a downward column probe, not a horizontal contact ray.

**Resolved 3 — #866 does not change `nearest_hit`'s signature or return type.** The only mechanical
conflict either could have had with the other does not arise. E′ adds no code *inside*
`nearest_hit`, and #866 adds none inside `slide()`.

**Not affected either way — the tangency results.** The exact-coincidence outcomes in §2 (a face top
exactly at the ray's z, decided oppositely in two scenes) fall out of Möller–Trumbore's
`|det| < 1e-6` degeneracy test and its barycentric bounds, not out of the `t > 1e-3` cliff.
REASONED from reading the traversal; not isolated experimentally.

**Merge order.** E′ touches `src/movement.rs` only; #866 touches
`crates/eqoxide-nav/src/collision.rs` and `swim_sink`. Disjoint files, either order safe.
`[[eq-cross-pr-semantic-conflict]]` is the reason this is stated rather than assumed: two PRs each
green on their own base turned main red once here, because the gate reviews PRs and not merge
*order*. The behavioural overlap to re-check before the second merge is exactly one line — whether
E′'s feet probe still reports contact at the #854 rim under the merged window. §6.2's example test
is that check, and it should be re-run on the merge commit, not only on each branch. #866 landing
first is mildly preferable, because §6.2's assertions are then written against the window that will
actually ship.

**Deliberate non-dependency.** E′ uses a *post-hoc* normal veto (`|n_z| < near_horizontal` applied
to whatever `nearest_hit` returns) rather than a filtered traversal (`nearest_hit_where`). The known
weakness of the post-hoc form is that a nearer standable face can hide a wall behind it inside one
frame's travel. Under the support gate that nearer standable face is uncommon — the body is not
floor-owned — and building a filtered sibling would put this change **inside the function #866 is
editing**. If the hiding case turns out to be reachable it is a follow-up issue, filed *after* #866
lands. Not concurrent surgery on a function someone else is mid-fix in.

---

## 6. Test plan

### 6.1 Property test — the universal

**Statement:** *a body whose vertical is not floor-owned cannot translate its XY across a face
plane at a z below that face's top.*

Generate a vertical face at east 0 whose top height sweeps `wt ∈ [feet − 1, feet + 1]`, and drive
the body horizontally into it under each non-`Floor` `VerticalOwner`. Assert the XY never crosses
the plane while `z < wt`.

**The driven population is the universal's real content, and round 1 got this wrong.** The
generator must instantiate **every** owner `free_step()` maps to `FreeStep::None` that the
controller can actually be in — today: swimming at a surface clamp, and buoyant with
`want_swim = false`; ballistic is included but is measured to ground itself before contact
(§2), which is a result the test should record rather than assume. If a future owner is added and
mapped to `FreeStep::None` without appearing here, the property is silently narrower than its
statement. Assert the owner list the harness drove **equals** the set of `FreeStep::None` variants,
so adding a variant fails this test until it is driven.

- **Coverage assertion, not just cases.** The harness must record that at least one generated `wt`
  landed in `(feet, feet + 0.5)` *and* at least one above `feet + 0.5`, and **fail** if the band was
  not reached. A generator that silently misses the band is `[[eq-guard-reach-control]]` again — a
  scanner that covered 12 % of its corpus and looked green.
- **Mutation check (deletion):** remove the feet probe → RED on the `(feet, feet + 0.5)` cases,
  GREEN above. This is a *known* RED, not a hoped-for one: §1 measures the crossing frame advancing
  at full speed at `wt = feet + 0.038` on today's code.
- **Mutation check (wrap), the one that matters:** wrap the probe in `if false { … }`, and
  *separately* weaken the veto to `|n_z| < 0.0` (never fires). Each must go RED **independently**.
  A probe that is written but never reached passes a deletion test that only greps.
  (`[[eq-source-text-pins-prove-written-not-reached]]` — seven measured evasions to date.)
- **Mutation check on the ARM, which round 1 had no test for at all:** revert the arming condition
  to `self.on_ground || swimming` while leaving the probe in place. This must go RED — not on the
  property above (a blocked body satisfies it) but on §6.3's buoyant-curb control. Two of the three
  candidate patches measured in §2 are indistinguishable without it, and that indistinguishability
  is exactly what shipped a wrong recommendation in round 1.

### 6.2 Example test — the repro

A new builder in `tests/synthetic_scenes/mod.rs`,
`flooded_pocket_with_a_rim_under_the_waterline(rim_top: f32)`. **Geometry** (all of it — round 1
gave this and stopped, which is why its numbers were not re-derivable, reviewer finding 6):

- floor at −70, east [−40, 0] × north [−20, 20];
- the **lid**, a quad at −55.969, east [−40, 0] × north [−20, 20];
- the **dry floor beyond**, a quad at −55.969, east [0, 100] × north [−20, 20];
- west wall and both north/south walls from −70 to −30 (full height);
- the **rim**: an east-facing wall at east 0, north [−20, 20], from −70 up to `rim_top`;
- a far wall at east 100 from −55.969 to −30, so the run is bounded;
- water box north [−20, 20] × east [−40, 0], z −69.5 to surface **−55.978**.

**Start state** (the part round 1 omitted, and it is load-bearing — §1): the body is created at
**east −5.0, north 0.0, z = surface − SKIN = −56.028**, i.e. already at the swim-surface clamp.
Drive `wish_dir = [1, 0]`, `wish_vspeed = +10`, `want_swim = true`, `speed = 35`, `dt = 1/60`.
Starting instead at the buoyancy plane (−57.978) measures a *legitimate haul-out*, not this defect.

Two instantiations: `rim_top = −55.969` (in the band, 0.059 above the feet) and the paired **reach
control** `rim_top = −30.0` (full height, everything else identical).

Assertions: the body's east coordinate never advances across east 0 while its z is below `rim_top`;
and the frame that does advance it does so with z already lifted to the slab height — it **mounted**
the rim.

**The discriminator is the crossing frame, not the final state.** MEASURED: on this fixture the
final position after 900 frames is **identical** on `main` and under variants 2, 3 and 4 (east
98.6, z −55.969, grounded, dry). Only frame 8 separates them — `main` post-z −56.028 (through),
every fix post-z −55.969 (over). A test that asserts an end position here would pass on `main` and
prove nothing. This is the single most important sentence in §6 for whoever implements it.

- **Mutation, direction 1:** revert the probe → RED, with the crossing frame's post-z back at
  −56.028 (the trace in §1).
- **Mutation, direction 2:** the full-height reach control → the test must still pass **and** the
  body must never cross at all (measured: stops at east −0.167, wet, un-grounded, after 900 frames).
  This is what distinguishes "mounted the rim" from "the scene has no exit", and it is the check the
  retracted "sealed on all six sides" diagnosis existed for.

### 6.3 Negative controls that must not move

- **The step ladder:** 0.30 / 0.45 / 1.00 / 1.90 / 2.40 crossed; 3.00 / 6.00 refused. Pin it —
  variant 1 moved the 2.40 boundary and nothing in the existing suite noticed.
- **Lip-with-drop, grounded:** 0.30 and 0.45 lips with the floor beyond at −2 and −10 still crossed.
  Pin it — variant 2 broke exactly this and the whole suite stayed green.
- **Lip-with-curb, buoyant** (new in round 2, and the control that separates E′ from E): a body with
  `want_swim = false` floating at `surface − float_depth`, driven into a 0.30 / 0.45 / 0.60 lip with
  a walkable floor at the same height beyond, must still cross. Pin it — E blocks all three and the
  entire existing suite stays green (85/0/1, 11/0/5, 6, 4, 5).
- **Named existing tests that must stay green**, because they are the ones that actually caught
  variant 1: `movement::tests::steps_up_a_2u_ledge`,
  `movement::tests::a_swimmer_at_a_solid_bank_still_hauls_out_the_duck_does_not_override_191`,
  `movement::tests::a_swimmer_hauling_out_at_a_legitimate_bank_never_raises_the_afloat_stall`, and
  **`walker_sim::p1_haul_out_admission_matches_controller_execution`** — the last of which *does*
  run in CI (see §7) and is the only guard here that is both CI-run and sensitive to the haul-out
  contract.
- **The beyond-floor sweep (new in round 3, and the control the whole option space now turns on).**
  Same lip height (0.45, inside the band), *sweeping the height of the floor beyond it* across at
  least `{+0.45, 0.00, −0.25, −0.40, −0.50, −1.00, −2.40, −10.0}`, driven by **both** a buoyant body
  and a **levitating** one. Whatever option ships, this sweep is where its residual becomes visible,
  and its result must be written into the test as an explicit table rather than a `crosses()`
  assertion — E′ is *expected* to stop below −0.40, so the test's job is to pin *where*, not to
  demand success. If a later change moves that boundary without moving the table, that is the
  regression this control exists to catch.
- **Two-face admission control** (only if F2 ships): two band-height faces 0.30 apart. Measured under
  F2, one admitted step crosses both. Pin the count of faces a single step may cross at **one**.

**Existing fixtures cannot decide any of this — measured, and this is the reason the new ones are
required.** `cargo test --lib movement::` (85/0/1), `--test walker_sim` (11/0/5) and the three
synthetic binaries (6, 4, 5) are **green on `main` and green under E′, F1 and F2 alike**. Four
behaviourally different builds, one of which has a measured silent wedge on ordinary geometry, are
indistinguishable to the entire shipped suite. The sweeps above are ~30 lines each on top of a single
new `tests/synthetic_scenes/mod.rs` builder — `lip_with_floor_beyond(lip_top, beyond_z, water: Option<f32>)`
— which is all the infrastructure any of the round-3 measurements needed.

**The falsification control — and why it cannot be built.** #854's control is that the same
horizontal drive with *zero* pitch, from the buoyancy plane, stalls wet for ever; the escape
requires the up-pitch clamp. That control is what would make the pitch coupling
(`src/app.rs:1648`, `wish_vspeed = dz · MOVE_SPEED`) the trigger rather than a coincidence.

Round 1 instructed the implementer to build a lid-less variant and pin it there. **That instruction
was impossible, and the reviewer's arithmetic is correct and is reproduced here.** The two
requirements are mutually exclusive:

- being inside the blind band requires `rim_top < feet_at_clamp + Body::foot` = `surface − SKIN +
  0.5` = **`surface + 0.45`**;
- stalling a zero-pitch swimmer requires the rim to be out of reach from the buoyancy plane, i.e.
  `rim_top > surface − float_depth + STEP_UP + GROUND_SNAP_TOL` = `surface − 2.0 + 2.5` =
  **`surface + 0.50`**.

`0.45 < 0.50`, so no rim height is simultaneously in the band and out of haul-out reach. MEASURED
corroboration, round 2, on the §6.2 fixture: the zero-pitch drive crosses at frame 8 and ends dry
and grounded at the slab, **identically on `main` and under every variant** — the swimmer simply
hauls out over a rim only 2.009 above its buoyancy plane. The reviewer independently built the
lid-less variant and measured the same haul-out at the same frame.

**What that costs, stated plainly.** No synthetic fixture in this family can separate "the up-pitch
clamp parks the feet in the band" from "this rim is reachable anyway". So:

- #854's **pass-through is reproduced** synthetically (§1's frame-8 trace, and the paired
  full-height reach control which does hold and does discriminate);
- #854's **pitch-specificity is not** reproduced synthetically, and this design does not claim it.
  The pitch coupling remains a reading of `src/app.rs:1648` plus the field report in the issue.
- Do **not** write a zero-pitch assertion into the suite. A control that cannot fail is worse than
  an absent one — it reads as coverage. If someone wants the pitch-specificity pinned, it needs a
  fixture family with a different vertical scale (rim reachable only from the clamp, unreachable
  from the buoyancy plane, which requires `float_depth`, `STEP_UP` or `GROUND_SNAP_TOL` to be
  injectable), and that is a separate piece of test infrastructure, not a line in this fix.

### 6.4 What no test here claims

No live run discharges "cannot pass through". A live pass in the shipped zone proves the *premise*
(the geometry is as described); it cannot prove the universal, because a race that usually wins is
indistinguishable from one that cannot lose. `[[eq-verification-hierarchy]]`.

---

## 7. Bound the blast radius

`slide()` runs on every movement frame carrying a horizontal wish. Under E′:

- **Grounded bodies: nothing changes.** The probe is gated off and the arm condition is unchanged
  for them. MEASURED: step ladder, grounded lip-with-drop ladder, and ballistic runs all identical
  to baseline; `cargo test --lib movement::` 85 passed / 0 failed / 1 ignored; `--test walker_sim`
  11 / 0 / 5 ignored; 6, 4, 5 on the three synthetic integration binaries.
- **Swimming / buoyant / ballistic / levitating bodies with a horizontal wish** (all four ship today
  — the "(future)" that stood here for levitate was wrong, see §2) gain a
  contact at the feet plane against non-standable faces, **and** gain the step-up/duck recovery that
  today only grounded and swimming bodies get. **If the recommendation is wrong, this is where it
  shows:** a swimmer that used to slide along a submerged lip now stops at it or climbs it; a
  floating body that used to drift through a submerged kerb now mounts it.

- **RETRACTED — round 1 said "the failure mode is extra work, not a new stall".** That was
  **REASONED** from the arming branch and it was **false**, and the branch it cited says the
  opposite: `if (self.on_ground || swimming) && low_hit && …` (`src/movement.rs:790`) does not arm
  for a body that is ungrounded and not swimming, which is exactly the population the gated probe
  newly blocks. MEASURED (round 2): under variant 3 a buoyant, non-swimming body stops dead at a
  0.30 lip that `main` crosses, and is still there at frame 3600. The recommendation was amended to
  E′ (§2 variant 4, §3) precisely because of this.
  **Round 3 narrows the repair, too:** "under E′ the stall is gone wherever a landing exists" was
  also false — it is gone only where the landing is within `GROUND_SNAP_TOL` of the feet, and
  measured wedges remain below that for buoyant *and* levitating bodies, silently. Both errors are
  reported rather than quietly deleted because the *reasoning error* is the reusable lesson twice
  over: a gate and its recovery are one decision; and a two-point sample (`0` and `−10`) is not a
  measurement of a continuum. The second is the more embarrassing one — the boundary was inside the
  interval the table skipped.
- **Cost:** +1 ray cast per low-slide iteration, ≤ `MAX_SLIDE_ITERS` = 3 per frame, only while not
  grounded, zero when grounded; plus at most one extra `try_step_up` on frames where that new
  contact fires. Arithmetic from the loop bound, **not** a profile.
- **CORRECTED — the CI claim.** Round 1 said `walker_sim`'s
  `p1_haul_out_admission_matches_controller_execution` is `#[ignore]`d and asset-gated and would not
  run in CI, and told the implementation PR to repeat that. **It is neither.** It is a bare
  `#[test]` at `tests/walker_sim.rs:116-117` that builds its own quads at `:120-135`; the binary
  reports 11 passed / 5 ignored and P1 is among the 11. It therefore **is** coverage: it is the one
  named guard in this design that runs in CI *and* is sensitive to the haul-out contract, and it
  goes RED under variant 1 (§2). Nothing else in §7 rested on the false claim — the grounded and
  cost bullets are independent of it — but §6.3's must-stay-green list now names P1.

**Out of scope, recorded so it is not mistaken for covered:** a hanging slab that touches no
ground — say a lintel spanning `feet + 1.0` to `feet + 3.0` — is invisible to *both* shipped probes
and remains invisible under E′. It is the same defect class, and option B is what fixes it.
**REASONED** from the probe heights; not measured.

**Found while building the round-2 controls, NOT part of this design, and now filed as #870.** On a
grounded walker driven into a lip with the floor beyond dropped to −10, the crossing ladder is **not
monotone**: lips of 2.40 / 2.60 / 2.80 stop the body dead, while 3.00 / 3.50 / 4.00 / 5.00 all end at
east 99.4, z −10 — beyond a wall the body never went over. MEASURED, and **identical on `main` and
under every variant here**, so it is pre-existing and not a consequence of anything proposed. The
independent reviewer reproduced it, found it wider (the curb shape too, and out to a 6.0 wall), and
measured the frame trace round 2 left unmeasured: ~70 frames of ±0.6 u oscillation at the face, then
**one frame** carrying the body from east −0.7068 / z 0 to east +0.0003 / z −10.

**My round-2 "coplanar floor edge" suspicion is withdrawn — the reviewer's control refutes it**
(insetting the floor edge so the junction is no longer coplanar still crosses). Its replacement
hypothesis, that **zero thickness** is the cause, is **not supported by my own re-run** and I am
recording the disagreement rather than adopting the tidier claim. **MEASURED (round 3)**, grounded
walker, 3.0-tall barrier, floor beyond at −10, varying only the barrier's thickness (back face + cap
added when thick), final east:

| thickness | 0.0 | 0.5 | 1.0 | 2.0 | 3.0 | 5.0 |
|---|---|---|---|---|---|---|
| result | **99.40 (crosses)** | −0.60 stop | −0.60 stop | **98.98 (crosses)** | **98.98 (crosses)** | −0.60 stop |

The 1.0-thick row agrees with the reviewer; 2.0 and 3.0 do not. So thickness is **not monotone** and
"zero thickness is the cause" over-fits three samples. What both runs do establish is that the
crossing is sensitive to the barrier's cross-section — some thicknesses stop it and some do not with
nothing else changed — which is a useful discriminator for #870 and a mechanism claim for **neither**
of us to make. The depenetration-net attribution stays **UNMEASURED** in this document.
Everything above belongs in #870; nothing in #854's fix depends on it.

---

## 8. Agent-honesty statement

**Removed:** the client no longer reports a position reached by translating through a wall's
interior as an ordinary position. MEASURED: the crossing becomes a mount (`low_hit` false → true,
`low_prog` 0.5833 → 0.0000, `stepped` false → true).

**NOT removed, residual 1 — the grounded free step.** A **grounded** body still crosses any face
whose top is within `Body::foot` = 0.5 of its feet, and reports the result as ordinary. The design's
claim is that this is not a falsehood, because the ground clamp resolves the body onto the lip in
the same frame, so the reported end-of-frame position is one a step could have reached. That claim
is **REASONED** from the clamp's ordering, supported only by the measured equivalence of the two
ladders — it is not a proof. The residual it leaves is concrete: a grounded body crossing a 0.4 lip
whose far side has *no* floor within the clamp's reach ends up airborne past a wall it never
contacted. The lip-with-drop fixture is exactly that shape, and it is crossed both on `main` and
under E′. Whether "step over a curb at a cliff edge and fall" is legal player behaviour or a second
instance of #854 is an **owner call**, and this design does not make it.

**NOT removed, residual 2 — RESTATED IN ROUND 3, and it is bigger and worse than round 2 said.**

Round 2 wrote this residual as "the ungrounded **no-landing** lip", i.e. bottomless drops. That was
false, and the correction is the main content of round 3. **MEASURED (§2):**

> Under E′, a body whose vertical is not floor-owned — **buoyant or levitating**, both shipping
> today — driven at any face in the blind band is stopped **permanently** whenever the floor beyond
> that face is more than `GROUND_SNAP_TOL` = **0.5 u below its feet**. `main` crosses every one of
> those. The boundary is `try_step_up`'s landing gate: the recovery E′ arms is a step *up* with no
> downward half.

That is a submerged 0.3 kerb with the bed a unit lower; a rock on a gently falling seabed; a
levitator gliding at a small obstruction with a dip beyond. Ordinary geometry, not a corner case.

**And it is silent — this is the part that matters most.** MEASURED: `hold()` returns `None` at every
wedged position at frame 3600. The client tells the driving agent it has an ordinary stationary body
in an ordinary place. A driver polling `player.hold` sees nothing to react to, will re-issue the same
wish for ever, and has no observable that distinguishes "arrived and idle" from "cannot move under
any driver". Under `[[eq-agent-honesty-invariant]]` that is a *silent wrong answer*, which this
project ranks above both features and crashes — the same class as #854 itself, arrived at from the
other side. Levitate makes it a true predicament rather than an inconvenience: `:1004` discards
vertical input, so the player cannot rise over the obstruction manually.

**What is genuinely true, and all that is:** where E′ *does* cross, the crossing is a real mount with
a written z, and a body reporting "stopped at east −0.167" while pressed against a real face is
telling the truth about its position. E′ trades a silent falsehood about *position* for a silent
falsehood about *mobility*. Round 2's framing ("uniform and truthful") claimed the first half and
skipped the second. **Shipping E′ without the hold reason proposed in §3 makes that trade worse, not
better**, and this design now says so where a reader will see it rather than leaving it to §3.

**Do not read this as closing the qcat escape.** After E′, leaving the pocket is a `try_step_up`
whose legality is decided by the haul-out contract (`haul_out_up` = 2.0 from the surface), and it
is reported truthfully — MEASURED, the body ends `on_ground` on the slab having mounted the rim.
The **pass-through** is closed; the **exit** is not. Anyone reading #854 as "the character can no
longer get out" will be wrong.

**Who owns that exit — corrected, with the issue states checked.** Round 1 handed it to
"#329 / #661 / #543". Verified on GitHub on 2026-08-05: **#661 CLOSED (completed, by PR #767)**,
**#543 CLOSED (completed)**, **#266 CLOSED (completed)**, **#649 CLOSED (completed)**; **only #329
is OPEN**. Deferring a live residual to closed issues is not a scoping decision, it is a disposal.
Worse, the substantive half: #661's shipped remedy is `try_duck_under`, and it is gated
`if swimming && intent.wish_vspeed <= 0.0` (`src/movement.rs:817`). #854's trigger is
`wish_vspeed = +10`. The duck **structurally cannot fire on the input that produces #854**, so
#661's fix does not reach this case even in principle.

So, corrected:

- **#329** — "walker strands on an unbounded swim_surface/haul_out rise" — is open and is the
  closest existing owner. It owns the *strand*: after E′ the body still ends dry and grounded on
  the slab at the historical wedge coordinate. Assigning the residual there is defensible **for the
  strand**.
- **It does not obviously own the mount itself** — "should a swimmer at a clamped surface be
  allowed to `try_step_up` a rim it can reach" is a capability question about the haul-out contract,
  not about an unbounded rise. If the answer is "no", that is a **new issue** and this design says
  so rather than shopping it to an issue that will not recognise it.

Recommendation: file one new issue for the mount-vs-strand capability question, cross-reference
#329, and do **not** re-open or re-target #661/#543/#266.

**What the implementation issue must carry out of round 3** (in decision order, because the first
answer changes the rest):

1. **The owner picks among E′ / F2** on §2's three-row tradeoff table. This design ranks E′ first on
   the honesty ordering and says why, and says equally plainly that F2 wins every mobility
   measurement taken here. Neither is a default.
2. **If E′: the hold reason is not optional.** A `FreeStep::None` body blocked at the feet plane with
   no recovery available must raise a `ControllerHold`, or E′ ships a measured silent wedge on
   ordinary geometry and the fix trades one `agent-honesty` defect for another.
3. **If F2: file the multi-face admission question** — one admitted step was measured crossing two
   band-height faces — and decide whether a step-over is limited to one face or re-swept at the feet
   plane on arrival.
4. **F1 needs no issue.** It is measured out (§3).

**Two prior diagnoses of #854 were retracted publicly** ("the camera has no collision test"; "the
chamber is sealed on all six sides"). Neither is restated here, and §6.2's paired reach control
exists specifically so the second cannot recur in this fix's own tests.
