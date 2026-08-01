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
//!   `JOINT_CAP`, and `pipeline::wgsl` — the single preprocessing step every `create_shader_module`
//!   in the tree goes through — substitutes `renderer::JOINT_CAP` into the text before naga (and
//!   therefore the driver) ever sees it. A GPU-side copy that could drift no longer exists.
//! - The five Rust palette-building copies collapsed into `renderer::pad_joint_palette`, which
//!   returns the named type `renderer::JointPalette = [[[f32;4];4]; JOINT_CAP]`. A draw site cannot
//!   choose a different length because it no longer states one.
//!
//! ## What this test adds on top, and why it is not a source-text pin
//!
//! The checks below read the **parsed and validated naga IR**, not the shader source text.
//! `uniform_struct_array_lengths` resolves each shader's uniform-address-space struct members
//! through naga's type arena and reads the array size out of the IR — the same representation the
//! backend lowers to SPIR-V/MSL/HLSL. That is immune to the whole `include_str!`-plus-`contains`
//! evasion family this repo has measured six times over (trailing comment, shadowing, `if false
//! {}`, `#[cfg(any())]`, macro-elided argument, satisfying the guard from an unrelated comment):
//! you cannot comment or `cfg` your way to a different validated array size — for a shader this
//! test already knows to look at. Two other checks here ARE source-text scans, not IR reads:
//! `no_shader_writes_a_numeric_palette_length`, a belt-and-braces sweep across shaders the IR
//! checks do not know about (scope stated at that test); and
//! `joint_palette_shaders_are_exactly_the_expected_three`'s struct-name match, which decides which
//! shaders the IR check above even looks at in the first place (scope, and its measured gap,
//! stated at that test).
//!
//! ## Reach control (the eqoxide#778 lesson)
//!
//! A scanner that silently stops short of its corpus is indistinguishable from a passing one. So:
//!
//! - The corpus is **discovered from disk** (`src/shaders/*.wgsl` under `CARGO_MANIFEST_DIR`), not
//!   listed in this file. A shader added tomorrow is scanned tomorrow.
//! - `discovered_corpus_is_not_silently_truncated` asserts the walk found a plausible corpus AND
//!   that every file named by the IR checks is inside it — so "the scanner couldn't see that file"
//!   fails loudly instead of passing quietly.
//! - `joint_palette_shaders_are_exactly_the_expected_three` asserts the set of shaders whose
//!   source contains the struct name `JointMatrices` equals the expected three. That is a **name
//!   match on source text, not a structural check** for "declares a fixed-length uniform
//!   `mat4x4` palette." A new shader that declares exactly such a palette under any other struct
//!   name — including through a type alias, which also defeats the numeric sweep above — is
//!   invisible to it: measured by planting a fourth shader (`alias JMat = mat4x4<f32>; struct
//!   DecoyPalette { mats: array<JMat, 99> }; @group(0) @binding(0) var<uniform> decoy:
//!   DecoyPalette;`) into the corpus and observing the suite stay 9 passed / 0 failed. Making
//!   this structural — reading the IR instead of matching a name — is tracked as a separate
//!   follow-up, not done here.
//!
//! The `JOINT_CAP`-value claims above — the three shaders and the five collapsed Rust sites —
//! were each demonstrated by planting a mismatch in the corresponding file and running this test;
//! see the PR body's per-file mutation table. The two reach-control bullets above this paragraph
//! were **not** exercised by that table: no mutation there truncates the corpus walk or renames a
//! struct, so those bullets' own "goes red" claims rest on reading the assertion, not on a
//! planted-and-observed failure — except for the fourth-shader gap just measured above, which
//! goes the other way (it stays green when the doc used to claim red).

use eqoxide_renderer::pipeline::{wgsl, JOINT_CAP_TOKEN};
use eqoxide_renderer::renderer::{pad_joint_palette, JointPalette, JOINT_BUF_BYTES, JOINT_CAP};
use std::collections::BTreeMap;

/// The shaders whose source is expected to contain the struct name `JointMatrices`. Asserted to
/// be EXACTLY the corpus's `JointMatrices`-matching set (not merely a subset) by
/// `joint_palette_shaders_are_exactly_the_expected_three` — a source-text name match, not a
/// structural one. A shader that adds a fixed-length uniform `mat4x4` palette under a different
/// struct name falls behind this list unnoticed; see that test's docstring for the measured gap.
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

/// Every fixed array length that appears as a member of a struct bound in the **uniform** address
/// space, read out of the validated IR's type arena.
///
/// This is the value the backend lowers into the shader the driver compiles — not a substring of a
/// file. `JointMatrices { mats: array<mat4x4<f32>, N> }` bound as `var<uniform>` yields `[N]`.
fn uniform_struct_array_lengths(module: &naga::Module) -> Vec<u32> {
    let mut lengths = Vec::new();
    for (_, global) in module.global_variables.iter() {
        if global.space != naga::AddressSpace::Uniform {
            continue;
        }
        let naga::TypeInner::Struct { members, .. } = &module.types[global.ty].inner else {
            continue;
        };
        for member in members {
            if let naga::TypeInner::Array { size, .. } = &module.types[member.ty].inner {
                if let naga::ArraySize::Constant(n) = size {
                    lengths.push(n.get());
                }
            }
        }
    }
    lengths
}

// ── Reach control ───────────────────────────────────────────────────────────────────────────────

/// The disk walk found a real corpus, and everything the IR checks below name is inside it.
///
/// This is the eqoxide#778 guard-reach control expressed as an assertion rather than as a belief:
/// on that issue a scanner silently stopped at ~12% of its corpus and reported clean, and every
/// mutation probe happened to land in the window it could still see. A truncated walk here fails
/// on the count, and a covered file that fell out of the walk fails on the membership check.
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
}

/// The set of shaders whose source contains the struct name `JointMatrices` is EXACTLY
/// `EXPECTED_JOINT_SHADERS`.
///
/// This is a **name match on source text**, not a structural check for "declares a fixed-length
/// uniform `mat4x4` palette." If one of the three known shaders' source stops containing
/// `JointMatrices` (loses the palette, or renames the struct), the detected set no longer equals
/// `EXPECTED_JOINT_SHADERS` and this assertion fails — but that specific mutation was not planted
/// in this PR, so treat it as read from the code, not measured. What WAS measured: a *new*
/// shader that declares such a palette under a different struct name — including via a type
/// alias, which also defeats `no_shader_writes_a_numeric_palette_length`'s text sweep — is
/// invisible to this check. Planting exactly that fourth shader left the suite at 9 passed / 0
/// failed. Making detection structural (read the IR instead of matching a name) is tracked as a
/// separate follow-up.
#[test]
fn joint_palette_shaders_are_exactly_the_expected_three() {
    let corpus = shader_corpus();
    let mut actual: Vec<&str> = corpus
        .iter()
        .filter(|(_, src)| src.contains("JointMatrices"))
        .map(|(name, _)| name.as_str())
        .collect();
    actual.sort_unstable();
    let mut expected = EXPECTED_JOINT_SHADERS.to_vec();
    expected.sort_unstable();
    assert_eq!(
        actual, expected,
        "the set of shaders declaring a joint palette changed; update EXPECTED_JOINT_SHADERS (and \
         confirm the new one is covered) rather than letting an unguarded palette ship"
    );
}

// ── The IR check: the GPU-side palette length IS `JOINT_CAP` ────────────────────────────────────

/// For each joint shader, the palette length in the **validated IR** equals `JOINT_CAP`.
///
/// Not a text search: `uniform_struct_array_lengths` reads naga's type arena after validation.
#[test]
fn every_joint_shader_compiles_to_a_palette_of_exactly_joint_cap() {
    let corpus = shader_corpus();
    for name in EXPECTED_JOINT_SHADERS {
        let raw = corpus
            .get(name)
            .unwrap_or_else(|| panic!("{name} missing from the shader corpus"));
        let module = parse_and_validate(&wgsl(raw), name);
        let lengths = uniform_struct_array_lengths(&module);
        assert!(
            !lengths.is_empty(),
            "{name}: no fixed-length array in any uniform struct — the palette this test exists to \
             pin is not there, so a passing result would mean nothing"
        );
        for len in lengths {
            assert_eq!(
                len as usize, JOINT_CAP,
                "{name}: the GPU-side uniform palette length is {len}, but renderer::JOINT_CAP is \
                 {JOINT_CAP}. The shader is the real enforcement — do not write a number in the \
                 .wgsl; write {JOINT_CAP_TOKEN} and let pipeline::wgsl substitute it."
            );
        }
    }
}

/// A shader source that skips `pipeline::wgsl` is **invalid WGSL**, so a missed substitution cannot
/// ship a silently wrong cap.
///
/// This is what lets `pipeline::wgsl` be a preprocessing step rather than a discipline: the bare
/// identifier `JOINT_CAP` has no declaration in any of these files, and naga rejects an undeclared
/// identifier used as an array length. Note what this does and does not prove — it proves the raw
/// sources do not compile, i.e. that forgetting the substitution FAILS LOUDLY at
/// `create_shader_module`. It does not prove any particular call site currently calls `wgsl()`;
/// that is proven by the whole render path failing to produce a pipeline, which no test in this
/// crate can observe without a GPU adapter.
#[test]
fn raw_joint_shader_sources_do_not_parse_without_substitution() {
    let corpus = shader_corpus();
    for name in EXPECTED_JOINT_SHADERS {
        let raw = corpus.get(name).unwrap();
        assert!(
            raw.contains(JOINT_CAP_TOKEN),
            "{name} no longer carries the {JOINT_CAP_TOKEN} placeholder — if it now carries a \
             number instead, the GPU-side copy of the cap is back"
        );
        assert!(
            naga::front::wgsl::parse_str(raw).is_err(),
            "{name} parses WITHOUT pipeline::wgsl substitution. That means the palette length is \
             expressible in the shader itself, which is the drift eqoxide#798 removed."
        );
    }
}

/// Belt-and-braces source sweep over the WHOLE corpus, including shaders with no joint palette
/// today: no `.wgsl` may write a numeric `array<mat4x4<f32>, N>` length.
///
/// Scope, stated plainly: this is a text scan, and a text scan proves only that this spelling is
/// absent. It is not what enforces the cap — `every_joint_shader_compiles_to_a_palette_of_exactly_
/// joint_cap` is. It exists to catch a *new* shader introducing a hardcoded palette before anyone
/// remembers to add it to `EXPECTED_JOINT_SHADERS`.
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
