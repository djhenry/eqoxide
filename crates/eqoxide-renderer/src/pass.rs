//! The per-frame render pass. Draws the zone terrain + placed objects, skinned characters (player
//! and NPCs, with equipment-texture swaps), camera-facing billboards/nameplates, and the egui HUD.
//! Reads GPU resources from `EqRenderer` and "what to draw" from `SceneState`. The armor-texture
//! selection + `equip_mesh_hidden` logic here is documented in `docs/equipment-textures-findings.md`.

use crate::renderer::EqRenderer;
use crate::scene::SceneState;

/// Milliseconds since the first call — the clock that drives animated zone textures
/// (fire/water/lava). Process-global so every frame shares one monotonic timeline.
fn anim_now_ms() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

// ── Instanced placed-object shadow-draw planning (#721) ─────────────────────────────────────────
//
// `encode_shadow_pass` used to inline three decisions in its hot loop, none of them reachable from
// a test because every path to them runs through a `wgpu::RenderPass` and this crate's test harness
// has no GPU device (see tests/fog_shader.rs / weather_shader.rs for the established device-free
// precedent):
//
//   1. which of the two instanced shadow pipelines a caster is drawn with, from its `RenderMode`
//      (#707/#718: routing every caster to the fragment-less pipeline makes tree shadows square),
//   2. which animated-texture frame the masked pipeline alpha-tests against, and
//   3. whether the masked sub-pass exists at all.
//
// #718's mutation check confirmed the gap directly: reverting (1), and separately reverting (2),
// each left the whole crate suite green. (1) was re-measured on this branch's merge-base while
// writing #721 — swapping the two `RenderMode` guards there left all 171 `eqoxide-renderer` tests
// passing. Those decisions now live in `plan_instanced_shadow_draws`
// below — a pure function over a device-free view of the casters — and `encode_shadow_pass` is a
// mechanical executor of the plan it returns. The plan is a lazy iterator, NOT a `Vec`: this runs
// every frame the shadow map is rebuilt (renderer.rs calls `encode_shadow_pass` unconditionally per
// frame), and a lazy iterator adds no allocation where the two loops it replaced had none. (Not a
// claim that one `Vec` would be a measurable cost — `encode_shadow_pass` already allocates twice
// per call. It was free to avoid, so it is avoided.) tests/shadow_routing.rs asserts zero
// allocations while draining a plan, with a counting allocator.

/// Which of the two instanced placed-object shadow pipelines draws a caster.
///
/// `Opaque` is the fragment-less `shadow_instanced` pipeline: it writes depth for every triangle at
/// no per-pixel cost, which is correct only for solid geometry. `Masked` is `shadow_instanced_masked`
/// (#718), which has a fragment stage that discards texels under the alpha cutout so alpha-keyed
/// foliage casts its cutout silhouette instead of its bounding rectangle (#707).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShadowPipelineKind {
    Opaque,
    Masked,
}

impl ShadowPipelineKind {
    /// **The #707 routing decision.** `None` means "this render mode casts no instanced shadow at
    /// all" — `Blend`/`Additive` placed objects are excluded, matching the pre-#721 code, which
    /// only ever ran its two loops for `RenderMode::Opaque` and `RenderMode::Masked`.
    pub fn for_render_mode(mode: eqoxide_assets::RenderMode) -> Option<Self> {
        use eqoxide_assets::RenderMode;
        match mode {
            RenderMode::Opaque => Some(Self::Opaque),
            RenderMode::Masked => Some(Self::Masked),
            RenderMode::Blend | RenderMode::Additive => None,
        }
    }
}

/// The instanced sub-passes `encode_shadow_pass` issues, **in draw order**. Owning the list here
/// rather than as two hand-written loops is what makes "the masked sub-pass got deleted" a single
/// testable edit instead of an invisible one.
pub const INSTANCED_SHADOW_SUBPASSES: [ShadowPipelineKind; 2] =
    [ShadowPipelineKind::Opaque, ShadowPipelineKind::Masked];

/// What a planned step needs to do to bind group 1 (the caster's diffuse texture) before it draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowTexBind {
    /// This sub-pass samples nothing (the opaque shadow pipeline has no fragment stage), so group 1
    /// is never bound for it.
    NotSampled,
    /// Group 1 already holds the right texture from an earlier step in this sub-pass — skip the
    /// redundant `set_bind_group`.
    Keep,
    /// Bind group 1 to this texture index (`None` → the renderer's fallback texture).
    Set(Option<usize>),
}

/// One planned instanced placed-object shadow draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstancedShadowStep {
    /// Index into the caster slice the plan was built from.
    pub caster: usize,
    /// Which pipeline draws it.
    pub pipeline: ShadowPipelineKind,
    /// True on the first step of each non-empty sub-pass: set the pipeline and bind group 0.
    pub set_pipeline: bool,
    /// Group-1 action for this step.
    pub bind: ShadowTexBind,
}

/// The device-free slice of a caster the shadow plan actually reads. Implemented for
/// [`crate::gpu::GpuInstancedMesh`] (whose other fields are `wgpu::Buffer`s, hence unconstructible
/// in a test) and for plain structs in tests.
pub trait InstancedShadowCaster {
    fn render_mode(&self) -> eqoxide_assets::RenderMode;
    fn texture_idx(&self) -> Option<usize>;
    fn anim(&self) -> Option<&(u32, Vec<usize>)>;
}

impl InstancedShadowCaster for crate::gpu::GpuInstancedMesh {
    fn render_mode(&self) -> eqoxide_assets::RenderMode { self.render_mode }
    fn texture_idx(&self) -> Option<usize> { self.texture_idx }
    fn anim(&self) -> Option<&(u32, Vec<usize>)> { self.anim.as_ref() }
}

/// The texture a mesh should bind at `now_ms`: its current animation frame if animated, else its
/// static texture. **The single definition** — both the colour pass and the masked shadow sub-pass
/// reach it through their planners ([`plan_zone_draws`] and [`plan_instanced_shadow_draws`]), so
/// they cannot drift apart, and this function's tests grade both. (Until #741 the colour side went
/// through a `frame_tex` closure in `encode_zone_pass` that delegated here; that closure is gone.)
///
/// The masked shadow sub-pass needs this, not the raw `texture_idx`: an animated `RenderMode::Masked`
/// caster's shadow must alpha-test against the SAME texel the color pass is sampling, or the two
/// silhouettes disagree on every frame but the first — the identical failure class as #707, just
/// triggered by time instead of by a missing discard.
pub fn animated_frame_texture(
    tex:    Option<usize>,
    anim:   Option<&(u32, Vec<usize>)>,
    now_ms: u64,
) -> Option<usize> {
    match anim {
        Some((ms, frames)) if !frames.is_empty() => {
            Some(frames[(now_ms / (*ms).max(1) as u64) as usize % frames.len()])
        }
        _ => tex,
    }
}

/// Plans every instanced placed-object shadow draw for one frame: which sub-passes run, in which
/// order, which caster is in which, and which texture each masked step must bind.
///
/// Returns a **lazy iterator** — no allocation, no `Vec` — because `encode_shadow_pass` runs every
/// frame. Steps come out in exactly the order `encode_shadow_pass` issues them, so a test can
/// collect them and grade the full draw sequence without a GPU device.
pub fn plan_instanced_shadow_draws<C: InstancedShadowCaster>(
    casters: &[C],
    now_ms:  u64,
) -> impl Iterator<Item = InstancedShadowStep> + '_ {
    INSTANCED_SHADOW_SUBPASSES.into_iter().flat_map(move |pipeline| {
        casters
            .iter()
            .enumerate()
            .filter(move |(_, c)| {
                ShadowPipelineKind::for_render_mode(c.render_mode()) == Some(pipeline)
            })
            .scan(
                (false, None::<Option<usize>>),
                move |(started, current_tex), (caster, c)| {
                    let bind = match pipeline {
                        ShadowPipelineKind::Opaque => ShadowTexBind::NotSampled,
                        ShadowPipelineKind::Masked => {
                            let tex = animated_frame_texture(c.texture_idx(), c.anim(), now_ms);
                            if *current_tex == Some(tex) {
                                ShadowTexBind::Keep
                            } else {
                                *current_tex = Some(tex);
                                ShadowTexBind::Set(tex)
                            }
                        }
                    };
                    let set_pipeline = !*started;
                    *started = true;
                    Some(InstancedShadowStep { caster, pipeline, set_pipeline, bind })
                },
            )
    })
}

/// The device-bound half of an instanced placed-object shadow draw, expressed entirely in **plan
/// vocabulary** — [`ShadowPipelineKind`]s and texture *indices*, never `wgpu` handles.
///
/// This is what makes [`execute_instanced_shadow_plan`] device-free: `encode_shadow_pass` supplies
/// an impl that closes over the renderer and the live `wgpu::RenderPass`, and
/// `tests/shadow_routing_equivalence.rs` supplies one that appends to a `Vec`. Both run the *same*
/// executor, so a regression in the plan→command translation is a test failure rather than
/// something only a GPU could notice.
///
/// The four methods are deliberately the *only* handles the executor has. In particular
/// [`Self::bind_texture`] receives an index and nothing else — it has no caster in scope, so the
/// #718 N2 bug ("bind `mesh.texture_idx` instead of the resolved animation frame") is not
/// expressible there at all.
pub trait InstancedShadowSink {
    /// Start a sub-pass: select the pipeline this kind names.
    fn set_pipeline(&mut self, kind: ShadowPipelineKind);
    /// Bind group 0 (the shadow light's view-projection uniform).
    fn bind_light_depth(&mut self);
    /// Bind group 1 to the *already-resolved* texture index (`None` → fallback texture).
    fn bind_texture(&mut self, idx: Option<usize>);
    /// Issue the draw for `casters[caster]`.
    fn draw(&mut self, caster: usize);
}

/// Turns a [`plan_instanced_shadow_draws`] plan into sink calls. **This is the real executor** —
/// `encode_shadow_pass` calls exactly this function, so the command sequence it emits is graded
/// device-free in `tests/shadow_routing_equivalence.rs` against a transcription of the pre-#721
/// loops.
///
/// Before #721's review round 2 this loop was inlined into `encode_shadow_pass` and therefore
/// unobservable: the reviewer reintroduced #718's N2 bug in it (`tex_bg(mesh.texture_idx)` for
/// `tex_bg(tex)`) and all 14 tests stayed green. Keep it here; do not inline it back.
pub fn execute_instanced_shadow_plan<C: InstancedShadowCaster, S: InstancedShadowSink>(
    casters: &[C],
    now_ms:  u64,
    sink:    &mut S,
) {
    for step in plan_instanced_shadow_draws(casters, now_ms) {
        if step.set_pipeline {
            sink.set_pipeline(step.pipeline);
            sink.bind_light_depth();
        }
        if let ShadowTexBind::Set(tex) = step.bind {
            sink.bind_texture(tex);
        }
        sink.draw(step.caster);
    }
}

// ── Character shadow sub-pass ROUTING (#739) ─────────────────────────────────────────────────────
//
// After the instanced placed objects, `encode_shadow_pass` draws the selected characters in two
// sub-passes: skinned first, then static. Each sets its pipeline and binds group 0 once, then binds
// each caster's `shadow_uniform_pool` slot at group 1 — and, for skinned casters ONLY, that caster's
// `shadow_joint_pool` slot at group 2. Those were two hand-written loops with a `bool` latch each,
// and nothing in the suite could see them: a flipped pipeline, a dropped sub-pass, or a static
// caster that bound a joint palette (or a skinned one that did not) was invisible.
//
// #739 asked explicitly whether this can share #721's vocabulary. **It cannot, and the planners are
// deliberately separate.** `ShadowPipelineKind` names the two INSTANCED pipelines and is chosen from
// a mesh's `RenderMode`; these two are chosen from whether a *model* is skinned. `ShadowTexBind`
// names a diffuse-texture bind; neither sub-pass here samples a texture, and what they bind instead
// is a pool slot index. The only shared shape is "one latch per sub-pass", which is three lines.

/// Which of the two character shadow pipelines draws a selected caster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CharacterShadowKind {
    /// `pipelines.shadow_skinned` — reads a joint palette at group 2.
    Skinned,
    /// `pipelines.shadow_static` — no joint palette.
    Static,
}

/// The character shadow sub-passes `encode_shadow_pass` issues, **in draw order**. Owning the list
/// here rather than as two hand-written loops is what makes "the static sub-pass got deleted" a
/// single testable edit.
pub const CHARACTER_SHADOW_SUBPASSES: [CharacterShadowKind; 2] =
    [CharacterShadowKind::Skinned, CharacterShadowKind::Static];

/// What a caster binds before it draws.
///
/// **The joint palette is inside the `Skinned` variant, which is the point.** A static caster
/// cannot carry a `shadow_joint_pool` slot — the bad state is unrepresentable rather than merely
/// untaken, so no test is needed to keep a joint index out of a static step. What this does NOT
/// prevent: a [`CharacterShadowCaster`] impl reporting the wrong variant for its model, and a
/// [`CharacterShadowSink`] impl binding group 2 anyway. The first is graded differentially in
/// tests/character_shadow_routing.rs; the second is `encode_shadow_pass`'s four-line sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharacterShadowBind {
    Skinned { u_slot: usize, j_slot: usize },
    Static  { u_slot: usize },
}

impl CharacterShadowBind {
    pub fn kind(self) -> CharacterShadowKind {
        match self {
            Self::Skinned { .. } => CharacterShadowKind::Skinned,
            Self::Static  { .. } => CharacterShadowKind::Static,
        }
    }
}

/// One planned character shadow draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharacterShadowStep {
    /// Index into the caster slice the plan was built from.
    pub caster:       usize,
    /// True on the first step of each non-empty sub-pass: set the pipeline and bind group 0.
    pub set_pipeline: bool,
    pub bind:         CharacterShadowBind,
}

/// The device-free slice of a selected shadow caster the ROUTING reads. The real caster owns a
/// `&GpuSkinnedModel`/`&GpuStaticModel` (both unconstructible in a test — they own `wgpu::Buffer`s);
/// routing only ever asks which pipeline draws it and which pool slots it binds.
pub trait CharacterShadowCaster {
    fn shadow_bind(&self) -> CharacterShadowBind;
}

/// Plans every character shadow draw for one frame: which sub-passes run, in which order, which
/// caster is in which, and what each binds.
///
/// Returns a **lazy iterator** — no allocation — because `encode_shadow_pass` runs every frame.
/// Steps come out in exactly the order `encode_shadow_pass` issues them, so the sub-pass split is
/// observable: a caster's position in this stream is its position in its sub-pass, not its position
/// in `casters`.
pub fn plan_character_shadow_draws<C: CharacterShadowCaster>(
    casters: &[C],
) -> impl Iterator<Item = CharacterShadowStep> + '_ {
    CHARACTER_SHADOW_SUBPASSES.into_iter().flat_map(move |kind| {
        casters
            .iter()
            .enumerate()
            .filter(move |(_, c)| c.shadow_bind().kind() == kind)
            .scan(false, move |started, (caster, c)| {
                let set_pipeline = !*started;
                *started = true;
                Some(CharacterShadowStep { caster, set_pipeline, bind: c.shadow_bind() })
            })
    })
}

/// The device-bound half of a character shadow draw, expressed entirely in **plan vocabulary** —
/// a [`CharacterShadowKind`] and pool slot *indices*, never `wgpu` handles.
///
/// `encode_shadow_pass` supplies an impl closing over the renderer and the live `wgpu::RenderPass`;
/// `tests/character_shadow_routing.rs` supplies one that appends to a `Vec`. Both run the *same*
/// executor.
pub trait CharacterShadowSink {
    /// Start a sub-pass: select the pipeline this kind names.
    fn set_pipeline(&mut self, kind: CharacterShadowKind);
    /// Bind group 0 (the shadow light's view-projection uniform).
    fn bind_light_depth(&mut self);
    /// Bind group 1 to `shadow_uniform_pool[u_slot]` (this caster's model matrix).
    fn bind_model_uniform(&mut self, u_slot: usize);
    /// Bind group 2 to `shadow_joint_pool[j_slot]`. Called for skinned casters only.
    fn bind_joints(&mut self, j_slot: usize);
    /// Issue the draws for `casters[caster]` (one per mesh of its model).
    fn draw(&mut self, caster: usize);
}

/// Turns a [`plan_character_shadow_draws`] plan into sink calls. **This is the real executor** —
/// `encode_shadow_pass` calls exactly this function, so the command sequence it emits is graded
/// device-free in `tests/character_shadow_routing.rs` against a transcription of the pre-#739 loops.
pub fn execute_character_shadow_plan<C: CharacterShadowCaster, S: CharacterShadowSink>(
    casters: &[C],
    sink:    &mut S,
) {
    for step in plan_character_shadow_draws(casters) {
        if step.set_pipeline {
            sink.set_pipeline(step.bind.kind());
            sink.bind_light_depth();
        }
        match step.bind {
            CharacterShadowBind::Skinned { u_slot, j_slot } => {
                sink.bind_model_uniform(u_slot);
                sink.bind_joints(j_slot);
            }
            CharacterShadowBind::Static { u_slot } => {
                sink.bind_model_uniform(u_slot);
            }
        }
        sink.draw(step.caster);
    }
}

/// Entity draw distance (EQ units, measured from the player). Beyond this an NPC's 3D
/// model is not drawn — it's a distant speck. Combined with a frustum test, this caps
/// the per-frame entity work in densely-populated zones (e.g. gfaydark, ~400 spawns).
pub const ENTITY_DRAW_DIST: f32 = 500.0;
/// NDC slack for the frustum test so a tall model whose feet sit just off-screen still
/// draws (the culled position is the feet; the body extends upward). Shared with the HUD so
/// nameplates cull on the exact same distance+frustum test as models (#177).
pub const ENTITY_CULL_MARGIN: f32 = 0.5;

// ── Shadow caster SELECTION (#740) ───────────────────────────────────────────────────────────────
//
// `encode_shadow_pass` decides, per frame, which entities make it into the shadow map: the player,
// then the nearby characters nearest-first, culled to the view frustum and bounded by
// `SHADOW_CASTER_SLOTS`, each posed either at its current animation clip or at bind pose. Those
// four decisions were inline in the pass until #740 and therefore unreachable from any test:
// `encode_shadow_pass` takes `&EqRenderer` + `&mut wgpu::CommandEncoder`, and neither
// `wgpu::Device` nor `wgpu::Queue` implements `Default` or has any non-adapter constructor
// (wgpu 22 has no `noop` backend), so an integration test cannot build the arguments at all.
// The extraction below is the same shape #721 used for the instanced draws: a pure planner over a
// device-free trait, graded in tests/shadow_caster_selection.rs.

/// The model resolved for a shadow-caster candidate, reduced to what SELECTION reads. The real
/// `GpuModel` is unconstructible in a test (it owns `wgpu::Buffer`s); selection only ever asks
/// "skinned or static, and how many clips does the skin have".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowModelKind {
    Skinned { clip_count: usize },
    Static,
    /// No model loaded for this race/gender — the candidate casts nothing and consumes no slot.
    Absent,
}

/// How a selected skinned caster is posed for the depth pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShadowPose {
    /// Evaluate the skin at this clip/time.
    Clip { idx: usize, time: f32 },
    /// Fall back to the rest pose. **This is the #692/#694 guard's output**: the bind-pose sentinel
    /// is `clip_idx = usize::MAX`, and a `clip_idx` carried over from a different model can exceed
    /// the current skin's clip count, so an unguarded `evaluate` would index out of range.
    BindPose,
}

/// What a selected caster draws as. Static casters need no joint palette, so they consume a
/// `shadow_uniform_pool` slot but not a `shadow_joint_pool` slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShadowCasterDraw {
    Skinned { j_slot: usize, pose: ShadowPose },
    Static,
}

/// Which candidate a step selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowCasterRef {
    /// The local player (entity id 0), selected before the nearby loop and never frustum-culled.
    Player,
    /// Index into the `nearby` slice the plan was built from — NOT the post-sort position, so the
    /// caller can index its own unsorted data with it.
    Nearby(usize),
}

/// One selected shadow caster, in the order `encode_shadow_pass` writes and draws it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowCasterStep {
    pub caster: ShadowCasterRef,
    /// `shadow_uniform_pool` index for this caster's model matrix.
    pub u_slot: usize,
    pub draw:   ShadowCasterDraw,
}

/// What the planner **looked at** on its way to a plan, reported by the planner itself.
///
/// Nothing in `encode_shadow_pass` reads this. It exists so the coverage corpus in
/// tests/shadow_caster_selection.rs can *observe* reach instead of re-deriving it, and it is here
/// rather than behind `#[cfg(test)]` because an integration test links the crate as a dependency
/// and cannot see `cfg(test)` items.
///
/// **Why it exists at all**, since a reader will otherwise reasonably ask why a renderer returns
/// telemetry it ignores. Two of the corpus's coverage counters need to know which candidates the
/// loop examined, and the plan alone cannot show that: a culled candidate, an `Absent` one, and a
/// candidate the loop never reached are all three no-ops, identical in `steps`. #747 shipped two
/// wrong answers to that before this one — reach computed from the fixture (round 2), then reach
/// reconstructed from a second copy of the planner's own "stop at the bound" rule (round 3), whose
/// self-check was a *subsequence* test and so constrained the ordering while placing no constraint
/// whatsoever on the cut. The round-3 review falsified it by measurement: capping the nearby loop
/// at 100 examined candidates lost real reach in 85 of 400 scenes and moved **not one counter**.
/// Reported from inside the loop, the cut has exactly one source and there is nothing to drift.
///
/// The same argument covers the two `Absent` arms, which emit no step: an empty match arm is
/// indistinguishable from a dead one, so a `continue` inserted before it would silently zero that
/// cell's coverage. Counting inside the arm is what makes the arm observable.
///
/// `nearby_examined == nearby_culled + nearby_absent + nearby_static + nearby_skinned` holds by
/// construction, and the corpus asserts that identity on every scene — so a misplaced or duplicated
/// increment here reports as a broken counter rather than as quietly wrong coverage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShadowCasterReach {
    /// Nearby candidates whose loop body ran — i.e. those reached before the slot bound stopped the
    /// loop. Counted at the top of the body, before the cull.
    pub nearby_examined: usize,
    /// …of those, rejected by `entity_in_view`.
    pub nearby_culled: usize,
    /// …of those that survived the cull, how many landed in each `ShadowModelKind` arm.
    /// `nearby_skinned` is counted on entry to the arm, before the (documented-dead) `j_slot`
    /// guard, so the identity above holds regardless of whether that guard ever fires.
    pub nearby_absent: usize,
    pub nearby_static: usize,
    pub nearby_skinned: usize,
    /// A player was supplied and took the player path's `Absent` arm — also step-less, and
    /// otherwise inferable only as "supplied but missing from the plan".
    pub player_absent: bool,
}

/// A frame's caster selection: the steps to draw, plus what the planner examined to produce them.
///
/// Only `steps` drives rendering; see [`ShadowCasterReach`] for why `reach` is here.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShadowCasterPlan {
    pub steps: Vec<ShadowCasterStep>,
    pub reach: ShadowCasterReach,
}

/// The device-free slice of a shadow-caster candidate that SELECTION reads. `encode_shadow_pass`
/// implements it over `(&Billboard, Option<&GpuModel>)`; tests implement it on a plain struct.
pub trait ShadowCasterCandidate {
    /// World position — the sort key (distance to the light center) and the frustum-cull input.
    fn pos(&self) -> [f32; 3];
    fn model_kind(&self) -> ShadowModelKind;
    /// This entity's animation state as `(clip_idx, time)`, or `None` if it has none.
    fn anim_state(&self) -> Option<(usize, f32)>;
}

/// **The #692/#694 guard.** Picks the pose for a skinned caster: its live clip if that clip index
/// is actually in range for *this* skin, otherwise the rest pose.
///
/// `clip_count != 0` is redundant with `idx < clip_count` (no `usize` is less than 0) and is kept
/// only because the pass wrote both halves; `redundant_is_empty_half_of_the_guard_is_provably_dead`
/// in tests/shadow_caster_selection.rs pins that redundancy rather than leaving it as folklore.
pub fn shadow_pose_for(anim: Option<(usize, f32)>, clip_count: usize) -> ShadowPose {
    match anim {
        Some((idx, time)) if clip_count != 0 && idx < clip_count => ShadowPose::Clip { idx, time },
        _ => ShadowPose::BindPose,
    }
}

/// Selects the frame's shadow casters: player first, then the nearby candidates **nearest-first to
/// `light_center`**, dropping anything outside the entity frustum/distance cull, until
/// `SHADOW_CASTER_SLOTS` uniform slots are spent.
///
/// Returns steps in write/draw order with their pool slots already assigned, so the pass has no
/// selection decision left to make, alongside a [`ShadowCasterReach`] the pass ignores and the
/// coverage corpus reads. Allocates: the nearest-first sort needs an owned index order (unlike
/// [`plan_instanced_shadow_draws`], which is lazy).
///
/// `model_kind()` is called only for candidates that survive the cull *within this function*.
///
/// **Do not read that as a statement about the pass.** The production call site
/// ([`encode_shadow_pass`]) resolves every billboard's `GpuModel` eagerly into its `CandidateView`s
/// *before* calling this, so the pass's order of work does differ from pre-#740 — that is the one
/// disclosed behaviour-neutral delta (#740 §2). An earlier draft of this comment claimed the order
/// of work "matches the pre-#740 order"; it does not, and the claim is retracted here rather than
/// deleted.
pub fn plan_shadow_casters<C: ShadowCasterCandidate>(
    player:       Option<&C>,
    nearby:       &[C],
    light_center: [f32; 3],
    player_pos:   [f32; 3],
    view_proj:    [[f32; 4]; 4],
) -> ShadowCasterPlan {
    use crate::renderer::SHADOW_CASTER_SLOTS;

    let mut steps: Vec<ShadowCasterStep> = Vec::new();
    let mut reach = ShadowCasterReach::default();
    let mut u_slot = 0usize;
    let mut j_slot = 0usize;

    // ── Player (id 0): always selected when it has a model; no cull, no bound (it is first). ──
    if let Some(p) = player {
        match p.model_kind() {
            ShadowModelKind::Skinned { clip_count } => {
                let pose = shadow_pose_for(p.anim_state(), clip_count);
                steps.push(ShadowCasterStep {
                    caster: ShadowCasterRef::Player,
                    u_slot,
                    draw: ShadowCasterDraw::Skinned { j_slot, pose },
                });
                u_slot += 1;
                j_slot += 1;
            }
            ShadowModelKind::Static => {
                steps.push(ShadowCasterStep {
                    caster: ShadowCasterRef::Player,
                    u_slot,
                    draw: ShadowCasterDraw::Static,
                });
                u_slot += 1;
            }
            // Counted, not empty: an empty arm cannot be told apart from a dead one. NOTE the
            // counter is `reach.player_absent`, NOT `u_slot` — an absent player must consume no
            // slot (`a_player_with_no_model_casts_nothing_and_consumes_no_slot`).
            ShadowModelKind::Absent => reach.player_absent = true,
        }
    }

    // ── Nearby characters, nearest-first to the light center, bounded ────────────────────────
    let dist2 = |p: [f32; 3]| {
        (p[0] - light_center[0]).powi(2)
            + (p[1] - light_center[1]).powi(2)
            + (p[2] - light_center[2]).powi(2)
    };
    let mut order: Vec<usize> = (0..nearby.len()).collect();
    // Stable, like the pre-#740 `Vec<&Billboard>` sort: equidistant candidates keep spawn order.
    order.sort_by(|&a, &b| {
        dist2(nearby[a].pos())
            .partial_cmp(&dist2(nearby[b].pos()))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for i in order {
        if u_slot >= SHADOW_CASTER_SLOTS {
            break;
        }
        // The one place the cut is observable. Everything after the `break` above is unexamined,
        // and nothing in `steps` distinguishes "culled", "absent" and "never looked at".
        reach.nearby_examined += 1;
        let c = &nearby[i];
        if !crate::camera::entity_in_view(
            c.pos(), player_pos, view_proj, ENTITY_DRAW_DIST, ENTITY_CULL_MARGIN,
        ) {
            reach.nearby_culled += 1;
            continue;
        }
        match c.model_kind() {
            ShadowModelKind::Skinned { clip_count } => {
                // Counted before the guard below, so `examined == culled + absent + static +
                // skinned` holds whether or not the guard fires.
                reach.nearby_skinned += 1;
                // Unreachable in practice (`j_slot <= u_slot` always, and `u_slot <
                // SHADOW_CASTER_SLOTS` was just checked), kept because the pre-#740 pass wrote it.
                // `joint_slots_never_outrun_uniform_slots` pins the invariant that makes it dead.
                if j_slot >= SHADOW_CASTER_SLOTS {
                    continue;
                }
                let pose = shadow_pose_for(c.anim_state(), clip_count);
                steps.push(ShadowCasterStep {
                    caster: ShadowCasterRef::Nearby(i),
                    u_slot,
                    draw: ShadowCasterDraw::Skinned { j_slot, pose },
                });
                u_slot += 1;
                j_slot += 1;
            }
            ShadowModelKind::Static => {
                reach.nearby_static += 1;
                steps.push(ShadowCasterStep {
                    caster: ShadowCasterRef::Nearby(i),
                    u_slot,
                    draw: ShadowCasterDraw::Static,
                });
                u_slot += 1;
            }
            // Counted, not empty — same reason as the player arm above. NOTE the counter is
            // `reach.nearby_absent`, NOT `u_slot`: an absent candidate consumes no slot.
            ShadowModelKind::Absent => reach.nearby_absent += 1,
        }
    }

    ShadowCasterPlan { steps, reach }
}

/// Vestigial: this used to HIDE an armor mesh whose exact material+variant texture was
/// missing (e.g. the variant-03 main chest torso for an armor material that only ships
/// variants 01/02). But the chest variant pieces are DISJOINT (zero shared verts), so
/// hiding the textureless torso left a see-through hole (a "transparent chest") rather than
/// revealing a sibling. `resolve_overlay_tex` now falls back to the material-0 base cloth
/// for such pieces, so nothing ever needs hiding. Kept as a no-op so the call sites in the
/// two-pass body draw stay readable; always returns false.
fn equip_mesh_hidden(
    _r: &EqRenderer, _prefix: &str,
    _slot: Option<crate::models::EquipSlot>, _equipment: &[u32; 9],
) -> bool {
    false
}

fn resolve_equip_tex<'a>(
    r:          &'a EqRenderer,
    baked_bgs:  &'a [wgpu::BindGroup],
    baked_idx:  Option<usize>,
    prefix:     &str,
    slot:       Option<crate::models::EquipSlot>,
    equipment:  &[u32; 9],
) -> &'a wgpu::BindGroup {
    if let Some(es) = slot {
        let mat = equipment[es.slot];
        // equip_swap_key returns None for material 0 (naked → baked texture) / no prefix.
        if let Some(key) = crate::models::equip_swap_key(prefix, es.clone(), mat) {
            if let Some(Some(bg)) = r.equipment_tex_cache.get(&key) {
                return bg;
            }
        }
        // Velious-range (17-23) fallback: the raw racial texture (e.g. elflg2301) often doesn't
        // exist, so remap to the classic base tier (e.g. 23 → 1 leather) like the original client.
        if let Some(rmat) = crate::models::velious_material_fallback(mat) {
            if let Some(key) = crate::models::equip_swap_key(prefix, es, rmat) {
                if let Some(Some(bg)) = r.equipment_tex_cache.get(&key) {
                    return bg;
                }
            }
        }
    }
    match baked_idx {
        Some(i) if i < baked_bgs.len() => &baked_bgs[i],
        _ => &r.fallback_texture_bg,
    }
}

/// Skin-base bind group for a body mesh: the model's own baked texture (the Luclin skin layer the
/// WLD material palette references by default), or the white fallback if the mesh has none.
fn skin_base_tex<'a>(
    r: &'a EqRenderer, baked_bgs: &'a [wgpu::BindGroup], baked_idx: Option<usize>,
) -> &'a wgpu::BindGroup {
    match baked_idx {
        Some(i) if i < baked_bgs.len() => &baked_bgs[i],
        _ => &r.fallback_texture_bg,
    }
}

/// The cloth/armor OVERLAY bind group for a body slot, if a usable swapped texture is cached.
/// Unlike `resolve_equip_tex`, this returns `None` (rather than the baked skin) when there is no
/// overlay — material-0 skin regions, rejected transparent stubs, and missing textures. The
/// two-pass renderer draws the skin base first, then this overlay alpha-blended on top, so a
/// `None` here means bare skin shows (e.g. the elf-female exposed midriff).
fn resolve_overlay_tex<'a>(
    r: &'a EqRenderer, prefix: &str,
    slot: Option<crate::models::EquipSlot>, equipment: &[u32; 9],
) -> Option<&'a wgpu::BindGroup> {
    let es = slot?;
    let mat = equipment[es.slot];
    if let Some(key) = crate::models::equip_swap_key(prefix, es.clone(), mat) {
        if let Some(Some(bg)) = r.equipment_tex_cache.get(&key) { return Some(bg); }
    }
    if let Some(rmat) = crate::models::velious_material_fallback(mat) {
        if let Some(key) = crate::models::equip_swap_key(prefix, es.clone(), rmat) {
            if let Some(Some(bg)) = r.equipment_tex_cache.get(&key) { return Some(bg); }
        }
    }
    // Base-cloth fallback: a body region whose armor material lacks a texture for THIS
    // variant stays clothed instead of vanishing. The chest's disjoint variant pieces
    // don't all ship per material (e.g. material 3 has chest variants 01/02 but not the
    // main 03 torso), so without this the textureless piece would be hidden into a
    // see-through hole. Skin regions (he/hn/ft) return None at material 0 → bare skin.
    if mat != 0 {
        if let Some(key) = crate::models::equip_swap_key(prefix, es, 0) {
            if let Some(Some(bg)) = r.equipment_tex_cache.get(&key) { return Some(bg); }
        }
    }
    // Final base-cloth fallback: a BODY piece whose exact variant's baseline-cloth texture was
    // never extracted still renders clothed by borrowing variant-01 of the same region. The male
    // equip-texture sets are incomplete (e.g. wood-elf male ships elmch0001/0002 but NOT elmch0003,
    // and only variant-01 of the arms) where the female set is complete — so without this, male
    // humanoids rendered bare on exactly those pieces while females looked fine. Skin regions
    // (he/hn/ft) are excluded so bare hands/head/feet stay skin. (eqoxide#82)
    if !matches!(&es.region, b"he" | b"hn" | b"ft") && es.variant != 1 {
        let base = crate::models::EquipSlot { variant: 1, ..es };
        if let Some(key) = crate::models::equip_swap_key(prefix, base, 0) {
            if let Some(Some(bg)) = r.equipment_tex_cache.get(&key) { return Some(bg); }
        }
    }
    None
}

/// Sky gradient background pass. MUST be called before all other passes.
/// Fills the color buffer with the gradient; subsequent passes draw on top.
/// No depth attachment — sky is purely a background layer.
pub fn encode_sky_pass(
    r:       &EqRenderer,
    encoder: &mut wgpu::CommandEncoder,
    view:    &wgpu::TextureView,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("sky"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load:  wgpu::LoadOp::Clear(wgpu::Color { r: 0.74, g: 0.86, b: 0.97, a: 1.0 }),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.sky);
    // Time-of-day gradient colors (eqoxide#561), written to this buffer earlier in the frame.
    pass.set_bind_group(0, &r.sky_uniform.bind_group, &[]);
    pass.draw(0..6, 0..1);
}

// ── Zone colour pass ROUTING (#741) ──────────────────────────────────────────────────────────────
//
// `encode_zone_pass` issues six sub-passes: {static, instanced} × {opaque+masked, blend, additive}.
// Which mesh list each reads, which pipeline it selects, which render modes it accepts, the order
// they run in, and when group 1 is rebound were two hand-written closures called six times, inside a
// function that takes `&EqRenderer` + `&mut wgpu::CommandEncoder` — arguments no test can build
// (neither `wgpu::Device` nor `wgpu::Queue` has a non-adapter constructor, and wgpu 22 has no `noop`
// backend). A flipped source, a dropped sub-pass or a widened mode filter was therefore invisible.
// The split below is the same shape #721 used for the instanced shadow draws: a pure planner over a
// device-free trait plus a sink for the `wgpu` handles, graded in tests/zone_pass_routing.rs.

/// Which mesh list a zone sub-pass draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZoneMeshSource {
    /// `EqRenderer::gpu_meshes` — one draw per mesh, one instance.
    Static,
    /// `EqRenderer::gpu_instanced` — one draw per mesh over `instance_count` instances.
    Instanced,
}

/// The blend class of a zone sub-pass. **This is the mode filter**: a sub-pass does not carry a
/// free-form list of [`eqoxide_assets::RenderMode`]s, so "the opaque sub-pass silently started
/// accepting `Blend`" is not expressible in the table below — only in this one function.
///
/// `Opaque` and `Masked` share a class because they share a pipeline: masked discards in-shader with
/// depth-write still on, so both belong in the depth-writing prepass. `Blend` and `Additive` run
/// after, each with its own depth-write-off pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZoneBlendClass {
    OpaqueMasked,
    Blend,
    Additive,
}

impl ZoneBlendClass {
    /// Whether a mesh in this render mode is drawn by this class of sub-pass. Every mode is accepted
    /// by exactly one class — a mesh is drawn once, never twice and never zero times, which is what
    /// `zone_pass_routing.rs::every_render_mode_is_drawn_exactly_once` grades.
    pub fn accepts(self, mode: eqoxide_assets::RenderMode) -> bool {
        use eqoxide_assets::RenderMode;
        matches!(
            (self, mode),
            (Self::OpaqueMasked, RenderMode::Opaque | RenderMode::Masked)
                | (Self::Blend, RenderMode::Blend)
                | (Self::Additive, RenderMode::Additive)
        )
    }
}

/// One zone sub-pass: a mesh list and a blend class. The pipeline is a pure function of the pair
/// (`ZoneDrawSink::set_pipeline`'s six arms), so there is no third field to get wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ZoneSubpass {
    pub source: ZoneMeshSource,
    pub class:  ZoneBlendClass,
}

/// The zone sub-passes, **in draw order**. Owning the order here rather than as six call statements
/// is what makes "the additive instanced sub-pass got dropped" a single testable edit.
///
/// The order matters twice over: all depth-writing geometry must precede all depth-write-off
/// geometry, and within a class the static terrain is drawn before the placed objects standing on
/// it.
pub const ZONE_SUBPASSES: [ZoneSubpass; 6] = [
    ZoneSubpass { source: ZoneMeshSource::Static,    class: ZoneBlendClass::OpaqueMasked },
    ZoneSubpass { source: ZoneMeshSource::Instanced, class: ZoneBlendClass::OpaqueMasked },
    ZoneSubpass { source: ZoneMeshSource::Static,    class: ZoneBlendClass::Blend },
    ZoneSubpass { source: ZoneMeshSource::Instanced, class: ZoneBlendClass::Blend },
    ZoneSubpass { source: ZoneMeshSource::Static,    class: ZoneBlendClass::Additive },
    ZoneSubpass { source: ZoneMeshSource::Instanced, class: ZoneBlendClass::Additive },
];

/// Group-1 action for a planned zone draw. There is no `NotSampled` arm (unlike
/// [`ShadowTexBind`]): every zone pipeline has a fragment stage and samples its diffuse texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneTexBind {
    /// Bind group 1 to this texture index (`None` → the renderer's fallback texture).
    Set(Option<usize>),
    /// Group 1 already holds the right texture from an earlier step **in this sub-pass** — skip the
    /// redundant `set_bind_group`. The cache does not survive a sub-pass boundary.
    Keep,
}

/// One planned zone draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneDrawStep {
    /// Index into [`ZONE_SUBPASSES`].
    pub subpass:      usize,
    pub source:       ZoneMeshSource,
    pub class:        ZoneBlendClass,
    /// Index into the mesh list named by `source` — NOT a position within the sub-pass.
    pub mesh:         usize,
    /// True on the first step of each non-empty sub-pass: set the pipeline and bind groups 0 and 2.
    pub set_pipeline: bool,
    pub bind:         ZoneTexBind,
}

/// The device-free slice of a mesh the zone pass reads.
///
/// Structurally identical to [`InstancedShadowCaster`], and deliberately NOT the same trait: that
/// one names the instanced *shadow* casters and is implemented only for
/// [`crate::gpu::GpuInstancedMesh`], whereas this is implemented for both zone mesh lists. Unifying
/// them would rename #721's public trait for no coverage gain, which is out of scope for a coverage
/// issue.
pub trait ZoneDrawMesh {
    fn render_mode(&self) -> eqoxide_assets::RenderMode;
    fn texture_idx(&self) -> Option<usize>;
    fn anim(&self) -> Option<&(u32, Vec<usize>)>;
}

impl ZoneDrawMesh for crate::gpu::GpuMesh {
    fn render_mode(&self) -> eqoxide_assets::RenderMode { self.render_mode }
    fn texture_idx(&self) -> Option<usize> { self.texture_idx }
    fn anim(&self) -> Option<&(u32, Vec<usize>)> { self.anim.as_ref() }
}

impl ZoneDrawMesh for crate::gpu::GpuInstancedMesh {
    fn render_mode(&self) -> eqoxide_assets::RenderMode { self.render_mode }
    fn texture_idx(&self) -> Option<usize> { self.texture_idx }
    fn anim(&self) -> Option<&(u32, Vec<usize>)> { self.anim.as_ref() }
}

/// Plans every zone colour draw for one frame: which sub-passes run, in which order, which mesh is
/// in which, and which texture each step must bind.
///
/// Returns a **lazy iterator** — no allocation — because `encode_zone_pass` runs every frame. Steps
/// come out in exactly the order `encode_zone_pass` issues them.
pub fn plan_zone_draws<'a, S: ZoneDrawMesh, I: ZoneDrawMesh>(
    statics:   &'a [S],
    instanced: &'a [I],
    now_ms:    u64,
) -> impl Iterator<Item = ZoneDrawStep> + 'a {
    let len_of = move |src: ZoneMeshSource| match src {
        ZoneMeshSource::Static    => statics.len(),
        ZoneMeshSource::Instanced => instanced.len(),
    };
    let mode_of = move |src: ZoneMeshSource, i: usize| match src {
        ZoneMeshSource::Static    => statics[i].render_mode(),
        ZoneMeshSource::Instanced => instanced[i].render_mode(),
    };
    let tex_of = move |src: ZoneMeshSource, i: usize| match src {
        ZoneMeshSource::Static =>
            animated_frame_texture(statics[i].texture_idx(), statics[i].anim(), now_ms),
        ZoneMeshSource::Instanced =>
            animated_frame_texture(instanced[i].texture_idx(), instanced[i].anim(), now_ms),
    };

    ZONE_SUBPASSES.into_iter().enumerate().flat_map(move |(subpass, sp)| {
        (0..len_of(sp.source))
            .filter(move |&i| sp.class.accepts(mode_of(sp.source, i)))
            // The `scan` seed is re-evaluated per sub-pass, so `current_tex` starts as `None` inside
            // every sub-pass and the first step of each necessarily takes the `Set` arm. The
            // sub-pass boundary reset is therefore STRUCTURAL, not a condition anyone can flip: an
            // added `!started` guard here measured green against the whole suite (mutation Z5,
            // #739/#741 PR body) precisely because it was unreachable.
            .scan((false, None::<Option<usize>>), move |(started, current_tex), mesh| {
                let tex  = tex_of(sp.source, mesh);
                let bind = if *current_tex == Some(tex) {
                    ZoneTexBind::Keep
                } else {
                    *current_tex = Some(tex);
                    ZoneTexBind::Set(tex)
                };
                let set_pipeline = !*started;
                *started = true;
                Some(ZoneDrawStep {
                    subpass, source: sp.source, class: sp.class, mesh, set_pipeline, bind,
                })
            })
    })
}

/// The device-bound half of a zone draw, expressed entirely in **plan vocabulary** —
/// [`ZoneMeshSource`]/[`ZoneBlendClass`] and texture *indices*, never `wgpu` handles.
///
/// `encode_zone_pass` supplies an impl closing over the renderer and the live `wgpu::RenderPass`;
/// `tests/zone_pass_routing.rs` supplies one that appends to a `Vec`. Both run the *same* executor,
/// so a regression in the plan→command translation is a test failure rather than something only a
/// GPU could notice.
///
/// [`Self::bind_texture`] receives an index and has no mesh in scope, so "bind the mesh's base
/// `texture_idx` instead of the resolved animation frame" — #718's N2 bug, which shipped once
/// already — is not expressible in the executor at all.
pub trait ZoneDrawSink {
    /// Start a sub-pass: select the pipeline for this (source, class) pair.
    fn set_pipeline(&mut self, source: ZoneMeshSource, class: ZoneBlendClass);
    /// Bind group 0 (the camera uniform).
    fn bind_camera(&mut self);
    /// Bind group 1 to the *already-resolved* texture index (`None` → fallback texture).
    fn bind_texture(&mut self, idx: Option<usize>);
    /// Bind group 2 (the sun shadow map, #518).
    fn bind_shadow_sample(&mut self);
    /// Issue the draw for mesh `mesh` of the list named by `source`.
    fn draw(&mut self, source: ZoneMeshSource, mesh: usize);
}

/// Turns a [`plan_zone_draws`] plan into sink calls. **This is the real executor** —
/// `encode_zone_pass` calls exactly this function.
///
/// The call order within a first step (pipeline, group 0, group 1, group 2) is the pre-#741 order,
/// preserved because `tests/zone_pass_routing.rs` grades this stream against a verbatim
/// transcription of the two pre-#741 closures.
pub fn execute_zone_draw_plan<S: ZoneDrawMesh, I: ZoneDrawMesh, K: ZoneDrawSink>(
    statics:   &[S],
    instanced: &[I],
    now_ms:    u64,
    sink:      &mut K,
) {
    for step in plan_zone_draws(statics, instanced, now_ms) {
        if step.set_pipeline {
            sink.set_pipeline(step.source, step.class);
            sink.bind_camera();
        }
        if let ZoneTexBind::Set(tex) = step.bind {
            sink.bind_texture(tex);
        }
        if step.set_pipeline {
            sink.bind_shadow_sample();
        }
        sink.draw(step.source, step.mesh);
    }
}

/// Zone geometry pass. Clears depth to 1.0; preserves sky color from sky pass.
pub fn encode_zone_pass(
    r:       &EqRenderer,
    encoder: &mut wgpu::CommandEncoder,
    view:    &wgpu::TextureView,
    _scene:  &SceneState,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("zone"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Load,
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(1.0),  // only pass that clears depth
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
    });

    // The pass ROUTING — the six sub-passes and their order, which mesh list and which render modes
    // each one draws, which animated texture frame a mesh binds, and when a group-1 rebind is
    // actually needed — comes from `plan_zone_draws` (#741), which is device-free and unit tested in
    // tests/zone_pass_routing.rs (with a differential pin against the pre-#741 closures there).
    //
    // `ZoneSink` below is the rest: five bodies that turn plan vocabulary into `wgpu` handles, ALL
    // of which need a live device and NONE of which any test can reach. Two of the five make real
    // decisions, so "nothing here decides anything" would be false — measured, by the #784 reviewer,
    // with two mutations that both left the crate green at 226 passed / 0 failed:
    //
    //   * `draw` picks `gpu_meshes` vs `gpu_instanced` from the `ZoneMeshSource`. Pointing the
    //     `Static` arm at `gpu_instanced` draws the wrong geometry for every static zone mesh, and
    //     no test notices (mutation S1).
    //   * `bind_texture` picks `texture_bind_groups[i]` vs `fallback_texture_bg`. Ignoring the index
    //     and always binding the fallback untextures the whole zone, and no test notices (S2).
    //
    // Both predate #741 (they survive identically on the base file) and are recorded rather than
    // fixed here; closing them needs a device or a further indirection, not another planner test.
    // The other three bodies — the six-arm pipeline lookup, `camera_uniform.bind_group`,
    // `shadow_sample_bg` — are straight lookups with no condition in them.
    //
    // If you add a condition to this impl, it belongs in the planner, where it is testable.
    struct ZoneSink<'r, 'p, 'e> {
        r:    &'r EqRenderer,
        pass: &'p mut wgpu::RenderPass<'e>,
    }
    impl ZoneDrawSink for ZoneSink<'_, '_, '_> {
        fn set_pipeline(&mut self, source: ZoneMeshSource, class: ZoneBlendClass) {
            let p = &self.r.pipelines;
            self.pass.set_pipeline(match (source, class) {
                (ZoneMeshSource::Static,    ZoneBlendClass::OpaqueMasked) => &p.zone,
                (ZoneMeshSource::Static,    ZoneBlendClass::Blend)        => &p.zone_blend,
                (ZoneMeshSource::Static,    ZoneBlendClass::Additive)     => &p.zone_additive,
                (ZoneMeshSource::Instanced, ZoneBlendClass::OpaqueMasked) => &p.zone_instanced,
                (ZoneMeshSource::Instanced, ZoneBlendClass::Blend)        => &p.zone_instanced_blend,
                (ZoneMeshSource::Instanced, ZoneBlendClass::Additive)     => &p.zone_instanced_additive,
            });
        }
        fn bind_camera(&mut self) {
            let r = self.r;
            self.pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
        }
        fn bind_texture(&mut self, idx: Option<usize>) {
            let r = self.r;
            let bg = match idx {
                Some(i) if i < r.texture_bind_groups.len() => &r.texture_bind_groups[i],
                _ => &r.fallback_texture_bg,
            };
            self.pass.set_bind_group(1, bg, &[]);
        }
        fn bind_shadow_sample(&mut self) {
            let r = self.r;
            self.pass.set_bind_group(2, &r.shadow_sample_bg, &[]); // sun shadow map (#518)
        }
        fn draw(&mut self, source: ZoneMeshSource, mesh: usize) {
            match source {
                ZoneMeshSource::Static => {
                    let m = &self.r.gpu_meshes[mesh];
                    self.pass.set_vertex_buffer(0, m.vertex_buf.slice(..));
                    self.pass.set_index_buffer(m.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                    self.pass.draw_indexed(0..m.index_count, 0, 0..1);
                }
                ZoneMeshSource::Instanced => {
                    let m = &self.r.gpu_instanced[mesh];
                    self.pass.set_vertex_buffer(0, m.vertex_buf.slice(..));
                    self.pass.set_vertex_buffer(1, m.instance_buf.slice(..));
                    self.pass.set_index_buffer(m.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                    self.pass.draw_indexed(0..m.index_count, 0, 0..m.instance_count);
                }
            }
        }
    }

    // Wall-clock ms since process start, for cycling animated textures (fire/water/lava). The frame
    // a mesh resolves to is `animated_frame_texture`, the SAME function the masked shadow sub-pass
    // uses (#721) — the colour and shadow silhouettes must agree on the texel, or #718's N2 mismatch
    // comes back. The planner calls it; do not re-inline a copy here.
    let now_ms = anim_now_ms();
    let mut sink = ZoneSink { r, pass: &mut pass };
    execute_zone_draw_plan(&r.gpu_meshes, &r.gpu_instanced, now_ms, &mut sink);
}

/// Draw the zone's doors (closed state). Each door uses its object model if loaded, else a
/// reddish fallback cube at the door position. Per-door model matrix lets Task 9 animate opens.
/// Each mesh binds its decoded texture from the shared `door_textures` (by `texture_idx`),
/// falling back to the white placeholder only when a model/texture is missing.
///
/// Placement (closed): `m = translate(pos) * rotZ(yaw) * rotY(incline) * scale(size/100)`,
/// `yaw = heading*TAU/512 + FRAC_PI_2`. The door model's origin is the hinge edge (= door.pos),
/// so the open animation swings about the origin.
pub fn encode_door_pass(
    r:       &EqRenderer,
    encoder: &mut wgpu::CommandEncoder,
    view:    &wgpu::TextureView,
    scene:   &SceneState,
) {
    use crate::gpu::EntityUniform;
    if scene.doors.is_empty() { return; }

    // Phase 1: assign a uniform slot per door, write its model matrix, and record what to draw.
    // (slot_idx, &GpuMesh) — meshes of the same door share that door's slot/matrix.
    // (slot, mesh, texture bind group) — None texture falls back to the white placeholder.
    let mut draws: Vec<(usize, &crate::gpu::GpuMesh, Option<&wgpu::BindGroup>)> = Vec::new();
    let mut slot = 0usize;
    for door in &scene.doors {
        if slot >= r.door_uniform_pool.len() { break; }

        let model_meshes: Vec<&crate::gpu::GpuMesh> =
            match r.door_models.get(&door.name.to_uppercase()) {
                Some(w) => w.meshes.iter().collect(),
                None    => match &r.door_fallback {
                    Some(cube) => vec![cube],
                    None       => continue,
                },
            };
        if model_meshes.is_empty() { continue; }

        // Build the placement matrix. Fallback cube uses translate-only (no model orientation).
        let key = door.name.to_uppercase();
        let mat = if r.door_models.contains_key(&key) {
            let scale = door.size as f32 / 100.0;
            // Door heading is raw EQ units (0..512). The +90° offset (verified visually: doors
            // face the correct way with it) matches the entity/player convention:
            // yaw = heading*TAU/512 + FRAC_PI_2.
            let yaw   = (door.heading / 512.0) * std::f32::consts::TAU + std::f32::consts::FRAC_PI_2;
            let placement = glam::Mat4::from_translation(glam::Vec3::from(door.pos))
                * glam::Mat4::from_rotation_z(yaw)
                * glam::Mat4::from_rotation_y((door.incline as f32 / 512.0) * std::f32::consts::TAU)
                * glam::Mat4::from_scale(glam::Vec3::splat(scale));

            // Apply open animation in door-local model space (after scale).
            let f = door.open_frac;
            let local_open = match door.opentype {
                100..=119 => glam::Mat4::from_translation(glam::vec3(0.0, 0.0, 10.0 * f)),
                11..=15   => glam::Mat4::from_translation(glam::vec3(8.0 * f, 0.0, 0.0)),
                _ => {
                    // Hinged swing about the model origin (= door.pos = the hinge edge in EQ).
                    // Negative angle swings the leaf outward (away from the player side).
                    glam::Mat4::from_rotation_z(-f * std::f32::consts::FRAC_PI_2)
                }
            };
            placement * local_open
        } else {
            glam::Mat4::from_translation(glam::Vec3::from(door.pos))
        };

        r.queue.write_buffer(&r.door_uniform_pool[slot].0, 0,
            bytemuck::bytes_of(&EntityUniform { model: mat.to_cols_array_2d(), tint: [1.0; 4] }));
        for mesh in model_meshes {
            // Resolve the mesh's decoded texture from the shared door texture set; the fallback
            // cube has texture_idx None -> white placeholder.
            let tex_bg = mesh.texture_idx.and_then(|i| r.door_textures.get(i));
            draws.push((slot, mesh, tex_bg));
        }
        slot += 1;
    }
    if draws.is_empty() { return; }

    // Phase 2: one render pass, drawing every recorded door mesh.
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("doors"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view, resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None, occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.character);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    for (slot_idx, mesh, tex_bg) in draws {
        pass.set_bind_group(1, tex_bg.unwrap_or(&r.fallback_texture_bg), &[]);
        pass.set_bind_group(2, &r.door_uniform_pool[slot_idx].1, &[]);
        pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
        pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
    }
}

/// Billboard pass for NPC entities that have no 3D model. Skipped if nothing to draw.
pub fn encode_billboard_pass(
    r:         &EqRenderer,
    encoder:   &mut wgpu::CommandEncoder,
    view:      &wgpu::TextureView,
    scene:     &SceneState,
    cam_right: [f32; 3],
    cam_up:    [f32; 3],
) {
    use wgpu::util::DeviceExt;
    use crate::billboard::{billboard_quad, npc_color, npc_size};

    let mut all_verts: Vec<crate::gpu::Vertex> = Vec::new();
    let mut all_idxs:  Vec<u32>                = Vec::new();

    for b in &scene.billboards {
        if r.character_model_for(&b.race, b.gender).is_some() { continue; }
        let (verts, idxs) = billboard_quad(
            b.pos, npc_size(b.level), npc_color(b.is_target, b.dead, b.hp_pct),
            cam_right, cam_up,
        );
        let base = all_verts.len() as u32;
        all_verts.extend(verts);
        all_idxs.extend(idxs.iter().map(|i| i + base));
    }

    if all_verts.is_empty() { return; }

    let vbuf = r.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("billboard_verts"),
        contents: bytemuck::cast_slice(&all_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let ibuf = r.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("billboard_idxs"),
        contents: bytemuck::cast_slice(&all_idxs),
        usage: wgpu::BufferUsages::INDEX,
    });
    let idx_count = all_idxs.len() as u32;

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("billboards"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view, resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.billboard);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    pass.set_vertex_buffer(0, vbuf.slice(..));
    pass.set_index_buffer(ibuf.slice(..), wgpu::IndexFormat::Uint32);
    pass.draw_indexed(0..idx_count, 0, 0..1);
}

/// Player pass. Renders a 3D model when scene.player_race maps to a loaded archetype.
/// Draws nothing if no race is set or no model is loaded.
///
/// Uses entity_uniform_pool[0..PLAYER_UNIFORM_SLOTS) and joint_buf_pool[0] (player slot).
/// The entity passes must use pool slots >= PLAYER_UNIFORM_SLOTS to avoid overlap.
pub fn encode_player_pass(
    r:         &EqRenderer,
    encoder:   &mut wgpu::CommandEncoder,
    view:      &wgpu::TextureView,
    scene:     &SceneState,
) {
    use crate::renderer::PLAYER_UNIFORM_SLOTS;
    use crate::models::race_to_archetype;
    use crate::gpu::{EntityUniform, GpuModel};

    if !scene.player_race.is_empty() {
        let archetype = race_to_archetype(&scene.player_race);

        match r.character_model_for(&scene.player_race, scene.player_gender) {
            Some(GpuModel::Skinned(model)) => {
                // #694 hardening: guard with `clip_idx < clips.len()`, matching the NPC pass sites
                // below (e.g. the death-sentinel check at ~line 907). `!clips.is_empty()` alone does
                // NOT bound `clip_idx` — the #692/#694 bind-pose sentinel is `usize::MAX`, which is
                // always `>= clips.len()` for any non-empty clip set and would index out of range.
                let matrices = match r.anim_states.get(&0) {
                    Some(state) if !model.skin.clips.is_empty()
                        && state.clip_idx < model.skin.clips.len() =>
                        model.skin.evaluate(state.clip_idx, state.time),
                    _ => model.skin.bind_pose(),
                };
                let id4 = [[1f32,0.,0.,0.],[0.,1.,0.,0.],[0.,0.,1.,0.],[0.,0.,0.,1.]];
                let mut joint_array = [id4; 128];
                for (i, m) in matrices.iter().enumerate().take(128) { joint_array[i] = *m; }
                // Write to pool slot 0 (reserved for player).
                r.queue.write_buffer(&r.joint_buf_pool[0].0, 0, bytemuck::cast_slice(&joint_array));

                let target = crate::models::skinned_target_height(
                    &scene.player_race, archetype, model.true_height);
                // Normalize to `target` height and ground by the model's own feet. This math
                // lives in `models::humanoid_placement` so the placement regression test can
                // exercise the exact production computation (see the fn's doc; #357).
                //   - mesh_scale = target/true_height (NO node_scale re-apply, #149).
                //   - Skinned EQ models are authored horizontally centered on the origin, so NO
                //     recenter (center_xz=[0,0]); measured centers were unreliable and pushed
                //     the model off. Vertically the origin sits above the feet, so visual_scale
                //     lifts by -2*feet_offset*mesh_scale to ground the feet.
                let placement = crate::models::humanoid_placement(
                    model.true_height, model.feet_offset, target);
                let dominant_mesh_scale = placement.mesh_scale;
                let visual_scale = placement.visual_scale;

                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    let mat = crate::camera::entity_model_matrix_heading(
                        scene.player_pos, scene.player_heading, visual_scale,
                        dominant_mesh_scale, [0.0, 0.0], true, 0.0,
                        crate::models::archetype_correction(archetype),
                    );
                    let tint = match model.equip_slots[i] {
                        Some(ref es) if scene.player_equipment_tint[es.slot] != [0, 0, 0] => {
                            let t = scene.player_equipment_tint[es.slot];
                            [t[0] as f32 / 255.0, t[1] as f32 / 255.0, t[2] as f32 / 255.0, 1.0]
                        }
                        _ => mesh.base_color,
                    };
                    // Runtime-tint synthetic hair shells by the player's hair color (eqoxide#98).
                    let tint = crate::models::head_part_tint(model.head_parts[i], scene.player_haircolor,
                        &scene.player_race, scene.player_gender)
                        .unwrap_or(tint);
                    r.queue.write_buffer(
                        &r.entity_uniform_pool[i].0, 0,
                        bytemuck::bytes_of(&EntityUniform { model: mat, tint }),
                    );
                }

                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("player_skinned"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view, resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &r.depth_view,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: None, occlusion_query_set: None,
                });
                pass.set_pipeline(&r.pipelines.skinned);
                pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
                pass.set_bind_group(1, &r.fallback_texture_bg, &[]);
                pass.set_bind_group(3, &r.joint_buf_pool[0].1, &[]);
                // Two-layer Luclin body: pass 1 draws the opaque skin base (the model's baked
                // texture) for every visible mesh; pass 2 composites the cloth/armor overlay on top
                // (alpha-blended, LessEqual depth) so exposed skin shows where the overlay is
                // transparent (e.g. the elf-female midriff). See docs/equipment-textures-findings.md.
                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    if equip_mesh_hidden(r, &model.prefix, model.equip_slots[i], &scene.player_equipment) { continue; }
                    if !crate::models::head_part_visible(
                        model.head_parts[i], model.head_default_hidden[i],
                        scene.player_face, scene.player_hairstyle,
                    ) { continue; }
                    pass.set_bind_group(2, &r.entity_uniform_pool[i].1, &[]);
                    pass.set_bind_group(1, skin_base_tex(r, &model.texture_bind_groups, mesh.texture_idx), &[]);
                    pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                    pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                }
                pass.set_pipeline(&r.pipelines.skinned_overlay);
                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    if equip_mesh_hidden(r, &model.prefix, model.equip_slots[i], &scene.player_equipment) { continue; }
                    if !crate::models::head_part_visible(
                        model.head_parts[i], model.head_default_hidden[i],
                        scene.player_face, scene.player_hairstyle,
                    ) { continue; }
                    let Some(overlay) = resolve_overlay_tex(r, &model.prefix,
                        model.equip_slots[i].clone(), &scene.player_equipment) else { continue };
                    pass.set_bind_group(2, &r.entity_uniform_pool[i].1, &[]);
                    pass.set_bind_group(1, overlay, &[]);
                    pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                    pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                }
                drop(pass); // end the skinned pass before drawing the weapon

                // ── Held items: draw each equipped item at the rig's attachment bone
                // (R_POINT = primary hand, L_POINT = left hand, SHIELD_POINT = off-hand
                // shield), posed by the current animation so it swings with combat. EQ
                // authors IT models in the point-bone frame with an identity attach; the
                // only extra transform bridges the weapon bake's vertex convention into
                // the rig frame (see models::held_item_xform); no per-weapon tuning.
                // #694 hardening: `joint_world` indexes `clips[clip_idx]` directly and panics if
                // out of range (same as `evaluate`). The old `unwrap_or((0, 0.0))` here didn't
                // guard against a valid-but-OOB `clip_idx` (the #692/#694 bind-pose sentinel is
                // `usize::MAX`) — match the NPC held-item pattern below (~line 976): filter the
                // anim state by `clip_idx < clips.len()` and use `None` (→ `joint_world_rest`,
                // the None arm at the `hand` match below) when it's out of range.
                let clip_time = r.anim_states.get(&0)
                    .filter(|s| s.clip_idx < model.skin.clips.len())
                    .map(|s| (s.clip_idx, s.time));
                let pmat = glam::Mat4::from_cols_array_2d(&crate::camera::entity_model_matrix_heading(
                    scene.player_pos, scene.player_heading, visual_scale, dominant_mesh_scale,
                    [0.0, 0.0], true, 0.0, crate::models::archetype_correction(archetype)));
                let hx = crate::models::held_item_xform();
                // Held-item source unified with every other spawn (equipment materials 7/8),
                // inventory IDFile preferred when present. Primary → R_POINT (right), secondary →
                // L_POINT (left) / SHIELD_POINT — the mapping verified correct against the RoF2
                // skeleton; #515's "wrong hand" was a false report, so the hands are NOT swapped.
                let held: Vec<(String, &'static str, usize)> = crate::models::self_held_item_keys(
                        &scene.player_equipment,
                        &scene.primary_weapon_idfile,
                        &scene.secondary_weapon_idfile,
                        false,
                    ).into_iter().enumerate()
                     .filter_map(|(i, k)| k.map(|(key, bone)| (key, bone, i)))
                     .collect();
                let mut weapon_draws: Vec<(&crate::gpu::GpuWeapon, usize)> = Vec::new();
                for (wkey, bone, wslot) in &held {
                    let Some(Some(weapon)) = r.weapon_cache.get(wkey) else { continue };
                    // GLBs baked before joint names were exported can't locate the bone;
                    // skip rather than guess (a wrong bone reads worse than no weapon).
                    // Old rigs may lack SHIELD_POINT — fall back to gripping at L_POINT.
                    let Some(joint) = model.skin.attach_joint(bone)
                        .or_else(|| (*bone == "SHIELD_POINT")
                            .then(|| model.skin.attach_joint("L_POINT")).flatten())
                        else { continue };
                    let hand = glam::Mat4::from_cols_array_2d(&match clip_time {
                        Some((clip_i, t)) => model.skin.joint_world(clip_i, t, joint),
                        None               => model.skin.joint_world_rest(joint),
                    });
                    let wmat = (pmat * hand * hx).to_cols_array_2d();
                    r.queue.write_buffer(&r.weapon_uniform_pool[*wslot].0, 0,
                        bytemuck::bytes_of(&EntityUniform { model: wmat, tint: [1.0, 1.0, 1.0, 1.0] }));
                    weapon_draws.push((weapon, *wslot));
                }
                if !weapon_draws.is_empty() {
                    let mut wpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("player_weapon"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view, resolve_target: None,
                            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                        })],
                        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                            view: &r.depth_view,
                            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
                            stencil_ops: None,
                        }),
                        timestamp_writes: None, occlusion_query_set: None,
                    });
                    wpass.set_pipeline(&r.pipelines.character);
                    wpass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
                    for (weapon, wslot) in &weapon_draws {
                        wpass.set_bind_group(2, &r.weapon_uniform_pool[*wslot].1, &[]);
                        for mesh in &weapon.meshes {
                            let bg = mesh.texture_idx.and_then(|ti| weapon.texture_bind_groups.get(ti))
                                .unwrap_or(&r.fallback_texture_bg);
                            wpass.set_bind_group(1, bg, &[]);
                            wpass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                            wpass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                            wpass.draw_indexed(0..mesh.index_count, 0, 0..1);
                        }
                    }
                }
                return;
            }
            Some(GpuModel::Static(model)) => {
                // `floating: false` — the player's z is the CharacterController's FOOT datum, not
                // a wire passthrough, so the player is never a model-origin placement (#756).
                let p = crate::models::static_placement(
                    archetype, model.y_bottom,
                    [model.x_center, model.z_center], false);
                let mat = crate::camera::entity_model_matrix_heading(
                    scene.player_pos, scene.player_heading, 0.0, p.mesh_scale,
                    p.center_xz, true, p.y_bottom,
                    crate::models::archetype_correction(archetype),
                );
                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    let tint = match model.equip_slots[i] {
                        Some(ref es) if scene.player_equipment_tint[es.slot] != [0, 0, 0] => {
                            let t = scene.player_equipment_tint[es.slot];
                            [t[0] as f32 / 255.0, t[1] as f32 / 255.0, t[2] as f32 / 255.0, 1.0]
                        }
                        _ => mesh.base_color,
                    };
                    // Runtime-tint synthetic hair shells by the player's hair color (eqoxide#98).
                    let tint = crate::models::head_part_tint(model.head_parts[i], scene.player_haircolor,
                        &scene.player_race, scene.player_gender)
                        .unwrap_or(tint);
                    r.queue.write_buffer(
                        &r.entity_uniform_pool[i].0, 0,
                        bytemuck::bytes_of(&EntityUniform { model: mat, tint }),
                    );
                }
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("player_static"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view, resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &r.depth_view,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: None, occlusion_query_set: None,
                });
                pass.set_pipeline(&r.pipelines.character);
                pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
                pass.set_bind_group(1, &r.fallback_texture_bg, &[]);
                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    pass.set_bind_group(2, &r.entity_uniform_pool[i].1, &[]);
                    let bg = resolve_equip_tex(r, &model.texture_bind_groups, mesh.texture_idx,
                        &model.prefix, model.equip_slots[i].clone(), &scene.player_equipment);
                    pass.set_bind_group(1, bg, &[]);
                    pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                    pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                }
                return;
            }
            None => {}
        }
    }
}

/// Render a single static model with the given transform.
/// This is the core rendering logic shared by the player pass, entity pass,
/// and the standalone model viewer (`render_model`).
///
/// `model_matrix` is the full 4×4 model→world transform (from `entity_model_matrix_heading`).
/// Uniform buffer slots are taken from `r.entity_uniform_pool[base_slot..]`.
/// At most `max_meshes` meshes are drawn; pass `usize::MAX` for no limit.
#[allow(clippy::too_many_arguments)]
pub fn render_static_model(
    r:            &EqRenderer,
    encoder:      &mut wgpu::CommandEncoder,
    view:         &wgpu::TextureView,
    model:        &crate::gpu::GpuStaticModel,
    model_matrix: [[f32; 4]; 4],
    tint:         [f32; 4],
    base_slot:    usize,
    max_meshes:   usize,
) {
    use crate::gpu::EntityUniform;

    let slot_count = r.entity_uniform_pool.len();
    for (i, _mesh) in model.meshes.iter().enumerate() {
        if i >= max_meshes { break; }
        let slot = base_slot + i;
        if slot >= slot_count { break; }
        r.queue.write_buffer(
            &r.entity_uniform_pool[slot].0, 0,
            bytemuck::bytes_of(&EntityUniform { model: model_matrix, tint }),
        );
    }

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("static_model"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view, resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None, occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.character);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    pass.set_bind_group(1, &r.fallback_texture_bg, &[]);
    let mut cur_tex: Option<usize> = None;
    for (i, mesh) in model.meshes.iter().enumerate() {
        if i >= max_meshes { break; }
        let slot = base_slot + i;
        if slot >= slot_count { break; }
        pass.set_bind_group(2, &r.entity_uniform_pool[slot].1, &[]);
        if mesh.texture_idx != cur_tex {
            cur_tex = mesh.texture_idx;
            let bg = match cur_tex {
                Some(idx) if idx < model.texture_bind_groups.len() =>
                    &model.texture_bind_groups[idx],
                _ => &r.fallback_texture_bg,
            };
            pass.set_bind_group(1, bg, &[]);
        }
        pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
        pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
    }
}

/// Static glTF character model pass — all static-model entities in ONE render pass.
/// Uses entity_uniform_pool[PLAYER_UNIFORM_SLOTS .. pool_len/2+PLAYER_UNIFORM_SLOTS).
pub fn encode_entity_pass(
    r:        &EqRenderer,
    encoder:  &mut wgpu::CommandEncoder,
    view:     &wgpu::TextureView,
    scene:    &SceneState,
    _cam_pos: [f32; 3],
) {
    use crate::renderer::PLAYER_UNIFORM_SLOTS;
    use crate::models::race_to_archetype;
    use crate::gpu::GpuModel;

    struct DrawCmd { archetype: &'static str, mesh_idx: usize, uniform_slot: usize, equipment: [u32; 9], gender: u8, face: u8, hairstyle: u8 }

    let mut draws: Vec<DrawCmd> = Vec::new();
    let pool_half = r.entity_uniform_pool.len() / 2;
    let slot_end  = PLAYER_UNIFORM_SLOTS + pool_half;
    let mut slot  = PLAYER_UNIFORM_SLOTS;

    for b in &scene.billboards {
        if !crate::camera::entity_in_view(b.pos, scene.player_pos, r.last_view_proj,
                                          ENTITY_DRAW_DIST, ENTITY_CULL_MARGIN) { continue; }
        let archetype = race_to_archetype(&b.race);
        let Some(GpuModel::Static(model)) = r.model_for(archetype, b.gender) else { continue };
        // A floating spawn's stored z never went through `wire_z_to_foot`'s foot conversion,
        // so `static_placement` drops the grounding lift for it (#756). The horizontal
        // recentre is unchanged — that datum was not established; see the fn's doc.
        let p = crate::models::static_placement(
            archetype, model.y_bottom, [model.x_center, model.z_center], b.floating);
        let mat = crate::camera::entity_model_matrix_heading(b.pos, b.heading, 0.0, p.mesh_scale,
            p.center_xz, true, p.y_bottom, crate::models::archetype_correction(archetype));
        for (mesh_idx, mesh) in model.meshes.iter().enumerate() {
            if slot >= slot_end { break; }
            let slot_meta = model.equip_slots[mesh_idx];
            let tint: [f32; 4] = if b.dead { [0.5, 0.5, 0.5, 1.0] }
                                 else if b.is_target { [1.0, 0.3, 0.3, 1.0] }
                                 else {
                                     match slot_meta {
                                         Some(es) if b.equipment_tint[es.slot] != [0, 0, 0] => {
                                             let t = b.equipment_tint[es.slot];
                                             [t[0] as f32 / 255.0, t[1] as f32 / 255.0, t[2] as f32 / 255.0, 1.0]
                                         }
                                         _ => mesh.base_color,
                                     }
                                 };
            // Runtime-tint synthetic hair shells by the NPC's hair color (eqoxide#98) — unless the
            // whole model is dead-greyed or target-highlighted (those overrides win).
            let tint = if !b.dead && !b.is_target {
                crate::models::head_part_tint(model.head_parts[mesh_idx], b.haircolor, &b.race, b.gender).unwrap_or(tint)
            } else { tint };
            r.queue.write_buffer(
                &r.entity_uniform_pool[slot].0, 0,
                bytemuck::bytes_of(&crate::gpu::EntityUniform { model: mat, tint }),
            );
            draws.push(DrawCmd { archetype, mesh_idx, uniform_slot: slot, equipment: b.equipment, gender: b.gender, face: b.face, hairstyle: b.hairstyle });
            slot += 1;
        }
        if slot >= slot_end { break; }
    }
    if draws.is_empty() { return; }

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("entities"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view, resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None, occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.character);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    pass.set_bind_group(1, &r.fallback_texture_bg, &[]);

    for draw in &draws {
        let Some(GpuModel::Static(model)) = r.model_for(draw.archetype, draw.gender) else { continue };
        let mesh = &model.meshes[draw.mesh_idx];
        if equip_mesh_hidden(r, &model.prefix, model.equip_slots[draw.mesh_idx], &draw.equipment) { continue; }
        if !crate::models::head_part_visible(
            model.head_parts[draw.mesh_idx],
            model.head_default_hidden[draw.mesh_idx],
            draw.face, draw.hairstyle,
        ) { continue; }
        pass.set_bind_group(2, &r.entity_uniform_pool[draw.uniform_slot].1, &[]);
        let bg = resolve_equip_tex(r, &model.texture_bind_groups, mesh.texture_idx,
            &model.prefix, model.equip_slots[draw.mesh_idx], &draw.equipment);
        pass.set_bind_group(1, bg, &[]);
        pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
        pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
    }
}

/// Skinned glTF character model pass — all skinned-model entities in ONE render pass.
/// Joint pool: slot 0 = player (reserved), slots 1..N = entities.
/// Uniform pool: upper half (avoids overlap with static entity pass and player slots).
pub fn encode_skinned_entity_pass(
    r:        &EqRenderer,
    encoder:  &mut wgpu::CommandEncoder,
    view:     &wgpu::TextureView,
    scene:    &SceneState,
    _cam_pos: [f32; 3],
) {
    use crate::renderer::PLAYER_UNIFORM_SLOTS;
    use crate::models::race_to_archetype;
    use crate::gpu::{EntityUniform, GpuModel};

    struct DrawCmd { model_key: &'static str, model_slot: u8, mesh_idx: usize, uniform_slot: usize, joint_slot: usize, equipment: [u32; 9], face: u8, hairstyle: u8 }

    let mut draws: Vec<DrawCmd> = Vec::new();
    // Held items (spawn equipment slots 7/8) drawn at the rig's attach bones, same
    // contract as the player pass. weapon_uniform_pool slots 0-1 belong to the player;
    // entities allocate from 2, nearest-first, and overflow just skips the item.
    let mut weapon_draws: Vec<(String, usize)> = Vec::new();
    let mut w_slot = 2usize;
    let hx = crate::models::held_item_xform();
    let pool_half    = r.entity_uniform_pool.len() / 2;
    let uniform_base = pool_half + PLAYER_UNIFORM_SLOTS; // upper half for skinned
    let mut u_slot   = uniform_base;
    let mut j_slot   = 1usize; // slot 0 reserved for player

    let id4 = [[1f32,0.,0.,0.],[0.,1.,0.,0.],[0.,0.,1.,0.],[0.,0.,0.,1.]];

    // Each humanoid model is ~27 meshes, so the uniform/joint pools can't hold every
    // spawn in a crowded zone. Render NEAREST-first so the NPCs around the player always
    // draw, and only draw a model that fits ENTIRELY in the remaining pool (no partial,
    // shrunken-looking bodies). Distant overflow spawns fall back to their nameplate.
    let pp = scene.player_pos;
    let mut order: Vec<&crate::scene::Billboard> =
        scene.billboards.iter().collect();
    order.sort_by(|a, b| {
        let da = (a.pos[0]-pp[0]).powi(2) + (a.pos[1]-pp[1]).powi(2) + (a.pos[2]-pp[2]).powi(2);
        let db = (b.pos[0]-pp[0]).powi(2) + (b.pos[1]-pp[1]).powi(2) + (b.pos[2]-pp[2]).powi(2);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });

    for b in order {
        if !crate::camera::entity_in_view(b.pos, scene.player_pos, r.last_view_proj,
                                          ENTITY_DRAW_DIST, ENTITY_CULL_MARGIN) { continue; }
        let archetype = race_to_archetype(&b.race);
        let (model_key, model_slot) = crate::models::character_model_key(&b.race, b.gender);
        let Some(GpuModel::Skinned(model)) = r.model_by_key(model_key, model_slot) else { continue };
        // Skip (don't break) if this model doesn't fully fit — a later, smaller model
        // (e.g. an 8-mesh rat) may still fit in the remaining slots.
        if j_slot >= r.joint_buf_pool.len() { continue; }
        if u_slot + model.meshes.len() > r.entity_uniform_pool.len() { continue; }

        // Use the animation state's clip and time for all actions including "dead":
        // the renderer plays the D05 death clip once and holds the last frame.
        // When no death clip exists the sentinel clip_idx (usize::MAX) is out of range
        // so the bind-pose fallback fires automatically — standing corpse as before.
        let matrices: Vec<[[f32;4];4]> = match r.anim_states.get(&b.id) {
            Some(state) if !model.skin.clips.is_empty()
                        && state.clip_idx < model.skin.clips.len() =>
                model.skin.evaluate(state.clip_idx, state.time),
            _ => model.skin.bind_pose(),
        };
        let mut joint_array = [id4; 128];
        for (i, m) in matrices.iter().enumerate().take(128) { joint_array[i] = *m; }
        r.queue.write_buffer(&r.joint_buf_pool[j_slot].0, 0, bytemuck::cast_slice(&joint_array));

        let target = crate::models::skinned_target_height(&b.race, archetype, model.true_height);
        let height = if model.true_height > 0.001 { model.true_height } else { 1.0 };
        // See the player pass: normalize to `target` only — do not re-apply the authored
        // `node_scale` (it would re-inflate; the scale-100 `fish.glb` rendered ~100× too big).
        let dominant_scale    = target / height;
        // Same placement as the player pass: no recenter (models are authored centered),
        // lift by a calibrated fraction of target height to ground the feet.
        // Ground by the model's own feet: lift = -feet_offset * mesh_scale.
        let visual_scale = -2.0 * model.feet_offset * dominant_scale;

        for (mesh_idx, mesh) in model.meshes.iter().enumerate() {
            let mat = crate::camera::entity_model_matrix_heading(
                b.pos, b.heading, visual_scale, dominant_scale,
                [0.0, 0.0], true, 0.0,
                crate::models::archetype_correction(archetype),
            );
            let slot_meta = model.equip_slots[mesh_idx];
            let tint: [f32; 4] = if b.dead { [0.5, 0.5, 0.5, 1.0] }
                                 else if b.is_target { [1.0, 0.3, 0.3, 1.0] }
                                 else {
                                     match slot_meta {
                                         Some(es) if b.equipment_tint[es.slot] != [0, 0, 0] => {
                                             let t = b.equipment_tint[es.slot];
                                             [t[0] as f32 / 255.0, t[1] as f32 / 255.0, t[2] as f32 / 255.0, 1.0]
                                         }
                                         _ => mesh.base_color,
                                     }
                                 };
            // Runtime-tint synthetic hair shells by the NPC's hair color (eqoxide#98) — unless the
            // whole model is dead-greyed or target-highlighted (those overrides win).
            let tint = if !b.dead && !b.is_target {
                crate::models::head_part_tint(model.head_parts[mesh_idx], b.haircolor, &b.race, b.gender).unwrap_or(tint)
            } else { tint };
            r.queue.write_buffer(
                &r.entity_uniform_pool[u_slot].0, 0,
                bytemuck::bytes_of(&EntityUniform { model: mat, tint }),
            );
            draws.push(DrawCmd { model_key, model_slot, mesh_idx, uniform_slot: u_slot, joint_slot: j_slot, equipment: b.equipment, face: b.face, hairstyle: b.hairstyle });
            u_slot += 1;
        }

        // Held items at the rig attach bones, posed to match the body (animated pose
        // when the anim state is valid, rest pose when the body fell back to bind).
        let emat = glam::Mat4::from_cols_array_2d(&crate::camera::entity_model_matrix_heading(
            b.pos, b.heading, visual_scale, dominant_scale, [0.0, 0.0], true, 0.0,
            crate::models::archetype_correction(archetype)));
        let anim = r.anim_states.get(&b.id)
            .filter(|s| !model.skin.clips.is_empty() && s.clip_idx < model.skin.clips.len());
        for held in crate::models::held_item_keys(&b.equipment, b.dead) {
            let Some((key, bone)) = held else { continue };
            if w_slot >= r.weapon_uniform_pool.len() { break; }
            if !matches!(r.weapon_cache.get(&key), Some(Some(_))) { continue; }
            // Old rigs may lack SHIELD_POINT — fall back to gripping at L_POINT.
            let Some(joint) = model.skin.attach_joint(bone)
                .or_else(|| (bone == "SHIELD_POINT").then(|| model.skin.attach_joint("L_POINT")).flatten())
                else { continue };
            let hand = glam::Mat4::from_cols_array_2d(&match anim {
                Some(s) => model.skin.joint_world(s.clip_idx, s.time, joint),
                None    => model.skin.joint_world_rest(joint),
            });
            let wmat = (emat * hand * hx).to_cols_array_2d();
            r.queue.write_buffer(&r.weapon_uniform_pool[w_slot].0, 0,
                bytemuck::bytes_of(&EntityUniform { model: wmat, tint: [1.0, 1.0, 1.0, 1.0] }));
            weapon_draws.push((key, w_slot));
            w_slot += 1;
        }
        j_slot += 1;
    }
    if draws.is_empty() { return; }

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("skinned_entities"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view, resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None, occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.skinned);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    pass.set_bind_group(1, &r.fallback_texture_bg, &[]);

    // Two-layer Luclin body (same as the player pass): pass 1 lays down the opaque skin base for
    // every visible mesh; pass 2 composites the cloth/armor overlay on top, so skin shows through
    // wherever the overlay is transparent.
    let mut cur_joint = usize::MAX;
    for draw in &draws {
        let Some(GpuModel::Skinned(model)) = r.model_by_key(draw.model_key, draw.model_slot) else { continue };
        let mesh = &model.meshes[draw.mesh_idx];
        if draw.joint_slot != cur_joint {
            pass.set_bind_group(3, &r.joint_buf_pool[draw.joint_slot].1, &[]);
            cur_joint = draw.joint_slot;
        }
        if equip_mesh_hidden(r, &model.prefix, model.equip_slots[draw.mesh_idx], &draw.equipment) { continue; }
        if !crate::models::head_part_visible(
            model.head_parts[draw.mesh_idx],
            model.head_default_hidden[draw.mesh_idx],
            draw.face, draw.hairstyle,
        ) { continue; }
        pass.set_bind_group(2, &r.entity_uniform_pool[draw.uniform_slot].1, &[]);
        pass.set_bind_group(1, skin_base_tex(r, &model.texture_bind_groups, mesh.texture_idx), &[]);
        pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
        pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
    }
    pass.set_pipeline(&r.pipelines.skinned_overlay);
    cur_joint = usize::MAX;
    for draw in &draws {
        let Some(GpuModel::Skinned(model)) = r.model_by_key(draw.model_key, draw.model_slot) else { continue };
        let mesh = &model.meshes[draw.mesh_idx];
        if draw.joint_slot != cur_joint {
            pass.set_bind_group(3, &r.joint_buf_pool[draw.joint_slot].1, &[]);
            cur_joint = draw.joint_slot;
        }
        if equip_mesh_hidden(r, &model.prefix, model.equip_slots[draw.mesh_idx], &draw.equipment) { continue; }
        if !crate::models::head_part_visible(
            model.head_parts[draw.mesh_idx],
            model.head_default_hidden[draw.mesh_idx],
            draw.face, draw.hairstyle,
        ) { continue; }
        let Some(overlay) = resolve_overlay_tex(r, &model.prefix,
            model.equip_slots[draw.mesh_idx], &draw.equipment) else { continue };
        pass.set_bind_group(2, &r.entity_uniform_pool[draw.uniform_slot].1, &[]);
        pass.set_bind_group(1, overlay, &[]);
        pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
        pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
    }

    // Held items: static meshes at the pre-posed attach-bone matrices (same pipeline
    // the player weapon pass uses; bind group 3 is beyond this pipeline's layout and
    // is ignored).
    pass.set_pipeline(&r.pipelines.character);
    for (key, wslot) in &weapon_draws {
        let Some(Some(weapon)) = r.weapon_cache.get(key) else { continue };
        pass.set_bind_group(2, &r.weapon_uniform_pool[*wslot].1, &[]);
        for mesh in &weapon.meshes {
            let bg = mesh.texture_idx.and_then(|ti| weapon.texture_bind_groups.get(ti))
                .unwrap_or(&r.fallback_texture_bg);
            pass.set_bind_group(1, bg, &[]);
            pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
            pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.index_count, 0, 0..1);
        }
    }
}

/// Sun shadow-map DEPTH pass (#518). Renders the nearby shadow casters — the player, nearby
/// character models (skinned + static), and the zone's placed objects (instanced) — from the
/// directional light's POV into `r.shadow_map_view`. The lit zone shaders then sample that map so
/// characters/objects cast shadows onto the terrain. Depth-only: no color target.
///
/// Casters write their model matrices/joints to the DEDICATED `shadow_uniform_pool`/
/// `shadow_joint_pool` (never the color passes' pools), so encoding this pass first can't alias
/// their per-frame writes. The matrices are computed with the SAME `entity_model_matrix_heading` +
/// placement calls the color passes use, so a caster's shadow lines up with its rendered body.
/// The pass always runs (even with no casters) to clear the map to "fully lit".
pub fn encode_shadow_pass(
    r:            &EqRenderer,
    encoder:      &mut wgpu::CommandEncoder,
    scene:        &SceneState,
    light_center: [f32; 3],
) {
    use crate::models::{race_to_archetype, character_model_key, skinned_target_height,
                        archetype_correction, humanoid_placement, static_placement};
    use crate::gpu::{EntityUniform, GpuModel};

    enum Caster<'a> {
        Skinned { model: &'a crate::gpu::GpuSkinnedModel, u_slot: usize, j_slot: usize },
        Static  { model: &'a crate::gpu::GpuStaticModel,  u_slot: usize },
    }
    let id4 = [[1f32,0.,0.,0.],[0.,1.,0.,0.],[0.,0.,1.,0.],[0.,0.,0.,1.]];

    let mut casters: Vec<Caster> = Vec::new();

    // Write a padded 128-joint palette to shadow_joint_pool[slot], returning nothing.
    let write_joints = |slot: usize, mats: &[[[f32;4];4]]| {
        let mut joint_array = [id4; 128];
        for (i, m) in mats.iter().enumerate().take(128) { joint_array[i] = *m; }
        r.queue.write_buffer(&r.shadow_joint_pool[slot].0, 0, bytemuck::cast_slice(&joint_array));
    };
    let write_model = |slot: usize, mat: [[f32;4];4]| {
        r.queue.write_buffer(&r.shadow_uniform_pool[slot].0, 0,
            bytemuck::bytes_of(&EntityUniform { model: mat, tint: [1.0; 4] }));
    };

    // ── SELECTION ───────────────────────────────────────────────────────────
    //
    // Which entities cast this frame — nearest-first order, the frustum/distance cull, the
    // `SHADOW_CASTER_SLOTS` bound, the pool-slot assignment, and the #692/#694 clip guard — is
    // decided by `plan_shadow_casters` (#740), which is device-free and unit tested in
    // tests/shadow_caster_selection.rs (with a differential pin against the pre-#740 loops in the
    // same file). Nothing below this point makes a selection decision: it turns each planned step
    // into a matrix + buffer write. If you find yourself adding a condition here, it belongs in
    // the planner.
    /// A candidate's selection inputs, with its `GpuModel` resolved (a pure `HashMap` get).
    struct CandidateView<'a> {
        pos:   [f32; 3],
        model: Option<&'a GpuModel>,
        anim:  Option<(usize, f32)>,
    }
    impl ShadowCasterCandidate for CandidateView<'_> {
        fn pos(&self) -> [f32; 3] { self.pos }
        fn model_kind(&self) -> ShadowModelKind {
            match self.model {
                Some(GpuModel::Skinned(m)) =>
                    ShadowModelKind::Skinned { clip_count: m.skin.clips.len() },
                Some(GpuModel::Static(_))  => ShadowModelKind::Static,
                None                       => ShadowModelKind::Absent,
            }
        }
        fn anim_state(&self) -> Option<(usize, f32)> { self.anim }
    }

    let player_view = (!scene.player_race.is_empty()).then(|| CandidateView {
        pos:   scene.player_pos,
        model: r.character_model_for(&scene.player_race, scene.player_gender),
        anim:  r.anim_states.get(&0).map(|s| (s.clip_idx, s.time)),
    });
    // COST NOTE (#740 §2), reasoned from source and not benchmarked: this resolves EVERY billboard,
    // where the pre-#740 loop resolved only candidates that both survived the cull and were reached
    // before the 64-slot `break` — so the work goes from at most ~SHADOW_CASTER_SLOTS resolutions
    // per frame to `billboards.len()`. Each resolution is a `character_model_key` (one or two
    // `str::to_uppercase()` String allocations), one or two `gpu_character_models` gets, and one
    // `anim_states` get, plus this `Vec`. The OUTPUT is unchanged — all of it is side-effect-free.
    //
    // What would settle it (nobody has done this): a frame-time A/B in a crowded zone with this
    // eager resolve versus a lazy one, at a known `billboards.len()`. The missing input is how large
    // `billboards.len()` actually gets in the field, which is exactly the open question in #748 —
    // until that number exists, any figure here is arithmetic, not a measurement.
    let nearby: Vec<CandidateView> = scene.billboards.iter().map(|b| {
        let (key, slot) = character_model_key(&b.race, b.gender);
        CandidateView {
            pos:   b.pos,
            model: r.model_by_key(key, slot),
            anim:  r.anim_states.get(&b.id).map(|s| (s.clip_idx, s.time)),
        }
    }).collect();

    let plan = plan_shadow_casters(
        player_view.as_ref(), &nearby, light_center, scene.player_pos, r.last_view_proj);

    // `plan.reach` is coverage telemetry for tests/shadow_caster_selection.rs and is deliberately
    // unread here; see `ShadowCasterReach`.
    for step in &plan.steps {
        // Placement differs between the player and a nearby character (the player uses
        // `humanoid_placement`, nearby characters the feet-offset/dominant-scale form), so the two
        // arms stay separate — but neither decides *whether* to draw.
        match step.caster {
            ShadowCasterRef::Player => {
                let archetype = race_to_archetype(&scene.player_race);
                match (player_view.as_ref().and_then(|v| v.model), step.draw) {
                    (Some(GpuModel::Skinned(model)), ShadowCasterDraw::Skinned { j_slot, pose }) => {
                        let matrices = match pose {
                            ShadowPose::Clip { idx, time } => model.skin.evaluate(idx, time),
                            ShadowPose::BindPose           => model.skin.bind_pose(),
                        };
                        write_joints(j_slot, &matrices);
                        let target = skinned_target_height(&scene.player_race, archetype, model.true_height);
                        let p = humanoid_placement(model.true_height, model.feet_offset, target);
                        let mat = crate::camera::entity_model_matrix_heading(
                            scene.player_pos, scene.player_heading, p.visual_scale, p.mesh_scale,
                            [0.0, 0.0], true, 0.0, archetype_correction(archetype));
                        write_model(step.u_slot, mat);
                        casters.push(Caster::Skinned { model, u_slot: step.u_slot, j_slot });
                    }
                    (Some(GpuModel::Static(model)), ShadowCasterDraw::Static) => {
                        // Same placement call as the color pass, so the shadow tracks the body
                        // (see this fn's doc). `floating: false` — the player is never one (#756).
                        let p = static_placement(archetype, model.y_bottom,
                            [model.x_center, model.z_center], false);
                        let mat = crate::camera::entity_model_matrix_heading(
                            scene.player_pos, scene.player_heading, 0.0, p.mesh_scale,
                            p.center_xz, true, p.y_bottom,
                            archetype_correction(archetype));
                        write_model(step.u_slot, mat);
                        casters.push(Caster::Static { model, u_slot: step.u_slot });
                    }
                    _ => {}
                }
            }
            ShadowCasterRef::Nearby(i) => {
                let b = &scene.billboards[i];
                let archetype = race_to_archetype(&b.race);
                match (nearby[i].model, step.draw) {
                    (Some(GpuModel::Skinned(model)), ShadowCasterDraw::Skinned { j_slot, pose }) => {
                        let matrices = match pose {
                            ShadowPose::Clip { idx, time } => model.skin.evaluate(idx, time),
                            ShadowPose::BindPose           => model.skin.bind_pose(),
                        };
                        write_joints(j_slot, &matrices);
                        let target = skinned_target_height(&b.race, archetype, model.true_height);
                        let height = if model.true_height > 0.001 { model.true_height } else { 1.0 };
                        let dominant_scale = target / height;
                        let visual_scale   = -2.0 * model.feet_offset * dominant_scale;
                        let mat = crate::camera::entity_model_matrix_heading(
                            b.pos, b.heading, visual_scale, dominant_scale,
                            [0.0, 0.0], true, 0.0, archetype_correction(archetype));
                        write_model(step.u_slot, mat);
                        casters.push(Caster::Skinned { model, u_slot: step.u_slot, j_slot });
                    }
                    (Some(GpuModel::Static(model)), ShadowCasterDraw::Static) => {
                        // Same placement call as the color pass, so a floating hull's shadow
                        // tracks the hull instead of staying above it (#756; the gap between the two
                        // arms is 3.9823u for `boat.glb` since #768 corrected the grounded lift).
                        let p = static_placement(archetype, model.y_bottom,
                            [model.x_center, model.z_center], b.floating);
                        let mat = crate::camera::entity_model_matrix_heading(
                            b.pos, b.heading, 0.0, p.mesh_scale,
                            p.center_xz, true, p.y_bottom,
                            archetype_correction(archetype));
                        write_model(step.u_slot, mat);
                        casters.push(Caster::Static { model, u_slot: step.u_slot });
                    }
                    _ => {}
                }
            }
        }
    }

    // ── Encode the depth pass (always, to clear the map to "lit") ────────────
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("sun_shadow"),
        color_attachments: &[],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.shadow_map_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(1.0),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None, occlusion_query_set: None,
    });

    // Placed objects (opaque/masked instanced meshes) cast shadows. Opaque stays on the cheap
    // fragment-less `shadow_instanced` pipeline (no per-pixel cost). Masked routes through
    // `shadow_instanced_masked` (#707), which alpha-tests the caster's diffuse texture at the same
    // 0.5 threshold as the color pass — otherwise a foliage/branch quad casts its full bounding
    // rectangle instead of its cutout silhouette (the tree-shadow-is-square bug).
    //
    // Everything decided here — sub-pass order, which pipeline each caster goes to, which animated
    // texture frame the masked sub-pass alpha-tests against, and when a group-1 rebind is actually
    // needed — comes from `plan_instanced_shadow_draws` (#721), which is device-free and unit
    // tested in tests/shadow_routing.rs. The plan→command translation is `execute_instanced_shadow_plan`,
    // which is ALSO device-free and is graded in tests/shadow_routing_equivalence.rs against a
    // transcription of the pre-#721 loops. All that is left here is `InstancedShadowSink`: four
    // one-expression bodies that turn plan vocabulary into `wgpu` handles. Nothing in this impl
    // decides anything — if you find yourself adding a condition to it, it belongs in the planner.
    struct PassSink<'r, 'p, 'e> {
        r:    &'r EqRenderer,
        pass: &'p mut wgpu::RenderPass<'e>,
    }
    impl InstancedShadowSink for PassSink<'_, '_, '_> {
        fn set_pipeline(&mut self, kind: ShadowPipelineKind) {
            let r = self.r;
            // tests/shadow_routing.rs source-text-pins these two arms.
            self.pass.set_pipeline(match kind {
                ShadowPipelineKind::Opaque => &r.pipelines.shadow_instanced,
                ShadowPipelineKind::Masked => &r.pipelines.shadow_instanced_masked,
            });
        }
        fn bind_light_depth(&mut self) {
            let r = self.r;
            self.pass.set_bind_group(0, &r.light_depth_bg, &[]);
        }
        fn bind_texture(&mut self, idx: Option<usize>) {
            let r = self.r;
            // The out-of-range fallback is the same one `encode_zone_pass`'s `ZoneSink::bind_texture`
            // applies. (It named a `tex_bg` closure there until #741 replaced it with that method.)
            let bg = match idx {
                Some(i) if i < r.texture_bind_groups.len() => &r.texture_bind_groups[i],
                _ => &r.fallback_texture_bg,
            };
            self.pass.set_bind_group(1, bg, &[]);
        }
        fn draw(&mut self, caster: usize) {
            let mesh = &self.r.gpu_instanced[caster];
            self.pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
            self.pass.set_vertex_buffer(1, mesh.instance_buf.slice(..));
            self.pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
            self.pass.draw_indexed(0..mesh.index_count, 0, 0..mesh.instance_count);
        }
    }
    let now_ms = anim_now_ms();
    {
        let mut sink = PassSink { r, pass: &mut pass };
        execute_instanced_shadow_plan(&r.gpu_instanced, now_ms, &mut sink);
    }

    // Character casters, skinned sub-pass then static sub-pass. Which sub-pass a caster lands in,
    // the one-time pipeline/group-0 bind per sub-pass, and the fact that only skinned casters bind a
    // joint palette all come from `plan_character_shadow_draws` (#739), device-free and unit tested
    // in tests/character_shadow_routing.rs (with a differential pin against the pre-#739 loops).
    // `CharacterSink` below is the `wgpu`-handle translation: five bodies, none reachable by a test.
    // Four are straight lookups (`shadow_skinned`/`shadow_static`, `light_depth_bg`,
    // `shadow_uniform_pool[u_slot]`, `shadow_joint_pool[j_slot]`). `draw` re-matches the caster's own
    // variant, which is a branch but not a decision: the two arms differ only in the concrete model
    // type and are otherwise the same loop, so they cannot be swapped — the compiler rejects it.
    // Contrast `encode_zone_pass`'s `ZoneSink`, where two bodies DO decide and are ungraded; see the
    // note there. Do not paraphrase this as "decides nothing" — an earlier draft did, and the
    // equivalent sentence in the zone pass was measurably false.
    impl CharacterShadowCaster for Caster<'_> {
        fn shadow_bind(&self) -> CharacterShadowBind {
            match *self {
                Caster::Skinned { u_slot, j_slot, .. } =>
                    CharacterShadowBind::Skinned { u_slot, j_slot },
                Caster::Static { u_slot, .. } => CharacterShadowBind::Static { u_slot },
            }
        }
    }
    struct CharacterSink<'r, 'p, 'e, 'c> {
        r:       &'r EqRenderer,
        pass:    &'p mut wgpu::RenderPass<'e>,
        casters: &'c [Caster<'c>],
    }
    impl CharacterShadowSink for CharacterSink<'_, '_, '_, '_> {
        fn set_pipeline(&mut self, kind: CharacterShadowKind) {
            let r = self.r;
            self.pass.set_pipeline(match kind {
                CharacterShadowKind::Skinned => &r.pipelines.shadow_skinned,
                CharacterShadowKind::Static  => &r.pipelines.shadow_static,
            });
        }
        fn bind_light_depth(&mut self) {
            let r = self.r;
            self.pass.set_bind_group(0, &r.light_depth_bg, &[]);
        }
        fn bind_model_uniform(&mut self, u_slot: usize) {
            let r = self.r;
            self.pass.set_bind_group(1, &r.shadow_uniform_pool[u_slot].1, &[]);
        }
        fn bind_joints(&mut self, j_slot: usize) {
            let r = self.r;
            self.pass.set_bind_group(2, &r.shadow_joint_pool[j_slot].1, &[]);
        }
        fn draw(&mut self, caster: usize) {
            // One draw per mesh of the caster's model. Mesh count is not a routing decision, so it
            // is not in the plan.
            match &self.casters[caster] {
                Caster::Skinned { model, .. } => {
                    for mesh in &model.meshes {
                        self.pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                        self.pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                        self.pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                    }
                }
                Caster::Static { model, .. } => {
                    for mesh in &model.meshes {
                        self.pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                        self.pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                        self.pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                    }
                }
            }
        }
    }
    {
        let mut sink = CharacterSink { r, pass: &mut pass, casters: &casters };
        execute_character_shadow_plan(&casters, &mut sink);
    }
}

/// Weather precipitation pass (eqoxide#542). Draws an instanced billboard particle field (rain
/// streaks or snow flakes) around the camera, density scaled by the server weather intensity. When
/// weather is clear the plan's count is 0 and the pass is skipped entirely — the clean on/off
/// transition. Alpha-blended, depth-tested against the scene (so precipitation is occluded by
/// geometry in front of it) but depth-write off. Drawn after the world/entity passes.
///
/// `cam_right`/`cam_up` are the world-space camera basis (for billboarding); `time_sec` drives the
/// fall animation. The per-frame uniform is written here (like `encode_door_pass`): queue writes
/// are applied before the command buffer runs, so this ordering is safe.
pub fn encode_weather_pass(
    r:         &EqRenderer,
    encoder:   &mut wgpu::CommandEncoder,
    view:      &wgpu::TextureView,
    scene:     &SceneState,
    cam_right: [f32; 3],
    cam_up:    [f32; 3],
) {
    use eqoxide_core::weather::{particle_plan, WeatherKind};
    let plan = particle_plan(&scene.weather);
    if plan.count == 0 {
        return; // clear weather → nothing to draw (on/off transition handled by the count)
    }

    // Per-kind look: snow falls slowly with larger soft flakes; rain falls fast as thin streaks.
    // Box sizes (EQ units) span the near field around the third-person camera (~80 back / 40 up).
    let snow = plan.kind == WeatherKind::Snow;
    let (fall, psize, box_xy, box_h) = if snow {
        (22.0f32, 1.6f32, 220.0f32, 180.0f32)
    } else {
        (150.0f32, 6.0f32, 200.0f32, 170.0f32)
    };
    // Mild alpha ramp with intensity on top of the density ramp (both read as "heavier weather").
    let intensity = scene.weather.intensity.max(1) as f32;
    let alpha = (0.55 + 0.12 * (intensity - 1.0)).min(1.0);
    let time_sec = anim_now_ms() as f32 / 1000.0;

    let data = crate::gpu::WeatherUniformData {
        right:   [cam_right[0], cam_right[1], cam_right[2], 0.0],
        up:      [cam_up[0], cam_up[1], cam_up[2], 0.0],
        params:  [time_sec, if snow { 1.0 } else { 0.0 }, box_xy, box_h],
        params2: [fall, psize, alpha, 0.0],
    };
    r.queue.write_buffer(&r.weather.uniform_buf, 0, bytemuck::bytes_of(&data));

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("weather"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view, resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None, occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.weather);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    pass.set_bind_group(1, &r.weather.bind_group, &[]);
    pass.set_vertex_buffer(0, r.weather.quad_buf.slice(..));
    pass.set_vertex_buffer(1, r.weather.instance_buf.slice(..));
    pass.draw(0..6, 0..plan.count);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_shadow_pass_has_correct_signature() {
        let _: fn(
            &crate::renderer::EqRenderer,
            &mut wgpu::CommandEncoder,
            &crate::scene::SceneState,
            [f32; 3],
        ) = encode_shadow_pass;
    }

    #[test]
    fn encode_weather_pass_has_correct_signature() {
        let _: fn(
            &EqRenderer,
            &mut wgpu::CommandEncoder,
            &wgpu::TextureView,
            &SceneState,
            [f32; 3],
            [f32; 3],
        ) = encode_weather_pass;
    }

    #[test]
    fn encode_sky_pass_has_correct_signature() {
        let _: fn(
            &crate::renderer::EqRenderer,
            &mut wgpu::CommandEncoder,
            &wgpu::TextureView,
        ) = encode_sky_pass;
    }

    #[test]
    fn encode_zone_pass_has_correct_signature() {
        let _: fn(
            &crate::renderer::EqRenderer,
            &mut wgpu::CommandEncoder,
            &wgpu::TextureView,
            &crate::scene::SceneState,
        ) = encode_zone_pass;
    }

    #[test]
    fn encode_billboard_pass_has_correct_signature() {
        let _: fn(
            &crate::renderer::EqRenderer,
            &mut wgpu::CommandEncoder,
            &wgpu::TextureView,
            &crate::scene::SceneState,
            [f32; 3],
            [f32; 3],
        ) = encode_billboard_pass;
    }

    #[test]
    fn encode_player_pass_has_correct_signature() {
        let _: fn(
            &crate::renderer::EqRenderer,
            &mut wgpu::CommandEncoder,
            &wgpu::TextureView,
            &crate::scene::SceneState,
        ) = encode_player_pass;
    }

    #[test]
    fn encode_entity_pass_has_correct_signature() {
        let _: fn(
            &crate::renderer::EqRenderer,
            &mut wgpu::CommandEncoder,
            &wgpu::TextureView,
            &crate::scene::SceneState,
            [f32; 3],
        ) = encode_entity_pass;
    }

    #[test]
    fn encode_skinned_entity_pass_has_correct_signature() {
        let _: fn(
            &crate::renderer::EqRenderer,
            &mut wgpu::CommandEncoder,
            &wgpu::TextureView,
            &crate::scene::SceneState,
            [f32; 3],
        ) = encode_skinned_entity_pass;
    }
}
