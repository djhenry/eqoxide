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
#[derive(Debug, Clone)]
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
    plan_shadow_casters(None::<&Cand>, nearby, light_center, ORIGIN, VP)
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
/// below. So read this as pinning "ties are not reordered by the current sort", not as the
/// stability guard.
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

    let steps = plan_shadow_casters(None::<&Cand>, std::slice::from_ref(&c), light, ORIGIN, VP);
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
    let steps = plan_shadow_casters(None::<&Cand>, &cands, player, player, VP);
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
#[test]
fn selection_is_bounded_at_exactly_shadow_caster_slots() {
    for (n, expect) in [
        (SHADOW_CASTER_SLOTS - 1, SHADOW_CASTER_SLOTS - 1),
        (SHADOW_CASTER_SLOTS,     SHADOW_CASTER_SLOTS),
        (SHADOW_CASTER_SLOTS + 1, SHADOW_CASTER_SLOTS),
        (SHADOW_CASTER_SLOTS * 3, SHADOW_CASTER_SLOTS),
    ] {
        // Candidate i sits at x = i * 0.5, so index order == nearest-first order.
        let cands: Vec<Cand> =
            (0..n).map(|i| Cand::skinned([i as f32 * 0.5, 0.0, 0.0], 3)).collect();
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
#[test]
fn the_player_takes_the_first_slot_and_shrinks_the_nearby_budget() {
    let player = Cand::playing([0.0, 0.0, 0.0], 5, 2, 0.25);
    let cands: Vec<Cand> =
        (0..SHADOW_CASTER_SLOTS * 2).map(|i| Cand::skinned([i as f32 * 0.5, 0.0, 0.0], 3)).collect();

    let steps = plan_shadow_casters(Some(&player), &cands, ORIGIN, ORIGIN, VP);
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
    let steps = plan_shadow_casters(Some(&player), &[] as &[Cand], ORIGIN, ORIGIN, VP);
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

        let player = plan_shadow_casters(Some(&c), &[] as &[Cand], ORIGIN, ORIGIN, VP);
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

/// Deterministic LCG — a fixed corpus, so a failure is reproducible from the seed alone.
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

/// 400 pseudo-random scenes: mixed model kinds, mixed clip counts (including zero), clip indices
/// on both sides of the bound plus the `usize::MAX` sentinel, populations straddling
/// `SHADOW_CASTER_SLOTS`, and light centers that differ from the player. The extracted planner must
/// emit a byte-identical step stream to the pre-#740 loops in every one.
///
/// Coverage bound, as a rule: this is *sampled*, not exhaustive — it is a differential over a fixed
/// pseudo-random corpus, so it can only catch divergences some scene in that corpus reaches. The
/// four decisions are pinned exhaustively at their boundaries by the tests above; this catches
/// interaction bugs between them.
#[test]
fn extracted_planner_matches_the_pre_740_loops_over_a_random_corpus() {
    let mut rng = Rng(0x5EED_740);
    let mut saw_truncation = 0usize;
    let mut saw_cull = 0usize;
    let mut saw_bind_fallback = 0usize;

    for scene in 0..400 {
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
        let cands: Vec<Cand> = (0..n).map(|_| {
            let pos = [rng.coord(half), rng.coord(half), rng.coord(half)];
            match rng.range(10) {
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
            }
        }).collect();
        let ppos = |rng: &mut Rng| [rng.coord(player_half), rng.coord(player_half), rng.coord(player_half)];
        let player = match rng.range(4) {
            0 => None,
            1 => Some(Cand::statik(ppos(&mut rng))),
            _ => Some(Cand::playing(ppos(&mut rng), 4, rng.range(6) as usize, 0.5)),
        };
        let light = [rng.coord(half), rng.coord(half), rng.coord(half)];
        let player_pos = ppos(&mut rng);

        let got = plan_shadow_casters(player.as_ref(), &cands, light, player_pos, VP);
        let want = old_selection(player.as_ref(), &cands, light, player_pos, VP);
        assert_eq!(
            got, want,
            "scene {} ({} candidates) diverged from the pre-#740 selection loops", scene, n,
        );

        // Corpus-reach accounting: a differential that never reaches the interesting states is a
        // differential over the empty set.
        if got.len() == SHADOW_CASTER_SLOTS { saw_truncation += 1; }
        if got.iter().filter(|s| matches!(s.caster, ShadowCasterRef::Nearby(_))).count() < n {
            saw_cull += 1;
        }
        if got.iter().any(|s| matches!(s.draw, ShadowCasterDraw::Skinned { pose: ShadowPose::BindPose, .. })) {
            saw_bind_fallback += 1;
        }
    }

    assert!(saw_truncation >= 60, "corpus reached the slot bound in only {} scenes", saw_truncation);
    assert!(saw_cull >= 150, "corpus culled in only {} scenes", saw_cull);
    assert!(saw_bind_fallback >= 150, "corpus hit the clip guard in only {} scenes", saw_bind_fallback);
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
