//! Runs an Agent Plugin `Call` through the in-process route table.
//!
//! An `op` is `"<METHOD> <path>"` (e.g. `"GET /v1/inventory"`); `args`
//! may carry `query` (an object of query parameters) and `body` (the JSON request body). The
//! response status is mapped to a `CallStatus` so a refusal is never reported as `Ok`.

use crate::OpState;
use eqoxide_agent_protocol::call::{Call, CallStatus};
use serde_json::Value;

#[derive(Clone)]
pub struct ApiDispatcher {
    ops: crate::wire::Ops,
    state: OpState,
}

impl ApiDispatcher {
    pub(crate) fn new(state: &OpState) -> Self {
        Self {
            ops: crate::v1_ops(),
            state: state.clone(),
        }
    }

    /// Runs `call` to completion. Never panics on a malformed call: it answers `Invalid`.
    pub async fn dispatch(&self, call: Call) -> (CallStatus, Value) {
        if self.ops.contains(&call.op) {
            return self.dispatch_native(&call).await;
        }
        (CallStatus::NotFound, Value::String(format!("no such op: {:?}", call.op)))
    }
}

impl ApiDispatcher {
    async fn dispatch_native(&self, call: &Call) -> (CallStatus, Value) {
        let query = match call.args.get("query") {
            None | Some(Value::Null) => serde_json::Map::new(),
            Some(Value::Object(o)) => o.clone(),
            Some(_) => return (CallStatus::Invalid, Value::String("args.query must be an object".into())),
        };
        let body = call.args.get("body").filter(|b| !b.is_null()).cloned();
        let req = crate::wire::Req::new(query, body);
        let ops = self.ops.clone();
        let name = call.op.clone();
        let state = self.state.clone();
        let run: crate::wire::ReplyFuture = {
            let (ops, name, state) = (ops, name.clone(), state.clone());
            Box::pin(async move {
                ops.run(&name, state, req).await.expect("contains() was checked")
            })
        };
        let reply = crate::actions::track_op(state, &name, run).await;
        (map_status_code(reply.status.as_u16()), reply.body)
    }
}

fn map_status_code(code: u16) -> CallStatus {
    match code {
        202 => CallStatus::Accepted,
        200..=299 => CallStatus::Ok,
        404 => CallStatus::NotFound,
        409 => CallStatus::Conflict,
        503 => CallStatus::Unavailable,
        400..=499 => CallStatus::Invalid,
        _ => CallStatus::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(op: &str, args: Value) -> Call {
        Call {
            id: 1,
            op: op.into(),
            args,
        }
    }

    #[test]
    fn status_mapping_never_reports_a_refusal_as_ok() {
        use crate::wire::StatusCode as S;
        let m = |s: S| map_status_code(s.as_u16());
        assert_eq!(m(S::OK), CallStatus::Ok);
        assert_eq!(m(S::ACCEPTED), CallStatus::Accepted);
        assert_eq!(m(S::BAD_REQUEST), CallStatus::Invalid);
        assert_eq!(m(S::UNPROCESSABLE_ENTITY), CallStatus::Invalid);
        assert_eq!(m(S::NOT_FOUND), CallStatus::NotFound);
        assert_eq!(m(S::CONFLICT), CallStatus::Conflict);
        assert_eq!(m(S::SERVICE_UNAVAILABLE), CallStatus::Unavailable);
        assert_eq!(m(S::INTERNAL_SERVER_ERROR), CallStatus::Failed);
    }

    #[tokio::test]
    async fn a_native_op_is_tracked_and_a_refusal_is_a_conflict_not_ok() {
        let state = crate::testkit::empty_state();
        let d = ApiDispatcher::new(&state);
        let (first, body) = d.dispatch(call("POST /v1/trainer/close", Value::Null)).await;
        assert_eq!(first, CallStatus::Accepted, "{body}");
        assert!(body["request_id"].as_u64().is_some(), "an admitted action carries its id: {body}");
        let (second, _) = d.dispatch(call("POST /v1/trainer/close", Value::Null)).await;
        assert_eq!(second, CallStatus::Conflict, "the undrained slot must refuse, never answer Ok");
    }

    #[tokio::test]
    async fn a_real_op_answers_through_the_dispatcher_and_an_unknown_one_is_not_found() {
        let d = ApiDispatcher::new(&crate::testkit::empty_state());
        let (status, body) = d
            .dispatch(call("GET /v1/observe/skills", Value::Null))
            .await;
        assert_eq!(status, CallStatus::Ok, "{body}");
        assert!(body.is_object(), "{body}");
        let (status, _) = d.dispatch(call("GET /v1/no/such/route", Value::Null)).await;
        assert_eq!(status, CallStatus::NotFound);
    }
}
