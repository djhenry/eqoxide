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
//! `EqRenderer::build_character_model` demands one to pick a render arm at all.
//!
//! What that does and does not buy, stated precisely:
//!
//! - **Does:** selecting a render arm without having reported the downgrade is not expressible by
//!   omission. Deleting the observation from `ensure_character_model` does not produce a silent
//!   regression; it produces a compile error, because there is then no `ObservedSkinFit` to pass.
//!   Measured, not reasoned — see the PR's mutation table.
//! - **Does not:** stop a future edit from *adding* a constructor here, or from calling
//!   `SkinFit::classify` directly and routing around this module. Nothing in Rust prevents a
//!   deliberate edit to the module that holds the invariant. The claim is about omission, not
//!   about intent.

use std::collections::BTreeMap;
use std::path::Path;

use crate::renderer::{record_skin_cap_downgrade, SkinFit, JOINT_CAP};

/// A [`SkinFit`] that has been through [`observe_skin_fit`] — i.e. one whose downgrade, if any, has
/// already been logged and recorded.
///
/// The inner field is private to this module and there is no public constructor, so this type is a
/// witness rather than a wrapper: holding one is evidence the observation ran.
/// `EqRenderer::build_character_model` takes it by value for exactly that reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedSkinFit(SkinFit);

impl ObservedSkinFit {
    /// The classification itself, for the caller that has to choose a render arm.
    pub fn fit(self) -> SkinFit {
        self.0
    }

    /// Convenience for the one question the render path asks. Delegates to [`SkinFit::is_skinned`]
    /// rather than restating it, so the arm choice cannot drift from the classification.
    pub fn is_skinned(self) -> bool {
        self.0.is_skinned()
    }
}

/// Classify one loaded model's skin and, if it is a genuine joint-cap downgrade, report it on both
/// channels: the `error!` log and the `downgrades` map that backs
/// `EqRenderer::skin_cap_downgrades`.
///
/// `joint_count` is `None` when the model has no skin at all. `model_path` is the GLB that was
/// actually loaded — the report is keyed by its file name, not by `label`, because the joint count
/// belongs to the file (eqoxide#813); `label` appears only in the log line.
///
/// Nothing is recorded for `NoSkin`, `EmptySkin`, or `Fits`. That asymmetry is the entire point of
/// eqoxide#780: an unskinned `boat.glb` and a 129-joint rig both take the static arm, and only the
/// second is a downgrade.
///
/// Pure apart from the log line and the map it is handed, so the wiring this function *is* can be
/// driven directly from a test with no `wgpu::Device` — see `tests/skin_cap_selection.rs`.
pub fn observe_skin_fit(
    downgrades: &mut BTreeMap<String, usize>,
    model_path: &Path,
    label: &str,
    joint_count: Option<usize>,
) -> ObservedSkinFit {
    let fit = SkinFit::classify(joint_count);
    if let Some(reason) = fit.static_reason() {
        // Both channels are gated by the SAME expression — `downgrade_joint_count`, which
        // `record_skin_cap_downgrade` also uses. Gating them separately let a mutation change one
        // without the other; see that method's doc and the eqoxide#780 PR's mutation M5.
        if reason.downgrade_joint_count().is_some() {
            tracing::error!(
                "renderer: character model '{label}' ({}) has a skin that EXCEEDS the \
                 {JOINT_CAP}-joint cap ({fit:?}) — falling back to the STATIC (unskinned) render \
                 arm; this model will not animate (eqoxide#780)",
                model_path.display()
            );
        }
        record_skin_cap_downgrade(downgrades, model_path, reason);
    }
    ObservedSkinFit(fit)
}
