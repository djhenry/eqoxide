//! #776's afloat-stall signal: a body afloat in water, asked to swim somewhere, going nowhere.
//!
//! # Why this lives in `eqoxide-core` (#801)
//!
//! #800 built this as a private `mod afloat` inside the app crate's `movement.rs`, because that was
//! the only place that needed it. #801 publishes the signal over HTTP, and the path a controller
//! observable travels to `GET /v1/observe` runs `movement.rs` → `eqoxide_ipc::ControllerView` →
//! `GameState` → `eqoxide-http`. `eqoxide-ipc` cannot depend on the app crate (that is a cycle —
//! the app crate depends on it), so the published type has to be nameable from below all of them.
//! `eqoxide-core` is that place; it is where `ControllerHold` already lives, for exactly this
//! reason.
//!
//! The whole module moved rather than just the type. Moving only `AfloatStall` would have needed a
//! `pub` constructor for `AfloatStallClock::stall` — in another crate — to call, which is precisely
//! the fabricable surface the module boundary exists to deny.
//!
//! ## What that move does and does not do to #800's guarantee — MEASURED, not reasoned
//!
//! **Unchanged, and now enforced workspace-wide rather than file-wide.** [`AfloatStall`]'s fields
//! stay private to *this module*, it has no public constructor, no `Default`, and no setters. No
//! crate — including `movement.rs`, which used to be a sibling of the module and is now outside the
//! defining crate entirely — can name `secs` or `anchor` to build or edit one. Five fabrication
//! forms were compiled from outside this crate and all five were rejected; the errors are quoted
//! verbatim, with their real `file:line:column`, in
//! `crates/eqoxide-core/tests/afloat_unconstructible.rs`, and each one also lives as a
//! `compile_fail` doctest on the type it attacks, so the suite fails the day one of them starts
//! compiling. Measured, not asserted in prose.
//!
//! **Widened, and this is the honest cost.** [`AfloatStallClock`], [`AfloatFrame`] and the two
//! thresholds were `pub(super)` — reachable only from `movement.rs`. They are `pub` now, because
//! `CharacterController` holds a clock as a field and folds frames into it from another crate. So
//! any crate can now build a clock of its own, feed it synthetic `Wished` frames with a fabricated
//! `dt`, and obtain a real [`AfloatStall`] from it. That is a *simulation*, not a fabrication —
//! every value obtained that way still satisfies `secs >= AFLOAT_STALL_SECS` and carries the
//! window's true opening position, because the threshold test lives inside [`AfloatStallClock::stall`]
//! and cannot be bypassed — but the set of code that can mint one grew from one file to any crate.
//! Stated here rather than left for a reader to discover: what #801 strengthens is the
//! *unfabricability of the value*, not the *unreachability of the constructor*.
//!
//! ## Why a MODULE and not just private fields — carried over from #800 verbatim
//!
//! Round-2 review N3 measured the reason this is a module: Rust's private fields are MODULE-private,
//! not type-private, so while [`AfloatStall`]'s fields kept every caller in the workspace out, any
//! line of `movement.rs` — 4,100 of them — could still write `AfloatStall { secs: 0.0, anchor: … }`
//! and fabricate a sub-threshold instance. The doc claimed otherwise. Rather than weaken the claim
//! to "outside this module", the boundary was moved to where the claim already was: nothing outside
//! this module can name those fields, [`AfloatStallClock::stall`] is the sole construction site, and
//! an edit anywhere else fails to compile rather than producing a premature alarm. #801's relocation
//! keeps that boundary and moves it to a 260-line file, where "everything that can name these fields
//! is in front of you" is checkable by reading rather than trusted.
//!
//! # The signal
//!
//! Names below that live in the app crate's `movement.rs` rather than here — `step`,
//! `depenetrate`, `try_duck_under`, `CharacterController`, and the test names — are its call sites;
//! this module is the definition they call into.
//!
//! Since #661 a body afloat in water never enters the depenetration net (see `depenetrate`'s door,
//! and the reasons there — every question the net asks about a floating body is mis-posed). That
//! removed a LOUD failure mode: the review that produced #767 measured `main` walking a swimmer
//! ~140 u OUT of a sealed box through the walls, one ring radius at a time. What it left behind is a
//! QUIET one. A swimmer sealed in a pocket, or pressing at a passage `try_duck_under` refuses,
//! simply stops: `on_ground = false`, `in_water = true`, `hold() = None`, and `stuck_time` never
//! accrues because the net's clock is the only one and the net no longer runs for floaters. Every
//! observable reads "swimming normally". That is the project's top-ranked defect class — a missing
//! disclosure an agent cannot detect, cannot retry around, and builds every later decision on.
//!
//! # Why this is NOT a `ControllerHoldReason`, and why it needs its own signal
//!
//! The state is **not locally distinguishable from correct behaviour**. A swimmer resting at its
//! float plane beside a wall is also stationary, also unsupported, also wet — that is what a swimmer
//! DOES. Publishing the shape naively would raise an alarm on every ordinary floating character, and
//! **a false alarm in an honesty observable is the same defect as a silence** (the argument is
//! recorded verbatim at the neutral-buoyancy branch in `step`, which declines to report itself for
//! exactly this reason).
//!
//! So the signal is not the shape, it is **sustained zero progress against a nonzero wish**:
//!
//!   * **nonzero horizontal wish** — the driver is actively asking for lateral motion, and asking
//!     with a speed behind it. An idle floater has no wish, so it can never open a window. This is
//!     the whole resting-vs-trapped distinction, and it is enforced by TYPE below
//!     ([`AfloatFrame::Resting`] has no path to the advancing arm), not by a guard a later edit can
//!     invert;
//!   * the WISH half is **horizontal only**, deliberately: a pure UP-wish that goes nowhere is what
//!     a swimmer at the surface gets from the surface clamp in `step`, and is what the walker's own
//!     haul-out steering sends. Opening a window on it would false-alarm on the single most common
//!     wish in the whole water system;
//!   * the PROGRESS half is **three-dimensional**, and the asymmetry is the point. The reason above
//!     is a reason about WISHES and does not carry over: the surface up-wish case has a ZERO
//!     horizontal wish, so it is already `Resting` and no window is open for a vertical progress
//!     term to keep alive. Meanwhile the production intent shape sets a unit horizontal `wish_dir`
//!     and a `swim_vspeed` in the same `MoveIntent` (the walker's Slice-3 depth control, and the
//!     manual path), so a swimmer descending a shaft with its lateral wish pressed into the shaft
//!     wall is ordinary — and a horizontal-only progress term disclosed exactly that body as going
//!     nowhere. **Measured**, round-2 review B1 and
//!     `a_driven_dive_or_rise_along_a_blocked_face_never_raises_the_afloat_stall`: 120 u of vertical
//!     travel in 6 s reported as `AfloatStall { secs: 5.92 }`, with a log line offering "a down-wish
//!     dive may still cross" to a body already performing one. Counting z can only ever RE-ANCHOR
//!     the window, never advance it, so widening it here is strictly a false-alarm reduction;
//!   * **sustained** — `AFLOAT_STALL_SECS` against net displacement from an anchor, so no transient
//!     (buoyancy settling, a refused step-up, a grazing slide) can trip it.
//!
//! # What this deliberately does NOT claim
//!
//! An `AfloatStall` is weaker than a [`ControllerHold`] and must never be conflated with one. A hold
//! says *the body cannot move at all, under any driver*. A stall says only *this wish has produced
//! no motion for this long*. The trapped swimmer at qcat's pocket mouth is escapable — by a DRIVEN
//! dive, which `qcat_pocket_swimmer_escapes_to_the_shaft_under_a_driven_dive` pins — while the
//! horizontal-only drive that stalls there cannot get out on its own. Reporting "you are not moving"
//! is true of both; reporting "you are frozen" would be false of the first. That is why this is a
//! separate type with its own vocabulary rather than a third `ControllerHoldReason` variant.
//!
//! Scope, stated plainly: this covers the AFLOAT case only (`in_water && !on_ground`). A DRY body
//! pressed against a wall is equally silent and equally uncovered — but that is pre-existing and not
//! what #661 changed, and widening this to dry bodies would collide with the depenetration net's own
//! `stuck_time`/`EmbeddedNoRecovery` accounting. A wading body standing on the bottom is likewise
//! out (it is `on_ground`, so the dry-net vocabulary applies to it).
//!
//! # The FALSE-NEGATIVE classes this signal does not cover — named, not implied
//!
//! The predicate is deliberately narrow, and narrowness has a cost. These bodies are genuinely stuck
//! and this signal stays silent about every one of them. That is the correct behaviour for the
//! predicate as defined, not a bug to be "fixed" by loosening it — but leaving them unnamed would be
//! the same defect in prose that a silent observable is in code:
//!
//!   * **a swimmer lidded under a PURE vertical wish** (`wish_dir = [0, 0]`, `wish_vspeed > 0`) —
//!     `hold() = None`, `afloat_stall() = None`, measured. There is no horizontal wish, so no window
//!     ever opens. This is the **#783** shape: at the qcat pocket the walker's steering turns a
//!     rise-at-destination `swim_surface` edge into an immediate up-wish, the body clamps under the
//!     pocket lid, and it stays there for the whole 20 s of the offline repro. Widening the WISH
//!     half to vertical is NOT the answer — it would false-alarm on every surface hold, which is the
//!     single most common wish in the water system (see the bullet above). #783 fixes it where it
//!     belongs, in the steering that produces the wrong wish;
//!   * **a swimmer slowly losing ground** — retreating, or drifting, at under `AFLOAT_PROGRESS` per
//!     window is not "no progress" by this definition;
//!   * **a swimmer circling a pocket wider than `AFLOAT_PROGRESS`** — it keeps re-anchoring, so the
//!     window never matures. Widening the progress term to 3D (above) extends this same residual to
//!     the vertical axis: a body oscillating vertically through more than `AFLOAT_PROGRESS` about a
//!     fixed point re-anchors too. Both are the known cost of a NET-displacement-from-anchor test,
//!     which is itself chosen over a per-frame epsilon for a stronger reason (see `AFLOAT_PROGRESS`).

/// #776: seconds of sustained no-progress, against a NONZERO horizontal wish, while the body is
/// afloat, before the controller discloses an [`AfloatStall`].
///
/// Sized to be far longer than any legitimate transient a floating body has: buoyancy settles to
/// the float plane at `BUOY_RATE` = 30 u/s (a full `float_depth` = 2 u of settle is 0.07 s), a
/// swimming step-up / duck-under either resolves on the frame it is tried or never, and a slide
/// along a wall makes real tangential progress. Three seconds of a driver asking for motion and
/// getting under [`AFLOAT_PROGRESS`] of it is not any of those.
pub const AFLOAT_STALL_SECS: f32 = 3.0;
/// #776: net displacement from the window's anchor (units) that counts as PROGRESS and re-arms
/// the window from the new position.
///
/// Measured as NET displacement from an anchor, not per-frame delta, on purpose: a per-frame
/// epsilon would be re-armed for ever by float noise or by a body oscillating between two
/// contacts, which is the same "not going anywhere" the signal exists to disclose. 0.5 u is
/// ~0.011 s of travel at the 44 u/s the nav swim drive uses, so a body that is genuinely
/// swimming clears it on the first frame and never opens a window at all.
///
/// Measured in **three dimensions** (round-2 review B1): see the `PROGRESS half` bullet in the
/// block comment above this module for why the wish half's horizontal-only reason does not carry
/// over to this half, and for the 120 u false alarm it produced when it did.
pub const AFLOAT_PROGRESS: f32 = 0.5;

/// Euclidean length of a 3-vector. The progress term, and the ONE place the signal's notion of
/// "got somewhere" is defined.
#[inline]
fn len3(d: [f32; 3]) -> f32 { (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() }

/// #776: the body is afloat and a horizontal wish has produced no net progress for a sustained
/// window. Level-triggered — recomputed from scratch every stepped frame, exactly like
/// `ControllerHold` — so it cannot outlive its cause.
///
/// The fields are private and there is no public constructor **on purpose**: the only code that
/// can build one is [`AfloatStallClock::stall`], and only once the window has actually reached
/// [`AFLOAT_STALL_SECS`]. A premature or fabricated `AfloatStall` is therefore not
/// representable — see this module's own doc for why that is a module boundary and not just a
/// field one — which is the point: this is an honesty observable, and the failure that matters
/// most for it is the FALSE one.
///
/// # The fabrication forms are compile errors, and these run (#801)
///
/// Each block below is a doctest, which rustdoc compiles as its **own crate** linking this one as
/// an external dependency — so they probe the real cross-crate boundary, not a same-crate one, and
/// `cargo test` fails the suite if any of them ever starts compiling. The error each one actually
/// produced is quoted verbatim, with its real `file:line:column`, in
/// `crates/eqoxide-core/tests/afloat_unconstructible.rs`, which also covers the half that is NOT a
/// compile error — reaching for a sub-threshold value through the public surface — and states
/// plainly what widening this module to `pub` cost.
///
/// ```compile_fail
/// // Form 1 — a fabricated struct literal. `secs`/`anchor` are private to the defining module.
/// let _ = eqoxide_core::afloat::AfloatStall { secs: 99.0, anchor: [0.0, 0.0, 0.0] };
/// ```
///
/// ```compile_fail
/// // Form 2 — a SUB-THRESHOLD literal fails identically. The refusal is not on the value.
/// let _ = eqoxide_core::afloat::AfloatStall { secs: 0.1, anchor: [0.0, 0.0, 0.0] };
/// ```
///
/// ```compile_fail
/// // Form 3 — no `Default`, so there is no zero-argument way in either.
/// let _: eqoxide_core::afloat::AfloatStall = Default::default();
/// ```
///
/// ```compile_fail
/// // Form 4 — editing a legitimately obtained one. The accessors return copies, never `&mut`,
/// // so a real stall cannot be re-pointed at a different anchor or aged past its true window.
/// let mut clock = eqoxide_core::afloat::AfloatStallClock::default();
/// for _ in 0..100 {
///     clock.observe(eqoxide_core::afloat::AfloatFrame::Wished, [0.0, 0.0, 0.0], 0.1);
/// }
/// let mut stall = clock.stall().unwrap();
/// stall.secs = 0.0;
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AfloatStall {
    secs:   f32,
    anchor: [f32; 3],
}

impl AfloatStall {
    /// Seconds the body has been afloat, wished at, and within [`AFLOAT_PROGRESS`] of `anchor`.
    /// Always `>= AFLOAT_STALL_SECS` — see the type doc. Counts the WHOLE window, including the
    /// pre-threshold part, so an agent watching it advance sees the true age of the stall.
    pub fn secs(self) -> f32 { self.secs }
    /// The position the window opened at — the point the body has failed to get more than
    /// [`AFLOAT_PROGRESS`] away from, in any direction.
    pub fn anchor(self) -> [f32; 3] { self.anchor }
}

/// One frame's answer to "is this body being asked to swim somewhere, and failing to?".
///
/// A total function of the frame's own facts, computed in ONE place ([`AfloatFrame::classify`]),
/// so the clock below cannot advance on a frame that was never classified [`Self::Wished`]. This
/// is the resting-vs-trapped distinction made unrepresentable-in-the-wrong-direction rather than
/// guarded: there is no arm of [`AfloatStallClock::observe`] that both accepts a non-`Wished`
/// frame and advances the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfloatFrame {
    /// Not afloat: dry, or standing/wading on the bottom (`on_ground`). The depenetration net's
    /// vocabulary (`stuck_time`, `ControllerHoldReason::EmbeddedNoRecovery`) owns these bodies.
    NotAfloat,
    /// Afloat with NO horizontal wish — an ordinary floating character, holding at its plane.
    /// **This variant is the false-alarm guard.** It resets the window like `NotAfloat` does.
    Resting,
    /// Afloat AND asked to move horizontally. Only this variant can advance the clock.
    Wished,
}

impl AfloatFrame {
    /// `afloat` = the body is in water and unsupported. `throttle` = the frame's horizontal wish
    /// magnitude and `speed` its requested speed — the same two values `step` multiplies
    /// together for its collide-and-slide, so the classification and the motion can never
    /// disagree about whether a wish was made.
    ///
    /// **BOTH are required** (round-2 review N5). `throttle` alone is `|wish_dir|`, so
    /// `wish_dir = [1, 0], speed = 0.0` used to classify `Wished` and mature a stall in EMPTY
    /// OPEN WATER with no geometry at all — measured, and pinned by
    /// `a_unit_wish_at_zero_speed_never_raises_the_afloat_stall`. A frame whose requested
    /// displacement is identically zero is not a drive that is failing; nobody asked for
    /// anything, which is precisely `Resting`.
    pub fn classify(afloat: bool, throttle: f32, speed: f32) -> Self {
        if !afloat { Self::NotAfloat }
        else if throttle > 1e-4 && speed > 0.0 { Self::Wished }
        else { Self::Resting }
    }
}

/// The window state behind [`AfloatStall`]. `None` = no window open.
///
/// A window carries the position it opened at and how long it has been open. Progress is
/// measured as NET 3-D displacement from that anchor rather than per-frame delta — see
/// [`AFLOAT_PROGRESS`].
///
/// # This type is `pub` in #801, and its field is still not (Form 5)
///
/// It was `pub(super)` while the module lived in `movement.rs`. `CharacterController` holds one as
/// a field and folds frames into it from another crate now, so it had to widen. `window` did not:
/// the only way to advance a clock remains [`observe`](Self::observe), which means the only way to
/// reach an [`AfloatStall`] is to genuinely hold a body at one point under a sustained wish.
///
/// ```compile_fail
/// // Form 5 — mint a matured window directly, skipping every frame that should have earned it.
/// let clock = eqoxide_core::afloat::AfloatStallClock { window: Some(([9.0, 9.0, 9.0], 99.0)) };
/// let _ = clock.stall();
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AfloatStallClock {
    window: Option<([f32; 3], f32)>,
}

impl AfloatStallClock {
    /// Fold one resolved frame in. `pos` is the body's position AFTER the frame resolved, `dt`
    /// the frame time.
    pub fn observe(&mut self, frame: AfloatFrame, pos: [f32; 3], dt: f32) {
        match frame {
            // Not afloat, or afloat with nothing asked of it: no window, nothing to disclose.
            // Both arms are unconditional — a resting floater cannot carry a window across an
            // idle frame and resume it later, so an alarm can never be assembled out of
            // scattered frames.
            AfloatFrame::NotAfloat | AfloatFrame::Resting => self.window = None,
            AfloatFrame::Wished => match self.window {
                None => self.window = Some((pos, 0.0)),
                Some((anchor, secs)) => {
                    // THREE-dimensional (round-2 review B1). A swimmer that descended 120 u of
                    // water column while its blocked lateral wish went nowhere is not trapped,
                    // and a horizontal-only term said it was. `hlen` is still the right measure
                    // for the WISH; it is the wrong one for "did the body get anywhere".
                    let moved = len3([pos[0] - anchor[0], pos[1] - anchor[1], pos[2] - anchor[2]]);
                    // Progress: re-anchor HERE and restart the clock. A body creeping forward
                    // 0.5 u at a time is making progress and must never accumulate a stall.
                    if moved > AFLOAT_PROGRESS { self.window = Some((pos, 0.0)); }
                    else { self.window = Some((anchor, secs + dt)); }
                }
            },
        }
    }

    /// The stall in force right now, or `None`. The ONLY constructor of [`AfloatStall`].
    pub fn stall(self) -> Option<AfloatStall> {
        match self.window {
            Some((anchor, secs)) if secs >= AFLOAT_STALL_SECS =>
                Some(AfloatStall { secs, anchor }),
            _ => None,
        }
    }
}
