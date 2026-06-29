//! `/v1/chat/*` — the inter-agent chat channel: read the event feed, send tells/ooc/shout/group.

use axum::{extract::{Query, State}, http::StatusCode, routing::{get, post}, Json, Router};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/events", get(get_events))
        .route("/tell", post(post_tell))
        .route("/ooc", post(post_ooc))
        .route("/shout", post(post_shout))
        .route("/group", post(post_group))
}

#[derive(serde::Deserialize, Default)]
struct EventsQuery {
    /// Return only events with id greater than this cursor (default 0 = all).
    since:    Option<u64>,
    /// Long-poll: block up to this many seconds (capped at 30) for a new event before returning.
    wait:     Option<u64>,
    /// 1 = only messages addressed specifically to you (a /tell to your name, or a GM message).
    directed: Option<u8>,
}

/// GET /v1/chat/events — the inter-agent chat feed (tells/ooc/shout/group/gmsay/zone) as structured
/// events.
///
/// This is how an agent becomes aware of a whisper meant for it, or that it just changed zones. Pass
/// `?since=<last_id>` to get only new events; use the response's `last_id` as your next cursor.
/// `?wait=<secs>` long-polls — the request blocks (up to ~30s) until a new event arrives, so an
/// agent can "listen" without busy-polling (run it in a loop). `?directed=1` returns only messages
/// addressed specifically to you (including zone changes). Each event: `{id, from, channel,
/// directed, text}`.
async fn get_events(
    State(s): State<HttpState>,
    Query(q): Query<EventsQuery>,
) -> Json<serde_json::Value> {
    let since         = q.since.unwrap_or(0);
    let directed_only = q.directed.unwrap_or(0) != 0;
    let wait          = q.wait.unwrap_or(0).min(30);
    let deadline      = std::time::Instant::now() + std::time::Duration::from_secs(wait);
    loop {
        let (events, last_id) = {
            let all = s.chat_events.lock().unwrap();
            let last_id = all.last().map(|e| e.id).unwrap_or(since).max(since);
            let evs: Vec<ChatEvent> = all.iter()
                .filter(|e| e.id > since && (!directed_only || e.directed))
                .cloned().collect();
            (evs, last_id)
        };
        if !events.is_empty() || std::time::Instant::now() >= deadline {
            return Json(serde_json::json!({
                "count": events.len(), "last_id": last_id, "events": events,
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

#[derive(serde::Deserialize)]
struct TellBody { to: String, text: String }

/// POST /v1/chat/tell {"to","text"} — send a directed whisper to one character (EQ /tell, chan 7).
/// The recipient's client receives it as a `directed` event on GET /v1/chat/events.
async fn post_tell(State(s): State<HttpState>, Json(b): Json<TellBody>) -> (StatusCode, String) {
    if b.to.trim().is_empty() || b.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "tell requires non-empty 'to' and 'text'".into());
    }
    s.chat_send.lock().unwrap().push(ChatSend { chan: 7, to: b.to.clone(), text: b.text });
    (StatusCode::OK, format!("tell queued to {}", b.to))
}

#[derive(serde::Deserialize)]
struct TextBody { text: String }

/// POST /v1/chat/ooc {"text"} — zone-wide out-of-character broadcast (chan 5).
async fn post_ooc(State(s): State<HttpState>, Json(b): Json<TextBody>) -> (StatusCode, String) {
    if b.text.trim().is_empty() { return (StatusCode::BAD_REQUEST, "ooc requires 'text'".into()); }
    s.chat_send.lock().unwrap().push(ChatSend { chan: 5, to: String::new(), text: b.text });
    (StatusCode::OK, "ooc queued".into())
}

/// POST /v1/chat/shout {"text"} — zone-wide shout (chan 3).
async fn post_shout(State(s): State<HttpState>, Json(b): Json<TextBody>) -> (StatusCode, String) {
    if b.text.trim().is_empty() { return (StatusCode::BAD_REQUEST, "shout requires 'text'".into()); }
    s.chat_send.lock().unwrap().push(ChatSend { chan: 3, to: String::new(), text: b.text });
    (StatusCode::OK, "shout queued".into())
}

/// POST /v1/chat/group {"text"} — group-channel message (chan 2; only seen by your group).
async fn post_group(State(s): State<HttpState>, Json(b): Json<TextBody>) -> (StatusCode, String) {
    if b.text.trim().is_empty() { return (StatusCode::BAD_REQUEST, "group requires 'text'".into()); }
    s.chat_send.lock().unwrap().push(ChatSend { chan: 2, to: String::new(), text: b.text });
    (StatusCode::OK, "group queued".into())
}
