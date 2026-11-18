//! Per-entity velocity sampled from the server position updates of a remote entity.

use std::time::{Duration, Instant};

/// Shortest time between two stored position samples. An observation closer than this to the
/// stored sample is ignored, so a burst of updates cannot produce a velocity from a tiny `dt`.
pub const MOTION_MIN_DT: Duration = Duration::from_millis(50);

/// Age of the stored sample beyond which [`EntityMotion::velocity_at`] reports zero. An entity
/// that has stopped sending position updates is no longer moving at its last measured velocity.
pub const MOTION_STALE: Duration = Duration::from_secs(3);

/// The last stored position sample of an entity and the velocity measured when it was stored.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EntityMotion {
    sample: Option<Sample>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Sample {
    pos: [f32; 3],
    at: Instant,
    velocity: [f32; 3],
}

impl EntityMotion {
    /// Stores `pos` observed at `now` and measures the velocity from the previous sample.
    ///
    /// The first observation stores a sample with zero velocity, and so does an observation more
    /// than [`MOTION_STALE`] after the stored sample. An observation less than [`MOTION_MIN_DT`]
    /// after the stored sample is ignored. Any other observation sets the velocity to the
    /// displacement from the stored sample over the elapsed seconds, then replaces the stored
    /// sample.
    pub fn observe(&mut self, pos: [f32; 3], now: Instant) {
        let velocity = match self.sample {
            None => [0.0; 3],
            Some(prev) => {
                let dt = now.saturating_duration_since(prev.at);
                if dt < MOTION_MIN_DT {
                    return;
                }
                if dt > MOTION_STALE {
                    [0.0; 3]
                } else {
                    let secs = dt.as_secs_f32();
                    [
                        (pos[0] - prev.pos[0]) / secs,
                        (pos[1] - prev.pos[1]) / secs,
                        (pos[2] - prev.pos[2]) / secs,
                    ]
                }
            }
        };
        self.sample = Some(Sample { pos, at: now, velocity });
    }

    /// Returns the measured velocity in world units per second at `now`, or zero when there is
    /// no stored sample or the stored sample is older than [`MOTION_STALE`].
    pub fn velocity_at(&self, now: Instant) -> [f32; 3] {
        match self.sample {
            Some(s) if now.saturating_duration_since(s.at) <= MOTION_STALE => s.velocity,
            _ => [0.0; 3],
        }
    }

    /// Returns the position of the stored sample, or `None` before the first observation.
    pub fn sampled_pos(&self) -> Option<[f32; 3]> {
        self.sample.map(|s| s.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn first_observation_has_zero_velocity() {
        let t0 = Instant::now();
        let mut motion = EntityMotion::default();
        motion.observe([0.0, 0.0, 0.0], t0);
        assert_eq!(motion.velocity_at(t0), [0.0; 3]);
        assert_eq!(motion.sampled_pos(), Some([0.0, 0.0, 0.0]));
    }

    #[test]
    fn velocity_is_displacement_over_elapsed_time() {
        let t0 = Instant::now();
        let mut motion = EntityMotion::default();
        motion.observe([0.0, 0.0, 0.0], t0);
        motion.observe([10.0, -5.0, 0.0], t0 + SECOND);
        assert_eq!(motion.velocity_at(t0 + SECOND), [10.0, -5.0, 0.0]);
    }

    #[test]
    fn velocity_ages_out_to_zero() {
        let t0 = Instant::now();
        let mut motion = EntityMotion::default();
        motion.observe([0.0, 0.0, 0.0], t0);
        motion.observe([10.0, -5.0, 0.0], t0 + SECOND);
        let stale = t0 + SECOND + MOTION_STALE + Duration::from_millis(1);
        assert_eq!(motion.velocity_at(stale), [0.0; 3]);
    }

    #[test]
    fn samples_closer_than_min_dt_are_ignored() {
        let t0 = Instant::now();
        let mut motion = EntityMotion::default();
        motion.observe([0.0, 0.0, 0.0], t0);
        let early = t0 + Duration::from_millis(5);
        motion.observe([500.0, 0.0, 0.0], early);
        assert_eq!(motion.velocity_at(early), [0.0; 3]);
        assert_eq!(motion.sampled_pos(), Some([0.0, 0.0, 0.0]));

        motion.observe([10.0, 0.0, 0.0], t0 + SECOND);
        assert_eq!(motion.velocity_at(t0 + SECOND), [10.0, 0.0, 0.0]);
    }

    #[test]
    fn an_observation_after_a_long_gap_has_zero_velocity() {
        let t0 = Instant::now();
        let mut motion = EntityMotion::default();
        motion.observe([0.0, 0.0, 0.0], t0);
        let returned = t0 + Duration::from_secs(60);
        motion.observe([600.0, 0.0, 0.0], returned);
        assert_eq!(motion.velocity_at(returned), [0.0; 3]);
        assert_eq!(motion.sampled_pos(), Some([600.0, 0.0, 0.0]));

        motion.observe([610.0, 0.0, 0.0], returned + SECOND);
        assert_eq!(motion.velocity_at(returned + SECOND), [10.0, 0.0, 0.0]);
    }

    #[test]
    fn a_stationary_resample_reports_zero() {
        let t0 = Instant::now();
        let mut motion = EntityMotion::default();
        motion.observe([3.0, 4.0, 5.0], t0);
        motion.observe([3.0, 4.0, 5.0], t0 + SECOND);
        assert_eq!(motion.velocity_at(t0 + SECOND), [0.0; 3]);
    }
}
