//! **eqoxide#741** — coverage for `encode_zone_pass`'s `draw_static`/`draw_instanced` routing.
//!
//! The colour pass issues six sub-passes: {static terrain, instanced placed objects} × {opaque and
//! masked, blend, additive}. Before #741 those were two closures called six times inside a function
//! taking `&EqRenderer` + `&mut wgpu::CommandEncoder`, so **no test could reach them**: neither
//! `wgpu::Device` nor `wgpu::Queue` has a non-adapter constructor and wgpu 22 has no `noop` backend,
//! so an integration test cannot build the arguments at all. A flipped mesh list, a dropped
//! sub-pass, a widened mode filter or a lost texture rebind was invisible to the whole suite.
//!
//! Do not read that sentence as a list of what this file fixed, and read the four items at
//! different strengths. A dropped sub-pass, a widened mode filter and a lost texture rebind are
//! graded **semantically**, by driving the real executor (mutations Z3, Z2 and Z4 of the #784
//! table). A flipped mesh list is graded only as **source text**: the two lists are interchangeable
//! to the type system at the call site, so no semantic test in this crate can tell them apart, and
//! the only thing that catches the swap is the call-site pin at the end of this file. See the
//! call-site bullet under "What this file does NOT cover" for what that pin does and does not buy.
//!
//! What is graded here is the **command sequence** `encode_zone_pass` emits, produced by the real
//! executor (`pass::execute_zone_draw_plan` — the exact function the pass calls) driven into a
//! recording sink. Two kinds of assertion:
//!
//! - a **differential pin** (`old()` below is a verbatim transcription of the pre-#741 closures at
//!   merge-base `3d60bfd`) that discharges "#741 changed no rendering behaviour", and
//! - **behavioural** tests of the routing itself: the mode partition, the sub-pass order, and the
//!   texture-cache reset at a sub-pass boundary.
//!
//! ## What this file does NOT cover
//!
//! - **All five bodies of `encode_zone_pass`'s `ZoneSink`.** This file supplies its own sink, so
//!   nothing it does can reach the production one. Enumerated, in the style `shadow_routing.rs`
//!   already uses for the instanced sub-pass:
//!
//!   1. `set_pipeline` — the six-arm `(ZoneMeshSource, ZoneBlendClass)` → `&wgpu::RenderPipeline`
//!      match. Swapping two arms there is invisible here, because this file's sink maps the same
//!      pair to a `&'static str`.
//!   2. `bind_texture` — `texture_bind_groups[i]` vs `fallback_texture_bg`. The in-range
//!      **decision** here was pure, so it is no longer in the sink: it is
//!      `pass::zone_texture_slot`, graded by
//!      `a_texture_index_falls_back_exactly_when_it_is_out_of_range` below. What is left in the sink
//!      is the handle lookup, and *that* is still ungraded — a sink body that discarded the slot and
//!      always bound the fallback would untexture the whole zone silently (mutation S2b).
//!   3. `draw` — `gpu_meshes` vs `gpu_instanced`, and the static vs instanced buffer/draw form.
//!      **This one decides.** Pointing the `Static` arm at `gpu_instanced` draws the wrong geometry
//!      for every static zone mesh (mutation S1).
//!   4. `bind_camera` — `camera_uniform.bind_group` at group 0. A straight lookup.
//!   5. `bind_shadow_sample` — `shadow_sample_bg` at group 2. A straight lookup.
//!
//!   **An earlier draft of this paragraph claimed "everything which *decides* is graded and the
//!   handle lookup is not". That was false**, and items 2 and 3 are why: the #784 reviewer mutated
//!   both in the production sink and both left the crate green. Neither is a regression — both
//!   survive identically on the base file — but the description was wrong about where the hole is
//!   and how wide it is.
//!
//!   A second draft then justified leaving *both* alone as "a refactor rather than the coverage this
//!   issue asked for". That was right for item 3 and wrong for item 2, whose predicate depends on
//!   nothing but an index and a length: extracting it cost six lines, which is the same shape this
//!   file already applies everywhere else, and this impl's own rule at `pass.rs` says a condition in
//!   the sink belongs somewhere testable. So item 2's decision is now graded and item 3 is not.
//!   Item 3 genuinely does need an indirection between the plan and the handles — its two arms reach
//!   `wgpu::RenderPass` differently over two different concrete mesh types — and S1 **remains
//!   ungraded after this PR**, as does the residual lookup in item 2 (S2b).
//!
//!   "Left the crate green" is the claim; the pass/fail *totals* for every mutation named in this
//!   file are in the #784 PR body, against the head they were measured at. An earlier draft of this
//!   paragraph carried a bare "226 passed" here, and it was already stale by one commit — a count in
//!   a tracked file goes wrong the moment anyone adds a test, which is exactly what this PR does.
//!
//!   Item 1 is additionally left open *deliberately* rather than for cost: pinning it would mean
//!   asserting on the source text of `pass.rs`, and a source-text pin has now been measured on this
//!   crate to be evadable by shadowing a local (#773 review, evasions E1b/E2).
//! - **That the plan is executed, *semantically*.** Nothing in the sink-driven tests below would
//!   fail if the `execute_zone_draw_plan` call in `encode_zone_pass` were deleted or wrapped in
//!   `if false` — the same gap `shadow_routing.rs` records for its own call site, and the reason
//!   this bullet exists in that file's wording.
//!
//!   The zone call site is **worse than the one that bullet covers**, because the executor is
//!   generic in *both* list parameters and `GpuMesh` and `GpuInstancedMesh` both implement
//!   `ZoneDrawMesh`: the two arguments are interchangeable to the type system. Swapping them
//!   compiles with no error, and every static mesh is then planned from the instanced list's modes
//!   and textures and drawn out of the other list. Deleting the call renders nothing, which is loud;
//!   swapping its arguments renders the whole zone wrongly, which is silent — and by this project's
//!   ordering the silent one is the worse failure, which is why it is pinned rather than only
//!   disclosed. Measured: before that pin existed the swap left the whole crate green (#784 round 2,
//!   mutation S3).
//!
//!   `encode_zone_pass_executes_the_plan_on_the_lists_in_this_order`, at the end of this file,
//!   closes the swap and *only* the swap. It is a **source-text** assert — same technique and same
//!   caveat as `shadow_caster_selection.rs`'s call-site pin and `shadow_routing.rs`'s pipeline pin —
//!   so it proves the call is *written* with `gpu_meshes` first, not that it is *reached*: an
//!   `if false` around it, or a second inlined draw loop added beside it, still passes. Treat it as
//!   a backstop over the one call this crate cannot make device-free, not as a semantic grader.
//!
//!   `now_ms` is only weakly covered by the same pin: it pins the *spelling* `now_ms` at the call,
//!   so a different clock behind that name would pass. Every test below passes `now_ms` explicitly.
//!
//!   This bullet is here because round 2 of the #784 review enumerated the sink and stopped at its
//!   boundary. The question this heading answers is "what can still break silently", not "what is in
//!   that struct", and the answer did not stop where the struct did.
//! - **wgpu semantics.** Nothing here creates a `wgpu::RenderPass`, so a command sequence that is
//!   correct as a sequence but invalid as wgpu (wrong group index for a layout, say) still passes.
//! - **Whether a flip is visible in play.** #741 asked; it is still not established. A wrong
//!   pipeline for a blend mesh would render it opaque or not at all, but nobody has looked, and
//!   there is no live before/after in this PR.

use eqoxide_assets::RenderMode;
use eqoxide_renderer::pass::{
    execute_zone_draw_plan, plan_zone_draws, zone_texture_slot, ZoneBlendClass, ZoneDrawMesh,
    ZoneDrawSink, ZoneMeshSource, ZoneTexBind, ZONE_SUBPASSES,
};

#[derive(Debug, PartialEq, Eq, Clone)]
enum Cmd {
    SetPipeline(&'static str),
    BindCamera,
    BindTex(Option<usize>),
    BindShadow,
    DrawStatic(usize),
    DrawInstanced(usize),
}

struct Mesh {
    mode: RenderMode,
    tex:  Option<usize>,
    anim: Option<(u32, Vec<usize>)>,
}

impl Mesh {
    fn new(mode: RenderMode, tex: Option<usize>) -> Self {
        Mesh { mode, tex, anim: None }
    }
    fn animated(mode: RenderMode, ms: u32, frames: &[usize]) -> Self {
        Mesh { mode, tex: Some(999), anim: Some((ms, frames.to_vec())) }
    }
}

impl ZoneDrawMesh for Mesh {
    fn render_mode(&self) -> RenderMode { self.mode }
    fn texture_idx(&self) -> Option<usize> { self.tex }
    fn anim(&self) -> Option<&(u32, Vec<usize>)> { self.anim.as_ref() }
}

/// Verbatim transcription of `encode_zone_pass`'s two closures and their six call sites at
/// `3d60bfd` (the merge-base of this branch), with `wgpu` handles replaced by the names of the
/// fields they were read from. Nothing here is shared with the production code under test.
fn old(statics: &[Mesh], instanced: &[Mesh], now_ms: u64) -> Vec<Cmd> {
    let frame_tex = |tex: Option<usize>, anim: &Option<(u32, Vec<usize>)>| -> Option<usize> {
        match anim {
            Some((ms, frames)) if !frames.is_empty() => {
                Some(frames[(now_ms / (*ms).max(1) as u64) as usize % frames.len()])
            }
            _ => tex,
        }
    };
    fn draw_static(
        out: &mut Vec<Cmd>, meshes: &[Mesh], pipeline: &'static str, modes: &[RenderMode],
        frame_tex: &dyn Fn(Option<usize>, &Option<(u32, Vec<usize>)>) -> Option<usize>,
    ) {
        let mut bound = false;
        let mut current_tex: Option<usize> = None;
        for (i, mesh) in meshes.iter().enumerate() {
            if !modes.contains(&mesh.mode) { continue; }
            let etex = frame_tex(mesh.tex, &mesh.anim);
            if !bound {
                out.push(Cmd::SetPipeline(pipeline));
                out.push(Cmd::BindCamera);
                out.push(Cmd::BindTex(etex));
                out.push(Cmd::BindShadow);
                current_tex = etex;
                bound = true;
            } else if etex != current_tex {
                current_tex = etex;
                out.push(Cmd::BindTex(current_tex));
            }
            out.push(Cmd::DrawStatic(i));
        }
    }
    fn draw_instanced(
        out: &mut Vec<Cmd>, meshes: &[Mesh], pipeline: &'static str, modes: &[RenderMode],
        frame_tex: &dyn Fn(Option<usize>, &Option<(u32, Vec<usize>)>) -> Option<usize>,
    ) {
        let mut bound = false;
        let mut current_tex: Option<usize> = None;
        for (i, mesh) in meshes.iter().enumerate() {
            if !modes.contains(&mesh.mode) { continue; }
            let etex = frame_tex(mesh.tex, &mesh.anim);
            if !bound {
                out.push(Cmd::SetPipeline(pipeline));
                out.push(Cmd::BindCamera);
                out.push(Cmd::BindTex(etex));
                out.push(Cmd::BindShadow);
                current_tex = etex;
                bound = true;
            } else if etex != current_tex {
                current_tex = etex;
                out.push(Cmd::BindTex(current_tex));
            }
            out.push(Cmd::DrawInstanced(i));
        }
    }

    let mut out = Vec::new();
    let om = [RenderMode::Opaque, RenderMode::Masked];
    draw_static(&mut out, statics, "zone", &om, &frame_tex);
    draw_instanced(&mut out, instanced, "zone_instanced", &om, &frame_tex);
    draw_static(&mut out, statics, "zone_blend", &[RenderMode::Blend], &frame_tex);
    draw_instanced(&mut out, instanced, "zone_instanced_blend", &[RenderMode::Blend], &frame_tex);
    draw_static(&mut out, statics, "zone_additive", &[RenderMode::Additive], &frame_tex);
    draw_instanced(&mut out, instanced, "zone_instanced_additive", &[RenderMode::Additive],
        &frame_tex);
    out
}

/// A device-free [`ZoneDrawSink`]: the same five methods `encode_zone_pass`'s `ZoneSink` implements
/// against a live `wgpu::RenderPass`, recording into a `Vec` instead.
#[derive(Default)]
struct Recorder {
    out: Vec<Cmd>,
}

impl ZoneDrawSink for Recorder {
    fn set_pipeline(&mut self, source: ZoneMeshSource, class: ZoneBlendClass) {
        self.out.push(Cmd::SetPipeline(match (source, class) {
            (ZoneMeshSource::Static,    ZoneBlendClass::OpaqueMasked) => "zone",
            (ZoneMeshSource::Static,    ZoneBlendClass::Blend)        => "zone_blend",
            (ZoneMeshSource::Static,    ZoneBlendClass::Additive)     => "zone_additive",
            (ZoneMeshSource::Instanced, ZoneBlendClass::OpaqueMasked) => "zone_instanced",
            (ZoneMeshSource::Instanced, ZoneBlendClass::Blend)        => "zone_instanced_blend",
            (ZoneMeshSource::Instanced, ZoneBlendClass::Additive)     => "zone_instanced_additive",
        }));
    }
    fn bind_camera(&mut self) { self.out.push(Cmd::BindCamera); }
    fn bind_texture(&mut self, idx: Option<usize>) { self.out.push(Cmd::BindTex(idx)); }
    fn bind_shadow_sample(&mut self) { self.out.push(Cmd::BindShadow); }
    fn draw(&mut self, source: ZoneMeshSource, mesh: usize) {
        self.out.push(match source {
            ZoneMeshSource::Static    => Cmd::DrawStatic(mesh),
            ZoneMeshSource::Instanced => Cmd::DrawInstanced(mesh),
        });
    }
}

fn new(statics: &[Mesh], instanced: &[Mesh], now_ms: u64) -> Vec<Cmd> {
    let mut rec = Recorder::default();
    execute_zone_draw_plan(statics, instanced, now_ms, &mut rec);
    rec.out
}

const ALL_MODES: [RenderMode; 4] =
    [RenderMode::Opaque, RenderMode::Masked, RenderMode::Blend, RenderMode::Additive];

// ── Differential pin ─────────────────────────────────────────────────────────────────────────────

/// The extracted planner must emit the byte-identical command stream the pre-#741 closures emitted,
/// over a corpus that exercises every mode in both lists, texture runs, animated meshes and empty
/// sub-passes.
///
/// This grades the new code against the OLD implementation, not against itself. **When to change
/// this test**: it deliberately freezes pre-#741 behaviour, so it is exactly what should fail if
/// someone later *intends* to change the zone draw order (say, sorting meshes by texture to cut
/// rebinds). Update it as part of that change; do not weaken it to make a diff go green.
/// The frame corpus: every mode in both lists, texture runs, animated meshes at three points of
/// their cycle, degenerate animations, and empty sub-passes. Shared by the differential pin and by
/// the `set_pipeline ⟹ Set` property below, so a case added for one is graded by both.
fn corpus() -> Vec<(&'static str, Vec<Mesh>, Vec<Mesh>, u64)> {
    vec![
        ("empty", vec![], vec![], 0),
        ("statics only, one of each mode",
            ALL_MODES.iter().map(|&m| Mesh::new(m, Some(1))).collect(), vec![], 0),
        ("instanced only, one of each mode",
            vec![], ALL_MODES.iter().map(|&m| Mesh::new(m, Some(1))).collect(), 0),
        ("both lists, interleaved modes",
            vec![Mesh::new(RenderMode::Blend, Some(4)), Mesh::new(RenderMode::Opaque, Some(1)),
                 Mesh::new(RenderMode::Additive, Some(7)), Mesh::new(RenderMode::Masked, Some(1))],
            vec![Mesh::new(RenderMode::Additive, Some(2)), Mesh::new(RenderMode::Opaque, None),
                 Mesh::new(RenderMode::Opaque, Some(3)), Mesh::new(RenderMode::Blend, Some(3))],
            0),
        ("texture runs, so the Keep elision is exercised",
            vec![Mesh::new(RenderMode::Opaque, Some(5)), Mesh::new(RenderMode::Opaque, Some(5)),
                 Mesh::new(RenderMode::Masked, Some(5)), Mesh::new(RenderMode::Opaque, Some(6)),
                 Mesh::new(RenderMode::Opaque, Some(5))],
            vec![], 0),
        ("animated meshes at t=0",
            vec![Mesh::animated(RenderMode::Opaque, 100, &[3, 4, 5])],
            vec![Mesh::animated(RenderMode::Blend, 250, &[8, 9])], 0),
        ("animated meshes mid-cycle",
            vec![Mesh::animated(RenderMode::Opaque, 100, &[3, 4, 5])],
            vec![Mesh::animated(RenderMode::Blend, 250, &[8, 9])], 1_234),
        ("animated meshes far into the cycle",
            vec![Mesh::animated(RenderMode::Masked, 40, &[1, 2, 3, 4])],
            vec![Mesh::animated(RenderMode::Additive, 7, &[0, 1])], 987_654_321),
        ("degenerate animation (zero ms, empty frame list)",
            vec![Mesh::animated(RenderMode::Opaque, 0, &[2, 2]),
                 Mesh { mode: RenderMode::Opaque, tex: Some(11), anim: Some((50, vec![])) }],
            vec![], 5_000),
    ]
}

#[test]
fn the_extracted_plan_emits_the_pre_741_command_stream() {
    for (name, statics, instanced, now_ms) in &corpus() {
        assert_eq!(new(statics, instanced, *now_ms), old(statics, instanced, *now_ms),
            "#741: the extracted zone plan diverged from the pre-#741 closures on case {name:?}");
    }
}

/// **A pipeline-setting step always carries `Set`, never `Keep`.**
///
/// This is the invariant the deleted `*started &&` guard used to assert by hand. Removing that
/// guard (see `the_texture_cache_does_not_survive_a_subpass_boundary`) was correct — it was
/// unreachable — but it left the invariant true only by construction and asserted nowhere, so a
/// later change to the cache could make a sub-pass start with group 1 holding the *previous*
/// sub-pass's texture and nothing would say so. This turns that reading argument into a check.
///
/// Universal over the whole corpus rather than one example, because the claim is universal.
#[test]
fn a_pipeline_setting_step_never_elides_its_texture_bind() {
    for (name, statics, instanced, now_ms) in &corpus() {
        for step in plan_zone_draws(statics, instanced, *now_ms) {
            if step.set_pipeline {
                assert!(matches!(step.bind, ZoneTexBind::Set(_)),
                    "case {name:?}: sub-pass {} starts with {:?} — the first step of a sub-pass must \
                     bind group 1, since the pipeline switch does not preserve it",
                    step.subpass, step.bind);
            }
        }
    }
}

// ── The routing itself ───────────────────────────────────────────────────────────────────────────

/// **The mode partition.** Every render mode is drawn by exactly one sub-pass of each source — never
/// twice (double-draw, wrong blending) and never zero times (invisible geometry). This is the
/// property the pre-#741 `modes: &[RenderMode]` lists had to satisfy by hand at six call sites.
#[test]
fn every_render_mode_is_drawn_exactly_once_per_source() {
    for mode in ALL_MODES {
        let statics = vec![Mesh::new(mode, Some(1))];
        let instanced = vec![Mesh::new(mode, Some(1))];
        let steps: Vec<_> = plan_zone_draws(&statics, &instanced, 0).collect();
        let s = steps.iter().filter(|s| s.source == ZoneMeshSource::Static).count();
        let i = steps.iter().filter(|s| s.source == ZoneMeshSource::Instanced).count();
        assert_eq!((s, i), (1, 1),
            "{mode:?} must be drawn exactly once from each mesh list, got \
             {s} static / {i} instanced draws");
    }
    // and the classes themselves partition the mode space
    for mode in ALL_MODES {
        let accepting = [ZoneBlendClass::OpaqueMasked, ZoneBlendClass::Blend,
                         ZoneBlendClass::Additive].iter().filter(|c| c.accepts(mode)).count();
        assert_eq!(accepting, 1, "{mode:?} must be accepted by exactly one blend class");
    }
}

/// **Sub-pass order.** All depth-writing geometry precedes all depth-write-off geometry, and within
/// a class the static terrain precedes the placed objects standing on it. Asserted on the plan, so a
/// reordering of `ZONE_SUBPASSES` fails here whether or not any mesh happens to be present.
#[test]
fn depth_writing_subpasses_run_before_transparent_ones_static_first() {
    let order: Vec<(ZoneMeshSource, ZoneBlendClass)> =
        ZONE_SUBPASSES.iter().map(|s| (s.source, s.class)).collect();
    assert_eq!(order, vec![
        (ZoneMeshSource::Static,    ZoneBlendClass::OpaqueMasked),
        (ZoneMeshSource::Instanced, ZoneBlendClass::OpaqueMasked),
        (ZoneMeshSource::Static,    ZoneBlendClass::Blend),
        (ZoneMeshSource::Instanced, ZoneBlendClass::Blend),
        (ZoneMeshSource::Static,    ZoneBlendClass::Additive),
        (ZoneMeshSource::Instanced, ZoneBlendClass::Additive),
    ], "#741: zone sub-pass order changed — transparent geometry must be drawn after every \
        depth-writing sub-pass, or it depth-occludes the opaque world behind it");

    // and the plan honours that order for real meshes
    let meshes: Vec<Mesh> = ALL_MODES.iter().map(|&m| Mesh::new(m, Some(1))).collect();
    let meshes2: Vec<Mesh> = ALL_MODES.iter().map(|&m| Mesh::new(m, Some(1))).collect();
    let subpasses: Vec<usize> =
        plan_zone_draws(&meshes, &meshes2, 0).map(|s| s.subpass).collect();
    assert!(subpasses.windows(2).all(|w| w[0] <= w[1]),
        "plan steps must come out in sub-pass order, got {subpasses:?}");
    assert_eq!(subpasses.len(), 8, "4 modes × 2 lists = 8 draws, got {}", subpasses.len());
}

/// **The texture cache is per sub-pass, not per frame.** Group 1 is bound by the pipeline-setting
/// step of each sub-pass, so the same texture used by the last mesh of one sub-pass and the first
/// mesh of the next must be bound again — the pipeline switch in between does not preserve it.
/// Eliding that rebind would leave the second sub-pass sampling whatever the first left behind.
///
/// **The limit of this test.** The reset is *structural* in `plan_zone_draws` — the `scan` seed is
/// re-evaluated per sub-pass — so no one-line condition controls it, and a guard added in front of
/// it is unreachable. That was measured, not assumed: mutation Z5 (`if *started && …`) left this
/// file and the whole crate green, which is why that dead guard was deleted rather than kept as
/// belt-and-braces. What this test does catch is the state being hoisted out of the `flat_map` so it
/// genuinely spans sub-passes — mutation Z7 — and the elision being removed outright — mutation Z4.
/// Both turn this red; Z7 is the one that targets the boundary specifically.
#[test]
fn the_texture_cache_does_not_survive_a_subpass_boundary() {
    // One blend mesh and one additive mesh, same texture, adjacent sub-passes of the same source.
    let statics = vec![Mesh::new(RenderMode::Blend, Some(7)), Mesh::new(RenderMode::Additive, Some(7))];
    let binds: Vec<ZoneTexBind> = plan_zone_draws(&statics, &[] as &[Mesh], 0).map(|s| s.bind).collect();
    assert_eq!(binds, vec![ZoneTexBind::Set(Some(7)), ZoneTexBind::Set(Some(7))],
        "each sub-pass must bind group 1 for its first mesh even when the previous sub-pass \
         already had that texture bound");

    // Within one sub-pass the elision still applies.
    let run = vec![Mesh::new(RenderMode::Opaque, Some(7)), Mesh::new(RenderMode::Masked, Some(7)),
                   Mesh::new(RenderMode::Opaque, Some(8))];
    let binds: Vec<ZoneTexBind> = plan_zone_draws(&run, &[] as &[Mesh], 0).map(|s| s.bind).collect();
    assert_eq!(binds, vec![ZoneTexBind::Set(Some(7)), ZoneTexBind::Keep, ZoneTexBind::Set(Some(8))],
        "a repeated texture within one sub-pass must not be rebound");
}

/// **`set_pipeline` marks the first step of a non-empty sub-pass and only that step.** An empty
/// sub-pass emits nothing at all — no pipeline switch, no binds — which is what keeps a zone with no
/// additive geometry from paying for two pipeline switches every frame.
#[test]
fn an_empty_subpass_emits_nothing_and_a_nonempty_one_sets_its_pipeline_once() {
    let statics = vec![Mesh::new(RenderMode::Opaque, Some(1)), Mesh::new(RenderMode::Opaque, Some(2))];
    let cmds = new(&statics, &[] as &[Mesh], 0);
    assert_eq!(cmds.iter().filter(|c| matches!(c, Cmd::SetPipeline(_))).count(), 1,
        "one non-empty sub-pass must set exactly one pipeline, got {cmds:?}");
    assert_eq!(cmds.iter().filter(|c| *c == &Cmd::BindCamera).count(), 1);
    assert_eq!(cmds.iter().filter(|c| *c == &Cmd::BindShadow).count(), 1);
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::DrawInstanced(_))),
        "an empty instanced list must produce no instanced draw");

    let steps: Vec<_> = plan_zone_draws(&statics, &[] as &[Mesh], 0).collect();
    assert_eq!(steps.iter().filter(|s| s.set_pipeline).count(), 1);
    assert!(steps[0].set_pipeline, "the first step of a sub-pass sets the pipeline");
}

/// The one **decision** `ZoneSink::bind_texture` used to make inline: an index the renderer has a
/// bind group for binds that group, anything else binds the fallback. A mesh whose `texture_idx` is
/// stale or out of range must not index past `texture_bind_groups` (that panics) and must not
/// silently bind a *different* mesh's texture either.
///
/// This is graded only because the predicate is pure — it depends on the index and a length and
/// nothing else — so #741 lifted it to `pass::zone_texture_slot`. What stayed in the sink is the
/// handle lookup, which still needs a device; see the call-site bullet at the top of this file.
#[test]
fn a_texture_index_falls_back_exactly_when_it_is_out_of_range() {
    assert_eq!(zone_texture_slot(None, 4), None, "an untextured mesh binds the fallback");
    assert_eq!(zone_texture_slot(Some(0), 4), Some(0));
    assert_eq!(zone_texture_slot(Some(3), 4), Some(3), "the last valid index is in range");
    assert_eq!(zone_texture_slot(Some(4), 4), None, "one past the end is out of range");
    assert_eq!(zone_texture_slot(Some(9), 4), None);
    assert_eq!(zone_texture_slot(Some(0), 0), None, "no bind groups at all: everything falls back");

    // The property behind the cases: in-range indices are passed through UNCHANGED (binding a
    // neighbour's texture would be silent), and nothing else resolves to a slot.
    for n in 0..6usize {
        for i in 0..8usize {
            assert_eq!(zone_texture_slot(Some(i), n), (i < n).then_some(i),
                "idx {i} against {n} bind groups");
        }
    }
}

/// **A mesh's index is its index in its own list**, not its position within the sub-pass — the sink
/// uses it to index `gpu_meshes`/`gpu_instanced` directly, so an off-by-one here draws the wrong
/// geometry.
#[test]
fn a_step_carries_the_meshs_index_in_its_own_list() {
    let statics = vec![Mesh::new(RenderMode::Blend, Some(1)), Mesh::new(RenderMode::Opaque, Some(1)),
                       Mesh::new(RenderMode::Blend, Some(1))];
    let blend: Vec<usize> = plan_zone_draws(&statics, &[] as &[Mesh], 0)
        .filter(|s| s.class == ZoneBlendClass::Blend).map(|s| s.mesh).collect();
    assert_eq!(blend, vec![0, 2],
        "the blend sub-pass skips mesh 1, but its steps must still name meshes 0 and 2");
}

// ── Call-site pin (source text, not semantics) ──────────────────────────────────────────────────

const PASS_RS: &str = include_str!("../src/pass.rs");

/// **The one thing in this file's subject that no semantic test in this crate can grade.**
///
/// `encode_zone_pass` ends with
///
/// ```text
/// execute_zone_draw_plan(&r.gpu_meshes, &r.gpu_instanced, now_ms, &mut sink);
/// ```
///
/// and `execute_zone_draw_plan<S: ZoneDrawMesh, I: ZoneDrawMesh, K: ZoneDrawSink>` is generic in
/// **both** list parameters, with `GpuMesh` and `GpuInstancedMesh` both implementing `ZoneDrawMesh`.
/// The two arguments are therefore interchangeable to the type system: swapping them compiles with
/// no error, plans every static mesh from the instanced list's modes and textures, draws it out of
/// the wrong list, and — measured by the #784 round-2 reviewer as mutation S3 — left the whole
/// crate green. Everything else this file grades is reached through its own sink, which the
/// production call site cannot affect.
///
/// It cannot be graded semantically here: `EqRenderer` owns `wgpu` handles and no test can build
/// one, which is the whole premise of this file. So it is pinned as **source text**, the same
/// technique (and the same caveat) as `shadow_caster_selection.rs`'s
/// `encode_shadow_pass_calls_the_planner_this_file_grades` and `shadow_routing.rs`'s
/// `executor_binds_each_kind_to_the_pipeline_this_file_grades`.
///
/// **What this pin does not prove**, stated so nobody upgrades it by reading:
///
/// * That the call is *reached*. Wrapping it in `if false`, or returning early above it, passes.
/// * That it is the *only* draw path. A second inlined loop added beside it passes.
/// * That `now_ms` is the right clock. Only the identifier is pinned, not what it holds.
/// * It is a guard over text, and guards over text are evadable. Whole-line `//` comments are
///   stripped so a decoy comment cannot satisfy it, but per #721's measured evasion A2b a
///   *trailing* comment on a code line still can, and per #773's E1b/E2 shadowing a local can defeat
///   a pin of this shape. It is a backstop, not the mechanism.
///
/// **Measured, both ways** (totals in the #784 round-3 comment, against the head they were measured
/// at — a count written *here* goes stale the moment anyone adds a test, which is R4 of that same
/// review). Re-applying S3 turns this test RED — it is the *first* assert below that fires — and,
/// paired with a deletion mutation on the sibling call that `character_shadow_routing.rs` pins,
/// exactly those two tests move from `ok` to `FAILED` and **no other test in the crate changes
/// state**. So this pin has teeth and is not over-broad.
///
/// The second assert below adds the one thing the first cannot: the first is satisfied by a correct
/// line even if a swapped one sits beside it, and the second fails on the swapped spelling
/// **anywhere** in the function. It is not what caught S3; it is what stops S3 being reintroduced
/// alongside a surviving correct call.
#[test]
fn encode_zone_pass_executes_the_plan_on_the_lists_in_this_order() {
    let pass = PASS_RS
        .split_once("pub fn encode_zone_pass(")
        .expect("encode_zone_pass not found in pass.rs")
        .1;
    let body = pass.split_once("\npub fn ").map(|(b, _)| b).unwrap_or(pass);
    // Whole-line comments are stripped: this function's own ⚠ note spells out the swapped order on
    // purpose, and must not be able to satisfy or trip either assert.
    let code: String = body
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let flat: String = code.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        flat.contains("execute_zone_draw_plan(&r.gpu_meshes, &r.gpu_instanced, now_ms, &mut sink)"),
        "encode_zone_pass must execute the plan with the STATIC list first and the INSTANCED list \
         second. Both implement ZoneDrawMesh, so the wrong order compiles and renders the whole \
         zone out of the wrong lists in silence. If this line was legitimately reformatted, update \
         this pin AND re-check the order — do not make the pin pass by moving text.",
    );
    assert!(
        !flat.contains("execute_zone_draw_plan(&r.gpu_instanced, &r.gpu_meshes"),
        "encode_zone_pass draws the static sub-passes out of gpu_instanced and vice versa \
         (mutation S3). Every static zone mesh is planned from the wrong list's render modes and \
         texture indices and then drawn from the wrong buffers.",
    );
}
