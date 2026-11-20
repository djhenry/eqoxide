//! Runs an Agent Plugin `Call` against the session by matching its [`Op`].
//!
//! The `CallStatus` of a reply comes from the op's `OpResult`, so a refusal is never reported as
//! `Ok`. A call whose `op` or `args` did not parse reads as [`Op::Unreadable`] and answers
//! `Invalid` with the reason.

use crate::ops::wire::{invalid, OpResult};
use crate::ops::OpState;
use crate::ops::{combat, events, group, guild, interact, merchant, observe, quests, social, trainer};
use eqoxide_agent_protocol::call::{Call, CallStatus};
use eqoxide_agent_protocol::op::Op;
use serde_json::Value;

#[derive(Clone)]
pub struct ApiDispatcher {
    state: OpState,
}

impl ApiDispatcher {
    pub fn new(state: &OpState) -> Self {
        Self { state: state.clone() }
    }

    /// Runs `call` to completion and returns its `CallStatus` and reply body.
    pub async fn dispatch(&self, call: Call) -> (CallStatus, Value) {
        let state = self.state.clone();
        let op = call.op;
        let tracked = op.clone();
        let run: crate::ops::wire::ReplyFuture = Box::pin(run(state.clone(), op));
        crate::ops::wire::settle(crate::ops::actions::track_op(state, &tracked, run).await)
    }
}

/// Calls the handler for `op`.
pub(crate) async fn run(s: OpState, op: Op) -> OpResult {
    match op {
        Op::Unreadable(why) => invalid(why),

        Op::GroupRoster => group::get_roster(s).await,
        Op::GroupInvite(a) => group::post_invite(s, a).await,
        Op::GroupAccept => group::post_accept(s).await,
        Op::GroupDecline => group::post_decline(s).await,
        Op::GroupLeave => group::post_leave(s).await,
        Op::GroupKick(a) => group::post_kick(s, a).await,
        Op::GroupMakeLeader(a) => group::post_makeleader(s, a).await,

        Op::GuildRoster => guild::get_roster(s).await,
        Op::GuildInvite(a) => guild::post_invite(s, a).await,
        Op::GuildAccept => guild::post_accept(s).await,
        Op::GuildLeave => guild::post_leave(s).await,
        Op::GuildRemove(a) => guild::post_remove(s, a).await,

        Op::MerchantOpen(a) => merchant::post_trade_open(s, a).await,
        Op::MerchantClose => merchant::post_trade_close(s).await,
        Op::MerchantList => merchant::get_trade_list(s).await,
        Op::MerchantBuy(a) => merchant::post_buy(s, a).await,
        Op::MerchantSell(a) => merchant::post_sell(s, a).await,

        Op::QuestsLog => quests::get_log(s).await,
        Op::QuestsCompleted => quests::get_completed(s).await,
        Op::QuestsOffers => quests::get_offers(s).await,
        Op::QuestsAccept(a) => quests::post_accept(s, a).await,
        Op::QuestsDecline => quests::post_decline(s).await,
        Op::QuestsCancel(a) => quests::post_cancel(s, a).await,

        Op::SocialFriends => social::get_friends(s).await,
        Op::SocialEditFriends(a) => social::post_friends(s, a).await,

        Op::TrainerOpen(a) => trainer::post_open(s, a).await,
        Op::TrainerList => trainer::get_list(s).await,
        Op::TrainerTrain(a) => trainer::post_train(s, a).await,
        Op::TrainerClose => trainer::post_close(s).await,

        Op::CombatMemorize(a) => combat::post_memorize(s, a).await,
        Op::CombatScribe(a) => combat::post_scribe(s, a).await,

        Op::InteractHail(a) => interact::post_hail(s, a).await,
        Op::InteractSay(a) => interact::post_say(s, a).await,
        Op::InteractLoot(a) => interact::post_loot(s, a).await,
        Op::InteractGive(a) => interact::post_give(s, a).await,
        Op::InteractClickDoor(a) => interact::post_door_click(s, a).await,
        Op::InteractRun => interact::post_run(s).await,
        Op::InteractWalk => interact::post_walk(s).await,
        Op::InteractDialogue(a) => interact::post_dialogue(s, a).await,
        Op::InteractRead(a) => interact::post_read(s, a).await,

        Op::ObserveInventory => observe::get_inventory(s).await,
        Op::ObserveMessages(a) => observe::get_messages(s, a).await,
        Op::ObserveDialogue => observe::get_dialogue(s).await,
        Op::ObserveSpells => observe::get_spells(s).await,
        Op::ObserveSkills => observe::get_skills(s).await,
        Op::ObserveDoors => observe::get_doors(s).await,
        Op::ObserveZoneEntrances => observe::get_zone_entrances(s).await,
        Op::ObserveZoneExits => observe::get_zone_exits(s).await,
        Op::ObserveItemText => observe::get_item_text(s).await,
        Op::ObserveWho => observe::get_who(s).await,
        Op::ObserveFrame => observe::get_frame(s).await,

        Op::EventsRead(a) => events::read(s, a).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_agent_protocol::op::{MerchantBuy, TrainerTrain};

    fn call(op: Op) -> Call {
        Call { id: 1, op }
    }

    #[tokio::test]
    async fn a_tracked_op_is_admitted_and_a_repeat_is_a_conflict_not_ok() {
        let state = crate::ops::testkit::empty_state();
        let d = ApiDispatcher::new(&state);
        let (first, body) = d.dispatch(call(Op::TrainerClose)).await;
        assert_eq!(first, CallStatus::Accepted, "{body}");
        assert!(body["request_id"].as_u64().is_some(), "an admitted action carries its id: {body}");
        let (second, _) = d.dispatch(call(Op::TrainerClose)).await;
        assert_eq!(second, CallStatus::Conflict, "the undrained slot must refuse, never answer Ok");
    }

    #[tokio::test]
    async fn a_read_op_answers_through_the_dispatcher() {
        let d = ApiDispatcher::new(&crate::ops::testkit::empty_state());
        let (status, body) = d.dispatch(call(Op::ObserveSkills)).await;
        assert_eq!(status, CallStatus::Ok, "{body}");
        assert!(body.is_object(), "{body}");
    }

    #[tokio::test]
    async fn an_unreadable_op_is_invalid_with_its_reason() {
        let d = ApiDispatcher::new(&crate::ops::testkit::empty_state());
        let (status, body) = d.dispatch(call(Op::Unreadable("no such op".into()))).await;
        assert_eq!(status, CallStatus::Invalid);
        assert_eq!(body, Value::String("no such op".into()));
    }

    #[tokio::test]
    async fn a_malformed_call_read_from_the_wire_never_reaches_a_handler() {
        let d = ApiDispatcher::new(&crate::ops::testkit::empty_state());
        let line = r#"{"id":2,"op":"merchant.buy","args":{"merchant":"V","slot":"x"}}"#;
        let call: Call = serde_json::from_str(line).unwrap();
        let (status, body) = d.dispatch(call).await;
        assert_eq!(status, CallStatus::Invalid, "{body}");
        assert!(body.as_str().is_some_and(|b| b.contains("slot")), "{body}");
        // typed arguments that parse still run
        let ok = Op::MerchantBuy(MerchantBuy { merchant: "V".into(), slot: 1 });
        assert_ne!(d.dispatch(call_of(ok)).await.0, CallStatus::Invalid);
        let _ = Op::TrainerTrain(TrainerTrain { skill_id: 1 });
    }

    fn call_of(op: Op) -> Call {
        Call { id: 3, op }
    }
}
