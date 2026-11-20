//! `group.*` — group (party) management: invite, accept, decline, leave, kick, makeleader,
//! and the live roster view.

use crate::ops::wire::{invalid, ok, ok_text, OpError, OpResult};
use eqoxide_agent_protocol::op::{Named};
use super::*;
use crate::ops::refusal::Refusal;


/// `group.roster` — current group roster, leader, and any pending invite. Empty
/// `members` means not currently grouped.
pub(super) async fn get_roster(s: OpState) -> OpResult {
    let g = s.group_slots.group.lock().unwrap();
    ok(reply::GroupRoster {
        leader: g.leader.clone(),
        pending_invite: g.pending_invite.clone(),
        you_are_leader: g.you_are_leader,
        members: g.members.iter().map(|m| reply::GroupMember {
            name: m.name.clone(),
            level: m.level,
            is_leader: m.is_leader,
            is_merc: m.is_merc,
            tank: m.tank,
            assist: m.assist,
            puller: m.puller,
            offline: m.offline,
            hp_pct: m.hp_pct,
        }).collect(),
    })
}

// ── 409 CONFLICT body for an occupied command slot (#347 step 2) ─────────────────────────────────
// Every `group.*` verb queues into its OWN single-slot mailbox the net thread drains once per
// tick. Before #347 a second request inside that window OVERWROTE the pending one and BOTH callers
// were told `200`, so one of the two actions silently never happened. The slot now refuses the
// second write and keeps the first. A 409 here means the request was NOT queued and definitively
// did not happen — retrying after the drain is safe. The slots are per-verb, so this only ever
// refuses a repeat of the SAME verb (two invites, two kicks, …), never a different one.
const BUSY_GROUP: &str = "the same group action is already queued and undrained — retry in a moment (it was NOT queued)";

/// The player name in `args`, or an `Invalid` refusal when the name is blank.
pub(super) fn checked_name(args: Named) -> Result<String, OpError> {
    if args.name.trim().is_empty() {
        return Err(OpError::invalid("provide {\"name\":\"X\"}"));
    }
    Ok(args.name)
}

/// `group.invite` {"name":"X"} — send an invite to player X.
pub(super) async fn post_invite(s: OpState, args: Named) -> OpResult {
    require_live_session(&s)?;
    let name = checked_name(args)?;
    if let Some(busy) = s.command.request_group_invite(name.clone()).refused(BUSY_GROUP) { return Err(busy); }
    tracing::info!("group: queued invite to {name}");
    ok_text(format!("inviting {name}"))
}

/// `group.accept` — accept the current pending invite. 400 if there is none.
pub(super) async fn post_accept(s: OpState) -> OpResult {
    require_live_session(&s)?;
    if s.group_slots.group.lock().unwrap().pending_invite.is_none() {
        return invalid("no pending invite");
    }
    if let Some(busy) = s.command.request_group_accept().refused(BUSY_GROUP) { return Err(busy); }
    tracing::info!("group: queued accept");
    ok_text("accepting invite")
}

/// `group.decline` — decline the current pending invite (sends a defensive
/// OP_GroupDisband cleanup — RoF2 has no working OP_GroupCancelInvite). 400 if there is none.
pub(super) async fn post_decline(s: OpState) -> OpResult {
    require_live_session(&s)?;
    if s.group_slots.group.lock().unwrap().pending_invite.is_none() {
        return invalid("no pending invite");
    }
    if let Some(busy) = s.command.request_group_decline().refused(BUSY_GROUP) { return Err(busy); }
    tracing::info!("group: queued decline");
    ok_text("declining invite")
}

/// `group.leave` — leave the current group. If leader with < 3 total members, this fully
/// disbands the group (confirmed EQEmu server behavior — no auto handoff). 400 if not grouped.
pub(super) async fn post_leave(s: OpState) -> OpResult {
    require_live_session(&s)?;
    if s.group_slots.group.lock().unwrap().members.is_empty() {
        return invalid("not in a group");
    }
    if let Some(busy) = s.command.request_group_leave().refused(BUSY_GROUP) { return Err(busy); }
    tracing::info!("group: queued leave");
    ok_text("leaving group")
}

/// `group.kick` {"name":"X"} — leader-only: remove member X. 400 if not leader or X isn't
/// a current member.
pub(super) async fn post_kick(s: OpState, args: Named) -> OpResult {
    require_live_session(&s)?;
    let name = checked_name(args)?;
    let g = s.group_slots.group.lock().unwrap();
    if !g.you_are_leader {
        return invalid("only the group leader can kick");
    }
    if !g.members.iter().any(|m| m.name == name) {
        return invalid(format!("{name} is not a current group member"));
    }
    drop(g);
    if let Some(busy) = s.command.request_group_kick(name.clone()).refused(BUSY_GROUP) { return Err(busy); }
    tracing::info!("group: queued kick of {name}");
    ok_text(format!("kicking {name}"))
}

/// `group.makeleader` {"name":"X"} — leader-only: transfer leadership to X without
/// disbanding. 400 if not leader or X isn't a current member.
pub(super) async fn post_makeleader(s: OpState, args: Named) -> OpResult {
    require_live_session(&s)?;
    let name = checked_name(args)?;
    let g = s.group_slots.group.lock().unwrap();
    if !g.you_are_leader {
        return invalid("only the group leader can transfer leadership");
    }
    if !g.members.iter().any(|m| m.name == name) {
        return invalid(format!("{name} is not a current group member"));
    }
    drop(g);
    if let Some(busy) = s.command.request_group_make_leader(name.clone()).refused(BUSY_GROUP) { return Err(busy); }
    tracing::info!("group: queued makeleader {name}");
    ok_text(format!("transferring leadership to {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_agent_protocol::call::CallStatus;
    use crate::ops::testkit::call;
    use eqoxide_agent_protocol::op::{Named, Op};

    fn empty_state() -> OpState {
        crate::ops::testkit::empty_state()
    }

    fn named(name: &str) -> Named {
        Named { name: name.to_string() }
    }

    #[tokio::test]
    async fn roster_is_empty_when_not_grouped() {
        let state = empty_state();
        let resp = call(&state, Op::GroupRoster).await;
        assert_eq!(resp.status, CallStatus::Ok);
        let json = resp.body;
        assert_eq!(json["members"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn invite_with_empty_name_is_400() {
        let state = empty_state();
        let resp = call(&state, Op::GroupInvite(named(""))).await;
        assert_eq!(resp.status, CallStatus::Invalid);
    }

    #[tokio::test]
    async fn invite_with_name_is_200_and_queues_request() {
        let state = empty_state();
        let command = state.command.clone();
        let resp = call(&state, Op::GroupInvite(named("Sariel"))).await;
        assert_eq!(resp.status, CallStatus::Ok);
        assert_eq!(command.take_group_invite(), Some("Sariel".to_string()));
    }

    #[tokio::test]
    async fn accept_with_no_pending_invite_is_400() {
        let state = empty_state();
        let resp = call(&state, Op::GroupAccept).await;
        assert_eq!(resp.status, CallStatus::Invalid);
    }

    #[tokio::test]
    async fn accept_with_pending_invite_is_200_and_queues_request() {
        let state = empty_state();
        state.group_slots.group.lock().unwrap().pending_invite = Some("Sariel".into());
        let command = state.command.clone();
        let resp = call(&state, Op::GroupAccept).await;
        assert_eq!(resp.status, CallStatus::Ok);
        assert!(command.take_group_accept().is_some());
    }

    #[tokio::test]
    async fn decline_with_no_pending_invite_is_400() {
        let state = empty_state();
        let resp = call(&state, Op::GroupDecline).await;
        assert_eq!(resp.status, CallStatus::Invalid);
    }

    #[tokio::test]
    async fn leave_when_not_grouped_is_400() {
        let state = empty_state();
        let resp = call(&state, Op::GroupLeave).await;
        assert_eq!(resp.status, CallStatus::Invalid);
    }

    #[tokio::test]
    async fn leave_when_grouped_is_200_and_queues_request() {
        let state = empty_state();
        state.group_slots.group.lock().unwrap().members.push(GroupMemberView {
            name: "Aldric".into(), level: 10, is_leader: true, is_merc: false,
            tank: false, assist: false, puller: false, offline: false, hp_pct: 100.0,
        });
        let command = state.command.clone();
        let resp = call(&state, Op::GroupLeave).await;
        assert_eq!(resp.status, CallStatus::Ok);
        assert!(command.take_group_leave().is_some());
    }

    #[tokio::test]
    async fn kick_when_not_leader_is_400() {
        let state = empty_state();
        state.group_slots.group.lock().unwrap().members.push(GroupMemberView {
            name: "Sariel".into(), level: 8, is_leader: false, is_merc: false,
            tank: false, assist: false, puller: false, offline: false, hp_pct: 100.0,
        });
        state.group_slots.group.lock().unwrap().you_are_leader = false;
        let resp = call(&state, Op::GroupKick(named("Sariel"))).await;
        assert_eq!(resp.status, CallStatus::Invalid);
    }

    #[tokio::test]
    async fn kick_unknown_member_is_400() {
        let state = empty_state();
        state.group_slots.group.lock().unwrap().you_are_leader = true;
        let resp = call(&state, Op::GroupKick(named("Nobody"))).await;
        assert_eq!(resp.status, CallStatus::Invalid);
    }

    #[tokio::test]
    async fn kick_known_member_as_leader_is_200_and_queues_request() {
        let state = empty_state();
        {
            let mut g = state.group_slots.group.lock().unwrap();
            g.you_are_leader = true;
            g.members.push(GroupMemberView {
                name: "Sariel".into(), level: 8, is_leader: false, is_merc: false,
                tank: false, assist: false, puller: false, offline: false, hp_pct: 100.0,
            });
        }
        let command = state.command.clone();
        let resp = call(&state, Op::GroupKick(named("Sariel"))).await;
        assert_eq!(resp.status, CallStatus::Ok);
        assert_eq!(command.take_group_kick(), Some("Sariel".to_string()));
    }

    #[tokio::test]
    async fn makeleader_when_not_leader_is_400() {
        let state = empty_state();
        {
            // Seed the target as a current member so the membership guard is satisfied and
            // the LEADER guard is the one actually under test here — without this, the 400
            // comes from "Sariel is not a current group member" instead. See #355 M4.
            let mut g = state.group_slots.group.lock().unwrap();
            g.you_are_leader = false;
            g.members.push(GroupMemberView {
                name: "Sariel".into(), level: 8, is_leader: false, is_merc: false,
                tank: false, assist: false, puller: false, offline: false, hp_pct: 100.0,
            });
        }
        let resp = call(&state, Op::GroupMakeLeader(named("Sariel"))).await;
        assert_eq!(resp.status, CallStatus::Invalid);
        assert_eq!(resp.body, "only the group leader can transfer leadership");
    }

    #[tokio::test]
    async fn makeleader_known_member_as_leader_is_200_and_queues_request() {
        let state = empty_state();
        {
            let mut g = state.group_slots.group.lock().unwrap();
            g.you_are_leader = true;
            g.members.push(GroupMemberView {
                name: "Sariel".into(), level: 8, is_leader: false, is_merc: false,
                tank: false, assist: false, puller: false, offline: false, hp_pct: 100.0,
            });
        }
        let command = state.command.clone();
        let resp = call(&state, Op::GroupMakeLeader(named("Sariel"))).await;
        assert_eq!(resp.status, CallStatus::Ok);
        assert_eq!(command.take_group_make_leader(), Some("Sariel".to_string()));
    }

    /// #347 step 2 (review round 1, B1): two invites inside one undrained tick. Before the fix the
    /// second OVERWROTE the first and BOTH callers were told `200`, so one invite silently never
    /// went out — the agent believed it had invited two people and had invited one.
    #[tokio::test]
    async fn a_second_invite_before_the_drain_is_409_and_the_first_survives() {
        let state = empty_state();
        let command = state.command.clone();

        let first = call(&state, Op::GroupInvite(named("Sariel"))).await;
        assert_eq!(first.status, CallStatus::Ok);

        let second = call(&state, Op::GroupInvite(named("Aldric"))).await;
        assert_eq!(second.status, CallStatus::Conflict,
            "the second invite must be refused, not silently swallowed");

        assert_eq!(command.take_group_invite(), Some("Sariel".to_string()),
            "the FIRST invite must be the one that survives to the net thread");
        assert_eq!(command.take_group_invite(), None,
            "and nothing else may be queued behind it");
    }

    /// The unit-slot verbs (`accept`/`decline`/`leave`) carry no payload, so a lost overwrite is
    /// invisible in the drained value — the only observable difference is the status code. `accept`
    /// stands in for all three.
    #[tokio::test]
    async fn a_second_accept_before_the_drain_is_409() {
        let state = empty_state();
        state.group_slots.group.lock().unwrap().pending_invite = Some("Sariel".into());
        let command = state.command.clone();

        let first = call(&state, Op::GroupAccept).await;
        assert_eq!(first.status, CallStatus::Ok);

        let second = call(&state, Op::GroupAccept).await;
        assert_eq!(second.status, CallStatus::Conflict,
            "the second accept must be refused, not silently swallowed");

        assert!(command.take_group_accept().is_some());
        assert!(command.take_group_accept().is_none(),
            "and nothing else may be queued behind it");
    }
}
