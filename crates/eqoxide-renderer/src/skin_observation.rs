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
//! [`observe_skin_fit`] does the whole thing — read the model's joint count, classify it, log the
//! downgrade, record the downgrade — and is the **only** way to obtain an [`ObservedModel`].
//! `ObservedModel`'s fields are private to this module, so no code elsewhere in the crate can build
//! one, and `EqRenderer::build_character_model` demands one to pick a render arm at all. It is
//! deliberately not `Copy` or `Clone`: one observation licenses exactly one arm choice, and a second
//! use of the same witness is `E0382`.
//!
//! **The witness owns the asset it describes.** `observe_skin_fit` takes the
//! [`crate::models::ModelAsset`] *by value*, reads `asset.skin.as_ref().map(|s| s.joint_count)`
//! itself, and hands the asset back out only through [`ObservedModel::into_parts`].
//! `build_character_model` has no `ModelAsset` parameter at all, so the model it uploads is the same
//! *value* that was observed, not merely one that compares equal to it. That closes the route a
//! reviewer measured on the previous version of this module, where the joint count was computed at
//! the call site and passed in as an `Option<usize>`: replacing that one expression with `None`
//! compiled clean and left `-p eqoxide-renderer` byte-identical to its control, so a 129-joint rig
//! took the static arm with neither channel firing. That is eqoxide#780 itself, one argument to the
//! right of where the previous round fixed it.
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
//!   regression; it produces a compile error, because there is then no `ObservedModel` to pass.
//!   Measured, not reasoned — see the PR's mutation table.
//! - **Does:** the report cannot be aimed at a map other than the renderer's own field. Measured
//!   twice at the call site in `ensure_character_model`: handing `observe_skin_fit` a scratch
//!   `BTreeMap` — the exact round-1 substitution — is `E0308`, and reaching for
//!   `DowngradeSink::detached` instead is `E0599` in the plain lib build (the item is
//!   `cfg(test)`-gated) and `E0624` in the lib-test build (it is private to this module, so even a
//!   `cfg(test)` build of the renderer cannot call it).
//! - **Does:** the observation cannot be about a joint count the asset does not have, because the
//!   caller no longer supplies one. Measured three ways on this head (eqoxide#797 round), against
//!   the then-current control `cargo test -p eqoxide-renderer --locked --no-fail-fast` = **270
//!   passed / 0 failed / 12 ignored**, 14 `running N tests` headers vs 14 `test result:` lines. (The
//!   control has since grown to **277 / 0 / 13** — see the eqoxide#848 note below — but these three
//!   rows were not re-run against it; the mechanism they establish, that the joint count comes from
//!   the asset and not a caller-supplied `Option<usize>`, is unchanged by #848, which touched a
//!   different pair of arguments.)
//!   - **R1** — `None` in place of the `asset` argument at `ensure_character_model`'s call site,
//!     i.e. the reviewer's own mutation applied one round later: `error[E0308]: mismatched types
//!     … expected `ModelAsset`, found `Option<_>``, **twice** (once in `(lib)`, once in
//!     `(lib test)`). The same nine-token edit against the previous signature compiled clean and
//!     left the crate byte-identical to its control.
//!   - **R1b** — `crate::models::ModelAsset::default()` in its place: `error[E0599]: no associated
//!     function or constant named `default` found for struct `ModelAsset``, twice. `ModelAsset`
//!     carries no `#[derive]` and no `impl Default`, so conjuring a second asset means writing all
//!     the fields out by hand rather than calling one function.
//!   - **R1c** — blanking the derivation *inside* this function (`SkinFit::classify(None)`):
//!     compiles, and goes **RED — 266 passed / 4 failed / 12 ignored**, because `mod tests` below
//!     hands `observe_skin_fit` real `ModelAsset` values. That is the reachability control
//!     (eqoxide#797 / eqoxide#799): the line that decides what is observed is executed by the suite,
//!     which is exactly what was not true of the call-site expression it replaced.
//! - **Does (closed by eqoxide#848, and only actually closed by eqoxide#900's review round — was
//!   the open "Does not" here):** pin the function's *other* caller-supplied arguments,
//!   `model_path` and `label`. They are **gone as separate arguments**: `ModelAsset` carries the
//!   path `ModelAsset::load` actually opened, set inside `load()` itself, exposed read-only through
//!   [`crate::models::ModelAsset::loaded_from`], and `observe_skin_fit` derives both the report key
//!   and the log's identifying string from it. The two rows this closed:
//!   - **R3** (was open) — `&path` replaced by `Path::new("/models/boat.glb")` at the call site.
//!     There is no `&path` argument left at the call site to replace: `ensure_character_model`
//!     calls `observe_skin_fit(sink, asset)` — two arguments, not four — so this exact mutation is
//!     an edit with nowhere to land.
//!   - **R3b** (was open) — `key`/`label` replaced by a string literal. Same closure, same reason:
//!     `label` does not exist as a parameter to swap.
//!   - **R3′** — the same *symptom* reached without either argument, by writing the field instead
//!     of passing it: `Ok(mut asset) => { asset.loaded_from = PathBuf::from("/models/boat.glb"); }`
//!     inserted into `ensure_character_model` right where `load` returns. **Deleting an argument
//!     did not close this**, and the eqoxide#900 reviewer measured it: while `loaded_from` was a
//!     `pub` field on an all-`pub` struct, that insertion compiled clean, filed every downgrade in
//!     the session under `boat.glb`, and left `-p eqoxide-renderer` at **277 passed / 0 failed /
//!     13 ignored**, byte-identical to its control. The fix is privacy, not another argument
//!     removal: the field is now private to `models.rs` with a read-only accessor, and the only
//!     other constructor is `ModelAsset::probe_for_tests`, which is `cfg(test)` **and**
//!     `pub(crate)` — the same tier as `DowngradeSink::detached` below. Re-measured after the fix
//!     by re-applying the identical insertion: `error[E0616]`, "field `loaded_from` of struct
//!     `ModelAsset` is private", reported **twice** — once in `(lib)` and once in `(lib test)`, so
//!     no build accepts it. The *other* route to the same forgery, a `ModelAsset` struct literal
//!     naming the field (which is what the test helper below used to be, and what the reviewer
//!     cited as proof the literal was expressible), was measured separately because typeck aborts
//!     before the privacy pass runs when both are applied at once: `error[E0451]`, same field, same
//!     verdict. See the eqoxide#900 round-2 PR comment for both runs.
//!
//!   What replaces both as the live mutation target is the [`crate::renderer::downgrade_key`]
//!   collision path itself, which R3 was gesturing at: two downgraded rigs keyed alike (same base
//!   name, different source path) still collapse into one *entry* — `downgrade_key` is deliberately
//!   a pure function of the file's base name, not the full path, per this project's no-local-detail
//!   policy — but the entry now carries `key_collision: bool` so the collapse is a distinguishable,
//!   observable state instead of a silent one. Mutation-tested this session, both directions, on
//!   `SkinCapDowngrade`'s sticky-OR update in `record_skin_cap_downgrade`
//!   (`existing.key_collision = existing.key_collision || collided_this_write`):
//!   - **Revert** (naive from-scratch recompute `existing.source != source` in place of the sticky
//!     OR) — RED: `key_collision_is_sticky_and_does_not_clear_on_a_later_agreeing_write` panics,
//!     "the flag must stay set forever once two distinct sources have collided, even once every
//!     subsequent write agrees with the one immediately before it". **276 / 1 / 13.**
//!   - **WRAP** (`if false { existing.key_collision = … }`) — RED: the same test fails plus
//!     `two_roots_with_the_same_basename_collide_into_one_entry`. **275 / 2 / 13.**
//!   - Both restored to **277 / 0 / 13**, the current control, with a `Compiling eqoxide-renderer`
//!     line confirmed in the post-restore log (not just a cached, mtime-skipped rebuild).
//!
//!   `two_genders_that_load_the_same_file_are_reported_once` and
//!   `two_genders_that_load_two_different_files_are_reported_separately` in
//!   `tests/skin_cap_selection.rs` pin the ordinary (non-colliding) cases on either side of it.
//! - **Does not:** check that the asset it is handed is the one `ModelAsset::load` just returned. A
//!   deliberate edit could load a second asset and observe *that*. What the by-value witness buys is
//!   that such an edit is no longer silent: `build_character_model` uploads whatever asset is inside
//!   the witness, so an observation about a different model is also a *render* of that different
//!   model. The report and the uploaded geometry therefore cannot disagree by omission — that holds
//!   only while `build_character_model` takes no other asset, which is a signature it would have to
//!   grow. Both can still be wrong together.
//! - **Does not:** stop a future edit from *adding* a constructor here, or from calling
//!   `SkinFit::classify` directly and routing around this module. Nothing in Rust prevents a
//!   deliberate edit to the module that holds the invariant. The claim is about omission, not
//!   about intent.
//! - **Does not:** extend past `EqRenderer`. `src/bin/render_model.rs` — the offline model viewer —
//!   calls `SkinFit::classify` itself and has no `skin_cap_downgrades` to report into; it prints
//!   the downgrade instead. That is a present fact about the workspace, not a hypothetical future
//!   edit.
//! - **Does not:** grade the log line, **or pin it at all**. No test here asserts on log text, and
//!   that is a deliberate choice rather than an oversight: the driving agent reads structured
//!   observables, not logs, so the report is the channel worth pinning and it *is* pinned. What the
//!   shared `if let` buys is only that the *gate* resists a cheap widening — the `error!` and the
//!   `record_skin_cap_downgrade` call sit inside one `if let`, the message interpolates that
//!   binding, and the binding's value leaves the function as [`ObservedModel::reported_downgrade`],
//!   where the tests pin it against the recorded entry. All four rows re-measured on this head
//!   (eqoxide#797 round, against the then-current 270/0/12 control; not re-run against the
//!   277/0/13 control eqoxide#848 left behind, since none of the four touches the arguments #848
//!   changed):
//!   - **C4** — the gate replaced by `if true`: `error[E0425]` at **two** sites, `skin_observation.rs`
//!     line 224 col 74 (the `{over_cap_joints}` interpolation in the log message) and line 230
//!     col 29 (the `reported = Some(over_cap_joints)` assignment).
//!   - **C3** — the gate widened with `.or(Some(0))`: compiles, **RED — 267 / 3 / 12**.
//!   - **P1**, positive control for the report channel — deleting the `record_skin_cap_downgrade`
//!     call: compiles, **RED — 267 / 3 / 12**.
//!   - **R2** — deleting the whole `tracing::error!` statement, one statement, nothing else touched:
//!     **compiles** and stays **GREEN at 270 / 0 / 12, identical to control**. The only complaint is
//!     two warnings, `unused import: JOINT_CAP` and `unused variable: label` (2 in `(lib)`, 1 in
//!     `(lib test)`, where `mod tests` still uses `JOINT_CAP`); this workspace sets no
//!     `deny(warnings)` anywhere, so warnings are not a gate.
//!
//!   So the set of models that get *logged* can go to empty under a **one-statement** edit, and
//!   nothing notices. An earlier version of this bullet said the log was "pinned against
//!   single-expression edits and not against a two-line one" — that was measured false by R2, which
//!   is neither. The honest statement is the plain one: the report is the pinned channel, the log is
//!   not pinned in either direction, and the `if let` constrains only the *gate*. (Round 3's C6 —
//!   re-splitting the gate over two lines while restoring `reported`, which also survives — is
//!   carried over from that round's measurement; it was not re-run here, and R2 makes it moot as a
//!   boundary anyway.)

use std::collections::BTreeMap;

use crate::renderer::{record_skin_cap_downgrade, EqRenderer, SkinCapDowngrade, SkinFit, JOINT_CAP};

/// Where a cap downgrade gets reported: a mutable borrow of `EqRenderer::skin_cap_downgrades`.
///
/// The field is private and [`DowngradeSink::of`] is the only constructor a non-test build has, so
/// an [`observe_skin_fit`] call cannot be pointed at some other map. That matters because the
/// witness [`ObservedModel`] would otherwise prove only that the observation *ran* — a caller
/// could hand over a scratch `BTreeMap`, take a perfectly valid witness, pick a render arm, and
/// leave the renderer's report empty. That was measured on the first version of this module, and it
/// is exactly the eqoxide#780 failure mode reached by a compile-clean edit.
pub struct DowngradeSink<'a> {
    entries: &'a mut BTreeMap<String, SkinCapDowngrade>,
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
    fn detached(entries: &'a mut BTreeMap<String, SkinCapDowngrade>) -> DowngradeSink<'a> {
        DowngradeSink { entries }
    }
}

/// A loaded [`crate::models::ModelAsset`] **plus** the [`SkinFit`] that was observed for it — i.e.
/// an asset whose downgrade, if any, has already been logged and recorded into the renderer's own
/// report, carried together with the classification that reporting produced.
///
/// The inner fields are private to this module and there is no public constructor, so this type is a
/// witness rather than a wrapper: holding one is evidence that the observation ran, and — because
/// the asset is *inside* it and [`into_parts`] is the only way back out — evidence that it ran for
/// the very asset the holder is about to upload. It is not `Copy` and not `Clone`, so it cannot be
/// spent twice: `EqRenderer::build_character_model` takes it by value, and a second hand-off of the
/// same witness is a borrow-check error rather than a second arm chosen on one observation.
///
/// [`into_parts`]: ObservedModel::into_parts
pub struct ObservedModel {
    /// The asset that was observed, and the only asset `build_character_model` can upload.
    asset: crate::models::ModelAsset,
    fit: SkinFit,
    /// The value of the single gate that drove both channels: `Some(joint_count)` exactly when the
    /// `error!` fired and an entry was recorded. Carried out so a test can evaluate the production
    /// gate itself rather than an extensionally-equal restatement of it.
    reported: Option<usize>,
}

impl ObservedModel {
    /// The classification itself, for the caller that has to choose a render arm.
    pub fn fit(&self) -> SkinFit {
        self.fit
    }

    /// Consume the witness, yielding the observed asset together with its classification. The two
    /// come out of one value, so the arm chosen and the geometry uploaded are about the same model.
    pub fn into_parts(self) -> (crate::models::ModelAsset, SkinFit) {
        (self.asset, self.fit)
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
/// Takes the `asset` **by value** and reads its joint count itself rather than accepting a
/// caller-computed `Option<usize>`, and returns it inside the [`ObservedModel`] witness. That is the
/// whole reason the signature looks like this: with a caller-supplied count, a reviewer replaced
/// that one expression with `None` at the call site and the crate compiled clean and stayed green
/// while a 129-joint rig went unreported — eqoxide#780 reached by a nine-token edit. The asset now
/// enters and leaves through the same value, so what was observed and what gets uploaded are the
/// same model.
///
/// ## eqoxide#848: `model_path` and `label` are gone, not just unpinned
///
/// Earlier rounds of this function took `model_path: &Path` and `label: &str` as separate
/// caller-supplied arguments — the report was keyed by `model_path`'s file name, and `label`
/// appeared only in the log line. Both were measured **unpinned**: handing this function a path
/// that was not the one the asset actually came from compiled clean, stayed green, and filed the
/// downgrade under the wrong name (the previous revision of this doc's row R3), and two rigs keyed
/// alike silently collapsed into one entry, which is exactly eqoxide#848's collision symptom.
///
/// Neither argument exists anymore. [`crate::models::ModelAsset`] now carries the path
/// `ModelAsset::load` actually opened, readable through
/// [`crate::models::ModelAsset::loaded_from`], and this function derives both the report key and
/// the log's identifying string from it instead of from a second, independently-suppliable value.
/// The `asset` parameter this function already took by value (closing eqoxide#780's own R1) is now
/// *also* what closes R3/R3b: there is no longer a `model_path` or `label` token at the call site
/// for a mutation to swap out, because there is no longer a parameter there to swap.
/// `ensure_character_model`'s call site went from four arguments (`sink, &path, key, asset`) to two
/// (`sink, asset`) — see that function's own comment for why the loaded path cannot drift from what
/// was loaded (it is set by `load` itself, from the same `path` it opened, in the same function
/// call, into a field private to `models.rs`).
///
/// Nothing is recorded for `NoSkin`, `EmptySkin`, or `Fits`. That asymmetry is the entire point of
/// eqoxide#780: an unskinned `boat.glb` and a 129-joint rig both take the static arm, and only the
/// second is a downgrade.
///
/// Pure apart from the log line and the sink it is handed, so the wiring this function *is* can be
/// driven directly from a test with no `wgpu::Device` — see `mod tests` below.
pub fn observe_skin_fit(sink: DowngradeSink<'_>, asset: crate::models::ModelAsset) -> ObservedModel {
    let fit = SkinFit::classify(asset.skin.as_ref().map(|s| s.joint_count));
    let mut reported = None;
    if let Some(reason) = fit.static_reason() {
        // ONE gate for both channels. Forcing it open with `if true` is E0425 at TWO sites (row C4)
        // — the log message interpolates `over_cap_joints`, and so does the `reported` assignment
        // below. The assignment alone is enough: with the whole `error!` deleted *and* the gate
        // forced open, it is still E0425 at the assignment (row R2C4, measured). Nothing here holds
        // the *log* honest, though: deleting the `error!` statement on its own compiles and stays
        // green (row R2) — see the module header's "Does not: grade the log line" row.
        // `record_skin_cap_downgrade` tests the same predicate again internally; that is redundancy,
        // not a second decision.
        if let Some(over_cap_joints) = reason.downgrade_joint_count() {
            tracing::error!(
                "renderer: character model '{}' has a skin of {over_cap_joints} joints, EXCEEDING \
                 the {JOINT_CAP}-joint cap — falling back to the STATIC (unskinned) render arm; \
                 this model will not animate (eqoxide#780)",
                asset.loaded_from().display()
            );
            record_skin_cap_downgrade(sink.entries, asset.loaded_from(), reason);
            reported = Some(over_cap_joints);
        }
    }
    ObservedModel { asset, fit, reported }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::MAX_MEASURED_CHARACTER_RIG_JOINTS;

    /// A minimal [`crate::models::ModelAsset`] whose only interesting properties are its skin and
    /// the path it claims to have been loaded from: `None` for a model with no skin at all
    /// (`boat.glb`), `Some(n)` for one with an `n`-joint rig, keyed for the report by `loaded_from`.
    ///
    /// Every other field is the empty/zero value. That is deliberate: `observe_skin_fit` reads
    /// exactly two things off the asset — the skin, and the loaded path — and building the asset
    /// here rather than passing bare arguments is what makes the *derivation* —
    /// `asset.skin.as_ref().map(|s| s.joint_count)` and `asset.loaded_from()` — part of what the
    /// suite executes rather than something a test hands over pre-computed. Before eqoxide#780's
    /// round 4 the joint-count expression lived at the call site in `ensure_character_model`, where
    /// no test could reach it, and a reviewer measured that replacing it with `None` there compiled
    /// clean and left the crate green; the loaded path closes the same hole for eqoxide#848's
    /// `model_path`.
    ///
    /// This goes through [`crate::models::ModelAsset::probe_for_tests`] rather than writing a
    /// struct literal, because since eqoxide#900's review round the loaded path is a field private
    /// to `models.rs`. That is the point of the change: the struct literal this helper used to
    /// write was itself the standing proof that a caller-chosen `loaded_from` was expressible, and
    /// the eqoxide#900 reviewer cited exactly this helper as the counter-example to the PR's
    /// "nothing outside `load` constructs a `ModelAsset`" claim. Measured on this head: restoring a
    /// literal that names the field here — even as a functional-update expression over
    /// `probe_for_tests` — is `error[E0451]`, "field `loaded_from` of struct `models::ModelAsset`
    /// is private", in the `(lib test)` build. There is no build in which it is accepted.
    fn asset_with_skin(joint_count: Option<usize>, loaded_from: &str) -> crate::models::ModelAsset {
        crate::models::ModelAsset::probe_for_tests(
            loaded_from,
            joint_count.map(|joint_count| crate::anim::SkinData {
                joint_count,
                parents: vec![],
                inv_bind: vec![],
                clips: vec![],
                rest_translations: vec![],
                rest_rotations: vec![],
                rest_scales: vec![],
                ground_probes: vec![],
                joint_names: vec![],
            }),
        )
    }

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
        let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
        let observed = observe_skin_fit(
            DowngradeSink::detached(&mut map),
            asset_with_skin(Some(129), "/models/race_over.glb"),
        );

        assert!(!observed.is_skinned(), "a 129-joint skin must take the STATIC arm");
        assert_eq!(observed.fit(), SkinFit::ExceedsCap { joint_count: 129 });
        assert_eq!(observed.reported_downgrade(), Some(129));
        assert_eq!(map.get("race_over.glb").map(|e| e.joint_count), Some(129),
            "a 129-joint skin must be REPORTED as a downgrade, keyed by the loaded file");
        assert_eq!(map.get("race_over.glb").map(|e| e.key_collision), Some(false),
            "one file loaded once is not a collision");
        assert_eq!(map.len(), 1);
    }

    /// The other half of the acceptance bar: the two static arms that are NOT downgrades take the
    /// same arm and produce no report. Before eqoxide#780 all three of these were one `bool` and
    /// nothing distinguished them; the point is that the arm is the same and the report is not.
    #[test]
    fn observing_an_unremarkable_static_model_takes_the_static_arm_and_reports_nothing() {
        let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();

        let empty = observe_skin_fit(
            DowngradeSink::detached(&mut map),
            asset_with_skin(Some(0), "/models/race_empty.glb"),
        );
        assert!(!empty.is_skinned());
        assert_eq!(empty.fit(), SkinFit::EmptySkin);
        assert_eq!(empty.reported_downgrade(), None);
        assert!(map.is_empty(),
            "a zero-joint skin is degenerate data, not a cap downgrade — it must not be reported");

        let none = observe_skin_fit(
            DowngradeSink::detached(&mut map),
            asset_with_skin(None, "/models/boat.glb"),
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
        let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
        let observed = observe_skin_fit(
            DowngradeSink::detached(&mut map),
            asset_with_skin(Some(MAX_MEASURED_CHARACTER_RIG_JOINTS), "/models/race_pcfroglok.glb"),
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
            let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
            let observed = observe_skin_fit(
                DowngradeSink::detached(&mut map),
                asset_with_skin(joint_count, "/models/x.glb"),
            );

            assert_eq!(observed.reported_downgrade(), map.get("x.glb").map(|e| e.joint_count),
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
            let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
            let observed = observe_skin_fit(
                DowngradeSink::detached(&mut map), asset_with_skin(Some(n), "/models/x.glb"),
            );

            let reported = map.get("x.glb").map(|e| e.joint_count);
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
        let mut map: BTreeMap<String, SkinCapDowngrade> = BTreeMap::new();
        let observed = observe_skin_fit(
            DowngradeSink::detached(&mut map), asset_with_skin(None, "/models/boat.glb"),
        );
        assert!(map.is_empty(), "no skin at all is not a downgrade");
        assert_eq!(observed.reported_downgrade(), None);
        assert_eq!(observed.is_skinned(), old_use_skinned(None));
    }
}
