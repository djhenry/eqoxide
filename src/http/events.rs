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

// `deny_unknown_fields`: a typo'd param (e.g. `?snice=5`) must be a 400 naming the bad field, not
// silently dropped so the field falls back to its default and the caller gets a misleadingly
// "healthy" 200 (eqoxide#363 — the query-string half of the #341/#351 JSON-body fix).
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
///
/// The response also carries `first_id` — the oldest event id still retained in the (200-entry)
/// ring buffer `GameState::push_event` maintains — plus a `dropped` count. `push_event` silently
/// evicts the oldest entry once the ring is full, so a caller that only sees `last_id` can advance
/// its `since` cursor past ids it never actually received (eqoxide#350). `first_id > since + 1`
/// means at least one event between them was evicted before this poll ever saw it; `dropped` is
/// exactly how many. When nothing has been evicted (or the ring is empty), `first_id <= since + 1`
/// and `dropped == 0`.
async fn fetch(s: HttpState, q: EventsQuery, category: Option<String>) -> Json<serde_json::Value> {
    let since         = q.since.unwrap_or(0);
    let directed_only = q.directed.unwrap_or(0) != 0;
    let wait          = q.wait.unwrap_or(0).min(30);
    let deadline      = Instant::now() + Duration::from_secs(wait);
    loop {
        let (events, last_id, first_id) = {
            let all = s.chat_events.lock().unwrap();
            let last_id = all.last().map(|e| e.id).unwrap_or(since).max(since);
            // The oldest id still in the ring — independent of `since`/`category`, since eviction
            // is global. Falls back to `last_id` when the ring is empty (nothing retained, but
            // also nothing to report as dropped).
            let first_id = all.first().map(|e| e.id).unwrap_or(last_id);
            let evs: Vec<Event> = all.iter()
                .filter(|e| e.id > since
                    && (!directed_only || e.directed)
                    && category.as_deref().map_or(true, |c| e.category == c))
                .cloned().collect();
            (evs, last_id, first_id)
        };
        if !events.is_empty() || Instant::now() >= deadline {
            let dropped = first_id.saturating_sub(since + 1);
            return Json(serde_json::json!({
                "count": events.len(),
                "last_id": last_id,
                "first_id": first_id,
                "dropped": dropped,
                "category": category,
                "events": events,
            }));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use crate::http::quests::tests::empty_state;
    use crate::http::Event;

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn ev(id: u64) -> Event {
        Event {
            id, category: "chat".to_string(), kind: "ooc".to_string(),
            from: "someone".to_string(), directed: false, text: format!("event {id}"),
        }
    }

    /// eqoxide#350: `push_event` retains only the newest 200 events (FIFO eviction). A caller that
    /// polled with an old `since` cursor has no way to tell — from `last_id` alone — that events
    /// between `since` and the oldest retained id were dropped before it ever saw them. `first_id`
    /// must reveal the gap.
    #[tokio::test]
    async fn first_id_reveals_dropped_events_after_the_ring_wraps() {
        let state = empty_state();
        {
            let mut events = state.chat_events.lock().unwrap();
            // Simulate the 200-cap ring having wrapped: ids 1..=50 were evicted, 51..=250 remain.
            for id in 51..=250u64 {
                events.push(ev(id));
            }
        }
        let app = router().with_state(state);
        // Poll with a cursor from before the wrap — an agent that last saw id 10 and comes back now.
        let resp = app.oneshot(Request::get("/all?since=10").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let j = body_json(resp).await;
        assert_eq!(j["last_id"], 250);
        assert_eq!(j["first_id"], 51, "first_id must report the oldest event still retained");
        // Events 11..=50 were evicted before this poll ever saw them: since=10, first_id=51 → 40 lost.
        assert_eq!(j["dropped"], 40, "dropped must reveal the gap between since and first_id");
    }

    /// When nothing has been evicted (ring hasn't wrapped), `first_id` must not falsely report a gap.
    #[tokio::test]
    async fn first_id_reports_no_gap_when_ring_has_not_wrapped() {
        let state = empty_state();
        {
            let mut events = state.chat_events.lock().unwrap();
            for id in 1..=5u64 {
                events.push(ev(id));
            }
        }
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get("/all?since=0").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let j = body_json(resp).await;
        assert_eq!(j["first_id"], 1);
        assert_eq!(j["dropped"], 0, "nothing was evicted, so dropped must be 0");
    }

    /// An empty ring (never populated, or everything already consumed) must not falsely report a gap.
    #[tokio::test]
    async fn first_id_reports_no_gap_when_ring_is_empty() {
        let state = empty_state();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get("/all?since=42").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let j = body_json(resp).await;
        assert_eq!(j["last_id"], 42);
        assert_eq!(j["first_id"], 42);
        assert_eq!(j["dropped"], 0);
    }

    /// eqoxide#363: a typo'd query param (`?snice=5` instead of `?since=5`) must be rejected with an
    /// explicit 400 naming the bad field, NOT silently ignored so `since` falls back to its default
    /// of 0 and the caller gets the whole 200-entry ring back looking like a normal, healthy 200.
    /// Without `#[serde(deny_unknown_fields)]` on `EventsQuery` this returned 200 with `since` fixed
    /// at 0 — indistinguishable from a legitimate `?since=0` poll.
    #[tokio::test]
    async fn typoed_query_param_is_rejected_not_silently_dropped() {
        let state = empty_state();
        {
            let mut events = state.chat_events.lock().unwrap();
            for id in 1..=8u64 {
                events.push(ev(id));
            }
        }
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get("/all?snice=5").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST,
            "a typo'd/unknown query param must be an explicit failure, not a silent 200 over the whole ring");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let msg = String::from_utf8_lossy(&bytes);
        assert!(msg.contains("snice"), "the 400 body should name the offending field, got: {msg}");
    }

    /// REGRESSION GUARD — this pins behaviour that is ALREADY CORRECT today; it fixes nothing.
    ///
    /// A *present but unparseable* `since` (garbage, negative, or overflowing u64) is rejected with
    /// a 400 by `u64::from_str` inside serde_urlencoded's `Part::deserialize_u64` — it does NOT fall
    /// back to the `unwrap_or(0)` default in `fetch`, which applies only to a genuinely ABSENT param.
    /// That distinction is the whole point of #363: a default may mask an OMITTED field, never a
    /// PARSE FAILURE. Nothing in the type system stops a future refactor from taking `since` as a
    /// `String` and doing `.parse().unwrap_or(0)` — which would silently resurrect #363 in a new
    /// coat (garbage cursor → whole ring replayed → confident 200). This test exists to make that
    /// refactor fail loudly. (Verified non-vacuous by exactly that mutation; see the PR.)
    #[tokio::test]
    async fn unparseable_since_is_rejected_not_silently_defaulted_to_zero() {
        for bad in ["abc", "-1", "99999999999999999999999"] {
            let state = empty_state();
            {
                let mut events = state.chat_events.lock().unwrap();
                for id in 1..=8u64 {
                    events.push(ev(id));
                }
            }
            let app = router().with_state(state);
            let uri = format!("/all?since={bad}");
            let resp = app.oneshot(Request::get(&uri).body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST,
                "?since={bad} must be an explicit 400, not a silent fallback to since=0 that replays the whole ring");
        }
    }

    /// The happy path must not regress: a correctly-spelled `since` still parses and filters normally.
    #[tokio::test]
    async fn valid_since_param_still_works() {
        let state = empty_state();
        {
            let mut events = state.chat_events.lock().unwrap();
            for id in 1..=8u64 {
                events.push(ev(id));
            }
        }
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get("/all?since=5").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let j = body_json(resp).await;
        // since=5 should only return events with id > 5, i.e. 6, 7, 8.
        assert_eq!(j["count"], 3);
        assert_eq!(j["last_id"], 8);
    }
}
