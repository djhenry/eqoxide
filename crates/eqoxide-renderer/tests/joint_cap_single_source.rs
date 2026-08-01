//! eqoxide#798 — the joint-palette cap exists as a number in exactly one place, and this test is
//! what holds that true.
//!
//! ## What went wrong
//!
//! eqoxide#795 introduced `renderer::JOINT_CAP = 128`. The name did not reach the places that
//! actually enforce the cap: three `.wgsl` files each hardcoded `array<mat4x4<f32>, 128>`, and five
//! Rust sites each hardcoded `128` again in an array length plus a `.take(128)`. The **GPU-side
//! value is the real enforcement** — the uniform buffer's declared array length is what the driver
//! bounds against — so the Rust constant was a restatement of the WGSL, not the other way round.
//! Nothing in the tree went red if any one of the six drifted.
//!
//! ## What replaced it
//!
//! - The `.wgsl` files contain **no palette length at all**. They write the placeholder
//!   `pipeline::JOINT_CAP_TOKEN`, and `pipeline::wgsl` — the single preprocessing step every
//!   `create_shader_module` in the tree goes through — substitutes `renderer::JOINT_CAP` into the
//!   text before naga (and therefore the driver) ever sees it. A GPU-side copy that could drift no
//!   longer exists.
//! - The five Rust palette-building copies collapsed into `renderer::pad_joint_palette`, which
//!   returns the named type `renderer::JointPalette = [[[f32;4];4]; JOINT_CAP]`. A draw site cannot
//!   choose a different length because it no longer states one.
//!
//! ## Detection is structural, over the WHOLE corpus (eqoxide#811)
//!
//! The first version of this file decided *which* shaders carry a joint palette by matching the
//! struct name `JointMatrices` in the source text, and ran the IR check only on that name-derived
//! subset. A shader declaring a fixed-length uniform `mat4x4` palette under any other name — or
//! through a type alias — was invisible to it, and shipped unguarded; that was measured on
//! eqoxide#809 and filed as eqoxide#811.
//!
//! The name match is gone. [`uniform_mat4_palette_lengths`] resolves every uniform-address-space
//! struct member through naga's **validated type arena** and reports the length of any fixed-size
//! `mat4x4<f32>` array it finds. A palette is detected by being one, so a struct name, a type alias
//! (naga resolves aliases before the arena exists) and whitespace inside the declaration are all
//! irrelevant by construction.
//!
//! Which checks run over the **whole discovered corpus**, precisely — an earlier draft of this
//! header said "every check below" and a reviewer measured that to be false:
//! `every_uniform_mat4_palette_in_the_corpus_is_exactly_joint_cap`,
//! `every_palette_length_tracks_the_substituted_token`,
//! `palette_bearing_shaders_are_exactly_the_expected_three`,
//! `raw_palette_shader_sources_do_not_parse_without_substitution` (over the set the first three
//! detect), `no_shader_writes_a_numeric_palette_length`, and
//! `substitution_leaves_every_shader_comment_byte_identical`. The two `${JOINT_CAP}` token checks
//! are properties of the token's characters and read no shader at all;
//! `no_draw_site_states_a_joint_palette_length` scans two named Rust files and says so.
//!
//! ## What is a source-text scan here, and what that is worth
//!
//! Everything above reads the IR — the representation the backend lowers to SPIR-V/MSL/HLSL — so it
//! is immune to the `include_str!`-plus-`contains` evasion family this repo has measured seven times
//! over (trailing comment, shadowing, `if false {}`, `#[cfg(any())]`, macro-elided argument,
//! satisfying the guard from an unrelated comment, `if false`-wrapping a written call): you cannot
//! comment or `cfg` your way to a different validated array size.
//!
//! Two checks in this file ARE text scans, and each states its own scope at its docstring:
//! `no_shader_writes_a_numeric_palette_length` (a *negative* sweep, and a deliberately partial one)
//! and `no_draw_site_states_a_joint_palette_length` (likewise, over two Rust files).
//!
//! ## Reach control (the eqoxide#778 lesson)
//!
//! A scanner that silently stops short of its corpus is indistinguishable from a passing one. So:
//!
//! - The corpus is **discovered from disk** (every `.wgsl` under `src/shaders/`, recursively, from
//!   `CARGO_MANIFEST_DIR`), not listed in this file. A shader added tomorrow is scanned tomorrow.
//! - `discovered_corpus_is_not_silently_truncated` compares that walk against an oracle it does not
//!   produce — the shader names `pipeline.rs` compiles in via `include_str!`, which are literals and
//!   provably name files that exist. A walk that stops short of any shipped shader fails on
//!   membership; a file the parse pass dropped fails on walk-vs-parse divergence.
//!   **This control's earlier form did not have that property, and its docstring said it did**: a
//!   reviewer truncated the walk to three files with a 99-length decoy palette on disk and the file
//!   stayed 13 passed / 0 failed. That test's own docstring carries the measurement and the hole
//!   that remains.
//! - **The oracle then had the same disease one level down, and again the docstring was the thing
//!   that was wrong.** Its scan took `include_str!` hits from `pipeline.rs` *including ones inside
//!   comments*, while claiming an extension filter prevented that. Measured, not argued: a phantom
//!   name in a comment padded `MIN_PIPELINE_INCLUDED_SHADERS`, and a scan that silently dropped a
//!   genuinely-included shader still measured **14 passed / 0 failed**. The scan now skips comments
//!   via [`comment_spans`] — the same scanner the comment-identity test uses, not a second one — and
//!   `the_include_scan_ignores_comments_but_not_code` pins both directions against a synthetic
//!   source, so it keeps testing the rule after `pipeline.rs` changes.
//! - `palette_bearing_shaders_are_exactly_the_expected_three` asserts the structurally-detected
//!   palette set equals the expected three. Both directions of that assertion were planted and run
//!   for eqoxide#811 (a fourth palette shader added; a known palette removed) — see the PR body's
//!   mutation table.
//!
//! Nothing in this header claims a mutation went red that was not planted and observed. The claims
//! whose mutations are in the eqoxide#798 PR body: the three shaders' palette lengths and the five
//! collapsed Rust sites. The claims whose mutations are in the eqoxide#811/812/813/814 PR body: the
//! structural detection set (both directions), the whole-corpus length check via a decoy shader, the
//! token-tracking check, the delimited-token identifier property, the comment-preservation property
//! (line and block), the recursive walk via a nested decoy, and the `pipeline.rs` superset oracle —
//! that last one planted in both directions, a truncated walk (red) and an untouched walk (green).
//! Added in the third review round: the oracle's comment filter, planted four ways — a phantom in a
//! line comment (was a false red, now green), the phantom-pads-the-floor composite (was a false
//! **green**, now red), the filter neutered (red), and the comment scanner made to swallow the whole
//! file (red, via the floor at zero). Nothing here is claimed from reading the code.

use eqoxide_renderer::pipeline::{wgsl, JOINT_CAP_TOKEN};
use eqoxide_renderer::renderer::{pad_joint_palette, JointPalette, JOINT_BUF_BYTES, JOINT_CAP};
use std::collections::{BTreeMap, BTreeSet};

/// The shaders expected to declare a joint palette. Asserted to be EXACTLY the corpus's
/// **structurally detected** palette set by `palette_bearing_shaders_are_exactly_the_expected_three`
/// — detection is "the validated IR contains a fixed-length `mat4x4<f32>` array in a uniform-space
/// struct", not a name or spelling. A fourth shader that adds such a palette under any struct name
/// fails that assertion instead of slipping past it (eqoxide#811).
const EXPECTED_JOINT_SHADERS: [&str; 3] =
    ["character_skinned.wgsl", "shadow.wgsl", "skin_probe.wgsl"];

/// Every `.wgsl` anywhere under `src/shaders/`, read from DISK at test time. Keyed by the path
/// relative to `src/shaders/`, `/`-separated — a bare file name for everything that ships today,
/// because no subdirectory exists.
///
/// Deliberately not a hardcoded list of `include_str!`s: the corpus this test claims to cover has
/// to be the corpus that actually exists, or the reach claim is a guess.
///
/// **The walk recurses (eqoxide#811 review, finding 2).** It was a single non-recursive `read_dir`
/// while this docstring already said "under `src/shaders/`", and the gap was measured, not spotted
/// by reading: a reviewer planted a palette-bearing decoy one directory deep and *nothing named it*
/// in the same run that caught a top-level one. Recursing is what makes the sentence true rather
/// than true by the accident of a flat directory.
fn shader_corpus() -> BTreeMap<String, String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/shaders");
    let mut out = BTreeMap::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot read shader dir {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("wgsl") {
                continue;
            }
            let rel = path.strip_prefix(&root).expect("walked path is under the shader root");
            let key = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            out.insert(key, src);
        }
    }
    out
}

/// The shader files `pipeline.rs` compiles into the binary, extracted from **its source text**
/// (`include_str!("shaders/<name>")`), keyed the way [`shader_corpus`] keys its walk.
///
/// This is the only corpus oracle in this file that the shader walk does not itself produce, which
/// is exactly what makes it usable as the walk's reach control. It rests on two structural
/// properties, neither of which is a convention someone has to keep:
///
/// - `include_str!` takes a **literal** path. It cannot be handed a value computed at runtime, so
///   this list is fixed in the source text and cannot shrink when the walk shrinks.
/// - Every name it yields provably **exists on disk**, because the crate under test would not
///   compile if it did not. "The walk did not return this file" can therefore only mean the walk
///   missed it.
///
/// ## Comments in `pipeline.rs` are excluded, and the `.wgsl` filter is not what does it
///
/// An earlier version of this docstring said "names that do not end in `.wgsl` are ignored, so a doc
/// comment that happens to spell the macro cannot inject a phantom file name". The *so* did not
/// follow: the extension filter rejects a comment naming a non-`.wgsl` path and does nothing about a
/// comment naming a `.wgsl` one. A reviewer reasoned that out; both consequences were then measured
/// here, and the second is worse than either of us expected:
///
/// - **False RED.** `// include_str!("shaders/ghost.wgsl")` planted as a comment made
///   `discovered_corpus_is_not_silently_truncated` fail with `the shader walk never returned
///   ["ghost.wgsl"]` — the honest direction, but an alarm about a file that never existed.
/// - **False GREEN, and this one is a real hole.** A phantom *inflates* `included.len()`, which is
///   what `MIN_PIPELINE_INCLUDED_SHADERS` counts. Composite RC-M9 — an orphan `.wgsl` on disk, a
///   comment naming it, and the scan silently dropping one genuinely-included shader — measured
///   **14 passed / 0 failed**. The floor was padded by the phantom and the superset check never
///   looked at the dropped name, because a name the scan loses is a name the scan cannot check.
///   The oracle's own reach control was defeated by prose.
///
/// So the scan now skips any hit inside a comment, using [`comment_spans`] — the same scanner the
/// comment-identity test uses, not a second one. `parse_and_validate` never sees this file, so a
/// deliberately partial comment scanner is the honest tool here; what makes that safe is that both
/// ways it can be wrong are **loud**, and both were measured (RC-M10, RC-M11 in the PR body):
/// swallowing real code drops names below the floor, and exposing extra text yields a name that is
/// not on disk and fails the superset check.
///
/// `comment_spans` was written for WGSL, which has no string literals; Rust does. A Rust string
/// containing a comment opener would mis-parse — into one of the two loud directions above, not into
/// silence — and `the_include_scan_ignores_comments_but_not_code` pins the behaviour this relies on.
fn shaders_pipeline_includes() -> BTreeSet<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pipeline.rs");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let out = scan_include_str_shaders(&src, &path.display().to_string());

    // The comment filter must be doing something, or a future refactor could silently make it a
    // no-op and nobody would notice until a phantom padded the floor again.
    assert!(
        !comment_spans(&src).is_empty(),
        "{} has no comments at all, so the comment filter below is untested at its own call site",
        path.display()
    );
    out
}

/// The scan itself, over an arbitrary source string so it can be driven by a test rather than only
/// by the real `pipeline.rs`. Hits inside comments are skipped; names not ending in `.wgsl` are
/// ignored (that filter is still worth having — it just is not the thing that stops phantoms).
fn scan_include_str_shaders(src: &str, label: &str) -> BTreeSet<String> {
    const NEEDLE: &str = "include_str!(\"shaders/";
    let spans = comment_spans(src);
    let in_comment = |off: usize| spans.iter().any(|&(_, s, e)| off >= s && off < e);

    let mut out = BTreeSet::new();
    let mut at = 0usize;
    while let Some(i) = src[at..].find(NEEDLE) {
        let hit = at + i;
        let after = hit + NEEDLE.len();
        let end = after
            + src[after..]
                .find('"')
                .unwrap_or_else(|| panic!("unterminated include_str! path literal in {label}"));
        if !in_comment(hit) {
            let name = &src[after..end];
            if name.ends_with(".wgsl") {
                out.insert(name.to_string());
            }
        }
        at = end;
    }
    out
}

/// Floor on how many distinct shader files [`shaders_pipeline_includes`] must find, so a bug in that
/// scan cannot quietly shrink the oracle and make the superset check below vacuous — the oracle needs
/// a reach control of its own, or it is just a second scanner nobody is watching.
///
/// Measured 2026-08-01 on this tree with a tool that is *not* that scanner:
/// `grep -o 'include_str!("shaders/[^"]*"' crates/eqoxide-renderer/src/pipeline.rs | sort -u | wc -l`
/// → **11** distinct files (12 occurrences; `character.wgsl` is included twice). If a shader is
/// deliberately deleted this assertion is where that gets acknowledged rather than absorbed.
const MIN_PIPELINE_INCLUDED_SHADERS: usize = 11;

fn parse_and_validate(source: &str, label: &str) -> naga::Module {
    let module = naga::front::wgsl::parse_str(source)
        .unwrap_or_else(|e| panic!("{label}: WGSL failed to parse: {e}"));
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .unwrap_or_else(|e| panic!("{label}: WGSL failed validation: {e}"));
    module
}

/// Is this IR type `mat4x4<f32>`?
fn is_mat4x4_f32(inner: &naga::TypeInner) -> bool {
    matches!(
        inner,
        naga::TypeInner::Matrix {
            columns: naga::VectorSize::Quad,
            rows: naga::VectorSize::Quad,
            scalar: naga::Scalar { kind: naga::ScalarKind::Float, width: 4 },
        }
    )
}

/// Every fixed-length `mat4x4<f32>` array that appears as a member of a struct bound in the
/// **uniform** address space, read out of the validated IR's type arena. That *is* a joint palette:
/// it is the declaration the driver bounds indexing against.
///
/// This is a structural property, not a spelling. `JointMatrices { mats: array<mat4x4<f32>, N> }`,
/// `Decoy { m: array<JMat, N> }` with `alias JMat = mat4x4<f32>`, and
/// `X { m : array < mat4x4<f32> , N > }` all yield `[N]` — naga has already resolved aliases and
/// discarded whitespace by the time the arena exists.
///
/// Deliberately narrower than "any fixed-length uniform array": a `array<vec4<f32>, 4>` in a uniform
/// struct is not a joint palette and must not be forced to equal `JOINT_CAP`.
fn uniform_mat4_palette_lengths(module: &naga::Module) -> Vec<u32> {
    let mut lengths = Vec::new();
    for (_, global) in module.global_variables.iter() {
        if global.space != naga::AddressSpace::Uniform {
            continue;
        }
        let naga::TypeInner::Struct { members, .. } = &module.types[global.ty].inner else {
            continue;
        };
        for member in members {
            let naga::TypeInner::Array { base, size, .. } = &module.types[member.ty].inner else {
                continue;
            };
            if !is_mat4x4_f32(&module.types[*base].inner) {
                continue;
            }
            if let naga::ArraySize::Constant(n) = size {
                lengths.push(n.get());
            }
        }
    }
    lengths
}

/// The whole corpus, substituted through `pipeline::wgsl` and parsed: filename → palette lengths.
///
/// Every file is parsed. A shader that fails to parse or validate panics here rather than being
/// skipped, because a skipped file is a hole in the corpus that would report clean.
fn corpus_palette_lengths() -> BTreeMap<String, Vec<u32>> {
    shader_corpus()
        .iter()
        .map(|(name, raw)| {
            let module = parse_and_validate(&wgsl(raw), name);
            (name.clone(), uniform_mat4_palette_lengths(&module))
        })
        .collect()
}

// ── Reach control ───────────────────────────────────────────────────────────────────────────────

/// The disk walk reached its corpus: it returned every shader `pipeline.rs` compiles in, it contains
/// everything the palette checks name, and every file it returned was parsed.
///
/// This is the eqoxide#778 guard-reach control: on that issue a scanner silently stopped at ~12% of
/// its corpus and reported clean, and every mutation probe happened to land in the window it could
/// still see.
///
/// ## The first version of this control did not have the property its name claims
///
/// Measured by a reviewer, not argued: with a 99-length uniform `mat4x4` palette planted in the
/// corpus directory **and** `shader_corpus()` truncated to three files, this test file stayed
/// **13 passed / 0 failed** — indistinguishable from clean. Both sides of the walk-vs-parse
/// assertion came from the same walk (`corpus_palette_lengths()` is a total map over
/// `shader_corpus()`, so a short walk shrinks both sides together), and the only other defence was a
/// count floor of `EXPECTED_JOINT_SHADERS.len()` = 3 against a real corpus of 11. A walk cut to a
/// quarter of its corpus cleared it. That is the eqoxide#778 shape reproduced *inside the control
/// written to prevent eqoxide#778*.
///
/// ## What replaced it
///
/// The walk is now compared against [`shaders_pipeline_includes`] — an enumeration that lives in
/// `pipeline.rs`'s source text, that this file does not produce, and that cannot shrink when the
/// walk does. Every name in it provably exists on disk (the crate would not compile otherwise), so a
/// missing name means the walk missed a file that is definitely there.
///
/// ## Scope, both directions measured (see the PR body)
///
/// A walk that drops any shader `pipeline.rs` includes fails here. A walk that drops **only** files
/// no `include_str!` names — a planted decoy, or a `.wgsl` used solely by a dev bin — is still
/// invisible to this control: nothing outside the disk knows such a file exists, so no oracle in
/// this file can miss it loudly. That is a real remaining hole and it is stated rather than covered
/// by a sentence.
#[test]
fn discovered_corpus_is_not_silently_truncated() {
    let corpus = shader_corpus();

    // The oracle's own reach control. A scan bug that returned an empty (or nearly empty) set would
    // make the superset assertion below pass vacuously — an unwatched second scanner.
    let included = shaders_pipeline_includes();
    assert!(
        included.len() >= MIN_PIPELINE_INCLUDED_SHADERS,
        "the pipeline.rs include_str! scan found {} shader file name(s), fewer than the {} measured \
         to be there — the corpus ORACLE is truncated, so the superset check below would pass \
         vacuously. Found: {:?}",
        included.len(),
        MIN_PIPELINE_INCLUDED_SHADERS,
        included
    );

    // The superset check: the walk must have reached every shader the crate compiles in.
    let missing: Vec<&String> = included.iter().filter(|n| !corpus.contains_key(*n)).collect();
    assert!(
        missing.is_empty(),
        "the shader walk never returned {missing:?}, which pipeline.rs compiles in via include_str! \
         — those files provably exist (the crate would not build otherwise), so the walk stopped \
         short of its corpus and every scan built on it is measuring a hole. Walked: {:?}",
        corpus.keys().collect::<Vec<_>>()
    );

    for name in EXPECTED_JOINT_SHADERS {
        assert!(
            corpus.contains_key(name),
            "{name} is claimed to be covered but the corpus walk never saw it. Found: {:?}",
            corpus.keys().collect::<Vec<_>>()
        );
    }
    let parsed = corpus_palette_lengths();
    let seen: Vec<&String> = parsed.keys().collect();
    let walked: Vec<&String> = corpus.keys().collect();
    assert_eq!(
        seen, walked,
        "the palette scan parsed a different set of files than the walk discovered — a file that \
         is walked but never parsed is a hole that reports clean"
    );
}

/// The set of shaders that **structurally** declare a fixed-length uniform `mat4x4<f32>` palette is
/// EXACTLY `EXPECTED_JOINT_SHADERS`.
///
/// eqoxide#811: this used to be `src.contains("JointMatrices")` — a struct name. A new shader
/// declaring the same palette under a different name, or through a type alias, was invisible to it
/// (measured on eqoxide#809: the suite stayed 9 passed / 0 failed with exactly such a shader in the
/// corpus). Detection now reads the validated IR, so the name a shader gives its struct is not
/// something this guard depends on being guessed correctly.
///
/// This assertion is two-sided on purpose: a *new* palette shader makes `actual` longer, and a known
/// shader losing its palette makes it shorter. Both were planted and run — see the PR body.
#[test]
fn palette_bearing_shaders_are_exactly_the_expected_three() {
    let lengths = corpus_palette_lengths();
    let actual: Vec<&str> = lengths
        .iter()
        .filter(|(_, l)| !l.is_empty())
        .map(|(name, _)| name.as_str())
        .collect();
    let mut expected = EXPECTED_JOINT_SHADERS.to_vec();
    expected.sort_unstable();
    assert_eq!(
        actual, expected,
        "the set of shaders declaring a fixed-length uniform mat4x4 palette changed. If a shader \
         gained one, it is now covered by every_uniform_mat4_palette_in_the_corpus_is_exactly_\
         joint_cap and belongs in EXPECTED_JOINT_SHADERS; if one lost its palette, the cap it used \
         to enforce went with it."
    );
}

// ── The IR check: EVERY uniform mat4 palette in the corpus IS `JOINT_CAP` ───────────────────────

/// For **every** shader in the discovered corpus, every fixed-length uniform `mat4x4<f32>` array in
/// the validated IR has length `JOINT_CAP`.
///
/// Not a text search, and not restricted to a list: `uniform_mat4_palette_lengths` reads naga's type
/// arena after validation, and this runs over the whole corpus. That is the eqoxide#811 fix — the
/// machinery was already alias-immune, but it was only pointed at a name-derived subset.
#[test]
fn every_uniform_mat4_palette_in_the_corpus_is_exactly_joint_cap() {
    let lengths = corpus_palette_lengths();
    let mut offenders = Vec::new();
    for (name, lens) in &lengths {
        for len in lens {
            if *len as usize != JOINT_CAP {
                offenders.push(format!("{name}: palette length {len}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a GPU-side uniform mat4 palette does not equal renderer::JOINT_CAP ({JOINT_CAP}). The \
         shader is the real enforcement — do not write a number in the .wgsl; write the \
         {JOINT_CAP_TOKEN} placeholder and let pipeline::wgsl substitute it. Offenders: \
         {offenders:#?}"
    );
    // Positive control: a corpus with no palette at all would satisfy the loop above vacuously.
    assert!(
        lengths.values().any(|l| !l.is_empty()),
        "no shader in the corpus declares a uniform mat4 palette — the thing this test pins is not \
         there, so a passing result would mean nothing"
    );
}

/// Every uniform mat4 palette **in the whole corpus** tracks the substituted token, whatever value
/// is substituted.
///
/// This is the check that makes a hardcoded-but-currently-correct length impossible. `array<JMat,
/// 128>` written literally compiles to the same IR as the placeholder does today, so
/// `every_uniform_mat4_palette_in_the_corpus_is_exactly_joint_cap` cannot tell them apart. Here the
/// token is substituted with a SENTINEL that is deliberately not `JOINT_CAP`: a length derived from
/// the token moves to the sentinel, a length written as a literal does not, and the mismatch is the
/// failure. Spelling-, alias- and whitespace-independent, because the comparison is against the IR.
///
/// It iterates the **discovered corpus**, not `EXPECTED_JOINT_SHADERS` (eqoxide#811 review,
/// finding 3): it used to walk the 3-name list while two sentences elsewhere described it as
/// whole-corpus, so a hardcoded palette in a *fourth* shader was outside it. Detection of that
/// fourth shader was never lost — `palette_bearing_shaders_are_exactly_the_expected_three` and
/// `raw_palette_shader_sources_do_not_parse_without_substitution` both fire on one, which the
/// reviewer measured — but the sentences were false, and the cheaper repair was to make them true.
/// The list is still used, at the end, as the positive control: the three known palette shaders must
/// each have yielded a palette, or a corpus that lost them all would satisfy the loop vacuously.
///
/// **Two sentinels, not one, and that is not belt-and-braces.** With a single sentinel, a shader
/// that hardcodes exactly the sentinel value is indistinguishable from one that tracks the token.
/// That is not hypothetical: it was measured here — a planted decoy declaring `array < JMat2 , 77 >`
/// slipped past this test while the single sentinel was 77, and only the other palette checks caught
/// it. A length that comes from the token equals *both* sentinels; a length written into the shader
/// can equal at most one, so no single literal survives.
#[test]
fn every_palette_length_tracks_the_substituted_token() {
    const SENTINELS: [u32; 2] = [77, 91];
    for s in SENTINELS {
        assert_ne!(s as usize, JOINT_CAP, "a sentinel must differ from the real cap");
    }
    assert_ne!(SENTINELS[0], SENTINELS[1], "the sentinels must differ from each other");
    let corpus = shader_corpus();
    let mut with_palette: Vec<&str> = Vec::new();
    let mut checked = 0usize;
    for (name, raw) in &corpus {
        for sentinel in SENTINELS {
            let substituted = raw.replace(JOINT_CAP_TOKEN, &sentinel.to_string());
            let module = parse_and_validate(&substituted, name);
            let lens = uniform_mat4_palette_lengths(&module);
            if !lens.is_empty() && !with_palette.contains(&name.as_str()) {
                with_palette.push(name.as_str());
            }
            for len in lens {
                assert_eq!(
                    len, sentinel,
                    "{name}: a uniform mat4 palette is {len} when the token is substituted with \
                     {sentinel} — that length is written into the shader, not derived from \
                     renderer::JOINT_CAP"
                );
                checked += 1;
            }
        }
    }
    for name in EXPECTED_JOINT_SHADERS {
        assert!(
            with_palette.contains(&name),
            "{name}: no palette found under sentinel substitution. Shaders that did yield one: \
             {with_palette:?}"
        );
    }
    assert!(checked >= EXPECTED_JOINT_SHADERS.len(), "checked only {checked} palettes");
}

/// A shader source that skips `pipeline::wgsl` is **invalid WGSL**, so a missed substitution cannot
/// ship a silently wrong cap.
///
/// This is what lets `pipeline::wgsl` be a preprocessing step rather than a discipline: the raw text
/// carries `${JOINT_CAP}`, and `$` is not a WGSL token character at all, so naga rejects the
/// unsubstituted source outright. Note what this does and does not prove — it proves the raw sources
/// do not compile, i.e. that forgetting the substitution FAILS LOUDLY at `create_shader_module`. It
/// does not prove any particular call site currently calls `wgsl()`; that is proven by the whole
/// render path failing to produce a pipeline, which no test in this crate can observe without a GPU
/// adapter. (Measured end-to-end on eqoxide#809: removing the wrapper from one call site killed the
/// release binary at startup with a naga error.)
///
/// Runs over the **structurally detected** palette set, not a name-matched one (eqoxide#811).
#[test]
fn raw_palette_shader_sources_do_not_parse_without_substitution() {
    let corpus = shader_corpus();
    let lengths = corpus_palette_lengths();
    let detected: Vec<&String> =
        lengths.iter().filter(|(_, l)| !l.is_empty()).map(|(n, _)| n).collect();
    assert!(!detected.is_empty(), "no palette shader detected — nothing would be checked");
    for name in detected {
        let raw = corpus.get(name).unwrap();
        assert!(
            raw.contains(JOINT_CAP_TOKEN),
            "{name} declares a uniform mat4 palette but does not carry the {JOINT_CAP_TOKEN} \
             placeholder — its length is a second copy of the cap"
        );
        assert!(
            naga::front::wgsl::parse_str(raw).is_err(),
            "{name} parses WITHOUT pipeline::wgsl substitution. That means the palette length is \
             expressible in the shader itself, which is the drift eqoxide#798 removed."
        );
    }
}

/// Backstop source sweep over the whole corpus: no `.wgsl` may write a numeric
/// `array<mat4x4<f32>, N>` length, in any address space.
///
/// **Scope, stated plainly, because this one is weak and it matters that nobody mistakes it for the
/// enforcement.** It is a substring scan for one spelling. It does **not** catch a palette written
/// through a type alias (`alias JMat = mat4x4<f32>; array<JMat, 99>`), nor the whitespace form
/// `array < mat4x4<f32> , 99 >`, nor `array<mat4x4 <f32>, 99>` — both measured limitations, and the
/// alias case is exactly what made eqoxide#811 possible when this test was one of only two things
/// looking at the wider corpus.
///
/// It is kept for the one thing the IR checks structurally cannot cover: a fixed-length mat4 array
/// in a **non-uniform** address space (`var<private>`, `var<storage>`), which is still a copy of the
/// cap but is not a uniform palette, so `uniform_mat4_palette_lengths` correctly ignores it. Every
/// uniform-space palette, in any spelling, is covered by
/// `every_palette_length_tracks_the_substituted_token` instead.
#[test]
fn no_shader_writes_a_numeric_palette_length() {
    let corpus = shader_corpus();
    let mut offenders = Vec::new();
    for (name, src) in &corpus {
        for (i, line) in src.lines().enumerate() {
            let Some(rest) = line.split_once("array<mat4x4<f32>,").map(|(_, r)| r) else {
                continue;
            };
            if rest.trim_start().starts_with(|c: char| c.is_ascii_digit()) {
                offenders.push(format!("{name}:{}: {}", i + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "shader(s) hardcode a numeric mat4 array length instead of the {JOINT_CAP_TOKEN} \
         placeholder — that is a second copy of the cap: {offenders:#?}"
    );
}

// ── eqoxide#812: the token is delimited, so substitution touches nothing else ────────────────────

/// The token cannot be a substring of a WGSL identifier, **by the characters it is made of**.
///
/// WGSL identifiers are XID_Start/XID_Continue plus `_`. `$`, `{` and `}` are none of those, so no
/// identifier can contain the token — which is why `pipeline::wgsl` can stay a plain `replace`
/// instead of growing a word-boundary rule that a future edit could get wrong.
#[test]
fn the_joint_cap_token_cannot_occur_inside_an_identifier() {
    let non_ident: Vec<char> = JOINT_CAP_TOKEN
        .chars()
        .filter(|c| !(c.is_alphanumeric() || *c == '_'))
        .collect();
    assert!(
        !non_ident.is_empty(),
        "JOINT_CAP_TOKEN is {JOINT_CAP_TOKEN:?} — every character of it is identifier-legal, so it \
         is a prefix/infix of longer identifiers and pipeline::wgsl will rewrite them mid-name. \
         That is eqoxide#812: the bare token turned `JOINT_CAP_SCALE` into `128_SCALE`."
    );
}

/// Substitution leaves identifiers that merely *contain* the words alone.
///
/// eqoxide#812, measured on the pre-fix tree: with the bare token, adding
/// `const JOINT_CAP_SCALE: f32 = 1.0;` to a joint shader produced `character_skinned.wgsl: WGSL
/// failed to parse: expected identifier, found '128'`. Exhaustive over the shapes an identifier can
/// take around the word: prefix, suffix, infix, doubled, and the bare word alone.
#[test]
fn substitution_cannot_alter_an_identifier_that_contains_the_word_joint_cap() {
    let cases = [
        "JOINT_CAP",
        "JOINT_CAP_SCALE",
        "MAX_JOINT_CAP",
        "MAX_JOINT_CAP_BYTES",
        "aJOINT_CAPb",
        "JOINT_CAPJOINT_CAP",
        "_JOINT_CAP_",
    ];
    for ident in cases {
        let src = format!("const {ident}: u32 = 1u;\nlet x = {ident};\n");
        assert_eq!(
            wgsl(&src),
            src,
            "substitution rewrote the identifier `{ident}` — a token that is a substring of an \
             identifier mangles it (eqoxide#812)"
        );
    }
    // And the delimited token itself IS substituted, or the whole scheme does nothing.
    assert_eq!(wgsl(JOINT_CAP_TOKEN), JOINT_CAP.to_string());
    assert_eq!(
        wgsl(&format!("array<mat4x4<f32>, {JOINT_CAP_TOKEN}>")),
        format!("array<mat4x4<f32>, {JOINT_CAP}>")
    );
}

/// Every comment in `src`, in source order, as `(1-based line of its opener, text including the
/// opener)`.
///
/// Both WGSL comment forms. `//` runs to end of line. `/* … */` **nests** — WGSL permits
/// `/* /* */ */`, and a scanner that ended the outer comment at the first `*/` would then compare
/// the remaining *code* as if it were prose, which is the quiet direction of wrong.
///
/// WGSL has no string literals, so "a comment opener inside a string" is not a case that exists in
/// this grammar. `substitution_leaves_every_shader_comment_byte_identical` does not take that on
/// trust: it asserts every quote character in every corpus shader falls inside an extracted comment,
/// so if that assumption is ever wrong the test says so instead of silently mis-parsing.
fn comments(src: &str) -> Vec<(usize, String)> {
    comment_spans(src)
        .into_iter()
        .map(|(line, start, end)| (line, src[start..end].to_string()))
        .collect()
}

/// The one comment scanner in this file: `(line, byte_start, byte_end)` for every comment.
///
/// Byte-oriented on purpose. `/`, `*` and `\n` are ASCII, and a UTF-8 continuation byte can never
/// equal an ASCII byte, so scanning bytes cannot land inside a multi-byte character and every offset
/// it reports is a valid `str` boundary.
///
/// [`comments`] and [`shaders_pipeline_includes`] both go through this, deliberately: a second
/// comment scanner written for the second caller is exactly the "two scanners, one of them watched"
/// shape this file exists to prevent.
fn comment_spans(src: &str) -> Vec<(usize, usize, usize)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut line = 1usize;
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            let start = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            out.push((line, start, i));
            continue;
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            let start = i;
            let start_line = line;
            let mut depth = 0usize;
            while i < b.len() {
                if b[i] == b'\n' {
                    line += 1;
                    i += 1;
                    continue;
                }
                if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                    continue;
                }
                if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                i += 1;
            }
            out.push((start_line, start, i));
            continue;
        }
        i += 1;
    }
    out
}

/// Self-check for [`comments`], both directions. A comment scanner that is wrong about what a
/// comment is makes the byte-identity test below pass for the wrong reason.
#[test]
fn comment_extractor_finds_both_forms_including_nesting() {
    let src = "let a = 1; // line\n/* block\n   spans */ let b = 2;\n/* /* nested */ still */ let c = 3;\nlet d = 4;\n";
    let got = comments(src);
    assert_eq!(got.len(), 3, "expected three comments, got {got:?}");
    assert_eq!(got[0], (1, "// line".to_string()));
    assert_eq!(got[1].0, 2, "a block comment is reported at its OPENING line");
    assert_eq!(got[1].1, "/* block\n   spans */");
    // Nesting: the outer comment must not end at the inner `*/`.
    assert_eq!(got[2], (4, "/* /* nested */ still */".to_string()));
    // Must NOT mistake code for a comment: division and multiplication are not openers.
    assert!(comments("let x = a / b;\nlet y = c * d;\n").is_empty());
    // An unterminated block comment is still reported (as running to EOF) rather than dropped.
    assert_eq!(comments("/* oops").len(), 1);
}

/// The corpus oracle's scan ingests `include_str!` paths from **code** and ignores them in **prose**.
///
/// This exists because the previous docstring asserted the property and the code did not have it: a
/// comment spelling `include_str!("shaders/ghost.wgsl")` was ingested as a real shader name. That
/// was not merely a false alarm — the phantom padded `MIN_PIPELINE_INCLUDED_SHADERS`, so a scan that
/// silently dropped a genuinely-included shader still measured 14 passed / 0 failed (RC-M9). Both
/// halves are asserted here so the fix cannot regress into either direction.
///
/// Driven against a synthetic source rather than the real `pipeline.rs`, so it keeps testing the
/// rule after the real file changes.
#[test]
fn the_include_scan_ignores_comments_but_not_code() {
    let src = concat!(
        "pub const A: &str = include_str!(\"shaders/real_one.wgsl\");\n",
        "// include_str!(\"shaders/line_ghost.wgsl\")\n",
        "/* include_str!(\"shaders/block_ghost.wgsl\") */\n",
        "/* /* include_str!(\"shaders/nested_ghost.wgsl\") */ */\n",
        "pub const B: &str = include_str!(\"shaders/real_two.wgsl\");\n",
        "pub const C: &str = include_str!(\"shaders/not_a_shader.txt\");\n",
    );
    let got = scan_include_str_shaders(src, "<synthetic>");

    let want: BTreeSet<String> = ["real_one.wgsl", "real_two.wgsl"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(got, want, "the scan must take code hits and only code hits");

    // Said again as the two failure directions, so a regression names which way it broke.
    for ghost in ["line_ghost.wgsl", "block_ghost.wgsl", "nested_ghost.wgsl"] {
        assert!(
            !got.contains(ghost),
            "{ghost} came from a COMMENT — a phantom name inflates the oracle's floor and can mask a \
             real name the scan lost (measured: RC-M9 stayed 14 passed / 0 failed)"
        );
    }
    for real in ["real_one.wgsl", "real_two.wgsl"] {
        assert!(
            got.contains(real),
            "{real} is a genuine include_str! in code and the comment filter swallowed it — the \
             oracle is now smaller than the truth, which is the direction that under-reports a \
             truncated walk"
        );
    }

    // A comment that is not adjacent to any hit must not shift the offsets of the hits around it.
    let shifted = scan_include_str_shaders(
        concat!(
            "/* leading prose */\n",
            "pub const A: &str = include_str!(\"shaders/real_one.wgsl\");\n",
        ),
        "<synthetic>",
    );
    assert_eq!(shifted.len(), 1, "a leading comment must not desynchronise the scan: {shifted:?}");
}

/// Substitution leaves every shader **comment** byte-identical.
///
/// eqoxide#812's second half: with the bare token, `pipeline::wgsl` rewrote the shaders' own prose,
/// so the text naga received read "`128` is NOT a WGSL constant … substitutes the Rust
/// `renderer::128` into this text". Self-falsifying documentation shipped to the compiler. This
/// compares every comment of every corpus shader, before and after substitution.
///
/// Scope: line **and** block comments, via [`comments`]. The line-only version this replaces was
/// honest about not modelling blocks, but a reviewer measured the consequence — a block comment
/// spelling the token is rewritten *silently*, where a line comment is caught loudly — so the gap is
/// closed rather than disclosed. Both forms have a positive control below; no block comment exists
/// in the corpus today, so that control is the only thing proving the block half works.
#[test]
fn substitution_leaves_every_shader_comment_byte_identical() {
    let corpus = shader_corpus();
    let mut compared = 0usize;
    for (name, raw) in &corpus {
        let sub = wgsl(raw);
        let raw_c = comments(raw);
        let sub_c = comments(&sub);

        // The extractor's no-string-literal premise, asserted rather than assumed.
        let quotes_in_src = raw.matches('"').count();
        let quotes_in_comments: usize = raw_c.iter().map(|(_, t)| t.matches('"').count()).sum();
        assert_eq!(
            quotes_in_src, quotes_in_comments,
            "{name}: a quote character occurs OUTSIDE a comment. WGSL has no string literals, which \
             is why `comments` does not model one — if that stopped being true, this comparison can \
             mis-parse and must be revisited"
        );

        assert_eq!(
            raw_c.len(),
            sub_c.len(),
            "{name}: substitution changed how many comments the shader has"
        );
        for ((rl, r), (_, s)) in raw_c.iter().zip(sub_c.iter()) {
            compared += 1;
            assert_eq!(r, s, "{name}:{rl}: pipeline::wgsl rewrote a comment (eqoxide#812)");
        }
    }
    assert!(
        compared > 0,
        "no comments were compared across {} shader(s) — this test would pass vacuously",
        corpus.len()
    );
    // Positive controls for the comparison itself, one per comment form: a comment that DOES carry
    // the token must be both rewritten by wgsl() and visible as a difference to `comments`.
    for (form, doctored) in [
        ("line", format!("// the {JOINT_CAP_TOKEN} placeholder\n")),
        ("block", format!("/* the {JOINT_CAP_TOKEN} placeholder */\n")),
    ] {
        let sub = wgsl(&doctored);
        assert_ne!(sub, doctored, "wgsl() must rewrite a {form} comment that spells the token");
        assert_ne!(
            comments(&doctored),
            comments(&sub),
            "the comparison above can only detect a rewritten {form} comment if it sees this one"
        );
    }
}

// ── The Rust side: one palette builder, one length ──────────────────────────────────────────────

/// `pad_joint_palette` produces exactly `JOINT_CAP` matrices, whatever it is handed.
///
/// This is the mutation-discriminating test for the Rust half: setting `JointPalette`'s length or
/// `pad_joint_palette`'s fill to anything but `JOINT_CAP` fails here. The five former copies of
/// this loop no longer exist — `pass.rs` (×3) and the `render_model` dev bin (×2) call this
/// function, which the compiler proves by naming all five if its signature changes.
#[test]
fn pad_joint_palette_is_always_exactly_joint_cap_long() {
    for n in [0usize, 1, 17, JOINT_CAP - 1, JOINT_CAP, JOINT_CAP + 1, JOINT_CAP * 2] {
        let mats: Vec<[[f32; 4]; 4]> = (0..n).map(|i| [[i as f32; 4]; 4]).collect();
        let palette = pad_joint_palette(&mats);
        assert_eq!(palette.len(), JOINT_CAP, "n={n}");
        // The first min(n, JOINT_CAP) slots are the caller's matrices, in order.
        for (i, m) in mats.iter().take(JOINT_CAP).enumerate() {
            assert_eq!(&palette[i], m, "n={n} slot={i}: caller's matrix must survive");
        }
        // Every remaining slot is the identity — an unwritten joint must not move geometry.
        let id4 = [[1., 0., 0., 0.], [0., 1., 0., 0.], [0., 0., 1., 0.], [0., 0., 0., 1.]];
        for (i, slot) in palette.iter().enumerate().skip(n.min(JOINT_CAP)) {
            assert_eq!(slot, &id4, "n={n} slot={i}: unused slots must be identity");
        }
    }
}

/// The two files that used to hand-roll a joint palette, relative to the WORKSPACE root.
///
/// `pass.rs` held three copies and the `render_model` dev bin two. `renderer.rs` is deliberately
/// absent: it is the single definition site, and `JOINT_CAP = 128` living there is the whole point.
const JOINT_SITE_FILES: [&str; 2] =
    ["crates/eqoxide-renderer/src/pass.rs", "src/bin/render_model.rs"];

/// Workspace root, derived from this crate's manifest dir (`<root>/crates/eqoxide-renderer`).
fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate manifest dir has a workspace root two levels up")
        .to_path_buf()
}

/// Does `code` contain an integer literal of 2+ digits?
///
/// "Literal" means the digit run is not preceded by an identifier character, so the `32` in `u32` /
/// `f32` and the `4` in `mat4x4` are not literals — without that, every `[u32; 4]` in a
/// joint-mentioning line would be a false positive (measured: 4 of them across the two files).
/// Single-digit literals are allowed through: pool slot indices are legitimate.
fn has_multidigit_literal(code: &str) -> bool {
    let b: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let attached_to_ident =
            start > 0 && (b[start - 1].is_alphanumeric() || b[start - 1] == '_');
        if !attached_to_ident && i - start >= 2 {
            return true;
        }
    }
    false
}

/// Self-check for [`has_multidigit_literal`]: the discriminating cases, both directions. A scanner
/// whose predicate is wrong is a scanner that reports clean for the wrong reason.
#[test]
fn multidigit_literal_predicate_discriminates() {
    // Must fire — these are the reintroduced-copy shapes.
    assert!(has_multidigit_literal("let mut joint_array = [id4; 127];"));
    assert!(has_multidigit_literal("for (i, m) in matrices.iter().enumerate().take(128)"));
    assert!(has_multidigit_literal(r#"label: Some("joints"), size: 128 * 64,"#));
    // Must NOT fire — type suffixes and 4x4 matrix shapes are not palette lengths.
    assert!(!has_multidigit_literal("joint_indices: [u32; 4],"));
    assert!(!has_multidigit_literal("mats: &[[[f32;4];4]]"));
    assert!(!has_multidigit_literal("r.queue.write_buffer(&r.joint_buf_pool[0].0, 0, x)"));
    assert!(!has_multidigit_literal("let joint_array = pad_joint_palette(&matrices);"));
}

/// No draw site may state a palette length: in `pass.rs` and the `render_model` dev bin, a line
/// that mentions a joint may not carry a multi-digit integer literal.
///
/// **What this proves and what it does not.** It is a source-text scan, so it proves the absence of
/// this *spelling* — a reintroduced `[id4; 127]` / `.take(127)` / `size: 127 * 64` next to the word
/// "joint". It does not prove any call site currently calls `pad_joint_palette`; that is proven by
/// the compiler, which names all five sites if the function's signature changes (demonstrated in
/// the eqoxide#798 PR body). It is a *negative* scan, so the `if false {}` / `#[cfg(any())]` /
/// shadowing evasions that defeat `contains`-style call pins do not apply — those hide a call, and
/// this test is not looking for one. Line comments are stripped first so prose may still say 128.
///
/// One-digit literals are allowed through deliberately: pool slot indices (`joint_buf_pool[0]`,
/// `write_buffer(&sk.joints_buf, 0, ..)`) are legitimate and unrelated to the cap.
#[test]
fn no_draw_site_states_a_joint_palette_length() {
    let root = workspace_root();
    let mut offenders = Vec::new();
    let mut scanned = 0usize;
    for rel in JOINT_SITE_FILES {
        let path = root.join(rel);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read joint-site file {}: {e}", path.display()));
        scanned += 1;
        for (i, raw) in src.lines().enumerate() {
            // Strip line comments: doc/prose may legitimately mention 128.
            let code = raw.split("//").next().unwrap_or("");
            if !code.to_ascii_lowercase().contains("joint") {
                continue;
            }
            if has_multidigit_literal(code) {
                offenders.push(format!("{rel}:{}: {}", i + 1, raw.trim()));
            }
        }
    }
    // Reach control: the scan must have actually opened every file it claims to cover. A walk that
    // read nothing would otherwise report clean (eqoxide#778).
    assert_eq!(
        scanned,
        JOINT_SITE_FILES.len(),
        "scanned {scanned} of {} joint-site files",
        JOINT_SITE_FILES.len()
    );
    assert!(
        offenders.is_empty(),
        "a draw site states a joint-palette length again instead of calling \
         renderer::pad_joint_palette — that is a fresh copy of the cap: {offenders:#?}"
    );
}

/// The palette and the uniform buffer it is written into are the same size.
///
/// `JOINT_BUF_BYTES` sizes every `joint_buf_pool` / `shadow_joint_pool` buffer; `JointPalette` is
/// what `bytemuck::cast_slice` writes into them. A palette larger than the buffer is a wgpu
/// validation error at runtime; smaller leaves stale matrices in the tail.
#[test]
fn palette_and_uniform_buffer_are_the_same_size() {
    assert_eq!(std::mem::size_of::<JointPalette>() as u64, JOINT_BUF_BYTES);
    assert_eq!(JOINT_BUF_BYTES, JOINT_CAP as u64 * 64);
}
