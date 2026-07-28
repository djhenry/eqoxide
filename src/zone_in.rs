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
//! 1. `app.rs` observes `zone_needs_reload`, drops the OLD zone's collision, arms this one-shot
//!    ([`ZoneIn::arm`]) and drops the controller's previous-zone recovery ring.
//! 2. ~10 s of asset load. The controller is **not stepped** on those frames (there is no
//!    collision to step against), so nothing about it changes — including `on_ground`.
//! 3. The nav streamer may hand over a large server correction, which `app.rs` applies as a
//!    [`crate::movement::CharacterController::teleport`]. It may equally hand over **nothing**:
//!    see [`ZoneIn::on_frame`]'s note on the #593 path.
//! 4. The first frame with the new zone's collision in hand: this one-shot runs.
//! 5. Ordinary stepped frames from there on.

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
#[derive(Clone, Copy, Debug, Default)]
pub struct ZoneIn {
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
    /// Why the one-shot last stopped being armed; `None` while armed, and before the first zone
    /// change of the session.
    outcome: Option<ZoneInOutcome>,
}

impl ZoneIn {
    /// Arm the one-shot for a zone change.
    ///
    /// Called from `app.rs`'s `zone_needs_reload` block, at the moment the OLD zone's collision is
    /// dropped. Everything per-zone resets here — `stepped_in_this_zone` above all, since the
    /// controller has demonstrably not been stepped against a zone whose geometry has not loaded.
    pub fn arm(&mut self) {
        self.armed = true;
        self.stepped_in_this_zone = false;
        self.outcome = None;
    }

    /// Record that `app.rs` has just stepped the controller against the current zone's collision.
    ///
    /// Must be called immediately after [`CharacterController::step`] and nowhere else.
    ///
    /// Its meaning is the weaker, exactly-true one: "the controller has now been stepped against
    /// THIS zone's collision at least once". It is **not** "`on_ground` now describes this zone" —
    /// `step`'s depenetration early return can leave `on_ground` untouched and stale (see
    /// [`ZoneIn::on_frame`]'s settled arm, which is why that arm also corroborates against the
    /// geometry). This flag is a necessary condition for trusting `on_ground`, never a sufficient
    /// one. `the_app_rs_wiring_is_still_in_place` pins the call site.
    pub fn note_stepped(&mut self) {
        self.stepped_in_this_zone = true;
    }

    /// `true` while the one-shot is still waiting for a verdict.
    pub fn armed(&self) -> bool { self.armed }

    /// Why the one-shot last stopped being armed. `None` while armed.
    pub fn last_outcome(&self) -> Option<ZoneInOutcome> { self.outcome }

    /// One frame of zone-in orchestration, run by `app.rs` **before** it steps the controller.
    ///
    /// `loading` is the asset-load throttle (`App::loading`); `collision` is `None` for the whole
    /// load window. `underworld` comes from the live world state (`OP_NewZone`) rather than from
    /// `controller.underworld`, because `set_underworld` is only called from the step below, which
    /// does not run while collision is `None` — so on this first post-load frame the controller
    /// still holds the PREVIOUS zone's threshold, for exactly the same reason `on_ground` does.
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
        loading:    bool,
        collision:  Option<&Collision>,
        underworld: Option<f32>,
    ) {
        if !self.armed { return; }
        // The throttle. The new zone's geometry is still arriving; anything decided now would be
        // decided against the wrong world (or, while `App::collision` is `None`, against no world
        // at all). Wait.
        if loading { return; }
        let Some(col) = collision else { return };

        // The previous zone's recovery ring must not survive into this zone (#712, #724, #745).
        //
        // `app.rs` drops it at the moment the old collision is dropped, which is earlier than, and
        // independent of, any arrival — keep that call, it is the timely one. This is the same
        // invariant restated where a test can reach it, because #745 measured that call site
        // pinned by nothing behavioural. The ring's samples are untagged coordinates from another
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
    struct ZoneInHarness {
        ctrl:       CharacterController,
        zone_in:    ZoneIn,
        collision:  Option<Collision>,
        loading:    bool,
        underworld: Option<f32>,
        dt:         f32,
    }

    impl ZoneInHarness {
        /// A character already living in a zone: collision loaded, nothing pending.
        fn in_zone(collision: Collision, underworld: Option<f32>, at: [f32; 3]) -> Self {
            let mut ctrl = CharacterController::new(at);
            ctrl.on_ground = true;
            Self { ctrl, zone_in: ZoneIn::default(), collision: Some(collision), loading: false,
                   underworld, dt: 1.0 / 60.0 }
        }

        /// One `App::update`: the zone-in one-shot, then the controller step (only when there is a
        /// collision to step against — `app.rs` holds position through the whole load window).
        fn frame(&mut self) {
            self.zone_in.on_frame(&mut self.ctrl, self.loading, self.collision.as_ref(),
                                  self.underworld);
            if let Some(c) = &self.collision {
                self.ctrl.set_underworld(self.underworld);
                self.ctrl.step(idle(), self.dt, c);
                self.zone_in.note_stepped();
            }
        }

        fn frames(&mut self, n: usize) { for _ in 0..n { self.frame(); } }

        /// `app.rs`'s `zone_needs_reload` block: drop the old zone's collision, arm the one-shot,
        /// and — if `clear_ring` — drop the controller's previous-zone recovery ring.
        ///
        /// `clear_ring` is a knob rather than a constant on purpose: it is exactly the `app.rs`
        /// statement #745 is about, so every test can state whether it is leaning on that line.
        fn zone_change(&mut self, clear_ring: bool) {
            self.loading = true;
            self.collision = None;
            self.zone_in.arm();
            if clear_ring { self.ctrl.forget_recovery_history(); }
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

        h.zone_change(true);
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

        // `clear_ring: true` — this test is about `on_ground`, not the ring, so the `app.rs`
        // statement #745 is about is present and doing its job.
        h.zone_change(true);
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

        h.zone_change(true);
        h.frames(120);
        h.assets_ready(steamfont_arrival_column(), Some(STEAMFONT_UNDERWORLD));
        // The pre-fix behaviour, reproduced exactly: the one-shot retires on the stale flag before
        // it measures anything. Disarming it here is that retirement.
        h.zone_in = ZoneIn::default();

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

        h.zone_change(true);
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
        h.zone_change(true);
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
        h.zone_change(true);
        h.frames(30);
        h.assets_ready(steamfont_arrival_column(), Some(STEAMFONT_UNDERWORLD));
        h.correction([0.0, 0.0, -114.4]); // the measured #712 arrival: 1.15 u under the floor
        assert!(!h.ctrl.on_ground, "fixture: a correction leaves the body ungrounded");

        // The one-shot ONLY, without the step that follows it: the assignment under test is the
        // orchestration's own claim about the body, not something the step re-derives.
        h.zone_in.on_frame(&mut h.ctrl, h.loading, h.collision.as_ref(), h.underworld);

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
        h.zone_change(true);
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
                        h.zone_change(true);
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
    /// precisely that arrival with `clear_ring: false` — i.e. with the `app.rs` statement deleted —
    /// into a column where the body has nowhere legal to land, which is the one shape that actually
    /// makes a recovery path *read* the ring.
    ///
    /// The two outcomes are the reason this is an agent-honesty test and not a physics one:
    ///
    /// * ring cleared → the guard has nothing to restore, the body **holds**, and `player.hold`
    ///   says so. A reported failure.
    /// * ring intact → the guard grounds the body on the previous zone's coordinate. `on_ground`
    ///   is `true`, no hold is reported, and the position is perfectly plausible. A confident wrong
    ///   answer — #712's live record.
    ///
    /// MUTATION-CHECK: delete `controller.forget_recovery_history();` from `on_frame` → RED on the
    /// per-frame assertion.
    ///
    /// What this does NOT establish: it does not pin `app.rs`'s own call site, which runs earlier
    /// (when the old collision is dropped) and is pinned textually — by `movement.rs`'s
    /// `the_zone_change_reload_block_still_forgets_the_recovery_ring` and by
    /// `the_app_rs_wiring_is_still_in_place` in this file.
    #[test]
    fn a_zone_in_never_grounds_the_body_on_a_previous_zones_recovery_coordinate() {
        // Bank a ring in the previous zone. #712's measured lfaydark floor.
        let mut h = ZoneInHarness::in_zone(flat_at(LFAYDARK_FLOOR), Some(-500.0),
                                           [-80.0, 0.0, LFAYDARK_FLOOR]);
        h.frames(300); // several GOOD_SAMPLE_SECS (0.5 s) banking intervals
        assert!(h.ctrl.on_ground, "fixture: grounded, so the ring actually banks");
        let stale = h.ctrl.pos;

        // The zone change WITHOUT `app.rs`'s clear — the deletion #745 warns about — and without a
        // correction, which is the #593 arrival where nothing else clears the ring either.
        h.zone_change(false);
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

        // #593 arrival: no correction, so position and `on_ground` both survive. `app.rs` DOES
        // clear the ring here — this test is not about the ring, it is about the frozen step, and
        // an empty ring is what makes the freeze rather than a fallback.
        h.zone_change(true);
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

    /// The harness above establishes what [`ZoneIn`] does; nothing in it can see whether `app.rs`
    /// still calls it. This is that half, by the same `include_str!` technique `movement.rs` and
    /// `eqoxide-net`'s `transport.rs` use for one-call-site pins.
    ///
    /// Triage if this reds: a fired `expect` means an anchor moved — re-anchor it and keep the
    /// test. A fired `assert!` means the call is gone, and with it the `app.rs` half of #712.
    #[test]
    fn the_app_rs_wiring_is_still_in_place() {
        const APP_RS: &str = include_str!("app.rs");

        let start = APP_RS.find("if zone_needs_reload(&self.scene.zone, &self.current_zone) {")
            .expect("app.rs no longer has the zone-change reload block — if it moved, move this \
                     pin with it; do not delete it");
        let end = APP_RS[start..].find("\n        }")
            .expect("could not find the end of the zone-change reload block in app.rs");
        let reload = &APP_RS[start..start + end];
        assert!(reload.contains("self.zone_in.arm();"),
            "the zone-change block must ARM the one-shot reground — without this a zone-in never \
             regrounds at all (#712). Block was:\n{reload}");
        // #745 is exactly this line, and it is asserted TWICE on purpose. `movement.rs`'s
        // `the_zone_change_reload_block_still_forgets_the_recovery_ring` (added by #744, after #745
        // was filed) makes the same assertion, but it lives in the module the reground logic no
        // longer lives in — `zone_in.rs` is where a future reader comes to change this
        // orchestration, so the pin has to be reachable from here too. The behavioural half is
        // `a_zone_in_never_grounds_the_body_on_a_previous_zones_recovery_coordinate`, which holds
        // even with this line gone; that is why this line ALSO needs a pin, and why deleting it as
        // "the backstop covers it" is the specific mistake #745 predicted.
        assert!(reload.contains("self.controller.forget_recovery_history();"),
            "the zone-change block must drop the controller's previous-zone recovery ring at the \
             moment the OLD collision goes (#712/#745) — the backstop in `ZoneIn::on_frame` runs \
             ~10 s later, at the far side of the asset load. Block was:\n{reload}");

        let start = APP_RS.find(
            "if self.camera_initialized {\n                if let Some(c) = self.collision.as_deref() {")
            .expect("app.rs no longer has the camera-init/collision-guarded controller step — if \
                     it moved, move this pin with it; do not delete it");
        let end = APP_RS[start..].find("\n            }")
            .expect("could not find the end of the controller-step block in app.rs");
        let step_block = &APP_RS[start..start + end];
        let stepped_at = step_block.find("self.controller.step(intent, dt, c);").expect(
            "the controller step this pin is anchored on has moved or been renamed");
        let noted_at = step_block.find("self.zone_in.note_stepped();").expect(
            "`note_stepped` must be called on the frames that STEP the controller: its entire \
             meaning is \"the controller has been stepped against THIS zone\", the necessary \
             condition for reading `on_ground` at all, and #728 N1 is what happens when the \
             orchestration believes that one frame too early");
        assert!(noted_at > stepped_at,
            "`note_stepped` must come AFTER the step, not before — before the step, `on_ground` is \
             still the previous zone's answer, which is the whole of #728 N1. Block was:\n\
             {step_block}");
        let else_at = step_block.find("} else {").expect(
            "the no-collision arm of the controller-step block has moved");
        assert!(noted_at < else_at,
            "`note_stepped` must be inside the arm that STEPS. The `else` arm is the frames with no \
             collision loaded: nothing is stepped there, so `on_ground` is still whatever the \
             previous zone left, and claiming otherwise re-opens #728 N1. Block was:\n{step_block}");

        assert!(APP_RS.contains("self.zone_in.on_frame("),
            "app.rs must still run the zone-in one-shot each frame — see #712 / #728");
    }
}
