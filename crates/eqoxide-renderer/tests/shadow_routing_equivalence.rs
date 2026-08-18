//! Refactor-equivalence pin for eqoxide#721: the extracted draw planner must issue the **exact**
//! command sequence the two hand-written loops in `encode_shadow_pass` issued before it.
//!
//! #721 is a testability refactor with a hard "no rendering behaviour change" constraint, and the
//! only way to discharge that claim without a GPU device is differentially: `old()` below is a
//! verbatim transcription of the pre-#721 instanced loops (commit d977706, the merge-base of the
//! #721 branch), `new()` runs the **real** executor — `pass::execute_instanced_shadow_plan`, the
//! exact function `encode_shadow_pass` calls — into a recording sink, and every case asserts the
//! two emit an identical `Cmd` stream: same sub-pass order, same pipeline switches, same group-1
//! binds (including the redundant-bind elision), same draw order.
//!
//! `new()` was a hand-written *transcription* of the executor until review round 2, which is how the
//! reviewer was able to reintroduce #718's N2 bug (`tex_bg(mesh.texture_idx)` for `tex_bg(tex)`) in
//! `encode_shadow_pass` with all 14 tests green. The sink closed that: the only production code the
//! executor can now reach that this file does not is the four `wgpu`-handle lookups in
//! `encode_shadow_pass`'s `PassSink` impl.
//!
//! **This grades the new code against the OLD implementation, not against itself.** Confirmed to
//! discriminate: inverting `ShadowPipelineKind::for_render_mode`'s two arms makes both tests here
//! fail. See the #721 PR body for the full mutation table.
//!
//! **When to change this file.** It deliberately freezes pre-#721 behaviour, so it is exactly what
//! should fail if someone later *intends* to change the instanced shadow draw order (say, sorting
//! casters by texture to cut rebinds). That is a decision to make explicitly — update or delete
//! this file as part of that change, do not weaken it to make a diff go green.
//!
//! Nothing here creates a `wgpu::RenderPass`, so it grades the command *sequence*, not the wgpu
//! calls that sequence turns into — see `shadow_routing.rs`'s "What this file does NOT cover".

use eqoxide_assets::RenderMode;
use eqoxide_renderer::pass::{
    execute_instanced_shadow_plan, InstancedShadowCaster, InstancedShadowSink, ShadowPipelineKind,
};

#[derive(Debug, PartialEq, Eq, Clone)]
enum Cmd {
    SetPipeline(&'static str),
    BindGroup0,
    BindGroup1(Option<usize>),
    Draw(usize),
}

/// `(mode, texture, animation)` triple describing one caster in the exhaustive small-scene alphabet
/// below — factored out only to satisfy `clippy::type_complexity`, not a behaviour change.
type CasterSpec = (RenderMode, Option<usize>, Option<(u32, Vec<usize>)>);

struct Caster {
    mode: RenderMode,
    tex:  Option<usize>,
    anim: Option<(u32, Vec<usize>)>,
}
impl InstancedShadowCaster for Caster {
    fn render_mode(&self) -> RenderMode { self.mode }
    fn texture_idx(&self) -> Option<usize> { self.tex }
    fn anim(&self) -> Option<&(u32, Vec<usize>)> { self.anim.as_ref() }
}

/// Verbatim transcription of origin/main (d977706) `encode_shadow_pass`'s two instanced loops.
fn old(casters: &[Caster], now_ms: u64) -> Vec<Cmd> {
    let frame_tex = |tex: Option<usize>, anim: &Option<(u32, Vec<usize>)>| -> Option<usize> {
        match anim {
            Some((ms, frames)) if !frames.is_empty() => {
                Some(frames[(now_ms / (*ms).max(1) as u64) as usize % frames.len()])
            }
            _ => tex,
        }
    };
    let mut out = Vec::new();
    let mut inst_bound = false;
    for (i, mesh) in casters.iter().enumerate() {
        if mesh.mode != RenderMode::Opaque { continue; }
        if !inst_bound {
            out.push(Cmd::SetPipeline("shadow_instanced"));
            out.push(Cmd::BindGroup0);
            inst_bound = true;
        }
        out.push(Cmd::Draw(i));
    }
    let mut inst_masked_bound = false;
    let mut inst_masked_tex: Option<usize> = None;
    for (i, mesh) in casters.iter().enumerate() {
        if mesh.mode != RenderMode::Masked { continue; }
        let etex = frame_tex(mesh.tex, &mesh.anim);
        if !inst_masked_bound {
            out.push(Cmd::SetPipeline("shadow_instanced_masked"));
            out.push(Cmd::BindGroup0);
            out.push(Cmd::BindGroup1(etex));
            inst_masked_tex = etex;
            inst_masked_bound = true;
        } else if etex != inst_masked_tex {
            inst_masked_tex = etex;
            out.push(Cmd::BindGroup1(inst_masked_tex));
        }
        out.push(Cmd::Draw(i));
    }
    out
}

/// A device-free [`InstancedShadowSink`]: the same four methods `encode_shadow_pass`'s `PassSink`
/// implements against a live `wgpu::RenderPass`, recording into a `Vec` instead.
///
/// Note what is *absent*: `bind_texture` receives an index and has no caster in scope, which is why
/// the round-2 reviewer's MY3 mutation (bind the caster's base `texture_idx` instead of the
/// resolved animation frame) is no longer expressible in the executor at all.
#[derive(Default)]
struct Recorder {
    out: Vec<Cmd>,
}

impl InstancedShadowSink for Recorder {
    fn set_pipeline(&mut self, kind: ShadowPipelineKind) {
        self.out.push(Cmd::SetPipeline(match kind {
            ShadowPipelineKind::Opaque => "shadow_instanced",
            ShadowPipelineKind::Masked => "shadow_instanced_masked",
        }));
    }
    fn bind_light_depth(&mut self) {
        self.out.push(Cmd::BindGroup0);
    }
    fn bind_texture(&mut self, idx: Option<usize>) {
        self.out.push(Cmd::BindGroup1(idx));
    }
    fn draw(&mut self, caster: usize) {
        self.out.push(Cmd::Draw(caster));
    }
}

/// The #721 executor — **the real one**, `pass::execute_instanced_shadow_plan`, not a copy of it.
fn new(casters: &[Caster], now_ms: u64) -> Vec<Cmd> {
    let mut rec = Recorder::default();
    execute_instanced_shadow_plan(casters, now_ms, &mut rec);
    rec.out
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn upto(&mut self, n: u64) -> u64 { self.next() % n }
}

#[test]
fn plan_reproduces_the_pre_721_command_sequence() {
    let mut rng = Lcg(0x5eed);
    let mut cases = 0usize;
    let mut cmds = 0usize;
    for _ in 0..20_000 {
        let n = rng.upto(9) as usize;
        let casters: Vec<Caster> = (0..n)
            .map(|_| {
                let mode = match rng.upto(4) {
                    0 => RenderMode::Opaque,
                    1 => RenderMode::Masked,
                    2 => RenderMode::Blend,
                    _ => RenderMode::Additive,
                };
                // Index 0 is in the alphabet deliberately: `texture_bind_groups[0]` is an ordinary
                // texture, not a sentinel, and an alphabet that never emits it cannot catch a
                // mutant that special-cases 0 (round 2's MY6).
                let tex = match rng.upto(5) {
                    0 => None,
                    k => Some(k as usize - 1), // 0, 1, 2, 3
                };
                let anim = match rng.upto(5) {
                    0 => None,
                    1 => Some((0u32, vec![])),          // degenerate: zero interval, no frames
                    2 => Some((rng.upto(3) as u32, vec![7])),
                    3 => Some((rng.upto(200) as u32, vec![0, 8])), // frame index 0
                    _ => Some((rng.upto(200) as u32, vec![7, 8, 9][..1 + rng.upto(3) as usize].to_vec())),
                };
                Caster { mode, tex, anim }
            })
            .collect();
        let now_ms = rng.upto(5_000);
        let a = old(&casters, now_ms);
        let b = new(&casters, now_ms);
        assert_eq!(a, b, "divergence at case {cases} (now_ms={now_ms})");
        cases += 1;
        cmds += a.len();
    }
    println!("EQUIVALENCE: {cases} random scenes, {cmds} commands compared, zero divergences");
}

/// Exhaustive over every 3-caster scene built from a small alphabet, at several timestamps.
#[test]
fn plan_reproduces_the_pre_721_command_sequence_exhaustively_for_small_scenes() {
    let alphabet: Vec<CasterSpec> = vec![
        (RenderMode::Opaque, Some(1), None),
        (RenderMode::Opaque, None, Some((10, vec![4, 5]))),
        (RenderMode::Masked, Some(1), None),
        (RenderMode::Masked, Some(1), Some((10, vec![4, 5]))),
        (RenderMode::Masked, Some(2), None),
        (RenderMode::Masked, None, None),
        (RenderMode::Masked, Some(3), Some((0, vec![]))),
        // Texture index 0, both as a static bind and as an animation frame. `texture_bind_groups[0]`
        // is an ordinary entry; round 2's MY6 mutation (treat frame 0 as "no frame") survived an
        // alphabet that could not express it.
        (RenderMode::Masked, Some(0), None),
        (RenderMode::Masked, Some(2), Some((10, vec![0, 5]))),
        (RenderMode::Blend, Some(1), None),
        (RenderMode::Additive, Some(1), None),
    ];
    let m = alphabet.len();
    let mut cases = 0usize;
    for i in 0..m {
        for j in 0..m {
            for k in 0..m {
                let casters: Vec<Caster> = [i, j, k]
                    .iter()
                    .map(|&x| Caster {
                        mode: alphabet[x].0,
                        tex:  alphabet[x].1,
                        anim: alphabet[x].2.clone(),
                    })
                    .collect();
                for now_ms in [0u64, 9, 10, 11, 20, 12_345] {
                    assert_eq!(old(&casters, now_ms), new(&casters, now_ms));
                    cases += 1;
                }
            }
        }
    }
    println!("EQUIVALENCE (exhaustive): {cases} scene×time combinations, zero divergences");
}
