//! Zone-in orchestration — the glue that drives the #712 one-shot reground, lifted out of
//! [`crate::app::App`] so it can be run without a device.
//!
//! # Why this module exists
//!
//! #728: *"there is no harness that runs a zone-in from arrival through correction to the first
//! grounded frame."* The three pieces of that sequence that lived in `app.rs` — the asset-load
//! throttle, the `on_ground` early return, and the `on_ground = true` after a
//! [`crate::movement::Reground::Lift`] — were untested, because `App::update` owns a window, a GPU
//! and an event loop and cannot be constructed in a test. #720's own commit message says so in as
//! many words ("Not covered by any test: the `app.rs` half"). The answer to that is not two unit
//! tests bolted onto `App`; it is a seam. This module is the seam, and its test module is the
//! harness — `ZoneInHarness::frame` performs, in `app.rs`'s order, the two things `App::update`
//! does with the controller.
//!
//! # What a zone-in looks like, in order
//!
//! 1. `app.rs` observes `zone_needs_reload`, drops the OLD zone's collision and drops the
//!    controller's previous-zone recovery ring.
//! 2. ~10 s of asset load. The controller is **not stepped** on those frames (there is no
//!    collision to step against), so nothing about it changes — including `on_ground`.
//! 3. The nav streamer may hand over a large server correction, which `app.rs` applies as a
//!    [`crate::movement::CharacterController::teleport`]. It may equally hand over **nothing**:
//!    see [`ZoneIn::on_frame`]'s note on the #593 path.
//! 4. The first frame with the new zone's collision in hand: this one-shot runs.
//! 5. Ordinary stepped frames from there on.
//!
//! # The wiring, and why there is only one call
//!
//! `app.rs` has exactly **one** statement that reaches this module: the per-frame
//! [`ZoneIn::on_frame`]. Nothing else. That is a deliberate consequence of #791's round-2 review,
//! which demonstrated the limit of the source-scan pins this module used to lean on: wrapping a
//! pinned call in `if false { … }` left every test in this file green, because `str::contains`
//! sees that a call is *written* and cannot see that it is *reached*. Three separate call sites
//! meant three separate chances to lose the orchestration silently.
//!
//! So the other two were removed rather than pinned harder — the fix is a type, not a guard
//! (verification hierarchy, tier 1):
//!
//! * **Arming.** There is no `arm()` call. `on_frame` is handed the zone `app.rs` is loading and
//!   arms itself the frame that name changes. A caller cannot forget to arm, arm the wrong zone,
//!   or arm twice, because it does not arm at all.
//! * **"The controller has been stepped in this zone."** There is no `note_stepped()` call.
//!   `on_frame` derives it from the arguments it is already given — the caller stepped the
//!   controller since the last call exactly when the last call was handed a collision and was told
//!   the caller was stepping. A caller cannot promise a step it did not take.
//!
//! What that does **not** reach is the last statement itself: whether `App::render_frame` runs at
//! all is outside any test in this workspace (that is #728's own premise — it owns a window, a GPU
//! and an event loop). `the_app_rs_call_into_this_module_is_an_unconditional_statement` closes as
//! much of the remaining distance as a source scan can — see its doc for exactly what it does and
//! does not establish, and #799 for the residual.

use crate::movement::{zone_in_reground, CharacterController, Reground};
use crate::nav::collision::Collision;

/// Why the zone-in one-shot stopped being armed.
///
/// **This type is the #728 N1 disclosure.** The `app.rs` shape it replaces had two silent
/// retirements — the `Reground::Retire` arm and the `on_ground` early return were both a bare
/// `self.needs_reground = false;` — so "the reground was not needed" and "the reground was skipped
/// because a flag from another zone said the body was already grounded" were the same observable:
/// nothing at all. That is the shape this project ranks hardest (compare #343, #679): the client is
/// not wrong out loud, it simply does not act, and an agent driving it cannot tell the two apart.
/// Every exit from the armed state now names itself, is logged, and is readable through
/// [`ZoneIn::last_outcome`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ZoneInOutcome {
    /// The arrival had nothing landable below it, so the body was lifted onto the floor at this
    /// height and marked grounded.
    Lifted(f32),
    /// [`zone_in_reground`] measured this zone's collision under the body and returned
    /// [`Reground::Retire`]: there is somewhere landable below it, or it is in water and the swim
    /// branch owns it. Nothing to do.
    ///
    /// This is a verdict **measured against the new zone's geometry**, which is the whole
    /// difference between it and [`Self::SettledInThisZone`].
    NotNeeded,
    /// The controller had already been stepped against THIS zone's collision, reported itself
    /// grounded, and this zone's geometry corroborated that there is something under its feet — so
    /// the arrival is over and the one-shot must not stay armed to fire at some position the
    /// character has since walked to (#720 review, non-blocking C).
    ///
    /// All three conjuncts are load-bearing; see [`ZoneIn::on_frame`] for the frozen-void arrival
    /// that satisfies the first two and none of the meaning.
    SettledInThisZone,
}

impl ZoneInOutcome {
    /// Stable label for logs, and for anything that later mirrors this to an agent.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lifted(_)         => "lifted",
            Self::NotNeeded         => "not_needed",
            Self::SettledInThisZone => "settled_in_this_zone",
        }
    }
}

/// The one-shot reground applied after a zone change, and the state it needs in order to be honest
/// about what it did.
#[derive(Clone, Debug, Default)]
pub struct ZoneIn {
    /// The zone this orchestration is currently following — `app.rs`'s `current_zone`, i.e. the
    /// zone whose geometry it last started loading.
    ///
    /// **This field is what replaced the `arm()` call site.** [`ZoneIn::on_frame`] is handed the
    /// caller's zone name every frame and arms itself the frame it changes, so there is no
    /// separate arm statement in `app.rs` that could be deleted, misplaced, or wrapped in a
    /// never-taken branch. Empty until the first zone loads, which matches `zone_needs_reload`'s
    /// own "an empty name means nothing is loaded" convention — the first real zone name is a
    /// change and arms, exactly as the old explicit call did.
    zone: String,
    /// A zone change has happened and the reground has not yet reached a verdict.
    armed: bool,
    /// The controller has been stepped at least once against THIS zone's collision.
    ///
    /// This is the #728 N1 fix. `CharacterController::on_ground` is a *carried* field: nothing
    /// clears it at a zone change, and the controller is not stepped for the whole asset load, so
    /// on the first post-load frame it still holds the answer computed against the PREVIOUS zone's
    /// geometry. Reading it there is reading another zone's world, and this flag is what stops
    /// that.
    ///
    /// It is a **necessary** condition for reading `on_ground`, not a sufficient one: a step that
    /// takes `depenetrate`'s early return re-derives nothing, so `on_ground` can still be the
    /// previous zone's answer even with this flag set. [`ZoneIn::on_frame`]'s settled arm adds the
    /// geometry check that covers the difference.
    stepped_in_this_zone: bool,
    /// Will the caller step the controller between this [`ZoneIn::on_frame`] call and the next one?
    ///
    /// **This field is what replaced the `note_stepped()` call site.** `app.rs` steps the
    /// controller after `on_frame`, and exactly when two things it already passes in are true: the
    /// camera has been initialised (`stepping`) and a collision is loaded. So the step does not
    /// need to be reported back — it is derivable from this call's own arguments, and read on the
    /// next call. A promise a caller cannot make is a promise a caller cannot break.
    ///
    /// Meaning, precisely: `stepping && collision.is_some()` as of the last call. That is the same
    /// weaker, exactly-true claim `note_stepped` carried — "the controller has been stepped
    /// against THIS zone's collision at least once", never "`on_ground` now describes this zone".
    /// `step`'s depenetration early return can leave `on_ground` untouched and stale, which is why
    /// [`ZoneIn::on_frame`]'s settled arm also corroborates against the geometry.
    stepped_pending: bool,
    /// Why the one-shot last stopped being armed; `None` while armed, and before the first zone
    /// change of the session.
    outcome: Option<ZoneInOutcome>,
}

impl ZoneIn {
    /// `true` while the one-shot is still waiting for a verdict.
    pub fn armed(&self) -> bool { self.armed }

    /// Why the one-shot last stopped being armed. `None` while armed.
    pub fn last_outcome(&self) -> Option<ZoneInOutcome> { self.outcome }

    /// One frame of zone-in orchestration, run by `app.rs` **before** it steps the controller.
    ///
    /// This is the module's whole interface to `app.rs` — see the module doc for why arming and
    /// "has been stepped" are derived here rather than reported by two further call sites.
    ///
    /// * `zone` is the caller's current zone (`App::current_zone`): the zone whose geometry it last
    ///   started loading. **A change in this value is the arm.**
    /// * `loading` is the asset-load throttle (`App::loading`); `collision` is `None` for the whole
    ///   load window.
    /// * `stepping` is whether the caller will step the controller at all this frame given a
    ///   collision (`App::camera_initialized`) — the other half of the derived
    ///   `stepped_in_this_zone`.
    /// * `underworld` comes from the live world state (`OP_NewZone`) rather than from
    ///   `controller.underworld`, because `set_underworld` is only called from the step below,
    ///   which does not run while collision is `None` — so on this first post-load frame the
    ///   controller still holds the PREVIOUS zone's threshold, for exactly the same reason
    ///   `on_ground` does.
    ///
    /// # The #593 arrival, and why `on_ground` cannot be trusted here
    ///
    /// `eqoxide-net`'s `stream_position` hands a server position to the controller as a `teleport`
    /// only when it has moved more than `CORRECTION_SQ` (12 u) from the last position we streamed
    /// — **and that test is two-dimensional**: `cd` is built from `gp[0]` and `gp[1]` only, so a
    /// difference in Z, however large, does not enter it. A cross-zone arrival landing within 12 u
    /// horizontally of where we last were therefore issues **no correction at all**, and `teleport`
    /// — the only thing that clears `on_ground` — never fires. On that path the body keeps the
    /// previous zone's position *and* the previous zone's `on_ground`.
    ///
    /// The old `app.rs` shape read `on_ground` directly on this frame, so on that path the one-shot
    /// retired having measured nothing. See
    /// `the_no_correction_path_leaves_the_previous_zones_on_ground_standing` (the premise) and
    /// `a_stale_on_ground_must_not_retire_the_one_shot_before_it_has_measured_this_zone` (the
    /// consequence).
    pub fn on_frame(
        &mut self,
        controller: &mut CharacterController,
        zone:       &str,
        loading:    bool,
        stepping:   bool,
        collision:  Option<&Collision>,
        underworld: Option<f32>,
    ) {
        // Derived, not promised: the caller steps the controller after this call, iff it is
        // stepping at all and has a collision to step against. Read on the NEXT call, which is
        // where the old `note_stepped()` promise was read from too.
        let stepped_since_last_call =
            std::mem::replace(&mut self.stepped_pending, stepping && collision.is_some());

        if zone != self.zone {
            // The arm. Everything per-zone resets here — `stepped_in_this_zone` above all, since
            // the controller has demonstrably not been stepped against a zone whose name we are
            // learning this very frame. `stepped_since_last_call` is deliberately dropped on the
            // floor: that step, if it happened, was against the zone we are leaving.
            self.zone.clear();
            self.zone.push_str(zone);
            self.armed = true;
            self.stepped_in_this_zone = false;
            self.outcome = None;
            // The previous zone's recovery ring goes with the previous zone (#712, #724, #745).
            //
            // `app.rs` also clears it, in its `zone_needs_reload` block, earlier in this same
            // frame and before anything can step or consult the ring — that call is the one
            // `movement::forget_recovery_history`'s doc calls load-bearing, and it stays. This one
            // is not a second-guessing of it: it is what makes the invariant hold *by
            // construction* rather than by a call site nobody executes in a test. Arming and
            // dropping the old zone's samples are now the same statement and cannot come apart,
            // which is #745's actual content — the round-1 pin on `app.rs`'s call could only see
            // that the call was written. Pinned behaviourally by
            // `arming_for_a_new_zone_drops_the_previous_zones_recovery_ring`.
            controller.forget_recovery_history();
            tracing::info!("zone-in: armed for zone {zone:?}");
        } else if self.armed && stepped_since_last_call {
            self.stepped_in_this_zone = true;
        }

        if !self.armed { return; }
        // The throttle. The new zone's geometry is still arriving; anything decided now would be
        // decided against the wrong world (or, while `App::collision` is `None`, against no world
        // at all). Wait.
        if loading { return; }
        let Some(col) = collision else { return };

        // The previous zone's recovery ring must not survive into this zone (#712, #724, #745).
        //
        // The arm above already dropped it, in the same frame `app.rs` dropped the old collision.
        // This is the per-armed-frame repeat, and it is not redundant — see the paragraph below on
        // re-banking. The ring's samples are untagged coordinates from another
        // zone, and either recovery path in `step` (the #150 fall-through guard, the depenetration
        // stuck fallback) will restore one into THIS zone's coordinate space and ground the body
        // there — a confident wrong answer with a perfectly plausible-looking position, which is
        // exactly what #712 recorded live. Pinned by
        // `a_zone_in_never_grounds_the_body_on_a_previous_zones_recovery_coordinate`.
        //
        // Every armed frame, not once: `depenetrate` banks a fresh good sample whenever
        // `on_ground && good_timer >= GOOD_SAMPLE_SECS`, and `on_ground` on the first stepped frames
        // of a new zone is the PREVIOUS zone's answer (the same staleness #728 N1 is about) — so a
        // single clear can be undone one frame later by a re-bank of the very coordinate it just
        // dropped. While this one-shot is armed the body is by definition not yet confirmed settled
        // here, so no sample taken in that window is worth keeping; the frame the body genuinely
        // lands, the `stepped_in_this_zone && on_ground` arm below retires and the clearing stops.
        controller.forget_recovery_history();

        let p = controller.pos;

        // The arrival is over once the body has come to rest — but ONLY once `on_ground` is this
        // zone's answer, AND only once this zone's geometry corroborates it.
        //
        // `stepped_in_this_zone` is the first half; see the field's doc and the #593 note above.
        // It is not sufficient on its own, which I found by reading `step` rather than by
        // reasoning about it: `step` opens with `if self.depenetrate(dt, col, prev_hold) { return
        // self.pos; }`, and that early return re-derives NOTHING — not the position, not
        // `on_ground`. A body that arrives somewhere `is_embedded` calls embedded (which includes
        // "no floor at all within `GROUND_DEPTH` below", i.e. a void arrival) takes that early
        // return on every frame of the depenetration attempt, so it has been stepped against this
        // zone and STILL carries the previous zone's `on_ground`. Retiring on that would emit
        // `SettledInThisZone` for a body frozen in a void — precisely the confident falsehood
        // (#728's agent-honesty framing) this module exists to remove.
        //
        // So corroborate with the world: re-ask `zone_in_reground` with **no** underworld, which
        // reduces it to "is the body in water, or is there anything at all under its feet in THIS
        // zone" — `Retire` iff yes. Asking the same function rather than probing by hand keeps the
        // probe origin/depth constants in `movement.rs` where they are defined (they are private
        // there, and `movement.rs` is out of scope for this change). The real `underworld` is
        // deliberately NOT passed: a zone whose baked underworld sits at or above its arrival floor
        // makes the real-underworld answer `Wait` for ever even though the body is genuinely
        // standing on geometry — measured, 20 of the 144 arrivals in
        // `the_one_shot_never_leaves_the_armed_state_without_saying_why`'s matrix are that shape.
        if self.stepped_in_this_zone
            && controller.on_ground
            && matches!(zone_in_reground(col, p, None), Reground::Retire)
        {
            self.retire(ZoneInOutcome::SettledInThisZone, p);
            return;
        }

        match zone_in_reground(col, p, underworld) {
            Reground::Lift(f) => {
                controller.teleport([p[0], p[1], f]);
                // `teleport` deliberately leaves the body ungrounded (it is a position
                // discontinuity, and the controller re-derives support on the next step). A `Lift`
                // is the one case where we already know better: the destination IS a floor, we
                // just measured it. `Reground::Lift`'s documented contract is "lift the body onto
                // this floor height AND mark it grounded"; this is that second half.
                controller.on_ground = true;
                tracing::info!(
                    "zone-in: regrounded controller to floor z={:.2} (arrived at z={:.2}; nothing \
                     landable below, underworld {:?})",
                    f, p[2], underworld);
                self.retire(ZoneInOutcome::Lifted(f), controller.pos);
            }
            Reground::Retire => self.retire(ZoneInOutcome::NotNeeded, p),
            Reground::Wait   => {}
        }
    }

    /// Disarm, recording and disclosing why. The single exit from the armed state — see
    /// [`ZoneInOutcome`] for why every exit has to name itself.
    fn retire(&mut self, outcome: ZoneInOutcome, at: [f32; 3]) {
        self.armed = false;
        self.outcome = Some(outcome);
        tracing::info!("zone-in: one-shot reground retired [{}] at {:?}", outcome.as_str(), at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{MeshData, RenderMode, ZoneAssets};
    use crate::movement::{ControllerHoldReason, MoveIntent};

    // ── fixtures ────────────────────────────────────────────────────────────────────────────────

    fn mesh(positions: Vec<[f32; 3]>) -> MeshData {
        MeshData {
            positions, normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }
    /// Floor at height `z` over east [e0,e1] × north [-100,100]. libeq pos = [north, height, east].
    fn floor(z: f32, e0: f32, e1: f32) -> MeshData {
        mesh(vec![[-100.0, z, e0], [100.0, z, e0], [100.0, z, e1], [-100.0, z, e1]])
    }
    fn col(meshes: Vec<MeshData>) -> Collision {
        Collision::build(&ZoneAssets { terrain: meshes, objects: vec![], textures: vec![] }, 4.0)
    }
    fn idle() -> MoveIntent {
        MoveIntent { wish_dir: [0.0, 0.0], wish_vspeed: 0.0, jump: false, want_swim: false,
                     speed: 0.0, climb: 0.0, hop: false }
    }
    /// A single flat floor at `z`, spanning the whole test extent.
    fn flat_at(z: f32) -> Collision { col(vec![floor(z, -100.0, 100.0)]) }

    /// The measured #712 arrival column (lfaydark → steamfont): the arrival floor at −113.25, the
    /// next surface 117 u below at −232.0, and a zone underworld of −222.0 — so the only thing
    /// under a body that arrives just below the arrival floor is somewhere the fall-through guard
    /// refuses to let it rest. Same numbers as `movement.rs`'s `steamfont_arrival_column`,
    /// restated here because that helper is private to that module's test scope.
    fn steamfont_arrival_column() -> Collision {
        col(vec![floor(-113.25, -100.0, 100.0), floor(-232.0, -100.0, 100.0)])
    }
    const STEAMFONT_UNDERWORLD: f32 = -222.0;
    /// The arrival floor's height — where a correctly regrounded body ends up.
    const STEAMFONT_FLOOR: f32 = -113.25;
    /// #712's measured lfaydark z, the previous-zone coordinate the recovery ring carried.
    const LFAYDARK_FLOOR: f32 = -4.78;

    fn assert_lifted(outcome: Option<ZoneInOutcome>, expect: f32) {
        match outcome {
            Some(ZoneInOutcome::Lifted(f)) => assert!((f - expect).abs() < 0.05,
                "expected a lift onto {expect}, got Lifted({f})"),
            other => panic!("expected Lifted({expect}), got {other:?}"),
        }
    }

    // ── the harness ─────────────────────────────────────────────────────────────────────────────

    /// A zone-in, run from arrival through correction to the first grounded frame, with no window,
    /// no GPU and no event loop.
    ///
    /// [`Self::frame`] is the contract: it performs, in `app.rs`'s own order, the two things
    /// `App::update` does with the controller — the one-shot reground first, the step second, and
    /// no step at all on the frames where there is no collision. Every test below drives real
    /// frames through it against a real [`Collision`]; nothing here short-circuits the controller.
    ///
    /// **The harness deliberately does NOT model `app.rs`'s own `forget_recovery_history()` call**
    /// (the `zone_needs_reload` block's). It used to, behind a `clear_ring` knob. It no longer
    /// needs to: [`ZoneIn::on_frame`]'s arm drops the ring itself, so every test below runs the
    /// harsher of the two worlds — the one where that `app.rs` statement is absent or never
    /// executes — and the #745 invariant is asserted against it. That is the round-2 answer to a
    /// pin that could only see the statement was written.
    struct ZoneInHarness {
        ctrl:       CharacterController,
        zone_in:    ZoneIn,
        /// `App::current_zone` — the value whose change is the arm.
        zone:       String,
        collision:  Option<Collision>,
        loading:    bool,
        /// `App::camera_initialized`: whether this frame will step the controller at all.
        stepping:   bool,
        underworld: Option<f32>,
        dt:         f32,
    }

    /// The zone the harness starts in, and the one it changes to. Only the *difference* matters —
    /// `ZoneIn` compares names, it does not interpret them.
    const OLD_ZONE: &str = "lfaydark";
    const NEW_ZONE: &str = "steamfont";

    impl ZoneInHarness {
        /// A character already living in a zone: collision loaded, nothing pending, and the
        /// orchestration already following that zone (so the fixture frames are ordinary in-zone
        /// frames, not an arrival).
        fn in_zone(collision: Collision, underworld: Option<f32>, at: [f32; 3]) -> Self {
            let mut ctrl = CharacterController::new(at);
            ctrl.on_ground = true;
            Self { ctrl,
                   zone_in: ZoneIn { zone: OLD_ZONE.to_string(), ..ZoneIn::default() },
                   zone: OLD_ZONE.to_string(), collision: Some(collision), loading: false,
                   stepping: true, underworld, dt: 1.0 / 60.0 }
        }

        /// One `App::update`: the zone-in one-shot, then the controller step (only when there is a
        /// collision to step against — `app.rs` holds position through the whole load window).
        fn frame(&mut self) {
            self.zone_in.on_frame(&mut self.ctrl, &self.zone, self.loading, self.stepping,
                                  self.collision.as_ref(), self.underworld);
            if self.stepping {
                if let Some(c) = &self.collision {
                    self.ctrl.set_underworld(self.underworld);
                    self.ctrl.step(idle(), self.dt, c);
                }
            }
        }

        fn frames(&mut self, n: usize) { for _ in 0..n { self.frame(); } }

        /// `app.rs`'s `zone_needs_reload` block, minus the parts that are not this module's: the
        /// new zone's name is adopted and the old zone's collision is dropped.
        ///
        /// Note what is NOT here — no `arm()`. The arm happens inside the next
        /// [`ZoneIn::on_frame`], off the name change, which is the whole point (see the struct doc
        /// and the module doc's "wiring" section).
        fn zone_change(&mut self) {
            self.zone.clear();
            self.zone.push_str(NEW_ZONE);
            self.loading = true;
            self.collision = None;
        }

        /// The nav streamer's large-correction hand-over, applied the way `app.rs` applies it.
        /// **Not** called on the #593 sub-12 u arrival — that is the whole point of that path.
        fn correction(&mut self, pos: [f32; 3]) { self.ctrl.teleport(pos); }

        /// The new zone's assets land: collision in, throttle off.
        fn assets_ready(&mut self, collision: Collision, underworld: Option<f32>) {
            self.collision = Some(collision);
            self.underworld = underworld;
            self.loading = false;
        }
    }

    // ── #728 N1: a previous-zone `on_ground` must not retire the one-shot ────────────────────────

    /// **PREMISE for the N1 tests below**, measured rather than assumed: on the #593 no-correction
    /// path the controller really does carry the previous zone's `on_ground` (and position) into
    /// the new zone, and nothing in the load window changes either.
    ///
    /// If this ever goes RED, the N1 tests are exercising a state that can no longer occur, and the
    /// right response is to re-read them rather than to patch this.
    #[test]
    fn the_no_correction_path_leaves_the_previous_zones_on_ground_standing() {
        let mut h = ZoneInHarness::in_zone(flat_at(0.0), Some(-500.0), [0.0, 0.0, 0.0]);
        h.frames(60);
        assert!(h.ctrl.on_ground, "fixture: the body should be standing in the OLD zone");
        let old_pos = h.ctrl.pos;

        h.zone_change();
        h.frames(120); // the asset-load window: no collision, so no step

        assert!(h.ctrl.on_ground,
            "PREMISE: nothing clears `on_ground` at a zone change, and the controller is not \
             stepped during the load — so on the first post-load frame it is still the PREVIOUS \
             zone's answer. `teleport` is the only thing that clears it, and on the #593 sub-12 u \
             arrival no `teleport` fires.");
        assert_eq!(h.ctrl.pos, old_pos,
            "PREMISE: and the position is the previous zone's too — the correction branch that \
             would have replaced it is the same branch that did not fire");
    }

    /// **#728 N1 — the finding.** A cross-zone arrival on the #593 no-correction path, landing
    /// under the new zone's floor with nothing landable below it: the #712 shape, reached without
    /// any `teleport`.
    ///
    /// The old `app.rs` read `controller.on_ground` on the first post-load frame. That flag says
    /// "standing" because the character was standing *in the zone it just left*, so the one-shot
    /// retired having measured nothing, and the body then free-fell past the underworld into the
    /// fall-through guard — see the characterisation test below for where it ends up.
    ///
    /// MUTATION-CHECK: drop the `self.stepped_in_this_zone &&` conjunct from `on_frame`'s early
    /// return (i.e. restore the pre-fix `if controller.on_ground`) → RED.
    #[test]
    fn a_stale_on_ground_must_not_retire_the_one_shot_before_it_has_measured_this_zone() {
        // Standing in the old zone at −114.4 — 1.15 u under what will be the new zone's floor.
        let mut h = ZoneInHarness::in_zone(flat_at(-114.4), Some(-500.0), [0.0, 0.0, -114.4]);
        h.frames(60);
        assert!(h.ctrl.on_ground, "fixture: standing in the old zone");

        // This test is about `on_ground`, not the ring; the arm clears the ring either way.
        h.zone_change();
        h.frames(120);
        // …and NO correction: the arrival is within 12 u horizontally of where we last streamed.
        h.assets_ready(steamfont_arrival_column(), Some(STEAMFONT_UNDERWORLD));

        h.frames(200);

        assert!((h.ctrl.pos[2] - STEAMFONT_FLOOR).abs() < 0.5,
            "#728 N1: the one-shot must measure THIS zone before retiring. It read a previous-zone \
             `on_ground`, retired without acting, and the body fell out of the world instead of \
             being lifted onto the arrival floor at {STEAMFONT_FLOOR}. Ended at {:?}, hold {:?}",
            h.ctrl.pos, h.ctrl.hold());
        assert!(h.ctrl.on_ground, "and it must end STANDING there, not hanging: {:?}", h.ctrl.pos);
        assert_lifted(h.zone_in.last_outcome(), STEAMFONT_FLOOR);
    }

    /// The answer to #728's second open question — *"whether the stale `on_ground` can cause
    /// anything worse than a skipped reground"* — measured on the fixture above by running it with
    /// the pre-fix rule and looking at where the body actually ends up.
    ///
    /// It can. The skipped reground is not the end state: the body free-falls, passes the
    /// underworld, and the #150 fall-through guard stops it with an empty recovery ring — a
    /// `UnderworldNoRecovery` hold, which is terminal (nothing generates a second server
    /// correction; see the retraction in `CharacterController::teleport`'s comment). The character
    /// ends ~108 u below the floor it should be standing on and cannot get back.
    ///
    /// The pre-fix retirement is reproduced by disarming the one-shot at the moment the assets
    /// land, which is precisely what the stale flag did. This is a characterisation of the defect,
    /// and it is what makes the paragraph above measured rather than reasoned. The one thing the
    /// old shape had going for it is asserted too: the hold is *disclosed*.
    #[test]
    fn a_skipped_reground_ends_in_a_terminal_underworld_hold_not_merely_a_missed_lift() {
        let mut h = ZoneInHarness::in_zone(flat_at(-114.4), Some(-500.0), [0.0, 0.0, -114.4]);
        h.frames(60);

        h.zone_change();
        h.frames(120);
        h.assets_ready(steamfont_arrival_column(), Some(STEAMFONT_UNDERWORLD));
        // The pre-fix behaviour, reproduced exactly: the one-shot retires on the stale flag before
        // it measures anything. Disarming it here is that retirement — and it has to be a disarm
        // of the CURRENT zone, not a `ZoneIn::default()`, because the orchestration now re-arms off
        // a zone-name change and a default's empty name would look like one.
        h.zone_in.armed = false;

        h.frames(400);

        assert!(h.ctrl.pos[2] > STEAMFONT_UNDERWORLD && h.ctrl.pos[2] < STEAMFONT_UNDERWORLD + 8.0,
            "the skipped reground does not leave the body where it arrived — it falls to the \
             underworld: {:?}", h.ctrl.pos);
        assert!(!h.ctrl.on_ground, "and it is not standing on anything: {:?}", h.ctrl.pos);
        let hold = h.ctrl.hold().expect(
            "the guard that stopped it must report a hold — that disclosure is the one honest \
             thing about the pre-fix path");
        assert_eq!(hold.reason, ControllerHoldReason::UnderworldNoRecovery);
        assert!((h.ctrl.pos[2] - STEAMFONT_FLOOR).abs() > 100.0,
            "…and it is ~108 u below the floor it should be standing on, so 'worse than a skipped \
             reground' is the right reading: {:?}", h.ctrl.pos);
    }

    /// The #720-review-C behaviour, and the other half of N1: once the body really has settled in
    /// THIS zone the one-shot retires, rather than staying armed to fire at some position the
    /// character has since walked to.
    ///
    /// This is the one arrival shape where the two halves are visible on consecutive frames, and it
    /// is the shape that makes `stepped_in_this_zone` a distinction rather than a formality. The
    /// body arrives on the #593 path — no correction, so it carries the previous zone's `on_ground`
    /// — onto a floor at exactly its own height, in a zone whose underworld is baked at that floor.
    /// `zone_in_reground` can then neither retire (`ground_below` finds the floor, but it is not
    /// *above* the underworld) nor lift (`f > p[2]` is false), so it answers `Wait` for ever, and
    /// the ONLY thing that ends the arrival is the settled arm.
    ///
    /// * **Frame 1** — `on_ground` is still the previous zone's answer. The one-shot must not use
    ///   it. It stays armed; the step then re-derives support from THIS zone's geometry (the ground
    ///   clamp finds the floor within `GROUND_SNAP_TOL` and keeps the body standing).
    /// * **Frame 2** — the flag has been earned. The arrival is over, and the outcome says why.
    ///
    /// MUTATION-CHECK: drop the `stepped_in_this_zone &&` conjunct → frame 1 retires → RED on the
    /// first assertion.
    #[test]
    fn the_arrival_is_over_once_the_body_has_settled_in_this_zone() {
        let mut h = ZoneInHarness::in_zone(flat_at(0.0), Some(-500.0), [0.0, 0.0, 0.0]);
        h.frames(30);
        assert!(h.ctrl.on_ground, "fixture: standing in the old zone");

        h.zone_change();
        h.frames(30);
        // Same floor height, and an underworld baked AT that floor — see the doc above for why this
        // is the shape that isolates the settled arm.
        h.assets_ready(flat_at(0.0), Some(0.0));

        h.frame();
        assert!(h.zone_in.armed(),
            "frame 1: `on_ground` had not yet been re-derived against this zone, so the one-shot \
             must not have read it (#728 N1)");
        assert_eq!(h.zone_in.last_outcome(), None);
        assert!(h.ctrl.on_ground,
            "fixture: the step must leave the body standing, or frame 2 tests nothing: {:?}",
            h.ctrl.pos);

        h.frame();
        assert_eq!(h.zone_in.last_outcome(), Some(ZoneInOutcome::SettledInThisZone),
            "frame 2: the flag has been earned — a body that has settled in THIS zone ends the \
             arrival, and says that is why");
        assert!(!h.zone_in.armed());
    }

    /// **The derivation that replaced `note_stepped()`.** `stepped_in_this_zone` used to be set by
    /// a call `app.rs` made from inside the arm that steps; it is now derived here from the two
    /// facts that *define* that arm — `camera_initialized` and "there is a collision" — both of
    /// which `on_frame` is already handed. A derived fact cannot be a call site someone deletes,
    /// and #791's round-2 review is the reason that matters: the source-text pin that guarded the
    /// old call could see it was written, not that it ran.
    ///
    /// This pins the `stepping` half. `app.rs` does not step the controller before the camera is
    /// initialised, so on those frames `on_ground` is untouched — still the previous zone's answer,
    /// the #728 N1 staleness — and the one-shot must not treat the arrival as complete.
    ///
    /// The fixture is `the_arrival_is_over_once_the_body_has_settled_in_this_zone`'s: a zone whose
    /// underworld is baked at its floor, so `zone_in_reground` can neither lift nor retire and the
    /// settled arm is the ONLY exit. Whether that exit is taken therefore reports exactly whether
    /// the flag was earned.
    ///
    /// MUTATION-CHECK: drop the `stepping &&` conjunct (i.e. derive the flag from the collision
    /// alone) → the one-shot retires `SettledInThisZone` for a body nothing has stepped → RED.
    #[test]
    fn a_frame_the_caller_does_not_step_cannot_earn_the_stepped_flag() {
        let mut h = ZoneInHarness::in_zone(flat_at(0.0), Some(-500.0), [0.0, 0.0, 0.0]);
        h.frames(30);
        assert!(h.ctrl.on_ground, "fixture: standing in the old zone");

        h.zone_change();
        h.frames(30);
        h.assets_ready(flat_at(0.0), Some(0.0));
        // The camera has not been initialised in the new zone yet: `app.rs` holds position and does
        // not step, so nothing has re-derived `on_ground` against this zone's geometry.
        h.stepping = false;

        h.frames(100);

        assert!(h.ctrl.on_ground,
            "fixture: `on_ground` is still the OLD zone's answer — nothing has stepped the body");
        assert!(h.zone_in.armed(),
            "#728 N1: no frame here stepped the controller, so the body cannot have settled in \
             THIS zone. Retiring on an unstepped frame would tell the agent the arrival is \
             complete on the strength of a flag another zone set.");
        assert_eq!(h.zone_in.last_outcome(), None);

        // The contrast, on the same fixture: stepping is the only thing that was missing.
        h.stepping = true;
        h.frames(2);
        assert_eq!(h.zone_in.last_outcome(), Some(ZoneInOutcome::SettledInThisZone),
            "…and once the caller does step, the flag is earned without anyone announcing it");
    }

    // ── #728 N2: the `app.rs` half — the throttle, the early return, the `on_ground = true` ──────

    /// The throttle. While the new zone's assets are still loading, the one-shot must not act —
    /// whatever collision is in hand describes a world we are leaving or have not arrived in.
    ///
    /// In `app.rs` today the collision is *also* `None` across that window, so the throttle is
    /// belt-and-braces there; this pins the throttle itself, so that a refactor which stops nulling
    /// the collision cannot silently lose it.
    ///
    /// MUTATION-CHECK: delete `if loading { return; }` from `on_frame` → RED.
    #[test]
    fn the_one_shot_does_not_fire_while_the_new_zones_assets_are_still_loading() {
        let mut h = ZoneInHarness::in_zone(flat_at(-114.4), Some(-500.0), [0.0, 0.0, -114.4]);
        h.frames(30);
        h.zone_change();
        // The load has not finished, but a collision IS in hand — and it is the one this arrival
        // would be lifted onto, so a missing throttle is visible rather than merely unproven.
        h.collision = Some(steamfont_arrival_column());
        h.underworld = Some(STEAMFONT_UNDERWORLD);

        h.frames(10);

        assert!(h.zone_in.armed(),
            "the one-shot must still be armed: the zone it would decide against has not loaded");
        assert_eq!(h.zone_in.last_outcome(), None, "and it must not have recorded a verdict");
        // Not "the body did not move" — the harness steps the controller on these frames (that is
        // what `App` does whenever a collision is in hand), and an arrival 1.15 u under a floor it
        // cannot see falls. What must NOT have happened is the LIFT: the throttle's whole job is
        // that the one-shot did not teleport the body onto -113.25 and retire.
        assert!(h.ctrl.pos[2] < -114.0,
            "the one-shot lifted the body while the zone was still loading: {:?}", h.ctrl.pos);
        assert!(!h.ctrl.on_ground, "…and marked it grounded: {:?}", h.ctrl.pos);

        // The contrast, on the same fixture: the throttle is what is holding it, and nothing else.
        h.loading = false;
        h.frame();
        assert_lifted(h.zone_in.last_outcome(), STEAMFONT_FLOOR);
    }

    /// A `Lift` must mark the body grounded. `teleport` deliberately clears `on_ground` (a position
    /// discontinuity is not a landing), so without the explicit re-assert the body spends the frame
    /// airborne at its own floor height, and `Reground::Lift`'s documented contract — "lift the
    /// body onto this floor height **and mark it grounded**" — is only half kept.
    ///
    /// MUTATION-CHECK: delete `controller.on_ground = true;` from the `Lift` arm → RED.
    #[test]
    fn a_lift_marks_the_body_grounded() {
        let mut h = ZoneInHarness::in_zone(flat_at(0.0), Some(-500.0), [0.0, 0.0, 0.0]);
        h.frames(30);
        h.zone_change();
        h.frames(30);
        h.assets_ready(steamfont_arrival_column(), Some(STEAMFONT_UNDERWORLD));
        h.correction([0.0, 0.0, -114.4]); // the measured #712 arrival: 1.15 u under the floor
        assert!(!h.ctrl.on_ground, "fixture: a correction leaves the body ungrounded");

        // The one-shot ONLY, without the step that follows it: the assignment under test is the
        // orchestration's own claim about the body, not something the step re-derives.
        h.zone_in.on_frame(&mut h.ctrl, &h.zone, h.loading, h.stepping, h.collision.as_ref(),
                           h.underworld);

        assert_lifted(h.zone_in.last_outcome(), STEAMFONT_FLOOR);
        assert!(h.ctrl.on_ground,
            "#728 N2: a lifted body is standing on the floor we just measured — the one-shot must \
             say so, because `teleport` cleared the flag on the way in. Pos {:?}", h.ctrl.pos);
    }

    /// The `Wait` arm: nothing legal to stand on anywhere, so the one-shot stays armed and looks
    /// again next frame rather than retiring on a non-answer.
    #[test]
    fn the_one_shot_stays_armed_while_there_is_nowhere_legal_to_stand() {
        // One floor, itself BELOW the underworld: nothing landable below, nothing landable above.
        let mut h = ZoneInHarness::in_zone(flat_at(0.0), Some(-500.0), [0.0, 0.0, 0.0]);
        h.frames(30);
        h.zone_change();
        h.frames(30);
        h.assets_ready(flat_at(-232.0), Some(STEAMFONT_UNDERWORLD));
        h.correction([0.0, 0.0, -114.4]);

        h.frame();

        assert!(h.zone_in.armed(),
            "nothing legal to land on → the one-shot must stay armed, not retire quietly");
        assert_eq!(h.zone_in.last_outcome(), None);
    }

    // ── #728 N1, the honesty half ───────────────────────────────────────────────────────────────

    /// **The agent-honesty invariant, over every arrival this module can reach.** The one-shot may
    /// not leave the armed state without saying why.
    ///
    /// This is the shape #728 ranks hardest, and the reason it was filed rather than noted: a
    /// one-shot that silently retires is, from the driving agent's side, indistinguishable between
    /// "regrounding was not needed" and "regrounding was skipped". Both used to be a bare
    /// `self.needs_reground = false;`.
    ///
    /// The trailing assertions are a **coverage** claim, not decoration: an invariant of the form
    /// "no exit is silent" is worth exactly as much as the number of exits the matrix actually
    /// takes, so the matrix has to demonstrate it reaches all three. It does — measured, not
    /// reasoned. (A first draft of this test asserted the opposite about `SettledInThisZone`, on a
    /// structural argument that `zone_in_reground`'s retire probe and `step`'s ground clamp are the
    /// same call and so a `NotNeeded` must always precede any landing. The matrix falsified it on
    /// the first run — 20 of 144 — because that argument silently assumed the retire probe's
    /// `f > underworld` conjunct always holds, and it does not when the zone's underworld is baked
    /// at or above the arrival floor. The claim is recorded here as retracted rather than quietly
    /// replaced.)
    ///
    /// MUTATION-CHECK: replace either `self.retire(...)` call with a bare `self.armed = false;` →
    /// RED.
    #[test]
    fn the_one_shot_never_leaves_the_armed_state_without_saying_why() {
        // Arrival heights spanning: above the floor, on it, well above the lower slab, just under
        // the arrival floor (the #712 shape), far below it, and below the underworld.
        let arrivals = [40.0f32, 0.0, -100.0, -113.25, -114.4, -150.0, -221.0, -230.0, -300.0];
        // Underworlds spanning: unknown, above everything, the measured value, below everything.
        let underworlds = [None, Some(0.0), Some(STEAMFONT_UNDERWORLD), Some(-500.0)];
        let mut runs = 0;
        let (mut lifted, mut not_needed, mut settled, mut still_armed) = (0, 0, 0, 0);
        for zone in [steamfont_arrival_column as fn() -> Collision, || flat_at(0.0)] {
            for &z in &arrivals {
                for &uw in &underworlds {
                    // Both arrival paths: a large server correction, and the #593 path where the
                    // body simply carries its previous-zone position across.
                    for corrected in [true, false] {
                        runs += 1;
                        let mut h = ZoneInHarness::in_zone(flat_at(z), Some(-500.0),
                                                           [0.0, 0.0, z]);
                        h.frames(20);
                        h.zone_change();
                        h.frames(20);
                        h.assets_ready(zone(), uw);
                        if corrected { h.correction([0.0, 0.0, z]); }

                        h.frames(400);

                        if h.zone_in.armed() {
                            still_armed += 1;
                            assert_eq!(h.zone_in.last_outcome(), None,
                                "an ARMED one-shot must not be carrying a verdict (z={z}, \
                                 uw={uw:?}, corrected={corrected})");
                            continue;
                        }
                        match h.zone_in.last_outcome() {
                            Some(ZoneInOutcome::Lifted(_))         => lifted += 1,
                            Some(ZoneInOutcome::NotNeeded)         => not_needed += 1,
                            Some(ZoneInOutcome::SettledInThisZone) => settled += 1,
                            None => panic!(
                                "the one-shot retired without recording why (z={z}, uw={uw:?}, \
                                 corrected={corrected}) — that is the silent retirement #728 is \
                                 about: an agent cannot tell it apart from 'not needed'"),
                        }
                    }
                }
            }
        }
        assert_eq!(runs, 144, "fixture: the matrix should be 2 zones × 9 arrivals × 4 underworlds × 2 paths");
        // Coverage: "no exit is silent" is only worth the exits the matrix takes. All three are
        // taken, and the still-armed column is real too (arrivals with nothing legal anywhere).
        assert!(lifted > 0 && not_needed > 0 && settled > 0,
            "the matrix must exercise every retirement reason, or the invariant above is asserted \
             over exits nothing reaches — lifted={lifted} not_needed={not_needed} \
             settled={settled} still_armed={still_armed} of {runs}");
    }

    // ── #745: the zone-change recovery-ring clear ───────────────────────────────────────────────

    /// **#745, behaviourally.** A zone-in must never ground the body on a coordinate banked in the
    /// zone we left — **even with `app.rs`'s own `forget_recovery_history()` call absent.**
    ///
    /// #745's content is that deleting `src/app.rs`'s call left the suite green, and that it would
    /// be easy to delete it as "redundant with `teleport`", because `movement.rs` documents a
    /// cross-zone arrival (the #593 sub-12 u path) where no `teleport` fires. This test runs
    /// precisely that arrival — and the harness never makes that `app.rs` call at all, so the run
    /// is the world in which the statement is deleted — into a column where the body has nowhere
    /// legal to land, which is the one shape that actually makes a recovery path *read* the ring.
    ///
    /// The two outcomes are the reason this is an agent-honesty test and not a physics one:
    ///
    /// * ring cleared → the guard has nothing to restore, the body **holds**, and `player.hold`
    ///   says so. A reported failure.
    /// * ring intact → the guard grounds the body on the previous zone's coordinate. `on_ground`
    ///   is `true`, no hold is reported, and the position is perfectly plausible. A confident wrong
    ///   answer — #712's live record.
    ///
    /// MUTATION-CHECK: delete BOTH `controller.forget_recovery_history();` calls in `on_frame` (the
    /// arm's and the per-armed-frame backstop's) → RED on the per-frame assertion. Deleting only
    /// one of them is covered separately: the arm's by
    /// `arming_for_a_new_zone_drops_the_previous_zones_recovery_ring`, the backstop's by
    /// `an_armed_frame_denies_the_recovery_ring_the_previous_zones_coordinate`.
    ///
    /// What this does NOT establish: it says nothing about `app.rs`'s own call site (which runs
    /// earlier, when the old collision is dropped, and remains pinned textually by `movement.rs`'s
    /// `the_zone_change_reload_block_still_forgets_the_recovery_ring`). It does not need to — this
    /// runs in the world where that statement is gone, and the invariant holds anyway.
    #[test]
    fn a_zone_in_never_grounds_the_body_on_a_previous_zones_recovery_coordinate() {
        // Bank a ring in the previous zone. #712's measured lfaydark floor.
        let mut h = ZoneInHarness::in_zone(flat_at(LFAYDARK_FLOOR), Some(-500.0),
                                           [-80.0, 0.0, LFAYDARK_FLOOR]);
        h.frames(300); // several GOOD_SAMPLE_SECS (0.5 s) banking intervals
        assert!(h.ctrl.on_ground, "fixture: grounded, so the ring actually banks");
        let stale = h.ctrl.pos;

        // The zone change WITHOUT `app.rs`'s clear — the harness never makes that call, which is
        // the deletion #745 warns about — and without a correction, which is the #593 arrival where
        // nothing else clears the ring either.
        h.zone_change();
        h.frames(120);
        // The new zone: a floor the body can SEE (so the depenetration net does not classify it as
        // embedded — `is_embedded` counts "nothing within GROUND_DEPTH below" as embedded, and that
        // freezes the step before the fall-through guard is ever reached) but cannot LAND on,
        // because it lies below the zone's underworld. That is #712's measured shape — steamfont's
        // arrival column had its next surface 117 u down and 10 u below the underworld — at a
        // separation this arrival height can reach; the two heights themselves are constructed.
        h.assets_ready(flat_at(-150.0), Some(-140.0));

        for i in 0..400 {
            h.frame();
            let d = [h.ctrl.pos[0] - stale[0], h.ctrl.pos[1] - stale[1], h.ctrl.pos[2] - stale[2]];
            let at_stale = d[0].abs() + d[1].abs() + d[2].abs() < 0.5;
            assert!(!(h.ctrl.on_ground && at_stale),
                "#712/#724/#745 (frame {i}): the zone-in grounded the body on the PREVIOUS zone's \
                 coordinate {stale:?} — the recovery ring survived the change. `on_ground` is true \
                 and no hold is reported, so the client is telling its agent the character is \
                 standing somewhere it has never been. Pos {:?}, hold {:?}",
                h.ctrl.pos, h.ctrl.hold());
        }

        assert!(!h.ctrl.on_ground,
            "the honest end state is a body the guard is holding, not a grounded one: {:?}",
            h.ctrl.pos);
        let hold = h.ctrl.hold().expect(
            "…and the hold must be REPORTED — a failure the agent can read is the whole point of \
             clearing the ring (#724)");
        assert_eq!(hold.reason, ControllerHoldReason::UnderworldNoRecovery);
    }

    /// The **arm's** ring clear, isolated from the per-armed-frame backstop.
    ///
    /// The test above passes with either clear present, so on its own it would let one of them be
    /// deleted silently — the exact failure mode #745 is about, one level in. This one runs the
    /// window where the backstop *cannot* run: the new zone's collision has arrived but `loading`
    /// is still true, so `on_frame` returns at the throttle. Everything that protects the body here
    /// is the clear that happens in the same statement as the arm.
    ///
    /// That is why the clear lives on the arm rather than next to it: arming and dropping the
    /// previous zone's samples cannot come apart, because they are one branch.
    ///
    /// MUTATION-CHECK: delete `controller.forget_recovery_history();` from the ARM branch (leaving
    /// the backstop) → RED here, and green in every other test in this file.
    #[test]
    fn arming_for_a_new_zone_drops_the_previous_zones_recovery_ring() {
        let mut h = ZoneInHarness::in_zone(flat_at(LFAYDARK_FLOOR), Some(-500.0),
                                           [-80.0, 0.0, LFAYDARK_FLOOR]);
        h.frames(300);
        assert!(h.ctrl.on_ground, "fixture: grounded, so the ring actually banks");
        let stale = h.ctrl.pos;

        h.zone_change();
        // The new zone's geometry, handed over while the load is STILL in progress. `app.rs` does
        // not do this today (it nulls the collision for the whole window), and that is the point:
        // the invariant must not rest on that incidental fact. Same shape as the test above — a
        // floor the body can see but not land on, below the underworld.
        h.collision  = Some(flat_at(-150.0));
        h.underworld = Some(-140.0);
        assert!(h.loading, "fixture: `loading` must stay set, or the backstop runs and this test \
                            stops isolating the arm");

        for i in 0..400 {
            h.frame();
            let d = [h.ctrl.pos[0] - stale[0], h.ctrl.pos[1] - stale[1], h.ctrl.pos[2] - stale[2]];
            let at_stale = d[0].abs() + d[1].abs() + d[2].abs() < 0.5;
            assert!(!(h.ctrl.on_ground && at_stale),
                "#745 (frame {i}): the ARM did not drop the previous zone's recovery ring, and the \
                 fall-through guard has grounded the body on {stale:?} — a coordinate from a zone \
                 it has left. Pos {:?}, hold {:?}", h.ctrl.pos, h.ctrl.hold());
        }
        assert!(!h.ctrl.on_ground, "the honest end state is a held body: {:?}", h.ctrl.pos);
        assert_eq!(h.ctrl.hold().expect("…and the hold must be reported").reason,
                   ControllerHoldReason::UnderworldNoRecovery);
    }

    /// The **per-armed-frame backstop**, isolated from the arm's clear.
    ///
    /// The arm clears the ring once. `depenetrate` can put a sample straight back: it banks
    /// `self.pos` whenever `on_ground && good_timer >= GOOD_SAMPLE_SECS && !is_embedded`, and on
    /// the first stepped frames of a new zone `on_ground` is still the PREVIOUS zone's answer
    /// (that is #728 N1, and `the_no_correction_path_leaves_the_previous_zones_on_ground_standing`
    /// measures it). So the very coordinate the arm just dropped can be re-banked one frame later,
    /// by a body that has not landed anywhere. Clearing once is not enough; while the one-shot is
    /// armed the body is by definition not confirmed settled here, so no sample taken in that
    /// window is worth keeping.
    ///
    /// The long first frame is what makes the re-bank certain rather than phase-dependent:
    /// `good_timer` carries an arbitrary sub-`GOOD_SAMPLE_SECS` remainder out of the old zone, and
    /// a single `dt` of 0.6 s crosses the threshold from any remainder. `MAX_FALL` (128 u/s) caps
    /// what that costs in altitude at ~77 u, which is well short of the underworld — so the bank
    /// happens on that frame and the guard is not reached until several ordinary frames later,
    /// which is what leaves the backstop something to do.
    ///
    /// MUTATION-CHECK: delete the per-armed-frame `controller.forget_recovery_history();` (leaving
    /// the arm's) → RED here, and green in every other test in this file.
    #[test]
    fn an_armed_frame_denies_the_recovery_ring_the_previous_zones_coordinate() {
        let mut h = ZoneInHarness::in_zone(flat_at(LFAYDARK_FLOOR), Some(-500.0),
                                           [-80.0, 0.0, LFAYDARK_FLOOR]);
        h.frames(300);
        assert!(h.ctrl.on_ground, "fixture: grounded, so the ring actually banks");
        let stale = h.ctrl.pos;

        h.zone_change();
        h.frames(120);                                   // the load window: armed, nothing stepped
        assert!(h.ctrl.on_ground,
            "fixture: the #593 path carries the OLD zone's `on_ground` in — that stale flag is \
             what lets `depenetrate` re-bank a coordinate the body is not standing on");
        h.assets_ready(flat_at(-150.0), Some(-140.0));

        h.dt = 0.6;   // one long frame → `good_timer` crosses GOOD_SAMPLE_SECS whatever it was
        h.frame();    // …and the stale `on_ground` banks the stale coordinate, after the arm
        h.dt = 1.0 / 60.0;
        assert!(!h.ctrl.on_ground && h.ctrl.pos[2] > -140.0,
            "fixture: that frame must leave the body falling and still ABOVE the underworld, or \
             the guard fires inside it and the backstop has nothing left to protect: {:?}",
            h.ctrl.pos);

        for i in 0..400 {
            h.frame();
            let d = [h.ctrl.pos[0] - stale[0], h.ctrl.pos[1] - stale[1], h.ctrl.pos[2] - stale[2]];
            let at_stale = d[0].abs() + d[1].abs() + d[2].abs() < 0.5;
            assert!(!(h.ctrl.on_ground && at_stale),
                "#745 (frame {i}): a sample re-banked during the armed window survived, and the \
                 fall-through guard has grounded the body on the previous zone's {stale:?}. \
                 Clearing the ring once, at the arm, is not enough. Pos {:?}, hold {:?}",
                h.ctrl.pos, h.ctrl.hold());
        }
        assert!(!h.ctrl.on_ground, "the honest end state is a held body: {:?}", h.ctrl.pos);
        assert_eq!(h.ctrl.hold().expect("…and the hold must be reported").reason,
                   ControllerHoldReason::UnderworldNoRecovery);
    }

    /// The **second** terminal shape a skipped reground can end in, and the one that nearly got
    /// past me: a *void* arrival, where the new zone has nothing at all within `GROUND_DEPTH`
    /// (200 u) below the body.
    ///
    /// This is not the fall-through guard's story. `is_embedded` counts "no floor within 200 u
    /// below" as embedded, so `step` never reaches the guard at all — it takes `depenetrate`'s
    /// early return (`if self.depenetrate(dt, col, prev_hold) { return self.pos; }`), which
    /// re-derives **nothing**: not the position, not `on_ground`. The push-out cannot help either,
    /// because `Recovery::at_column` looks for a floor in the same 200 u band.
    ///
    /// Two things fall out, and both are measured below rather than argued:
    ///
    /// 1. **A disclosure gap.** For the first `STUCK_FALLBACK_SECS` (0.5 s) the client reports a
    ///    body that is `on_ground` at the PREVIOUS zone's coordinate with **no hold** — a confident
    ///    wrong answer with no error attached, for ~30 frames. Only after that does
    ///    `EmbeddedNoRecovery` appear. This is characterisation, not a fix: closing it means
    ///    editing `movement.rs`, which is out of scope for this change. Filed here so the next
    ///    person finds it measured rather than having to re-derive it.
    ///
    /// 2. **Why `ZoneIn`'s settled arm corroborates against the geometry.** `stepped_in_this_zone`
    ///    is true here — `app.rs` really did step the controller against this zone — and
    ///    `on_ground` is true, so a settled arm resting on those two alone would retire the
    ///    one-shot with `SettledInThisZone`. That would be the client telling its agent "your
    ///    arrival is complete, you are standing" about a body frozen in a void at another zone's
    ///    coordinate. The geometry check is what refuses it, and this test is what holds that
    ///    check in place — see the `M`-mutant that deletes it.
    #[test]
    fn a_void_arrival_freezes_the_body_and_must_not_be_mistaken_for_having_settled() {
        let mut h = ZoneInHarness::in_zone(flat_at(LFAYDARK_FLOOR), Some(-500.0),
                                           [-80.0, 0.0, LFAYDARK_FLOOR]);
        h.frames(300);
        assert!(h.ctrl.on_ground, "fixture: standing in the OLD zone");
        let stale = h.ctrl.pos;

        // #593 arrival: no correction, so position and `on_ground` both survive. The arm clears the
        // ring — this test is not about the ring, it is about the frozen step, and an empty ring is
        // what makes the freeze rather than a fallback.
        h.zone_change();
        h.frames(60);
        // The new zone's only surface is 395 u down: outside `GROUND_DEPTH`, so from the body's
        // position this column reads as empty space in every direction the net can probe.
        h.assets_ready(flat_at(-400.0), Some(-500.0));

        // (1) The disclosure gap, measured. 20 frames ≈ 0.33 s, inside STUCK_FALLBACK_SECS.
        h.frames(20);
        assert_eq!(h.ctrl.pos, stale,
            "premise: the body is frozen — `depenetrate` returned true and `step` early-returned");
        assert!(h.ctrl.on_ground,
            "premise: and the early return left the PREVIOUS zone's `on_ground` untouched");
        assert!(h.ctrl.hold().is_none(),
            "MEASUREMENT (movement.rs, out of scope here): for the first STUCK_FALLBACK_SECS the \
             client reports standing-and-fine at another zone's coordinate, with no hold. If this \
             ever goes RED because a hold now appears immediately, that gap has been closed — \
             delete this assertion and say so.");

        // (2) …and the one-shot must NOT have called that a settled arrival.
        assert_ne!(h.zone_in.last_outcome(), Some(ZoneInOutcome::SettledInThisZone),
            "#728 agent-honesty: `stepped_in_this_zone && on_ground` are both true here and both \
             mean nothing — the body is frozen in a void carrying another zone's flag. Retiring on \
             them would report a completed arrival that never happened. The settled arm must \
             corroborate against THIS zone's geometry.");
        assert!(h.zone_in.armed(),
            "the honest state is still-armed: the one-shot has not been able to measure anything \
             yet, and says so by not answering. Outcome was {:?}", h.zone_in.last_outcome());

        // Past the fallback deadline the controller does eventually disclose, on its own channel.
        h.frames(120);
        let hold = h.ctrl.hold().expect(
            "after STUCK_FALLBACK_SECS with an empty ring, `depenetrate` must report \
             EmbeddedNoRecovery (#724 review B1)");
        assert_eq!(hold.reason, ControllerHoldReason::EmbeddedNoRecovery);
        assert_ne!(h.zone_in.last_outcome(), Some(ZoneInOutcome::SettledInThisZone),
            "and it must still not claim the arrival settled");
    }

    // ── #728's first open question: can the two paths co-occur? ──────────────────────────────────

    /// #728 makes this a gate: *"whether the #593 sub-12 u no-correction path can actually co-occur
    /// with a below-floor arrival in a real zone, or whether the two are mutually exclusive in
    /// practice. … If they are mutually exclusive, N1 is theoretical."*
    ///
    /// They are **not** mutually exclusive, and the reason needs no zone data: the two conditions
    /// share no variable.
    ///
    /// * The no-correction path is `cd[0]² + cd[1]² <= CORRECTION_SQ` in `stream_position`, where
    ///   `cd` is built from `gp[0]` and `gp[1]` **only**. It is a two-dimensional test; Z does not
    ///   appear in it, so no value of Z can make it false. `action_loop.rs`'s own #593 note says
    ///   exactly this ("the check is 2D (`cd` omits `dz`), so a stale Z is not bounded by it").
    /// * "Below-floor arrival" is a statement about Z and about the new zone's geometry in that
    ///   column — `zone_in_reground` returning `Lift`.
    ///
    /// So the horizontal gate cannot constrain the vertical condition. This test exhibits a pair
    /// satisfying both at once, and then sweeps the whole inside of the gate to show the vertical
    /// verdict never changes across it.
    ///
    /// **What this does NOT establish, stated plainly:** that a *particular real zone pair* puts
    /// its arrival within 12 u horizontally of a departure point while also landing under the
    /// arrival floor. That needs zone-point data or a live crossing, and neither was done here.
    /// What is established is that nothing in the client makes the combination impossible — which
    /// is what "mutually exclusive in practice" would have required.
    #[test]
    fn the_no_correction_path_and_a_below_floor_arrival_are_not_mutually_exclusive() {
        // Transcribed from `stream_position`; `correction_sq_is_still_the_2d_12u_gate` pins it.
        const CORRECTION_SQ: f32 = 144.0;

        // Where we last streamed in the OLD zone, and where the body sits in the NEW one. On this
        // path they are the same point: no correction means no position hand-over at all.
        let last_streamed = [-80.0f32, 0.0, -114.4];
        let arrival = last_streamed;
        let cd = [arrival[0] - last_streamed[0], arrival[1] - last_streamed[1]];
        assert!(cd[0] * cd[0] + cd[1] * cd[1] <= CORRECTION_SQ,
            "condition A: the correction branch is SKIPPED — no `teleport`, so `on_ground` and the \
             recovery ring both survive the change");

        let zone = steamfont_arrival_column();
        let verdict = zone_in_reground(&zone, arrival, Some(STEAMFONT_UNDERWORLD));
        assert!(matches!(verdict, Reground::Lift(_)),
            "condition B: the same position IS a below-floor arrival — got {verdict:?}");

        // And a sweep across the inside of the gate, to show A does not narrow B at all.
        let mut inside = 0;
        for i in -24..=24 {
            for j in -24..=24 {
                let (de, dn) = (i as f32 * 0.5, j as f32 * 0.5);
                if de * de + dn * dn > CORRECTION_SQ { continue; }
                inside += 1;
                let p = [last_streamed[0] + de, last_streamed[1] + dn, last_streamed[2]];
                assert!(matches!(zone_in_reground(&zone, p, Some(STEAMFONT_UNDERWORLD)),
                                 Reground::Lift(_)),
                    "the horizontal gate does not constrain the vertical condition: {p:?}");
            }
        }
        assert!(inside > 400, "fixture: the sweep should cover the gate ({inside} samples)");
    }

    /// The gate the test above transcribes, pinned at its source so the model cannot drift
    /// silently.
    ///
    /// If this reds: `stream_position` in `crates/eqoxide-net/src/action_loop.rs` changed. Update
    /// the literal in `the_no_correction_path_and_a_below_floor_arrival_are_not_mutually_exclusive`
    /// to match and re-read that test's argument — its conclusion rests on the *2-D-ness*, not on
    /// the magnitude, but a change here is the right moment to check both. Do not delete either
    /// test: the co-occurrence question is #728's own gate.
    #[test]
    fn correction_sq_is_still_the_2d_12u_gate() {
        const ACTION_LOOP: &str = include_str!("../crates/eqoxide-net/src/action_loop.rs");
        assert!(ACTION_LOOP.contains("const CORRECTION_SQ: f32 = 144.0;"),
            "the server-correction gate this module models has changed value or spelling");
        assert!(ACTION_LOOP.contains(
            "let cd = [gp[0] - self.last_streamed[0], gp[1] - self.last_streamed[1]];"),
            "the server-correction gate is no longer the 2-D test this module's co-occurrence \
             argument rests on");
    }

    // ── the wiring ──────────────────────────────────────────────────────────────────────────────

    const APP_RS: &str = include_str!("app.rs");

    /// `app.rs` with comments removed, plus a parallel mask marking which bytes are CODE — i.e.
    /// **not** inside a string literal.
    ///
    /// Both halves matter. Stripping comments is what stops a call that has been commented out from
    /// satisfying a scan (#721 A2b's evasion was a trailing comment). The string mask is what stops
    /// `app.rs`'s many `"{:.2}"`-style format literals from being counted as braces, which would
    /// desynchronise every depth measurement below and make the whole scan meaningless while still
    /// passing.
    ///
    /// Deliberately not a Rust parser, and it does not need to be. It handles `//`, `/* */`,
    /// `"…"` with `\` escapes, and nothing else. Two known limits, both checked rather than assumed:
    /// it does not understand raw strings (`r"…"`) or char literals (`'{'`), so
    /// `the_app_rs_scan_reads_a_balanced_file` asserts the brace count comes out balanced over the
    /// whole file — which is what either construct would break.
    fn strip_comments(src: &str) -> (Vec<u8>, Vec<bool>) {
        let b = src.as_bytes();
        let (mut text, mut code) = (Vec::with_capacity(b.len()), Vec::with_capacity(b.len()));
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
                while i < b.len() && b[i] != b'\n' { i += 1; }
            } else if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') { i += 1; }
                i = (i + 2).min(b.len());
            } else if b[i] == b'"' {
                // Copy the literal verbatim so failure messages stay readable, but mark it non-code
                // so neither the brace walk nor the needle search can see inside it.
                text.push(b[i]); code.push(true); i += 1;         // the opening quote IS code
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' && i + 1 < b.len() {
                        text.push(b[i]); code.push(false); i += 1;
                    }
                    text.push(b[i]); code.push(false); i += 1;
                }
                if i < b.len() { text.push(b[i]); code.push(true); i += 1; } // the closing quote
            } else {
                text.push(b[i]); code.push(true); i += 1;
            }
        }
        (text, code)
    }

    /// Every offset at which `needle` appears as CODE (see [`strip_comments`]).
    fn code_occurrences(text: &[u8], code: &[bool], needle: &str) -> Vec<usize> {
        let n = needle.as_bytes();
        if n.is_empty() || text.len() < n.len() { return Vec::new(); }
        (0..=text.len() - n.len())
            .filter(|&i| code[i] && &text[i..i + n.len()] == n)
            .collect()
    }

    /// The offsets of the `{` still open at `at`, outermost first — the statement's block chain.
    fn open_braces(text: &[u8], code: &[bool], at: usize) -> Vec<usize> {
        let mut stack = Vec::new();
        for i in 0..at.min(text.len()) {
            if !code[i] { continue; }
            if text[i] == b'{' { stack.push(i); } else if text[i] == b'}' { stack.pop(); }
        }
        stack
    }

    /// The text that INTRODUCES the block opened by the `{` at `brace`: everything back to the
    /// previous statement boundary (`;`, `{` or `}`).
    ///
    /// That is the right unit rather than "the brace's own line", because a condition can wrap:
    /// `if a\n    && b {` puts nothing on the brace's line at all.
    fn block_head(text: &[u8], code: &[bool], brace: usize) -> String {
        let start = (0..brace).rev()
            .find(|&i| code[i] && matches!(text[i], b';' | b'{' | b'}'))
            .map(|i| i + 1)
            .unwrap_or(0);
        String::from_utf8_lossy(&text[start..brace]).into_owned()
    }

    /// Does this block head introduce a block whose body may be SKIPPED?
    ///
    /// `fn f(…)`, `impl T`, and a bare `{` (empty head) are unconditional: reaching the head means
    /// reaching the body. `if …`, `else`, `while …`, `for …`, `loop`, `match …`, a match arm
    /// (`… =>`) and a closure (`|…|`) are not.
    fn is_conditional(head: &str) -> bool {
        if head.contains("=>") || head.contains('|') { return true; }
        head.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|w| matches!(w, "if" | "else" | "while" | "for" | "loop" | "match"))
    }

    /// The scan's own reach control. If [`strip_comments`] ever mis-parses `app.rs` — a raw string,
    /// a `'{'` char literal, an unbalanced construct — every depth measurement below silently
    /// becomes noise, and a noisy scan is a scan that passes.
    #[test]
    fn the_app_rs_scan_reads_a_balanced_file() {
        let (text, code) = strip_comments(APP_RS);
        let mut depth = 0i32;
        for i in 0..text.len() {
            if !code[i] { continue; }
            if text[i] == b'{' { depth += 1; }
            if text[i] == b'}' { depth -= 1; }
            assert!(depth >= 0, "brace depth went negative at byte {i} of app.rs — the scan in \
                                 this module is mis-parsing the file, and every check that rests \
                                 on it is meaningless");
        }
        assert_eq!(depth, 0, "app.rs's braces do not balance under this module's scan — most \
                              likely a raw string or a char literal containing a brace, which \
                              `strip_comments` does not understand. Fix the scan, do not delete \
                              the tests that use it.");
        // …and the file it read is the real one, not an empty include.
        assert!(APP_RS.len() > 50_000, "app.rs came back implausibly small ({} bytes) — the \
                                        include is not resolving to the source file", APP_RS.len());
    }

    /// The scan bites. Fed synthetic sources rather than `app.rs`, so it proves the MECHANISM
    /// discriminates rather than proving today's `app.rs` happens to pass.
    ///
    /// The disabled case is the exact mutation #791's round-1 review used to defeat the previous
    /// version of this pin: wrap the call in `if false { … }` and every test stayed green, because
    /// `str::contains` cannot see the wrapper.
    #[test]
    fn an_unconditional_statement_check_tells_a_live_call_from_dead_code() {
        let live = "impl App {\n    fn f(&mut self) {\n        let x = 1;\n        \
                    self.zone_in.on_frame();\n    }\n}\n";
        let disabled = "impl App {\n    fn f(&mut self) {\n        let x = 1;\n        \
                        if false { self.zone_in.on_frame(); }\n    }\n}\n";
        let commented = "impl App {\n    fn f(&mut self) {\n        let x = 1;\n        \
                         // self.zone_in.on_frame();\n    }\n}\n";
        let in_a_string = "impl App {\n    fn f(&mut self) {\n        \
                           log(\"self.zone_in.on_frame();\");\n    }\n}\n";
        // A runtime `if`, NOT a `#[cfg(...)]` — the #791 round-2 review found that a genuine
        // compile-time feature flag is a different mechanism this scan cannot see at all (see the
        // "What this does NOT establish" section on the sibling test below). This case was
        // previously mislabelled "behind a flag", which is what generalised the false claim in the
        // first place.
        let behind_a_runtime_if = "impl App {\n    fn f(&mut self) {\n        \
                        if self.ready { self.zone_in.on_frame(); }\n    }\n}\n";
        let in_a_closure = "impl App {\n    fn f(&mut self) {\n        \
                            self.later(|| { self.zone_in.on_frame(); });\n    }\n}\n";
        let in_a_match_arm = "impl App {\n    fn f(&mut self) {\n        match e {\n            \
                              A => { self.zone_in.on_frame(); }\n        }\n    }\n}\n";

        let verdict = |src: &str| -> Result<(), String> {
            let (text, code) = strip_comments(src);
            let hits = code_occurrences(&text, &code, "self.zone_in.on_frame(");
            let at = *hits.first().ok_or_else(|| "not written at all".to_string())?;
            for b in open_braces(&text, &code, at) {
                let head = block_head(&text, &code, b);
                if is_conditional(&head) { return Err(format!("conditional block: `{}`", head.trim())); }
            }
            Ok(())
        };

        assert_eq!(verdict(live), Ok(()), "the live call must pass");
        for (name, src) in [("if false", disabled), ("commented out", commented),
                            ("inside a string", in_a_string),
                            ("behind a runtime if", behind_a_runtime_if),
                            ("inside a closure", in_a_closure), ("in a match arm", in_a_match_arm)] {
            assert!(verdict(src).is_err(),
                "a call that is {name} must NOT satisfy this check — that is the #791 round-1 \
                 evasion, and the whole reason this scan exists");
        }
    }

    /// **The one remaining `app.rs` call site into this module, and what pinning it can honestly
    /// claim.**
    ///
    /// # What this establishes
    ///
    /// 1. `app.rs` contains exactly one call into this module, `self.zone_in.on_frame(…)`, written
    ///    as code (not in a comment, not inside a string literal).
    /// 2. That statement is **unconditional within its function**: every block enclosing it, up to
    ///    and including `render_frame`'s own body, is introduced by something that cannot skip its
    ///    body. Wrapping the call in `if false { … }`, in a closure or in a match arm reds this
    ///    test. `an_unconditional_statement_check_tells_a_live_call_from_dead_code`
    ///    demonstrates that on synthetic sources; `the_app_rs_scan_reads_a_balanced_file` is the
    ///    reach control that stops a mis-parse from making it vacuous.
    /// 3. The call is handed the five live values it needs, by name. (A round-3 review pass over
    ///    this doc found the previous version said "four" while checking only four of the five
    ///    non-`controller` arguments — silently excluding `zone_underworld`. Fixed by adding the
    ///    fifth check rather than narrowing the sentence to match the gap.)
    ///
    /// # What this does NOT establish — and cannot
    ///
    /// **That `App::render_frame` runs.** It is called from `window_event`'s `RedrawRequested` arm,
    /// and `App` owns a window, a GPU and an event loop, so no test in this workspace can execute
    /// it. That is #728's own premise, and it is why this module exists at all. A source scan can
    /// show a statement is written and un-nested; it cannot show its function is called. #799
    /// tracks that residual and the two pins of this shape still standing in `movement.rs`.
    ///
    /// **That the statement survives compilation.** #791 round-2 review MEASURED this, against the
    /// real `app.rs` call site, copy-aside/restore, md5-verified before and after: prepending
    /// `#[cfg(any())]` directly above the statement, and separately wrapping it in a
    /// `macro_rules!` that discards its argument list (`never!(self.zone_in.on_frame(…))`), both
    /// leave this test `1 passed, 0 failed` — while in both cases rustc's own dead-code lint fired
    /// `warning: field \`zone_in\` is never read`, independently proving the call had been excluded
    /// from the compiled binary. Neither mutation is caught, and neither can be, by the mechanism
    /// above: `is_conditional` only rejects a block introduced by
    /// `if`/`else`/`while`/`for`/`loop`/`match`/a match arm/a closure, and `#[cfg]` introduces no
    /// block at all — it is resolved by rustc before this scan ever walks the text — while a macro
    /// invocation's discarded arguments live inside `(`/`)`, which `open_braces` does not track.
    /// A text scan cannot see whether the compiler kept a statement; only the compiler's own
    /// warnings can.
    ///
    /// This paragraph replaces a sentence in the previous version of this test that said nothing in
    /// the harness "can see whether `app.rs` still calls it" — which reads as an execution claim,
    /// and was false of a mechanism that only checked a substring (#791 round-1 review, blocking).
    ///
    /// # Why there is only one call site left to pin
    ///
    /// Because two were deleted rather than pinned harder. Arming and "has been stepped" are now
    /// derived inside [`ZoneIn::on_frame`] from values it is already handed — see the module doc.
    /// A call site that does not exist cannot be written-but-not-reached.
    ///
    /// # Triage if this reds
    ///
    /// A fired count assertion means the call moved, was duplicated, or was removed. A fired
    /// conditional assertion means it is now nested inside something that can skip it — which is a
    /// #712 regression, not a test problem. An argument assertion means the call is being fed
    /// something else; check that the new value still changes when the zone does, because that
    /// change IS the arm.
    #[test]
    fn the_app_rs_call_into_this_module_is_an_unconditional_statement() {
        let (text, code) = strip_comments(APP_RS);

        let hits = code_occurrences(&text, &code, "self.zone_in.");
        assert_eq!(hits.len(), 1,
            "app.rs must reach this module through exactly ONE statement, and it must be \
             `on_frame`. Found {} call(s). If a second one has been added, it is a second chance \
             to lose the wiring silently — derive it inside `ZoneIn` instead (see the module doc).",
            hits.len());
        let at = hits[0];
        assert_eq!(code_occurrences(&text, &code, "self.zone_in.on_frame(").first(), Some(&at),
            "app.rs's single call into this module is no longer `on_frame` — see #712 / #728");

        for b in open_braces(&text, &code, at) {
            let head = block_head(&text, &code, b);
            assert!(!is_conditional(&head),
                "the zone-in one-shot must run on EVERY rendered frame, but its call sits inside a \
                 block that can be skipped: `{}`. A source scan cannot tell a call that never runs \
                 from one that is merely written — which is exactly how #791's round-1 pin was \
                 defeated by `if false {{ … }}` — so this check refuses the nesting outright.",
                head.trim());
        }

        // The arguments. Textual, and labelled as such: this cannot see what the values ARE at
        // runtime, only that the call is still spelled against the fields whose meanings the
        // module's docs describe. The zone argument is the load-bearing one — a constant there
        // would mean the one-shot never arms again, silently.
        let close = text[at..].iter().position(|&c| c == b';')
            .expect("the on_frame call in app.rs has no terminating `;`");
        let args = String::from_utf8_lossy(&text[at..at + close]).into_owned();
        for (arg, why) in [
            ("&self.current_zone",
             "the zone whose CHANGE is the arm — a constant or a stale field here means the \
              one-shot never arms again, and nothing else in the client would notice"),
            ("self.loading",           "the asset-load throttle"),
            ("self.camera_initialized","whether this frame will step the controller at all"),
            ("self.collision",         "this zone's geometry, `None` for the whole load window"),
            ("self.game_state_view.world.zone_underworld",
             "this zone's baked underworld — `zone_in_reground` uses it to tell a genuine void \
              arrival from a landable floor; a stale value here silently answers for the WRONG \
              zone's underworld"),
        ] {
            assert!(args.contains(arg),
                "app.rs must still pass `{arg}` to `ZoneIn::on_frame` — {why}. Call was:\n{args}");
        }
    }

    /// `app.rs`'s own `forget_recovery_history()` — the #745 line — must be a **direct, un-nested
    /// statement of the zone-change reload block**, not merely present somewhere inside it.
    ///
    /// This module's behavioural tests deliberately run in the world where that line is *absent*
    /// (the harness never makes the call), so nothing here needs it. It is pinned anyway because
    /// it is the TIMELY clear — it drops the ring in the same statement run that drops the old
    /// collision, roughly a ten-second asset load before `ZoneIn::on_frame` first runs — and
    /// `movement::forget_recovery_history`'s doc calls it load-bearing for exactly that reason.
    ///
    /// **Complement, not duplicate.** `movement::tests::
    /// the_zone_change_reload_block_still_forgets_the_recovery_ring` asserts the line is *written*
    /// inside that block. It is a `str::contains`, so it survives the line being wrapped in
    /// `if false { … }` — the round-1 evasion in #791, on the very line #745 is about. This test
    /// asserts what a substring cannot: the statement's innermost enclosing block is the reload
    /// block itself, so any conditional wrapper reds it. Both are worth having; `movement.rs` was
    /// held by another change when this was written, so hardening its own two pins of this shape is
    /// #799's, not this PR's.
    ///
    /// Triage if this reds: check what the statement is nested inside now. If a condition has been
    /// added deliberately, the question to answer first is what happens to the ring on the frames
    /// that condition is false — because on those frames the previous zone's coordinates outlive
    /// the previous zone's collision, which is #712.
    #[test]
    fn the_zone_change_blocks_ring_clear_is_not_nested_behind_a_condition() {
        const RELOAD_HEAD: &str = "if zone_needs_reload(&self.scene.zone, &self.current_zone)";
        let (text, code) = strip_comments(APP_RS);

        let hits = code_occurrences(&text, &code, "self.controller.forget_recovery_history();");
        assert_eq!(hits.len(), 1,
            "app.rs should clear the controller's recovery ring in exactly one place, the \
             zone-change reload block (#712/#745). Found {}.", hits.len());

        let chain = open_braces(&text, &code, hits[0]);
        let innermost = *chain.last().expect("the call is not inside any block at all");
        let head = block_head(&text, &code, innermost).trim().to_string();
        assert_eq!(head, RELOAD_HEAD,
            "the #745 ring clear must sit directly in the zone-change reload block. Its innermost \
             enclosing block is introduced by `{head}` instead — if that is a condition, the ring \
             now outlives the collision on every frame it is false, and a `str::contains` pin \
             (which is what `movement.rs` has) cannot see the difference. See #799.");
    }

    // #867's ordering guarantee is NOT pinned here, deliberately. An earlier revision of this fix
    // added two scans of `app.rs` asserting the `camera_snapshot` write sat at a higher byte offset
    // than the `render_frame` call and was not brace-nested behind a condition. Independent review
    // defeated both while they stayed green: extracting the write into a helper placed later in
    // `impl App` and calling it from the old pre-draw site satisfies both scans, and so does adding
    // a second pre-draw write through `Arc::clone(&self.camera_snapshot)` (the `hits.len() == 1`
    // guard is a substring count over one spelling). Neither is an attack; the first is an ordinary
    // tidy-up. What a byte-offset scan measures is where characters sit, not when a write executes.
    // The ordering is now a type fact instead — `CameraState::snapshot` takes the
    // `eqoxide_renderer::DrawnFrame` that only `render_frame` returns, so a pre-draw publisher has
    // no argument to pass and does not compile. See #799 for the pattern, and `DrawnFrame`'s own
    // `compile_fail` doctest for the proof that it cannot be fabricated.
}
