//! The transport-free request and result types the agent ops are written against.
//!
//! An op is an async function `(OpState, args) -> OpResult` that [`crate::ops::dispatch`] picks by
//! matching the `Op` of a `Call`. An `OpError` maps one to one onto a `CallStatus`, and nothing
//! here listens on a socket or speaks HTTP.

use serde_json::Value;
use eqoxide_agent_protocol::call::CallStatus;
use eqoxide_agent_protocol::reply::{ErrorBody, Reply};
use std::future::Future;
use std::pin::Pin;

/// The reason an op refused or could not finish. Each variant carries the reply body, which is a
/// string for a plain refusal and a typed [`ErrorBody`] where the op documents more.
#[derive(Clone, Debug, PartialEq)]
pub enum OpError {
    /// The arguments are malformed or name something out of range.
    Invalid(ErrorBody),
    /// The call names something that does not exist.
    NotFound(ErrorBody),
    /// The call is valid but the game state refuses it now.
    Conflict(ErrorBody),
    /// The session cannot serve the call.
    Unavailable(ErrorBody),
    /// The op failed for a reason the agent cannot correct.
    Failed(ErrorBody),
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

    /// The typed reply body.
    pub fn error_body(&self) -> &ErrorBody {
        match self {
            Self::Invalid(b) | Self::NotFound(b) | Self::Conflict(b) | Self::Unavailable(b) | Self::Failed(b) => b,
        }
    }

    /// The same error with `body` in place of its reply body.
    pub fn with_body(self, body: ErrorBody) -> Self {
        match self {
            Self::Invalid(_) => Self::Invalid(body),
            Self::NotFound(_) => Self::NotFound(body),
            Self::Conflict(_) => Self::Conflict(body),
            Self::Unavailable(_) => Self::Unavailable(body),
            Self::Failed(_) => Self::Failed(body),
        }
    }

    /// The reply body as JSON.
    pub fn body(&self) -> Value {
        self.error_body().to_body()
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(ErrorBody::Message(message.into()))
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(ErrorBody::Message(message.into()))
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(ErrorBody::Message(message.into()))
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(ErrorBody::Message(message.into()))
    }
}

/// An op's successful answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Done {
    /// The op finished and the body says how.
    Ok(Reply),
    /// The op queued an action that the game has not confirmed.
    Accepted(Reply),
}

impl Done {
    /// The `CallStatus` this answer reports.
    pub fn status(&self) -> CallStatus {
        match self {
            Self::Ok(_) => CallStatus::Ok,
            Self::Accepted(_) => CallStatus::Accepted,
        }
    }

    /// The typed reply body.
    pub fn reply(&self) -> &Reply {
        match self {
            Self::Ok(b) | Self::Accepted(b) => b,
        }
    }

    /// The reply body as JSON.
    pub fn body(&self) -> Value {
        self.reply().to_body()
    }
}

/// What every op returns.
pub type OpResult = Result<Done, OpError>;

/// The `CallStatus` and body that answer `result`.
pub fn settle(result: OpResult) -> (CallStatus, Value) {
    match result {
        Ok(done) => (done.status(), done.body()),
        Err(e) => (e.status(), e.body()),
    }
}

/// A success with a typed body.
pub fn ok(body: impl Into<Reply>) -> OpResult {
    Ok(Done::Ok(body.into()))
}

/// A success with a text body.
pub fn ok_text(message: impl Into<String>) -> OpResult {
    Ok(Done::Ok(Reply::Text(message.into())))
}

/// A queued action the game has not confirmed.
pub fn accepted(body: impl Into<Reply>) -> OpResult {
    Ok(Done::Accepted(body.into()))
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
        let reply = || Reply::Text("x".into());
        let error = || ErrorBody::Message("x".into());
        assert_eq!(settle(Ok(Done::Ok(reply()))), (CallStatus::Ok, json!("x")));
        assert_eq!(settle(Ok(Done::Accepted(reply()))).0, CallStatus::Accepted);
        assert_eq!(settle(Err(OpError::Invalid(error()))).0, CallStatus::Invalid);
        assert_eq!(settle(Err(OpError::NotFound(error()))).0, CallStatus::NotFound);
        assert_eq!(settle(Err(OpError::Conflict(error()))).0, CallStatus::Conflict);
        assert_eq!(settle(Err(OpError::Unavailable(error()))).0, CallStatus::Unavailable);
        assert_eq!(settle(Err(OpError::Failed(error()))).0, CallStatus::Failed);
    }
}
