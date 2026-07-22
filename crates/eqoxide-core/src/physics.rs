//! Pure movement/physics constants and calculations shared by the controller (`movement`), the
//! action loop (`eq_net::action_loop`), and the navigation planner/walker (`nav`).
//!
//! These are dependency-free scalars and `f32 -> …` functions — no wgpu/winit/tokio/net/app types —
//! so they live in the leaf `eqoxide-core` crate (#544 Step 2d). The behavior that *operates* on
//! them (the `CharacterController`, the packet builders) stays in the app crate; only the numbers and
//! the pure kinematics moved down. Each original site re-exports the symbol it used to define, so
//! `crate::movement::{PLAYER_RADIUS,STEP_UP,JUMP_VELOCITY,running_jump_reach}`,
//! `crate::eq_net::action_loop::RUN_SPEED`, and `crate::eq_net::protocol::fall_damage` all keep
//! resolving unchanged. Keeping a single source of truth here also prevents the two sides of a shared
//! physics number from silently drifting apart.

/// Wall-collision sphere radius, matched to the reference RoF2 client.
pub const PLAYER_RADIUS: f32 = 1.0;

/// Step-up height, matched to the reference RoF2 client. This is a HARD cap: the
/// native client can auto-step a ledge at most 2.0u tall; anything taller is a wall (jump or go
/// around) — there is no larger climb and no separate slope check. It is the single source of truth
/// for how high nav may climb, so `find_path` derives its edge-climb cap (`STEP_H`) from it. Both
/// free WASD and the nav walker are clamped to this — navigation must never climb what a WASD player
/// can't (#239). (Was decoupled from a super-human `NAV_CLIMB = 20.0`, which teleported the walker up
/// 20u ridges/invisible walls and stranded it on the high side of boundaries.)
pub const STEP_UP: f32 = 2.0;

/// Gravity / terminal fall (matches the renderer's prior physics + falling-physics.md).
pub const GRAVITY: f32 = 120.0;

/// Jump impulse for the free-WASD Space jump. Peak height = v²/(2·GRAVITY); at 31 that's ~4.0u —
/// enough to clear/mount low ledges, steps and small crates (well above the 2u step-up), matching
/// the reference RoF2 client's usable jump. The old value (13 → only ~0.7u peak, "barely leaves
/// the ground") was a placeholder carried over from the pre-controller WASD block (eqoxide#92).
/// (Exact RoF2 parity of the impulse is worth a live check; 4u restores a usable jump.)
pub const JUMP_VELOCITY: f32 = 31.0;

/// Native Titanium base run speed (u/s). The action loop tags outbound move intents with it and the
/// nav walker/steering integrate at it, so it is the single source of truth for both drivers.
pub const RUN_SPEED: f32 = 44.0;

/// Locomotion clip threshold (u/s), matching the native rule verified for #623: the client
/// chooses the run clip purely by comparing the actor's current forward speed against ITS OWN
/// `walkspeed` (strict `>`; equal walks). eqoxide does not (yet) carry each spawn's individual
/// walkspeed/runspeed floats (`EQEmu/common/patches/rof2_structs.h:444-445` — a longer-term
/// option noted in #623 but not implemented here), so this is a single constant derived from the
/// one speed eqoxide does track precisely: `RUN_SPEED` (44 u/s) is this client's controller cap,
/// which corresponds to the player-special-cased native runspeed float 0.7
/// (`EQEmu/zone/mob.cpp:190-196`). The equivalent native walk speed is
/// `RUN_SPEED * (0.3/0.7) ≈ 18.857 u/s` (0.3 = native walkspeed float), so 20.0 sits just above
/// walk and well below run, giving margin against float noise in the measured speed.
pub const WALK_RUN_THRESHOLD: f32 = 20.0;

/// Minimum real-time window (seconds) a [`windowed_speed_sample`] anchor must span before it is
/// re-sampled. See that function's doc for why this exists.
pub const NAV_SPEED_SAMPLE_WINDOW: f32 = 0.15;

/// Samples a 2D speed (u/s) over a real elapsed-time window, returning `None` until the window has
/// actually elapsed. Exists to fix a live-validation finding for #623: `src/app.rs`'s self-player
/// speed estimate used to re-anchor its distance/time baseline on **every call** (i.e. every render
/// frame), against a denominator `clamp`ed up to a 50ms floor. That is only correct if the position
/// source itself only changes on discrete ~150ms nav ticks, matching the old code's comment — but it
/// does not: `game_state_view.player_x/y/z` is mirrored on essentially every render tick (~10ms),
/// the same defect already found and fixed for the OUTBOUND wire-encoding path in #624 (see
/// `eqoxide-net/src/action_loop.rs`'s `last_streamed` vs `last_sent_pos` split). Re-anchoring every
/// frame meant the numerator only ever covered ~16ms of travel (at 60fps) while the denominator was
/// floored to 50ms, understating a true 44 u/s run as roughly 44×(0.016/0.05) ≈ 14 u/s — exactly the
/// ~5-17 u/s ceiling observed live, never reaching `WALK_RUN_THRESHOLD` (20 u/s). Sampling only once
/// a real `min_window_s` has elapsed since the anchor keeps the numerator and denominator covering
/// the same window regardless of how often the position source itself is mirrored. Callers must only
/// advance their anchor (position + timestamp) when this returns `Some`.
pub fn windowed_speed_sample(
    current_pos: [f32; 2],
    anchor_pos: [f32; 2],
    elapsed_since_anchor_s: f32,
    min_window_s: f32,
) -> Option<f32> {
    if elapsed_since_anchor_s < min_window_s {
        return None;
    }
    let dx = current_pos[0] - anchor_pos[0];
    let dy = current_pos[1] - anchor_pos[1];
    Some((dx * dx + dy * dy).sqrt() / elapsed_since_anchor_s)
}

#[cfg(test)]
mod windowed_speed_sample_tests {
    use super::*;

    #[test]
    fn returns_none_before_window_elapses() {
        // Reference NAV_SPEED_SAMPLE_WINDOW itself, not a hardcoded 0.15 literal (#623 PR review):
        // a hardcoded literal here would keep passing even if the constant were changed to
        // something else entirely, since it would no longer be testing the constant actually in
        // use anywhere — it would just be re-verifying the function's generic `<` behavior for an
        // arbitrary fixed number. Referencing the constant means shrinking/growing
        // NAV_SPEED_SAMPLE_WINDOW is reflected here automatically.
        assert_eq!(
            windowed_speed_sample([1.0, 0.0], [0.0, 0.0], NAV_SPEED_SAMPLE_WINDOW - 0.001, NAV_SPEED_SAMPLE_WINDOW),
            None
        );
        assert_eq!(windowed_speed_sample([1.0, 0.0], [0.0, 0.0], 0.0, NAV_SPEED_SAMPLE_WINDOW), None);
    }

    #[test]
    fn samples_correct_speed_once_window_elapses() {
        // 10 units over exactly the window -> 10 / window u/s.
        let got = windowed_speed_sample([10.0, 0.0], [0.0, 0.0], NAV_SPEED_SAMPLE_WINDOW, NAV_SPEED_SAMPLE_WINDOW).unwrap();
        assert!((got - 10.0 / NAV_SPEED_SAMPLE_WINDOW).abs() < 1e-4, "got {got}");
    }

    #[test]
    fn diagonal_distance_uses_euclidean_norm() {
        // 3-4-5 triangle over a 1s window -> speed 5. Window itself is incidental to this test (it
        // only needs to be small enough that 1s clears it), but reference the constant anyway for
        // consistency with the rest of this module.
        let got = windowed_speed_sample([3.0, 4.0], [0.0, 0.0], 1.0, NAV_SPEED_SAMPLE_WINDOW).unwrap();
        assert!((got - 5.0).abs() < 1e-4, "got {got}");
    }

    /// The regression this exists for: simulate a real player moving at a constant RUN_SPEED,
    /// mirrored into `current_pos` on EVERY call (as `game_state_view.player_x/y/z` really is,
    /// per the #624-review finding), at a 60fps frame cadence. The OLD per-frame-reanchor +
    /// `clamp(0.05, 0.5)` formula systematically underestimates this below `WALK_RUN_THRESHOLD`;
    /// the windowed sampler must report the true speed once its window has elapsed.
    #[test]
    fn sixty_fps_mirroring_still_reports_true_run_speed() {
        let frame_dt = 1.0 / 60.0_f32;
        let mut pos = [0.0_f32, 0.0];
        let mut anchor_pos = pos;
        let mut elapsed_since_anchor = 0.0_f32;
        let mut last_sample: Option<f32> = None;

        // Simulate 2 full sample windows worth of 60fps frames.
        let frames = ((NAV_SPEED_SAMPLE_WINDOW * 2.0) / frame_dt).ceil() as u32 + 2;
        for _ in 0..frames {
            pos[0] += RUN_SPEED * frame_dt; // mirrored in every frame, like game_state_view.player_x
            elapsed_since_anchor += frame_dt;
            if let Some(speed) =
                windowed_speed_sample(pos, anchor_pos, elapsed_since_anchor, NAV_SPEED_SAMPLE_WINDOW)
            {
                last_sample = Some(speed);
                anchor_pos = pos;
                elapsed_since_anchor = 0.0;
            }
        }

        let speed = last_sample.expect("at least one window should have elapsed");
        assert!(
            speed > WALK_RUN_THRESHOLD,
            "windowed sample {speed} must clear WALK_RUN_THRESHOLD for a true {RUN_SPEED} u/s run \
             (this is the exact live-validation gap #623's self-player fix needed to close)"
        );
        assert!((speed - RUN_SPEED).abs() < 1.0, "windowed sample {speed} should be close to {RUN_SPEED}");
    }

    /// Same simulation, but reproducing the OLD (buggy) per-frame-reanchor formula directly, to
    /// document — as a passing test, not a comment — that it really did understate a true run below
    /// threshold. This is the mutation-check control: it must stay green (proving the OLD formula was
    /// really broken) both before and after the `windowed_speed_sample` fix lands, since it does not
    /// call `windowed_speed_sample` at all.
    #[test]
    fn old_per_frame_reanchor_formula_understated_a_true_run_below_threshold() {
        let frame_dt = 1.0 / 60.0_f32;
        let mut prev = [0.0_f32, 0.0];
        let mut pos = [0.0_f32, 0.0];
        let mut speed = 0.0_f32;
        for _ in 0..120 {
            pos[0] += RUN_SPEED * frame_dt;
            let dist = ((pos[0] - prev[0]).powi(2) + (pos[1] - prev[1]).powi(2)).sqrt();
            if dist > 0.01 {
                let dt_upd = frame_dt.clamp(0.05, 0.5);
                speed = dist / dt_upd;
            }
            prev = pos;
        }
        assert!(
            speed < WALK_RUN_THRESHOLD,
            "control check failed: expected the OLD formula to understate speed {speed} below \
             threshold {WALK_RUN_THRESHOLD} for a true {RUN_SPEED} u/s run"
        );
    }

    /// Simulates a run of `windowed_speed_sample` calls at `render_tick_dt` cadence against a
    /// position source that only actually CHANGES at `backend_tick_dt` cadence (a staircase, not a
    /// smooth ramp), with the two clocks phase-offset from each other by `backend_phase_offset` —
    /// i.e. NOT lockstep. Returns every non-`None` sample taken.
    fn simulate_staircase_samples(
        render_tick_dt: f32,
        backend_tick_dt: f32,
        backend_phase_offset: f32,
        min_window_s: f32,
        total_time_s: f32,
    ) -> Vec<f32> {
        let mut backend_pos = [0.0_f32, 0.0];
        let mut backend_next_tick = backend_phase_offset;
        let mut backend_elapsed = 0.0_f32;

        let mut anchor_pos = [0.0_f32, 0.0];
        let mut elapsed_since_anchor = 0.0_f32;
        let mut samples = Vec::new();

        let mut t = 0.0_f32;
        while t < total_time_s {
            t += render_tick_dt;
            backend_elapsed += render_tick_dt;
            // The backend's own position step always uses its REAL elapsed dt for that tick (not a
            // fixed assumed value), so total distance / total real time converges to the true speed
            // — but only over a window wide enough to span at least one full backend tick. A window
            // narrower than backend_tick_dt can straddle zero tick boundaries and read zero motion.
            while backend_elapsed >= backend_next_tick {
                backend_pos[0] += RUN_SPEED * backend_tick_dt;
                backend_next_tick += backend_tick_dt;
            }
            elapsed_since_anchor += render_tick_dt;
            if let Some(speed) =
                windowed_speed_sample(backend_pos, anchor_pos, elapsed_since_anchor, min_window_s)
            {
                samples.push(speed);
                anchor_pos = backend_pos;
                elapsed_since_anchor = 0.0;
            }
        }
        samples
    }

    /// Reproduces the review finding directly (rather than asserting it in prose): shrinking
    /// `min_window_s` down to a single render frame (~1/60s, the literal the reviewer mutated
    /// `NAV_SPEED_SAMPLE_WINDOW` to) reintroduces the ORIGINAL failure mode — misclassifying a
    /// genuinely sustained `RUN_SPEED` run as "walking" — even though `windowed_speed_sample` itself
    /// has no clamp bug. `sixty_fps_mirroring_still_reports_true_run_speed` above cannot show this:
    /// it mirrors position into the SAME clock that drives its own sampling loop, in perfect
    /// lockstep, and uniform motion sampled by its own clock is mathematically exact for ANY window
    /// size — so that test is structurally incapable of distinguishing window sizes.
    ///
    /// The real system does not have that guarantee: `game_state_view.player_x/y/z` is mirrored by
    /// the NETWORK thread's own tick loop (`gameplay.rs`'s `sleep(Duration::from_millis(10))`),
    /// while the render loop samples it on its OWN, independently-scheduled clock
    /// (`Instant::now()` in `render_frame`). `tokio::time::sleep` is a *minimum* delay, not a
    /// real-time guarantee — under system load (mutex contention, GC-like pauses, scheduling
    /// noise) the network thread's actual tick period can and does drift above its nominal 10ms.
    /// This simulates that: a backend tick period of 20ms (a realistic delayed/jittered cadence)
    /// mirrored into a position sampled by a render loop at a 1/60s cadence, with the two clocks
    /// NOT phase-aligned. A window narrower than the backend's tick period can and does land
    /// entirely inside a "the backend hasn't ticked yet" gap, reading zero distance.
    #[test]
    fn phase_misaligned_backend_tick_needs_the_real_window_not_one_frame() {
        let render_tick_dt = 1.0_f32 / 60.0; // one render frame — the reviewer's literal mutation
        let backend_tick_dt = 0.020_f32;     // jittered/delayed backend cadence (nominal 10ms + drift)
        let backend_phase_offset = 0.007_f32; // not phase-locked to the render clock
        let total_time_s = 1.0_f32;          // one full second of sustained running

        let shrunk_window_samples = simulate_staircase_samples(
            render_tick_dt, backend_tick_dt, backend_phase_offset, render_tick_dt, total_time_s,
        );
        assert!(
            shrunk_window_samples.iter().any(|&s| s <= WALK_RUN_THRESHOLD),
            "expected shrinking the window to a single render frame to misclassify at least one \
             sample of a sustained {RUN_SPEED} u/s run as walking against a phase-misaligned \
             backend clock — got samples: {shrunk_window_samples:?} (if this fails, the fixture's \
             parameters no longer reproduce the aliasing this test exists to catch — do not just \
             delete the assertion)"
        );

        let real_window_samples = simulate_staircase_samples(
            render_tick_dt, backend_tick_dt, backend_phase_offset, NAV_SPEED_SAMPLE_WINDOW, total_time_s,
        );
        assert!(
            !real_window_samples.is_empty(),
            "NAV_SPEED_SAMPLE_WINDOW should have elapsed at least once in {total_time_s}s"
        );
        assert!(
            real_window_samples.iter().all(|&s| s > WALK_RUN_THRESHOLD),
            "NAV_SPEED_SAMPLE_WINDOW must stay clear of WALK_RUN_THRESHOLD against the SAME \
             phase-misaligned backend clock that breaks a 1-frame window — got samples: \
             {real_window_samples:?}"
        );
    }
}

/// Chooses the walk-vs-run locomotion clip **purely from forward speed**, per the native rule
/// verified for #623 (strict `>`: exactly at the threshold still walks). This covers ONLY the
/// forward walk/run branch of the full native rule — callers remain responsible for the
/// higher-priority overrides that already exist at both integration sites (dead, combat swing,
/// submerged swim/tread, sitting) before ever reaching this call, exactly as they did before this
/// fix for the plain "walking" action. Returns the action-string literal understood by
/// `eqoxide_renderer::anim::Skin::clip_for_action`.
///
/// Native rule also specifies "moving backwards -> back-walk (never run)" — this function does
/// not implement that branch: neither integration site in `src/app.rs` currently derives a
/// movement-direction-relative-to-heading signal (self-player's action is computed from
/// server-authoritative position deltas, not WASD keys; remote entities always face their travel
/// direction, so they can never appear to move "backward" in this client's model), and
/// `eqoxide_renderer::anim::Skin::clip_for_action` has no `_ if action == "walking_backward"`-style
/// arm that would ever request a back-walk clip — it is **not wired up**, not because the clip data
/// is absent (baked GLBs DO carry clips whose name contains `walk_back`, e.g.
/// `L07A_walk_back`/`L07B_walk_back` in `humanoid.glb`/`elf.glb`; `clip_for_action`'s `"running"` arm
/// even explicitly excludes any clip name containing `"back"` so it can't be mis-picked as a run
/// clip). Whether that `walk_back` label is itself correct is a separate, pre-existing question:
/// `eqoxide_asset_server::convert::anim_label` (src/convert/mod.rs:1176) currently maps WLD code
/// `L07` to `"walk_back"`, but `eq_kb/animation-codes.md` (private knowledge base; see that repo
/// for sourcing) says `L07` is CLIMB, not a walk-backward loop, and lists no confirmed retail code
/// for backward walking at all. Regardless of which side of that dispute is
/// right, wiring a "backward" action through `clip_for_action` is out of scope for this fix (#623's
/// confirmed bug and required Fix A/B/C is walk-vs-run only) — noted here rather than silently
/// ignored, and left for whoever resolves the L07 labeling question.
pub fn walk_or_run(speed_u_per_s: f32) -> &'static str {
    if speed_u_per_s > WALK_RUN_THRESHOLD { "running" } else { "walking" }
}

#[cfg(test)]
mod walk_or_run_tests {
    use super::*;

    #[test]
    fn below_threshold_walks() {
        assert_eq!(walk_or_run(WALK_RUN_THRESHOLD - 0.01), "walking");
        assert_eq!(walk_or_run(5.0), "walking");
        assert_eq!(walk_or_run(0.0), "walking");
    }

    #[test]
    fn above_threshold_runs() {
        assert_eq!(walk_or_run(WALK_RUN_THRESHOLD + 0.01), "running");
        assert_eq!(walk_or_run(RUN_SPEED), "running", "RUN_SPEED itself must select the run clip");
    }

    #[test]
    fn exactly_at_threshold_walks_native_comparison_is_strict() {
        // Native rule is `speed > walkspeed -> run`, so equality still walks.
        assert_eq!(walk_or_run(WALK_RUN_THRESHOLD), "walking");
    }

    /// The threshold must sit strictly between the derived native walk speed (~18.857 u/s) and
    /// RUN_SPEED (44 u/s), or the constant itself would misclassify real walk/run traffic
    /// regardless of the comparison operator.
    #[test]
    fn threshold_sits_between_native_walk_speed_and_run_speed() {
        let native_walk_speed = RUN_SPEED * (0.3 / 0.7);
        assert!(WALK_RUN_THRESHOLD > native_walk_speed,
            "threshold {WALK_RUN_THRESHOLD} must clear the native walk speed {native_walk_speed}");
        assert!(WALK_RUN_THRESHOLD < RUN_SPEED,
            "threshold {WALK_RUN_THRESHOLD} must stay below RUN_SPEED {RUN_SPEED}");
    }
}

/// Horizontal distance a *running* jump clears to a landing at roughly takeoff height, at
/// `run_speed` (u/s). The character leaves the ground at `JUMP_VELOCITY` and, ignoring the small
/// landing-height difference, is airborne for `2·JUMP_VELOCITY/GRAVITY` seconds (up then back to
/// takeoff height); horizontal reach = airborne_time · run_speed. `find_path` uses this to add
/// jump-edges across genuine floor gaps no wider than a jump can bridge (eqoxide#190). A landing
/// that is LOWER than takeoff gives more airborne time, so this is a conservative (minimum) reach.
pub fn running_jump_reach(run_speed: f32) -> f32 {
    let air_time = 2.0 * JUMP_VELOCITY / GRAVITY;
    air_time * run_speed
}

/// Native Titanium fall damage for a fall of `height` EQ units. Fall damage is CLIENT-computed in
/// EQ (the server only validates OP_EnvDamage). Model: impact velocity = min(terminal,
/// sqrt(2·g·h)) converted to the client's internal per-update z-velocity units (~5-13); then
/// `fall_score = |z_vel| − 4` (char_counter≈0, no safe-fall skill): ≤0 → no damage, ≥9 → lethal
/// (20000), else a roll in `[0, score²·10]`. Returns (rolled_damage, max_damage). See
/// ~/git/eq_kb/falling-physics.md.
pub fn fall_damage(height: f32) -> (u32, u32) {
    const GRAVITY: f32 = 120.0;   // matches the renderer's fall physics
    const TERMINAL: f32 = 128.0;  // native internal z-velocity clamp
    const HZ: f32 = 10.0;         // native position-update rate the formula is calibrated to
    let v = (2.0 * GRAVITY * height.max(0.0)).sqrt().min(TERMINAL);
    let score = v / HZ - 4.0;
    if score <= 0.0 { return (0, 0); }
    if score >= 9.0 { return (20_000, 20_000); }
    let max = (score * score * 10.0) as u32;
    let roll = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos()).unwrap_or(0);
    (if max == 0 { 0 } else { roll % (max + 1) }, max)
}
