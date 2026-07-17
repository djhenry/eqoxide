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

    /// Stamp a FRESH GOAL IDENTITY on `nav_state` (#349), atomically with accepting a new request.
    ///
    /// This is the fix for "a read right after `POST /goto` returns the PREVIOUS goto's terminal
    /// state": we bump the monotonic `goal_id` and immediately reset `state` to `new_state` (an
    /// in-progress value, not a leftover terminal one) under a SINGLE lock hold. Because both the
    /// slot writes above and this stamp happen synchronously in the accepting call — before the
    /// walker's next ~150ms tick — there is no window in which a concurrent `/observe/debug` read can
    /// see the new `goal_id` paired with the old goal's `arrived`/`no_path`/`blocked`. The previous
    /// route's per-instance facts (`reason`/`blocked_*`/`tier`/`local`) are cleared for the same
    /// reason. Returns the new `goal_id` so the accepting HTTP handler can echo it to the caller.
    fn stamp_new_goal(&self, new_state: &str, goal: Option<(f32, f32, f32)>) -> u64 {
        let mut s = self.nav.nav_state.lock().unwrap();
        s.goal_id += 1;
        s.state = new_state.to_string();
        s.reason = None;
        s.goal = goal.map(|(x, y, z)| [x, y, z]);
        s.blocked_goal = None;
        s.blocked_frontier = None;
        s.tier = None;
        s.local = None;
        s.goal_id
    }

    /// Walk to a fixed point and stop on arrival (POST /v1/move/goto, and the zone-cross walker's
    /// own resolved destination). Clears any in-progress chase — a goto never chases. Returns the new
    /// `goal_id` (#349): the state is reset to `pending` for this fresh goal, so a caller can never
    /// read the previous goto's terminal outcome as this one's.
    pub fn request_goto(&self, target: (f32, f32, f32)) -> u64 {
        *self.nav.goto_target.lock().unwrap() = Some(target);
        *self.nav.goto_entity.lock().unwrap() = None;
        self.stamp_new_goal("pending", Some(target))
    }

    /// Walk to a named entity's current position and KEEP CHASING it (POST /v1/move/follow). `key`
    /// is the `entity_positions` key the walker re-resolves each tick; `pos` seeds the initial goal.
    /// Returns the new `goal_id` (#349); state resets to `pending`.
    pub fn request_follow(&self, key: String, pos: (f32, f32, f32)) -> u64 {
        *self.nav.goto_target.lock().unwrap() = Some(pos);
        *self.nav.goto_entity.lock().unwrap() = Some(key);
        self.stamp_new_goal("pending", Some(pos))
    }

    /// Cancel any active goto/follow (POST /v1/move/stop). Clears both slots — idempotent. Returns
    /// the new `goal_id` (#349): the state resets to `idle` under a fresh identity, so a read right
    /// after `/stop` can never return the cancelled goal's terminal `arrived`/`no_path`.
    pub fn request_stop(&self) -> u64 {
        *self.nav.goto_target.lock().unwrap() = None;
        *self.nav.goto_entity.lock().unwrap() = None;
        self.stamp_new_goal("idle", None)
    }

    /// Cancel an in-progress goto WITHOUT touching `goto_entity` — used where manual movement
    /// (keyboard WASD, the HTTP manual-move escape hatch, or an auto-melee-engage override) needs to
    /// take over steering this frame/tick but isn't itself a `/stop`. Narrower than
    /// [`Self::request_stop`] on purpose; preserves the exact pre-migration call sites' behavior.
    pub fn request_cancel_goto(&self) {
        *self.nav.goto_target.lock().unwrap() = None;
    }

    /// Queue a zone-line crossing (POST /v1/move/zone_cross). `0` = nearest line, `Some(id)` = a
    /// specific destination zone id (pre-validated as reachable by the HTTP handler). Returns the new
    /// `goal_id` (#349): the state resets to `pending` synchronously, so a read right after the 200
    /// can't return the previous nav's terminal state before the walker drains this request and
    /// resolves the concrete zone-line goal (which re-stamps a fresh id via `request_goto`). `goal`
    /// is left `None` until that resolution — the concrete destination isn't known yet.
    pub fn request_zone_cross(&self, zone_id: u16) -> u64 {
        *self.nav.zone_cross.lock().unwrap() = Some(zone_id);
        self.stamp_new_goal("pending", None)
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

    /// #349 regression — the exact bug: goto A, drive it to the terminal `arrived`, then goto B.
    /// A `nav_state` read (the shared `nav_state` Arc that GET /v1/observe/debug locks) must now
    /// report B's identity and an in-progress state, NEVER A's leftover `arrived`.
    ///
    /// Mutation check: delete the `stamp_new_goal("pending", …)` call in `request_goto` (leave only
    /// the two slot writes) and this test goes RED — the read still sees `state: "arrived"` with the
    /// stale `goal_id`, which is precisely the confident-but-stale answer #349 is about.
    #[test]
    fn new_goto_resets_stale_terminal_and_bumps_goal_id() {
        let cs = CommandState::default();

        // Goal A accepted, then the walker drives it to `arrived` — mutated IN PLACE exactly as
        // `Walker::set_nav_state_because` does, so `goal_id` is preserved across the transition.
        let a = cs.request_goto((10.0, 20.0, 3.0));
        {
            let mut s = cs.nav.nav_state.lock().unwrap();
            s.state = "arrived".into();
            assert_eq!(s.goal_id, a, "the terminal state belongs to goal A's id");
        }

        // Goal B: a fresh goto (the canonical agent loop is POST /goto then a nav_state read).
        let b = cs.request_goto((99.0, 88.0, 5.0));
        assert!(b > a, "each accepted goto bumps the goal id monotonically: {b} should exceed {a}");

        let s = cs.nav.nav_state.lock().unwrap();
        assert_eq!(s.goal_id, b, "the read must carry the NEW goal's id, not A's");
        assert_ne!(s.state, "arrived",
            "#349: a read after a new goto must NOT report the PREVIOUS goto's terminal `arrived`");
        assert_eq!(s.state, "pending",
            "the new goal is in-progress (`pending`) until the walker ticks — not terminal");
        assert_eq!(s.goal, Some([99.0, 88.0, 5.0]),
            "the read must carry the NEW goal's coords so a caller can correlate the state to it");
    }

    /// #349, the `/stop` shape: after a goal reaches `arrived`, a `/stop` must reset the state to a
    /// fresh `idle` identity so a read can't return the cancelled goal's terminal `arrived`.
    #[test]
    fn stop_after_arrived_resets_to_fresh_idle() {
        let cs = CommandState::default();
        let a = cs.request_goto((1.0, 2.0, 3.0));
        cs.nav.nav_state.lock().unwrap().state = "arrived".into();

        let b = cs.request_stop();
        assert!(b > a, "stop bumps the goal id");
        let s = cs.nav.nav_state.lock().unwrap();
        assert_eq!(s.state, "idle", "stop resets to idle, not the stale `arrived`");
        assert_eq!(s.goal_id, b);
        assert_eq!(s.goal, None, "a stop has no goal");
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
