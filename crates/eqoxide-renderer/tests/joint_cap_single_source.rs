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
//! `mat4x4<f32>` array it finds, and every check below runs it over **every** `.wgsl` in the
//! discovered corpus rather than over a list. A palette is detected by being one, so a struct name,
//! a type alias (naga resolves aliases before the arena exists) and whitespace inside the
//! declaration are all irrelevant by construction.
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
//! - The corpus is **discovered from disk** (`src/shaders/*.wgsl` under `CARGO_MANIFEST_DIR`), not
//!   listed in this file. A shader added tomorrow is scanned tomorrow.
//! - `discovered_corpus_is_not_silently_truncated` asserts the walk found a plausible corpus, that
//!   every file the palette checks name is inside it, and that **every discovered file was actually
//!   parsed** — so "the scanner couldn't see that file" fails loudly instead of passing quietly.
//! - `palette_bearing_shaders_are_exactly_the_expected_three` asserts the structurally-detected
//!   palette set equals the expected three. Both directions of that assertion were planted and run
//!   for eqoxide#811 (a fourth palette shader added; a known palette removed) — see the PR body's
//!   mutation table.
//!
//! Nothing in this header claims a mutation went red that was not planted and observed. The claims
//! whose mutations are in the eqoxide#798 PR body: the three shaders' palette lengths and the five
//! collapsed Rust sites. The claims whose mutations are in the eqoxide#811/812/813/814 PR body: the
//! structural detection set (both directions), the whole-corpus length check via a decoy shader, the
//! token-tracking check, the delimited-token identifier property, and the comment-preservation
//! property.

use eqoxide_renderer::pipeline::{wgsl, JOINT_CAP_TOKEN};
use eqoxide_renderer::renderer::{pad_joint_palette, JointPalette, JOINT_BUF_BYTES, JOINT_CAP};
use std::collections::BTreeMap;

/// The shaders expected to declare a joint palette. Asserted to be EXACTLY the corpus's
/// **structurally detected** palette set by `palette_bearing_shaders_are_exactly_the_expected_three`
/// — detection is "the validated IR contains a fixed-length `mat4x4<f32>` array in a uniform-space
/// struct", not a name or spelling. A fourth shader that adds such a palette under any struct name
/// fails that assertion instead of slipping past it (eqoxide#811).
const EXPECTED_JOINT_SHADERS: [&str; 3] =
    ["character_skinned.wgsl", "shadow.wgsl", "skin_probe.wgsl"];

/// Every `.wgsl` under `src/shaders/`, read from DISK at test time (filename → raw source).
///
/// Deliberately not a hardcoded list of `include_str!`s: the corpus this test claims to cover has
/// to be the corpus that actually exists, or the reach claim is a guess.
fn shader_corpus() -> BTreeMap<String, String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/shaders");
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read shader dir {}: {e}", dir.display()));
    let mut out = BTreeMap::new();
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("wgsl") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        out.insert(name, src);
    }
    out
}

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

/// The disk walk found a real corpus, everything the palette checks name is inside it, and every
/// discovered file was parsed.
///
/// This is the eqoxide#778 guard-reach control expressed as an assertion rather than as a belief:
/// on that issue a scanner silently stopped at ~12% of its corpus and reported clean, and every
/// mutation probe happened to land in the window it could still see. A truncated walk fails on the
/// count, a covered file that fell out of the walk fails on the membership check, and a file the
/// parse pass dropped fails on the third.
#[test]
fn discovered_corpus_is_not_silently_truncated() {
    let corpus = shader_corpus();
    assert!(
        corpus.len() >= EXPECTED_JOINT_SHADERS.len(),
        "shader corpus walk returned {} file(s) — fewer than the {} this test claims to cover; a \
         truncated walk must not look like a clean one. Found: {:?}",
        corpus.len(),
        EXPECTED_JOINT_SHADERS.len(),
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

/// Every palette length in a palette-bearing shader **tracks the substituted token**, whatever value
/// is substituted.
///
/// This is the check that makes a hardcoded-but-currently-correct length impossible. `array<JMat,
/// 128>` written literally compiles to the same IR as the placeholder does today, so
/// `every_uniform_mat4_palette_in_the_corpus_is_exactly_joint_cap` cannot tell them apart. Here the
/// token is substituted with a SENTINEL that is deliberately not `JOINT_CAP`: a length derived from
/// the token moves to the sentinel, a length written as a literal does not, and the mismatch is the
/// failure. Spelling-, alias- and whitespace-independent, because the comparison is against the IR.
#[test]
fn every_palette_length_tracks_the_substituted_token() {
    const SENTINEL: u32 = 77;
    assert_ne!(SENTINEL as usize, JOINT_CAP, "the sentinel must differ from the real cap");
    let corpus = shader_corpus();
    let mut checked = 0usize;
    for name in EXPECTED_JOINT_SHADERS {
        let raw = corpus.get(name).unwrap_or_else(|| panic!("{name} missing from the corpus"));
        let substituted = raw.replace(JOINT_CAP_TOKEN, &SENTINEL.to_string());
        let module = parse_and_validate(&substituted, name);
        let lens = uniform_mat4_palette_lengths(&module);
        assert!(!lens.is_empty(), "{name}: no palette found under sentinel substitution");
        for len in lens {
            assert_eq!(
                len, SENTINEL,
                "{name}: a uniform mat4 palette is {len} when the token is substituted with \
                 {SENTINEL} — that length is written into the shader, not derived from \
                 renderer::JOINT_CAP"
            );
            checked += 1;
        }
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

/// Substitution leaves every shader **comment** byte-identical.
///
/// eqoxide#812's second half: with the bare token, `pipeline::wgsl` rewrote the shaders' own prose,
/// so the text naga received read "`128` is NOT a WGSL constant … substitutes the Rust
/// `renderer::128` into this text". Self-falsifying documentation shipped to the compiler. This
/// compares the comment portion of every line of every corpus shader before and after substitution.
///
/// Scope: this is a line-comment comparison (`//` onwards), which is the only comment form these
/// shaders use — it does not model WGSL block comments or `//` inside a string literal, neither of
/// which occurs in the corpus.
#[test]
fn substitution_leaves_every_shader_comment_byte_identical() {
    let corpus = shader_corpus();
    let mut compared = 0usize;
    for (name, raw) in &corpus {
        let sub = wgsl(raw);
        let raw_lines: Vec<&str> = raw.lines().collect();
        let sub_lines: Vec<&str> = sub.lines().collect();
        assert_eq!(raw_lines.len(), sub_lines.len(), "{name}: substitution changed the line count");
        for (i, (r, s)) in raw_lines.iter().zip(sub_lines.iter()).enumerate() {
            let (Some(rc), Some(sc)) = (r.find("//"), s.find("//")) else { continue };
            compared += 1;
            assert_eq!(
                &r[rc..],
                &s[sc..],
                "{name}:{}: pipeline::wgsl rewrote a comment (eqoxide#812)",
                i + 1
            );
        }
    }
    assert!(
        compared > 0,
        "no comment lines were compared across {} shader(s) — this test would pass vacuously",
        corpus.len()
    );
    // Positive control for the comparison itself: a comment that DOES carry the token is caught.
    let doctored = format!("// the {JOINT_CAP_TOKEN} placeholder\n");
    assert_ne!(
        wgsl(&doctored),
        doctored,
        "the comparison above can only detect a rewritten comment if wgsl() rewrites this one"
    );
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
