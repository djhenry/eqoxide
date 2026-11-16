//! The transport-free request and result types the agent ops are written against.
//!
//! An op is an async function `(OpState, Req) -> OpResult` registered under its name
//! (`"POST /v1/trainer/open"`). The agent socket's `Call` carries the name and its `args`
//! (`query`, `body`). An `OpError` maps one to one onto a `CallStatus`, and nothing here listens on
//! a socket or speaks HTTP.

use crate::ops::OpState;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use eqoxide_agent_protocol::call::CallStatus;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// The reason an op refused or could not finish. Each variant carries the reply body, which is a
/// JSON string for a plain refusal and a JSON object where the op documents one.
#[derive(Clone, Debug, PartialEq)]
pub enum OpError {
    /// The arguments are malformed or name something out of range.
    Invalid(Value),
    /// The call names something that does not exist.
    NotFound(Value),
    /// The call is valid but the game state refuses it now.
    Conflict(Value),
    /// The session cannot serve the call.
    Unavailable(Value),
    /// The op failed for a reason the agent cannot correct.
    Failed(Value),
}

impl OpError {
    /// The `CallStatus` this error answers with.
    pub fn status(&self) -> CallStatus {
        match self {
            Self::Invalid(_) => CallStatus::Invalid,
            Self::NotFound(_) => CallStatus::NotFound,
            Self::Conflict(_) => CallStatus::Conflict,
            Self::Unavailable(_) => CallStatus::Unavailable,
            Self::Failed(_) => CallStatus::Failed,
        }
    }

    /// The reply body.
    pub fn body(&self) -> &Value {
        match self {
            Self::Invalid(b) | Self::NotFound(b) | Self::Conflict(b) | Self::Unavailable(b) | Self::Failed(b) => b,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(Value::String(message.into()))
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(Value::String(message.into()))
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(Value::String(message.into()))
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(Value::String(message.into()))
    }
}

/// An op's successful answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Done {
    /// The op finished and the body says how.
    Ok(Value),
    /// The op queued an action that the game has not confirmed.
    Accepted(Value),
}

impl Done {
    /// The `CallStatus` this answer reports.
    pub fn status(&self) -> CallStatus {
        match self {
            Self::Ok(_) => CallStatus::Ok,
            Self::Accepted(_) => CallStatus::Accepted,
        }
    }

    /// The reply body.
    pub fn body(&self) -> &Value {
        match self {
            Self::Ok(b) | Self::Accepted(b) => b,
        }
    }
}

/// What every op returns.
pub type OpResult = Result<Done, OpError>;

/// The `CallStatus` and body that answer `result`.
pub fn settle(result: OpResult) -> (CallStatus, Value) {
    match result {
        Ok(Done::Ok(body)) => (CallStatus::Ok, body),
        Ok(Done::Accepted(body)) => (CallStatus::Accepted, body),
        Err(e) => (e.status(), e.body().clone()),
    }
}

/// A success with a JSON body.
pub fn ok(body: Value) -> OpResult {
    Ok(Done::Ok(body))
}

/// A success with a text body.
pub fn ok_text(message: impl Into<String>) -> OpResult {
    Ok(Done::Ok(Value::String(message.into())))
}

/// A queued action the game has not confirmed.
pub fn accepted(body: Value) -> OpResult {
    Ok(Done::Accepted(body))
}

/// A refusal for malformed arguments.
pub fn invalid<T>(message: impl Into<String>) -> Result<T, OpError> {
    Err(OpError::invalid(message))
}

/// A refusal for something that does not exist.
pub fn not_found<T>(message: impl Into<String>) -> Result<T, OpError> {
    Err(OpError::not_found(message))
}

/// A refusal for game state that rejects the call now.
pub fn conflict<T>(message: impl Into<String>) -> Result<T, OpError> {
    Err(OpError::conflict(message))
}

/// A refusal for a session that cannot serve the call.
pub fn unavailable<T>(message: impl Into<String>) -> Result<T, OpError> {
    Err(OpError::unavailable(message))
}

/// One call's arguments.
#[derive(Clone, Debug, Default)]
pub struct Req {
    pub(crate) query: Map<String, Value>,
    pub(crate) body: Option<Value>,
    /// For an op registered with [`Ops::add_prefix`]: the part of the path after the prefix.
    pub(crate) tail: Option<String>,
}

impl Req {
    pub fn new(query: Map<String, Value>, body: Option<Value>) -> Self {
        Self { query, body, tail: None }
    }

    /// A request carrying only a JSON body.
    pub fn with_body(body: Value) -> Self {
        Self { query: Map::new(), body: Some(body), tail: None }
    }

    /// A request carrying only query parameters.
    pub fn with_query(query: Value) -> Self {
        Self { query: query.as_object().cloned().unwrap_or_default(), body: None, tail: None }
    }

    /// The body as `T`. An absent body and a body that does not fit `T` are both `Err`; the caller
    /// words the refusal (each route names its own required fields).
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, String> {
        match &self.body {
            None => Err("a JSON body is required".into()),
            Some(v) => serde_path_to_error::deserialize(v).map_err(|e| e.to_string()),
        }
    }

    /// The body as `T`, or `None` when none was sent. A body that was sent and does not fit `T` is
    /// a refusal naming what failed, never a silent default (eqoxide#328).
    pub fn optional_json<T: DeserializeOwned>(&self) -> Result<Option<T>, OpError> {
        match &self.body {
            None | Some(Value::Null) => Ok(None),
            Some(v) => serde_path_to_error::deserialize(v)
                .map(Some)
                .map_err(|e| OpError::invalid(format!("malformed JSON body: {e}"))),
        }
    }

    /// The query parameters as `T`. Values are rendered to text first and parsed back, so a
    /// numeric parameter is accepted whether the caller sent `5` or `"5"`.
    pub fn query<T: DeserializeOwned>(&self) -> Result<T, String> {
        let mut ser = form_urlencoded::Serializer::new(String::new());
        for (k, v) in &self.query {
            match v {
                Value::Null => continue,
                Value::String(s) => ser.append_pair(k, s),
                other => ser.append_pair(k, &other.to_string()),
            };
        }
        serde_urlencoded::from_str(&ser.finish()).map_err(|e| e.to_string())
    }

    /// The path segment an [`Ops::add_prefix`] op was reached through (`chat` for
    /// `GET /v1/events/chat`), or `None` when the op was registered under its exact name.
    pub fn tail(&self) -> Option<&str> {
        self.tail.as_deref()
    }

    /// The raw value of one query parameter, as text.
    pub fn query_text(&self, key: &str) -> Option<String> {
        match self.query.get(key)? {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }
    }
}

pub(crate) type ReplyFuture = Pin<Box<dyn Future<Output = OpResult> + Send>>;
type Handler = Arc<dyn Fn(OpState, Req) -> ReplyFuture + Send + Sync>;

/// The op table: `"<METHOD> <path>"` → handler.
#[derive(Clone, Default)]
pub struct Ops {
    table: HashMap<String, Handler>,
    /// `"<METHOD> <path prefix>"` → handler, for routes that carry one open path segment.
    prefixes: Vec<(String, Handler)>,
}

impl Ops {
    fn erase<F, Fut>(handler: F) -> Handler
    where
        F: Fn(OpState, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = OpResult> + Send + 'static,
    {
        Arc::new(move |s: OpState, r: Req| -> ReplyFuture { Box::pin(handler(s, r)) })
    }

    pub fn add<F, Fut>(&mut self, name: &str, handler: F)
    where
        F: Fn(OpState, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = OpResult> + Send + 'static,
    {
        let previous = self.table.insert(name.to_owned(), Self::erase(handler));
        assert!(previous.is_none(), "op {name:?} is registered twice");
    }

    /// Register one handler for every op under `prefix` (`"GET /v1/events/"`); it reads the rest of
    /// the path with [`Req::tail`]. An exact-name op always wins over a prefix.
    pub fn add_prefix<F, Fut>(&mut self, prefix: &str, handler: F)
    where
        F: Fn(OpState, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = OpResult> + Send + 'static,
    {
        assert!(prefix.ends_with('/'), "a prefix op ends at a path separator: {prefix:?}");
        assert!(!self.prefixes.iter().any(|(p, _)| p == prefix), "prefix {prefix:?} is registered twice");
        self.prefixes.push((prefix.to_owned(), Self::erase(handler)));
    }

    fn resolve(&self, name: &str) -> Option<(Handler, Option<String>)> {
        if let Some(h) = self.table.get(name) {
            return Some((h.clone(), None));
        }
        self.prefixes.iter().find_map(|(prefix, h)| {
            let tail = name.strip_prefix(prefix.as_str())?;
            (!tail.is_empty() && !tail.contains('/')).then(|| (h.clone(), Some(tail.to_owned())))
        })
    }

    pub fn contains(&self, name: &str) -> bool {
        self.resolve(name).is_some()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.table.keys().map(String::as_str)
    }

    pub async fn run(&self, name: &str, state: OpState, mut req: Req) -> Option<OpResult> {
        let (handler, tail) = self.resolve(name)?;
        req.tail = tail;
        Some(handler(state, req).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type Counts = std::collections::BTreeMap<String, u32>;

    #[test]
    fn a_body_that_does_not_fit_is_an_error_naming_the_field() {
        let err = Req::with_body(json!({"n": "x"})).json::<Counts>().unwrap_err();
        assert!(err.contains('n'), "{err}");
        assert!(Req::default().json::<Counts>().is_err());
        assert_eq!(Req::with_body(json!({"n": 3})).json::<Counts>().unwrap()["n"], 3);
    }

    #[test]
    fn an_optional_body_distinguishes_absent_from_malformed() {
        assert_eq!(Req::default().optional_json::<Counts>().unwrap(), None);
        let refusal = Req::with_body(json!({"n": 99999999999u64})).optional_json::<Counts>().unwrap_err();
        assert!(matches!(refusal, OpError::Invalid(_)));
    }

    #[test]
    fn query_accepts_a_number_or_its_text() {
        type Q = std::collections::BTreeMap<String, u64>;
        assert_eq!(Req::with_query(json!({"since": 5})).query::<Q>().unwrap()["since"], 5);
        assert_eq!(Req::with_query(json!({"since": "5"})).query::<Q>().unwrap()["since"], 5);
        assert!(Req::with_query(json!({"since": "x"})).query::<Q>().is_err());
    }

    #[tokio::test]
    async fn a_registered_op_runs_and_an_unknown_one_does_not() {
        let mut ops = Ops::default();
        ops.add("GET /v1/x", |_s, _r| async { conflict("busy") });
        let state = crate::ops::testkit::empty_state();
        let reply = ops.run("GET /v1/x", state.clone(), Req::default()).await.unwrap();
        assert_eq!(reply, Err(OpError::Conflict(json!("busy"))));
        assert!(ops.run("GET /v1/y", state, Req::default()).await.is_none());
    }

    #[tokio::test]
    async fn a_prefix_op_hands_the_handler_the_rest_of_the_path() {
        let mut ops = Ops::default();
        ops.add("GET /v1/e/all", |_s, _r| async { ok_text("exact") });
        ops.add_prefix("GET /v1/e/", |_s, r: Req| async move { ok_text(r.tail().unwrap_or("")) });
        let state = crate::ops::testkit::empty_state();
        let run = |name: &'static str| {
            let (ops, state) = (ops.clone(), state.clone());
            async move { ops.run(name, state, Req::default()).await }
        };
        assert_eq!(run("GET /v1/e/chat").await.unwrap(), ok_text("chat"));
        assert_eq!(run("GET /v1/e/all").await.unwrap(), ok_text("exact"), "an exact op wins");
        assert!(run("GET /v1/e/").await.is_none(), "an empty tail is not a route");
        assert!(run("GET /v1/e/a/b").await.is_none(), "a prefix op takes one segment");
        assert!(run("POST /v1/e/chat").await.is_none(), "the method is part of the name");
    }
}
