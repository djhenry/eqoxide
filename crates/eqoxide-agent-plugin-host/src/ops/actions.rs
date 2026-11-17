//! Correlate op admission with a later, honest action outcome.

use crate::ops::wire::{accepted, unavailable, Done, OpError, OpResult};
use eqoxide_agent_protocol::reply::{ErrorBody, Receipt, Reply};
use crate::ops::OpState;
use eqoxide_agent_protocol::op::Op;

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


/// Correlates admission with a later, honest outcome. A state-changing op is admitted into the
/// action tracker, and its outcome is published to the event feed whether or not the caller is
/// still waiting.
pub(crate) async fn track_op(
    state: OpState,
    op: &Op,
    run: crate::ops::wire::ReplyFuture,
) -> OpResult {
    if !op.is_tracked() {
        return run.await;
    }
    let (kind, awaited) = (op.name(), op.is_awaited());
    match tokio::spawn(execute_op(state, kind, awaited, run)).await {
        Ok(result) => result,
        Err(_) => unavailable("action handler stopped; outcome unknown"),
    }
}

/// The text that the event feed reports as the reason for a refused call.
fn refusal_reason(error: &ErrorBody) -> String {
    match error {
        ErrorBody::Message(message) => message.clone(),
        ErrorBody::Refused(refused) => refused.reason.clone(),
        ErrorBody::Unavailable(_) => error.to_body().to_string(),
    }
}

async fn execute_op(state: OpState, kind: String, awaited: bool, run: crate::ops::wire::ReplyFuture) -> OpResult {
    let tracker = state.command.actions();
    let context = tracker.begin(&kind);
    let _guard = AdmissionGuard { context: context.clone(), tracker: tracker.clone() };
    let result = context.scope(run).await;
    // Validation, no-op reads, and mailbox refusal keep their original reply and allocate no
    // externally visible action. Acceptance is durable even if the net loop already finished it.
    if !context.accepted() { return result; }
    let id = context.id();
    if awaited {
        return match result {
            Ok(Done::Ok(Reply::Receipt(receipt))) | Ok(Done::Accepted(Reply::Receipt(receipt))) => {
                let outcome = if receipt.is_confirmed() { "confirmed" } else { "unconfirmed" };
                tracker.finish(id, outcome, receipt.reason());
                publish(&state);
                let receipt = receipt.with_request_id(id);
                Ok(match outcome {
                    "confirmed" => Done::Ok(receipt.into()),
                    _ => Done::Accepted(receipt.into()),
                })
            }
            Err(OpError::Conflict(ErrorBody::Refused(mut refused))) => {
                tracker.finish(id, "refused", &refused.reason);
                publish(&state);
                refused.request_id = Some(id);
                Err(OpError::Conflict(refused.into()))
            }
            // An unreadable receipt is the only outcome signal on this path: answer it now rather than
            // leaving the watchdog to report `outcome_timeout` for an already-decided request.
            Ok(done) => {
                tracker.finish(id, "unconfirmed", "unreadable_receipt");
                publish(&state);
                Ok(done)
            }
            Err(error) => {
                tracker.finish(id, "refused", &refusal_reason(error.error_body()));
                publish(&state);
                Err(error)
            }
        };
    }
    match result {
        // Queued text often describes the intended end state ("auto-attack ON"). Do not present
        // that optimistic text as an achieved result, even if the network loop ran meanwhile.
        Ok(_) => accepted(Receipt::Accepted {
            request_id: Some(id),
            message: "queued; outcome not yet confirmed".to_owned(),
        }),
        Err(error) => {
            tracker.finish(id, "refused", &refusal_reason(error.error_body()));
            publish(&state);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ops::testkit::empty_state;

    use eqoxide_agent_protocol::call::{Call, CallStatus};
    use eqoxide_agent_protocol::op::Op;

    /// One agent-socket call, through the same dispatcher the running client uses.
    async fn rpc(state: &crate::ops::OpState, op: &str, body: &str) -> (CallStatus, serde_json::Value) {
        let op = Op::from_wire(op, Some(serde_json::from_str(body).unwrap())).unwrap();
        let call = Call { id: 1, op };
        crate::ops::dispatch::ApiDispatcher::new(state).dispatch(call).await
    }

    #[tokio::test]
    async fn queued_command_is_accepted_and_busy_preserves_first() {
        let state = empty_state();
        let body = r#"{"spell_id":202,"gem":0}"#;
        let (status, value) = rpc(&state, "combat.memorize", body).await;
        assert_eq!(status, CallStatus::Accepted);
        assert!(value["request_id"].as_u64().is_some());
        assert_eq!(value["status"], "accepted");
        let (status, _) = rpc(&state, "combat.memorize", body).await;
        assert_eq!(status, CallStatus::Conflict);
    }

    async fn events(state: &crate::ops::OpState) -> serde_json::Value {
        let op = Op::from_wire("events.read", Some(serde_json::json!({ "category": "action", "directed": 1 }))).unwrap();
        let call = Call { id: 1, op };
        crate::ops::dispatch::ApiDispatcher::new(state).dispatch(call).await.1
    }

    #[tokio::test]
    async fn invalid_command_keeps_error_and_emits_no_action() {
        let state = empty_state();
        let (status, _) = rpc(&state, "combat.memorize", r#"{"spell_id":202,"gem":255}"#).await;
        assert_eq!(status, CallStatus::Invalid);
        assert_eq!(events(&state).await["count"], 0);
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_expires_undrained_command_without_cancelling_it() {
        let state = empty_state();
        let watchdog = super::watchdog(state.clone());
        let (_, accepted) = rpc(&state, "combat.memorize", r#"{"spell_id":202,"gem":0}"#).await;
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
        for (name, body, expected) in [
            ("merchant.open", r#"{"merchant":"Beek"}"#, "open"),
            ("merchant.buy", r#"{"merchant":"Beek","slot":3}"#, "bought"),
            ("interact.give", r#"{"npc":"Beek","from":23}"#, "given"),
        ] {
            let state = empty_state();
            state.world.entity_ids_mut().insert_for_test("Innkeeper_Beek000".into(), 11);
            state.inventory_slots.inventory.lock().unwrap().push(eqoxide_core::game_state::InvItem {
                slot: 23, name: "Bone Chips".into(), item_id: 13073, ..Default::default()
            });
            let dispatcher = crate::ops::dispatch::ApiDispatcher::new(&state);
            let call = eqoxide_agent_protocol::call::Call {
                id: 1,
                op: Op::from_wire(name, Some(serde_json::from_str(body).unwrap())).unwrap(),
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
                early = &mut task => panic!("{name} returned before admission: {:?}", early.unwrap().0),
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
