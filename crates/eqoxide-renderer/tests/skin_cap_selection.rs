//! Device-free coverage for `SkinFit` — the renderer's skinned-vs-static model selection
//! (eqoxide#780).
//!
//! ## The bug
//!
//! `build_character_model` (`renderer.rs`) decides whether a loaded model uses the skinned
//! (GPU-animated) render arm or the static one. Before #780 that decision was one `bool`:
//!
//! ```text
//! let use_skinned = asset.skin.as_ref().is_some_and(|s| s.joint_count > 0 && s.joint_count <= 128);
//! ```
//!
//! `!use_skinned` folds three different situations into the same silent static fallback: no skin
//! at all (unremarkable — e.g. `boat.glb`), a skin with zero joints (degenerate data), and a skin
//! with MORE than the 128-joint uniform-buffer cap. Only the third is a genuine downgrade — the
//! model DOES have real joint data and quietly renders as if it had none — and the boolean could
//! not tell it apart from the other two, so nothing did. Measured live cache scan (below,
//! `no_shipped_local_model_exceeds_the_cap_today`): the closest any shipped model gets is
//! `race_pcfroglok.glb` at 127 of 128, so this is currently a LATENT path, not a live regression —
//! which is exactly why the margin has to be named rather than merely unreached.
//!
//! ## The fix
//!
//! `renderer::SkinFit::classify` names the three-way split explicitly (`NoSkin` / `EmptySkin` /
//! `ExceedsCap` / `Fits`) so a call site can no longer discard the distinction on the way into a
//! `bool`. `StaticReason` (the subset that actually reaches the static arm) exposes
//! `is_downgrade()` — true only for `ExceedsCap` — and `build_character_model` logs at `error!`
//! and records into `EqRenderer::skin_cap_downgrades` exactly when that's true.
//!
//! ## What this file does NOT cover
//!
//! - That `build_character_model` actually calls `SkinFit::classify` and wires its result to
//!   `error!`/`skin_cap_downgrades` — that needs a `wgpu::Device`, which nothing in this crate's
//!   test harness can construct (see `shadow_routing.rs`'s header for the established precedent).
//!   What's covered here is the decision itself: `SkinFit::classify`, `SkinFit::is_skinned`,
//!   `SkinFit::static_reason`, `StaticReason::is_downgrade`, and `record_skin_cap_downgrade` are
//!   all pure and reached directly.
//! - That the `error!` log line is emitted with any particular text — this project's own
//!   `AGENT-HONESTY` guidance is that a log line is a weak signal anyway (the driving agent does
//!   not read logs), so this file grades the *structured* observable
//!   (`record_skin_cap_downgrade` / `skin_cap_downgrades`), not the log text.

use eqoxide_renderer::renderer::{record_skin_cap_downgrade, SkinFit, StaticReason, JOINT_CAP};
use std::collections::BTreeMap;

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
    assert!(over.static_reason().unwrap().is_downgrade(),
        "a skin with 129 joints (over the 128 cap) must be reported as a downgrade");

    assert_eq!(empty.static_reason(), Some(StaticReason::EmptySkin));
    assert!(!empty.static_reason().unwrap().is_downgrade(),
        "a skin with 0 joints is degenerate data, not a cap downgrade — must not be reported as one");

    assert_eq!(none.static_reason(), Some(StaticReason::NoSkin));
    assert!(!none.static_reason().unwrap().is_downgrade(),
        "an unskinned model (e.g. boat.glb) must not be reported as a downgrade");
}

/// A `Fits` skin fit has no `StaticReason` at all — it never reaches the static arm, so asking
/// "why is it static" is a type error waiting to happen if this ever returned `Some`.
#[test]
fn a_fitting_skin_has_no_static_reason() {
    assert_eq!(SkinFit::classify(Some(1)).static_reason(), None);
    assert_eq!(SkinFit::classify(Some(JOINT_CAP)).static_reason(), None);
}

// ── The recording side-effect (what `EqRenderer::skin_cap_downgrades` is built from) ───────────

#[test]
fn record_skin_cap_downgrade_only_inserts_for_exceeds_cap() {
    let mut map: BTreeMap<String, usize> = BTreeMap::new();

    record_skin_cap_downgrade(&mut map, "race_ok", StaticReason::NoSkin);
    assert!(map.is_empty(), "NoSkin must not be recorded as a downgrade");

    record_skin_cap_downgrade(&mut map, "race_empty", StaticReason::EmptySkin);
    assert!(map.is_empty(), "EmptySkin must not be recorded as a downgrade");

    record_skin_cap_downgrade(&mut map, "race_pcfroglok", StaticReason::ExceedsCap { joint_count: 129 });
    assert_eq!(map.get("race_pcfroglok"), Some(&129),
        "an ExceedsCap reason must be recorded under the model's own label with its joint count");
    assert_eq!(map.len(), 1);
}

/// A later re-classification of the same model updates its recorded joint count rather than
/// leaving a stale one behind (relevant if a model is ever reloaded after a rebake).
#[test]
fn re_recording_the_same_label_updates_the_joint_count() {
    let mut map: BTreeMap<String, usize> = BTreeMap::new();
    record_skin_cap_downgrade(&mut map, "race_x", StaticReason::ExceedsCap { joint_count: 130 });
    record_skin_cap_downgrade(&mut map, "race_x", StaticReason::ExceedsCap { joint_count: 200 });
    assert_eq!(map.get("race_x"), Some(&200));
    assert_eq!(map.len(), 1);
}

/// Recording a non-downgrade for a label that already had a recorded downgrade must not erase it —
/// `record_skin_cap_downgrade` only ever inserts, never removes, so a caller that (incorrectly)
/// re-derives `NoSkin`/`EmptySkin` for an already-downgraded label can't accidentally un-report it.
#[test]
fn a_non_downgrade_call_never_removes_an_existing_entry() {
    let mut map: BTreeMap<String, usize> = BTreeMap::new();
    record_skin_cap_downgrade(&mut map, "race_x", StaticReason::ExceedsCap { joint_count: 130 });
    record_skin_cap_downgrade(&mut map, "race_x", StaticReason::NoSkin);
    assert_eq!(map.get("race_x"), Some(&130), "a non-downgrade call must not clear a prior record");
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
