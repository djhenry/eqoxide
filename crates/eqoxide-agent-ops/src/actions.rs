//! Correlate op admission with a later, honest action outcome.

use crate::OpState;

/// Flush completed actions into the same ordered ring as server events. Both the server-lifetime
/// watchdog and event polls use this, so an abandoned agent op call still gets a terminal outcome.
pub(crate) fn publish(state: &OpState) {
    let tracker = state.command.actions();
    tracker.expire();
    for action in tracker.take_events() {
        state.chat.push_action_event(action.id, &action.kind, &action.result, &action.reason);
    }
}

pub(crate) fn watchdog(state: OpState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            interval.tick().await;
            publish(&state);
        }
    })
}

// A cancelled handler that never admitted anything has no action to retain. An accepted action
// remains in the tracker for the independent watchdog, even if its caller drops the op future.
struct AdmissionGuard {
    context: eqoxide_command::ActionContext,
    tracker: eqoxide_command::ActionTracker,
}
impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if !self.context.accepted() { self.tracker.discard(self.context.id()); }
    }
}

/// Correlates admission with a later, honest outcome. A mutating op in one
/// of the tracked groups is admitted into the action tracker; its outcome is published to the
/// event feed whether or not the caller is still waiting.
pub(crate) async fn track_op(
    state: OpState,
    name: &str,
    run: crate::wire::ReplyFuture,
) -> crate::wire::Reply {
    use crate::wire::{Reply, StatusCode as Status};
    let Some((method, path)) = name.split_once(' ') else { return run.await };
    let group = path.strip_prefix("/v1/").and_then(|p| p.split('/').next());
    if method == "GET"
        || !matches!(group, Some("combat" | "interact" | "merchant" | "inventory" | "quests"
            | "group" | "guild" | "trainer" | "pet" | "chat"))
    {
        return run.await;
    }
    let path = path.to_owned();
    match tokio::spawn(execute_op(state, path, run)).await {
        Ok(reply) => reply,
        Err(_) => Reply::text(Status::SERVICE_UNAVAILABLE, "action handler stopped; outcome unknown"),
    }
}

fn body_text(body: &serde_json::Value) -> String {
    match body {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

async fn execute_op(state: OpState, path: String, run: crate::wire::ReplyFuture) -> crate::wire::Reply {
    use crate::wire::{Reply, StatusCode as Status};
    let tracker = state.command.actions();
    let kind = path.trim_start_matches("/v1/").replace('/', ".");
    let context = tracker.begin(&kind);
    let _guard = AdmissionGuard { context: context.clone(), tracker: tracker.clone() };
    let reply = context.scope(run).await;
    // Validation, no-op reads, and mailbox refusal keep their original reply and allocate no
    // externally visible action. Acceptance is durable even if the net loop already finished it.
    if !context.accepted() { return reply; }
    let id = context.id();
    let status = reply.status;
    let awaited = matches!(path.as_str(), "/v1/combat/cast" | "/v1/merchant/open"
        | "/v1/merchant/buy" | "/v1/interact/give" | "/v1/combat/target/name");
    if awaited {
        if let serde_json::Value::Object(mut object) = reply.body.clone() {
            let outcome = object.get("status").and_then(|v| v.as_str()).unwrap_or("unconfirmed");
            let result = match outcome {
                "completed" | "open" | "bought" | "given" | "targeting" if status == Status::OK => "confirmed",
                "refused" | "fizzled" | "interrupted" => "refused",
                _ => "unconfirmed",
            };
            let reason = object.get("reason").or_else(|| object.get("message"))
                .and_then(|v| v.as_str()).unwrap_or(outcome).to_owned();
            tracker.finish(id, result, &reason);
            object.insert("request_id".into(), id.into());
            publish(&state);
            return Reply::json(status, serde_json::Value::Object(object));
        }
        // An unreadable receipt is the only outcome signal on this path: answer it now rather than
        // leaving the watchdog to report `outcome_timeout` for an already-decided request.
        let (result, reason) = if status.is_success() {
            ("unconfirmed", "unreadable_receipt".to_owned())
        } else {
            ("refused", body_text(&reply.body))
        };
        tracker.finish(id, result, &reason);
        publish(&state);
        return reply;
    }
    if status.is_success() {
        // Queued text often describes the intended end state ("auto-attack ON"). Do not present
        // that optimistic text as an achieved result, even if the network loop ran meanwhile.
        Reply::json(Status::ACCEPTED, serde_json::json!({
            "request_id": id, "status": "accepted", "message": "queued; outcome not yet confirmed"
        }))
    } else {
        tracker.finish(id, "refused", &body_text(&reply.body));
        publish(&state);
        reply
    }
}

#[cfg(test)]
mod tests {
    use crate::testkit::empty_state;

    use eqoxide_agent_protocol::call::{Call, CallStatus};

    /// One agent-socket call, through the same dispatcher the running client uses.
    async fn rpc(state: &crate::OpState, op: &str, body: &str) -> (CallStatus, serde_json::Value) {
        let call = Call {
            id: 1,
            op: op.into(),
            args: serde_json::json!({ "body": serde_json::from_str::<serde_json::Value>(body).unwrap() }),
        };
        crate::dispatch::ApiDispatcher::new(state).dispatch(call).await
    }

    #[tokio::test]
    async fn queued_command_is_accepted_and_busy_preserves_first() {
        let state = empty_state();
        let body = r#"{"spell_id":202,"gem":0}"#;
        let (status, value) = rpc(&state, "POST /v1/combat/memorize", body).await;
        assert_eq!(status, CallStatus::Accepted);
        assert!(value["request_id"].as_u64().is_some());
        assert_eq!(value["status"], "accepted");
        let (status, _) = rpc(&state, "POST /v1/combat/memorize", body).await;
        assert_eq!(status, CallStatus::Conflict);
    }

    async fn events(state: &crate::OpState) -> serde_json::Value {
        let call = Call {
            id: 1,
            op: "GET /v1/events/action".into(),
            args: serde_json::json!({ "query": { "directed": 1 } }),
        };
        crate::dispatch::ApiDispatcher::new(state).dispatch(call).await.1
    }

    #[tokio::test]
    async fn invalid_command_keeps_error_and_emits_no_action() {
        let state = empty_state();
        let (status, _) = rpc(&state, "POST /v1/combat/memorize", r#"{"spell_id":202,"gem":255}"#).await;
        assert_eq!(status, CallStatus::Invalid);
        assert_eq!(events(&state).await["count"], 0);
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_expires_undrained_command_without_cancelling_it() {
        let state = empty_state();
        let watchdog = super::watchdog(state.clone());
        let (_, accepted) = rpc(&state, "POST /v1/combat/memorize", r#"{"spell_id":202,"gem":0}"#).await;
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        tokio::task::yield_now().await;
        // Read the raw published feed: the watchdog must work without an event poll to drive it.
        let published = state.chat.chat_events.lock().unwrap().clone();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].request_id, accepted["request_id"].as_u64());
        assert_eq!(published[0].result.as_deref(), Some("unconfirmed"));
        {
            let _scope = state.command.actions().drain_scope();
            assert!(state.command.take_mem_spell().is_some());
        }
        assert_eq!(events(&state).await["count"], 1, "late drain cannot contradict timeout");
        watchdog.abort();
    }

    #[tokio::test]
    async fn merchant_and_give_receipts_are_correlated_through_the_dispatcher() {
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
            let dispatcher = crate::dispatch::ApiDispatcher::new(&state);
            let call = eqoxide_agent_protocol::call::Call {
                id: 1,
                op: format!("POST {path}"),
                args: serde_json::json!({ "body": serde_json::from_str::<serde_json::Value>(body).unwrap() }),
            };
            let mut task = tokio::spawn(async move { dispatcher.dispatch(call).await });
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
                early = &mut task => panic!("{path} returned before admission: {:?}", early.unwrap().0),
            }
            let (status, receipt) = task.await.unwrap();
            assert_eq!(status, eqoxide_agent_protocol::call::CallStatus::Ok);
            assert_eq!(receipt["status"], expected);
            let feed = events(&state).await;
            assert_eq!(feed["events"][0]["request_id"], receipt["request_id"]);
            assert_eq!(feed["events"][0]["result"], "confirmed");
        }
    }
}
