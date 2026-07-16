//! Group (party) command verbs — invite/accept/decline/leave/kick/make-leader.
//!
//! Domain: `/v1/group/*` (the Group HUD window + HTTP handlers). Every method is a thin typed
//! read/write of a slot in `self.group`; validation (leader-only, membership, pending-invite
//! checks) and packet-building stay where they were (the HTTP handler and `ActionLoop::tick`'s
//! `drain_group`). No behavior change — just one typed surface. `self.group.group` (the roster
//! snapshot) is deliberately NOT exposed here — that's read-path, not a command (see `mod.rs`).

use super::CommandState;

impl CommandState {
    // ── request_* : the VIEW (UI click-handlers + HTTP handlers) makes these writes ──────────────

    /// Send an invite (POST /v1/group/invite {"name"}).
    pub fn request_group_invite(&self, name: String) {
        *self.group.group_invite.lock().unwrap() = Some(name);
    }

    /// Accept the current pending invite (POST /v1/group/accept, the invite banner's Accept
    /// button). Caller checks a pending invite exists first.
    pub fn request_group_accept(&self) {
        *self.group.group_accept.lock().unwrap() = Some(());
    }

    /// Decline the current pending invite (POST /v1/group/decline, the invite banner's Decline
    /// button). Caller checks a pending invite exists first.
    pub fn request_group_decline(&self) {
        *self.group.group_decline.lock().unwrap() = Some(());
    }

    /// Leave the current group (POST /v1/group/leave, the Group window's Leave button). Caller
    /// checks the player is currently grouped first.
    pub fn request_group_leave(&self) {
        *self.group.group_leave.lock().unwrap() = Some(());
    }

    /// Kick a member (POST /v1/group/kick {"name"}, or the Group window's per-row ✕ / context
    /// menu). Caller checks leadership + membership first.
    pub fn request_group_kick(&self, name: String) {
        *self.group.group_kick.lock().unwrap() = Some(name);
    }

    /// Transfer leadership (POST /v1/group/makeleader {"name"}, or the Group window's context
    /// menu). Caller checks leadership + membership first.
    pub fn request_group_make_leader(&self, name: String) {
        *self.group.group_make_leader.lock().unwrap() = Some(name);
    }

    // ── take_* : the MODEL (`ActionLoop::tick`'s `drain_group`) drains these once per tick ────────

    /// Drain a pending invite request.
    pub fn take_group_invite(&self) -> Option<String> {
        self.group.group_invite.lock().unwrap().take()
    }

    /// Drain a pending accept request.
    pub fn take_group_accept(&self) -> Option<()> {
        self.group.group_accept.lock().unwrap().take()
    }

    /// Drain a pending decline request.
    pub fn take_group_decline(&self) -> Option<()> {
        self.group.group_decline.lock().unwrap().take()
    }

    /// Drain a pending leave request.
    pub fn take_group_leave(&self) -> Option<()> {
        self.group.group_leave.lock().unwrap().take()
    }

    /// Drain a pending kick request.
    pub fn take_group_kick(&self) -> Option<String> {
        self.group.group_kick.lock().unwrap().take()
    }

    /// Drain a pending make-leader request.
    pub fn take_group_make_leader(&self) -> Option<String> {
        self.group.group_make_leader.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;

    /// A `request_*` write and the matching `take_*` drain touch the SAME slot, the drain removes
    /// it, and a second drain sees nothing — proven for every group verb.
    #[test]
    fn request_then_take_round_trips_each_group_slot() {
        let cs = CommandState::default();

        cs.request_group_invite("Sariel".into());
        assert_eq!(cs.take_group_invite(), Some("Sariel".to_string()));
        assert_eq!(cs.take_group_invite(), None, "a drained invite must not re-fire");

        cs.request_group_accept();
        assert_eq!(cs.take_group_accept(), Some(()));
        assert_eq!(cs.take_group_accept(), None);

        cs.request_group_decline();
        assert_eq!(cs.take_group_decline(), Some(()));
        assert_eq!(cs.take_group_decline(), None);

        cs.request_group_leave();
        assert_eq!(cs.take_group_leave(), Some(()));
        assert_eq!(cs.take_group_leave(), None);

        cs.request_group_kick("Aldric".into());
        assert_eq!(cs.take_group_kick(), Some("Aldric".to_string()));
        assert_eq!(cs.take_group_kick(), None);

        cs.request_group_make_leader("Kessen".into());
        assert_eq!(cs.take_group_make_leader(), Some("Kessen".to_string()));
        assert_eq!(cs.take_group_make_leader(), None);
    }

    /// A slot with nothing queued drains to `None` — no phantom command.
    #[test]
    fn take_on_empty_slot_is_none() {
        let cs = CommandState::default();
        assert_eq!(cs.take_group_invite(), None);
        assert_eq!(cs.take_group_accept(), None);
        assert_eq!(cs.take_group_kick(), None);
    }
}
