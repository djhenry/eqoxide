//! Device-free coverage for `SkinFit` — the renderer's skinned-vs-static model selection
//! (eqoxide#780).
//!
//! ## The bug
//!
//! Whether a loaded model uses the skinned (GPU-animated) render arm or the static one is decided
//! by `skin_observation::observe_skin_fit`, which classifies the skin; `build_character_model`
//! applies that decision and builds the GPU model. Before #780 the decision was one `bool`, made
//! inside `build_character_model` itself:
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
//! `is_downgrade()`, true only for `ExceedsCap`. `skin_observation::observe_skin_fit` logs at
//! `error!` and records into `EqRenderer::skin_cap_downgrades` under one `if let` on
//! `StaticReason::downgrade_joint_count` — the actual gate; `is_downgrade` is a convenience with no
//! production caller, and the test below that relates the two says only that, having once said
//! more. (Both channels lived in `build_character_model` / `ensure_character_model` until
//! eqoxide#780 moved them into one function a test can call.)
//!
//! ## What this file does NOT cover
//!
//! - That `EqRenderer::ensure_character_model` and `EqRenderer::build_character_model` behave as
//!   described — both need a `wgpu::Device`, which nothing in this crate's test harness can
//!   construct (see `shadow_routing.rs`'s header for the established precedent). **The wiring
//!   between them is no longer in that blind spot**, though, and that is the change eqoxide#797
//!   asked for: the classify → log → record sequence is now one device-free function,
//!   `skin_observation::observe_skin_fit`, exercised directly by that module's own `mod tests`
//!   (they cannot live here — see the WIRING note further down). What remains uncovered is only the
//!   two device-dependent methods' own bodies — and a render arm cannot be chosen in either without
//!   an `ObservedModel`, which only `observe_skin_fit` produces.
//! - That the `error!` log line is emitted with any particular text — this project's own
//!   `AGENT-HONESTY` guidance is that a log line is a weak signal anyway (the driving agent does
//!   not read logs), so this file grades the *structured* observable
//!   (`record_skin_cap_downgrade` / `skin_cap_downgrades`), not the log text.
//!
//! ## Later additions
//!
//! - **eqoxide#813** — the downgrade report is keyed by the **loaded GLB file**, not by
//!   `(label, gender)`. Those tests drive the real `renderer::resolve_character_model_path` (the
//!   function `ensure_character_model` calls) against a scratch asset dir, so the `(label, gender)`
//!   → file relationship they depend on is exercised rather than restated.
//! - **eqoxide#814** — a floor on `JOINT_CAP`'s VALUE, with the measured corpus figures it comes
//!   from. See `joint_cap_clears_the_widest_shipped_rig`; the primary enforcement is the `const`
//!   assertion in `renderer.rs`, which makes a too-small cap a compile error.
//! - **eqoxide#797** — the report is now served over HTTP (`/v1/observe/debug`'s
//!   `skin_cap_downgrades`, documented in `docs/http-api.md`); that plumbing lives above this crate
//!   (`src/app.rs`, `crates/eqoxide-http`) and is not covered here, which stays true to the file's
//!   own "what this file does NOT cover" note above.
//! - **eqoxide#848** — `record_skin_cap_downgrade`'s map value grew from a bare `usize` to
//!   [`SkinCapDowngrade`], which adds a `key_collision` flag: two DIFFERENT source files that share
//!   a base-name key no longer silently overwrite one another (`skin_observation::observe_skin_fit`
//!   also stopped taking a caller-supplied `model_path`/`label` at all — see that function's own
//!   doc). `two_roots_with_the_same_basename_collide_into_one_entry` used to accept the silent
//!   overwrite as expected behavior; it now asserts the flag instead, and
//!   `key_collision_is_sticky_and_does_not_clear_on_a_later_agreeing_write` pins that the flag
//!   cannot be cleared once set.

use eqoxide_renderer::renderer::{
    downgrade_key, record_skin_cap_downgrade, resolve_character_model_path, SkinCapDowngrade,
    SkinFit, StaticReason, JOINT_CAP, MAX_MEASURED_CHARACTER_RIG_JOINTS,
};
use std::collections::BTreeMap;
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

/// A scratch asset dir containing the given GLB file names (empty files — only their *existence*
/// matters to `resolve_character_model_path`). Removed on drop.
///
/// The downgrade-key tests below run the **real** `renderer::resolve_character_model_path`, the
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

#[test]
fn record_skin_cap_downgrade_only_inserts_for_exceeds_cap() {
    let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
    let dir = Path::new("/models");

    record_skin_cap_downgrade(&mut map, &dir.join("race_ok.glb"), StaticReason::NoSkin);
    assert!(map.is_empty(), "NoSkin must not be recorded as a downgrade");

    record_skin_cap_downgrade(&mut map, &dir.join("race_empty.glb"), StaticReason::EmptySkin);
    assert!(map.is_empty(), "EmptySkin must not be recorded as a downgrade");

    record_skin_cap_downgrade(&mut map, &dir.join("race_pcfroglok.glb"),
        StaticReason::ExceedsCap { joint_count: 129 });
    assert_eq!(map.get("race_pcfroglok.glb").map(|e| e.joint_count), Some(129),
        "an ExceedsCap reason must be recorded under the loaded FILE with its joint count");
    assert_eq!(map.get("race_pcfroglok.glb").map(|e| e.key_collision), Some(false),
        "a single file recorded once is not a collision");
    assert_eq!(map.len(), 1);
}

/// `StaticReason::is_downgrade` agrees with the predicate that actually gates the report.
///
/// Scope, stated because an earlier version of this test overstated it: `is_downgrade` has **no
/// production caller**. It does not gate the log — `skin_observation::observe_skin_fit` gates on
/// `downgrade_joint_count()` directly, and `the_log_and_the_report_come_from_one_gate_evaluation`
/// (a unit test in that module) is what pins the real gate. This test only keeps the public
/// convenience method from diverging from the method it delegates to, which is worth having because
/// `is_downgrade` is `pub` and a reader will believe it.
///
/// The delegation is not decorative. On an intermediate commit of this branch the log tested
/// `is_downgrade` while `record_skin_cap_downgrade` re-matched `ExceedsCap` itself, and mutating
/// `is_downgrade` to `true` changed which models got logged without changing which got reported.
#[test]
fn is_downgrade_agrees_with_the_report_gate() {
    for reason in [
        StaticReason::NoSkin,
        StaticReason::EmptySkin,
        StaticReason::ExceedsCap { joint_count: 129 },
    ] {
        let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
        record_skin_cap_downgrade(&mut map, Path::new("/models/x.glb"), reason);

        assert_eq!(map.contains_key("x.glb"), reason.is_downgrade(),
            "{reason:?}: `is_downgrade` disagrees with whether a report entry was written");
        assert_eq!(map.get("x.glb").map(|e| e.joint_count), reason.downgrade_joint_count(),
            "{reason:?}: the recorded count must be exactly what the shared predicate yields");
    }
}

// ── The WIRING: classify -> log -> record, as one reachable function (eqoxide#780 / eqoxide#797) ──
//
// Those tests are NOT here. They are unit tests in `src/skin_observation.rs`, because the sink that
// names the report's destination has no constructor outside `cfg(test)` — an integration test is a
// separate crate and links the library built without it. Moving them was the point: giving this
// file a way to build a sink would have handed the same escape hatch to the renderer.
//
// What lives there: `observing_a_cap_exceeding_skin_takes_the_static_arm_and_reports_it`,
// `observing_an_unremarkable_static_model_takes_the_static_arm_and_reports_nothing`,
// `observing_a_fitting_skin_takes_the_skinned_arm_and_reports_nothing`,
// `the_log_and_the_report_come_from_one_gate_evaluation`, and
// `observation_and_arm_agree_over_the_whole_range`.

/// The documented limit of the file-name key: two files with the same base name under two different
/// asset roots still share ONE map entry — `downgrade_key` is a pure function of the base name, so
/// a `BTreeMap` cannot hold two. eqoxide#848's fix is not to make that collision impossible (it
/// isn't, without keying on something no two real files can share) but to make it **stop being
/// silent**: before this test existed, `record_skin_cap_downgrade`'s doc claimed the collision could
/// not happen "because the key IS the file", a reviewer measured that it does, and the entry simply
/// silently reported whichever load happened last with no sign anything was wrong.
///
/// Two rigs keyed alike (`race_a/race_hum.glb` vs `race_b/race_hum.glb`) now leave a
/// `key_collision: true` flag an agent reading the report can act on — "the joint count you're
/// looking at under this name may not be either rig's own count" — instead of a bare number that
/// looks exactly as trustworthy as every other entry.
///
/// Not reachable in the shipped flow today (`EqRenderer` has one `assets_path`, set by
/// `load_character_models`), and the base name is still the right key for an unrelated reason: the
/// map is agent-facing over HTTP and an absolute path is machine-specific local detail this project
/// does not publish. If a second root is ever introduced, this is where the consequence is written
/// down and now detected rather than merely documented.
#[test]
fn two_roots_with_the_same_basename_collide_into_one_entry() {
    let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
    let a = Path::new("/root_a").join("race_hum.glb");
    let b = Path::new("/root_b").join("race_hum.glb");
    assert_ne!(a, b, "precondition: two genuinely different files");

    record_skin_cap_downgrade(&mut map, &a, StaticReason::ExceedsCap { joint_count: 140 });
    let before = map.get("race_hum.glb").cloned().expect("first write must record an entry");
    assert!(!before.key_collision, "one file loaded once is not yet a collision");

    record_skin_cap_downgrade(&mut map, &b, StaticReason::ExceedsCap { joint_count: 190 });

    assert_eq!(map.len(), 1, "same base name under two roots still keys the same entry");
    let after = map.get("race_hum.glb").expect("entry must still exist");
    assert_eq!(after.joint_count, 190,
        "the later write's joint count is what's stored — still a real number, just now flagged");
    assert!(after.key_collision,
        "two DIFFERENT source files sharing one key must be flagged, not silently overwritten");
}

/// The key is the loaded file name, not the directory-qualified path: two clients with different
/// asset roots must produce the same report for the same rig.
#[test]
fn the_downgrade_key_is_the_file_name() {
    assert_eq!(downgrade_key(Path::new("/a/b/race_hum_f.glb")), "race_hum_f.glb");
    assert_eq!(downgrade_key(Path::new("race_hum.glb")), "race_hum.glb");
    // Never empty: a path with no file-name component still names something.
    assert!(!downgrade_key(Path::new("/a/b/..")).is_empty());
}

/// eqoxide#798's half: two genders that resolve to two DIFFERENT files must produce two entries.
///
/// `ensure_character_model` prefers `<label>_f.glb` for gender 1, and the two files have
/// independent joint counts. Keyed by label alone (the pre-#798 key) the second load overwrote the
/// first, so a report could name one joint count while a second, differently sized rig was also
/// downgraded and went unmentioned.
#[test]
fn two_genders_that_load_two_different_files_are_reported_separately() {
    let assets = ScratchAssets::with("two-files", &["race_hum.glb", "race_hum_f.glb"]);
    let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
    let male   = resolve_character_model_path(assets.path(), "race_hum", 0);
    let female = resolve_character_model_path(assets.path(), "race_hum", 1);
    assert_ne!(male, female, "precondition: an archetype WITH an _f variant loads two files");

    record_skin_cap_downgrade(&mut map, &male,   StaticReason::ExceedsCap { joint_count: 140 });
    record_skin_cap_downgrade(&mut map, &female, StaticReason::ExceedsCap { joint_count: 190 });
    assert_eq!(map.len(), 2, "two distinct rigs must not collide into one entry");
    assert_eq!(map.get("race_hum.glb").map(|e| e.joint_count),   Some(140));
    assert_eq!(map.get("race_hum_f.glb").map(|e| e.joint_count), Some(190));
    assert_eq!(map.get("race_hum.glb").map(|e| e.key_collision),   Some(false),
        "two DIFFERENT keys is not a collision on either key");
    assert_eq!(map.get("race_hum_f.glb").map(|e| e.key_collision), Some(false));
}

/// eqoxide#813: two genders that resolve to the SAME file must produce ONE entry.
///
/// Only 3 archetypes in the shipped model cache have an `_f` variant (measured 2026-07-31:
/// `dwarf_f.glb`, `elf_f.glb`, `humanoid_f.glb`, out of 136 `.glb` files). For every other
/// archetype both genders load the identical GLB, so a `(label, gender)` key — #798's key —
/// recorded one downgraded file twice, under two keys, with the same count. That is a duplicate
/// entry in a report an agent reads: it says two rigs were downgraded when one was.
///
/// Note what makes this structural rather than a guard: `record_skin_cap_downgrade` keys on the
/// `&Path` it is given, so "same file, two entries" is not expressible — there is no second key to
/// insert under.
#[test]
fn two_genders_that_load_the_same_file_are_reported_once() {
    // No `_f` variant on disk — the shape of every archetype but 3.
    let assets = ScratchAssets::with("one-file", &["race_pcfroglok.glb"]);
    let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
    let male   = resolve_character_model_path(assets.path(), "race_pcfroglok", 0);
    let female = resolve_character_model_path(assets.path(), "race_pcfroglok", 1);
    assert_eq!(male, female, "precondition: an archetype with NO _f variant loads one file");

    record_skin_cap_downgrade(&mut map, &male,   StaticReason::ExceedsCap { joint_count: 200 });
    record_skin_cap_downgrade(&mut map, &female, StaticReason::ExceedsCap { joint_count: 200 });
    assert_eq!(map.len(), 1,
        "one GLB was downgraded once; reporting it twice would tell the agent two rigs failed");
    assert_eq!(map.get("race_pcfroglok.glb").map(|e| e.joint_count), Some(200));
    assert_eq!(map.get("race_pcfroglok.glb").map(|e| e.key_collision), Some(false),
        "the SAME file loaded twice (male/female resolving identically) is not a collision — the \
         source path agrees on every write");
}

/// The universal form of the two tests above: over every combination of labels, genders and
/// `_f`-variant presence, the number of entries equals the number of DISTINCT files loaded — never
/// more, never fewer. #798 (two files, one entry) and #813 (one file, two entries) are the two ways
/// this can break, and this is the single statement that excludes both.
///
/// Quantified over one asset root, which is what a renderer has. `distinct` counts distinct *paths*,
/// and every path here shares a root, so distinct path and distinct base name coincide. They do not
/// coincide in general — see `two_roots_with_the_same_basename_collide_into_one_entry`, which pins
/// the one case where this equality does not hold.
#[test]
fn entry_count_always_equals_distinct_file_count() {
    let labels = ["race_a", "race_b"];
    for f_variants in 0..(1usize << labels.len()) {
        let files: Vec<String> = labels
            .iter()
            .enumerate()
            .flat_map(|(i, l)| {
                let mut v = vec![format!("{l}.glb")];
                if f_variants & (1 << i) != 0 { v.push(format!("{l}_f.glb")); }
                v
            })
            .collect();
        let refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
        let assets = ScratchAssets::with(&format!("combo-{f_variants}"), &refs);

        let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
        let mut distinct = std::collections::BTreeSet::new();
        for label in labels {
            for gender in [0u8, 1u8] {
                let p = resolve_character_model_path(assets.path(), label, gender);
                distinct.insert(p.clone());
                record_skin_cap_downgrade(&mut map, &p,
                    StaticReason::ExceedsCap { joint_count: 999 });
            }
        }
        assert_eq!(map.len(), distinct.len(),
            "f_variants={f_variants:02b} files={files:?}: {} entries for {} distinct files loaded",
            map.len(), distinct.len());
    }
}

/// The resolver itself, on the two shapes that matter: with an `_f` variant present gender 1 gets
/// its own file; without one it falls back to the male GLB. Read directly rather than assumed,
/// because every downgrade-key claim above rests on it.
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

/// A later re-classification of the same file updates its recorded joint count rather than
/// leaving a stale one behind (relevant if a model is ever reloaded after a rebake).
#[test]
fn re_recording_the_same_file_updates_the_joint_count() {
    let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
    let p = Path::new("/models/race_x.glb");
    record_skin_cap_downgrade(&mut map, p, StaticReason::ExceedsCap { joint_count: 130 });
    record_skin_cap_downgrade(&mut map, p, StaticReason::ExceedsCap { joint_count: 200 });
    assert_eq!(map.get("race_x.glb").map(|e| e.joint_count), Some(200));
    assert_eq!(map.get("race_x.glb").map(|e| e.key_collision), Some(false),
        "the SAME path recorded twice is not a collision");
    assert_eq!(map.len(), 1);
}

/// Recording a non-downgrade for a file that already had a recorded downgrade must not erase it —
/// `record_skin_cap_downgrade` only ever inserts, never removes, so a caller that (incorrectly)
/// re-derives `NoSkin`/`EmptySkin` for an already-downgraded file can't accidentally un-report it.
#[test]
fn a_non_downgrade_call_never_removes_an_existing_entry() {
    let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
    let p = Path::new("/models/race_x.glb");
    record_skin_cap_downgrade(&mut map, p, StaticReason::ExceedsCap { joint_count: 130 });
    record_skin_cap_downgrade(&mut map, p, StaticReason::NoSkin);
    assert_eq!(map.get("race_x.glb").map(|e| e.joint_count), Some(130),
        "a non-downgrade call must not clear a prior record");
}

/// The collision flag is STICKY: once two distinct source files have ever shared a key, no later
/// write can clear it back to `false` — not even one that agrees with the write immediately before
/// it.
///
/// A naive implementation that recomputes the flag from scratch on every write — "does *this*
/// write's source disagree with whatever `source` is *currently* stored" — gets writes 1-3 right
/// here (a, then b disagrees with a: flag set; then a disagrees with the just-stored b: still set)
/// but goes wrong on write 4: a second `a` write agrees with write 3's `a`, so a from-scratch
/// recompute would report `false` again, even though `a` and `b` genuinely collided two writes ago.
/// That would be the flag lying about the one thing it exists to disclose. `record_skin_cap_downgrade`
/// avoids this by OR-ing into the existing flag rather than recomputing it, which write 4 below pins.
#[test]
fn key_collision_is_sticky_and_does_not_clear_on_a_later_agreeing_write() {
    let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
    let a = Path::new("/root_a").join("race_hum.glb");
    let b = Path::new("/root_b").join("race_hum.glb");

    record_skin_cap_downgrade(&mut map, &a, StaticReason::ExceedsCap { joint_count: 100 });
    assert!(!map.get("race_hum.glb").unwrap().key_collision, "one source: no collision yet");

    record_skin_cap_downgrade(&mut map, &b, StaticReason::ExceedsCap { joint_count: 101 });
    assert!(map.get("race_hum.glb").unwrap().key_collision, "two DIFFERENT sources: collision");

    // A third write, back to `a` — the same source as write 1, and it now agrees with nothing that
    // was just written (write 2 was `b`), so this alone doesn't prove stickiness.
    record_skin_cap_downgrade(&mut map, &a, StaticReason::ExceedsCap { joint_count: 102 });
    assert!(map.get("race_hum.glb").unwrap().key_collision,
        "a write disagreeing with the immediately-prior source must still read collided");

    // A fourth write, `a` again — THIS is the one that would clear a naive "current write agrees
    // with current source" implementation, because write 3 and write 4 are both `a`.
    record_skin_cap_downgrade(&mut map, &a, StaticReason::ExceedsCap { joint_count: 103 });
    assert!(map.get("race_hum.glb").unwrap().key_collision,
        "the flag must stay set forever once two distinct sources have collided, even once every \
         subsequent write agrees with the one immediately before it");
    assert_eq!(map.get("race_hum.glb").unwrap().joint_count, 103,
        "the joint count still reflects the latest write");
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
