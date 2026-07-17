//! Lifecycle command verbs (#459 stragglers).
//!
//! Domain: `/v1/lifecycle/*` (camp/exit + respawn). Slots live in `self.lifecycle`.
//!
//! Unlike every other migrated domain, the DRAIN side of `camp`/`respawn` is not `ActionLoop` at
//! all: `ipc::LifecycleSlots`'s own doc says `ActionLoop` only ever WRITES `camp` (the `/camp` chat
//! keyword) — the real reader is `eq_net::gameplay::run_gameplay_phase`, a standalone async fn that
//! is handed the raw `camp`/`camp_until`/`respawn` Arcs directly by `run_login_flow`, bypassing
//! `ActionLoop`/`CommandState` entirely. That drain has real retry-cadence state of its own
//! (`last_respawn_retry`, `pending_respawn`, `RESPAWN_RETRY_INTERVAL`) — model-internal logic, not a
//! view command — so, mirroring the nav-domain carve-out (`walker.rs`'s internal state machine), it
//! is deliberately left un-migrated here rather than threading `CommandState` through
//! `run_login_flow`/`run_gameplay_phase` for a read site that was never a raw "poke" to begin with.
//! `camp_until` (the published camp-deadline snapshot) is read-path and was never a candidate.
//!
//! What DOES migrate: every VIEW write to `camp`/`respawn` — the HTTP handlers
//! (`http/lifecycle.rs`), the HUD Camp button and Respawn button (`ui/windows/actions.rs`), and the
//! `/camp` chat keyword (`ui/windows/chat.rs`, `eq_net/action_loop.rs::drain_chat`).

use super::CommandState;
use crate::ipc::CampCmd;

impl CommandState {
    /// Start or toggle a camp (POST /v1/lifecycle/exit uses `Start`; POST /v1/lifecycle/camp, the
    /// HUD Camp button, and the `/camp` chat keyword use `Toggle`). See `ipc::CampCmd`.
    pub fn request_camp(&self, cmd: CampCmd) {
        *self.lifecycle.camp.lock().unwrap() = Some(cmd);
    }

    /// Release a held-dead character back to its bind point (POST /v1/lifecycle/respawn, the HUD
    /// Respawn button). A plain flag, not `Option` — `gameplay::run_gameplay_phase` clears it itself
    /// once the retry-driven respawn completes (see module docs).
    pub fn request_respawn(&self) {
        *self.lifecycle.respawn.lock().unwrap() = true;
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;
    use crate::ipc::CampCmd;

    #[test]
    fn request_camp_sets_the_slot() {
        let cs = CommandState::default();
        assert!(cs.lifecycle.camp.lock().unwrap().is_none());
        cs.request_camp(CampCmd::Toggle);
        assert_eq!(*cs.lifecycle.camp.lock().unwrap(), Some(CampCmd::Toggle));
        cs.request_camp(CampCmd::Start);
        assert_eq!(*cs.lifecycle.camp.lock().unwrap(), Some(CampCmd::Start));
    }

    #[test]
    fn request_respawn_sets_the_flag() {
        let cs = CommandState::default();
        assert!(!*cs.lifecycle.respawn.lock().unwrap());
        cs.request_respawn();
        assert!(*cs.lifecycle.respawn.lock().unwrap());
    }
}
