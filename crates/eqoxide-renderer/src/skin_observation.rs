//! The one place a model's skin is classified AND its cap downgrade reported (eqoxide#780).
//!
//! ## What eqoxide#795 and eqoxide#820 already did
//!
//! `renderer::SkinFit` names the three-way split (`NoSkin` / `EmptySkin` / `ExceedsCap` / `Fits`)
//! that a single `bool` used to fold flat, and `renderer::record_skin_cap_downgrade` records only
//! the genuine downgrade. Both are pure and both are covered device-free by
//! `tests/skin_cap_selection.rs`.
//!
//! ## The hole this module closes
//!
//! *Reaching* that machinery was a separate matter from *having* it. The wiring lived in two
//! different device-dependent methods — `build_character_model` classified and logged,
//! `ensure_character_model` recorded — and nothing in the test suite could execute either, because
//! `EqRenderer` needs a `wgpu::Device` no test in this crate can construct. eqoxide#797 records the
//! measurement: the eqoxide#795 reviewer wrapped both the `error!` and the
//! `record_skin_cap_downgrade` call in `if false {}` and `-p eqoxide-renderer` stayed at 238 passed
//! / 0 failed / 12 ignored, identical to the clean baseline. The classification was *written*, not
//! *reached* — the defect class tracked as eqoxide#799.
//!
//! Splitting the two channels across two functions also let them disagree: a caller of
//! `build_character_model` alone got the log line with no entry in the report.
//!
//! ## The shape
//!
//! [`observe_skin_fit`] does the whole thing — classify, log the downgrade, record the downgrade —
//! and is the **only** way to obtain an [`ObservedSkinFit`]. `ObservedSkinFit`'s field is private to
//! this module, so no code elsewhere in the crate can build one, and
//! `EqRenderer::build_character_model` demands one to pick a render arm at all. It is deliberately
//! not `Copy` or `Clone`: one observation licenses exactly one arm choice, and a second use of the
//! same witness is `E0382`.
//!
//! The destination is not a caller-chosen map either. [`DowngradeSink`] borrows
//! `EqRenderer::skin_cap_downgrades` and, outside `cfg(test)`, [`DowngradeSink::of`] — which takes
//! the renderer itself — is its only constructor. That closes the route a reviewer measured on the
//! first version of this module: passing a throwaway `BTreeMap` compiled clean and left
//! `skin_cap_downgrades` permanently empty, so every downgrade was "reported" into a map nobody
//! reads. See the "Does not" list below for what remains.
//!
//! What that does and does not buy, stated precisely:
//!
//! - **Does:** selecting a render arm without having reported the downgrade is not expressible by
//!   omission. Deleting the observation from `ensure_character_model` does not produce a silent
//!   regression; it produces a compile error, because there is then no `ObservedSkinFit` to pass.
//!   Measured, not reasoned — see the PR's mutation table.
//! - **Does:** in a non-`cfg(test)` build, the report cannot be aimed at a map other than the
//!   renderer's own field, because no other `DowngradeSink` can be constructed. Also measured: the
//!   throwaway-map substitution above is now `E0308`.
//! - **Does not:** stop a future edit from *adding* a constructor here, or from calling
//!   `SkinFit::classify` directly and routing around this module. Nothing in Rust prevents a
//!   deliberate edit to the module that holds the invariant. The claim is about omission, not
//!   about intent.
//! - **Does not:** extend past `EqRenderer`. `src/bin/render_model.rs` — the offline model viewer —
//!   calls `SkinFit::classify` itself and has no `skin_cap_downgrades` to report into; it prints
//!   the downgrade instead. That is a present fact about the workspace, not a hypothetical future
//!   edit.
//! - **Does not:** grade the log line. No test here asserts on log text (the driving agent does not
//!   read logs, and the project's guidance is to grade structured observables). What keeps the log
//!   honest is structural rather than tested: the message interpolates the joint count bound by the
//!   *same* `if let` that records the entry, so there is one gate, not two that agree. Forcing that
//!   gate open does not compile.

use std::collections::BTreeMap;
use std::path::Path;

use crate::renderer::{record_skin_cap_downgrade, EqRenderer, SkinFit, JOINT_CAP};

/// Where a cap downgrade gets reported: a mutable borrow of `EqRenderer::skin_cap_downgrades`.
///
/// The field is private and [`DowngradeSink::of`] is the only constructor a non-test build has, so
/// an [`observe_skin_fit`] call cannot be pointed at some other map. That matters because the
/// witness [`ObservedSkinFit`] would otherwise prove only that the observation *ran* — a caller
/// could hand over a scratch `BTreeMap`, take a perfectly valid witness, pick a render arm, and
/// leave the renderer's report empty. That was measured on the first version of this module, and it
/// is exactly the eqoxide#780 failure mode reached by a compile-clean edit.
pub struct DowngradeSink<'a> {
    entries: &'a mut BTreeMap<String, usize>,
}

impl<'a> DowngradeSink<'a> {
    /// Borrow the renderer's own report. The only way to build a sink outside `cfg(test)`.
    pub fn of(renderer: &'a mut EqRenderer) -> DowngradeSink<'a> {
        DowngradeSink { entries: &mut renderer.skin_cap_downgrades }
    }

    /// A sink over a caller-owned map, for this module's own unit tests — which is why the
    /// device-free coverage of [`observe_skin_fit`] lives in `mod tests` below rather than in
    /// `tests/skin_cap_selection.rs`. An integration test is a separate crate and links the library
    /// compiled *without* `cfg(test)`, so exposing this to one would reopen the hole it exists to
    /// close.
    #[cfg(test)]
    fn detached(entries: &'a mut BTreeMap<String, usize>) -> DowngradeSink<'a> {
        DowngradeSink { entries }
    }
}

/// A [`SkinFit`] that has been through [`observe_skin_fit`] — i.e. one whose downgrade, if any, has
/// already been logged and recorded into the renderer's own report.
///
/// The inner fields are private to this module and there is no public constructor, so this type is a
/// witness rather than a wrapper: holding one is evidence that the observation ran for the model it
/// describes. It is not `Copy` and not `Clone`, so it cannot be spent twice —
/// `EqRenderer::build_character_model` takes it by value, and a second hand-off of the same witness
/// is a borrow-check error rather than a second arm chosen on one observation.
#[derive(Debug, PartialEq, Eq)]
pub struct ObservedSkinFit {
    fit: SkinFit,
    /// The value of the single gate that drove both channels: `Some(joint_count)` exactly when the
    /// `error!` fired and an entry was recorded. Carried out so a test can evaluate the production
    /// gate itself rather than an extensionally-equal restatement of it.
    reported: Option<usize>,
}

impl ObservedSkinFit {
    /// The classification itself, for the caller that has to choose a render arm.
    pub fn fit(&self) -> SkinFit {
        self.fit
    }

    /// Convenience for the one question the render path asks. Delegates to [`SkinFit::is_skinned`]
    /// rather than restating it, so the arm choice cannot drift from the classification.
    pub fn is_skinned(&self) -> bool {
        self.fit.is_skinned()
    }

    /// What was reported, straight from the gate that reported it: `Some(joint_count)` iff this
    /// model was logged and recorded as a cap downgrade.
    pub fn reported_downgrade(&self) -> Option<usize> {
        self.reported
    }
}

/// Classify one loaded model's skin and, if it is a genuine joint-cap downgrade, report it on both
/// channels: the `error!` log and the renderer's `skin_cap_downgrades` map behind `sink`.
///
/// `joint_count` is `None` when the model has no skin at all. `model_path` is the GLB that was
/// actually loaded — the report is keyed by its file name, not by `label`, because the joint count
/// belongs to the file (eqoxide#813); `label` appears only in the log line.
///
/// Nothing is recorded for `NoSkin`, `EmptySkin`, or `Fits`. That asymmetry is the entire point of
/// eqoxide#780: an unskinned `boat.glb` and a 129-joint rig both take the static arm, and only the
/// second is a downgrade.
///
/// Pure apart from the log line and the sink it is handed, so the wiring this function *is* can be
/// driven directly from a test with no `wgpu::Device` — see `mod tests` below.
pub fn observe_skin_fit(
    sink: DowngradeSink<'_>,
    model_path: &Path,
    label: &str,
    joint_count: Option<usize>,
) -> ObservedSkinFit {
    let fit = SkinFit::classify(joint_count);
    let mut reported = None;
    if let Some(reason) = fit.static_reason() {
        // ONE gate for both channels. The log message interpolates `over_cap_joints`, so this
        // cannot be forced open (`if true`) without failing to compile — which is the only thing
        // holding the log honest, since nothing grades log text. `record_skin_cap_downgrade` tests
        // the same predicate again internally; that is redundancy, not a second decision.
        if let Some(over_cap_joints) = reason.downgrade_joint_count() {
            tracing::error!(
                "renderer: character model '{label}' ({}) has a skin of {over_cap_joints} joints, \
                 EXCEEDING the {JOINT_CAP}-joint cap — falling back to the STATIC (unskinned) \
                 render arm; this model will not animate (eqoxide#780)",
                crate::renderer::downgrade_key(model_path)
            );
            record_skin_cap_downgrade(sink.entries, model_path, reason);
            reported = Some(over_cap_joints);
        }
    }
    ObservedSkinFit { fit, reported }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::MAX_MEASURED_CHARACTER_RIG_JOINTS;

    /// Verbatim transcription of the pre-eqoxide#780 boolean (`renderer.rs`,
    /// `asset.skin.as_ref().is_some_and(|s| s.joint_count > 0 && s.joint_count <= 128)`), with
    /// `Option<SkinData>` reduced to the one field it read. `tests/skin_cap_selection.rs` holds the
    /// same transcription for `SkinFit::classify`; both pin the same deleted line.
    fn old_use_skinned(joint_count: Option<usize>) -> bool {
        joint_count.is_some_and(|n| n > 0 && n <= 128)
    }

    /// eqoxide#780's acceptance bar, applied to the function the renderer really calls rather than
    /// to the classifier alone: a synthetic model with `joint_count = 129` takes the static arm
    /// **and** is reported as a downgrade.
    ///
    /// 129 is the issue's own number, written literally rather than as `JOINT_CAP + 1`, so this
    /// keeps asserting the case the issue names even if the cap moves.
    #[test]
    fn observing_a_cap_exceeding_skin_takes_the_static_arm_and_reports_it() {
        let mut map: BTreeMap<String, usize> = BTreeMap::new();
        let observed = observe_skin_fit(
            DowngradeSink::detached(&mut map),
            Path::new("/models/race_over.glb"),
            "race_over",
            Some(129),
        );

        assert!(!observed.is_skinned(), "a 129-joint skin must take the STATIC arm");
        assert_eq!(observed.fit(), SkinFit::ExceedsCap { joint_count: 129 });
        assert_eq!(observed.reported_downgrade(), Some(129));
        assert_eq!(map.get("race_over.glb"), Some(&129),
            "a 129-joint skin must be REPORTED as a downgrade, keyed by the loaded file");
        assert_eq!(map.len(), 1);
    }

    /// The other half of the acceptance bar: the two static arms that are NOT downgrades take the
    /// same arm and produce no report. Before eqoxide#780 all three of these were one `bool` and
    /// nothing distinguished them; the point is that the arm is the same and the report is not.
    #[test]
    fn observing_an_unremarkable_static_model_takes_the_static_arm_and_reports_nothing() {
        let mut map: BTreeMap<String, usize> = BTreeMap::new();

        let empty = observe_skin_fit(
            DowngradeSink::detached(&mut map),
            Path::new("/models/race_empty.glb"),
            "race_empty",
            Some(0),
        );
        assert!(!empty.is_skinned());
        assert_eq!(empty.fit(), SkinFit::EmptySkin);
        assert_eq!(empty.reported_downgrade(), None);
        assert!(map.is_empty(),
            "a zero-joint skin is degenerate data, not a cap downgrade — it must not be reported");

        let none = observe_skin_fit(
            DowngradeSink::detached(&mut map), Path::new("/models/boat.glb"), "boat", None,
        );
        assert!(!none.is_skinned());
        assert_eq!(none.fit(), SkinFit::NoSkin);
        assert_eq!(none.reported_downgrade(), None);
        assert!(map.is_empty(),
            "an unskinned model (e.g. boat.glb) must not be reported as a downgrade");
    }

    /// A fitting skin takes the skinned arm and is not reported. Stated separately because a report
    /// that fired on every load would also be a falsehood, just a noisier one.
    #[test]
    fn observing_a_fitting_skin_takes_the_skinned_arm_and_reports_nothing() {
        let mut map: BTreeMap<String, usize> = BTreeMap::new();
        let observed = observe_skin_fit(
            DowngradeSink::detached(&mut map),
            Path::new("/models/race_pcfroglok.glb"),
            "race_pcfroglok",
            Some(MAX_MEASURED_CHARACTER_RIG_JOINTS),
        );

        assert!(observed.is_skinned(),
            "the widest rig that ships is under the cap and must render SKINNED");
        assert_eq!(observed.fit(), SkinFit::Fits { joint_count: MAX_MEASURED_CHARACTER_RIG_JOINTS });
        assert_eq!(observed.reported_downgrade(), None);
        assert!(map.is_empty());
    }

    /// The log channel and the report channel are one gate evaluated once, not two expressions that
    /// happen to agree.
    ///
    /// `reported_downgrade()` is the value of the `if let` that guards *both* the `error!` and the
    /// `record_skin_cap_downgrade` call, carried out of the function — so asserting on it evaluates
    /// the gate the log really uses, which is what an earlier version of this suite claimed to do
    /// and did not (it pinned `StaticReason::is_downgrade`, which no production code calls).
    /// Asserting it against the map as well pins that one evaluation to one recorded entry.
    #[test]
    fn the_log_and_the_report_come_from_one_gate_evaluation() {
        for joint_count in [None, Some(0), Some(1), Some(JOINT_CAP), Some(JOINT_CAP + 1)] {
            let mut map: BTreeMap<String, usize> = BTreeMap::new();
            let observed = observe_skin_fit(
                DowngradeSink::detached(&mut map), Path::new("/models/x.glb"), "x", joint_count,
            );

            assert_eq!(observed.reported_downgrade(), map.get("x.glb").copied(),
                "{joint_count:?}: the gate that fired the log must be the gate that wrote the \
                 report — one evaluation, one entry");
        }
    }

    /// The universal the examples above are instances of: over every joint count from "no skin"
    /// through well past the cap, a model is reported as a downgrade **iff** it has a skin that
    /// exceeds the cap, and it takes the skinned arm **iff** it is not reported and has a non-empty
    /// skin.
    ///
    /// Both sides are also checked against `old_use_skinned`, the verbatim pre-eqoxide#780 boolean,
    /// so this simultaneously re-pins "eqoxide#780 is not a placement change" across the whole range
    /// for the function the renderer calls — not just for `SkinFit::classify`.
    #[test]
    fn observation_and_arm_agree_over_the_whole_range() {
        for n in 0..=(JOINT_CAP + 8) {
            let mut map: BTreeMap<String, usize> = BTreeMap::new();
            let observed = observe_skin_fit(
                DowngradeSink::detached(&mut map), Path::new("/models/x.glb"), "x", Some(n),
            );

            let reported = map.get("x.glb").copied();
            assert_eq!(reported, (n > JOINT_CAP).then_some(n),
                "joint_count {n}: reported-as-downgrade must hold exactly when the cap is exceeded");
            assert_eq!(observed.reported_downgrade(), reported,
                "joint_count {n}: the gate's value must match what landed in the report");
            assert_eq!(observed.is_skinned(), old_use_skinned(Some(n)),
                "joint_count {n}: the render arm must match the pre-#780 boolean");
            assert!(!(observed.is_skinned() && reported.is_some()),
                "joint_count {n}: a model cannot both render skinned and be a cap downgrade");
        }

        // The `None` (no skin at all) case, which the numeric loop cannot express.
        let mut map: BTreeMap<String, usize> = BTreeMap::new();
        let observed = observe_skin_fit(
            DowngradeSink::detached(&mut map), Path::new("/models/boat.glb"), "boat", None,
        );
        assert!(map.is_empty(), "no skin at all is not a downgrade");
        assert_eq!(observed.reported_downgrade(), None);
        assert_eq!(observed.is_skinned(), old_use_skinned(None));
    }
}
