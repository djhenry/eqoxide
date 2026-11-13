//! `Step` — one frame of agent input (spec §7). Movement is continuous and always present or
//! absent as a whole; the discrete verb (Task 4/5/6) layers on top independently.

use crate::movement::AgentMovement;
use crate::verb::AgentVerb;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub movement: Option<AgentMovement>,
    pub verb: Option<AgentVerb>,
    /// Caller-chosen id of this `Step`'s verb, echoed in the `VerbResult` that answers it. Absent
    /// means the caller does not want an answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// One-shot operations to run, each answered by a `CallReply` in a later `Observation`. Absent
    /// on the wire when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<crate::call::Call>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{decode_line, encode_line};

    #[test]
    fn step_with_movement_round_trips() {
        let s = Step {
            movement: Some(AgentMovement {
                dir: [1.0, 0.0],
                up: 0.0,
                jump: false,
                wish_heading: None,
            }),
            verb: None,
            seq: None,
            calls: vec![],
        };
        let line = encode_line(&s).unwrap();
        let back: Step = decode_line(&line).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn step_with_no_movement_round_trips() {
        let s = Step {
            movement: None,
            verb: None,
            seq: None,
            calls: vec![],
        };
        let line = encode_line(&s).unwrap();
        let back: Step = decode_line(&line).unwrap();
        assert_eq!(back, s);
    }
}
