//! Continuous per-tick movement payload (spec §6, §7): the agent's raw directional/heading control,
//! mirroring `eqoxide_ipc::ManualMove`'s fields plus the new `wish_heading`. eqoxide latches this and
//! keeps applying it every tick until a newer one arrives — "held key" semantics, not a discrete verb.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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
}
