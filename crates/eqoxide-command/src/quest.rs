//! Quest command verbs — migrated per the Wave-2 fan-out pattern (see `mod.rs`).
//!
//! Domain: `/v1/quests/*` (accept/decline a selector offer, cancel an active task). Every method
//! is a thin typed read/write of a slot in `self.quest`; validation and packet-building stay where
//! they were (the HTTP handler, the quest journal window, and `ActionLoop::drain_quests`). No
//! behavior change — just one typed surface. The task-log/offer/completed rosters are read-path
//! snapshots (published by `ActionLoop`, read by `http/quests.rs` GETs) and are deliberately NOT
//! wrapped here — see `mod.rs`'s scope note.

use super::CommandState;

impl CommandState {
    // ── request_* : the VIEW (UI click-handlers + HTTP handlers) makes these writes ──────────────

    /// Accept one offered task (POST /v1/quests/accept {"task_id":N}, the quest journal's Accept
    /// button), or decline all pending offers using the `task_id == 0` sentinel (POST
    /// /v1/quests/decline, the journal's Decline button).
    pub fn request_accept_task(&self, task_id: u32) {
        *self.quest.accept_task.lock().unwrap() = Some(task_id);
    }

    /// Abandon an active task (POST /v1/quests/cancel {"task_id":N}, the journal's Abandon button).
    pub fn request_cancel_task(&self, task_id: u32) {
        *self.quest.cancel_task.lock().unwrap() = Some(task_id);
    }

    // ── take_* : the MODEL (`ActionLoop::drain_quests`) drains these once per tick ────────────────

    /// Drain a pending accept/decline-all request.
    pub fn take_accept_task(&self) -> Option<u32> {
        self.quest.accept_task.lock().unwrap().take()
    }

    /// Drain a pending cancel-task request.
    pub fn take_cancel_task(&self) -> Option<u32> {
        self.quest.cancel_task.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;

    /// A `request_*` write and the matching `take_*` drain touch the SAME slot, the drain removes
    /// it (so a stale command can't fire twice), and a second drain sees nothing. Proven for both
    /// quest command verbs.
    #[test]
    fn request_then_take_round_trips_each_quest_slot() {
        let cs = CommandState::default();

        cs.request_accept_task(42);
        assert_eq!(cs.take_accept_task(), Some(42));
        assert_eq!(cs.take_accept_task(), None, "a drained accept must not re-fire");

        // The decline-all sentinel (task_id 0) rides the same slot.
        cs.request_accept_task(0);
        assert_eq!(cs.take_accept_task(), Some(0));

        cs.request_cancel_task(7);
        assert_eq!(cs.take_cancel_task(), Some(7));
        assert_eq!(cs.take_cancel_task(), None);
    }

    /// A slot with nothing queued drains to `None` — no phantom command.
    #[test]
    fn take_on_empty_slot_is_none() {
        let cs = CommandState::default();
        assert_eq!(cs.take_accept_task(), None);
        assert_eq!(cs.take_cancel_task(), None);
    }
}
