//! Device-free coverage for the instanced placed-object shadow draw plan (eqoxide#721).
//!
//! ## Why this file exists
//!
//! `encode_shadow_pass` picks between two shadow pipelines per instanced caster from its
//! `RenderMode` (`Opaque` → the fragment-less `shadow_instanced`, `Masked` →
//! `shadow_instanced_masked`, which alpha-tests the cutout so trees stop casting square shadows —
//! eqoxide#707, fixed in #718), and resolves the masked caster's *current animation frame* texture
//! rather than a stale `texture_idx`. Before #721 neither decision was reachable from any test:
//! #718's own mutation check reverted the routing, and separately reverted the animated-texture
//! fix, and the entire `eqoxide-renderer` suite stayed green both times.
//!
//! There is no GPU device in this crate's test harness (`fog_shader.rs` / `weather_shader.rs`
//! established that precedent deliberately), and `GpuInstancedMesh` is unconstructible without one
//! — it owns three `wgpu::Buffer`s. So #721's fix pulls the decisions out of the render-pass hot
//! loop into `pass::plan_instanced_shadow_draws`, a pure function over the
//! `pass::InstancedShadowCaster` trait, which this file implements on a plain struct.
//!
//! ## ⚠ This test binary installs a `#[global_allocator]`
//!
//! `plan_is_lazy_and_allocates_nothing` needs to count allocations, so `CountingAlloc` below is the
//! allocator for **every test in this file**, not just that one. It is a thin per-thread counter
//! over `System` and adds no behaviour, but anything added here runs through it — if you need a
//! test that must not, put it in another file.
//!
//! ## What this file does NOT cover
//!
//! - **The `wgpu` handle lookups.** `encode_shadow_pass`'s `PassSink` impl of
//!   `pass::InstancedShadowSink` is four one-expression bodies that turn plan vocabulary into
//!   `wgpu` handles: the two-arm `ShadowPipelineKind` → `r.pipelines.*` lookup, `light_depth_bg`,
//!   the `texture_bind_groups[i]` out-of-range fallback, and the caster's buffer slices. All four
//!   need a live device. The pipeline lookup is pinned by
//!   `executor_binds_each_kind_to_the_pipeline_this_file_grades` below, but that is a *source-text*
//!   assert, not a semantic one (same technique, and same caveat, as the `PIPELINE_RS` asserts in
//!   `fog_shader.rs` / `weather_shader.rs` / `nav_debug_shader.rs`); the other three are not
//!   covered at all.
//! - **That the plan is executed at all.** Nothing in *this file* would fail if the
//!   `execute_instanced_shadow_plan` call in `encode_shadow_pass` were deleted or wrapped in
//!   `if false`. (The executor's own logic *is* graded, in `shadow_routing_equivalence.rs`; it is
//!   the one call site that is not.)
//! - **The depth-attachment clear** in `encode_shadow_pass`. That is the last of the three items
//!   this bullet originally listed and the only one still uncovered here.
//!
//!   This bullet is a **recurring maintenance point** — it has now been discharged twice, and the
//!   count has been wrong after each discharge, so check it rather than trusting it. It read "the
//!   other three sub-passes (skinned casters, static casters, the depth-attachment clear) … remain
//!   as untested as they were before #721", plus a *fourth* item for the caster selection that
//!   fills `casters`. #740 extracted that fourth to `pass::plan_shadow_casters` and graded it in
//!   `shadow_caster_selection.rs` (leaving the count reading "four" — an off-by-one this file
//!   introduced). #739 then extracted the **skinned and static** sub-passes to
//!   `pass::plan_character_shadow_draws` and graded them in `character_shadow_routing.rs`, which
//!   falsified two more of the three without touching this sentence.
//!
//!   What is *still* uncovered from those discharges, and is not restated by the files that took
//!   them over: what caster selection turns into (matrices, buffer writes), and — exactly as in the
//!   first bullet above — `encode_shadow_pass`'s `CharacterSink` handle lookups, which
//!   `character_shadow_routing.rs` enumerates for itself.

use eqoxide_assets::RenderMode;
use eqoxide_renderer::pass::{
    animated_frame_texture, plan_instanced_shadow_draws, InstancedShadowCaster, InstancedShadowStep,
    ShadowPipelineKind, ShadowTexBind, INSTANCED_SHADOW_SUBPASSES,
};

// ── Allocation counter (for `plan_is_lazy_and_allocates_nothing`) ────────────────────────────────
//
// Per-THREAD, not global: the libtest harness runs tests in parallel, so a process-wide counter
// would be polluted by whatever another test allocates concurrently. `const`-initialised so the TLS
// slot itself never allocates (which would recurse through this allocator).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static A: CountingAlloc = CountingAlloc;

fn allocs_on_this_thread() -> usize {
    ALLOCS.with(|c| c.get())
}

// ── Test caster ─────────────────────────────────────────────────────────────────────────────────

/// A device-free stand-in for `GpuInstancedMesh`, carrying only the fields the plan reads.
struct Caster {
    mode: RenderMode,
    tex:  Option<usize>,
    anim: Option<(u32, Vec<usize>)>,
}

impl Caster {
    fn new(mode: RenderMode, tex: Option<usize>) -> Self {
        Self { mode, tex, anim: None }
    }
    fn animated(mode: RenderMode, tex: Option<usize>, interval_ms: u32, frames: &[usize]) -> Self {
        Self { mode, tex, anim: Some((interval_ms, frames.to_vec())) }
    }
}

impl InstancedShadowCaster for Caster {
    fn render_mode(&self) -> RenderMode { self.mode }
    fn texture_idx(&self) -> Option<usize> { self.tex }
    fn anim(&self) -> Option<&(u32, Vec<usize>)> { self.anim.as_ref() }
}

fn plan(casters: &[Caster], now_ms: u64) -> Vec<InstancedShadowStep> {
    plan_instanced_shadow_draws(casters, now_ms).collect()
}

// ── 1. Routing: RenderMode → pipeline ───────────────────────────────────────────────────────────

/// Exhaustive over every `RenderMode` variant. This is the assert that dies if the `Opaque`/`Masked`
/// arms are swapped, if either is mapped to the wrong pipeline, or if `for_render_mode` is replaced
/// by a constant — every one of the four inputs has a distinct expected output, so no single return
/// value satisfies the test.
///
/// The `Blend`/`Additive` rows are not decoration: they pin the pre-#721 behaviour that
/// blended/additive placed objects cast **no** instanced shadow at all. A mutant that made
/// `for_render_mode` total (say `_ => Some(Opaque)`) would start drawing translucent geometry as
/// solid shadow casters, and this catches it.
#[test]
fn every_render_mode_routes_to_exactly_one_intended_pipeline() {
    assert_eq!(
        ShadowPipelineKind::for_render_mode(RenderMode::Opaque),
        Some(ShadowPipelineKind::Opaque),
        "solid geometry must use the cheap fragment-less shadow pipeline",
    );
    assert_eq!(
        ShadowPipelineKind::for_render_mode(RenderMode::Masked),
        Some(ShadowPipelineKind::Masked),
        "alpha-keyed foliage must use the alpha-cutout shadow pipeline, or eqoxide#707 \
         (square tree shadows) comes back",
    );
    assert_eq!(ShadowPipelineKind::for_render_mode(RenderMode::Blend), None);
    assert_eq!(ShadowPipelineKind::for_render_mode(RenderMode::Additive), None);
}

/// The same routing, observed through the plan rather than through `for_render_mode` directly — so
/// a mutation that leaves `for_render_mode` correct but rewires the plan around it still fails.
#[test]
fn plan_puts_each_caster_in_the_pipeline_its_render_mode_selects() {
    let casters = [
        Caster::new(RenderMode::Opaque, Some(1)),
        Caster::new(RenderMode::Masked, Some(2)),
        Caster::new(RenderMode::Blend, Some(3)),
        Caster::new(RenderMode::Additive, Some(4)),
        Caster::new(RenderMode::Masked, Some(5)),
        Caster::new(RenderMode::Opaque, Some(6)),
    ];
    let steps = plan(&casters, 0);

    let routed: Vec<(usize, ShadowPipelineKind)> =
        steps.iter().map(|s| (s.caster, s.pipeline)).collect();
    assert_eq!(
        routed,
        vec![
            (0, ShadowPipelineKind::Opaque),
            (5, ShadowPipelineKind::Opaque),
            (1, ShadowPipelineKind::Masked),
            (4, ShadowPipelineKind::Masked),
        ],
        "expected the two opaque casters on the opaque pipeline first, then the two masked \
         casters on the masked pipeline, and the blend/additive casters not drawn at all",
    );
}

// ── 2. The masked sub-pass exists ───────────────────────────────────────────────────────────────

/// Deleting the masked sub-pass — the mutation #718 could not catch — removes
/// `ShadowPipelineKind::Masked` from `INSTANCED_SHADOW_SUBPASSES`, which empties this plan.
#[test]
fn masked_only_scene_still_produces_masked_draws() {
    let casters = [
        Caster::new(RenderMode::Masked, Some(1)),
        Caster::new(RenderMode::Masked, Some(2)),
    ];
    let steps = plan(&casters, 0);
    assert_eq!(
        steps.len(),
        2,
        "a scene of nothing but alpha-keyed foliage must still emit two masked shadow draws; \
         an empty plan means the masked sub-pass was dropped (eqoxide#707 reintroduced)",
    );
    assert!(steps.iter().all(|s| s.pipeline == ShadowPipelineKind::Masked));
}

/// Symmetric guard for the opaque sub-pass, and for the sub-pass list itself.
#[test]
fn both_instanced_subpasses_run_in_opaque_then_masked_order() {
    // Compared as SLICES, not arrays: dropping a sub-pass changes the const's length, and an
    // array-vs-array `assert_eq!` would turn that into a compile error in this file rather than a
    // readable test failure.
    assert_eq!(
        INSTANCED_SHADOW_SUBPASSES.as_slice(),
        [ShadowPipelineKind::Opaque, ShadowPipelineKind::Masked].as_slice(),
        "both instanced shadow sub-passes must run, opaque first (grouping by pipeline is what \
         keeps the pass to one pipeline switch per kind)",
    );

    let casters = [Caster::new(RenderMode::Opaque, Some(1))];
    assert_eq!(plan(&casters, 0).len(), 1, "an opaque-only scene must still cast shadows");
}

// ── 3. Animated texture frame ───────────────────────────────────────────────────────────────────

/// The #718 fix reverted by the round-2 reviewer: the masked sub-pass must bind the animation frame
/// current at `now_ms`, not the caster's base `texture_idx`. `texture_idx` here is `Some(99)`, which
/// is not in the frame list at all — so a revert to `mesh.texture_idx` produces `Set(Some(99))` at
/// every timestamp and every row below fails.
#[test]
fn masked_caster_binds_the_current_animation_frame_not_the_base_texture() {
    let frames = [7usize, 8, 9];
    let casters = [Caster::animated(RenderMode::Masked, Some(99), 100, &frames)];

    // now_ms/100 mod 3 indexes `frames`: 0→7, 1→8, 2→9, 3→7, 10→8, 11→9.
    for (now_ms, expected) in [(0u64, 7usize), (100, 8), (250, 9), (300, 7), (1_000, 8), (1_150, 9)]
    {
        let steps = plan(&casters, now_ms);
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps[0].bind,
            ShadowTexBind::Set(Some(expected)),
            "at t={now_ms}ms the masked shadow must alpha-test frame {expected}, the same texel \
             the color pass samples; binding anything else desynchronises the shadow silhouette \
             from the rendered one",
        );
    }
}

/// A non-animated caster falls through to its static texture, and the frame selector's two edge
/// cases (empty frame list, zero interval) do not panic or divide by zero.
#[test]
fn frame_selector_edge_cases() {
    assert_eq!(animated_frame_texture(Some(4), None, 12_345), Some(4));
    assert_eq!(animated_frame_texture(None, None, 12_345), None);

    let empty = (100u32, Vec::<usize>::new());
    assert_eq!(
        animated_frame_texture(Some(4), Some(&empty), 12_345),
        Some(4),
        "an animation with no frames must fall back to the static texture, not index out of range",
    );

    let zero_interval = (0u32, vec![1usize, 2]);
    assert_eq!(animated_frame_texture(Some(4), Some(&zero_interval), 0), Some(1));
    assert_eq!(
        animated_frame_texture(Some(4), Some(&zero_interval), 1),
        Some(2),
        "a 0ms interval must be clamped to 1ms, not divide by zero",
    );

    // Texture index 0 is an ordinary `texture_bind_groups` entry, not a "no texture" sentinel.
    // The round-2 reviewer's MY6 mutation special-cased it and survived, because neither this file
    // nor the differential alphabet ever emitted a 0.
    let frame_zero = (10u32, vec![0usize, 5]);
    assert_eq!(
        animated_frame_texture(Some(4), Some(&frame_zero), 0),
        Some(0),
        "frame index 0 must be bound as texture 0, not fall through to the base texture",
    );
    assert_eq!(animated_frame_texture(Some(4), Some(&frame_zero), 10), Some(5));
    assert_eq!(animated_frame_texture(Some(0), None, 12_345), Some(0));
}

// ── 4. Bind-group bookkeeping ───────────────────────────────────────────────────────────────────

/// The opaque shadow pipeline has no fragment stage and therefore no group-1 layout. Binding a
/// texture for it would be a wgpu validation error at runtime; the plan must never ask.
#[test]
fn opaque_subpass_never_binds_a_texture() {
    let casters = [
        Caster::new(RenderMode::Opaque, Some(1)),
        Caster::animated(RenderMode::Opaque, Some(2), 50, &[3, 4]),
    ];
    let steps = plan(&casters, 137);
    assert_eq!(steps.len(), 2);
    assert!(
        steps.iter().all(|s| s.bind == ShadowTexBind::NotSampled),
        "opaque casters sample nothing in the shadow pass: {steps:?}",
    );
}

/// Group 1 is rebound exactly when the resolved texture changes, and the pipeline exactly once per
/// non-empty sub-pass — the redundant-bind elision the pre-#721 loops did by hand.
#[test]
fn texture_is_rebound_only_when_the_resolved_frame_changes() {
    let casters = [
        Caster::new(RenderMode::Masked, Some(5)),
        Caster::new(RenderMode::Masked, Some(5)), // same texture → no rebind
        Caster::new(RenderMode::Masked, Some(6)), // different → rebind
        Caster::new(RenderMode::Masked, Some(6)),
        Caster::new(RenderMode::Masked, None), // fallback texture is a distinct binding
    ];
    let steps = plan(&casters, 0);
    let binds: Vec<ShadowTexBind> = steps.iter().map(|s| s.bind).collect();
    assert_eq!(
        binds,
        vec![
            ShadowTexBind::Set(Some(5)),
            ShadowTexBind::Keep,
            ShadowTexBind::Set(Some(6)),
            ShadowTexBind::Keep,
            ShadowTexBind::Set(None),
        ],
    );
}

#[test]
fn pipeline_is_set_exactly_once_per_nonempty_subpass() {
    let casters = [
        Caster::new(RenderMode::Opaque, Some(1)),
        Caster::new(RenderMode::Masked, Some(2)),
        Caster::new(RenderMode::Opaque, Some(3)),
        Caster::new(RenderMode::Masked, Some(4)),
    ];
    let steps = plan(&casters, 0);
    let switches: Vec<(usize, ShadowPipelineKind)> = steps
        .iter()
        .filter(|s| s.set_pipeline)
        .map(|s| (s.caster, s.pipeline))
        .collect();
    assert_eq!(
        switches,
        vec![(0, ShadowPipelineKind::Opaque), (1, ShadowPipelineKind::Masked)],
        "exactly one pipeline switch per sub-pass, on its first caster",
    );

    // An empty sub-pass must not switch to its pipeline at all.
    let masked_only = [Caster::new(RenderMode::Masked, Some(1))];
    let steps = plan(&masked_only, 0);
    assert_eq!(steps.iter().filter(|s| s.set_pipeline).count(), 1);
    assert!(steps.iter().all(|s| s.pipeline == ShadowPipelineKind::Masked));
}

#[test]
fn empty_scene_plans_nothing() {
    assert!(plan(&[], 0).is_empty());
    let no_casters = [
        Caster::new(RenderMode::Blend, Some(1)),
        Caster::new(RenderMode::Additive, Some(2)),
    ];
    assert!(plan(&no_casters, 0).is_empty());
}

// ── 5. Cost ─────────────────────────────────────────────────────────────────────────────────────

/// `encode_shadow_pass` runs every frame the shadow map is rebuilt, so the plan should not add a
/// *new* per-frame heap allocation — the two hand-written loops it replaced had none, and a lazy
/// iterator was free. This is a "don't regress for nothing" pin, not a claim that one `Vec` would
/// have been a measurable frame-time cost: the enclosing `encode_shadow_pass` already builds a
/// `Vec<Caster>` and a sorted `Vec<&Billboard>` per call. (That last sentence is from reading the
/// function, not from a profiler.) This test measures the plan's own allocations with a per-thread
/// counting allocator rather than asserting them from reading the code.
///
/// The plan is *drained* (not collected) — `collect()` would allocate the `Vec`, which is a test
/// artifact, not something `encode_shadow_pass` does.
#[test]
fn plan_is_lazy_and_allocates_nothing() {
    let casters: Vec<Caster> = (0..256)
        .map(|i| {
            if i % 3 == 0 {
                Caster::animated(RenderMode::Masked, Some(i), 40, &[i, i + 1])
            } else {
                Caster::new(RenderMode::Opaque, Some(i))
            }
        })
        .collect();

    // Warm up anything lazily-initialised on this thread before the measurement window.
    let mut drawn = 0usize;
    for _ in plan_instanced_shadow_draws(&casters, 7) {
        drawn += 1;
    }
    assert_eq!(drawn, 256, "every opaque/masked caster must be drawn exactly once");

    let before = allocs_on_this_thread();
    let mut checksum = 0usize;
    for step in plan_instanced_shadow_draws(&casters, 7) {
        checksum += step.caster;
    }
    let after = allocs_on_this_thread();

    assert_eq!(checksum, (0..256).sum::<usize>());
    assert_eq!(
        after - before,
        0,
        "planning a 256-caster frame allocated {} time(s); the plan must stay a lazy iterator",
        after - before,
    );
}

// ── 6. Source-text pin on the one decision left in the executor ─────────────────────────────────

const PASS_RS: &str = include_str!("../src/pass.rs");

/// `PassSink::set_pipeline`'s `ShadowPipelineKind` → `r.pipelines.*` lookup needs a live
/// `Pipelines` (hence a device) to observe semantically, so it is pinned as source text instead —
/// the same technique as the `PIPELINE_RS` asserts in `fog_shader.rs`, `weather_shader.rs` and
/// `nav_debug_shader.rs`. Without this, swapping those two arms would invert the routing while
/// every semantic test in this crate stayed green.
///
/// #721's review round 2 argued this pin could be deleted once the executor became device-free.
/// It is kept, because deleting it strictly loses coverage: the swap is still a real bug, and it is
/// still caught here (measured — round 2's A2 goes RED with this pin, GREEN without it). What round
/// 2 was right about is that this is a *guard*, not a type: whole-line `//` comments are stripped so
/// a decoy comment cannot satisfy the assert, but a *trailing* comment appended to a code line still
/// can (measured, mutation A2b). Treat it as a backstop over the one lookup that genuinely cannot be
/// made device-free, not as the mechanism.
#[test]
fn executor_binds_each_kind_to_the_pipeline_this_file_grades() {
    let code: String = PASS_RS
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let flat: String = code.split_whitespace().collect::<Vec<_>>().join(" ");

    for (arm, why) in [
        (
            "ShadowPipelineKind::Opaque => &r.pipelines.shadow_instanced,",
            "solid casters must stay on the cheap fragment-less shadow pipeline",
        ),
        (
            "ShadowPipelineKind::Masked => &r.pipelines.shadow_instanced_masked,",
            "alpha-keyed casters must use the cutout shadow pipeline (eqoxide#707)",
        ),
    ] {
        assert!(
            flat.contains(arm),
            "encode_shadow_pass no longer contains the match arm `{arm}` — {why}. If the executor \
             was legitimately reformatted or restructured, update this pin AND confirm the routing \
             is still correct; if the two arms were swapped, this is the bug.",
        );
    }
}
