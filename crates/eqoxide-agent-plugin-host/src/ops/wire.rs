//! The transport-free request and result types the agent ops are written against.
//!
//! An op is an async function `(OpState, args) -> OpResult` that [`crate::ops::dispatch`] picks by
//! matching the `Op` of a `Call`. An `OpError` maps one to one onto a `CallStatus`, and nothing
//! here listens on a socket or speaks HTTP.

use serde_json::Value;
use eqoxide_agent_protocol::call::CallStatus;
use std::future::Future;
use std::pin::Pin;

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

pub(crate) type ReplyFuture = Pin<Box<dyn Future<Output = OpResult> + Send>>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_error_never_settles_as_ok() {
        let body = || json!("x");
        assert_eq!(settle(Ok(Done::Ok(body()))).0, CallStatus::Ok);
        assert_eq!(settle(Ok(Done::Accepted(body()))).0, CallStatus::Accepted);
        assert_eq!(settle(Err(OpError::Invalid(body()))).0, CallStatus::Invalid);
        assert_eq!(settle(Err(OpError::NotFound(body()))).0, CallStatus::NotFound);
        assert_eq!(settle(Err(OpError::Conflict(body()))).0, CallStatus::Conflict);
        assert_eq!(settle(Err(OpError::Unavailable(body()))).0, CallStatus::Unavailable);
        assert_eq!(settle(Err(OpError::Failed(body()))).0, CallStatus::Failed);
    }
}
