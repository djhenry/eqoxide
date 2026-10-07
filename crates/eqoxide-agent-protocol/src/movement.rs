//! Continuous per-tick movement payload (spec §6, §7): the agent's raw directional/heading control,
//! mirroring `eqoxide_ipc::ManualMove`'s fields plus the new `wish_heading`. eqoxide latches this and
//! keeps applying it every tick until a newer one arrives — "held key" semantics, not a discrete verb.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMovement {
    /// World `(east, north)` direction. Any magnitude; the plugin host normalizes it. Zero = stand
    /// in place (e.g. a jump with no movement) — same convention as `ManualMove::dir`.
    pub dir: [f32; 2],
    /// Vertical axis, `-1..1`. Only has an effect while swimming or on a climbable (auto-detected
    /// from the character's position, not something the agent selects) — same as `ManualMove::up`.
    pub up: f32,
    pub jump: bool,
    /// Independently-settable heading (spec §6): melee requires facing the target server-side, so
    /// this can be set even while strafing sideways or standing still. `None` falls back to the
    /// direction-derived heading (facing the way you walk), matching the pre-existing WASD/manual
    /// behavior when the agent doesn't care.
    pub wish_heading: Option<f32>,
}

impl AgentMovement {
    /// Returns `true` if every field holds a finite value. A `Step` carrying a non-finite `dir`,
    /// `up`, or `wish_heading` (overflowed from the wire's f64 into `f32::INFINITY`, or sent as
    /// `NaN` directly) must be rejected before it reaches the character controller: the controller
    /// integrates position from these fields every tick, so one non-finite value permanently
    /// corrupts the session's `GameState` position from that tick onward.
    pub fn is_finite(&self) -> bool {
        self.dir[0].is_finite()
            && self.dir[1].is_finite()
            && self.up.is_finite()
            && self.wish_heading.map_or(true, f32::is_finite)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{decode_line, encode_line};

    #[test]
    fn agent_movement_round_trips_with_heading() {
        let m = AgentMovement { dir: [1.0, 0.0], up: 0.0, jump: false, wish_heading: Some(90.0) };
        let line = encode_line(&m).unwrap();
        let back: AgentMovement = decode_line(&line).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn agent_movement_round_trips_without_heading() {
        let m = AgentMovement { dir: [0.0, 0.0], up: 1.0, jump: true, wish_heading: None };
        let line = encode_line(&m).unwrap();
        let back: AgentMovement = decode_line(&line).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn is_finite_accepts_ordinary_values() {
        let m = AgentMovement { dir: [1.0, -1.0], up: 0.5, jump: false, wish_heading: Some(90.0) };
        assert!(m.is_finite());
    }

    #[test]
    fn is_finite_rejects_a_non_finite_dir() {
        let m = AgentMovement { dir: [f32::INFINITY, 0.0], up: 0.0, jump: false, wish_heading: None };
        assert!(!m.is_finite());
    }

    #[test]
    fn is_finite_rejects_a_nan_up() {
        let m = AgentMovement { dir: [0.0, 0.0], up: f32::NAN, jump: false, wish_heading: None };
        assert!(!m.is_finite());
    }

    #[test]
    fn is_finite_rejects_a_non_finite_wish_heading() {
        let m = AgentMovement {
            dir: [0.0, 0.0],
            up: 0.0,
            jump: false,
            wish_heading: Some(f32::INFINITY),
        };
        assert!(!m.is_finite());
    }

    #[test]
    fn a_step_with_an_unknown_field_is_rejected() {
        let result: Result<AgentMovement, _> =
            decode_line("{\"dir\":[0.0,0.0],\"up\":0.0,\"jump\":false,\"wish_heading\":null,\"bogus\":1}\n");
        assert!(result.is_err());
    }
}
