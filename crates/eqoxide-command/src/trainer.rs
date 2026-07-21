//! Trainer command verbs — migrated per the Wave-2 fan-out pattern (see `mod.rs`).
//!
//! Domain: `/v1/trainer/*` (open a training window with a nearby guildmaster, close it, train one
//! point of a skill). Every method is a thin typed read/write of a slot in `self.trainer`;
//! validation and packet-building stay where they were (the HTTP handlers, the trainer window, and
//! `ActionLoop::drain_trainer`). No behavior change — just one typed surface.

use super::CommandState;

impl CommandState {
    // ── request_* : the VIEW (UI click-handlers + HTTP handlers) makes these writes ──────────────

    /// Open a training session with `npc_id` (POST /v1/trainer/open, resolved by name), or end the
    /// current session using the `npc_id == 0` sentinel (POST /v1/trainer/close — 0 is never a real
    /// spawn id).
    pub fn request_open_trainer(&self, npc_id: u32) {
        *self.trainer.trainer_open_req.lock().unwrap() = Some(npc_id);
    }

    /// Train one point of `skill_id` at the open trainer (POST /v1/trainer/train, the trainer
    /// window's Train button).
    pub fn request_train_skill(&self, skill_id: u32) {
        *self.trainer.trainer_train_req.lock().unwrap() = Some(skill_id);
    }

    // ── take_* : the MODEL (`ActionLoop::drain_trainer`) drains these once per tick ───────────────

    /// Drain a pending open/close request. `Some(0)` means close.
    pub fn take_trainer_open(&self) -> Option<u32> {
        self.trainer.trainer_open_req.lock().unwrap().take()
    }

    /// Drain a pending train-skill request.
    pub fn take_train_skill(&self) -> Option<u32> {
        self.trainer.trainer_train_req.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;

    /// A `request_*` write and the matching `take_*` drain touch the SAME slot, the drain removes
    /// it (so a stale command can't fire twice), and a second drain sees nothing. Proven for both
    /// trainer command verbs.
    #[test]
    fn request_then_take_round_trips_each_trainer_slot() {
        let cs = CommandState::default();

        cs.request_open_trainer(9);
        assert_eq!(cs.take_trainer_open(), Some(9));
        assert_eq!(cs.take_trainer_open(), None, "a drained open must not re-fire");

        // The close sentinel (npc_id 0) rides the same slot.
        cs.request_open_trainer(0);
        assert_eq!(cs.take_trainer_open(), Some(0));

        cs.request_train_skill(3);
        assert_eq!(cs.take_train_skill(), Some(3));
        assert_eq!(cs.take_train_skill(), None);
    }

    /// A slot with nothing queued drains to `None` — no phantom command.
    #[test]
    fn take_on_empty_slot_is_none() {
        let cs = CommandState::default();
        assert_eq!(cs.take_trainer_open(), None);
        assert_eq!(cs.take_train_skill(), None);
    }
}
