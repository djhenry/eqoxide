//! `/v1/chat/*` — send messages on the inter-agent channels (tell/ooc/shout/group/guild).
//! The incoming side (reading what others said) is the read-only `/v1/events/*` feed.

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/tell", post(post_tell))
        .route("/ooc", post(post_ooc))
        .route("/shout", post(post_shout))
        .route("/group", post(post_group))
        .route("/guild", post(post_guild))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TellBody { to: String, text: String }

/// POST /v1/chat/tell {"to","text"} — send a directed whisper to one character (EQ /tell, chan 7).
/// The recipient's client receives it as a `directed` event on GET /v1/events/chat.
async fn post_tell(State(s): State<HttpState>, Json(b): Json<TellBody>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if b.to.trim().is_empty() || b.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "tell requires non-empty 'to' and 'text'".into());
    }
    s.command.request_chat_send(ChatSend { chan: 7, to: b.to.clone(), text: b.text });
    (StatusCode::OK, format!("tell queued to {}", b.to))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TextBody { text: String }

/// POST /v1/chat/ooc {"text"} — zone-wide out-of-character broadcast (chan 5).
async fn post_ooc(State(s): State<HttpState>, Json(b): Json<TextBody>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if b.text.trim().is_empty() { return (StatusCode::BAD_REQUEST, "ooc requires 'text'".into()); }
    s.command.request_chat_send(ChatSend { chan: 5, to: String::new(), text: b.text });
    (StatusCode::OK, "ooc queued".into())
}

/// POST /v1/chat/shout {"text"} — zone-wide shout (chan 3).
async fn post_shout(State(s): State<HttpState>, Json(b): Json<TextBody>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if b.text.trim().is_empty() { return (StatusCode::BAD_REQUEST, "shout requires 'text'".into()); }
    s.command.request_chat_send(ChatSend { chan: 3, to: String::new(), text: b.text });
    (StatusCode::OK, "shout queued".into())
}

/// POST /v1/chat/group {"text"} — group-channel message (chan 2; only seen by your group).
async fn post_group(State(s): State<HttpState>, Json(b): Json<TextBody>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if b.text.trim().is_empty() { return (StatusCode::BAD_REQUEST, "group requires 'text'".into()); }
    s.command.request_chat_send(ChatSend { chan: 2, to: String::new(), text: b.text });
    (StatusCode::OK, "group queued".into())
}

/// POST /v1/chat/guild {"text"} — guild-channel broadcast (EQEmu chan 0; reaches every online guild
/// member regardless of zone). The shared channel for a guild of agents. Inbound guild messages
/// arrive as `kind:"guild"` events on GET /v1/events/chat and in /v1/observe/messages. Requires guild
/// membership to have recipients (#295). (#294)
async fn post_guild(State(s): State<HttpState>, Json(b): Json<TextBody>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if b.text.trim().is_empty() { return (StatusCode::BAD_REQUEST, "guild requires 'text'".into()); }
    s.command.request_chat_send(ChatSend { chan: 0, to: String::new(), text: b.text });
    (StatusCode::OK, "guild queued".into())
}
