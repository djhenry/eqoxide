//! What eqoxide answers a `Step`'s verb with. A verb is fire-and-forget on the wire, but the agent
//! must still be told when eqoxide declined it, with the reason: answering "ok" for something that
//! did not and cannot happen is the falsehood class #952, #1002 and #347 were filed against.

use serde::{Deserialize, Serialize};

/// The outcome of one verb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "reason")]
pub enum VerbOutcome {
    /// eqoxide took the verb and handed it to the game. This is not a claim that the game did it:
    /// the server can still refuse, and what the server did shows up in later `Observation`s.
    Accepted,
    /// eqoxide did not act on the verb, and says why. Nothing was sent to the server.
    Refused(String),
}

/// The outcome of the verb a `Step` carried, echoing the `Step`'s `seq` so the agent can tell which
/// of its verbs it answers. Carried by the first `Observation` built after the verb was dispatched,
/// and by no other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerbResult {
    pub seq: u64,
    #[serde(flatten)]
    pub outcome: VerbOutcome,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{decode_line, encode_line};

    #[test]
    fn accepted_wire_shape_is_pinned() {
        let r = VerbResult { seq: 3, outcome: VerbOutcome::Accepted };
        assert_eq!(encode_line(&r).unwrap(), "{\"seq\":3,\"outcome\":\"Accepted\"}\n");
    }

    #[test]
    fn refused_wire_shape_is_pinned_and_round_trips() {
        let r = VerbResult { seq: 4, outcome: VerbOutcome::Refused("the character is dead".into()) };
        let line = encode_line(&r).unwrap();
        assert_eq!(line, "{\"seq\":4,\"outcome\":\"Refused\",\"reason\":\"the character is dead\"}\n");
        assert_eq!(decode_line::<VerbResult>(&line).unwrap(), r);
    }
}
