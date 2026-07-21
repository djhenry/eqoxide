//! Time-of-day server clock + sky-gradient palette (eqoxide#561, Slice 1).
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
//!    four phases (dawn / day / dusk / night). The phase breakpoints and the per-phase colors are
//!    the native outdoor "DefaultClear" day-cycle values; the concrete figures live in the project's
//!    private technical KB (they are derived from the commercial client's data files and are not
//!    reproduced verbatim in this public repo beyond what is needed to render a look-alike gradient).
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

// ── Per-phase palettes (native DefaultClear family; concrete values in the private KB) ───────────
// Day is a hair off the pre-#561 hardcoded gradient on purpose — that gradient already approximated
// the native Day phase, so reusing near-identical stops keeps daytime looking unchanged.
fn day_colors() -> SkyColors {
    SkyColors { zenith: rgb(90, 131, 178), horizon: rgb(206, 209, 233) }
}
fn night_colors() -> SkyColors {
    // Flat deep blue — native Night map has no vertical gradient.
    SkyColors { zenith: rgb(4, 17, 42), horizon: rgb(4, 17, 42) }
}
fn dawn_colors() -> SkyColors {
    // Zenith stays near night-blue through dawn; horizon a warm sunrise glow (a 2-stop
    // simplification of the native alpha-composited dawn map — full fidelity is Slice 2).
    SkyColors { zenith: rgb(19, 42, 74), horizon: rgb(233, 166, 110) }
}
fn dusk_colors() -> SkyColors {
    SkyColors { zenith: rgb(27, 44, 74), horizon: rgb(232, 162, 92) }
}

// ── Phase breakpoints, in fractional hours (native DefaultClear ColorSet Time/Transition track) ──
// Each phase's transition BEGINS at *_START and reaches full color at *_END; between the end of one
// transition and the start of the next the flat phase color holds. Night holds from NIGHT_END
// through to the next day's DAWN_START (the long overnight branch). Concrete source values are in
// the private technical KB.
const DAWN_START: f32 = 5.639_64;
const DAWN_END: f32 = 6.479_35;
const DAY_START: f32 = 6.719_98;
const DAY_END: f32 = 7.199_71;
const DUSK_START: f32 = 16.799_93;
const DUSK_END: f32 = 17.279_66;
const NIGHT_START: f32 = 17.759_76;
const NIGHT_END: f32 = 18.479_74;

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
        // Sanity: the four flat holds are the four distinct phases in order.
        assert_eq!(sky_colors(3.0), night_colors());
        assert_eq!(sky_colors(6.6), dawn_colors());
        assert_eq!(sky_colors(12.0), day_colors());
        assert_eq!(sky_colors(17.5), dusk_colors());
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
}
