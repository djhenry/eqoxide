//! Server-authoritative weather state + the CPU→GPU particle plan (eqoxide#542, Slice 1).
//!
//! Two pure pieces, both unit-tested here so they never need a GPU or a live server:
//!
//! 1. [`WeatherState`] — what the server told us the sky is doing, decoded from `OP_Weather`
//!    ([`WeatherState::from_wire`]). Type (none / rain / snow) + an intensity byte. Honest (the
//!    agent-honesty invariant): this reflects the REAL server weather or nothing — a short/invalid
//!    packet is dropped and the previous state is kept, never snapped to a fabricated storm.
//!
//! 2. [`particle_plan`] — maps a [`WeatherState`] to how many particles of which kind the renderer
//!    should draw. `None` → zero particles (the pass is skipped), so weather turning OFF on the wire
//!    turns precipitation off on screen with no separate teardown. Density scales with intensity.
//!
//! Slice 1 is VISUAL precipitation only. Weather SOUND is deferred (the client audio system is
//! unstarted, eqoxide#226); so are wind, puddles, accumulation, lightning and fog-coupling.

/// Kind of precipitation the server has active. `None` = clear (the default and the "weather off"
/// state). The wire distinguishes rain from snow; anything we don't recognize decodes to `None`
/// rather than a guessed default (honesty).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WeatherKind {
    /// Clear skies — no precipitation. Renders zero particles.
    #[default]
    None,
    /// Falling rain (vertical streaks).
    Rain,
    /// Drifting snow (soft flakes).
    Snow,
}

/// The current weather as last delivered by `OP_Weather`. Lives Model-side in
/// `WorldState::weather` (single-writer) and rides the render snapshot into the particle pass.
///
/// `intensity` is the server's raw intensity byte (0 = off / clear). Its magnitude scales particle
/// density in [`particle_plan`]; the wire semantics are documented on [`WeatherState::from_wire`].
/// `PartialEq`/`Eq` are derived and load-bearing — the render snapshot's identity is a
/// "did anything change" signal, and a steady storm must not thrash it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct WeatherState {
    pub kind: WeatherKind,
    /// Server intensity byte (a density knob, not a fixed enum). 0 means clear regardless of
    /// `kind`; the default rain/snow intensities are 1/2 but the GM `#weather 3` command and quest
    /// scripts can send arbitrary values, so this is stored raw and clamped only for density.
    pub intensity: u8,
}

/// Intensity at (or above) which the particle field is at [`MAX_PARTICLES`]. The server's default
/// rain/snow intensities are 1 and 2; treating 3 as "full" gives those a visible density ramp while
/// still saturating for the stronger `#weather 3 ...` / quest values. Purely a rendering knob — it
/// does NOT clamp the stored server value (that stays honest to the wire).
pub const FULL_DENSITY_INTENSITY: u8 = 3;

/// Upper bound on particles drawn at full intensity. Kept small (a few thousand) so the particle
/// pass stays cheap — this is a look-alike weather field, not a physical simulation.
pub const MAX_PARTICLES: u32 = 4000;

/// Minimum particle count for any active (non-clear) weather, so even intensity 1 reads as weather
/// rather than a sparse scattering.
pub const MIN_ACTIVE_PARTICLES: u32 = 1200;

/// How many particles of which kind the renderer should draw this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParticlePlan {
    pub kind: WeatherKind,
    pub count: u32,
}

impl WeatherState {
    /// Clear weather (no precipitation) — the default / "weather off" state.
    pub const CLEAR: WeatherState = WeatherState { kind: WeatherKind::None, intensity: 0 };

    /// True when there is precipitation to draw.
    pub fn is_active(&self) -> bool {
        self.kind != WeatherKind::None && self.intensity > 0
    }

    /// Decode the RoF2 `OP_Weather` payload (app opcode 0x661e). Returns `None` when the packet does
    /// NOT convey a new visible weather state — a short packet OR a zone-in handshake sentinel — so
    /// the caller keeps the previous weather rather than snapping to garbage or fabricating a storm.
    ///
    /// Wire layout, verified against the native RoF2 client's decode (the client reads only the
    /// first two little-endian `uint32`s; a trailing `mode`/`uint32` in the 12-byte zone-in variant
    /// is dead — never read):
    /// ```text
    /// /*00*/ uint32 val1;  // KIND: 0 = rain(-or-off), 2 = snowing, 1 = snow-off (only w/ intensity 0)
    /// /*04*/ uint32 type;  // INTENSITY: 0 = off/clear, otherwise strength (arbitrary; default 1 rain, 2 snow)
    /// ```
    /// Semantics (matching the native client so we render exactly what it would):
    /// - `val1 == 0xFFFF_FFFF` or `0x0000_00FF` is the **zone-in handshake sentinel** (a state-machine
    ///   trigger, not weather) → `None`, leaving the previous state untouched. The real initial
    ///   weather arrives immediately after as an ordinary 8-byte packet.
    /// - `type == 0` (intensity off) → clear ([`WeatherKind::None`]), regardless of `val1`.
    /// - otherwise `val1 == 2` → snow, everything else → rain (the client's exact rule: only 2 is
    ///   snow; kind 0 and any other value are rain). Intensity is the raw server byte.
    pub fn from_wire(bytes: &[u8]) -> Option<WeatherState> {
        if bytes.len() < 8 {
            return None;
        }
        let val1 = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let intensity_raw = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);

        // Zone-in handshake sentinel — advances the client's connect state machine, carries no
        // visible weather. Keep whatever weather we already had (never fabricate one).
        if val1 == 0xFFFF_FFFF || val1 == 0x0000_00FF {
            return None;
        }
        // intensity 0 = off; the kind byte is then meaningless → clear.
        if intensity_raw == 0 {
            return Some(WeatherState::CLEAR);
        }
        // Only kind byte 2 is snow; 0 (and anything else) is rain — the native client's exact rule.
        let kind = if (val1 & 0xFF) == 2 { WeatherKind::Snow } else { WeatherKind::Rain };
        let intensity = intensity_raw.min(u8::MAX as u32) as u8;
        Some(WeatherState { kind, intensity })
    }
}

/// Map a weather state to the particle draw plan. `None`/clear → zero particles (pass skipped);
/// otherwise a density between [`MIN_ACTIVE_PARTICLES`] and [`MAX_PARTICLES`] scaled by intensity.
///
/// This is the single source of truth for "how much precipitation" so it can be unit-tested without
/// a GPU: state in, particle count/kind out. Turning weather off (`None` or `intensity 0`) is just
/// `count == 0` here — the renderer skips the pass, which is the clean on/off transition.
pub fn particle_plan(w: &WeatherState) -> ParticlePlan {
    if !w.is_active() {
        return ParticlePlan { kind: WeatherKind::None, count: 0 };
    }
    let i = w.intensity.min(FULL_DENSITY_INTENSITY).max(1) as u32;
    let span = MAX_PARTICLES - MIN_ACTIVE_PARTICLES;
    // intensity 1 → MIN_ACTIVE_PARTICLES, intensity >= FULL_DENSITY_INTENSITY → MAX_PARTICLES,
    // linear between. Intensities above FULL_DENSITY_INTENSITY saturate (never exceed the cap).
    let count = MIN_ACTIVE_PARTICLES + span * (i - 1) / (FULL_DENSITY_INTENSITY as u32 - 1).max(1);
    ParticlePlan { kind: w.kind, count }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an OP_Weather payload: `kind`(val1)@0, `intensity`(type)@4. `extra` appends the dead
    /// trailing `mode` uint32 so we can also exercise the 12-byte zone-in variant.
    fn wire(kind: u32, intensity: u32, extra: bool) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&kind.to_le_bytes());
        v.extend_from_slice(&intensity.to_le_bytes());
        if extra {
            v.extend_from_slice(&0u32.to_le_bytes());
        }
        v
    }

    #[test]
    fn decode_rain() {
        // #weather 1 → val1(kind)=0, type(intensity)=1.
        let s = WeatherState::from_wire(&wire(0, 1, false)).unwrap();
        assert_eq!(s.kind, WeatherKind::Rain);
        assert_eq!(s.intensity, 1);
        assert!(s.is_active());
    }

    #[test]
    fn decode_snow() {
        // #weather 2 → val1(kind)=2, type(intensity)=2.
        let s = WeatherState::from_wire(&wire(2, 2, false)).unwrap();
        assert_eq!(s.kind, WeatherKind::Snow);
        assert_eq!(s.intensity, 2);
        assert!(s.is_active());
    }

    #[test]
    fn twelve_byte_variant_ignores_trailing_mode() {
        // The 12-byte zone-in-follow packet decodes identically (client never reads `mode`).
        let s = WeatherState::from_wire(&wire(2, 2, true)).unwrap();
        assert_eq!(s.kind, WeatherKind::Snow);
        assert_eq!(s.intensity, 2);
    }

    #[test]
    fn intensity_zero_is_clear_regardless_of_kind() {
        // #weather 0 → stop: intensity 0 with any kind byte must read as clear, never as rain/snow.
        assert_eq!(WeatherState::from_wire(&wire(0, 0, false)).unwrap(), WeatherState::CLEAR); // rain-off
        assert_eq!(WeatherState::from_wire(&wire(1, 0, false)).unwrap(), WeatherState::CLEAR); // snow-off sentinel
        assert_eq!(WeatherState::from_wire(&wire(2, 0, false)).unwrap(), WeatherState::CLEAR); // snow, off
        assert!(!WeatherState::from_wire(&wire(0, 0, false)).unwrap().is_active());
    }

    #[test]
    fn handshake_sentinel_keeps_previous_weather() {
        // The zone-in handshake OP_Weather (val1 = 0xFFFFFFFF or 0x000000FF) is a state-machine
        // trigger, not weather — it must NOT overwrite the current state (return None).
        assert!(WeatherState::from_wire(&wire(0xFFFF_FFFF, 5, false)).is_none());
        assert!(WeatherState::from_wire(&wire(0x0000_00FF, 5, true)).is_none());
    }

    #[test]
    fn non_snow_kind_is_rain_matching_native_client() {
        // The client's exact rule: only kind byte 2 is snow; kind 0 (and any other) is rain — never
        // silently dropped. With intensity>0 an unusual kind must still render (as rain), not vanish.
        let s = WeatherState::from_wire(&wire(7, 3, false)).unwrap();
        assert_eq!(s.kind, WeatherKind::Rain);
        assert_eq!(s.intensity, 3);
    }

    #[test]
    fn short_packet_is_dropped() {
        assert!(WeatherState::from_wire(&[0u8; 7]).is_none());
        assert!(WeatherState::from_wire(&[]).is_none());
    }

    #[test]
    fn arbitrary_intensity_is_preserved_not_hardcoded() {
        // #weather 3 / quest scripts send arbitrary intensity — it must survive decode intact.
        let s = WeatherState::from_wire(&wire(0, 200, false)).unwrap();
        assert_eq!(s.intensity, 200);
        assert_eq!(s.kind, WeatherKind::Rain);
    }

    #[test]
    fn plan_none_is_zero_particles() {
        assert_eq!(particle_plan(&WeatherState::CLEAR).count, 0);
        assert_eq!(particle_plan(&WeatherState::CLEAR).kind, WeatherKind::None);
        // kind set but intensity 0 is still clear → zero.
        let off = WeatherState { kind: WeatherKind::Rain, intensity: 0 };
        assert_eq!(particle_plan(&off).count, 0);
    }

    #[test]
    fn plan_scales_with_intensity() {
        let low = particle_plan(&WeatherState { kind: WeatherKind::Rain, intensity: 1 });
        let high = particle_plan(&WeatherState { kind: WeatherKind::Rain, intensity: FULL_DENSITY_INTENSITY });
        assert_eq!(low.count, MIN_ACTIVE_PARTICLES);
        assert_eq!(high.count, MAX_PARTICLES);
        assert!(low.count < high.count);
        assert_eq!(low.kind, WeatherKind::Rain);
    }

    #[test]
    fn plan_is_monotonic_nondecreasing_in_intensity() {
        let mut prev = 0u32;
        for i in 1..=FULL_DENSITY_INTENSITY {
            let c = particle_plan(&WeatherState { kind: WeatherKind::Snow, intensity: i }).count;
            assert!(c >= prev, "count must not decrease as intensity rises");
            assert!(c <= MAX_PARTICLES, "count must never exceed MAX_PARTICLES");
            prev = c;
        }
    }

    #[test]
    fn plan_never_exceeds_cap_even_if_intensity_overflows_range() {
        // Defense-in-depth: a very large intensity (e.g. from `#weather 3 0 200`) must saturate at
        // MAX_PARTICLES, never index past the particle buffer.
        let c = particle_plan(&WeatherState { kind: WeatherKind::Rain, intensity: 255 }).count;
        assert_eq!(c, MAX_PARTICLES);
    }
}
