//! Device-free coverage for `SkinFit` — the renderer's skinned-vs-static model selection
//! (eqoxide#780).
//!
//! Whether a loaded model uses the skinned (GPU-animated) render arm or the static one used to be a
//! single `bool` (`joint_count > 0 && joint_count <= 128`), which folded three different
//! situations into the same silent static fallback: no skin at all (unremarkable — e.g.
//! `boat.glb`), a skin with zero joints (degenerate data), and a skin with MORE than the 128-joint
//! uniform-buffer cap. Only the third is a genuine downgrade. `renderer::SkinFit::classify` names
//! the split explicitly (`NoSkin` / `EmptySkin` / `ExceedsCap` / `Fits`), and
//! `StaticReason::downgrade_joint_count` is the gate `ensure_character_model` uses to log the
//! `ExceedsCap` case at `error!`.
//!
//! ## What this file does NOT cover
//!
//! - `EqRenderer::ensure_character_model` and `build_character_model` themselves — both need a
//!   `wgpu::Device`, which nothing in this crate's test harness can construct (see
//!   `shadow_routing.rs`'s header for the precedent).
//! - The text of the `error!` log line.
//!
//! ## Later additions
//!
//! - **eqoxide#813** — `renderer::resolve_character_model_path` (which GLB an `(archetype, gender)`
//!   pair loads) is driven here against a scratch asset dir.
//! - **eqoxide#814** — a floor on `JOINT_CAP`'s VALUE, with the measured corpus figures it comes
//!   from. See `joint_cap_clears_the_widest_shipped_rig`; the primary enforcement is the `const`
//!   assertion in `renderer.rs`, which makes a too-small cap a compile error.

use eqoxide_renderer::renderer::{
    resolve_character_model_path, SkinFit, StaticReason, JOINT_CAP,
    MAX_MEASURED_CHARACTER_RIG_JOINTS,
};
use std::path::Path;

// ── Differential pin: #780 changes NO rendering behaviour ──────────────────────────────────────

/// Verbatim transcription of the pre-#780 boolean at the merge-base of this branch (`renderer.rs`,
/// `asset.skin.as_ref().is_some_and(|s| s.joint_count > 0 && s.joint_count <= 128)`), with
/// `Option<SkinData>` reduced to the one field it read.
fn old_use_skinned(joint_count: Option<usize>) -> bool {
    joint_count.is_some_and(|n| n > 0 && n <= 128)
}

/// `SkinFit::classify(..).is_skinned()` must agree with the deleted boolean over every joint count
/// that matters: 0, the cap boundary on both sides, and comfortably past it. The issue this file
/// fixes is explicit that #780 is "not a placement change" — this is what proves that half of the
/// claim, the other half (recording/logging) has no pre-#780 behaviour to diff against because it
/// didn't exist.
#[test]
fn is_skinned_agrees_with_the_pre_780_boolean_over_the_whole_range() {
    for n in 0..=400usize {
        let jc = Some(n);
        assert_eq!(
            SkinFit::classify(jc).is_skinned(), old_use_skinned(jc),
            "joint_count={n}: SkinFit diverged from the pre-#780 boolean"
        );
    }
    assert_eq!(SkinFit::classify(None).is_skinned(), old_use_skinned(None));
}

// ── The three-way split itself ──────────────────────────────────────────────────────────────────

/// Universal over the whole representable range (not just the boundary), because the claim is
/// universal: classify is `Fits` iff `1..=JOINT_CAP`, `ExceedsCap` iff `> JOINT_CAP`, `EmptySkin`
/// iff exactly `0`, and `NoSkin` iff there was no skin at all. Every joint count lands in exactly
/// one bucket.
#[test]
fn classify_partitions_every_joint_count_into_exactly_one_bucket() {
    assert_eq!(SkinFit::classify(None), SkinFit::NoSkin);
    for n in 0..=(JOINT_CAP * 3) {
        let fit = SkinFit::classify(Some(n));
        match n {
            0 => assert_eq!(fit, SkinFit::EmptySkin, "n={n}"),
            n if n <= JOINT_CAP => assert_eq!(fit, SkinFit::Fits { joint_count: n }, "n={n}"),
            n => assert_eq!(fit, SkinFit::ExceedsCap { joint_count: n }, "n={n}"),
        }
    }
}

/// The cap is INCLUSIVE — a skin with exactly `JOINT_CAP` joints fits, matching the deleted `<=
/// 128`. Getting this off by one either rejects the widest rig that currently ships
/// (`race_pcfroglok.glb` at 127 is comfortably under it either way, but the boundary itself must be
/// right) or silently accepts one joint too many for the fixed-size uniform buffer.
#[test]
fn the_cap_is_inclusive() {
    assert_eq!(SkinFit::classify(Some(JOINT_CAP)), SkinFit::Fits { joint_count: JOINT_CAP });
    assert!(SkinFit::classify(Some(JOINT_CAP)).is_skinned());
    assert_eq!(SkinFit::classify(Some(JOINT_CAP + 1)),
        SkinFit::ExceedsCap { joint_count: JOINT_CAP + 1 });
    assert!(!SkinFit::classify(Some(JOINT_CAP + 1)).is_skinned());
}

// ── The issue's own acceptance test: the two static arms must be distinguishable ───────────────

/// This is the exact test eqoxide#780 asks for under "Verification": a synthetic model with
/// `joint_count = 129` takes the static arm AND is reported as a downgrade, while one with
/// `joint_count = 0` takes the static arm and is NOT reported. Before #780 both were the same
/// `bool` and nothing distinguished them.
#[test]
fn exceeding_the_cap_and_having_no_joints_both_go_static_but_are_distinguishable() {
    let over  = SkinFit::classify(Some(129));
    let empty = SkinFit::classify(Some(0));
    let none  = SkinFit::classify(None);

    // All three take the static arm.
    assert!(!over.is_skinned());
    assert!(!empty.is_skinned());
    assert!(!none.is_skinned());

    // Only the cap-exceeding one is reported as a downgrade.
    assert_eq!(over.static_reason(), Some(StaticReason::ExceedsCap { joint_count: 129 }));
    assert_eq!(over.static_reason().unwrap().downgrade_joint_count(), Some(129),
        "a skin with 129 joints (over the 128 cap) must be reported as a downgrade");

    assert_eq!(empty.static_reason(), Some(StaticReason::EmptySkin));
    assert_eq!(empty.static_reason().unwrap().downgrade_joint_count(), None,
        "a skin with 0 joints is degenerate data, not a cap downgrade — must not be reported as one");

    assert_eq!(none.static_reason(), Some(StaticReason::NoSkin));
    assert_eq!(none.static_reason().unwrap().downgrade_joint_count(), None,
        "an unskinned model (e.g. boat.glb) must not be reported as a downgrade");
}

/// A `Fits` skin fit has no `StaticReason` at all — it never reaches the static arm, so asking
/// "why is it static" is a type error waiting to happen if this ever returned `Some`.
#[test]
fn a_fitting_skin_has_no_static_reason() {
    assert_eq!(SkinFit::classify(Some(1)).static_reason(), None);
    assert_eq!(SkinFit::classify(Some(JOINT_CAP)).static_reason(), None);
}

/// A scratch asset dir containing the given GLB file names (empty files — only their *existence*
/// matters to `resolve_character_model_path`). Removed on drop.
///
/// The resolver test below runs the **real** `renderer::resolve_character_model_path`, the
/// same function `ensure_character_model` calls, rather than a transcription of it: eqoxide#813 is
/// about the relationship between `(label, gender)` and the file that ends up loaded, so a test
/// that restates that relationship would grade its own restatement.
struct ScratchAssets(std::path::PathBuf);

impl ScratchAssets {
    fn with(tag: &str, files: &[&str]) -> Self {
        let dir = std::env::temp_dir().join(format!("eqoxide-skin-cap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch asset dir");
        for f in files {
            std::fs::write(dir.join(f), b"").expect("create scratch glb");
        }
        ScratchAssets(dir)
    }
    fn path(&self) -> &Path { &self.0 }
}

impl Drop for ScratchAssets {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

/// The resolver itself, on the two shapes that matter: with an `_f` variant present gender 1 gets
/// its own file; without one it falls back to the male GLB. Read directly rather than assumed,
/// because `ensure_character_model` loads whatever it returns.
#[test]
fn gender_1_falls_back_to_the_male_glb_when_no_f_variant_exists() {
    let with_f = ScratchAssets::with("resolver-with-f", &["race_hum.glb", "race_hum_f.glb"]);
    assert_eq!(resolve_character_model_path(with_f.path(), "race_hum", 1),
        with_f.path().join("race_hum_f.glb"));
    assert_eq!(resolve_character_model_path(with_f.path(), "race_hum", 0),
        with_f.path().join("race_hum.glb"));

    let without_f = ScratchAssets::with("resolver-no-f", &["race_hum.glb"]);
    assert_eq!(resolve_character_model_path(without_f.path(), "race_hum", 1),
        without_f.path().join("race_hum.glb"),
        "gender 1 with no _f variant must fall back to the male GLB");
}

// ── eqoxide#814: the cap's VALUE has a floor, and it is not silent ──────────────────────────────

/// `JOINT_CAP` must clear the widest rig that actually ships, and every measured shipped rig must
/// classify as skinned.
///
/// **Why this test exists.** Before eqoxide#798 the cap was spelled independently in WGSL and in
/// Rust, so a mistyped cap produced a wgpu binding-size error. Unification removed that accident:
/// the tree is now self-consistent for *any* `JOINT_CAP`, including one too small for the shipped
/// rigs. Such a cap does not error — it reclassifies real rigs `ExceedsCap` and renders them
/// unskinned. Silent wrong answer.
///
/// **Where the numbers came from — measured, not reasoned.** Scanned on 2026-07-31: the GLB JSON
/// chunk of every `.glb` in the local model cache that `build_character_model` can be handed
/// (`race_*` + the named archetypes; zone terrain and `*_doors.glb` excluded). 136 `.glb` files
/// present, 49 character-relevant ones carry a skin. The distribution's top and bottom:
/// `race_pcfroglok.glb` 127 (the max), `race_huf.glb`/`humanoid_f.glb` 110, six `race_*f.glb` +
/// `elf.glb`/`elf_f.glb` 109, `race_kem.glb` 108, … down to `fish.glb` 8. That is the same scan
/// `no_shipped_local_model_exceeds_the_cap_today` re-runs against the live cache; it is `#[ignore]`d
/// because the cache is a dev-machine artifact, which is precisely why the measured figures are
/// pinned here as constants that run in a plain `cargo test`.
///
/// **What this does and does not prove.** It proves the cap clears the corpus measured on that
/// date. It does NOT re-measure the cache (that is the ignored test's job) and it cannot know about
/// a rig baked after that date — a wider rig arriving later is caught by the ignored scan, not by
/// this one. The primary enforcement is not this test at all: `renderer.rs` carries
/// `const _: () = assert!(JOINT_CAP >= MAX_MEASURED_CHARACTER_RIG_JOINTS)`, so a too-small cap is a
/// **compile error** and this test never gets the chance to run. This test exists to state the
/// provenance and to check the consequence the const cannot express — that `SkinFit::classify`
/// actually calls each measured rig skinned.
#[test]
fn joint_cap_clears_the_widest_shipped_rig() {
    /// Every distinct joint count at the top of the measured distribution, plus the minimum.
    const MEASURED_SHIPPED_RIG_JOINTS: &[usize] = &[127, 110, 109, 108, 106, 105, 104, 8];

    assert_eq!(MAX_MEASURED_CHARACTER_RIG_JOINTS, 127,
        "MAX_MEASURED_CHARACTER_RIG_JOINTS moved off the measured 127 (race_pcfroglok.glb) without \
         this test's provenance paragraph moving with it");
    assert_eq!(
        MEASURED_SHIPPED_RIG_JOINTS.iter().copied().max(), Some(MAX_MEASURED_CHARACTER_RIG_JOINTS),
        "the constant must be the maximum of the measured corpus, not an unrelated number"
    );
    const { assert!(JOINT_CAP >= MAX_MEASURED_CHARACTER_RIG_JOINTS,
        "JOINT_CAP is below the widest shipped rig (race_pcfroglok.glb). A cap this small does not \
         error — it silently renders real rigs unskinned."); }
    for &n in MEASURED_SHIPPED_RIG_JOINTS {
        assert!(SkinFit::classify(Some(n)).is_skinned(),
            "a shipped rig with {n} joints is classified {:?} — it would render unskinned",
            SkinFit::classify(Some(n)));
    }
}

// ── Measured, not inferred: does any shipped model actually exceed the cap today? ───────────────

/// Not a claim from reading the code — this is a **measured** re-scan of the local model cache
/// this crate's own doc comment (`models.rs`, the `static_placement` block) cites: parses the GLB
/// container of every `.glb` this crate can load a character model from (skipping zone terrain,
/// which never goes through `build_character_model` at all) and asserts the two numbers the #780
/// issue is about. Runs only when that cache is actually present (a dev-machine artifact, not
/// something CI or a fresh checkout has), so it's `#[ignore]`d rather than a hard dependency.
#[test]
#[ignore = "requires the local eqoxide model cache (~/.local/share/eqoxide/assets/models); not present in CI"]
fn no_shipped_local_model_exceeds_the_cap_today() {
    let dir = dirs_models_path();
    let mut max_joints = 0usize;
    let mut max_name = String::new();
    let mut over_cap: Vec<(String, usize)> = Vec::new();

    for entry in std::fs::read_dir(&dir).expect("model cache dir must exist for this test") {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("glb") { continue; }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // Zone terrain / doors never go through build_character_model's skin selection.
        if name.ends_with("_doors.glb") || is_zone_terrain(&name) { continue; }
        if let Some(n) = glb_max_skin_joint_count(&path) {
            if n > max_joints { max_joints = n; max_name = name.clone(); }
            if n > JOINT_CAP { over_cap.push((name, n)); }
        }
    }

    assert!(over_cap.is_empty(),
        "models over the {JOINT_CAP}-joint cap TODAY (this would make #780 a live bug, not a \
         latent one): {over_cap:?}");
    assert_eq!(max_name, "race_pcfroglok.glb",
        "expected race_pcfroglok.glb to remain the highest-joint model (was {max_joints} on \
         {max_name}) — if this changed, the #780 margin has moved and models.rs's doc comment \
         needs updating too");
    assert_eq!(max_joints, 127, "race_pcfroglok.glb's joint count moved off the measured 127");
}

fn dirs_models_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    std::path::PathBuf::from(home).join(".local/share/eqoxide/assets/models")
}

fn is_zone_terrain(name: &str) -> bool {
    // A conservative allowlist of the non-zone character-relevant files, rather than an
    // exhaustive zone-name blocklist that would go stale as zones are added: anything NOT in this
    // set is treated as zone terrain and skipped. Mirrors the 51-file scan in models.rs's doc
    // comment (50 character files + weapons.glb).
    const CHAR_RELEVANT: &[&str] = &[
        "bat.glb", "bear.glb", "bird.glb", "boat.glb", "creature.glb", "dwarf_f.glb", "dwarf.glb",
        "elf_f.glb", "elf.glb", "fish.glb", "frog.glb", "gnoll.glb", "humanoid_f.glb",
        "humanoid.glb", "rat.glb", "skeleton.glb", "snake.glb", "wasp.glb", "weapons.glb",
        "wolf.glb", "worm.glb", "zombie.glb",
    ];
    if CHAR_RELEVANT.contains(&name) || name.starts_with("race_") { return false; }
    true
}

/// Parse a GLB's JSON chunk directly (12-byte header + length-prefixed chunk, no external crate)
/// and return the largest `skins[].joints.len()`, or `None` if the file has no skin.
fn glb_max_skin_joint_count(path: &std::path::Path) -> Option<usize> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 12];
    f.read_exact(&mut header).ok()?;
    if &header[0..4] != b"glTF" { return None; }
    let mut chunk_header = [0u8; 8];
    f.read_exact(&mut chunk_header).ok()?;
    let chunk_len = u32::from_le_bytes(chunk_header[0..4].try_into().unwrap()) as usize;
    let chunk_type = &chunk_header[4..8];
    if chunk_type != b"JSON" { return None; }
    let mut json_bytes = vec![0u8; chunk_len];
    f.read_exact(&mut json_bytes).ok()?;
    let doc: serde_json::Value = serde_json::from_slice(&json_bytes).ok()?;
    let skins = doc.get("skins")?.as_array()?;
    skins.iter()
        .filter_map(|s| s.get("joints")?.as_array().map(|j| j.len()))
        .max()
}
