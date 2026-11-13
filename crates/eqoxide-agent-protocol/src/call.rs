//! One-shot request/response operations: everything an agent can ask eqoxide that is not movement
//! or one of the per-tick verbs (reading inventory, quest logs, the chat log; trading with a
//! merchant; group, guild and social actions). A `Call` rides in a `Step`; its `CallReply` comes
//! back in a later `Observation`, matched by `id`.
//!
//! A call can take seconds (a merchant purchase waits for the server's echo), so the host runs each
//! one to completion on its own and answers it whenever it finishes, in any order relative to other
//! calls. The reply is never `Ok` for something that did not happen: every refusal carries a status
//! naming the kind of refusal and a body saying why.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A request to eqoxide. `op` is `"<METHOD> <path>"`, e.g. `"GET /v1/observe/inventory"` (the op
/// list is in eqoxide's `docs/agent-ops.md`); `args` is `{"query": {...}, "body": {...}}`, either
/// key optional. Both are validated by the host, never trusted: an unknown `op` is answered
/// `NotFound` and a malformed `args` `Invalid`, never dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    /// Caller-chosen, echoed in the `CallReply`. Must be unique among the caller's calls in flight.
    pub id: u64,
    pub op: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub args: Value,
}

/// What kind of answer a call got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallStatus {
    /// The operation happened (a read returned its data, or a write was done and settled).
    Ok,
    /// eqoxide took the request and handed it to the game, which has not confirmed it yet. What the
    /// server did shows up in later reads.
    Accepted,
    /// The request is malformed, or names something that cannot be done in the current state.
    /// Retrying the same request will not help.
    Invalid,
    /// The thing the request names (an item, a spawn, a quest) does not exist.
    NotFound,
    /// The request is valid but conflicts with something in flight or in the game's current state.
    /// Nothing was done; retrying later may succeed.
    Conflict,
    /// The game cannot act now (no live session, assets not loaded). Nothing was done.
    Unavailable,
    /// Anything else. The body says what.
    Failed,
}

/// The answer to one `Call`. `body` is the operation's result (a JSON value) for `Ok`/`Accepted`,
/// and the reason (a string) for every refusal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallReply {
    pub id: u64,
    pub status: CallStatus,
    pub body: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{decode_line, encode_line};
    use serde_json::json;

    #[test]
    fn call_wire_shape_is_pinned() {
        let c = Call {
            id: 7,
            op: "inventory.move".into(),
            args: json!({"from": 23, "to": 30}),
        };
        assert_eq!(
            encode_line(&c).unwrap(),
            "{\"id\":7,\"op\":\"inventory.move\",\"args\":{\"from\":23,\"to\":30}}\n"
        );
        let back: Call = decode_line(&encode_line(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn call_without_args_omits_them_and_reads_back_null() {
        let c = Call {
            id: 1,
            op: "observe.who".into(),
            args: Value::Null,
        };
        assert_eq!(
            encode_line(&c).unwrap(),
            "{\"id\":1,\"op\":\"observe.who\"}\n"
        );
        let back: Call = decode_line("{\"id\":1,\"op\":\"observe.who\"}\n").unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn call_rejects_unknown_fields() {
        assert!(decode_line::<Call>("{\"id\":1,\"op\":\"x\",\"seq\":2}\n").is_err());
    }

    #[test]
    fn reply_wire_shapes_are_pinned() {
        let ok = CallReply {
            id: 7,
            status: CallStatus::Ok,
            body: json!({"slots": []}),
        };
        assert_eq!(
            encode_line(&ok).unwrap(),
            "{\"id\":7,\"status\":\"Ok\",\"body\":{\"slots\":[]}}\n"
        );
        let refused = CallReply {
            id: 8,
            status: CallStatus::Conflict,
            body: json!("a move is already queued"),
        };
        assert_eq!(
            encode_line(&refused).unwrap(),
            "{\"id\":8,\"status\":\"Conflict\",\"body\":\"a move is already queued\"}\n"
        );
        let back: CallReply = decode_line(&encode_line(&refused).unwrap()).unwrap();
        assert_eq!(back, refused);
    }
}
