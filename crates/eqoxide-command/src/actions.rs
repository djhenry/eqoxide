use std::{collections::HashMap, future::Future, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, time::Duration};

use tokio::time::Instant;

/// Names one tracked command slot as a typed variant instead of a string.
///
/// [`CommandState::refuse_drained`] is called from `eqoxide-net` and must name the same slot the
/// `enqueue` here filled. With bare strings, a rename on one side silently matched nothing: the
/// action fell through to [`DrainScope`] and reported `unconfirmed` /
/// `processed_without_server_confirmation` instead of `refused`, with no error anywhere. A variant
/// makes that divergence a compile failure. The label text is derived from the `group.slot` idents,
/// so it cannot drift from the field path `enqueue` locks.
macro_rules! action_labels {
    ($($variant:ident => $group:ident . $slot:ident),* $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub enum Action { $($variant,)* }
        impl Action {
            /// The `domain.slot` text. Operator-facing only — correlation uses the variant.
            pub const fn label(self) -> &'static str {
                match self { $(Action::$variant => concat!(stringify!($group), ".", stringify!($slot)),)* }
            }
            #[cfg(test)]
            const ALL: &'static [Action] = &[$(Action::$variant,)*];
        }
    };
}
action_labels! {
    CombatTarget          => combat.target,
    CombatAttack          => combat.attack,
    CombatConsider        => combat.consider,
    CombatCast            => combat.cast,
    CombatCastAwait       => combat.cast_await,
    CombatMemSpell        => combat.mem_spell,
    CombatPetCmd          => combat.pet_cmd,
    MerchantBuy           => merchant.buy,
    MerchantBuyAwait      => merchant.buy_await,
    MerchantSell          => merchant.sell,
    MerchantTrade         => merchant.trade,
    MerchantOpenAwait     => merchant.open_await,
    InventoryMoveReq      => inventory.move_req,
    InteractHail          => interact.hail,
    InteractSay           => interact.say,
    InteractLoot          => interact.loot,
    InteractGive          => interact.give,
    InteractGiveAwait     => interact.give_await,
    InteractDoorClick     => interact.door_click,
    InteractSit           => interact.sit,
    InteractRunMode       => interact.run_mode,
    InteractDialogueClick => interact.dialogue_click,
    InteractReadBook      => interact.read_book,
    QuestAcceptTask       => quest.accept_task,
    QuestCancelTask       => quest.cancel_task,
    GroupInvite           => group.group_invite,
    GroupAccept           => group.group_accept,
    GroupDecline          => group.group_decline,
    GroupLeave            => group.group_leave,
    GroupKick             => group.group_kick,
    GroupMakeLeader       => group.group_make_leader,
    GuildAction           => guild.guild_action,
    TrainerOpenReq        => trainer.trainer_open_req,
    TrainerTrainReq       => trainer.trainer_train_req,
    ChatSend              => chat.chat_send,
}

#[derive(Clone, Default)]
pub struct ActionTracker(Arc<Mutex<State>>);
#[derive(Default)]
struct State {
    next: u64,
    pending: HashMap<u64, Pending>,
    queued: HashMap<usize, Vec<u64>>,
    names: HashMap<usize, Action>,
    drained: HashMap<usize, Vec<u64>>,
    events: Vec<ActionRecord>,
}
struct Pending { kind: String, accepted: bool, deadline: Instant }
#[derive(Clone, Debug)]
pub struct ActionRecord { pub id: u64, pub kind: String, pub result: String, pub reason: String }
#[derive(Clone)]
pub struct ActionContext { tracker: ActionTracker, id: u64, accepted: Arc<AtomicBool> }
tokio::task_local! { static CONTEXT: ActionContext; }
impl ActionContext {
    pub fn id(&self) -> u64 { self.id }
    pub fn accepted(&self) -> bool { self.accepted.load(Ordering::Acquire) }
    pub async fn scope<F: Future>(&self, future: F) -> F::Output { CONTEXT.scope(self.clone(), future).await }
}
impl ActionTracker {
    pub fn begin(&self, kind: &str) -> ActionContext {
        let mut state = self.0.lock().unwrap();
        state.next = state.next.checked_add(1).expect("action ID exhausted");
        let id = state.next;
        state.pending.insert(id, Pending { kind: kind.into(), accepted: false, deadline: Instant::now() + Duration::from_secs(30) });
        ActionContext { tracker: self.clone(), id, accepted: Arc::new(AtomicBool::new(false)) }
    }
    pub fn is_accepted(&self, id: u64) -> bool { self.0.lock().unwrap().pending.get(&id).is_some_and(|p| p.accepted) }
    pub fn discard(&self, id: u64) { self.0.lock().unwrap().pending.remove(&id); }
    pub fn finish(&self, id: u64, result: &str, reason: &str) {
        let mut state = self.0.lock().unwrap();
        if let Some(pending) = state.pending.remove(&id) {
            if pending.accepted { state.events.push(ActionRecord { id, kind: pending.kind, result: result.into(), reason: reason.into() }); }
        }
    }
    pub fn take_events(&self) -> Vec<ActionRecord> { std::mem::take(&mut self.0.lock().unwrap().events) }
    pub fn expire(&self) {
        let ids: Vec<_> = self.0.lock().unwrap().pending.iter().filter(|(_, p)| p.accepted && p.deadline <= Instant::now()).map(|(&id, _)| id).collect();
        for id in ids { self.finish(id, "unconfirmed", "outcome_timeout"); }
    }
    pub(crate) fn accept(&self, key: usize, awaited: bool, name: Action) {
        let _ = CONTEXT.try_with(|context| {
            if !Arc::ptr_eq(&self.0, &context.tracker.0) { return; }
            let mut state = self.0.lock().unwrap();
            state.names.insert(key, name);
            if let Some(p) = state.pending.get_mut(&context.id) {
                p.accepted = true;
                p.deadline = Instant::now() + Duration::from_secs(30);
                context.accepted.store(true, Ordering::Release);
                if !awaited { state.queued.entry(key).or_default().push(context.id); }
            }
        });
    }
    pub(crate) fn drain(&self, key: usize) {
        let mut state = self.0.lock().unwrap();
        if let Some(ids) = state.queued.remove(&key) { state.drained.entry(key).or_default().extend(ids); }
    }
    pub fn drain_scope(&self) -> DrainScope { DrainScope(self.clone()) }

}
pub struct DrainScope(ActionTracker);
impl Drop for DrainScope {
    fn drop(&mut self) {
        let drained = std::mem::take(&mut self.0.0.lock().unwrap().drained);
        for id in drained.into_values().flatten() { self.0.finish(id, "unconfirmed", "processed_without_server_confirmation"); }
    }
}
impl crate::CommandState {
    pub fn actions(&self) -> ActionTracker { self.actions.clone() }
    /// Report a local rejection for a command already drained this tick.
    /// `command` names the slot as an [`Action`], so it cannot drift from the `enqueue` that filled it.
    pub fn refuse_drained(&self, command: Action, reason: &str) {
        let ids = {
            let mut state = self.actions.0.lock().unwrap();
            let keys: Vec<_> = state.names.iter().filter(|&(_, &name)| name == command).map(|(&key, _)| key).collect();
            keys.into_iter().flat_map(|key| state.drained.remove(&key).unwrap_or_default()).collect::<Vec<_>>()
        };
        for id in ids { self.actions.finish(id, "refused", reason); }
    }
    pub(crate) fn enqueue<T>(&self, slot: &Mutex<Option<T>>, msg: T, awaited: bool, name: Action) -> bool {
        let mut value = slot.lock().unwrap();
        if value.is_some() { return false; }
        *value = Some(msg);
        self.actions.accept(slot as *const _ as usize, awaited, name);
        true
    }
    pub(crate) fn dequeue<T>(&self, slot: &Mutex<Option<T>>) -> Option<T> {
        let mut value = slot.lock().unwrap();
        let result = value.take();
        if result.is_some() { self.actions.drain(slot as *const _ as usize); }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::Action;
    use crate::CommandState;

    /// Duplicate labels would make `refuse_drained` refuse both slots, inventing a refusal for a
    /// command whose precondition was fine.
    #[test]
    fn every_action_label_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for action in Action::ALL {
            assert!(seen.insert(action.label()), "duplicate action label: {}", action.label());
        }
        assert_eq!(seen.len(), Action::ALL.len());
    }

    #[tokio::test]
    async fn accepted_command_survives_refusal_and_new_enqueue_after_drain() {
        let command = CommandState::default();
        let tracker = command.actions();
        let a = tracker.begin("target");
        let b = tracker.begin("target");
        assert!(a.scope(async { command.request_target(1) }).await);
        assert!(!b.scope(async { command.request_target(2) }).await);
        assert!(tracker.is_accepted(a.id()));
        assert!(!tracker.is_accepted(b.id()));
        {
            let _scope = tracker.drain_scope();
            assert_eq!(command.take_target(), Some(1));
            assert!(b.scope(async { command.request_target(2) }).await);
        }
        let events = tracker.take_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, a.id());
        assert_eq!(events[0].result, "unconfirmed");
        assert!(a.accepted(), "HTTP must still know acceptance after net finishes");
        assert!(tracker.is_accepted(b.id()));
        {
            let _scope = tracker.drain_scope();
            assert_eq!(command.take_target(), Some(2));
        }
        assert_eq!(tracker.take_events()[0].id, b.id());
    }
    #[tokio::test]
    async fn context_is_scoped_to_its_tracker_and_ui_is_untracked() {
        let command = CommandState::default();
        let other = CommandState::default();
        let tracker = command.actions();
        let context = tracker.begin("target");
        context.scope(async { assert!(other.request_target(1)); }).await;
        assert!(!tracker.is_accepted(context.id()));
        command.request_target(3);
        { let _scope = tracker.drain_scope(); command.take_target(); }
        assert!(tracker.take_events().is_empty());
    }
    #[tokio::test]
    async fn awaited_results_exclude_drain_fallback_and_finish_once() {
        let command = CommandState::default();
        let tracker = command.actions();
        let context = tracker.begin("open");
        let (tx, _rx) = tokio::sync::oneshot::channel();
        assert!(context.scope(async { command.request_open_await(1, tx) }).await);
        { let _scope = tracker.drain_scope(); command.take_open_await(); }
        assert!(tracker.take_events().is_empty());
        tracker.finish(context.id(), "confirmed", "server_reply");
        tracker.finish(context.id(), "unconfirmed", "timeout");
        assert_eq!(tracker.take_events().len(), 1);
    }
    #[tokio::test]
    async fn chat_fifo_keeps_every_request_identity() {
        let command = CommandState::default();
        let tracker = command.actions();
        for i in 0..3 {
            let context = tracker.begin("chat");
            context.scope(async { command.request_chat_send(eqoxide_ipc::ChatSend { chan: 5, to: String::new(), text: i.to_string() }); }).await;
        }
        { let _scope = tracker.drain_scope(); assert_eq!(command.take_chat_send().len(), 3); }
        assert_eq!(tracker.take_events().len(), 3);
    }
    #[tokio::test(start_paused = true)]
    async fn watchdog_expires_accepted_commands_once() {
        let command = CommandState::default();
        let tracker = command.actions();
        let context = tracker.begin("target");
        context.scope(async { command.request_target(1); }).await;
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        tracker.expire();
        let events = tracker.take_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].reason, "outcome_timeout");
        assert!(context.accepted());
        { let _scope = tracker.drain_scope(); command.take_target(); }
        tracker.expire();
        assert!(tracker.take_events().is_empty());
    }
    #[tokio::test]
    async fn refusal_names_only_the_drained_command() {
        let command = CommandState::default();
        let tracker = command.actions();
        let context = tracker.begin("target");
        context.scope(async { command.request_target(1); }).await;
        {
            let _scope = tracker.drain_scope();
            command.take_target();
            command.refuse_drained(Action::CombatTarget, "target_missing");
        }
        let events = tracker.take_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].result, "refused");
        assert_eq!(events[0].reason, "target_missing");
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_starts_at_admission_not_context_creation() {
        let command = CommandState::default();
        let tracker = command.actions();
        let context = tracker.begin("target");
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        tracker.expire();
        assert!(context.scope(async { command.request_target(1) }).await);
        assert!(context.accepted());
        tracker.expire();
        assert!(tracker.take_events().is_empty());
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        tracker.expire();
        assert_eq!(tracker.take_events()[0].id, context.id());
    }

}
