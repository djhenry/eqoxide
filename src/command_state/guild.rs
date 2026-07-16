//! Guild command verb — the one queued guild action.
//!
//! Domain: `/v1/guild/*` (invite/accept/leave/remove, HTTP-only — no dedicated guild UI window
//! today). All four verbs share a single slot (`self.guild.guild_action`, an `Option<GuildAction>`
//! enum) so `ActionLoop` only has one field to drain — see `ipc::GuildActionReq`'s doc. No
//! behavior change — just one typed surface. `self.guild.guild` (the roster/identity snapshot) is
//! deliberately NOT exposed here — that's read-path, not a command (see `mod.rs`).

use super::CommandState;
use crate::ipc::GuildAction;

impl CommandState {
    // ── request_* : the VIEW (the HTTP handlers) makes this write ─────────────────────────────────

    /// Queue one guild action (POST /v1/guild/{invite,accept,leave,remove}). Returns `false` (and
    /// leaves the already-pending action untouched) if a guild action is already queued and
    /// undrained — preserves the original `queue()` helper's atomic check-then-set, which the HTTP
    /// handler surfaces as 409 CONFLICT rather than silently clobbering a pending action.
    pub fn request_guild_action(&self, action: GuildAction) -> bool {
        let mut slot = self.guild.guild_action.lock().unwrap();
        if slot.is_some() {
            return false;
        }
        *slot = Some(action);
        true
    }

    // ── take_* : the MODEL (`ActionLoop::tick`'s `drain_guild`) drains this once per tick ─────────

    /// Drain a pending guild action.
    pub fn take_guild_action(&self) -> Option<GuildAction> {
        self.guild.guild_action.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;
    use crate::ipc::GuildAction;

    /// A `request_guild_action` write and the matching `take_guild_action` drain touch the SAME
    /// slot, the drain removes it, and a second drain sees nothing.
    #[test]
    fn request_then_take_round_trips_the_guild_action_slot() {
        let cs = CommandState::default();

        assert!(cs.request_guild_action(GuildAction::Invite("Sariel".into())));
        assert_eq!(cs.take_guild_action(), Some(GuildAction::Invite("Sariel".into())));
        assert_eq!(cs.take_guild_action(), None, "a drained action must not re-fire");

        assert!(cs.request_guild_action(GuildAction::Leave));
        assert_eq!(cs.take_guild_action(), Some(GuildAction::Leave));
    }

    /// A second `request_guild_action` while one is already pending is rejected (`false`) and does
    /// NOT clobber the first — mirrors the original `queue()` helper's 409 CONFLICT behavior.
    #[test]
    fn request_guild_action_rejects_when_already_pending() {
        let cs = CommandState::default();

        assert!(cs.request_guild_action(GuildAction::Accept));
        assert!(!cs.request_guild_action(GuildAction::Leave), "a second queued action must be rejected");
        assert_eq!(cs.take_guild_action(), Some(GuildAction::Accept), "the first action must survive untouched");
    }

    /// A slot with nothing queued drains to `None` — no phantom command.
    #[test]
    fn take_on_empty_slot_is_none() {
        let cs = CommandState::default();
        assert_eq!(cs.take_guild_action(), None);
    }
}
