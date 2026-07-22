//! Time-of-day server clock + sky-gradient palette (eqoxide#561 Slice 1; palette/breakpoints
//! re-derived from native measurements in eqoxide#628).
//!
//! Two pure pieces, both unit-tested here so they never need a GPU or a live server:
//!
//! 1. [`EqClock`] — the world clock the server hands us in `OP_TimeOfDay`. The wire struct carries
//!    NO epoch, and the packet is sent only on zone-entry / GM time-edit (never periodically), so
//!    the only honest model is: snap to the received `(hour, minute, …)` and extrapolate locally at
//!    the fixed EQ rate of **1 EQ-minute = 3 real seconds** until the next packet. `received_at`
//!    anchors that extrapolation. This mirrors `EQTime::GetCurrentEQTimeOfDay` on the server, which
//!    likewise recomputes from `(now − start)` rather than storing a ticking value.
//!
//! 2. [`sky_colors`] — maps the resulting hour-of-day to a 2-stop (zenith, horizon) gradient across
//!    four phases (dawn / day / dusk / night). #561/#583 shipped this with estimated colors and
//!    breakpoints, made before any side-by-side comparison against the native client existed. #628
//!    measured the native client at 11 hours in the same zone/spot/camera and found the estimate
//!    substantially wrong — night ~5x too bright, no dawn/dusk phases at all, transitions lagging
//!    1-2h. The palette and breakpoints below are re-derived directly from that measurement table
//!    (see the per-function derivation comments); the table itself, and the specific numbers below,
//!    are public (already published in the #628 issue body) — no native client internals beyond
//!    that aggregate luma/RGB data are reproduced here.
//!
//! Honesty (the agent-honesty invariant): the clock reflects real server time or nothing. Until an
//! `OP_TimeOfDay` has been received the clock is `None`; callers render [`DEFAULT_HOUR`] (a documented
//! daytime default that matches the sky's pre-#561 look) — never a fabricated "current" server time.

use std::time::{Duration, Instant};

/// Real seconds per EQ minute (native rate: 72 real minutes = one 24-hour EQ day).
pub const REAL_SECONDS_PER_EQ_MINUTE: f32 = 3.0;

/// Hour-of-day rendered when no `OP_TimeOfDay` has arrived yet. Noon → the Day phase, which matches
/// the flat blue gradient the sky showed before #561. This is an explicit fallback for "server time
/// unknown", NOT a claim that it is noon — see the honesty note in the module docs.
pub const DEFAULT_HOUR: f32 = 12.0;

/// The world clock as last delivered by `OP_TimeOfDay`, plus the instant we received it so the
/// current time can be extrapolated locally between packets.
///
/// `hour` is **1-based** on the wire (1..=24, where 24 = the final/midnight hour), exactly as the
/// RoF2 `TimeOfDay_Struct` sends it; [`hour_of_day`](EqClock::hour_of_day) normalizes it to a 0..24
/// clock. `PartialEq` is derived and load-bearing: this rides inside the render snapshot, whose Arc
/// identity is a "did anything change" signal — `received_at` only changes when a new packet lands,
/// so an idle clock keeps the snapshot stable while the rendered gradient still advances (the render
/// side calls `hour_of_day()` fresh each frame off `received_at.elapsed()`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqClock {
    /// 1-based hour, 1..=24 (24 = midnight hour), straight off the wire.
    pub hour: u8,
    pub minute: u8,
    pub day: u8,
    pub month: u8,
    pub year: u16,
    /// When this snapshot was received — the anchor for local extrapolation.
    pub received_at: Instant,
}

impl EqClock {
    /// Parse the 8-byte RoF2 `OP_TimeOfDay` payload (opcode 0x5070, straight passthrough — no RoF2
    /// ENCODE/DECODE override), anchoring `received_at` to now. Returns `None` on a short or clearly
    /// invalid packet rather than snapping the clock to garbage (honesty: no faked time).
    ///
    /// Layout: `hour:u8, minute:u8, day:u8, month:u8, year:u16, unknown:u16`.
    pub fn from_wire(bytes: &[u8]) -> Option<EqClock> {
        Self::from_wire_at(bytes, Instant::now())
    }

    /// [`from_wire`](EqClock::from_wire) with an injectable receipt instant, for deterministic tests.
    pub fn from_wire_at(bytes: &[u8], now: Instant) -> Option<EqClock> {
        if bytes.len() < 8 {
            return None;
        }
        let hour = bytes[0];
        let minute = bytes[1];
        let day = bytes[2];
        let month = bytes[3];
        let year = u16::from_le_bytes([bytes[4], bytes[5]]);
        // Reject values the wire struct can never legitimately carry — a corrupt/misrouted packet
        // must not silently move the clock to an impossible time.
        if !(1..=24).contains(&hour) || minute >= 60 {
            return None;
        }
        Some(EqClock { hour, minute, day, month, year, received_at: now })
    }

    /// Current hour-of-day as a 0.0..24.0 fraction, extrapolated from the snapshot using the real
    /// time elapsed since receipt. Wraps across midnight.
    pub fn hour_of_day(&self) -> f32 {
        self.hour_of_day_after(self.received_at.elapsed())
    }

    /// [`hour_of_day`](EqClock::hour_of_day) for an explicit elapsed real-time duration, so the tick
    /// math is unit-testable without sleeping.
    pub fn hour_of_day_after(&self, elapsed: Duration) -> f32 {
        // 1-based hour → 0-based minute-of-day. hour 24 (midnight hour) maps to 0.
        let base_min = ((self.hour as u32 - 1) % 24) * 60 + self.minute as u32;
        let eq_minutes_elapsed = elapsed.as_secs_f32() / REAL_SECONDS_PER_EQ_MINUTE;
        let mut total = base_min as f32 + eq_minutes_elapsed;
        total = total.rem_euclid(1440.0); // wrap the 24-hour (1440-minute) day
        total / 60.0
    }
}

/// A 2-stop vertical sky gradient: `zenith` (top) and `horizon` (bottom), each linear-ish sRGB
/// components in 0.0..1.0 (component = native 0..255 value / 255, matching the pre-#561 shader).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkyColors {
    pub zenith: [f32; 3],
    pub horizon: [f32; 3],
}

const fn rgb(r: u8, g: u8, b: u8) -> [f32; 3] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0]
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn mix(a: SkyColors, b: SkyColors, t: f32) -> SkyColors {
    SkyColors {
        zenith: lerp3(a.zenith, b.zenith, t),
        horizon: lerp3(a.horizon, b.horizon, t),
    }
}

// ── Per-phase palettes (eqoxide#628: re-derived from the native-vs-eqoxide measurement table in ──
// the issue body, NOT estimates — every number below traces to a specific table entry or an
// algebraic solve against it; see the derivation notes on each function. The original #561/#583
// palette was an estimate made before any native comparison existed and measured up to 5x too
// bright at night with the dawn/dusk phases entirely absent (#628 defects 1 and 2); this palette
// replaces it. Luma is `Y = 0.299R + 0.587G + 0.114B` on the native 0..255 scale throughout.
//
// Only Y (no RGB breakdown) is given in the issue for day/night, since those hours aren't called
// out as having a distinctive hue — so day/night below keep their previous hue *direction*
// (already a reasonable native-informed estimate, not contradicted by the issue) and are uniformly
// brightness-scaled to match the table's measured Y. Dawn and dusk, where the issue calls out a
// specific hue signature, are matched to the exact (dusk) or algebraically-completed (dawn) native
// RGB triple and kept FLAT (zenith == horizon): the issue's samples are single fixed-camera points,
// not a two-stop breakdown, so a non-flat split for those two phases would fabricate an unverified
// gradient ratio the table cannot support. Restoring a genuine vertical gradient for dawn/dusk (and
// true celestial-body color) is Slice 2 (sky dome) territory, out of scope here.
fn day_colors() -> SkyColors {
    // Native Y at 06/07/13 = 147.8 / 153.7 / 149.0 (mean 150.17). Previous zenith/horizon pair
    // averaged to Y=167.47; scaled by 150.17/167.47 ≈ 0.8964 (same hue direction, dimmed to match):
    // zenith (90,131,178)→(81,117,160), horizon (206,209,233)→(185,187,209). Resulting avg
    // Y=150.02, within 3.7 of every one of the three measured hours.
    SkyColors { zenith: rgb(81, 117, 160), horizon: rgb(185, 187, 209) }
}
fn night_colors() -> SkyColors {
    // Native night Y across 00/04/18/19/20/22 = 12.2/12.8/14.8/15.1/12.7/12.3 (mean 13.32). Kept
    // flat (no vertical gradient) per the pre-existing note that the native Night map has none.
    // Previous flat color (4,17,42) had the right hue but Y=15.96 (~3-4 too bright vs the mean, and
    // 5x too bright is what was actually observed live — see the PR body for why the *rendered*
    // number diverged even further from this constant). Scaled by 13.32/15.96 ≈ 0.8457, same hue:
    // (4,17,42)→(3,14,36), giving Y=13.22 — within 2.5 of every individual measured hour's Y (worst
    // case hour 19, native 15.1, diff 1.9).
    SkyColors { zenith: rgb(3, 14, 36), horizon: rgb(3, 14, 36) }
}
fn dawn_colors() -> SkyColors {
    // Native hour 05 (the pink-dawn sample): Y=69.1, R=86.7 > G=60.0 given directly; B was not
    // given, so solved from the luma equation: 69.1 = 0.299*86.7 + 0.587*60.0 + 0.114*B →
    // B ≈ 69.8. That makes the true order R(86.7) > B(69.8) > G(60.0) — a magenta-pink dawn, which
    // matches "pink" better than a plain warm-orange sunrise would. Rounded to (87, 60, 70), flat
    // (see module-level derivation note above for why flat): Y=69.21, R>G holds (87>60).
    SkyColors { zenith: rgb(87, 60, 70), horizon: rgb(87, 60, 70) }
}
fn dusk_colors() -> SkyColors {
    // Native hour 17 (the purple-dusk sample) gives the full triple directly: R=57.2, G=49.6,
    // B=74.7 (Y=54.7, and 0.299*57.2+0.587*49.6+0.114*74.7 = 54.73, self-consistent). Rounded to
    // (57, 50, 75), flat (see module-level derivation note above): Y=54.94, order B>R>G holds
    // (75>57>50) exactly as measured. The previous dusk color (232,162,92 horizon / 27,44,74
    // zenith) averaged to R>G>B — a warm sunset, the *wrong* hue direction entirely; this is why
    // #628 could report "no dusk at all" even though a `dusk_colors()` existed.
    SkyColors { zenith: rgb(57, 50, 75), horizon: rgb(57, 50, 75) }
}

// ── Phase breakpoints, in fractional hours ─────────────────────────────────────────────────────
// eqoxide#628 defect 3: native transitions ~1-2h earlier than the old breakpoints (native is
// already well into dawn by 05:00 and back to full night by 18:00; the old breakpoints didn't even
// start the dawn ramp until 05:38 and didn't finish returning to night until 18:29). Re-derived so
// hour 5.0 and hour 17.0 (the only hours the issue gives a hue signature for) land inside the FLAT
// dawn/dusk hold windows (not mid-transition, where the color would be a blend the issue never
// measured), and hour 6.0 / 18.0 (the next transition boundary) land at or past the transition's
// end, matching the near-day (147.8) / near-night (14.8) values measured there. No sub-hour native
// samples exist, so the exact transition widths (currently a symmetric ~0.6-0.7h ramp either side
// of the anchor hour) are a reasonable interpolation between measured integer hours, not a measured
// quantity themselves — a denser native sweep (as the issue suggests) would let a future pass pin
// these tighter.
const DAWN_START: f32 = 4.2;
const DAWN_END: f32 = 4.8;
const DAY_START: f32 = 5.3;
const DAY_END: f32 = 6.0;
const DUSK_START: f32 = 16.0;
const DUSK_END: f32 = 16.7;
const NIGHT_START: f32 = 17.3;
const NIGHT_END: f32 = 18.0;

/// Map an hour-of-day (0.0..24.0) to the sky gradient for that moment, interpolating across the
/// dawn/day/dusk/night transitions.
pub fn sky_colors(hour: f32) -> SkyColors {
    let hour = hour.rem_euclid(24.0);
    let (day, night, dawn, dusk) = (day_colors(), night_colors(), dawn_colors(), dusk_colors());
    if hour < DAWN_START {
        night
    } else if hour < DAWN_END {
        mix(night, dawn, (hour - DAWN_START) / (DAWN_END - DAWN_START))
    } else if hour < DAY_START {
        dawn
    } else if hour < DAY_END {
        mix(dawn, day, (hour - DAY_START) / (DAY_END - DAY_START))
    } else if hour < DUSK_START {
        day
    } else if hour < DUSK_END {
        mix(day, dusk, (hour - DUSK_START) / (DUSK_END - DUSK_START))
    } else if hour < NIGHT_START {
        dusk
    } else if hour < NIGHT_END {
        mix(dusk, night, (hour - NIGHT_START) / (NIGHT_END - NIGHT_START))
    } else {
        night
    }
}

impl SkyColors {
    /// Sky colors for a clock that may be unset: the live extrapolated hour if we have a server
    /// clock, else [`DEFAULT_HOUR`].
    pub fn for_clock(clock: Option<&EqClock>) -> SkyColors {
        sky_colors(clock.map(|c| c.hour_of_day()).unwrap_or(DEFAULT_HOUR))
    }

    /// Pull the horizon stop `t` of the way toward the zone's distance-fog color so the sky-to-fog
    /// seam at the bottom of the screen isn't a hard edge. Slice-1 cosmetic blend only — native fog
    /// and sky are independently authored (see the KB) — so `t` is a small fraction (~0.3–0.5).
    pub fn with_horizon_fog(mut self, fog_rgb: [f32; 3], t: f32) -> SkyColors {
        self.horizon = lerp3(self.horizon, fog_rgb, t);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Wire decode ──────────────────────────────────────────────────────────────────────────
    #[test]
    fn from_wire_parses_all_fields() {
        // hour=13 (1pm), minute=37, day=5, month=9, year=3000, +pad
        let bytes = [13u8, 37, 5, 9, 0xB8, 0x0B, 0, 0];
        let c = EqClock::from_wire_at(&bytes, Instant::now()).expect("valid");
        assert_eq!(c.hour, 13);
        assert_eq!(c.minute, 37);
        assert_eq!(c.day, 5);
        assert_eq!(c.month, 9);
        assert_eq!(c.year, 3000); // 0x0BB8
    }

    #[test]
    fn from_wire_rejects_short_packet() {
        assert!(EqClock::from_wire_at(&[13, 37, 5, 9], Instant::now()).is_none());
    }

    #[test]
    fn from_wire_rejects_impossible_hour_and_minute() {
        // hour 0 is invalid (1-based), hour 25 invalid, minute 60 invalid.
        assert!(EqClock::from_wire_at(&[0, 0, 1, 1, 0, 0, 0, 0], Instant::now()).is_none());
        assert!(EqClock::from_wire_at(&[25, 0, 1, 1, 0, 0, 0, 0], Instant::now()).is_none());
        assert!(EqClock::from_wire_at(&[12, 60, 1, 1, 0, 0, 0, 0], Instant::now()).is_none());
    }

    #[test]
    fn from_wire_accepts_hour_24() {
        // hour 24 is the top of the 1-based range and must be accepted. Under the spec formula
        // `(hour-1)%24`, EQ hour N is the Nth hour of the day → 0-based clock N-1, so EQ 24 → 23.0
        // (the 23:00 hour), and EQ 1 is the midnight hour → 0.0.
        let c = EqClock::from_wire_at(&[24, 0, 1, 1, 0, 0, 0, 0], Instant::now()).expect("valid");
        assert!((c.hour_of_day_after(Duration::ZERO) - 23.0).abs() < 1e-4);
    }

    // ── Tick math ────────────────────────────────────────────────────────────────────────────
    fn clock(hour: u8, minute: u8) -> EqClock {
        EqClock { hour, minute, day: 1, month: 1, year: 0, received_at: Instant::now() }
    }

    #[test]
    fn hour_of_day_is_1based_at_receipt() {
        // EQ hour 13 (1pm) with 0 elapsed → 12.0 on the 0..24 clock ((13-1)%24).
        assert!((clock(13, 0).hour_of_day_after(Duration::ZERO) - 12.0).abs() < 1e-4);
        // EQ hour 1 → 0.0; EQ hour 12 → 11.0.
        assert!((clock(1, 0).hour_of_day_after(Duration::ZERO) - 0.0).abs() < 1e-4);
        assert!((clock(12, 30).hour_of_day_after(Duration::ZERO) - 11.5).abs() < 1e-4);
    }

    #[test]
    fn tick_rate_is_one_eq_minute_per_three_real_seconds() {
        // 180 real seconds = 60 EQ minutes = 1 EQ hour. From EQ hour 13 (→12.0) advance one hour.
        let c = clock(13, 0);
        let after = c.hour_of_day_after(Duration::from_secs(180));
        assert!((after - 13.0).abs() < 1e-3, "expected 13.0, got {after}");
        // 3 real seconds = 1 EQ minute = 1/60 hour.
        let after_min = c.hour_of_day_after(Duration::from_secs(3));
        assert!((after_min - (12.0 + 1.0 / 60.0)).abs() < 1e-3, "got {after_min}");
    }

    #[test]
    fn tick_wraps_past_midnight() {
        // EQ hour 24, minute 30 → 0-based clock 23.5h. Advance one EQ hour (60 EQ min = 180 real s)
        // → 24.5h, which must wrap through midnight to 0.5h.
        let c = clock(24, 30); // → 23.5h
        let after = c.hour_of_day_after(Duration::from_secs(180));
        assert!((after - 0.5).abs() < 1e-3, "expected wrap to 0.5, got {after}");
    }

    // ── Phase / palette mapping ──────────────────────────────────────────────────────────────
    #[test]
    fn midday_is_flat_day_phase() {
        let c = sky_colors(12.0);
        assert_eq!(c, day_colors());
    }

    #[test]
    fn deep_night_is_flat_night_phase() {
        assert_eq!(sky_colors(2.0), night_colors());
        assert_eq!(sky_colors(22.0), night_colors());
        // Just before dawn transition and just after night transition both hold night.
        assert_eq!(sky_colors(DAWN_START - 0.1), night_colors());
        assert_eq!(sky_colors(NIGHT_END + 0.1), night_colors());
    }

    #[test]
    fn dawn_and_dusk_hold_their_flat_phase_between_transitions() {
        // Between DAWN_END and DAY_START it is flat dawn; between DUSK_END and NIGHT_START flat dusk.
        assert_eq!(sky_colors((DAWN_END + DAY_START) / 2.0), dawn_colors());
        assert_eq!(sky_colors((DUSK_END + NIGHT_START) / 2.0), dusk_colors());
    }

    #[test]
    fn dawn_transition_interpolates_night_to_dawn() {
        // Midpoint of the dawn transition → halfway between night and dawn on both stops.
        let mid = (DAWN_START + DAWN_END) / 2.0;
        let c = sky_colors(mid);
        let expect = mix(night_colors(), dawn_colors(), 0.5);
        for i in 0..3 {
            assert!((c.zenith[i] - expect.zenith[i]).abs() < 1e-3);
            assert!((c.horizon[i] - expect.horizon[i]).abs() < 1e-3);
        }
        // The transition really moves the color — start != end.
        assert_ne!(sky_colors(DAWN_START), sky_colors(DAWN_END - 1e-3));
    }

    #[test]
    fn phase_sequence_is_night_dawn_day_dusk_night_over_the_day() {
        // Sanity: the four flat holds are the four distinct phases in order. Hours 5.0/17.0 are
        // the exact native-measured dawn/pink and dusk/purple sample hours (eqoxide#628); with the
        // re-derived breakpoints they now land inside the flat dawn/dusk windows instead of the old
        // (too-late) breakpoints, which held night through hour 5 and day through hour 17.
        assert_eq!(sky_colors(3.0), night_colors());
        assert_eq!(sky_colors(5.0), dawn_colors());
        assert_eq!(sky_colors(12.0), day_colors());
        assert_eq!(sky_colors(17.0), dusk_colors());
        assert_eq!(sky_colors(20.0), night_colors());
        // Day, night, dawn, dusk are all visually distinct.
        assert_ne!(day_colors(), night_colors());
        assert_ne!(dawn_colors(), dusk_colors());
    }

    #[test]
    fn for_clock_uses_default_hour_when_unset() {
        assert_eq!(SkyColors::for_clock(None), sky_colors(DEFAULT_HOUR));
        assert_eq!(SkyColors::for_clock(None), day_colors()); // DEFAULT_HOUR = noon = Day
    }

    #[test]
    fn horizon_fog_blend_moves_only_horizon_toward_fog() {
        let base = day_colors();
        let fog = [0.0, 0.0, 0.0];
        let blended = base.with_horizon_fog(fog, 0.5);
        assert_eq!(blended.zenith, base.zenith); // zenith untouched
        for i in 0..3 {
            assert!((blended.horizon[i] - base.horizon[i] * 0.5).abs() < 1e-4);
        }
    }

    // ── eqoxide#628 regression: match the native measurement table, not just "directionally right" ─
    // The #561/#583 palette was an estimate; #628 measured the native RoF2 client at 11 hours
    // (same zone, spot, fixed camera, weather cleared) and found it substantially wrong. These
    // tests assert the *specific* native numbers from the #628 issue body, with tolerances tight
    // enough to fail the pre-#628 palette (verified: see the PR body for the exact before/after
    // numbers run against this same test on unmodified `main`).

    /// `Y = 0.299R + 0.587G + 0.114B` on the native 0..255 scale, matching the issue's luma formula.
    fn luma_255(c: [f32; 3]) -> f32 {
        (0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]) * 255.0
    }

    /// The issue's per-hour number is a single fixed-camera-pixel sample; our model exposes a
    /// 2-stop gradient. Average the two stops' luma as the closest available proxy — documented
    /// approximation, not a claim the sample point was literally the midpoint.
    fn sky_luma_255(c: SkyColors) -> f32 {
        (luma_255(c.zenith) + luma_255(c.horizon)) / 2.0
    }

    #[test]
    fn native_hour_table_matches_measured_luma_eqoxide_628() {
        // (hour, native Y, tolerance) — every (hour, native Y) pair is copied verbatim from the
        // table in the eqoxide#628 issue body. Tolerances are derived from the phase colors' own
        // fit quality (see the derivation comments on day_colors/night_colors/dawn_colors/
        // dusk_colors above) — e.g. night's flat color is at most 1.9 off any individual sampled
        // night hour, so 2.5 gives headroom without being vacuous.
        let cases: &[(f32, f32, f32)] = &[
            (0.0, 12.2, 2.5),
            (4.0, 12.8, 2.5),
            (5.0, 69.1, 2.0),
            (6.0, 147.8, 5.0),
            (7.0, 153.7, 5.0),
            (13.0, 149.0, 5.0),
            (17.0, 54.7, 2.0),
            (18.0, 14.8, 2.5),
            (19.0, 15.1, 2.5),
            (20.0, 12.7, 2.5),
            (22.0, 12.3, 2.5),
        ];
        for &(hour, native_y, tol) in cases {
            let y = sky_luma_255(sky_colors(hour));
            assert!(
                (y - native_y).abs() <= tol,
                "hour {hour}: got Y={y:.1}, native Y={native_y} (tol {tol})"
            );
        }
    }

    #[test]
    fn dawn_and_dusk_hue_ordering_matches_native_eqoxide_628() {
        // eqoxide#628's two qualitative "signature" facts, asserted on the actual rendered
        // gradient (not just the raw color constants) so a breakpoint regression that moved hour
        // 5/17 back into night/day would also fail this: at 05:00 native red exceeds green (pink
        // dawn); at 17:00 native blue exceeds red exceeds green (purple dusk).
        let dawn = sky_colors(5.0);
        assert!(dawn.horizon[0] > dawn.horizon[1], "dawn horizon should have R>G: {:?}", dawn.horizon);
        assert!(dawn.zenith[0] > dawn.zenith[1], "dawn zenith should have R>G: {:?}", dawn.zenith);

        let dusk = sky_colors(17.0);
        assert!(
            dusk.horizon[2] > dusk.horizon[0] && dusk.horizon[0] > dusk.horizon[1],
            "dusk horizon should have B>R>G: {:?}",
            dusk.horizon
        );
        assert!(
            dusk.zenith[2] > dusk.zenith[0] && dusk.zenith[0] > dusk.zenith[1],
            "dusk zenith should have B>R>G: {:?}",
            dusk.zenith
        );
    }

    #[test]
    fn eleven_sampled_hours_show_more_than_three_distinct_colors_eqoxide_628() {
        // eqoxide#628 defect 2: "eqoxide has only three distinct sky values across 11 sampled
        // hours where native grades continuously". Assert the fix produces strictly more than
        // three distinct SkyColors across the issue's exact 11 sampled hours (night, dawn, day,
        // dusk = 4, the minimum that actually reflects a 4-phase cycle).
        let hours = [0.0, 4.0, 5.0, 6.0, 7.0, 13.0, 17.0, 18.0, 19.0, 20.0, 22.0];
        let mut distinct: Vec<SkyColors> = Vec::new();
        for h in hours {
            let c = sky_colors(h);
            if !distinct.contains(&c) {
                distinct.push(c);
            }
        }
        assert!(
            distinct.len() > 3,
            "expected more than 3 distinct sky colors across the 11 native-sampled hours, got {}",
            distinct.len()
        );
    }
}
