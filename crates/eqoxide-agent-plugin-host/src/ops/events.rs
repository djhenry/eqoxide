//! `events.*` — the async event feed an agent polls for "what just happened as soon as it
//! happened": chat (tells/ooc/…), combat (slain/attacked/…), navigate (zone/…), system. Read the
//! whole stream with `events.read` or filter to one bucket with its `category` argument.
//!
//! Outgoing chat *actions* (tell/ooc/shout/group) are verbs, so this group is read-only.

use crate::ops::wire::{ok, OpResult};
use eqoxide_agent_protocol::op::EventsRead;
use std::time::{Duration, Instant};
use super::*;


// `deny_unknown_fields`: a typo'd param (e.g. `?snice=5`) must be a 400 naming the bad field, not
// silently dropped so the field falls back to its default and the caller gets a misleadingly
// "healthy" 200 (eqoxide#363 — the query-string half of the #341/#351 JSON-body fix).
/// Shared cursor read: filter by `id > since`, optional `directed`, and optional `category`;
/// long-poll up to `wait` seconds for a match. Each event: `{id, category, kind, directed, from, text}`.
///
/// `first_id` names the oldest retained event. `dropped` counts missing cursor positions in
/// `(since, last_id]` before category/directed filtering, including upstream game-source gaps.
/// Both producers share a 200-entry ring. A client must re-check state when events were lost.
pub(super) async fn read(s: OpState, q: EventsRead) -> OpResult {
    let category = q.category;
    let since         = q.since.unwrap_or(0);
    let directed_only = q.directed.unwrap_or(0) != 0;
    let wait          = q.wait.unwrap_or(0).min(30);
    let deadline      = Instant::now() + Duration::from_secs(wait);
    loop {
        crate::ops::actions::publish(&s);
        let (events, last_id, first_id, dropped) = {
            let all = s.chat.chat_events.lock().unwrap();
            let last_id = all.last().map(|e| e.id).unwrap_or(since).max(since);
            // The oldest id still in the ring — independent of `since`/`category`, since eviction
            // is global. Falls back to `last_id` when the ring is empty (nothing retained, but
            // also nothing to report as dropped).
            let first_id = all.first().map(|e| e.id).unwrap_or(last_id);
            let evs: Vec<Event> = all.iter()
                .filter(|e| e.id > since
                    && (!directed_only || e.directed)
                    && category.is_none_or(|c| e.category == c.as_str()))
                .cloned().collect();
            let retained = all.iter().filter(|e| e.id > since).count() as u64;
            let dropped = last_id.saturating_sub(since).saturating_sub(retained);
            (evs, last_id, first_id, dropped)
        };
        if !events.is_empty() || Instant::now() >= deadline {
            return ok(reply::Events {
                count: events.len(),
                last_id,
                first_id,
                dropped,
                category,
                events: events.into_iter().map(|e| reply::Event {
                    id: e.id,
                    category: e.category,
                    kind: e.kind,
                    from: e.from,
                    directed: e.directed,
                    text: e.text,
                    request_id: e.request_id,
                    result: e.result,
                    reason: e.reason,
                }).collect(),
            });
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use crate::ops::testkit::Answer;
    use eqoxide_agent_protocol::call::CallStatus;
    use crate::ops::testkit::{call_wire, empty_state, query_args};
    use crate::ops::{Event, OpState};

    /// `events.read` for `/all` or `/<category>`, with the query string read as arguments.
    async fn get(state: &OpState, uri: &str) -> Answer {
        let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
        let mut args = query_args(query).unwrap_or_else(|| serde_json::json!({}));
        if path != "/all" {
            args["category"] = path.trim_start_matches('/').into();
        }
        call_wire(state, "events.read", Some(args)).await
    }

    fn ev(id: u64) -> Event {
        Event {
            id, category: "chat".to_string(), kind: "ooc".to_string(),
            from: "someone".to_string(), directed: false, text: format!("event {id}"),
            request_id: None, result: None, reason: None,
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
            let mut events = state.chat.chat_events.lock().unwrap();
            // Simulate the 200-cap ring having wrapped: ids 1..=50 were evicted, 51..=250 remain.
            for id in 51..=250u64 {
                events.push(ev(id));
            }
        }
        // Poll with a cursor from before the wrap — an agent that last saw id 10 and comes back now.
        let resp = get(&state, "/all?since=10").await;
        assert_eq!(resp.status, CallStatus::Ok);
        let j = resp.body;
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
            let mut events = state.chat.chat_events.lock().unwrap();
            for id in 1..=5u64 {
                events.push(ev(id));
            }
        }
        let resp = get(&state, "/all?since=0").await;
        assert_eq!(resp.status, CallStatus::Ok);
        let j = resp.body;
        assert_eq!(j["first_id"], 1);
        assert_eq!(j["dropped"], 0, "nothing was evicted, so dropped must be 0");
    }

    /// An empty ring (never populated, or everything already consumed) must not falsely report a gap.
    #[tokio::test]
    async fn first_id_reports_no_gap_when_ring_is_empty() {
        let state = empty_state();
        let resp = get(&state, "/all?since=42").await;
        assert_eq!(resp.status, CallStatus::Ok);
        let j = resp.body;
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
            let mut events = state.chat.chat_events.lock().unwrap();
            for id in 1..=8u64 {
                events.push(ev(id));
            }
        }
        let resp = get(&state, "/all?snice=5").await;
        assert_eq!(resp.status, CallStatus::Invalid,
            "a typo'd/unknown query param must be an explicit failure, not a silent 200 over the whole ring");
        let msg = resp.body.as_str().unwrap_or_default().to_string();
        assert!(msg.contains("snice"), "the 400 body should name the offending field, got: {msg}");
    }

    /// REGRESSION GUARD — this pins behaviour that is ALREADY CORRECT today; it fixes nothing.
    ///
    /// A *present but unparseable* `since` (garbage, negative, or overflowing u64) is rejected with
    /// a 400 when the call's arguments are read into `EventsRead` — it does NOT fall back to a
    /// default, which applies only to a genuinely ABSENT param.
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
                let mut events = state.chat.chat_events.lock().unwrap();
                for id in 1..=8u64 {
                    events.push(ev(id));
                }
            }
                let uri = format!("/all?since={bad}");
            let resp = get(&state, &uri).await;
            assert_eq!(resp.status, CallStatus::Invalid,
                "?since={bad} must be an explicit 400, not a silent fallback to since=0 that replays the whole ring");
        }
    }

    /// The happy path must not regress: a correctly-spelled `since` still parses and filters normally.
    #[tokio::test]
    async fn valid_since_param_still_works() {
        let state = empty_state();
        {
            let mut events = state.chat.chat_events.lock().unwrap();
            for id in 1..=8u64 {
                events.push(ev(id));
            }
        }
        let resp = get(&state, "/all?since=5").await;
        assert_eq!(resp.status, CallStatus::Ok);
        let j = resp.body;
        // since=5 should only return events with id > 5, i.e. 6, 7, 8.
        assert_eq!(j["count"], 3);
        assert_eq!(j["last_id"], 8);
    }

    #[tokio::test]
    async fn interleaved_source_gaps_are_counted_before_filtering() {
        let state = empty_state();
        state.chat.push_action_event(42, "loot", "unconfirmed", "no receipt");
        state.chat.publish_game_events([ev(51), ev(52)]);
        let response = get(&state, "/action?since=0&directed=1").await;
        let value = response.body;
        assert_eq!(value["count"], 1);
        assert_eq!(value["first_id"], 1);
        assert_eq!(value["last_id"], 53);
        assert_eq!(value["dropped"], 50);
    }

    #[tokio::test]
    async fn maximum_cursor_does_not_overflow() {
        let response = get(&empty_state(), "/all?since=18446744073709551615").await;
        assert_eq!(response.body["dropped"], 0);
    }

}
