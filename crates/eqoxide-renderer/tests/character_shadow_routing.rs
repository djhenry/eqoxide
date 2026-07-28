//! **eqoxide#739** — coverage for the skinned and static character shadow sub-passes.
//!
//! #737 built a device-free draw planner for the **instanced** shadow sub-pass and proved it has
//! teeth. The skinned and static sub-passes that follow it in `encode_shadow_pass` made the same
//! kind of pipeline/bind decision with no equivalent coverage: two hand-written loops with a `bool`
//! latch each, inside a function taking `&EqRenderer` + `&mut wgpu::CommandEncoder`, which no test
//! can build (no non-adapter constructor for `wgpu::Device`/`Queue`, no `noop` backend in wgpu 22).
//! A flipped pipeline, a deleted sub-pass, or a static caster that bound a joint palette was
//! invisible to the whole suite.
//!
//! ## #739 asked whether this shares #737's vocabulary. It does not.
//!
//! The issue warned explicitly against assuming the sub-passes unify, citing a #721 correction where
//! exactly that assumption was wrong. Checked rather than assumed, and they do not:
//!
//! | | instanced (#721) | character (#739) |
//! |---|---|---|
//! | what picks the pipeline | the mesh's `RenderMode` | whether the *model* is skinned |
//! | what group 1 holds | a diffuse texture index | a `shadow_uniform_pool` slot |
//! | group 2 | never bound | a `shadow_joint_pool` slot, skinned only |
//! | per-step elision | redundant texture rebinds elided | none — every caster binds its own slot |
//!
//! `ShadowTexBind` cannot name "bind pool slot 3", and `ShadowPipelineKind::for_render_mode` has no
//! meaning for a character. The one genuinely shared shape is "one latch per sub-pass", three lines.
//! So `plan_character_shadow_draws` is a separate planner with its own vocabulary, and this file is
//! separate from `shadow_routing.rs`.
//!
//! ## What is a type here rather than a test
//!
//! `CharacterShadowBind::Skinned` carries the joint slot and `::Static` does not, so a static step
//! **cannot** carry a joint palette — that bad state is unrepresentable, not merely untested. The
//! tests below cover what the type cannot: that each caster lands in the right sub-pass, in the
//! right order, and that the executor calls `bind_joints` exactly for the skinned ones.
//!
//! ## What this file does NOT cover
//!
//! - **The two-arm pipeline lookup in `encode_shadow_pass`'s `CharacterSink::set_pipeline`.**
//!   Swapping `shadow_skinned` and `shadow_static` *there* is not observable from here, because this
//!   file's sink maps the same `CharacterShadowKind` to a string. Left open deliberately rather than
//!   pinned as source text: a source-text pin has been measured on this crate to be evadable by
//!   shadowing a local (#773 review, evasions E1b/E2), so it would buy a claim it cannot keep.
//! - **wgpu semantics**, and **whether a flip is visible in play** — #739 listed the second as not
//!   established, and this PR does not settle it. No client was run.
//! - **Which entities are selected at all** — that is `plan_shadow_casters` (#740), graded in
//!   `shadow_caster_selection.rs`. This file starts from an already-selected caster list.

use eqoxide_renderer::pass::{
    execute_character_shadow_plan, plan_character_shadow_draws, CharacterShadowBind,
    CharacterShadowCaster, CharacterShadowKind, CharacterShadowSink, CHARACTER_SHADOW_SUBPASSES,
};

#[derive(Debug, PartialEq, Eq, Clone)]
enum Cmd {
    SetPipeline(&'static str),
    BindGroup0,
    BindGroup1(usize),
    BindGroup2(usize),
    Draw(usize),
}

/// A selected caster, reduced to what ROUTING reads — the same reduction `encode_shadow_pass`'s
/// `Caster` enum implements against real `GpuSkinnedModel`/`GpuStaticModel` references.
enum Caster {
    Skinned { u_slot: usize, j_slot: usize },
    Static  { u_slot: usize },
}

impl CharacterShadowCaster for Caster {
    fn shadow_bind(&self) -> CharacterShadowBind {
        match *self {
            Caster::Skinned { u_slot, j_slot } => CharacterShadowBind::Skinned { u_slot, j_slot },
            Caster::Static  { u_slot }         => CharacterShadowBind::Static { u_slot },
        }
    }
}

/// Verbatim transcription of `encode_shadow_pass`'s two character loops at `3d60bfd` (the merge-base
/// of this branch), with `wgpu` handles replaced by the names/indices of what they were read from.
/// The per-mesh inner loop is collapsed to one `Draw` because mesh count is not a routing decision
/// and is not in the plan; both sides collapse it identically.
fn old(casters: &[Caster]) -> Vec<Cmd> {
    let mut out = Vec::new();

    let mut skinned_bound = false;
    for (i, c) in casters.iter().enumerate() {
        if let Caster::Skinned { u_slot, j_slot } = c {
            if !skinned_bound {
                out.push(Cmd::SetPipeline("shadow_skinned"));
                out.push(Cmd::BindGroup0);
                skinned_bound = true;
            }
            out.push(Cmd::BindGroup1(*u_slot));
            out.push(Cmd::BindGroup2(*j_slot));
            out.push(Cmd::Draw(i));
        }
    }

    let mut static_bound = false;
    for (i, c) in casters.iter().enumerate() {
        if let Caster::Static { u_slot } = c {
            if !static_bound {
                out.push(Cmd::SetPipeline("shadow_static"));
                out.push(Cmd::BindGroup0);
                static_bound = true;
            }
            out.push(Cmd::BindGroup1(*u_slot));
            out.push(Cmd::Draw(i));
        }
    }

    out
}

/// A device-free [`CharacterShadowSink`]: the same five methods `encode_shadow_pass`'s
/// `CharacterSink` implements against a live `wgpu::RenderPass`, recording into a `Vec` instead.
#[derive(Default)]
struct Recorder {
    out: Vec<Cmd>,
}

impl CharacterShadowSink for Recorder {
    fn set_pipeline(&mut self, kind: CharacterShadowKind) {
        self.out.push(Cmd::SetPipeline(match kind {
            CharacterShadowKind::Skinned => "shadow_skinned",
            CharacterShadowKind::Static  => "shadow_static",
        }));
    }
    fn bind_light_depth(&mut self) { self.out.push(Cmd::BindGroup0); }
    fn bind_model_uniform(&mut self, u_slot: usize) { self.out.push(Cmd::BindGroup1(u_slot)); }
    fn bind_joints(&mut self, j_slot: usize) { self.out.push(Cmd::BindGroup2(j_slot)); }
    fn draw(&mut self, caster: usize) { self.out.push(Cmd::Draw(caster)); }
}

fn new(casters: &[Caster]) -> Vec<Cmd> {
    let mut rec = Recorder::default();
    execute_character_shadow_plan(casters, &mut rec);
    rec.out
}

/// Casters in the interleaved order `plan_shadow_casters` actually produces — the player first, then
/// nearby characters nearest-first, so skinned and static are mixed rather than grouped.
fn mixed() -> Vec<Caster> {
    vec![
        Caster::Skinned { u_slot: 0, j_slot: 0 },
        Caster::Static  { u_slot: 1 },
        Caster::Skinned { u_slot: 2, j_slot: 1 },
        Caster::Skinned { u_slot: 3, j_slot: 2 },
        Caster::Static  { u_slot: 4 },
    ]
}

// ── Differential pin ─────────────────────────────────────────────────────────────────────────────

/// The extracted planner must emit the byte-identical command stream the pre-#739 loops emitted.
///
/// This grades the new code against the OLD implementation, not against itself. **When to change
/// this test**: it freezes pre-#739 behaviour, so it is what should fail if someone later *intends*
/// to change character shadow draw order. Update it as part of that change; do not weaken it.
#[test]
fn the_extracted_plan_emits_the_pre_739_command_stream() {
    let cases: Vec<(&str, Vec<Caster>)> = vec![
        ("no casters", vec![]),
        ("skinned only", vec![Caster::Skinned { u_slot: 0, j_slot: 0 },
                              Caster::Skinned { u_slot: 1, j_slot: 1 }]),
        ("static only", vec![Caster::Static { u_slot: 0 }, Caster::Static { u_slot: 1 }]),
        ("static first, then skinned", vec![Caster::Static { u_slot: 9 },
                                            Caster::Skinned { u_slot: 3, j_slot: 7 }]),
        ("interleaved", mixed()),
        ("repeated slots", vec![Caster::Skinned { u_slot: 2, j_slot: 2 },
                                Caster::Skinned { u_slot: 2, j_slot: 2 },
                                Caster::Static  { u_slot: 2 }]),
    ];
    for (name, casters) in &cases {
        assert_eq!(new(casters), old(casters),
            "#739: the extracted character shadow plan diverged from the pre-#739 loops on case \
             {name:?}");
    }
}

// ── The routing itself ───────────────────────────────────────────────────────────────────────────

/// **Only skinned casters bind a joint palette.** A static caster that bound group 2 would point the
/// static pipeline at a joint buffer it does not declare; a skinned caster that did not would pose
/// against whatever palette the previous caster left in slot 2 — the whole model drawn in another
/// character's pose. Asserted over the executor's command stream, which is where the decision is
/// made, not over the step type (where it is already unrepresentable).
#[test]
fn group_two_is_bound_exactly_for_the_skinned_casters() {
    let casters = mixed();
    let cmds = new(&casters);

    let joints: Vec<usize> = cmds.iter()
        .filter_map(|c| if let Cmd::BindGroup2(j) = c { Some(*j) } else { None }).collect();
    assert_eq!(joints, vec![0, 1, 2],
        "each skinned caster binds its own joint slot, in sub-pass order; got {joints:?}");

    // Every group-2 bind is immediately preceded by that caster's group-1 bind and followed by its
    // draw — i.e. no static caster is between a skinned caster's binds and its draw.
    for (i, c) in cmds.iter().enumerate() {
        if matches!(c, Cmd::BindGroup2(_)) {
            assert!(matches!(cmds[i - 1], Cmd::BindGroup1(_)),
                "a joint bind must follow the same caster's uniform bind, got {:?}", cmds[i - 1]);
            assert!(matches!(cmds[i + 1], Cmd::Draw(_)),
                "a joint bind must be followed by that caster's draw, got {:?}", cmds[i + 1]);
        }
    }

    let statics_drawn: Vec<usize> = plan_character_shadow_draws(&casters)
        .filter(|s| s.bind.kind() == CharacterShadowKind::Static).map(|s| s.caster).collect();
    assert_eq!(statics_drawn, vec![1, 4], "the two static casters must both be drawn");
}

/// **Sub-pass order and grouping.** Every skinned caster is drawn before every static one, whatever
/// order they were selected in, and each sub-pass sets its pipeline exactly once. Drawing in
/// selection order instead would switch pipeline per caster — the cost the two-loop split exists to
/// avoid.
#[test]
fn all_skinned_casters_draw_before_all_static_ones_with_one_pipeline_switch_each() {
    assert_eq!(CHARACTER_SHADOW_SUBPASSES,
        [CharacterShadowKind::Skinned, CharacterShadowKind::Static],
        "#739: the character shadow sub-pass order changed");

    let casters = mixed();
    let cmds = new(&casters);

    let pipelines: Vec<&str> = cmds.iter()
        .filter_map(|c| if let Cmd::SetPipeline(p) = c { Some(*p) } else { None }).collect();
    assert_eq!(pipelines, vec!["shadow_skinned", "shadow_static"],
        "exactly one pipeline switch per non-empty sub-pass, skinned first; got {pipelines:?}");
    assert_eq!(cmds.iter().filter(|c| **c == Cmd::BindGroup0).count(), 2,
        "group 0 is bound once per non-empty sub-pass");

    let draws: Vec<usize> = cmds.iter()
        .filter_map(|c| if let Cmd::Draw(i) = c { Some(*i) } else { None }).collect();
    assert_eq!(draws, vec![0, 2, 3, 1, 4],
        "draws must come out grouped by sub-pass (skinned 0,2,3 then static 1,4), not in \
         selection order; got {draws:?}");
}

/// **An empty sub-pass emits nothing** — no pipeline switch, no group-0 bind. A zone where every
/// caster is skinned must not pay for the static pipeline, and vice versa.
#[test]
fn an_empty_subpass_emits_no_pipeline_switch() {
    let skinned_only = vec![Caster::Skinned { u_slot: 0, j_slot: 0 }];
    let cmds = new(&skinned_only);
    assert_eq!(cmds.iter().filter(|c| matches!(c, Cmd::SetPipeline(_))).count(), 1,
        "one non-empty sub-pass, one pipeline switch; got {cmds:?}");
    assert!(!cmds.contains(&Cmd::SetPipeline("shadow_static")));

    let static_only = vec![Caster::Static { u_slot: 4 }];
    let cmds = new(&static_only);
    assert_eq!(cmds, vec![Cmd::SetPipeline("shadow_static"), Cmd::BindGroup0,
                          Cmd::BindGroup1(4), Cmd::Draw(0)],
        "a static-only frame must not switch to the skinned pipeline at all");

    assert!(new(&[]).is_empty(), "no casters must emit no commands");
}

/// **A step names the caster's index in the caster list**, not its position in the sub-pass — the
/// sink uses it to index the caster slice, so an off-by-one draws another character's model.
#[test]
fn a_step_carries_the_casters_index_in_the_selection_list() {
    let casters = mixed();
    let steps: Vec<_> = plan_character_shadow_draws(&casters).collect();
    assert_eq!(steps.iter().map(|s| s.caster).collect::<Vec<_>>(), vec![0, 2, 3, 1, 4]);
    assert_eq!(steps[0].bind, CharacterShadowBind::Skinned { u_slot: 0, j_slot: 0 });
    assert_eq!(steps[3].bind, CharacterShadowBind::Static { u_slot: 1 },
        "the first static step must carry caster 1's uniform slot, not the 4th caster's");
}
