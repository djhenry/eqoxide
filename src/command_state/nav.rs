//! Nav command verbs (#459 stragglers).
//!
//! Domain: `/v1/move/*` movement commands (goto / follow / stop / zone-cross). Slots live in
//! `self.nav`. Unlike combat's uniform one-shot-request/one-shot-drain shape, goto/follow/stop are
//! CONTINUOUS state: `goto_target`/`goto_entity` are held (not drained) across many nav-thread
//! ticks by `nav::walker::Walker` (its own `NavSlots` clone of the SAME Arcs — see `Walker::new` in
//! `main.rs`), which peeks/chases/clears them as part of its own pathing state machine. That
//! internal continuous read/write traffic is intentionally left un-migrated here (same carve-out as
//! `CommandState`'s documented read-path exclusion) — `walker.rs` is the model's own internals, not
//! a view. What DOES migrate: the VIEW writes (HTTP handlers, keyboard-cancel) that set/clear those
//! two slots, plus `zone_cross`, which — unlike goto/follow — genuinely is one-shot: written once by
//! a view and drained exactly once by `ActionLoop::drain_zone_cross`.
//!
//! `request_goto`/`request_follow`/`request_stop` mirror `POST /v1/move/{goto,follow,stop}`
//! (`http/move_api.rs`) exactly. `request_cancel_goto` is a DIFFERENT, narrower write used by
//! keyboard/manual-move cancellation (`app.rs`) and the melee-engage auto-cancel
//! (`eq_net/action_loop.rs`): it clears only `goto_target`, leaving `goto_entity` alone — preserving
//! the pre-migration behavior at each of those call sites (they never touched `goto_entity`).

use super::CommandState;

impl CommandState {
    // ── request_* : the VIEW (HTTP handlers, keyboard input) makes these writes ───────────────────

    /// Walk to a fixed point and stop on arrival (POST /v1/move/goto, and the zone-cross walker's
    /// own resolved destination). Clears any in-progress chase — a goto never chases.
    pub fn request_goto(&self, target: (f32, f32, f32)) {
        *self.nav.goto_target.lock().unwrap() = Some(target);
        *self.nav.goto_entity.lock().unwrap() = None;
    }

    /// Walk to a named entity's current position and KEEP CHASING it (POST /v1/move/follow). `key`
    /// is the `entity_positions` key the walker re-resolves each tick; `pos` seeds the initial goal.
    pub fn request_follow(&self, key: String, pos: (f32, f32, f32)) {
        *self.nav.goto_target.lock().unwrap() = Some(pos);
        *self.nav.goto_entity.lock().unwrap() = Some(key);
    }

    /// Cancel any active goto/follow (POST /v1/move/stop). Clears both slots — idempotent.
    pub fn request_stop(&self) {
        *self.nav.goto_target.lock().unwrap() = None;
        *self.nav.goto_entity.lock().unwrap() = None;
    }

    /// Cancel an in-progress goto WITHOUT touching `goto_entity` — used where manual movement
    /// (keyboard WASD, the HTTP manual-move escape hatch, or an auto-melee-engage override) needs to
    /// take over steering this frame/tick but isn't itself a `/stop`. Narrower than
    /// [`Self::request_stop`] on purpose; preserves the exact pre-migration call sites' behavior.
    pub fn request_cancel_goto(&self) {
        *self.nav.goto_target.lock().unwrap() = None;
    }

    /// Queue a zone-line crossing (POST /v1/move/zone_cross). `0` = nearest line, `Some(id)` = a
    /// specific destination zone id (pre-validated as reachable by the HTTP handler).
    pub fn request_zone_cross(&self, zone_id: u16) {
        *self.nav.zone_cross.lock().unwrap() = Some(zone_id);
    }

    // ── take_* : the MODEL (`ActionLoop::drain_zone_cross`) drains this once per tick ──────────────

    /// Drain a pending zone-cross request. `zone_cross` is the one genuinely one-shot nav slot.
    pub fn take_zone_cross(&self) -> Option<u16> {
        self.nav.zone_cross.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;

    #[test]
    fn request_goto_sets_target_and_clears_entity() {
        let cs = CommandState::default();
        cs.request_follow("a_mob".to_string(), (1.0, 2.0, 3.0));
        cs.request_goto((4.0, 5.0, 6.0));
        assert_eq!(*cs.nav.goto_target.lock().unwrap(), Some((4.0, 5.0, 6.0)));
        assert_eq!(*cs.nav.goto_entity.lock().unwrap(), None);
    }

    #[test]
    fn request_follow_sets_both_target_and_entity() {
        let cs = CommandState::default();
        cs.request_follow("a_mob".to_string(), (1.0, 2.0, 3.0));
        assert_eq!(*cs.nav.goto_target.lock().unwrap(), Some((1.0, 2.0, 3.0)));
        assert_eq!(*cs.nav.goto_entity.lock().unwrap(), Some("a_mob".to_string()));
    }

    #[test]
    fn request_stop_clears_both_slots() {
        let cs = CommandState::default();
        cs.request_follow("a_mob".to_string(), (1.0, 2.0, 3.0));
        cs.request_stop();
        assert_eq!(*cs.nav.goto_target.lock().unwrap(), None);
        assert_eq!(*cs.nav.goto_entity.lock().unwrap(), None);
    }

    #[test]
    fn request_cancel_goto_clears_only_target() {
        let cs = CommandState::default();
        cs.request_follow("a_mob".to_string(), (1.0, 2.0, 3.0));
        cs.request_cancel_goto();
        assert_eq!(*cs.nav.goto_target.lock().unwrap(), None);
        assert_eq!(*cs.nav.goto_entity.lock().unwrap(), Some("a_mob".to_string()),
            "request_cancel_goto must not touch goto_entity — that's request_stop's job");
    }

    #[test]
    fn request_then_take_zone_cross_round_trips() {
        let cs = CommandState::default();
        assert_eq!(cs.take_zone_cross(), None);
        cs.request_zone_cross(0);
        assert_eq!(cs.take_zone_cross(), Some(0));
        assert_eq!(cs.take_zone_cross(), None, "a drained zone_cross must not re-fire");

        cs.request_zone_cross(42);
        assert_eq!(cs.take_zone_cross(), Some(42));
    }
}
