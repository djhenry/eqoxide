//! Correlate HTTP admission with a later, honest action outcome.

use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use crate::HttpState;

/// Flush completed actions into the same ordered ring as server events. Both the server-lifetime
/// watchdog and event polls use this, so an abandoned HTTP request still gets a terminal outcome.
pub(crate) fn publish(state: &HttpState) {
    let tracker = state.command.actions();
    tracker.expire();
    for action in tracker.take_events() {
        state.chat.push_action_event(action.id, &action.kind, &action.result, &action.reason);
    }
}

pub(crate) fn watchdog(state: HttpState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            interval.tick().await;
            publish(&state);
        }
    })
}

// A cancelled handler that never admitted anything has no action to retain. An accepted action
// remains in the tracker for the independent watchdog, even if its caller drops the HTTP future.
struct AdmissionGuard {
    context: eqoxide_command::ActionContext,
    tracker: eqoxide_command::ActionTracker,
}
impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if !self.context.accepted() { self.tracker.discard(self.context.id()); }
    }
}

pub(crate) async fn track(State(state): State<HttpState>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let group = path.strip_prefix("/v1/").and_then(|p| p.split('/').next());
    if !matches!(*request.method(), Method::POST | Method::DELETE | Method::PUT | Method::PATCH)
        || !matches!(group, Some("combat" | "interact" | "merchant" | "inventory" | "quests"
            | "group" | "guild" | "trainer" | "pet" | "chat")) {
        return next.run(request).await;
    }
    // Read the bounded request body before detaching work. A caller abandoning a partial body
    // must not leave a background task waiting for it. Once a complete command is submitted,
    // keep its result receiver alive independently of the HTTP connection.
    let (parts, body) = request.into_parts();
    use axum::extract::FromRequest;
    let bytes = match axum::body::Bytes::from_request(Request::new(body), &state).await {
        Ok(bytes) => bytes,
        Err(error) => return error.into_response(),
    };
    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    match tokio::spawn(execute(state, request, next, path)).await {
        Ok(response) => response,
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "action handler stopped; outcome unknown").into_response(),
    }
}

async fn execute(state: HttpState, request: Request, next: Next, path: String) -> Response {
    let tracker = state.command.actions();
    let kind = path.trim_start_matches("/v1/").replace('/', ".");
    let context = tracker.begin(&kind);
    let _guard = AdmissionGuard { context: context.clone(), tracker: tracker.clone() };
    let response = context.scope(next.run(request)).await;
    // Validation, no-op reads, and mailbox refusal keep their original response and allocate no
    // externally visible action. Acceptance is durable even if the net loop already finished it.
    if !context.accepted() { return response; }
    let id = context.id();
    let status = response.status();
    let (parts, body) = response.into_parts();
    let bytes = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => {
            tracker.finish(id, "unconfirmed", "response_body_unavailable");
            return (StatusCode::ACCEPTED, Json(serde_json::json!({
                "request_id": id, "status": "unconfirmed", "message": "response body unavailable"
            }))).into_response();
        }
    };
    let awaited = matches!(path.as_str(), "/v1/combat/cast" | "/v1/merchant/open"
        | "/v1/merchant/buy" | "/v1/interact/give");
    if awaited {
        if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(object) = value.as_object_mut() {
                let outcome = object.get("status").and_then(|v| v.as_str()).unwrap_or("unconfirmed");
                let result = match outcome {
                    "completed" | "open" | "bought" | "given" if status == StatusCode::OK => "confirmed",
                    "refused" | "fizzled" | "interrupted" => "refused",
                    _ => "unconfirmed",
                };
                let reason = object.get("reason").or_else(|| object.get("message"))
                    .and_then(|v| v.as_str()).unwrap_or(outcome).to_owned();
                tracker.finish(id, result, &reason);
                object.insert("request_id".into(), id.into());
                publish(&state);
                return (status, Json(value)).into_response();
            }
        }
    }
    if status.is_success() {
        // Queued text often describes the intended end state ("auto-attack ON"). Do not present
        // that optimistic text as an achieved result, even if the network loop ran meanwhile.
        (StatusCode::ACCEPTED, Json(serde_json::json!({
            "request_id": id, "status": "accepted", "message": "queued; outcome not yet confirmed"
        }))).into_response()
    } else {
        tracker.finish(id, "refused", &String::from_utf8_lossy(&bytes));
        publish(&state);
        Response::from_parts(parts, axum::body::Body::from(bytes))
    }
}


#[cfg(test)]
mod tests {
    use crate::{testkit::empty_state, v1_router};
    use axum::{body::Body, http::{Request, StatusCode}};
    use tower::ServiceExt;

    #[tokio::test]
    async fn queued_command_is_accepted_and_busy_preserves_first() {
        let state = empty_state();
        let app = v1_router(&state).with_state(state.clone());
        let request = || Request::post("/v1/combat/attack").header("content-type", "application/json")
            .body(Body::from(r#"{"on":true}"#)).unwrap();
        let response = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(value["request_id"].as_u64().is_some());
        assert_eq!(value["status"], "accepted");
        let response = app.oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    async fn json(response: axum::response::Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn post(path: &str, body: &str) -> Request<Body> {
        Request::post(path).header("content-type", "application/json")
            .body(Body::from(body.to_owned())).unwrap()
    }

    async fn events(state: &crate::HttpState) -> serde_json::Value {
        json(v1_router(state).with_state(state.clone()).oneshot(
            Request::get("/v1/events/action?directed=1").body(Body::empty()).unwrap()
        ).await.unwrap()).await
    }

    #[tokio::test]
    async fn invalid_command_keeps_error_and_emits_no_action() {
        let state = empty_state();
        let response = v1_router(&state).with_state(state.clone())
            .oneshot(post("/v1/combat/cast", r#"{"gem":255}"#)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(events(&state).await["count"], 0);
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_expires_undrained_command_without_cancelling_it() {
        let state = empty_state();
        let watchdog = super::watchdog(state.clone());
        let response = v1_router(&state).with_state(state.clone())
            .oneshot(post("/v1/combat/attack", "{}")).await.unwrap();
        let accepted = json(response).await;
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        tokio::task::yield_now().await;
        // Read the raw published feed: the watchdog must work without an event poll to drive it.
        let published = state.chat.chat_events.lock().unwrap().clone();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].request_id, accepted["request_id"].as_u64());
        assert_eq!(published[0].result.as_deref(), Some("unconfirmed"));
        {
            let _scope = state.command.actions().drain_scope();
            assert_eq!(state.command.take_attack(), Some(true));
        }
        assert_eq!(events(&state).await["count"], 1, "late drain cannot contradict timeout");
        watchdog.abort();
    }

    #[tokio::test]
    async fn drained_command_event_has_same_id_and_never_claims_success() {
        let state = empty_state();
        let response = v1_router(&state).with_state(state.clone())
            .oneshot(post("/v1/interact/sit", "{}")).await.unwrap();
        let accepted = json(response).await;
        {
            let _scope = state.command.actions().drain_scope();
            assert_eq!(state.command.take_sit(), Some(true));
        }
        let feed = events(&state).await;
        assert_eq!(feed["events"][0]["request_id"], accepted["request_id"]);
        assert_eq!(feed["events"][0]["result"], "unconfirmed");
        assert_eq!(feed["events"][0]["kind"], "interact.sit");
    }

    #[tokio::test(start_paused = true)]
    async fn cast_results_keep_receipt_and_correlate_semantic_outcome() {
        use eqoxide_command::{CastEnd, CommandResult};
        for outcome in ["completed", "fizzled", "interrupted", "refused", "unconfirmed", "cancelled_http"] {
            let state = empty_state();
            crate::testkit::set_gs(&state, |gs| gs.mem_spells[0] = 202);
            let app = v1_router(&state).with_state(state.clone());
            let mut task = tokio::spawn(async move {
                app.oneshot(post("/v1/combat/cast", r#"{"gem":0}"#)).await.unwrap()
            });
            let (_request, sender) = tokio::select! {
                value = async {
                    loop {
                        if let Some(value) = state.command.take_cast_await() { break value; }
                        tokio::task::yield_now().await;
                    }
                } => value,
                early = &mut task => panic!("cast returned before admission: {:?}", early.unwrap().status()),
            };
            if outcome == "cancelled_http" {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
                sender.send(CommandResult::Refused("no mana after caller left".into()))
                    .expect("caller cancellation must not discard the result channel");
                for _ in 0..100 {
                    if !state.chat.chat_events.lock().unwrap().is_empty() { break; }
                    tokio::task::yield_now().await;
                }
                let published = state.chat.chat_events.lock().unwrap();
                assert_eq!(published.len(), 1);
                assert_eq!(published[0].result.as_deref(), Some("refused"));
                continue;
            }
            let result = match outcome {
                "unconfirmed" => CommandResult::Unconfirmed,
                "refused" => CommandResult::Refused("no mana".into()),
                _ => CommandResult::Resolved(CastEnd { outcome: outcome.into(), spell_id: 202,
                    spell_name: "Minor Healing".into(), text: outcome.into() }),
            };
            sender.send(result).unwrap();
            let response = task.await.unwrap();
            let expected_status = match outcome {
                "refused" => StatusCode::CONFLICT,
                "unconfirmed" => StatusCode::ACCEPTED,
                _ => StatusCode::OK,
            };
            assert_eq!(response.status(), expected_status);
            let receipt = json(response).await;
            assert_eq!(receipt["status"], outcome);
            assert_eq!(receipt["landed"], outcome == "completed");
            let feed = events(&state).await;
            assert_eq!(feed["count"], 1);
            assert_eq!(feed["events"][0]["request_id"], receipt["request_id"]);
            assert_eq!(feed["events"][0]["result"], match outcome {
                "completed" => "confirmed", "unconfirmed" => "unconfirmed", _ => "refused"
            });
        }
    }


    #[tokio::test]
    async fn merchant_and_give_receipts_are_correlated_through_public_router() {
        use eqoxide_command::{BuyOk, OpenOk, GiveOk, CommandResult};
        for (path, body, expected) in [
            ("/v1/merchant/open", r#"{"merchant":"Beek"}"#, "open"),
            ("/v1/merchant/buy", r#"{"merchant":"Beek","slot":3}"#, "bought"),
            ("/v1/interact/give", r#"{"npc":"Beek","from":23}"#, "given"),
        ] {
            let state = empty_state();
            state.world.entity_ids_mut().insert_for_test("Innkeeper_Beek000".into(), 11);
            state.inventory_slots.inventory.lock().unwrap().push(eqoxide_core::game_state::InvItem {
                slot: 23, name: "Bone Chips".into(), item_id: 13073, ..Default::default()
            });
            let app = v1_router(&state).with_state(state.clone());
            let mut task = tokio::spawn(async move { app.oneshot(post(path, body)).await.unwrap() });
            tokio::select! {
                _ = async {
                    loop {
                        let done = match expected {
                            "open" => state.command.take_open_await().map(|(id, sender)| {
                                assert_eq!(id, 11);
                                sender.send(CommandResult::Resolved(OpenOk { merchant_id: id })).unwrap();
                            }),
                            "bought" => state.command.take_buy_await().map(|(id, slot, sender)| {
                                assert_eq!((id, slot), (11, 3));
                                sender.send(CommandResult::Resolved(BuyOk { item_name: "Bread".into(),
                                    price: 5, coin_after: [0, 0, 0, 95] })).unwrap();
                            }),
                            _ => state.command.take_give_await().map(|(id, slot, sender)| {
                                assert_eq!((id, slot), (11, 23));
                                sender.send(CommandResult::Resolved(GiveOk { npc_id: id,
                                    item_name: "Bone Chips".into() })).unwrap();
                            }),
                        };
                        if done.is_some() { break; }
                        tokio::task::yield_now().await;
                    }
                } => {},
                early = &mut task => panic!("{path} returned before admission: {:?}", early.unwrap().status()),
            }
            let response = task.await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let receipt = json(response).await;
            assert_eq!(receipt["status"], expected);
            let feed = events(&state).await;
            assert_eq!(feed["events"][0]["request_id"], receipt["request_id"]);
            assert_eq!(feed["events"][0]["result"], "confirmed");
        }
    }

}
