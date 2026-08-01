//! Device-free coverage for the shadow-map **caster selection** block (eqoxide#740).
//!
//! ## What this grades
//!
//! `encode_shadow_pass` decides, per frame, which entities cast into the shadow map. Four
//! decisions, all previously untested:
//!
//! 1. **nearest-first order** — nearby characters are sorted by distance to the *light center*
//!    (not the player);
//! 2. **the `entity_in_view` cull** — anything outside the entity distance/frustum test is dropped
//!    and consumes no slot;
//! 3. **the `SHADOW_CASTER_SLOTS` bound** — at most 64 uniform slots per frame, the player first;
//! 4. **the `clip_idx < clips.len()` guard** (#692/#694) — an out-of-range or sentinel
//!    (`usize::MAX`) clip index must fall back to bind pose instead of indexing out of range.
//!
//! ## Why there is a planner at all
//!
//! #740 assumed this block was device-free-testable *as it stood*. It was not: it lived inline in
//! `encode_shadow_pass(r: &EqRenderer, encoder: &mut wgpu::CommandEncoder, …)`, and neither
//! `wgpu::Device` nor `wgpu::Queue` implements `Default` or has any constructor that does not go
//! through an `Adapter` (wgpu 22 has no `noop` backend), so an integration test cannot build the
//! arguments at all — `EqRenderer::new(Default::default(), …)` is six `E0277`s. So #740 pulls the
//! four decisions into `pass::plan_shadow_casters`, a pure function over the
//! `pass::ShadowCasterCandidate` trait, the same shape #721 used for the instanced draws. Every
//! test below calls that **production** function; `encode_shadow_pass` calls exactly the same one.
//!
//! ## The `ShadowModelKind` × path matrix
//!
//! The planner writes its slot bookkeeping out **once per path** — a player block and a nearby
//! block — so either copy can rot alone, exactly as the pre-#740 pass wrote the #692/#694 clip
//! guard out twice. Coverage is therefore a matrix, not a list, and the corpus asserts a floor on
//! every cell against a reach report the **planner** produces from inside its own loop
//! (`pass::ShadowCasterReach`):
//!
//! | | player path | nearby path |
//! |---|---|---|
//! | *(no candidate)* | `plan()` helper, corpus | trivial (empty slice) |
//! | `Absent` | `a_player_with_no_model_casts_nothing_and_consumes_no_slot`, corpus | `static_casters_take_a_uniform_slot_but_no_joint_slot`, corpus |
//! | `Static` | corpus | `static_casters_…`, `joint_slots_never_outrun_uniform_slots`, both bound tests, corpus |
//! | `Skinned` | `the_player_takes_the_first_slot_…`, `the_player_is_never_view_culled`, `clip_index_out_of_range_…`, corpus | most of the file |
//!
//! `player` × `Absent` was the cell that was empty on the first draft, and it is a live production
//! branch (a player whose race model has not loaded).
//!
//! **Corrected (#747 round 3):** an earlier draft of this table said every cell was "reached" while
//! the corpus assertions behind three of the cells were computed from `cands` — the *generator* —
//! and not from the returned plan. That is a claim about what the fixture built, and it cannot move
//! when the *planner* stops reaching a variant, which is the only failure the assertions exist to
//! catch. The two quantities differed measurably: 373/392/397 scenes generated a
//! nearby `Absent`/`Static`/`Skinned`, but only 334/376/392 had the planner reach one. Struck
//! claims are preserved rather than deleted.
//!
//! **Corrected again (#747 round 4):** round 3's replacement — an `examined_nearby` helper that
//! *reconstructed* the examined set from the plan — was wrong the same way one level down, and its
//! own doc claimed otherwise ("if it ever drifts from the planner, that assertion fires instead of
//! the counts quietly going wrong"). It was a second copy of the planner's termination rule, and
//! its self-check was a *subsequence* test: a prefix is always a subsequence of the full order, so
//! the check constrained the ordering and placed **no constraint on the cut** — the branch it took
//! in 320 of 400 scenes. The round-3 review falsified the doc by measurement: capping the nearby
//! loop at 100 examined candidates lost real reach in 85 of 400 scenes, over-claimed 1317
//! candidate-examinations, and moved not one counter (18 passed, 0 failed). It also showed the
//! nearby `Absent` cell could not observe its own arm going dead — 334 with the arm unreachable.
//! Both are now fixed by construction rather than by a tighter guard: `plan_shadow_casters` reports
//! its own reach, counted inside the loop and inside each arm, so there is one source for the cut,
//! `examined_nearby` is deleted, and an `Absent` arm that stops running reports zero. Two scene-level
//! asserts keep the report honest — the arm identity, and "the planner must not stop short of the
//! order with slots to spare", which is what the 100-candidate cap now fails.
//!
//! ## What this file does NOT cover
//!
//! Stated as rules, not examples — the gaps are categories, not single cases:
//!
//! - **Everything downstream of selection.** The plan→GPU half of `encode_shadow_pass` (every
//!   `entity_model_matrix_heading` / `humanoid_placement` / `archetype_*` call, both `write_model`
//!   and `write_joints`, and `skin.evaluate` / `skin.bind_pose` themselves) needs a device and is
//!   untested here. This file grades *which* casters are selected, in *what* order, into *which*
//!   pool slots, at *which* pose — never the matrices they turn into.
//! - **That `encode_shadow_pass` calls the planner at all.** Only
//!   `encode_shadow_pass_calls_the_planner_this_file_grades` covers that, and it is a *source-text*
//!   assert, not a semantic one (same technique and same caveat as the `PIPELINE_RS` asserts in
//!   `fog_shader.rs` and `shadow_routing.rs`). In particular it would not notice a *second*,
//!   re-inlined selection loop added next to the call.
//! - **`entity_in_view`'s own internals.** This file pins that selection calls the cull with the
//!   *player* position and the frame's view-projection, and that a culled candidate consumes no
//!   slot. It does not re-derive the cull's near/far/NDC rules; the `w <= 0` (behind-camera) branch
//!   in particular is never exercised, because the test matrices are all `w = 1`.
//! - **The instanced placed-object casters.** Different sub-system, covered by
//!   `shadow_routing.rs` / `shadow_routing_equivalence.rs`.

use eqoxide_renderer::camera::entity_in_view;
use eqoxide_renderer::pass::{
    plan_shadow_casters, shadow_pose_for, ShadowCasterDraw, ShadowCasterRef, ShadowCasterStep,
    ShadowModelKind, ShadowPose, ENTITY_CULL_MARGIN, ENTITY_DRAW_DIST,
};
use eqoxide_renderer::renderer::SHADOW_CASTER_SLOTS;

// ── Fixtures ────────────────────────────────────────────────────────────────────────────────────

/// A device-free stand-in for "a `Billboard` plus the `GpuModel` the renderer resolved for it".
#[derive(Debug, Clone, PartialEq)]
struct Cand {
    pos:  [f32; 3],
    kind: ShadowModelKind,
    anim: Option<(usize, f32)>,
}

impl Cand {
    /// A skinned caster with `clips` clips and no animation state → bind pose.
    fn skinned(pos: [f32; 3], clips: usize) -> Self {
        Self { pos, kind: ShadowModelKind::Skinned { clip_count: clips }, anim: None }
    }
    /// A skinned caster animating at `idx`/`time`.
    fn playing(pos: [f32; 3], clips: usize, idx: usize, time: f32) -> Self {
        Self { pos, kind: ShadowModelKind::Skinned { clip_count: clips }, anim: Some((idx, time)) }
    }
    fn statik(pos: [f32; 3]) -> Self {
        Self { pos, kind: ShadowModelKind::Static, anim: None }
    }
    /// A spawn whose race model is not loaded — casts nothing.
    fn absent(pos: [f32; 3]) -> Self {
        Self { pos, kind: ShadowModelKind::Absent, anim: None }
    }
}

impl eqoxide_renderer::pass::ShadowCasterCandidate for Cand {
    fn pos(&self) -> [f32; 3] { self.pos }
    fn model_kind(&self) -> ShadowModelKind { self.kind }
    fn anim_state(&self) -> Option<(usize, f32)> { self.anim }
}

/// Projection scale: `VP` maps world → NDC by dividing by this, with `w = 1`. Chosen so that for
/// any position within `ENTITY_DRAW_DIST` of the origin the frustum test always passes, making
/// "in view" equivalent to "within draw distance" for the origin-player fixtures. The two are
/// separated deliberately in `frustum_culled_candidates_are_dropped_even_when_close`.
const VP_SCALE: f32 = 1000.0;
const VP: [[f32; 4]; 4] = [
    [1.0 / VP_SCALE, 0.0, 0.0, 0.0],
    [0.0, 1.0 / VP_SCALE, 0.0, 0.0],
    [0.0, 0.0, 1.0 / VP_SCALE, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];
const ORIGIN: [f32; 3] = [0.0, 0.0, 0.0];

/// Precondition predicate — the REAL cull, so a fixture claim ("this one is in view") can never
/// disagree with what the planner sees. Preconditions asserted through this survive any mutation
/// of the planner, which is the point: a broken planner must report as a broken planner, not as an
/// invalid fixture.
fn in_view(pos: [f32; 3], player_pos: [f32; 3]) -> bool {
    entity_in_view(pos, player_pos, VP, ENTITY_DRAW_DIST, ENTITY_CULL_MARGIN)
}

fn plan(nearby: &[Cand], light_center: [f32; 3]) -> Vec<ShadowCasterStep> {
    plan_shadow_casters(None::<&Cand>, nearby, light_center, ORIGIN, VP).steps
}

/// The `Nearby` indices a plan selected, in plan order.
fn picked(steps: &[ShadowCasterStep]) -> Vec<usize> {
    steps.iter().filter_map(|s| match s.caster {
        ShadowCasterRef::Nearby(i) => Some(i),
        ShadowCasterRef::Player => None,
    }).collect()
}

fn u_slots(steps: &[ShadowCasterStep]) -> Vec<usize> {
    steps.iter().map(|s| s.u_slot).collect()
}

fn pose_of(step: &ShadowCasterStep) -> ShadowPose {
    match step.draw {
        ShadowCasterDraw::Skinned { pose, .. } => pose,
        ShadowCasterDraw::Static => panic!("expected a skinned step, got a static one"),
    }
}

// ── 1. Nearest-first ordering ───────────────────────────────────────────────────────────────────

/// The sort key is the distance to the **light center**, not to the player. The fixture is built so
/// the two orders are exact reverses of each other, and the input order is a third arrangement —
/// so "sorted by the player", "not sorted at all", and "sorted farthest-first" each produce a
/// different sequence from the expected one, and no single ordering satisfies all three.
#[test]
fn nearby_casters_are_ordered_nearest_first_to_the_light_center() {
    let light = [400.0, 0.0, 0.0];
    let cands = [
        Cand::skinned([0.0, 0.0, 0.0], 3),   // 400 from light, 0 from player
        Cand::skinned([200.0, 0.0, 0.0], 3), // 200 from light, 200 from player
        Cand::skinned([400.0, 0.0, 0.0], 3), // 0 from light, 400 from player
    ];
    for c in &cands {
        assert!(in_view(c.pos, ORIGIN), "fixture must not be culled: {:?}", c.pos);
    }

    let steps = plan(&cands, light);
    assert_eq!(
        picked(&steps), vec![2, 1, 0],
        "casters must be sorted nearest-first by distance to the LIGHT center; sorting by the \
         player position (or not sorting) would give [0, 1, 2], farthest-first would give [0, 1, 2] \
         too since the two metrics are reverses here",
    );
    assert_eq!(u_slots(&steps), vec![0, 1, 2], "slots are handed out in plan order from 0");
}

/// Equidistant casters keep their input order.
///
/// **Measured caveat, not a claim:** this test does *not* discriminate `sort_by` from
/// `sort_unstable_by` — that mutation was run and this test stayed green at every fixture shape
/// tried (all-tied at 8, 40; tied in groups of 4 at 48). What kills it is the corpus differential
/// below — which for that reason now generates exact ties **by construction** and asserts their
/// reach, rather than relying on a tie happening to fall out of the RNG (it stopped falling out
/// once, when an unrelated fixture edit shifted the stream). So read this as pinning "ties are not
/// reordered by the current sort", not as the stability guard.
#[test]
fn equidistant_casters_keep_input_order() {
    // Ties in GROUPS, interleaved with distinct keys — an all-tied fixture does not discriminate
    // (`sort_unstable_by` happens to preserve order when every key is equal, at every size tried).
    let cands: Vec<Cand> =
        (0..48).map(|i| Cand::skinned([(i / 4) as f32 * 10.0, 0.0, 0.0], 2)).collect();
    let steps = plan(&cands, ORIGIN);
    assert_eq!(picked(&steps), (0..48).collect::<Vec<_>>());
}

// ── 2. The view cull ────────────────────────────────────────────────────────────────────────────

/// A candidate past `ENTITY_DRAW_DIST` is dropped *and* consumes no slot — the nearer casters keep
/// slots 0..n with no hole. A mutant that keeps culled casters fails on the count; a mutant that
/// bumps the slot counter before culling fails on the slot sequence.
#[test]
fn out_of_view_candidates_are_dropped_and_consume_no_slot() {
    let near = Cand::skinned([100.0, 0.0, 0.0], 4);
    let far = Cand::skinned([900.0, 0.0, 0.0], 4); // > ENTITY_DRAW_DIST from the player
    let near2 = Cand::skinned([200.0, 0.0, 0.0], 4);
    assert!(in_view(near.pos, ORIGIN) && in_view(near2.pos, ORIGIN), "fixture: both near are visible");
    assert!(!in_view(far.pos, ORIGIN), "fixture: the far one must be culled");

    let steps = plan(&[near, far, near2], ORIGIN);
    assert_eq!(picked(&steps), vec![0, 2], "the culled candidate must not be selected");
    assert_eq!(u_slots(&steps), vec![0, 1], "and must not burn a uniform slot either");
}

/// The DISTANCE half of the cull is measured from the **player**, not from the light center. The
/// candidate sits exactly on the light center (distance 0 from it) and 1000 units from the player
/// — so a mutant that passes `light_center` where the cull wants `player_pos` keeps a caster this
/// asserts is dropped. The frustum half is insensitive to that swap (it uses only the
/// view-projection), which is precisely why this case is separate from the one below.
#[test]
fn the_distance_cull_is_measured_from_the_player_not_the_light() {
    let light = [1000.0, 0.0, 0.0];
    let c = Cand::skinned(light, 3);
    assert!(!in_view(c.pos, ORIGIN), "fixture: 1000 units from the player is past the draw distance");
    assert!(in_view(c.pos, light), "fixture: it WOULD pass if culled against the light center");

    let steps = plan_shadow_casters(None::<&Cand>, std::slice::from_ref(&c), light, ORIGIN, VP).steps;
    assert!(steps.is_empty(), "a candidate past the draw distance from the player casts nothing");
}

/// The FRUSTUM half culls independently of distance: both candidates here are well within
/// `ENTITY_DRAW_DIST` of the player, but one projects to NDC x = 1.6, outside `1.0 +
/// ENTITY_CULL_MARGIN`. A mutant that drops the frustum test (keeping only the distance test)
/// keeps it.
#[test]
fn frustum_culled_candidates_are_dropped_even_when_within_draw_distance() {
    let player = [1400.0, 0.0, 0.0];
    let on_screen = [1490.0, 0.0, 0.0];  // NDC x = 1.49 ≤ 1.5
    let off_screen = [1600.0, 0.0, 0.0]; // NDC x = 1.60 > 1.5, 200 units from the player
    assert!(in_view(on_screen, player), "fixture: this one must survive the cull");
    assert!(!in_view(off_screen, player), "fixture: this one must be frustum-culled");

    let cands = [Cand::skinned(off_screen, 3), Cand::skinned(on_screen, 3)];
    let steps = plan_shadow_casters(None::<&Cand>, &cands, player, player, VP).steps;
    assert_eq!(picked(&steps), vec![1], "only the on-screen candidate casts");
    assert_eq!(u_slots(&steps), vec![0], "and it takes slot 0, not slot 1");
}

// ── 3. The SHADOW_CASTER_SLOTS bound ────────────────────────────────────────────────────────────

/// Pins the bound's VALUE, so a retune is a deliberate edit here rather than something the
/// symbolic tests below would silently follow. If you intend to change the budget, change this
/// line in the same commit.
#[test]
fn shadow_caster_slot_budget_is_sixty_four() {
    assert_eq!(SHADOW_CASTER_SLOTS, 64);
}

/// Exercised **at** the bound, one under, and one over. Under → everything selected; at → still
/// everything; over → truncated to exactly the budget, and the *nearest* ones survive.
///
/// **The fixture's model-kind MIX is load-bearing, and the boundary rows are not what makes this
/// test work.** For an all-skinned population the documented-dead `j_slot >= SHADOW_CASTER_SLOTS`
/// guard wakes up and re-imposes the *exact same* 64-cap, so a skinned-only fixture cannot grade
/// the uniform bound at all: it would run at `-1` / exactly / `+1` / `*3`, pass every row, and
/// still stay green when the `u_slot` bound is **deleted outright**. That was measured, not
/// reasoned. Every fourth candidate is therefore static — statics spend a uniform slot and no joint
/// slot, so `u_slot` outruns `j_slot` and nothing but the real bound can produce these counts.
#[test]
fn selection_is_bounded_at_exactly_shadow_caster_slots() {
    for (n, expect) in [
        (SHADOW_CASTER_SLOTS - 1, SHADOW_CASTER_SLOTS - 1),
        (SHADOW_CASTER_SLOTS,     SHADOW_CASTER_SLOTS),
        (SHADOW_CASTER_SLOTS + 1, SHADOW_CASTER_SLOTS),
        (SHADOW_CASTER_SLOTS * 3, SHADOW_CASTER_SLOTS),
    ] {
        // Candidate i sits at x = i * 0.5, so index order == nearest-first order. Mixed kinds — see
        // the doc comment: a skinned-only population is graded by the dead joint guard, not by the
        // uniform bound this test is named for.
        let cands: Vec<Cand> = (0..n).map(|i| if i % 4 == 3 {
            Cand::statik([i as f32 * 0.5, 0.0, 0.0])
        } else {
            Cand::skinned([i as f32 * 0.5, 0.0, 0.0], 3)
        }).collect();
        for c in &cands {
            assert!(in_view(c.pos, ORIGIN), "fixture: all {} candidates are in view", n);
        }
        let steps = plan(&cands, ORIGIN);
        assert_eq!(steps.len(), expect, "{} candidates must yield {} casters", n, expect);
        assert_eq!(
            u_slots(&steps), (0..expect).collect::<Vec<_>>(),
            "uniform slots must be 0..{} with no gap and no repeat — slot 0 included", expect,
        );
        assert_eq!(
            picked(&steps), (0..expect).collect::<Vec<_>>(),
            "when over budget it is the FARTHEST casters that are dropped",
        );
    }
}

/// The player is selected first and eats slot 0, which shrinks the nearby budget by one. A mutant
/// that plans the player outside the budget (or after the loop) fails on the total.
///
/// Mixed static/skinned for the same reason as
/// `selection_is_bounded_at_exactly_shadow_caster_slots` — with an all-skinned population the dead
/// `j_slot` guard substitutes for the `u_slot` bound and this test stays green when the bound is
/// deleted.
#[test]
fn the_player_takes_the_first_slot_and_shrinks_the_nearby_budget() {
    let player = Cand::playing([0.0, 0.0, 0.0], 5, 2, 0.25);
    let cands: Vec<Cand> = (0..SHADOW_CASTER_SLOTS * 2).map(|i| if i % 4 == 3 {
        Cand::statik([i as f32 * 0.5, 0.0, 0.0])
    } else {
        Cand::skinned([i as f32 * 0.5, 0.0, 0.0], 3)
    }).collect();

    let steps = plan_shadow_casters(Some(&player), &cands, ORIGIN, ORIGIN, VP).steps;
    assert_eq!(steps.len(), SHADOW_CASTER_SLOTS, "the budget covers the player too");
    assert_eq!(steps[0].caster, ShadowCasterRef::Player, "the player is planned first");
    assert_eq!(steps[0].u_slot, 0);
    assert_eq!(
        picked(&steps).len(), SHADOW_CASTER_SLOTS - 1,
        "one fewer nearby caster fits once the player has taken a slot",
    );
    assert_eq!(u_slots(&steps), (0..SHADOW_CASTER_SLOTS).collect::<Vec<_>>());
}

/// The player is never view-culled. The fixture is deliberately self-inconsistent — the player
/// candidate's position is nowhere near the `player_pos` the cull would use — precisely so that
/// *any* cull applied to the player would reject it. In a real frame the two agree, so this can
/// only fail if the planner starts culling the player at all.
#[test]
fn the_player_is_never_view_culled() {
    let player = Cand::skinned([9_000.0, 9_000.0, 9_000.0], 2);
    assert!(!in_view(player.pos, ORIGIN), "fixture: this position fails the cull");
    let steps = plan_shadow_casters(Some(&player), &[] as &[Cand], ORIGIN, ORIGIN, VP).steps;
    assert_eq!(steps.len(), 1, "the player always casts when it has a model");
    assert_eq!(steps[0].caster, ShadowCasterRef::Player);
}

/// Static casters take a uniform slot but no joint slot; absent models take neither. The joint
/// counter must therefore lag the uniform counter, and the resulting `j_slot` sequence must still
/// be dense from 0 (a hole would leave a stale joint palette bound for a later caster).
#[test]
fn static_casters_take_a_uniform_slot_but_no_joint_slot() {
    let cands = [
        Cand::skinned([10.0, 0.0, 0.0], 3),
        Cand::statik([20.0, 0.0, 0.0]),
        Cand::absent([30.0, 0.0, 0.0]),
        Cand::skinned([40.0, 0.0, 0.0], 3),
        Cand::statik([50.0, 0.0, 0.0]),
        Cand::skinned([60.0, 0.0, 0.0], 3),
    ];
    let steps = plan(&cands, ORIGIN);
    assert_eq!(picked(&steps), vec![0, 1, 3, 4, 5], "the absent model casts nothing");
    assert_eq!(u_slots(&steps), vec![0, 1, 2, 3, 4], "uniform slots stay dense across both kinds");

    let j: Vec<usize> = steps.iter().filter_map(|s| match s.draw {
        ShadowCasterDraw::Skinned { j_slot, .. } => Some(j_slot),
        ShadowCasterDraw::Static => None,
    }).collect();
    assert_eq!(j, vec![0, 1, 2], "joint slots are dense from 0 and count only skinned casters");
}

/// **The player path's `Absent` arm.** A player whose race model has not loaded — a real state
/// during zone-in, and permanent for any race whose model never loads — casts nothing and consumes
/// **neither** a uniform slot nor a joint slot.
///
/// This row exists because the slot bookkeeping is written out twice (player block, nearby block)
/// and either copy can rot alone — the same argument this file already applies to the #692/#694
/// clip guard. It was measured, not assumed: without this test,
/// `ShadowModelKind::Absent => { u_slot += 1; }` in the **player** block survives every other test
/// in this file *including* the 400-scene differential, while the identical slip in the **nearby**
/// block is caught. The permitted failure is an ordinary tidy-up — hoisting the `u_slot += 1` out
/// of the two match arms, which the arms already end with — and it shifts every nearby caster onto
/// the wrong pre-allocated uniform buffer while silently dropping the frame's shadow budget to 63.
#[test]
fn a_player_with_no_model_casts_nothing_and_consumes_no_slot() {
    let player = Cand::absent(ORIGIN);
    let cands = [
        Cand::skinned([10.0, 0.0, 0.0], 3),
        Cand::statik([20.0, 0.0, 0.0]),
        Cand::skinned([30.0, 0.0, 0.0], 3),
    ];
    let steps = plan_shadow_casters(Some(&player), &cands, ORIGIN, ORIGIN, VP).steps;

    assert!(
        steps.iter().all(|s| s.caster != ShadowCasterRef::Player),
        "a player with no model must not be planned at all",
    );
    assert_eq!(picked(&steps), vec![0, 1, 2], "the nearby casters are unaffected");
    assert_eq!(
        u_slots(&steps), vec![0, 1, 2],
        "the absent player must not burn uniform slot 0 — if it did, every nearby caster would \
         bind the wrong pre-allocated uniform buffer and the frame budget would drop to 63",
    );
    let j: Vec<usize> = steps.iter().filter_map(|s| match s.draw {
        ShadowCasterDraw::Skinned { j_slot, .. } => Some(j_slot),
        ShadowCasterDraw::Static => None,
    }).collect();
    assert_eq!(j, vec![0, 1], "nor a joint slot");

    // …and, unlike a drawn player, it does not shrink the nearby budget.
    let many: Vec<Cand> =
        (0..SHADOW_CASTER_SLOTS * 2).map(|i| Cand::skinned([i as f32 * 0.5, 0.0, 0.0], 3)).collect();
    let steps = plan_shadow_casters(Some(&player), &many, ORIGIN, ORIGIN, VP).steps;
    assert_eq!(
        steps.len(), SHADOW_CASTER_SLOTS,
        "an absent player leaves the whole budget to the nearby casters",
    );
    assert_eq!(u_slots(&steps), (0..SHADOW_CASTER_SLOTS).collect::<Vec<_>>());
}

/// The invariant that makes the `j_slot >= SHADOW_CASTER_SLOTS` guard in the planner dead code:
/// every step's joint slot is `<= u_slot`, because joints are only ever consumed alongside a
/// uniform slot. Checked over a mixed, over-budget population.
///
/// **This is why mutating that guard survives** — see the PR's mutation table. Pinning the
/// invariant is the honest substitute for a test the guard cannot fail.
#[test]
fn joint_slots_never_outrun_uniform_slots() {
    let cands: Vec<Cand> = (0..SHADOW_CASTER_SLOTS * 2).map(|i| match i % 3 {
        0 => Cand::statik([i as f32 * 0.5, 0.0, 0.0]),
        1 => Cand::skinned([i as f32 * 0.5, 0.0, 0.0], 4),
        _ => Cand::playing([i as f32 * 0.5, 0.0, 0.0], 4, 1, 0.5),
    }).collect();
    let steps = plan(&cands, ORIGIN);
    assert!(!steps.is_empty(), "fixture produced no casters");
    for s in &steps {
        if let ShadowCasterDraw::Skinned { j_slot, .. } = s.draw {
            assert!(j_slot <= s.u_slot, "j_slot {} outran u_slot {}", j_slot, s.u_slot);
            assert!(j_slot < SHADOW_CASTER_SLOTS, "j_slot {} is out of pool range", j_slot);
        }
        assert!(s.u_slot < SHADOW_CASTER_SLOTS, "u_slot {} is out of pool range", s.u_slot);
    }
}

// ── 4. The #692/#694 clip guard ─────────────────────────────────────────────────────────────────

/// The guard, over the whole boundary, through the **planner** (not just `shadow_pose_for`), for
/// both the player path and the nearby path — the pre-#740 code wrote it out twice and either copy
/// could rot alone.
///
/// The `idx == clip_count` row is what dies if the guard is loosened to `<=`; the `usize::MAX` row
/// is the #692/#694 bind-pose sentinel and is what dies if the guard is deleted; the
/// `idx == clip_count - 1` row is what dies if the guard is inverted or made unconditional.
#[test]
fn clip_index_out_of_range_falls_back_to_bind_pose() {
    let cases: [(usize, usize, ShadowPose); 6] = [
        // (clip_count, clip_idx, expected)
        (4, 0,           ShadowPose::Clip { idx: 0, time: 0.75 }),
        (4, 3,           ShadowPose::Clip { idx: 3, time: 0.75 }),
        (4, 4,           ShadowPose::BindPose), // exactly at the bound → OUT of range
        (4, 5,           ShadowPose::BindPose),
        (4, usize::MAX,  ShadowPose::BindPose), // the #692/#694 sentinel
        (0, 0,           ShadowPose::BindPose), // a skin with no clips at all
    ];

    for (clips, idx, expect) in cases {
        let c = Cand::playing([10.0, 0.0, 0.0], clips, idx, 0.75);

        let nearby = plan(std::slice::from_ref(&c), ORIGIN);
        assert_eq!(nearby.len(), 1, "clips={} idx={}: fixture must select one caster", clips, idx);
        assert_eq!(
            pose_of(&nearby[0]), expect,
            "nearby caster with clip_count={} clip_idx={}: an out-of-range index MUST fall back to \
             bind pose (eqoxide#692/#694) — evaluating it would index past the clip list", clips, idx,
        );

        let player = plan_shadow_casters(Some(&c), &[] as &[Cand], ORIGIN, ORIGIN, VP).steps;
        assert_eq!(player.len(), 1, "clips={} idx={}: the player must be selected", clips, idx);
        assert_eq!(
            pose_of(&player[0]), expect,
            "the PLAYER path must apply the same guard as the nearby path",
        );
    }
}

/// An in-range clip is used verbatim — index *and* time. Catches a mutant that always bind-poses,
/// and one that forwards the wrong field (a hard-coded `time: 0.0` would still animate, just
/// frozen).
#[test]
fn in_range_clip_state_is_forwarded_verbatim() {
    let c = Cand::playing([10.0, 0.0, 0.0], 9, 7, 1.375);
    let steps = plan(std::slice::from_ref(&c), ORIGIN);
    assert_eq!(pose_of(&steps[0]), ShadowPose::Clip { idx: 7, time: 1.375 });
}

/// No animation state at all → bind pose, whatever the clip count.
#[test]
fn a_caster_with_no_anim_state_bind_poses() {
    for clips in [0, 1, 64] {
        let c = Cand::skinned([10.0, 0.0, 0.0], clips);
        let steps = plan(std::slice::from_ref(&c), ORIGIN);
        assert_eq!(pose_of(&steps[0]), ShadowPose::BindPose, "clip_count = {}", clips);
    }
}

/// The `clip_count != 0` half of the guard is redundant with `idx < clip_count`: no `usize` is
/// less than zero, so an in-range index already proves the skin has at least one clip. Pinned as a
/// property rather than left as folklore, because a mutation that deletes that half **survives**
/// (disclosed in the PR's mutation table) and the next reader deserves to know why.
#[test]
fn redundant_is_empty_half_of_the_guard_is_provably_dead() {
    for idx in [0usize, 1, 7, usize::MAX] {
        assert_eq!(
            shadow_pose_for(Some((idx, 0.0)), 0), ShadowPose::BindPose,
            "clip_count = 0 must never yield a clip pose (idx = {}) — and `idx < clip_count` alone \
             already guarantees it, because no usize is below zero", idx,
        );
    }
}

// ── 5. Differential pin against the pre-#740 loops ──────────────────────────────────────────────

/// Verbatim transcription of the selection half of `encode_shadow_pass` at the merge-base
/// (`fca02c9`), before #740 extracted it. Structure, guard order, and the `break`/`continue`
/// placement are preserved exactly; only the data source is swapped from `&Billboard` +
/// `r.model_by_key(…)` to the test candidate.
///
/// **This grades the extracted planner against the OLD implementation, not against itself.**
fn old_selection(
    player:       Option<&Cand>,
    nearby:       &[Cand],
    light_center: [f32; 3],
    player_pos:   [f32; 3],
    view_proj:    [[f32; 4]; 4],
) -> Vec<ShadowCasterStep> {
    let mut out: Vec<ShadowCasterStep> = Vec::new();
    let mut u_slot = 0usize;
    let mut j_slot = 0usize;

    if let Some(p) = player {
        match p.kind {
            ShadowModelKind::Skinned { clip_count } => {
                let pose = match p.anim {
                    Some((idx, time)) if clip_count != 0 && idx < clip_count =>
                        ShadowPose::Clip { idx, time },
                    _ => ShadowPose::BindPose,
                };
                out.push(ShadowCasterStep {
                    caster: ShadowCasterRef::Player,
                    u_slot,
                    draw: ShadowCasterDraw::Skinned { j_slot, pose },
                });
                u_slot += 1;
                j_slot += 1;
            }
            ShadowModelKind::Static => {
                out.push(ShadowCasterStep {
                    caster: ShadowCasterRef::Player, u_slot, draw: ShadowCasterDraw::Static,
                });
                u_slot += 1;
            }
            ShadowModelKind::Absent => {}
        }
    }

    let pp = light_center;
    let mut order: Vec<(usize, &Cand)> = nearby.iter().enumerate().collect();
    order.sort_by(|(_, a), (_, b)| {
        let da = (a.pos[0] - pp[0]).powi(2) + (a.pos[1] - pp[1]).powi(2) + (a.pos[2] - pp[2]).powi(2);
        let db = (b.pos[0] - pp[0]).powi(2) + (b.pos[1] - pp[1]).powi(2) + (b.pos[2] - pp[2]).powi(2);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });
    for (i, b) in order {
        if u_slot >= SHADOW_CASTER_SLOTS { break; }
        if !entity_in_view(b.pos, player_pos, view_proj, ENTITY_DRAW_DIST, ENTITY_CULL_MARGIN) {
            continue;
        }
        match b.kind {
            ShadowModelKind::Skinned { clip_count } => {
                if j_slot >= SHADOW_CASTER_SLOTS { continue; }
                let pose = match b.anim {
                    Some((idx, time)) if clip_count != 0 && idx < clip_count =>
                        ShadowPose::Clip { idx, time },
                    _ => ShadowPose::BindPose,
                };
                out.push(ShadowCasterStep {
                    caster: ShadowCasterRef::Nearby(i),
                    u_slot,
                    draw: ShadowCasterDraw::Skinned { j_slot, pose },
                });
                u_slot += 1;
                j_slot += 1;
            }
            ShadowModelKind::Static => {
                out.push(ShadowCasterStep {
                    caster: ShadowCasterRef::Nearby(i), u_slot, draw: ShadowCasterDraw::Static,
                });
                u_slot += 1;
            }
            ShadowModelKind::Absent => {}
        }
    }
    out
}

fn dist2_to(p: [f32; 3], c: [f32; 3]) -> f32 {
    (p[0] - c[0]).powi(2) + (p[1] - c[1]).powi(2) + (p[2] - c[2]).powi(2)
}

/// SplitMix64 finalizer (Steele, Lea & Flood), used only to scramble a per-scene LCG seed before
/// it is used — **not** as the corpus's own generator.
///
/// **Measured (eqoxide#751, second pass) — why this exists.** `Rng(0x5EED_740 ^ scene as u64)`
/// (the original per-scene reseed) fixed the cross-scene coupling but introduced a narrower,
/// measured defect: an LCG's *n*-th output is affine in its seed, so running the same small number
/// of steps from 400 seeds that differ only in their low bits (`scene` is 0..400, XORed into a
/// constant) does not sample the state space — it walks an arithmetic-ish progression through it.
/// At the LCG-step offset that produces each scene's first candidate's model-kind selector
/// (`rng.range(10)` — one `n` draw plus three `coord` draws in), this file's own values were
/// `Absent=39 Static=1 Skinned=360` against a ~40/80/280 expectation, chi²(9 df) = **399.90** — not
/// an XOR artefact (`scene` bare or `scene.wrapping_add(…)` reproduce it) but a property of taking
/// an affine function of a near-arithmetic sequence of seeds. Confirmed independently outside the
/// crate (a standalone replica of this exact `next`/`range` arithmetic, not a guess): folding the
/// seed through `splitmix64` before construction drops the worst offset found by scanning the first
/// 25 draws from chi² 399.90 to **~20.9** (still not perfectly uniform — 400 samples over 10 bins is
/// a coarse test — but two orders of magnitude closer, and every coverage floor in this file
/// (§`extracted_planner_matches_the_pre_740_loops_over_a_random_corpus`) stays clear under it; see
/// that test's doc comment for the re-measured corpus figures). `splitmix64` is applied once, to the
/// seed only — the LCG below is unchanged and is still what actually drives the corpus.
fn splitmix64(x: u64) -> u64 {
    let x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Deterministic LCG — a fixed corpus, so a failure is reproducible from the seed alone. Seeds fed
/// to this generator should go through `splitmix64` first (see its doc comment) — the LCG's own
/// step function does not decorrelate a near-arithmetic sequence of seeds on its own.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn range(&mut self, n: u64) -> u64 { self.next() % n }
    /// A coordinate in `[-half, half]`.
    fn coord(&mut self, half: i64) -> f32 { (self.range(half as u64 * 2 + 1) as i64 - half) as f32 }
}

/// One scene's fixture — candidates, player, light center, and player position — built from a
/// PRNG stream seeded from `scene` **alone** (eqoxide#751). This used to be inlined in the
/// differential test's `for` loop, drawing from one `Rng` created before the loop and threaded
/// through all 400 iterations; every scene's draws therefore started wherever the previous scene's
/// draws happened to leave the stream, so editing what scene `k` generates silently shifted what
/// scene `k + 1` (and every scene after it) generates. That already cost a mutant kill once — see
/// this file's later doc comments on `saw_ties` / round 2 of #747 — and is recorded in the repo memory
/// note `eq-fixture-edits-are-not-local`.
///
/// Pulling scene construction out to a free function that takes only `scene` guarantees one
/// specific thing, precisely: there is no variable anywhere in this file that holds an `Rng` (or
/// anything derived from one) across a call boundary, so no call's draws can depend on **when** it
/// was made or **which other calls** happened around it.
/// `scene_fixtures_are_independent_of_call_order_and_neighboring_scenes` below exercises exactly
/// that — call-order and neighboring-call independence — and only that.
///
/// **Retracted (eqoxide#751, second pass): this file previously called that coupling "structurally
/// impossible".** It is not — the property test above pins order-independence, not
/// edit-locality, and those are different claims. An independent reviewer's **MUT-6** makes the
/// difference concrete: mutate `build_scene` so that scene `k`'s seed is folded together with a
/// fresh, independent call to `build_scene(k - 1)` (no `Rng`, or anything derived from one, is
/// threaded across a call boundary — `build_scene(k - 1)` is recomputed from scratch, not read from
/// stored state). `build_scene(k)` is still a pure function of `k` alone under MUT-6, so it still
/// gives the same answer regardless of call order or what else was built around it — the property
/// test above cannot see the difference and stays green. But editing scene `k − 1`'s generation
/// logic now silently changes scene `k`'s fixture, which is exactly the failure mode this fix set
/// out to make impossible. Measured on the real build: MUT-6 leaves all 19 tests in this file green
/// (**SURVIVED**). A source-text scan
/// (`build_scenes_source_text_contains_no_literal_call_to_itself`, below) closes this specific
/// shape — it isolates this function's own body and fails if it contains a nested call to
/// `build_scene` — but it is a syntactic pin, narrower than this paragraph's framing: it proves
/// there is no *literal* `build_scene(` substring in the body, not that there is no dependency of
/// any kind on another scene. See that test's own doc comment for what it does and does not catch,
/// including a round-2 fix for a truncation gap in the scan itself.
fn build_scene(scene: usize) -> (Vec<Cand>, Option<Cand>, [f32; 3], [f32; 3]) {
    let mut rng = Rng(splitmix64(0x5EED_740 ^ scene as u64));

    // Every 5th scene is DENSE: a crowded zone where every candidate is comfortably inside the
    // cull, so the slot bound is actually reached. Left to chance, a uniformly-scattered corpus
    // hits the bound rarely, and the truncation path would go effectively unsampled.
    let dense = scene % 5 == 0;
    let (half, player_half) = if dense { (280, 10) } else { (550, 80) };
    let n = if dense {
        (SHADOW_CASTER_SLOTS as u64 + 40 + rng.range(60)) as usize
    } else {
        rng.range(SHADOW_CASTER_SLOTS as u64 * 2 + 4) as usize
    };
    let mut cands: Vec<Cand> = Vec::with_capacity(n);
    for k in 0..n {
        // **Deliberate exact ties**: roughly one candidate in four lands exactly on its
        // predecessor. This is the only thing in the whole file that discriminates `sort_by`
        // from `sort_unstable_by` (see `equidistant_casters_keep_input_order`), and it used to
        // be *incidental* — the corpus happened to contain a discriminating tie. An incidental
        // property is one an unrelated fixture edit silently deletes, and that is exactly what
        // happened: adding the absent-player arm below shifted the RNG stream and M14 went from
        // killed to surviving. `saw_ties` asserts the coverage rather than hoping for it.
        let pos = if k > 0 && rng.range(4) == 0 {
            cands[k - 1].pos
        } else {
            [rng.coord(half), rng.coord(half), rng.coord(half)]
        };
        cands.push(match rng.range(10) {
            0 => Cand::absent(pos),
            1 | 2 => Cand::statik(pos),
            3 => Cand::skinned(pos, rng.range(6) as usize),
            _ => {
                let clips = rng.range(6) as usize;
                let idx = match rng.range(8) {
                    0 => usize::MAX,
                    1 => clips,
                    2 => clips + 3,
                    _ => rng.range(6) as usize,
                };
                Cand::playing(pos, clips, idx, rng.range(1000) as f32 / 64.0)
            }
        });
    }
    let ppos = |rng: &mut Rng| [rng.coord(player_half), rng.coord(player_half), rng.coord(player_half)];
    // All four player states, including `Absent` — the corpus originally never built an absent
    // player, which left that production branch with zero coverage of any kind.
    let player = match rng.range(8) {
        0 | 1 => None,
        2 | 3 => Some(Cand::statik(ppos(&mut rng))),
        4 => Some(Cand::absent(ppos(&mut rng))),
        _ => Some(Cand::playing(ppos(&mut rng), 4, rng.range(6) as usize, 0.5)),
    };
    let light = [rng.coord(half), rng.coord(half), rng.coord(half)];
    let player_pos = ppos(&mut rng);

    (cands, player, light, player_pos)
}

/// 400 pseudo-random scenes: **all four model kinds on both paths**, mixed clip counts (including
/// zero), clip indices on both sides of the bound plus the `usize::MAX` sentinel, populations
/// straddling `SHADOW_CASTER_SLOTS`, deliberate exact ties, and light centers that differ from the
/// player. The extracted planner must emit a byte-identical step stream to the pre-#740 loops in
/// every one.
///
/// Coverage bound, as a rule: this is *sampled*, not exhaustive — it is a differential over a fixed
/// pseudo-random corpus, so it can only catch divergences some scene in that corpus reaches. The
/// four decisions are pinned exhaustively at their boundaries by the tests above; this catches
/// interaction bugs between them.
///
/// **The reach counters are reported by the planner itself** (`ShadowCasterReach`), not computed
/// out here. Two earlier drafts computed them, and both were wrong in the same way:
///
/// - round 2 read `cands` — the *generator* — so the counters stated what the fixture built and were
///   structurally incapable of moving when the *planner* stopped reaching a variant. Measured:
///   373/392/397 scenes generated a nearby `Absent`/`Static`/`Skinned`, only 334/376/392 reached one.
/// - round 3 read a reconstruction of the loop, i.e. a **second copy of the planner's termination
///   rule**, self-checked only by a *subsequence* test — which constrains the ordering and places no
///   constraint at all on the cut. Measured by the round-3 review: capping the nearby loop at 100
///   examined candidates lost real reach in 85 of 400 scenes (1317 candidate-examinations claimed
///   that never happened) and left every counter byte-identical.
///
/// There is now exactly one source for the cut and for each arm, and it is inside the loop. The two
/// asserts that keep it honest are in the scene body: the arm identity
/// (`examined == culled + absent + static + skinned`), which fails on a misplaced increment, and the
/// no-early-stop assert, which fails on the 100-candidate cap above.
///
/// **The thresholds below are smoke alarms, not safeguards — do not tighten them.** They sit far
/// under the observed values on purpose. What actually protects the sort-stability mutant (`sort_by`
/// → `sort_unstable_by`) is the by-construction tie *generation* above, not the `saw_ties` floor:
/// #747's round-2 review measured that cutting tie generation to ~one pair in half the scenes drops
/// tie coverage by more than half (390 → 185 scenes generating a tie, 377 → 90 reaching one) and the
/// mutant is *still* killed. A threshold pinned at observed-minus-epsilon would buy no grading power
/// and would make the test flaky under unrelated generator edits. These fail loudly if a future edit
/// empties a category outright, and that is their whole job.
///
/// **Historical, pre-#751 numbers — the single shared-stream seed, before per-scene reseeding.**
/// Observed at that seed, for reference and not as targets: truncation 80, cull 317, clip guard 391,
/// ties 369, and the matrix cells 98/45/95/162 (player) and 334/376/392 (nearby). Every one of these
/// was unchanged from round 3 — the round-3 reconstruction *was* accurate at this seed, which is
/// exactly why nothing here caught that it could not be falsified.
///
/// **`saw_ties = 369` vs the round-2 review's `377`, reconciled by measurement, not by argument
/// (also pre-#751 — see below for the numbers this corpus produces now).** They are different
/// quantities, not a discrepancy. Re-measured over the identical (pre-#751) corpus (the generator
/// had not changed since round 2 — "any tie in `cands`" still reproduced round 2's 390 exactly,
/// which pinned that the scenes were the same):
///
/// | definition | scenes |
/// |---|---|
/// | two **selected** casters equidistant — what `saw_ties` counts | **369** |
/// | two candidates the planner examined **and that survived the cull** are equidistant | **377** |
/// | two candidates the planner examined are equidistant | 390 |
/// | two candidates anywhere in `cands` are equidistant — round 2's `saw_ties` | 390 |
///
/// So round 2's `377` is the second row, and its label "(both in the plan)" was imprecise: a tie
/// between two examined, in-view candidates counts there even when one of them is `Absent` and
/// therefore never enters the plan. `369` was the right number for this assertion's stated purpose —
/// only casters that actually reach `steps` can discriminate `sort_by` from `sort_unstable_by`,
/// because a reordering that touches an `Absent` candidate is invisible in the output. The definitions
/// in this table are still the right definitions after #751; only the corpus (and so the counts)
/// changed — see below.
///
/// **Fixed (eqoxide#751): each scene now draws from its own `Rng(splitmix64(0x5EED_740 ^ scene as
/// u64))`**, built by `build_scene(scene)` and never threaded across scenes (see that function's
/// doc comment). Editing what an earlier scene generates can no longer move what a later scene
/// generates — the exact failure mode that silently dropped mutant M14 (`sort_by` →
/// `sort_unstable_by`) from killed to surviving while fixing #747's B1.
///
/// **Re-measured a second time (eqoxide#751 round 2), because the seed formula changed again.** An
/// independent review found the first `0x5EED_740 ^ scene as u64` seed, though no longer shared
/// across scenes, was measurably non-uniform *within* a scene: an LCG's output is affine in its
/// seed, and 400 seeds differing only in their low bits do not decorrelate in a handful of steps —
/// see `splitmix64`'s doc comment above for the chi² figures. Every number below was re-measured
/// against the `splitmix64`-scrambled seed (`eprintln!` added temporarily, then removed — not left
/// as test output): truncation 80, cull 317, clip guard 390, ties (selected-equidistant) 363, and
/// the matrix cells 103/42/100/155 (player, in None/Absent/Static/Skinned order) and 343/362/392
/// (nearby, in Absent/Static/Skinned order). All comfortably clear the floors below — the floors
/// were not retuned to these new values, for the same reason given at `saw_ties`'s own assert.
///
/// The reason a per-scene stream matters at all: a pseudo-random corpus only covers what it happens
/// to generate, and — before this fix — an unrelated edit to the generator could silently move the
/// whole stream. Not hypothetical: it happened while fixing #747's B1 (adding the absent-player arm
/// shifted the stream and the incidental tie that was killing that mutant vanished). Per-scene
/// seeding does not make the corpus exhaustive — it is still sampled. **What it actually rules out**
/// is one specific coupling: an edit to scene `k`'s fixture moving scene `k′ ≠ k`'s draws *by being
/// called before or after it*. It does not rule out a fixture generator that deliberately derives
/// scene `k`'s inputs from scene `k − 1`'s recomputed output — see `build_scene`'s doc comment for
/// that distinction and the mutant (MUT-6) that draws it.
#[test]
fn extracted_planner_matches_the_pre_740_loops_over_a_random_corpus() {
    let mut saw_truncation = 0usize;
    let mut saw_cull = 0usize;
    let mut saw_bind_fallback = 0usize;
    // Per-PATH model-kind reach, asserted below. Every `ShadowModelKind` variant must be reached on
    // BOTH paths, because the planner writes the slot bookkeeping out once per path and either copy
    // can rot alone. This is asserted rather than reasoned: the corpus used to reach
    // `Absent` only on the nearby path, and the player-path `Absent` arm had no coverage at all.
    //
    // Where each counter's SUBJECT comes from: the four nearby cells and `player/Absent` come from
    // `plan.reach`, which the planner reports from inside its own loop; `nearby/Static` and
    // `nearby/Skinned` are cross-checked against `got` on every scene, since those two arms do emit
    // steps. Nothing here re-derives the loop's termination rule, and nothing here counts what the
    // GENERATOR built. `cands` is still indexed — for `saw_ties`' positions and the clip guard's
    // anim states — but only at indices the PLAN supplies: the index SET is plan-derived, the
    // per-index lookup is necessarily fixture-side. (Round 3 claimed "none of them read `cands`";
    // three of them did, on the same screen. The claim was about the index set and was written as
    // if it were about the whole expression.)
    let (mut saw_player_none, mut saw_player_absent) = (0usize, 0usize);
    let (mut saw_player_static, mut saw_player_skinned) = (0usize, 0usize);
    let (mut saw_nearby_absent, mut saw_nearby_static, mut saw_nearby_skinned) = (0usize, 0usize, 0usize);
    let mut saw_ties = 0usize;

    for scene in 0..400 {
        // Built by an independently-seeded stream (eqoxide#751) — see `build_scene`'s doc comment.
        let (cands, player, light, player_pos) = build_scene(scene);
        let n = cands.len();

        let plan = plan_shadow_casters(player.as_ref(), &cands, light, player_pos, VP);
        let (got, reach) = (plan.steps, plan.reach);
        let want = old_selection(player.as_ref(), &cands, light, player_pos, VP);
        assert_eq!(
            got, want,
            "scene {} ({} candidates) diverged from the pre-#740 selection loops", scene, n,
        );

        // ── Corpus-reach accounting ─────────────────────────────────────────────────────────────
        // A differential that never reaches the interesting states is a differential over the empty
        // set. The subject of every counter below is the PLANNER; see the declarations.

        // The planner's reach report must add up. Every examined candidate lands in exactly one of
        // the four buckets, so a misplaced, duplicated or dropped increment inside the loop reports
        // here as a broken counter instead of as quietly wrong coverage. This is the one thing the
        // reach struct cannot get wrong silently.
        assert_eq!(
            reach.nearby_examined,
            reach.nearby_culled + reach.nearby_absent + reach.nearby_static + reach.nearby_skinned,
            "scene {}: the planner's reach report does not add up — {:?}", scene, reach,
        );
        // …and the two arms that DO emit steps must agree with the plan, which is an independent
        // subject: `reach` is written inside the loop, `got` is what came out of it.
        assert_eq!(
            reach.nearby_static,
            got.iter().filter(|s| matches!(s.caster, ShadowCasterRef::Nearby(_))
                && matches!(s.draw, ShadowCasterDraw::Static)).count(),
            "scene {}: reported nearby/Static reach disagrees with the plan", scene,
        );
        assert_eq!(
            reach.nearby_skinned,
            got.iter().filter(|s| matches!(s.caster, ShadowCasterRef::Nearby(_))
                && matches!(s.draw, ShadowCasterDraw::Skinned { .. })).count(),
            "scene {}: reported nearby/Skinned reach disagrees with the plan (the dead `j_slot` \
             guard is the only way these can legitimately differ, and it never fires)", scene,
        );
        // **The planner must not stop early with slots to spare.** `u_slot` advances once per
        // emitted step, so a plan short of the budget means the bound never fired and the loop owed
        // the whole order a look. Stated as an asserted property of the system, not used as a
        // derivation feeding a counter — that inversion is what #747 round 3 got wrong. This is the
        // assert that fails when the nearby loop is capped early (the round-3 review's R2 mutation,
        // which previously survived with every counter unmoved).
        if got.len() < SHADOW_CASTER_SLOTS {
            assert_eq!(
                reach.nearby_examined, cands.len(),
                "scene {}: the planner examined {} of {} candidates but filled only {} of {} slots \
                 — it stopped short of the order with budget left", scene,
                reach.nearby_examined, cands.len(), got.len(), SHADOW_CASTER_SLOTS,
            );
        }

        if got.len() == SHADOW_CASTER_SLOTS { saw_truncation += 1; }

        // A real `entity_in_view` rejection *inside the loop*, counted by the loop. The round-2
        // form (`nearby steps < n`) also counted `Absent` candidates and truncation as "culls",
        // which made it near-vacuous — 397 of 400 scenes — while its message said "culled".
        if reach.nearby_culled > 0 { saw_cull += 1; }

        // The clip guard FIRING, not merely a bind pose: a caster that had an animation state and
        // was still bind-posed. `Cand::skinned` has no anim state and bind-poses through the `_ =>`
        // arm without ever evaluating `idx < clip_count`.
        let anim_of = |s: &ShadowCasterStep| match s.caster {
            ShadowCasterRef::Player    => player.as_ref().and_then(|p| p.anim),
            ShadowCasterRef::Nearby(i) => cands[i].anim,
        };
        if got.iter().any(|s| {
            matches!(s.draw, ShadowCasterDraw::Skinned { pose: ShadowPose::BindPose, .. })
                && anim_of(s).is_some()
        }) { saw_bind_fallback += 1; }

        // A tie the planner actually SAW: two selected casters equidistant from the light. A tie
        // among candidates the loop never reached cannot discriminate the sort.
        let mut sel_d: Vec<f32> = picked(&got).iter().map(|&i| dist2_to(cands[i].pos, light)).collect();
        sel_d.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if sel_d.windows(2).any(|w| w[0] == w[1]) { saw_ties += 1; }

        // The variant × path matrix, straight off the planner's own report. The two `Absent` cells
        // emit no step, so before this they were the two cells nothing could observe: an empty
        // match arm is indistinguishable from a dead one, and the round-3 review proved it by
        // making the nearby `Absent` arm unreachable with a plan-neutral `continue` and watching
        // this counter hold at 334. It is counted *inside* the arm now, so that mutation zeroes it.
        if reach.nearby_absent > 0 { saw_nearby_absent += 1; }
        if reach.nearby_static > 0 { saw_nearby_static += 1; }
        if reach.nearby_skinned > 0 { saw_nearby_skinned += 1; }

        match (player.as_ref(), got.iter().find(|s| s.caster == ShadowCasterRef::Player)) {
            (None, _) => saw_player_none += 1,
            (Some(_), None) => {
                assert!(reach.player_absent, "scene {}: a supplied player produced no step and the \
                    planner did not report taking the player `Absent` arm", scene);
                saw_player_absent += 1;
            }
            (Some(_), Some(s)) if matches!(s.draw, ShadowCasterDraw::Static) => saw_player_static += 1,
            (Some(_), Some(_)) => saw_player_skinned += 1,
        }
    }

    // Floors, not targets — see this test's doc comment. Each message names the object it counts,
    // because the round-2 review of #747 found three of these saying "reached" while counting what
    // the generator built, and the round-3 review found the replacements saying "reached" while
    // counting a reconstruction that could not be falsified.
    assert!(
        saw_truncation >= 60,
        "the returned plan filled all {} slots in only {} of 400 scenes",
        SHADOW_CASTER_SLOTS, saw_truncation,
    );
    assert!(
        saw_cull >= 150,
        "only {} of 400 scenes had the planner examine a candidate and reject it on `entity_in_view` \
         (candidates past the bound are not examined and do not count)", saw_cull,
    );
    assert!(
        saw_bind_fallback >= 150,
        "only {} of 400 scenes produced a bind-posed step for a caster that HAD an animation state \
         — i.e. the #692/#694 clip guard actually firing, not a no-anim bind pose", saw_bind_fallback,
    );
    assert!(
        saw_ties >= 150,
        "only {} of 400 scenes had two SELECTED casters equidistant from the light — a tie among \
         candidates the planner never reached cannot discriminate `sort_by` from `sort_unstable_by`, \
         and that discrimination exists nowhere else in this file except \
         `equidistant_casters_keep_input_order`", saw_ties,
    );

    // The variant × path matrix, asserted against the planner's own reach report. A variant the
    // planner stops reaching on either path is a silent loss of coverage for a live production
    // branch, not harmless corpus drift. The two `Absent` cells emit no step, so they are counted
    // inside their arms — which is the only way they are observable at all, and is what makes
    // "reached" true of every row here rather than merely inferred for two of them.
    for (label, n) in [
        ("player/None", saw_player_none), ("player/Absent", saw_player_absent),
        ("player/Static", saw_player_static), ("player/Skinned", saw_player_skinned),
        ("nearby/Absent", saw_nearby_absent), ("nearby/Static", saw_nearby_static),
        ("nearby/Skinned", saw_nearby_skinned),
    ] {
        assert!(n >= 20, "the planner reached {} in only {} of 400 scenes", label, n);
    }

    // NOT asserted, deliberately: that some truncated scene truncates a MIXED static/skinned
    // population — the condition that stops the documented-dead `j_slot` guard from masking a
    // deleted `u_slot` bound (see `selection_is_bounded_at_exactly_shadow_caster_slots`). Measured
    // here: 80 of 80 truncating scenes are mixed and 0 are all-skinned, because a truncating scene
    // draws ~64+ candidates that are each static with probability ~0.2 — all-skinned has
    // probability ~0.8^64 ≈ 6e-7. That is the law of large numbers, not a coincidence one edit can
    // delete, and the mixed-fixture bound test above is the primary grader for it (this corpus is
    // redundancy). Asserting every property merely *relied on* would grow this test without
    // grading anything; the criterion for adding one is unasserted AND fragile.
}

// ── 5b. Per-scene stream independence (eqoxide#751) ─────────────────────────────────────────────

/// The executable form of "scene order cannot affect the result" (eqoxide#751), not just a
/// construction argument. Calls `build_scene` for the same handful of indices under three different
/// surrounding contexts — alone, interleaved with unrelated "noise" scenes built before and after
/// each probe, and in reverse overall order — and requires byte-identical fixtures every time.
///
/// This is a genuine property test over the mutation this issue is about: the old code's failure mode
/// was that scene `k`'s draws depended on what scene `k - 1` (or any earlier scene) had drawn, because
/// they shared one `Rng` advanced across the whole 400-scene loop. A single fixed corpus (like the
/// differential test above) cannot exercise "what if the scenes before this one had been different" —
/// it only ever builds the scenes in one order. This test deliberately varies that order.
#[test]
fn scene_fixtures_are_independent_of_call_order_and_neighboring_scenes() {
    let probe_scenes = [0usize, 1, 5, 17, 99, 200, 399];
    let noise_scenes = [321usize, 4, 250, 12, 77, 398, 6, 150, 43, 291];

    // Baseline: each probe built in total isolation, nothing else built before, between, or after.
    let baseline: Vec<_> = probe_scenes.iter().map(|&s| build_scene(s)).collect();

    // Interleaved: unrelated scenes built immediately before AND after every probe. If any state
    // leaked between calls (a shared/advancing stream, a thread-local, anything), this changes what
    // the probe sees relative to baseline.
    let interleaved: Vec<_> = probe_scenes.iter().enumerate().map(|(i, &s)| {
        let _ = build_scene(noise_scenes[i % noise_scenes.len()]);
        let out = build_scene(s);
        let _ = build_scene(noise_scenes[(i + 3) % noise_scenes.len()]);
        out
    }).collect();

    // Reverse overall order: guards against a coupling that only happens to cancel out when scenes
    // are built in increasing order (e.g. a stream re-synced periodically).
    let reversed: Vec<_> = probe_scenes.iter().rev().map(|&s| build_scene(s)).collect();

    for (i, &s) in probe_scenes.iter().enumerate() {
        assert_eq!(
            baseline[i], interleaved[i],
            "scene {}'s fixture changed depending on what unrelated scenes were built immediately \
             before/after it — the per-scene stream is not actually independent", s,
        );
        let rev_i = probe_scenes.len() - 1 - i;
        assert_eq!(
            baseline[i], reversed[rev_i],
            "scene {}'s fixture changed when the probes were built in a different overall order", s,
        );
    }
}

// ── 5c. Source-text pin against a hidden edit-locality coupling (eqoxide#751 round 2, MUT-6) ─────

const THIS_FILE: &str = include_str!("shadow_caster_selection.rs");

/// **Why this exists, and why it is not stronger.** An independent reviewer's `MUT-6` mutates
/// `build_scene(scene)` so that, when `scene > 0`, it folds in a value taken from a **freshly
/// recomputed** call to `build_scene(scene - 1)` (recomputed from scratch — no `Rng`, or anything
/// derived from one, is threaded across a call boundary; see `build_scene`'s doc comment). Under
/// that mutation `build_scene(k)` is still a pure function of `k` alone, so
/// `scene_fixtures_are_independent_of_call_order_and_neighboring_scenes` — which pins call-order and
/// neighboring-call independence — cannot see any difference and stays green, and so does every
/// coverage floor in the differential test. Measured on the real build: **19/19 green, SURVIVED**.
///
/// What actually changes under MUT-6 is not runtime behavior for the *current* source — it is what
/// happens when scene `k − 1`'s generation logic is later edited: scene `k`'s fixture moves too, the
/// exact silent-coupling failure mode eqoxide#751 exists to close, just introduced by construction
/// this time instead of by a shared stream. No blackbox input/output test can distinguish "a pure
/// function of `scene`" from "a pure function of `scene` that happens to be defined by recomputing
/// its predecessor", because both give identical answers for any fixed version of the source. The
/// only test genuinely proving that a coupling is closed here is a mutation test on that exact
/// source, which is a project-process check, not something this file can assert every time it runs.
///
/// This test is the file's disclosed, partial answer: a **source-text** scan over `build_scene`'s
/// own body, failing if it contains a nested call back into `build_scene`. It does **not** use the
/// same technique as `encode_shadow_pass_calls_the_planner_this_file_grades` below — that one splits
/// on the next `\npub fn `, which has no brace-balance dependency, because it only needs "everything
/// up to the next public item" and `encode_shadow_pass` is `pub`. `build_scene` is private and is
/// followed immediately by another function's doc comment (which itself quotes `build_scene(scene)`
/// in prose), so the equivalent `\nfn ` split swallows that prose and false-fails on the unmutated
/// file (this was tried first, and did exactly that, before this scan was written) — isolating
/// `build_scene`'s own body precisely requires walking its actual brace structure, not a text split.
///
/// It kills MUT-6 exactly as described above — confirmed by applying that literal mutation and
/// observing this test go red while the rest of the file stays green.
///
/// **Second round, per independent review: brace-depth counting has no lexer**, so it does not know
/// a `}` inside a `//` comment, a string literal (e.g. `write!(f, "}}")`), or a char literal (`'}'`)
/// is not a real closing brace. Measured: MUT-6's recursive call *plus* a single such comment placed
/// early in the body made the naive scan return to depth 0 after only ~58 bytes — long before
/// `build_scene`'s real end — so the truncated `body` never contained the recursive call at all and
/// this test stayed green (**SURVIVED**, 20/0), silently, with the recursive call sitting in plain
/// sight just outside the scanned window. That is strictly worse than the gaps disclosed below: those
/// are honestly out of scope; this one was in scope and the scan still missed it.
///
/// **Fix, not a full lexer**: after finding a candidate close brace, assert the scan actually reached
/// `build_scene`'s real tail — its literal final expression, `(cands, player, light, player_pos)`,
/// which is the last text in the function before its true closing brace. Any brace that closes the
/// scan early leaves that tail *outside* `body`, so a truncation is caught by its absence rather than
/// trusted by the depth counter's say-so; there is no room left in the real function, after that tail
/// expression, for a stray `}` to hide beyond it. Measured, both directions: on the unmutated file
/// this adds nothing observable (**20/0**, `body` already reaches the tail). Under MUT-6 plus the
/// stray-`}`-comment mutation, the terminator assert now fires (**19/1**) instead of the scan silently
/// returning a truncated, all-clear `body`.
///
/// This scan is still not a lexer, and the terminator check only proves the scan reached *at least*
/// as far as the known tail text — not that every byte in between was interpreted correctly. It would
/// **not** catch the same coupling reached through an intermediate helper function (e.g. `fn
/// prior_signal(scene: usize) -> u64 { let (c, ..) = build_scene(scene - 1); c.len() as u64 }` called
/// from inside `build_scene`), through `build_scene (scene - 1)` with inserted whitespace, through a
/// function-pointer alias, or through anything that recomputes an earlier scene's data without the
/// literal token `build_scene(` appearing inside this function's body. Those gaps are real and are
/// not claimed to be closed anywhere in this file. **What this test actually proves, precisely: no
/// literal, whitespace-exact `build_scene(` substring appears in `build_scene`'s own scanned source
/// text** — a narrower claim than "no dependency on another scene" (see the name below, chosen to
/// match).
#[test]
fn build_scenes_source_text_contains_no_literal_call_to_itself() {
    // Isolate exactly `build_scene`'s own `{ … }` by brace-depth counting from its signature's
    // opening brace to the matching close — NOT by splitting on the next `\nfn `, which swallows
    // every doc comment between this function and the next one (several of which quote
    // `build_scene(scene)` in prose) and made this test fail on the unmutated file the first time
    // it was written (see this test's doc comment).
    let after_sig = THIS_FILE
        .split_once("fn build_scene(scene: usize)")
        .expect("build_scene not found in this file")
        .1;
    let open = after_sig.find('{').expect("build_scene has no opening brace");
    let bytes = after_sig.as_bytes();
    let mut depth = 0i32;
    let mut close = None;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 { close = Some(i); break; }
            }
            _ => {}
        }
    }
    let close = close.expect("build_scene's closing brace not found");
    let body = &after_sig[open + 1..close];
    // This scan has no lexer: a `}` inside a comment or string/char literal in the real body would
    // close it early, and the truncated `body` below would then silently omit whatever comes after —
    // including a recursive call the assert below exists to catch. Guard against that by requiring
    // the scan to have reached `build_scene`'s actual last line of code, which sits immediately before
    // its true closing brace and leaves nothing between it and the end for a stray `}` to hide behind.
    assert!(
        body.contains("(cands, player, light, player_pos)"),
        "the brace-depth scan did not reach build_scene's real end (only {} bytes scanned) — a `}}` \
         inside a comment, string, or char literal earlier in the body most likely closed it early; \
         this scan cannot tell a real closing brace from one of those, so a truncated scan must fail \
         loudly here instead of silently reporting no self-call", body.len(),
    );
    assert!(
        !body.contains("build_scene("),
        "build_scene now calls itself (directly) — scene k's fixture can depend on another scene's \
         recomputed fixture, the coupling MUT-6 draws out (see this test's doc comment); no other \
         test in this file would catch that",
    );
}

// ── 6. Call-site pin (source text, not semantics) ───────────────────────────────────────────────

const PASS_RS: &str = include_str!("../src/pass.rs");

/// Nothing else in this file would fail if `encode_shadow_pass` stopped calling
/// `plan_shadow_casters` and went back to an inline loop. This is a **source-text** assert, not a
/// semantic one — it proves the call is written, not that it is reached — and it is the same
/// technique (with the same caveat) as the pipeline pins in `fog_shader.rs` / `shadow_routing.rs`.
/// It also cannot see a *second*, inlined selection added alongside the call; that gap is stated
/// in this file's header.
#[test]
fn encode_shadow_pass_calls_the_planner_this_file_grades() {
    let pass = PASS_RS
        .split_once("pub fn encode_shadow_pass(")
        .expect("encode_shadow_pass not found in pass.rs")
        .1;
    let body = pass.split_once("\npub fn ").map(|(b, _)| b).unwrap_or(pass);
    assert!(
        body.contains("plan_shadow_casters("),
        "encode_shadow_pass must select its casters via plan_shadow_casters — that call is the \
         only thing making this file's coverage real",
    );
}
