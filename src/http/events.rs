//! `/v1/events/*` — the async event feed an agent polls for "what just happened as soon as it
//! happened": chat (tells/ooc/…), combat (slain/attacked/…), navigate (zone/…), system. Get the
//! whole stream at `/v1/events/all` or filter to one bucket at `/v1/events/<category>`.
//!
//! Outgoing chat *actions* (tell/ooc/shout/group) live under `/v1/chat` — this group is read-only.

use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use std::time::{Duration, Instant};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/all", get(get_all))
        .route("/:category", get(get_by_category))
}

#[derive(serde::Deserialize, Default)]
struct EventsQuery {
    /// Return only events with id greater than this cursor (default 0 = all). Ids are 1-based.
    since:    Option<u64>,
    /// Long-poll: block up to this many seconds (capped at 30) for a matching event before returning.
    wait:     Option<u64>,
    /// 1 = only events addressed specifically to you (tells to your name, GM messages, your own
    /// zone changes / death).
    directed: Option<u8>,
}

/// GET /v1/events/all — every async event, newest semantics via the `since` cursor.
async fn get_all(State(s): State<HttpState>, Query(q): Query<EventsQuery>) -> Json<serde_json::Value> {
    fetch(s, q, None).await
}

/// GET /v1/events/<category> — only events in one bucket: `chat`, `combat`, `navigate`, or `system`.
/// (Any category string works; unknown categories simply return nothing.)
async fn get_by_category(
    State(s): State<HttpState>,
    Path(category): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Json<serde_json::Value> {
    fetch(s, q, Some(category)).await
}

/// Shared cursor read: filter by `id > since`, optional `directed`, and optional `category`;
/// long-poll up to `wait` seconds for a match. Each event: `{id, category, kind, directed, from, text}`.
async fn fetch(s: HttpState, q: EventsQuery, category: Option<String>) -> Json<serde_json::Value> {
    let since         = q.since.unwrap_or(0);
    let directed_only = q.directed.unwrap_or(0) != 0;
    let wait          = q.wait.unwrap_or(0).min(30);
    let deadline      = Instant::now() + Duration::from_secs(wait);
    loop {
        let (events, last_id) = {
            let all = s.chat_events.lock().unwrap();
            let last_id = all.last().map(|e| e.id).unwrap_or(since).max(since);
            let evs: Vec<Event> = all.iter()
                .filter(|e| e.id > since
                    && (!directed_only || e.directed)
                    && category.as_deref().map_or(true, |c| e.category == c))
                .cloned().collect();
            (evs, last_id)
        };
        if !events.is_empty() || Instant::now() >= deadline {
            return Json(serde_json::json!({
                "count": events.len(),
                "last_id": last_id,
                "category": category,
                "events": events,
            }));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
