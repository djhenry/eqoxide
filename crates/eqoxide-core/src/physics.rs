//! Pure movement/physics constants and calculations shared by the controller (`movement`), the
//! action loop (`eq_net::action_loop`), and the navigation planner/walker (`nav`).
//!
//! These are dependency-free scalars and `f32 -> …` functions — no wgpu/winit/tokio/net/app types —
//! so they live in the leaf `eqoxide-core` crate (#544 Step 2d). The behavior that *operates* on
//! them (the `CharacterController`, the packet builders) stays in the app crate; only the numbers and
//! the pure kinematics moved down. Each original site re-exports the symbol it used to define, so
//! `crate::movement::{PLAYER_RADIUS,STEP_UP,JUMP_VELOCITY,running_jump_reach}`,
//! `crate::eq_net::action_loop::RUN_SPEED`, and `crate::eq_net::protocol::fall_damage` all keep
//! resolving unchanged.
//!
//! This is a single source of truth for the symbols above and NOT for every physics number in the
//! workspace — do not read co-location here as identity. Two values are still defined more than
//! once, so editing the copy here moves only the sites that read the copy here. [`GRAVITY`] is
//! shadowed by a function-local `GRAVITY` inside [`fall_damage`]. The 128.0 fall terminal is
//! defined twice in two crates: as `MAX_FALL`, module-private in the app crate's `movement` module,
//! which is what the controller actually clamps to, and as a function-local `TERMINAL` inside
//! [`fall_damage`], which is a damage-curve clamp on a derived impact velocity — a DIFFERENT
//! quantity that happens to share both the name-shape and the value. Tracked as #1045.

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

/// Downward acceleration for the fall integration, in EQ units/s². This is the ACCELERATION only —
/// the terminal it is clamped against is `MAX_FALL`, module-private in the app crate's `movement`
/// module, which is the sole out-of-crate consumer of this constant (`CharacterController::step`).
/// `running_jump_reach` below reads it too.
///
/// UNCITED: nothing in this tree, or in the private EQ knowledge-base tree, derives this number.
/// See the note on [`fall_damage`] for the lineage — it is the same one, and it is no longer
/// available to cite. The function-local `GRAVITY` inside [`fall_damage`] repeats this value but is
/// a second unlinked copy (#1045), so it corroborates nothing.
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

/// Local movement speed (u/s) selected when the run/walk toggle (#625) is set to walk. Derived the
/// same way #623 derived `WALK_RUN_THRESHOLD`'s reference point: `RUN_SPEED * (0.3/0.7)`, scaling
/// the player-special-cased native walkspeed float (0.3) against the native runspeed float (0.7)
/// that `RUN_SPEED` (44 u/s) itself corresponds to (`EQEmu/zone/mob.cpp:190-196`).
///
/// This is purely this client's OWN local-controller speed selection. `OP_SetRunMode` (0x009f),
/// the wire message the toggle also sends, does not itself gate the server's anti-cheat speed
/// ceiling (verified against the EQEmu zone source: the ceiling is unconditionally the character's
/// run speed regardless of `runmode` — the flag is consumed elsewhere, by endurance drain and the
/// fear-speed calc). So walking is enforced by moving the controller slower here, not by the server
/// refusing a faster speed; see #625's PR body for the full citation trail.
pub const WALK_SPEED: f32 = RUN_SPEED * (0.3 / 0.7);

#[cfg(test)]
mod walk_speed_tests {
    use super::*;

    #[test]
    fn walk_speed_sits_strictly_below_the_walk_run_threshold() {
        // The locomotion-clip threshold (#623) must classify a WALK_SPEED move as walking, or the
        // two constants have drifted apart and #625's toggle would send OP_SetRunMode(false) while
        // still rendering (and reporting) the run clip.
        const {
            assert!(WALK_SPEED < WALK_RUN_THRESHOLD,
                "WALK_SPEED must clear the run clip threshold");
        }
    }

    #[test]
    fn walk_speed_is_strictly_slower_than_run_speed() {
        const { assert!(WALK_SPEED < RUN_SPEED, "a walk toggle that isn't slower than running is a no-op"); }
        // Sanity-check the derivation itself: 0.3/0.7 of RUN_SPEED, matching the #623 comment's
        // ~18.857 u/s figure.
        assert!((WALK_SPEED - 18.857).abs() < 0.01, "got {WALK_SPEED}");
    }
}

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

/// Convert a signed wire **gait** (the `animation` sub-field of `OP_ClientUpdate`) back into world
/// units/second, so a remote entity's locomotion clip can be chosen by the SAME
/// [`WALK_RUN_THRESHOLD`] (world u/s) that gates the self-player (#651). This is the exact inverse of
/// this client's own outbound encoder `eqoxide_net::action_loop::speed_to_wire_animation`, which maps
/// `anim = speed_u_per_s * (0.7 * 40 / RUN_SPEED)` — EQEmu computes `base_runspeed = runspeed_float *
/// 40` with the player special-case run `0.7 → 28` and walk `0.3 → 12` (`EQEmu/zone/mob.cpp:190-196`,
/// sent as `spu->animation`). So the recovery is `speed = gait * RUN_SPEED / (0.7 * 40)`.
///
/// Two properties this preserves, both load-bearing:
/// - **Correct units.** Naively reading `gait / 40` (as #651's suggested-shape sketch phrased it)
///   yields EQEmu's speed-*float* (0.7 at run, 0.3 at walk), NOT world u/s — comparing that against
///   the 20 u/s `WALK_RUN_THRESHOLD` would classify every gait as walking. The full inverse restores
///   the world-u/s domain the threshold actually lives in: run gait 28 → `RUN_SPEED` (44 u/s), native
///   walk gait 12 → ~18.857 u/s. (Equivalently, in gait units the threshold is `20 * 28/44 ≈ 12.7`,
///   i.e. gait ≥ 13 runs; the two named traffic values 12 and 28 straddle it with margin.)
/// - **Sign.** [`crate::game_state::Gait`] is signed (a backing-up mob carries a negative gait); this
///   maps that to a negative speed, which [`walk_or_run`] classifies as walking — a mob walking
///   backwards can never select the run clip. Feeding the raw *unsigned* 10-bit value would read a
///   backward gait of −12 as 1012 → a spurious run.
pub fn gait_to_speed(gait: i32) -> f32 {
    const EQ_RUNSPEED_FLOAT_AT_RUN: f32 = 0.7; // EQEmu player special-case runspeed (mob.cpp:190-196)
    const ANIM_SCALE: f32 = 40.0; // EQEmu: base_runspeed = runspeed_float * 40
    gait as f32 * RUN_SPEED / (EQ_RUNSPEED_FLOAT_AT_RUN * ANIM_SCALE)
}

#[cfg(test)]
mod gait_to_speed_tests {
    use super::*;

    /// The two native reference gaits (`EQEmu/zone/mob.cpp:190-196`: walk 0.3→12, run 0.7→28) must
    /// recover the world speeds the self-player path already uses, and classify accordingly. This
    /// pins `gait_to_speed` to the SAME anchor values `speed_to_wire_animation` encodes to.
    #[test]
    fn native_reference_gaits_recover_walk_and_run_speeds() {
        // Run gait 28 -> RUN_SPEED (44 u/s) exactly, and selects the run clip.
        assert!((gait_to_speed(28) - RUN_SPEED).abs() < 1e-3, "gait 28 must recover RUN_SPEED");
        assert_eq!(walk_or_run(gait_to_speed(28)), "running");
        // Walk gait 12 -> derived native walk speed (~18.857 u/s), below threshold -> walk clip.
        let native_walk = RUN_SPEED * (0.3 / 0.7);
        assert!((gait_to_speed(12) - native_walk).abs() < 1e-3, "gait 12 must recover native walk speed");
        assert_eq!(walk_or_run(gait_to_speed(12)), "walking");
    }

    /// The whole point of #651: a gait that clears the threshold selects run even though its
    /// magnitude in gait units (13) is nowhere near 20 — i.e. the threshold is applied in the
    /// RIGHT (world-u/s) domain, not naively against the gait code.
    #[test]
    fn gait_just_above_threshold_runs() {
        assert_eq!(walk_or_run(gait_to_speed(13)), "running", "gait 13 (~20.4 u/s) clears threshold");
        assert_eq!(walk_or_run(gait_to_speed(12)), "walking", "gait 12 (~18.9 u/s) does not");
    }

    /// A backing-up mob carries a NEGATIVE gait; it must map to a negative speed and never run.
    #[test]
    fn negative_gait_is_never_run() {
        assert!(gait_to_speed(-28) < 0.0, "backward run gait must be a negative speed");
        assert_eq!(walk_or_run(gait_to_speed(-28)), "walking",
            "a mob backing up at full speed must WALK, never run");
        assert_eq!(walk_or_run(gait_to_speed(-12)), "walking");
    }

    /// Stationary gait 0 -> 0 u/s -> walk classification (the `moving` gate at the call site keeps a
    /// truly-stopped entity idle; this only asserts 0 never spuriously runs).
    #[test]
    fn zero_gait_is_zero_speed() {
        assert_eq!(gait_to_speed(0), 0.0);
        assert_eq!(walk_or_run(gait_to_speed(0)), "walking");
    }
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
        const {
            assert!(WALK_RUN_THRESHOLD < RUN_SPEED,
                "threshold must stay below RUN_SPEED");
        }
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

/// Fall-damage magnitude to REPORT for a fall of `height` EQ units — curve UNCITED, see below.
///
/// The MAGNITUDE is client-computed because the protocol gives the server no way to derive it:
/// the server has no fall detection and reads the number straight out of the
/// `EnvDamage2_Struct.damage` field we report (see
/// `Client::Handle_OP_EnvDamage`, zone/client_packet.cpp). What it does NOT do is merely validate
/// that report — an earlier version of this comment said "the server only validates OP_EnvDamage",
/// which was wrong, and that misreading is plausibly why a local subtraction looked necessary
/// alongside the send (#1005). The handler APPLIES the damage
/// (`SetHP(GetHP() - damage * RuleR(Character, EnvironmentDamageMulipliter))`), having first
/// possibly scaled it by the environment-damage modifier and the spell/item/AA `ReduceFallDamage`
/// bonuses — or refused it entirely, deducting exactly 1 instead, for a GM, `GetInvul()`,
/// `GetInvulnerableEnvironmentDamage()`, or (with no HP change at all) in liquid on a zone with a
/// water map and in the tutorial/load zones.
///
/// Whether anything is sent BACK also differs by branch, and the two must not be collapsed: only
/// the branch that actually applies the damage falls through to `SendHPUpdate()`. Every refusal
/// returns before it, and `Mob::SetHP` sends nothing on its own. So on the GM / invulnerable
/// branches the correction reaches the client only when the 2 s `hpupdate_timer` poll next runs,
/// and only because that `-1` moved `current_hp` past `SendHPUpdate`'s change gate; on the liquid
/// and tutorial/load branches NO update is ever sent, because no HP ever changed. There is no
/// branch on which an authoritative number can be assumed to be coming promptly. Either way, the
/// value this function returns is what we ASK FOR, never what the player took, and it must not be
/// subtracted from any published HP. The wire and behaviour half of this is derived in
/// `swimming-and-fall-damage.md` in the private EQ knowledge-base tree (§3 "fall damage is
/// CLIENT-COMPUTED and entering water is NOT a fall", §4 gap analysis against this source tree).
///
/// Model, stated purely in terms of this function's own inputs/outputs and the constants defined
/// in its body below: `v = min(terminal, sqrt(2·gravity·max(height, 0)))`, then `score = v/hz − 4`.
/// `score` ≤ 0 → no damage; a `score` ≥ 9 branch returns a lethal (20000) pair; otherwise a roll in
/// `[0, score²·10]`. Returns (rolled_damage, max_damage).
///
/// **THAT LETHAL BRANCH IS UNREACHABLE AT THE CONSTANTS IN THE TREE TODAY, and this comment does
/// not ask you to take that on trust or re-derive it by hand** (#1058). `fall_damage_ceiling_tests`
/// below drives THIS function and pins the outcome: the lethal pair is never returned, and no input
/// — a swept range of heights, plus `f32::INFINITY`, `f32::MAX`, `f32::NAN`, negatives and `0.0` —
/// takes the returned pair above `(774, 774)`. A written-out derivation here would rot silently the
/// first time a constant moved; the test cannot, because it re-measures on every run. Read the test
/// for the bound, not this paragraph, and use [`fall_damage_ceiling`] in code rather than the
/// literal — it re-derives the number from this function instead of repeating it.
///
/// **WHICH READING OF THAT DISCREPANCY IS TRUE IS UNRESOLVED, so do not delete the branch to tidy
/// it away.** Either the curve never had a lethal case at these constants and the branch is
/// vestigial, or one of the three constants is miscalibrated — in which case the unreachable branch
/// is the surviving EVIDENCE of that and deleting it destroys the evidence. Nothing in this tree
/// can tell the two apart, and changing a constant to force one reading is a decision nobody has
/// taken. `todo.md`'s "exhaustive fall-damage testing (controlled-fall nav)" section settles it by
/// measurement — specifically its "Validate the fall-damage curve vs the real client across drop
/// heights" item. Until that runs, treat the ceiling as a property of THIS code, not of a fall.
///
/// The bound is not confined to this function: `eqoxide-nav`'s pre-emptive lethal-fall guard
/// compares `max_damage` against current HP, so it cannot fire at all above the ceiling. See
/// `walker::classify_ledge_fall`, which reports that case rather than passing it off as safe.
///
/// UNCITED, as a statement about the tree as it stands: no document in this repository, and none in
/// the private EQ knowledge-base tree, derives this curve or its constants. That is the operative
/// fact — treat every number here as unverified until someone re-derives it. #1005's `todo.md`
/// entry has the measurement plan.
///
/// The lineage, because it explains the gap rather than filling it: the curve and its constants
/// came from a reverse-engineering note that this repository once carried and deliberately removed
/// during the RoF2 retarget. The note was not carried into the knowledge-base tree either, so there
/// is nothing left to cite and the citation has NOT been repointed — a citation that resolves to a
/// merely-adjacent document would fail silently, which is worse than none. Knowing where the
/// numbers came from does not verify them, because the source is gone.
///
/// Two corollaries, so nobody re-litigates this:
/// - `swimming-and-fall-damage.md` is the real document for the wire and behaviour half above, but
///   it carries NO damage-curve content — no formula, no roll model, no constants — so it must not
///   be stretched to cover this curve.
/// - Do not attribute the curve to a named client to close the gap. The removed note was the only
///   thing that sourced the attribution this comment used to carry, and naming any client now would
///   be a fresh unsourced claim. Independently of where the curve came from, the opcode and struct
///   we actually send are RoF2's.
pub fn fall_damage(height: f32) -> (u32, u32) {
    // All three are UNCITED (see the doc note above). GRAVITY and TERMINAL repeat values that exist
    // elsewhere in the workspace; neither is linked to its twin, and TERMINAL is not the same
    // quantity as the controller's identically-valued `movement::MAX_FALL` (#1045).
    const GRAVITY: f32 = 120.0;   // private copy of the module GRAVITY, not a reference to it
    const TERMINAL: f32 = 128.0;  // damage-curve clamp on the derived impact velocity
    const HZ: f32 = 10.0;         // update rate the curve is calibrated to
    let v = (2.0 * GRAVITY * height.max(0.0)).sqrt().min(TERMINAL);
    let score = v / HZ - 4.0;
    if score <= 0.0 { return (0, 0); }
    if score >= 9.0 { return (20_000, 20_000); }
    let max = (score * score * 10.0) as u32;
    let roll = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos()).unwrap_or(0);
    (if max == 0 { 0 } else { roll % (max + 1) }, max)
}

/// The largest `max_damage` [`fall_damage`] can ever report, for ANY input — MEASURED off that
/// function, never a written-down copy of it.
///
/// Why infinity is the right probe, rather than a lucky guess: `fall_damage` clamps its derived
/// impact velocity to a terminal, and every step after that (`score`, then `score²·10`) is
/// non-decreasing in velocity, so `max_damage` is non-decreasing in `height` and SATURATES at the
/// clamp. `f32::INFINITY` therefore lands on exactly the same clamped velocity as every height at
/// or above the saturation point and reads the supremum off the live code path. That monotonicity
/// is not asserted here on the strength of this paragraph — `fall_damage_ceiling_tests` checks it
/// across the sweep, so if a future edit made the curve non-monotone this helper's justification
/// fails loudly instead of quietly returning a non-maximum.
///
/// The point of the indirection is that this cannot go stale. At the constants in the tree today
/// the value is 774, and no line of code here says 774 — re-derive `GRAVITY`, `TERMINAL` or `HZ`
/// (#1005, #1045) and every caller moves with them. Compare a damage threshold against this rather
/// than against a literal, and read [`fall_damage`]'s doc for why the number is a property of this
/// code and not yet a property of a real fall.
///
/// ```
/// use eqoxide_core::physics::{fall_damage, fall_damage_ceiling};
/// // No height, however absurd, is reported above the ceiling...
/// assert_eq!(fall_damage(f32::MAX).1, fall_damage_ceiling());
/// assert!(fall_damage(1e9).1 <= fall_damage_ceiling());
/// // ...and the documented lethal pair is never one of the outcomes (#1058).
/// assert_ne!(fall_damage(f32::INFINITY), (20_000, 20_000));
/// ```
pub fn fall_damage_ceiling() -> u32 {
    fall_damage(f32::INFINITY).1
}

/// The executable half of [`fall_damage`]'s doc comment (#1058).
///
/// The claim under test is that the function's documented `score >= 9 -> lethal (20000)` outcome
/// cannot occur at the constants the body defines, and that the real ceiling on the returned pair
/// is `(774, 774)`. It is a TEST rather than a sentence in the comment because a sentence stating
/// a derived bound is exactly the kind of claim that rots the first time somebody re-derives a
/// constant, with nothing to notice — and the whole reason #1058 is an honesty defect rather than
/// a tidiness one is that a stale statement about fall damage is a statement an agent plans on.
///
/// THE SWEEP'S OWN CONTROLS, because a check that only reports exceptions cannot tell "nothing
/// exceeded the bound" from "nothing was looked at": every assertion over the sweep is paired with
/// a positive one. The sample count is pinned, so an emptied corpus fails; the ceiling is asserted
/// ATTAINED, so a corpus that never reaches the velocity clamp fails; and the zero-damage,
/// saturation and monotonicity properties each name a height that must exhibit them.
#[cfg(test)]
mod fall_damage_ceiling_tests {
    use super::*;

    /// The bound the current constants produce. Written out ONCE, here, as the thing under test —
    /// production code reads [`fall_damage_ceiling`] instead, which derives it.
    const OBSERVED_CEILING: u32 = 774;

    /// Sweep resolution and extent. The extent deliberately runs far past the height at which the
    /// curve saturates, so the swept corpus contains the clamped region rather than approaching it.
    const SWEEP_STEP: f32 = 0.05;
    const SWEEP_MAX_U: f32 = 200.0;
    const SWEEP_SAMPLES: usize = 4001; // 0.0 ..= 200.0 inclusive, at 0.05u

    fn swept_heights() -> Vec<f32> {
        (0..SWEEP_SAMPLES).map(|i| i as f32 * SWEEP_STEP).collect()
    }

    /// Every pathological `f32` a caller can hand this function. `drop_to_target` in the nav walker
    /// is a subtraction of two published floats, so a NaN or an infinity is a real reachable input,
    /// not a theoretical one.
    fn pathological_heights() -> Vec<f32> {
        vec![
            0.0, -0.0, -1.0, -1e9, f32::MIN, f32::NEG_INFINITY,
            f32::MAX, f32::INFINITY, f32::NAN,
            f32::MIN_POSITIVE, f32::EPSILON,
        ]
    }

    /// The headline claim: across the whole corpus the lethal pair never appears and the returned
    /// pair never exceeds `(774, 774)`.
    ///
    /// Note the two assertions are NOT independent — the lethal pair is itself above the ceiling,
    /// so anything that produces it also breaks the bound. The lethal one is kept because it names
    /// the claim in #1058 directly, and so a failure says which sentence in the doc comment is now
    /// false rather than only that a number moved.
    #[test]
    fn no_input_reaches_the_lethal_branch_or_exceeds_the_ceiling() {
        let mut checked = 0usize;
        let mut attained = false;
        for h in swept_heights().into_iter().chain(pathological_heights()) {
            let (roll, max) = fall_damage(h);
            assert_ne!((roll, max), (20_000, 20_000),
                "fall_damage({h}) returned the documented lethal pair, which #1058 says the \
                 constants make unreachable — the doc comment and the constants now disagree");
            assert!(max <= OBSERVED_CEILING,
                "fall_damage({h}) reported max {max}, above the pinned ceiling {OBSERVED_CEILING}");
            assert!(roll <= max, "fall_damage({h}) rolled {roll} above its own max {max}");
            if max == OBSERVED_CEILING { attained = true; }
            checked += 1;
        }
        // The controls. Without these an empty or truncated corpus passes every line above.
        assert_eq!(checked, SWEEP_SAMPLES + pathological_heights().len(),
            "the corpus was not the one this test claims to have swept");
        assert!(attained,
            "the sweep never reached {OBSERVED_CEILING}, so 'nothing exceeded it' is vacuous — \
             the corpus does not contain the saturated region it claims to bound");
    }

    /// [`fall_damage_ceiling`] must equal the largest value actually observed over the corpus, not
    /// a number that happens to be written near it.
    #[test]
    fn ceiling_helper_equals_the_measured_maximum() {
        let observed = swept_heights().into_iter().chain(pathological_heights())
            .map(|h| fall_damage(h).1).max().expect("corpus must not be empty");
        assert_eq!(observed, OBSERVED_CEILING, "the swept corpus's maximum moved");
        assert_eq!(fall_damage_ceiling(), observed,
            "fall_damage_ceiling() must be derived from fall_damage, not a stale literal");
    }

    /// The property [`fall_damage_ceiling`]'s infinity probe rests on: `max_damage` is
    /// non-decreasing in height. If a future edit broke this, probing at infinity would silently
    /// stop returning the supremum.
    #[test]
    fn max_damage_is_monotone_non_decreasing_in_height() {
        let hs = swept_heights();
        assert!(hs.len() >= 2, "monotonicity needs at least one pair");
        let mut pairs = 0usize;
        let mut prev = fall_damage(hs[0]).1;
        for h in &hs[1..] {
            let cur = fall_damage(*h).1;
            assert!(cur >= prev, "max_damage fell from {prev} to {cur} at height {h}");
            prev = cur;
            pairs += 1;
        }
        assert_eq!(pairs, SWEEP_SAMPLES - 1, "monotonicity was not checked over the whole sweep");
        assert_eq!(prev, fall_damage_ceiling(),
            "the top of the sweep must already sit on the clamp the infinity probe reads");
    }

    /// The saturation point is inside the sweep and the curve is flat above it — the positive form
    /// of the bound, so this file cannot pass by never generating a large fall.
    #[test]
    fn the_curve_saturates_inside_the_swept_range() {
        // Below saturation the curve must still be climbing, or "flat above" would be trivially
        // true of a curve that is flat everywhere.
        assert!(fall_damage(30.0).1 < fall_damage(60.0).1,
            "the curve must still be rising below saturation");
        assert!(fall_damage(60.0).1 < OBSERVED_CEILING,
            "60u must sit below the ceiling, or the saturation point is not where this test thinks");
        for h in [69.0_f32, 100.0, 150.0, SWEEP_MAX_U, 1.0e9] {
            assert_eq!(fall_damage(h).1, OBSERVED_CEILING,
                "the curve must be flat at the ceiling above saturation, and is not at {h}");
        }
    }

    /// The doc comment above sends readers to three named referents. **A citation that no longer
    /// resolves does not fail loudly — it keeps reading as evidence**, which is the same class of
    /// defect #1058 itself is. These are the two this crate can reach; the third (`eqoxide-nav`'s
    /// `classify_ledge_fall`) is pinned from the nav side, which is the direction the dependency
    /// runs.
    #[test]
    fn the_doc_comments_citations_still_resolve() {
        const SRC: &str = include_str!("physics.rs");
        const TODO: &str = include_str!("../../../todo.md");
        // Non-degeneracy. Without these an empty or truncated corpus, or a `contains` that matched
        // anything, would pass every assertion below without looking at a thing.
        assert!(SRC.len() > 10_000, "physics.rs corpus looks truncated ({} bytes)", SRC.len());
        assert!(TODO.len() > 10_000, "todo.md corpus looks truncated ({} bytes)", TODO.len());
        // The control strings are ASSEMBLED, not written out: a literal absent-name control is a
        // contradiction in a test that searches its own file — the literal puts the name there.
        let absent_mod = format!("fall_damage_{}_tests", "basement");
        let absent_heading = format!("exhaustive fall-damage testing ({} free-fall)", "uncontrolled");
        assert!(!SRC.contains(&absent_mod),
            "control: a name that is NOT in physics.rs must not be found, or these searches prove nothing");
        assert!(!TODO.contains(&absent_heading),
            "control: a heading that is NOT in todo.md must not be found");

        // 1. The test module the doc tells readers to read for the bound.
        const TEST_MOD: &str = "fall_damage_ceiling_tests";
        assert!(SRC.contains(&format!("mod {TEST_MOD}")),
            "the doc cites `{TEST_MOD}`, and no module by that name exists in this file");
        assert!(SRC.contains(&format!("`{TEST_MOD}`")),
            "this module was renamed without updating the doc comment that sends readers to it");

        // 2. The todo.md section that settles the two readings, and the item inside it that does.
        const SECTION: &str = "## TODO: exhaustive fall-damage testing (controlled-fall nav)";
        const ITEM: &str = "Validate the fall-damage curve vs the real client across drop";
        assert!(SRC.contains("exhaustive fall-damage testing (controlled-fall nav)")
            && SRC.contains(ITEM), "the doc no longer names the todo.md section and item it cites");
        let sec = TODO.find(SECTION).unwrap_or_else(|| panic!("todo.md no longer has the section \
            the doc comment cites, verbatim: {SECTION:?}"));
        let item = TODO.find(ITEM).unwrap_or_else(|| panic!("todo.md no longer has the measurement \
            item the doc comment cites: {ITEM:?}"));
        // POSITION, not just presence: the item must live under that heading. A citation that
        // resolves to the right words under the wrong heading is the silent failure this guards.
        assert!(item > sec, "the cited item is above the section it is cited as belonging to");
        assert!(!TODO[sec + SECTION.len()..item].contains("\n## "),
            "another `## ` heading now sits between the cited section and the cited item, so the \
             item is no longer in that section");
    }

    /// The bottom of the curve, asserted positively as well as negatively: a short fall reports
    /// nothing, and the first height that reports anything is inside the swept range.
    #[test]
    fn short_and_invalid_falls_report_nothing() {
        for h in [0.0_f32, -0.0, 1.0, 6.0, -1.0, -1e9, f32::NEG_INFINITY, f32::MIN, f32::NAN] {
            assert_eq!(fall_damage(h), (0, 0), "fall_damage({h}) must report no damage");
        }
        // Positive control: damage does start somewhere inside the sweep, so the assertions above
        // are not passing because the function returns (0, 0) for everything.
        let first_damaging = swept_heights().into_iter().find(|h| fall_damage(*h).1 > 0)
            .expect("some swept height must report damage");
        assert!(first_damaging > 6.0 && first_damaging < 12.0,
            "the zero-damage cutoff moved: first damaging swept height was {first_damaging}");
    }
}
