//! #801: what the move of `mod afloat` into `eqoxide-core` did — and did not do — to #800's
//! guarantee, MEASURED from outside the defining crate.
//!
//! This file is an *integration* test, which cargo compiles as its own crate linking `eqoxide-core`
//! as an external dependency. Everything asserted here is therefore asserted across the same crate
//! boundary `movement.rs`, `eqoxide-ipc`, `eqoxide-net` and `eqoxide-http` now sit on.
//!
//! # The claim being checked
//!
//! #800 made a premature or fabricated [`AfloatStall`] unrepresentable, and enforced it with a
//! *module* boundary inside `movement.rs` (Rust field privacy is module-scoped, not type-scoped, so
//! without the module any of that file's 4,100 lines could have written the literal). #801 moves the
//! module DOWN into `eqoxide-core` — necessary, because `eqoxide-ipc` and `GameState` have to name
//! the type to carry it, and neither can depend on the app crate. That move requires widening
//! [`AfloatStallClock`], [`AfloatFrame`] and the two thresholds from `pub(super)` to `pub`, and
//! widening is the direction in which fabricability comes back. So it is measured here rather than
//! reasoned about in prose.
//!
//! # Half one: the fabrication forms are compile errors (measured, not reasoned)
//!
//! The three forms below live as `compile_fail` doctests on [`AfloatStall`] and on
//! [`AfloatStallClock`], so `cargo test` fails the suite the day any of them starts compiling. That
//! is the live gate. The transcript below is a *record of one measured run* — it was produced by
//! putting all five forms in a temporary integration test in this same directory and running
//! `cargo test -p eqoxide-core --locked --test <probe>` under `rustc 1.97.0 (2d8144b78 2026-07-07)`.
//! Being a comment it can go stale; the doctests cannot, which is why both exist.
//!
//! Forms 1, 2 and 5 (struct literals), verbatim:
//!
//! ```text
//! error[E0451]: fields `secs` and `anchor` of struct `AfloatStall` are private
//!  --> crates/eqoxide-core/tests/zz_fab_probe.rs:6:27
//!   |
//! 6 |     let _ = AfloatStall { secs: 99.0, anchor: [1.0, 2.0, 3.0] };
//!   |                           ^^^^        ^^^^^^ private field
//!   |                           |
//!   |                           private field
//!
//! error[E0451]: fields `secs` and `anchor` of struct `AfloatStall` are private
//!   --> crates/eqoxide-core/tests/zz_fab_probe.rs:11:27
//!    |
//! 11 |     let _ = AfloatStall { secs: 0.1, anchor: [1.0, 2.0, 3.0] };
//!    |                           ^^^^       ^^^^^^ private field
//!    |                           |
//!    |                           private field
//!
//! error[E0451]: field `window` of struct `AfloatStallClock` is private
//!   --> crates/eqoxide-core/tests/zz_fab_probe.rs:16:36
//!    |
//! 16 |     let clock = AfloatStallClock { window: Some(([9.0, 9.0, 9.0], 99.0)) };
//!    |                                    ^^^^^^ private field
//! ```
//!
//! Note the second one: a SUB-threshold literal fails for exactly the same reason a wild one does.
//! There is no "close enough" fabrication that slips through, because the check is not on the value.
//!
//! Forms 3 and 4 (no `Default`; editing an obtained instance), verbatim from a separate run — they
//! had to be compiled apart from the literals, because rustc reports E0451 in a privacy pass it does
//! not reach when typeck has already errored, and the first run silently showed only three of five:
//!
//! ```text
//! error[E0277]: the trait bound `AfloatStall: std::default::Default` is not satisfied
//!   --> crates/eqoxide-core/tests/zz_fab_probe.rs:16:26
//!    |
//! 16 |     let _: AfloatStall = Default::default();
//!    |                          ^^^^^^^^^^^^^^^^^^ the trait `std::default::Default` is not implemented for `AfloatStall`
//!
//! error[E0616]: field `secs` of struct `AfloatStall` is private
//!   --> crates/eqoxide-core/tests/zz_fab_probe.rs:26:11
//!    |
//! 26 |     stall.secs = 0.0;
//!    |           ^^^^ private field
//!
//! error[E0616]: field `anchor` of struct `AfloatStall` is private
//!   --> crates/eqoxide-core/tests/zz_fab_probe.rs:27:11
//!    |
//! 27 |     stall.anchor = [7.0, 7.0, 7.0];
//!    |           ^^^^^^ private field
//! ```
//!
//! # Half two: what the widening DID cost, stated plainly
//!
//! [`AfloatStallClock`] and [`AfloatFrame`] are `pub` now, so any crate in the workspace can build a
//! clock of its own and feed it synthetic frames. It cannot fabricate a value that way — the clock's
//! `window` field is private (form 5 above), the threshold test lives inside
//! [`AfloatStallClock::stall`], and there is no path to an [`AfloatStall`] that skips it — but it can
//! *simulate*: hand `observe` a made-up `dt` and get a real, correctly-shaped stall out. So what #801
//! strengthens is the **unfabricability of the value**, not the **unreachability of the constructor**;
//! the set of code that can mint one grew from one file to any crate. The tests below bound that
//! residual by construction rather than by assertion in prose: they drive the clock adversarially
//! from outside and check that every stall it will ever hand out is honest about its own two fields.

use eqoxide_core::afloat::{
    AfloatFrame, AfloatStall, AfloatStallClock, AFLOAT_PROGRESS, AFLOAT_STALL_SECS,
};

/// How far the modelled window and the clock's own accumulator may disagree.
///
/// The clock sums `dt` in `f32`, so after a few thousand frames its idea of the window's age drifts
/// from `frames * dt` by a few parts in ten thousand. Any test that predicts the exact frame the
/// threshold is crossed on is testing float accumulation, not the signal. Everything below allows
/// this slack around the crossing and asserts hard everywhere outside it.
const CROSSING_SLACK_SECS: f32 = 0.01;

/// Mature a stall the only way any crate can: real `Wished` frames at a real position.
///
/// Note the `+ 3`: the FIRST `Wished` frame opens a window at `secs = 0.0` and adds no time, so a
/// window that must reach `AFLOAT_STALL_SECS` needs one frame more than `SECS / dt`, and f32
/// accumulation can cost one or two more on top. This is a property of the clock, not a defect —
/// it errs toward silence, which is the correct direction for a false-alarm-sensitive signal.
fn matured(pos: [f32; 3], dt: f32) -> AfloatStall {
    let mut clock = AfloatStallClock::default();
    let n = (AFLOAT_STALL_SECS / dt).ceil() as usize + 3;
    for _ in 0..n {
        clock.observe(AfloatFrame::Wished, pos, dt);
    }
    clock.stall().expect("a body pinned at one point under a sustained wish must stall")
}

/// The public surface's sub-threshold answer is `None`, not a small `AfloatStall`.
///
/// This is the half of the orchestrator's question that is NOT a compile error: reaching for a
/// premature value through the API that #801 widened. It is refused at runtime instead, and the
/// refusal is total — there is no `secs` below [`AFLOAT_STALL_SECS`] that yields a `Some`.
///
/// **Axes deliberately varied:** elapsed time (a full sweep up to the threshold), `dt`, and all
/// three position components. **Axis deliberately NOT varied:** the body does not move — that is the
/// point of this test, and the movement axis is covered by
/// [`progress_on_any_axis_including_z_refuses_to_mature_a_stall`] below, which exists because #800
/// shipped a live false alarm past seven tests that all pinned `z = 0.0`.
#[test]
fn no_sub_threshold_stall_is_reachable_through_the_public_surface_801() {
    for &dt in &[0.008_f32, 0.016, 0.05, 0.1, 0.33] {
        for &pos in &[[0.0_f32, 0.0, 0.0], [-812.5, 43.0, -119.75], [1e4, -1e4, 250.0]] {
            let mut clock = AfloatStallClock::default();
            let cap = (AFLOAT_STALL_SECS / dt) as usize + 64;
            let mut disclosed = None;
            for frame in 1..=cap {
                clock.observe(AfloatFrame::Wished, pos, dt);
                // The first Wished frame OPENS the window at 0.0 and adds no time to it.
                let modelled = (frame - 1) as f32 * dt;
                match clock.stall() {
                    Some(s) => {
                        assert!(
                            modelled >= AFLOAT_STALL_SECS - CROSSING_SLACK_SECS,
                            "disclosed a stall after a {modelled}s window (dt={dt}, pos={pos:?}), \
                             which is below the {AFLOAT_STALL_SECS}s threshold",
                        );
                        assert!(s.secs() >= AFLOAT_STALL_SECS, "disclosed {}s", s.secs());
                        disclosed = Some(frame);
                        break;
                    }
                    None => assert!(
                        modelled <= AFLOAT_STALL_SECS + CROSSING_SLACK_SECS,
                        "still silent after a {modelled}s window (dt={dt}, pos={pos:?}) — that is \
                         the FALSE-NEGATIVE direction and #801 publishes this to an agent",
                    ),
                }
            }
            // Without this the loop above is satisfiable by a clock that never discloses anything.
            assert!(disclosed.is_some(), "no stall ever matured at dt={dt}, pos={pos:?}");
        }
    }
}

/// Every stall obtainable from outside the crate is honest about both of its fields.
///
/// The residual disclosed above is that another crate can *simulate* a stall. This bounds what a
/// simulation can produce: `secs` is never below the threshold and `anchor` is never a point the
/// caller did not actually pass to `observe`. A synthetic driver can therefore make the clock say
/// "stalled at P" only by genuinely holding the body at P for the full window.
///
/// **Axes varied:** `dt` (including a 30 ms hitch), anchor position on all three axes, and the
/// number of frames. **Axis not varied:** the frame kind is always `Wished` in the maturing loop —
/// non-`Wished` frames are covered by the reset test below.
#[test]
fn every_cross_crate_stall_carries_a_true_window_801() {
    for &dt in &[0.001_f32, 0.016, 0.03, 0.25] {
        for &pos in &[[0.0_f32, 0.0, 0.0], [12.5, -900.25, 41.5], [-3.0, 7.0, -212.75]] {
            let s = matured(pos, dt);
            assert!(
                s.secs() >= AFLOAT_STALL_SECS,
                "secs {} < threshold {AFLOAT_STALL_SECS}",
                s.secs(),
            );
            assert_eq!(s.anchor(), pos, "anchor must be the window's true opening position");
        }
    }
}

/// Net progress on ANY axis — including z alone — resets the window and keeps the signal silent.
///
/// This is the false-alarm direction, checked across the crate boundary. #800's live false alarm was
/// a body diving 120 u in 6 s while its lateral wish was blocked: real 3-D progress, reported as a
/// stall, because the progress term was horizontal-only. Seven dedicated false-alarm tests missed it
/// because every one of them pinned `z = 0.0`.
///
/// **Axes varied:** which single axis the body moves along (east, north, AND up/down, in both
/// directions), the per-frame step size, and `dt`. **Axis not varied:** the wish is always present —
/// a body with no wish is `Resting`, which is the next test.
#[test]
fn progress_on_any_axis_including_z_refuses_to_mature_a_stall_801() {
    let dt = 0.05_f32;
    // Per-frame steps that each exceed AFLOAT_PROGRESS, so the window re-anchors every frame.
    let step = AFLOAT_PROGRESS * 1.5;
    for axis in 0..3usize {
        for &sign in &[1.0_f32, -1.0] {
            let mut clock = AfloatStallClock::default();
            let mut pos = [0.0_f32, 0.0, 0.0];
            // Ten times the window's length. If a moving body can ever accumulate a stall, this
            // finds it.
            let n = (AFLOAT_STALL_SECS * 10.0 / dt) as usize;
            for _ in 0..n {
                pos[axis] += sign * step;
                clock.observe(AfloatFrame::Wished, pos, dt);
                assert!(
                    clock.stall().is_none(),
                    "a body making {step} u/frame of progress along axis {axis} (sign {sign}) is \
                     not trapped and must never be disclosed as stalled",
                );
            }
        }
    }
}

/// A non-`Wished` frame closes the window outright, from outside the crate too.
///
/// **Axes varied:** which non-`Wished` variant interrupts, and where in the window it lands.
/// **Axis not varied:** position is held constant — the interruption, not motion, is what is under
/// test here.
#[test]
fn a_single_resting_or_dry_frame_closes_the_window_801() {
    let dt = 0.1_f32;
    let pos = [4.0_f32, -8.0, 15.5];
    for interrupt in [AfloatFrame::Resting, AfloatFrame::NotAfloat] {
        for break_at in 1..((AFLOAT_STALL_SECS / dt) as usize) {
            let mut clock = AfloatStallClock::default();
            for i in 0..((AFLOAT_STALL_SECS / dt) as usize + 1) {
                if i == break_at {
                    clock.observe(interrupt, pos, dt);
                } else {
                    clock.observe(AfloatFrame::Wished, pos, dt);
                }
                // The surviving window can never be older than the frames since the break.
                if let Some(s) = clock.stall() {
                    let since_break = (i - break_at) as f32 * dt;
                    assert!(
                        s.secs() <= since_break + 1e-3,
                        "{interrupt:?} at frame {break_at} did not close the window: disclosed \
                         {}s at frame {i}, but only {since_break}s have elapsed since",
                        s.secs(),
                    );
                }
            }
        }
    }
}
