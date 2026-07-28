//! **eqoxide#741** — coverage for `encode_zone_pass`'s `draw_static`/`draw_instanced` routing.
//!
//! The colour pass issues six sub-passes: {static terrain, instanced placed objects} × {opaque and
//! masked, blend, additive}. Before #741 those were two closures called six times inside a function
//! taking `&EqRenderer` + `&mut wgpu::CommandEncoder`, so **no test could reach them**: neither
//! `wgpu::Device` nor `wgpu::Queue` has a non-adapter constructor and wgpu 22 has no `noop` backend,
//! so an integration test cannot build the arguments at all. A flipped mesh list, a dropped
//! sub-pass, a widened mode filter or a lost texture rebind was invisible to the whole suite.
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
//! - **The six-arm pipeline lookup in `encode_zone_pass`'s `ZoneSink::set_pipeline`.** That match
//!   turns a `(ZoneMeshSource, ZoneBlendClass)` pair into a `&wgpu::RenderPipeline`, and this file's
//!   sink turns the same pair into a `&'static str`. Swapping two arms *in the production sink* is
//!   not observable from here. That is a real hole and it is left open deliberately: pinning it
//!   would mean asserting on the source text of `pass.rs`, and a source-text pin has now been
//!   measured on this crate to be evadable by shadowing a local (#773 review, evasions E1b/E2). The
//!   honest boundary is that everything which *decides* is graded and the handle lookup is not.
//! - **wgpu semantics.** Nothing here creates a `wgpu::RenderPass`, so a command sequence that is
//!   correct as a sequence but invalid as wgpu (wrong group index for a layout, say) still passes.
//! - **Whether a flip is visible in play.** #741 asked; it is still not established. A wrong
//!   pipeline for a blend mesh would render it opaque or not at all, but nobody has looked, and
//!   there is no live before/after in this PR.

use eqoxide_assets::RenderMode;
use eqoxide_renderer::pass::{
    execute_zone_draw_plan, plan_zone_draws, ZoneBlendClass, ZoneDrawMesh, ZoneDrawSink,
    ZoneMeshSource, ZoneTexBind, ZONE_SUBPASSES,
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
#[test]
fn the_extracted_plan_emits_the_pre_741_command_stream() {
    let cases: Vec<(&str, Vec<Mesh>, Vec<Mesh>, u64)> = vec![
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
    ];

    for (name, statics, instanced, now_ms) in &cases {
        assert_eq!(new(statics, instanced, *now_ms), old(statics, instanced, *now_ms),
            "#741: the extracted zone plan diverged from the pre-#741 closures on case {name:?}");
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
/// genuinely spans sub-passes — mutation Z7, which turns this red.
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
