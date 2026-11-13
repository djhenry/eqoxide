//! The transport-free request/reply types the agent ops are written against.
//!
//! An op is an async function `(HttpState, Req) -> impl IntoReply` registered under its name
//! (`"POST /v1/trainer/open"`). The agent socket's `Call` carries the name and its `args`
//! (`query`, `body`); the reply status becomes a `CallStatus`. Nothing here listens on a socket or
//! speaks HTTP: the numeric status codes are kept only because every refusal in the handlers is
//! already expressed in them.

use crate::HttpState;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// Transitional: the same type the axum-served modules use, so both kinds of module share
// `Refusal` and `require_live_session`. Replaced by an own type when axum is removed.
pub use axum::http::StatusCode;

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
    pub fn optional_json<T: DeserializeOwned>(&self) -> Result<Option<T>, Reply> {
        match &self.body {
            None | Some(Value::Null) => Ok(None),
            Some(v) => serde_path_to_error::deserialize(v)
                .map(Some)
                .map_err(|e| Reply::text(StatusCode::BAD_REQUEST, format!("malformed JSON body: {e}"))),
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

/// An op's answer. `body` is JSON; plain-text refusals are carried as a JSON string.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub body: Value,
}

impl Reply {
    pub fn json(status: StatusCode, body: Value) -> Self {
        Self { status, body }
    }
    pub fn text(status: StatusCode, body: impl Into<String>) -> Self {
        Self { status, body: Value::String(body.into()) }
    }
    pub fn ok(body: Value) -> Self {
        Self::json(StatusCode::OK, body)
    }
}

pub trait IntoReply {
    fn into_reply(self) -> Reply;
}

/// Transitional: lets a module still served by axum return a `Reply` as its response.
impl From<Reply> for axum::response::Response {
    fn from(r: Reply) -> Self {
        use axum::response::IntoResponse;
        match r.body {
            Value::String(text) => (r.status, text).into_response(),
            other => (r.status, axum::Json(other)).into_response(),
        }
    }
}

impl IntoReply for Reply {
    fn into_reply(self) -> Reply {
        self
    }
}
impl IntoReply for (StatusCode, String) {
    fn into_reply(self) -> Reply {
        Reply::text(self.0, self.1)
    }
}
impl IntoReply for (StatusCode, &'static str) {
    fn into_reply(self) -> Reply {
        Reply::text(self.0, self.1)
    }
}
impl IntoReply for (StatusCode, Value) {
    fn into_reply(self) -> Reply {
        Reply::json(self.0, self.1)
    }
}
/// A bare JSON value is a `200`.
impl IntoReply for Value {
    fn into_reply(self) -> Reply {
        Reply::ok(self)
    }
}
impl<T: IntoReply, E: IntoReply> IntoReply for Result<T, E> {
    fn into_reply(self) -> Reply {
        match self {
            Ok(v) => v.into_reply(),
            Err(e) => e.into_reply(),
        }
    }
}

pub(crate) type ReplyFuture = Pin<Box<dyn Future<Output = Reply> + Send>>;
type Handler = Arc<dyn Fn(HttpState, Req) -> ReplyFuture + Send + Sync>;

/// The op table: `"<METHOD> <path>"` → handler.
#[derive(Clone, Default)]
pub struct Ops {
    table: HashMap<String, Handler>,
    /// `"<METHOD> <path prefix>"` → handler, for routes that carry one open path segment.
    prefixes: Vec<(String, Handler)>,
}

impl Ops {
    fn erase<F, Fut, R>(handler: F) -> Handler
    where
        F: Fn(HttpState, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoReply,
    {
        Arc::new(move |s: HttpState, r: Req| -> ReplyFuture {
            let fut = handler(s, r);
            Box::pin(async move { fut.await.into_reply() })
        })
    }

    pub fn add<F, Fut, R>(&mut self, name: &str, handler: F)
    where
        F: Fn(HttpState, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoReply,
    {
        let previous = self.table.insert(name.to_owned(), Self::erase(handler));
        assert!(previous.is_none(), "op {name:?} is registered twice");
    }

    /// Register one handler for every op under `prefix` (`"GET /v1/events/"`); it reads the rest of
    /// the path with [`Req::tail`]. An exact-name op always wins over a prefix.
    pub fn add_prefix<F, Fut, R>(&mut self, prefix: &str, handler: F)
    where
        F: Fn(HttpState, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoReply,
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

    pub async fn run(&self, name: &str, state: HttpState, mut req: Req) -> Option<Reply> {
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
        assert_eq!(refusal.status, StatusCode::BAD_REQUEST);
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
        ops.add("GET /v1/x", |_s, _r| async { (StatusCode::CONFLICT, "busy".to_string()) });
        let state = crate::testkit::empty_state();
        let reply = ops.run("GET /v1/x", state.clone(), Req::default()).await.unwrap();
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(reply.body, json!("busy"));
        assert!(ops.run("GET /v1/y", state, Req::default()).await.is_none());
    }

    #[tokio::test]
    async fn a_prefix_op_hands_the_handler_the_rest_of_the_path() {
        let mut ops = Ops::default();
        ops.add("GET /v1/e/all", |_s, _r| async { (StatusCode::OK, "exact".to_string()) });
        ops.add_prefix("GET /v1/e/", |_s, r: Req| async move { (StatusCode::OK, r.tail().unwrap_or("").to_string()) });
        let state = crate::testkit::empty_state();
        let run = |name: &'static str| {
            let (ops, state) = (ops.clone(), state.clone());
            async move { ops.run(name, state, Req::default()).await }
        };
        assert_eq!(run("GET /v1/e/chat").await.unwrap().body, json!("chat"));
        assert_eq!(run("GET /v1/e/all").await.unwrap().body, json!("exact"), "an exact op wins");
        assert!(run("GET /v1/e/").await.is_none(), "an empty tail is not a route");
        assert!(run("GET /v1/e/a/b").await.is_none(), "a prefix op takes one segment");
        assert!(run("POST /v1/e/chat").await.is_none(), "the method is part of the name");
    }
}
