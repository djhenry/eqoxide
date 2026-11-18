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

use crate::op::Op;
use crate::reply::{ErrorBody, Reply};

/// A request to eqoxide. `op` is one [`Op`] variant, written on the wire as `"op": "<group>.<action>"`
/// with its arguments as one object in `"args"` (the op list is in eqoxide's `docs/agent-ops.md`).
///
/// Reading a `Call` never fails on a bad `op` or bad `args`. The call reads as [`Op::Unreadable`]
/// carrying the reason, so the host answers it `Invalid` and the rest of the step still runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// Caller-chosen, echoed in the `CallReply`. Must be unique among the caller's calls in flight.
    pub id: u64,
    pub op: Op,
}

impl Serialize for Call {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let (name, args) = self
            .op
            .to_wire()
            .ok_or_else(|| serde::ser::Error::custom("an unreadable op has no wire form"))?;
        let mut map = serializer.serialize_map(Some(if args.is_some() { 3 } else { 2 }))?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("op", &name)?;
        if let Some(args) = &args {
            map.serialize_entry("args", args)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Call {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let Value::Object(mut fields) = Value::deserialize(deserializer)? else {
            return Err(D::Error::custom("a call must be a JSON object"));
        };
        let id = fields
            .remove("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| D::Error::custom("a call needs a numeric `id`"))?;
        let name = fields.remove("op");
        let args = fields.remove("args").filter(|a| !a.is_null());
        let op = match (name, fields.keys().next()) {
            (Some(Value::String(name)), None) => {
                Op::from_wire(&name, args).unwrap_or_else(|why| Op::Unreadable(why.to_string()))
            }
            (_, Some(extra)) => Op::Unreadable(format!("unknown field `{extra}` in a call")),
            _ => Op::Unreadable("a call needs an `op` name".to_owned()),
        };
        Ok(Call { id, op })
    }
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
/// and the reason for every refusal. [`CallReply::typed`] reads either as a type from
/// [`crate::reply`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallReply {
    pub id: u64,
    pub status: CallStatus,
    pub body: Value,
}

impl CallReply {
    /// Reads `body` as the typed answer to `op`, which is a [`Reply`] for `Ok` and `Accepted` and an
    /// [`ErrorBody`] for every other status. Returns the error if `body` does not fit `op`.
    pub fn typed(&self, op: &Op) -> Result<Result<Reply, ErrorBody>, serde_json::Error> {
        match self.status {
            CallStatus::Ok | CallStatus::Accepted => Reply::decode(op, self.body.clone()).map(Ok),
            _ => ErrorBody::decode(self.body.clone()).map(Err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{decode_line, encode_line};
    use serde_json::json;

    #[test]
    fn a_reply_reads_as_the_type_of_the_op_that_was_sent() {
        let ok = CallReply { id: 1, status: CallStatus::Ok, body: json!({"count": 0, "messages": [], "snapshot_age_ms": 5}) };
        match ok.typed(&Op::ObserveMessages(crate::op::ObserveMessages { kind: None })).unwrap() {
            Ok(Reply::Messages(m)) => assert_eq!(m.count, 0),
            other => panic!("unexpected {other:?}"),
        }
        let refused = CallReply { id: 2, status: CallStatus::Conflict, body: json!("busy") };
        assert_eq!(refused.typed(&Op::MerchantClose).unwrap(), Err(ErrorBody::Message("busy".into())));
        let wrong = CallReply { id: 3, status: CallStatus::Ok, body: json!("text") };
        assert!(wrong.typed(&Op::ObserveInventory).is_err());
    }

    #[test]
    fn call_wire_shape_is_pinned() {
        let c = Call { id: 7, op: Op::TrainerTrain(crate::op::TrainerTrain { skill_id: 5 }) };
        assert_eq!(
            encode_line(&c).unwrap(),
            "{\"id\":7,\"op\":\"trainer.train\",\"args\":{\"skill_id\":5}}\n"
        );
        let back: Call = decode_line(&encode_line(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn call_without_args_omits_them() {
        let c = Call { id: 1, op: Op::ObserveWho };
        assert_eq!(encode_line(&c).unwrap(), "{\"id\":1,\"op\":\"observe.who\"}\n");
        let back: Call = decode_line("{\"id\":1,\"op\":\"observe.who\"}\n").unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn a_bad_op_reads_as_unreadable_with_the_reason() {
        let call: Call = decode_line("{\"id\":4,\"op\":\"no.such\"}\n").unwrap();
        assert_eq!(call.id, 4);
        assert!(matches!(call.op, Op::Unreadable(_)));
        let call: Call =
            decode_line("{\"id\":5,\"op\":\"trainer.train\",\"args\":{\"skill_id\":\"x\"}}\n").unwrap();
        let Op::Unreadable(why) = call.op else { panic!("not unreadable") };
        assert!(why.contains("skill_id"), "{why}");
    }

    #[test]
    fn a_call_without_an_id_is_refused() {
        assert!(decode_line::<Call>("{\"op\":\"observe.who\"}\n").is_err());
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
