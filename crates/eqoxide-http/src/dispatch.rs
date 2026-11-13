//! Runs an Agent Plugin `Call` through the in-process route table.
//!
//! Transitional: it lets every route be reached over the agent socket before the routes themselves
//! are rewritten without HTTP. An `op` is `"<METHOD> <path>"` (e.g. `"GET /v1/inventory"`); `args`
//! may carry `query` (an object of query parameters) and `body` (the JSON request body). The
//! response status is mapped to a `CallStatus` so a refusal is never reported as `Ok`.

use crate::{v1_router, HttpState};
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use eqoxide_agent_protocol::call::{Call, CallStatus};
use serde_json::Value;
use tower::ServiceExt;

/// Largest response body read back from a route. The biggest honest reply (a full entity list) is
/// well under this.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct ApiDispatcher {
    router: Router,
}

impl ApiDispatcher {
    pub(crate) fn new(state: &HttpState) -> Self {
        Self {
            router: v1_router(state).with_state(state.clone()),
        }
    }

    /// Runs `call` to completion. Never panics on a malformed call: it answers `Invalid`.
    pub async fn dispatch(&self, call: Call) -> (CallStatus, Value) {
        let request = match build_request(&call) {
            Ok(r) => r,
            Err(why) => return (CallStatus::Invalid, Value::String(why)),
        };
        let response = match self.router.clone().oneshot(request).await {
            Ok(r) => r,
            Err(never) => match never {},
        };
        let status = response.status();
        let bytes = match axum::body::to_bytes(response.into_body(), MAX_RESPONSE_BYTES).await {
            Ok(b) => b,
            Err(e) => {
                return (
                    CallStatus::Failed,
                    Value::String(format!("the reply could not be read: {e}")),
                )
            }
        };
        let body = serde_json::from_slice::<Value>(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (map_status(status), body)
    }
}

fn map_status(status: StatusCode) -> CallStatus {
    match status.as_u16() {
        202 => CallStatus::Accepted,
        200..=299 => CallStatus::Ok,
        404 => CallStatus::NotFound,
        409 => CallStatus::Conflict,
        503 => CallStatus::Unavailable,
        400..=499 => CallStatus::Invalid,
        _ => CallStatus::Failed,
    }
}

fn build_request(call: &Call) -> Result<Request<Body>, String> {
    let (method, path) = call
        .op
        .split_once(' ')
        .ok_or_else(|| format!("op {:?} is not \"<METHOD> <path>\"", call.op))?;
    let method = match method {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "DELETE" => Method::DELETE,
        other => return Err(format!("unsupported method {other:?}")),
    };
    if !path.starts_with("/v1/") {
        return Err(format!("path {path:?} is not under /v1/"));
    }
    let mut uri = path.to_string();
    if let Some(query) = call.args.get("query") {
        let obj = query.as_object().ok_or("args.query must be an object")?;
        let mut ser = form_urlencoded::Serializer::new(String::new());
        for (k, v) in obj {
            let text = match v {
                Value::String(s) => s.clone(),
                Value::Null => continue,
                other => other.to_string(),
            };
            ser.append_pair(k, &text);
        }
        let q = ser.finish();
        if !q.is_empty() {
            uri.push('?');
            uri.push_str(&q);
        }
    }
    let builder = Request::builder().method(method).uri(uri);
    match call.args.get("body") {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => builder.body(Body::empty()),
    }
    .map_err(|e| format!("the request is not valid: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(op: &str, args: Value) -> Call {
        Call {
            id: 1,
            op: op.into(),
            args,
        }
    }

    #[test]
    fn status_mapping_never_reports_a_refusal_as_ok() {
        assert_eq!(map_status(StatusCode::OK), CallStatus::Ok);
        assert_eq!(map_status(StatusCode::ACCEPTED), CallStatus::Accepted);
        assert_eq!(map_status(StatusCode::BAD_REQUEST), CallStatus::Invalid);
        assert_eq!(
            map_status(StatusCode::UNPROCESSABLE_ENTITY),
            CallStatus::Invalid
        );
        assert_eq!(map_status(StatusCode::NOT_FOUND), CallStatus::NotFound);
        assert_eq!(map_status(StatusCode::CONFLICT), CallStatus::Conflict);
        assert_eq!(
            map_status(StatusCode::SERVICE_UNAVAILABLE),
            CallStatus::Unavailable
        );
        assert_eq!(
            map_status(StatusCode::INTERNAL_SERVER_ERROR),
            CallStatus::Failed
        );
    }

    #[test]
    fn malformed_ops_are_refused_before_reaching_a_route() {
        assert!(build_request(&call("inventory", Value::Null)).is_err());
        assert!(build_request(&call("PATCH /v1/x", Value::Null)).is_err());
        assert!(build_request(&call("GET /other", Value::Null)).is_err());
        assert!(build_request(&call("GET /v1/x", json!({"query": 3}))).is_err());
    }

    #[test]
    fn query_args_become_a_query_string() {
        let r = build_request(&call(
            "GET /v1/observe/who",
            json!({"query": {"a": 1, "b": "x y"}}),
        ))
        .unwrap();
        assert_eq!(r.uri().to_string(), "/v1/observe/who?a=1&b=x+y");
        assert_eq!(r.method(), Method::GET);
    }

    #[tokio::test]
    async fn a_real_route_answers_through_the_dispatcher_and_an_unknown_one_is_not_found() {
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
