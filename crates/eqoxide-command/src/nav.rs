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

/// `nav_reason` published when a drained `/v1/move/zone_cross` reached the end of its handling
/// WITHOUT anything observable being written for it (#725). It means: *the client consumed your
/// request and produced no outcome* — a client bug, reported rather than hidden. See
/// [`ZoneCrossTicket`], which is the only thing that can publish it.
pub const NAV_REASON_ZONE_CROSS_UNHANDLED: &str = "zone_cross_dropped_unhandled";

/// `nav_reason` on the `idle` that [`CommandState::request_stop`] publishes (#725 review, B1) —
/// "idle because YOU cancelled it", not "idle, nothing was ever asked for".
pub const NAV_REASON_STOPPED: &str = "stopped";

/// `nav_reason` on the `idle` that [`CommandState::request_cancel_goto`] publishes (#725 review,
/// B1) — the narrower cancel taken when manual movement, the HTTP manual-move escape hatch, or the
/// auto-melee-engage override takes over steering. Distinct from [`NAV_REASON_STOPPED`] because the
/// caller did NOT ask to stop: something else took the wheel, and an agent polling its own `/goto`
/// needs to be able to tell those apart.
pub const NAV_REASON_GOTO_CANCELLED: &str = "goto_superseded";

/// A `/v1/move/zone_cross` request that has been DRAINED out of its one-shot slot.
///
/// # Why this is a type and not a `u16` (#725)
///
/// The bug: `ActionLoop::drain_zone_cross` took the request out of the slot and then hit a branch
/// whose entire body was one `tracing::info!`. The request no longer existed anywhere, nothing an
/// agent can read was written, and `nav_state` sat at the `pending` the accept had stamped —
/// measured, unchanged, for 45 s / 48 s / 75 s across three crossings, with `nav_reason`,
/// `nav_goal` and `crossing_pending_ms` all `null` and no message in the log. `pending` is
/// documented as "your goal is genuinely in flight", so the agent was instructed to believe it.
///
/// **A drained request that publishes nothing is the general shape of that bug**; the distance gate
/// that triggered it was one instance. This ticket closes the shape by construction instead of by
/// review: it records `(goal_id, state, reason)` as they stood the instant the request left the
/// slot, and on `Drop` compares them with the live values. Every way of answering a cross moves at
/// least one of the three:
///
/// * [`CommandState::request_goto`] / [`CommandState::request_zone_cross`] (the load-window
///   re-queue) bump `goal_id` through `stamp_new_goal`;
/// * `Walker::set_nav_state_because` changes `state` and/or `reason`.
///
/// If NONE of them moved, nothing was published for this request and the ticket publishes the
/// honest terminal state itself: `idle` + [`NAV_REASON_ZONE_CROSS_UNHANDLED`]. **No call site has
/// to remember to set a flag** — forgetting is precisely the case this handles, and a flag someone
/// must remember would have exactly the failure mode of the branch that started this.
///
/// It deliberately does NOT overwrite a state that DID move. A concurrent `POST /v1/move/goto` on
/// the HTTP thread bumps `goal_id` mid-drain; stamping over that would be #349 (a read seeing an
/// older goal's outcome under a newer id) in reverse.
///
/// Being `#[must_use]`, dropping it on the floor is at least a warning at the call site; being a
/// `Drop` backstop, doing so is still *honest* rather than silent.
#[must_use = "a drained zone_cross must be answered; dropping the ticket publishes the \
              zone_cross_dropped_unhandled fallback"]
pub struct ZoneCrossTicket {
    zone_id:   u16,
    nav_state: eqoxide_ipc::NavStateShared,
    /// `(goal_id, state, reason)` at the instant the request left the slot.
    at_drain:  (u64, String, Option<String>),
}

impl ZoneCrossTicket {
    /// The requested destination: `0` = "the nearest zone line", otherwise a specific zone id
    /// (pre-validated as advertised by the HTTP handler).
    pub fn zone_id(&self) -> u16 { self.zone_id }
}

impl Drop for ZoneCrossTicket {
    fn drop(&mut self) {
        // Recover a poisoned guard rather than `unwrap()`: this runs in a `Drop`, and panicking
        // while another panic unwinds aborts the process. A poisoned nav_state still holds a
        // readable `NavStatus`, and publishing the honest fallback into it is strictly better than
        // taking the client down.
        let mut s = match self.nav_state.lock() {
            Ok(g)  => g,
            Err(e) => e.into_inner(),
        };
        let moved = (s.goal_id, s.state.as_str(), s.reason.as_deref())
            != (self.at_drain.0, self.at_drain.1.as_str(), self.at_drain.2.as_deref());
        if moved {
            return; // something WAS published for this request (or a newer goal superseded it)
        }
        s.state  = "idle".to_string();
        s.reason = Some(NAV_REASON_ZONE_CROSS_UNHANDLED.to_string());
        // The same per-instance retirement `Walker::set_nav_state_because` does: a transition must
        // not leave the previous route's facts standing beside the new state (#343 discipline).
        s.goal            = None;
        s.blocked_goal    = None;
        s.blocked_frontier = None;
        s.tier            = None;
        tracing::warn!(
            "zone_cross: request for zone_id={} was drained without publishing any outcome — \
             publishing idle/{NAV_REASON_ZONE_CROSS_UNHANDLED} (this is a client bug, #725)",
            self.zone_id);
    }
}

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
    ///
    /// `reason` is the `nav_reason` to publish alongside `new_state` (#725 review, B1). It is
    /// `None` for the in-progress stamps — a `pending` needs no explanation, the request itself is
    /// the explanation — and `Some(..)` for every `idle` stamp, because a bare `idle` is
    /// indistinguishable from "nothing was ever requested". After #725 that distinction is load
    /// bearing: on `idle`, `nav_reason: null` means exactly one thing, that no nav request has been
    /// made since the client started.
    fn stamp_new_goal(&self, new_state: &str, reason: Option<&str>, goal: Option<(f32, f32, f32)>) -> u64 {
        let mut s = self.nav.nav_state.lock().unwrap();
        s.goal_id += 1;
        s.state = new_state.to_string();
        s.reason = reason.map(str::to_string);
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
        self.stamp_new_goal("pending", None, Some(target))
    }

    /// Walk to a named entity's current position and KEEP CHASING it (POST /v1/move/follow). `key`
    /// is the `entity_positions` key the walker re-resolves each tick; `pos` seeds the initial goal.
    /// Returns the new `goal_id` (#349); state resets to `pending`.
    pub fn request_follow(&self, key: String, pos: (f32, f32, f32)) -> u64 {
        *self.nav.goto_target.lock().unwrap() = Some(pos);
        *self.nav.goto_entity.lock().unwrap() = Some(key);
        self.stamp_new_goal("pending", None, Some(pos))
    }

    /// Cancel any active goto/follow (POST /v1/move/stop). Clears the goto/follow slots AND the
    /// one-shot `zone_cross` slot — idempotent. Returns the new `goal_id` (#349): the state resets to
    /// `idle` under a fresh identity, so a read right after `/stop` can never return the cancelled
    /// goal's terminal `arrived`/`no_path`.
    ///
    /// **`zone_cross` must be cleared here too (#600 review round 3).** A `/zone_cross` issued while
    /// the zone's assets are still loading is RE-QUEUED by `drain_zone_cross` (so it resolves once
    /// they land). If `/stop` did not clear that slot, the cross would be UNCANCELLABLE — `/stop`
    /// would silently no-op and the client would cross anyway when the load finished (a confident
    /// ACTION lie: the agent asked to stop, the client crossed regardless), and the `zone_loading`
    /// state would never retire because `resolve_goal`'s `!zone_cross_pending` guard stayed false.
    pub fn request_stop(&self) -> u64 {
        *self.nav.goto_target.lock().unwrap() = None;
        *self.nav.goto_entity.lock().unwrap() = None;
        *self.nav.zone_cross.lock().unwrap() = None;
        // SAY WHY (#725 review, B1): `idle` with no reason is the boot state. An agent that polls
        // after `/stop` must be able to tell "your stop landed" from "I have no record of anything".
        self.stamp_new_goal("idle", Some(NAV_REASON_STOPPED), None)
    }

    /// Cancel an in-progress goto WITHOUT touching `goto_entity` — used where manual movement
    /// (keyboard WASD, the HTTP manual-move escape hatch, or an auto-melee-engage override) needs to
    /// take over steering this frame/tick but isn't itself a `/stop`. Narrower than
    /// [`Self::request_stop`] on purpose; preserves the exact pre-migration call sites' behavior
    /// for `goto_entity` (left alone here — the walker's own `drive_chase` clears it on its next
    /// tick once it observes `goto_target` is `None`, treating the goto as "cancelled elsewhere").
    ///
    /// #497: still stamps a fresh idle identity via [`Self::stamp_new_goal`], exactly like
    /// [`Self::request_stop`], so `nav_goal` is honestly cleared to `None` the instant the cancel is
    /// accepted — not left holding the cancelled goal's stale coordinates until the walker's next
    /// tick relabels `state` (which never touches `goal`; see `Walker::resolve_goal`). Without this,
    /// a read right after cancel could report `nav_state: idle` alongside a non-null `nav_goal`,
    /// contradicting the documented contract (`docs/http-api.md`). Returns the new `goal_id` (#349)
    /// for the same reason `request_stop` does: so a caller can never read the cancelled goal's
    /// terminal `arrived`/`no_path` under a fresh identity.
    pub fn request_cancel_goto(&self) -> u64 {
        *self.nav.goto_target.lock().unwrap() = None;
        // SAY WHY (#725 review, B1), and say something DIFFERENT from `/stop`: from the agent's side
        // these are not the same event. `stopped` = you asked. `goto_superseded` = you did not, and
        // steering was taken over by manual movement or the melee-engage override.
        self.stamp_new_goal("idle", Some(NAV_REASON_GOTO_CANCELLED), None)
    }

    /// Queue a zone-line crossing (POST /v1/move/zone_cross). `0` = nearest line, `Some(id)` = a
    /// specific destination zone id (pre-validated as reachable by the HTTP handler). Returns the new
    /// `goal_id` (#349): the state resets to `pending` synchronously, so a read right after the 200
    /// can't return the previous nav's terminal state before the walker drains this request and
    /// resolves the concrete zone-line goal (which re-stamps a fresh id via `request_goto`). `goal`
    /// is left `None` until that resolution — the concrete destination isn't known yet.
    pub fn request_zone_cross(&self, zone_id: u16) -> u64 {
        *self.nav.zone_cross.lock().unwrap() = Some(zone_id);
        self.stamp_new_goal("pending", None, None)
    }

    // ── take_* : the MODEL (`ActionLoop::drain_zone_cross`) drains this once per tick ──────────────

    /// Drain a pending zone-cross request. `zone_cross` is the one genuinely one-shot nav slot.
    ///
    /// Returns a [`ZoneCrossTicket`], not a bare `u16` (#725): once the request has left the slot it
    /// exists nowhere else, so the drainer OWES the agent an observable outcome. The ticket carries
    /// that obligation and, if the drainer somehow returns without meeting it, publishes the honest
    /// fallback itself. See its doc comment for the full argument.
    pub fn take_zone_cross(&self) -> Option<ZoneCrossTicket> {
        // Take the request FIRST (and release that lock) so the two nav locks are never nested.
        let zone_id = self.nav.zone_cross.lock().unwrap().take()?;
        let at_drain = {
            let s = self.nav.nav_state.lock().unwrap();
            (s.goal_id, s.state.clone(), s.reason.clone())
        };
        Some(ZoneCrossTicket { zone_id, nav_state: self.nav.nav_state.clone(), at_drain })
    }

    /// Is a zone-cross request queued? A **peek** — it does not drain, so it cannot incur the
    /// [`ZoneCrossTicket`] obligation. Tests use it to ask "was the one-shot re-queued?" without
    /// consuming the request they are asserting about.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn peek_zone_cross(&self) -> Option<u16> {
        *self.nav.zone_cross.lock().unwrap()
    }

    /// Peek the active `/goto` destination without draining it (the walker holds it continuously).
    /// A read-only accessor so callers outside this module — and tests — can observe whether a nav
    /// destination is set, without reaching into the private `nav` slots. `None` = the walker has no
    /// active goto (idle/stopped).
    ///
    /// Gated on `test-fixtures` (not bare `#[cfg(test)]`) and `pub` (not `pub(crate)`) — since
    /// #544 Step 2g this method lives in the separate `eqoxide-command` crate, and the app crate's
    /// own test code (`src/model.rs`'s `MockModel`, `src/eq_net/action_loop.rs`'s nav tests) calls
    /// it across that crate boundary. A bare `#[cfg(test)]` here would only apply when
    /// `eqoxide-command` itself is compiled as the crate under test, which never happens for a
    /// downstream dependent's `cargo test` — mirrors `eqoxide-core`'s `RegionMap::flat_below`-style
    /// fixtures. Off (and `pub(crate)`-equivalent in effect) in normal builds — the release binary
    /// never exposes this accessor.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn goto_target(&self) -> Option<(f32, f32, f32)> {
        *self.nav.goto_target.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CommandState, NAV_REASON_GOTO_CANCELLED, NAV_REASON_STOPPED, NAV_REASON_ZONE_CROSS_UNHANDLED,
    };

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

    /// #497: `request_cancel_goto` must be as honest as `request_stop` about `nav_goal`. Goto,
    /// drive it to `arrived` in place (exactly as the walker would), then cancel — a subsequent
    /// `nav_state` read must report `idle` with `goal: None`, matching `docs/http-api.md`'s
    /// "`nav_goal` … `null` for `idle`/`stop`" contract. Before the fix, `request_cancel_goto` only
    /// cleared `goto_target` and never touched `nav_state.goal`, so this read would see
    /// `state: "idle"` (set in-place by `Walker::resolve_goal` on its next tick) paired with the
    /// STALE, non-null coordinates from the cancelled goto.
    ///
    /// Mutation check: delete the `self.stamp_new_goal("idle", …)` call in `request_cancel_goto`
    /// (leave only the `goto_target` clear) and this test goes RED — `s.goal` is still
    /// `Some([10.0, 20.0, 3.0])` after the cancel, the exact stale-`nav_goal` bug #497 reports.
    #[test]
    fn cancel_goto_after_arrived_clears_goal_and_resets_to_idle() {
        let cs = CommandState::default();
        let a = cs.request_goto((10.0, 20.0, 3.0));
        cs.nav.nav_state.lock().unwrap().state = "arrived".into();

        let b = cs.request_cancel_goto();
        assert!(b > a, "cancel_goto bumps the goal id, same as stop");

        let s = cs.nav.nav_state.lock().unwrap();
        assert_eq!(s.state, "idle", "cancel resets to idle, not the stale `arrived`");
        assert_eq!(s.goal_id, b);
        assert_eq!(s.goal, None,
            "nav_goal must be null after a cancel — a stale non-null goal alongside idle is #497");
    }

    /// **#725 review, B1: a bare `idle` is not an answer.** `docs/http-api.md` tells an agent that
    /// on `idle` the `nav_reason` says how it got there. That was false: `/stop`, the manual-move
    /// cancel and the *success* path of a zone crossing all published `idle` with `nav_reason:
    /// null`, which is byte-identical to "no request has ever been made" — so the endpoint's
    /// success looked exactly like its failure.
    ///
    /// This pins the half of that contract that lives in `CommandState`. The walker's half (the
    /// retirement reasons and `zoned`) is pinned in `eqoxide-nav`; between them, `idle` +
    /// `nav_reason: null` is reachable only as the boot default.
    ///
    /// **Mutation check:** pass `None` as the reason at either call site → the corresponding case
    /// goes RED. Give both the SAME reason → the last assertion goes RED, which is the point of
    /// having two: "you asked me to stop" and "something took the wheel from you" are different
    /// facts about a goal an agent issued, and collapsing them re-hides one of them.
    #[test]
    fn every_idle_command_state_publishes_names_itself_725() {
        for (label, act, want) in [
            ("stop",        Box::new(|cs: &CommandState| { cs.request_stop(); })        as Box<dyn Fn(&CommandState)>,
             NAV_REASON_STOPPED),
            ("cancel_goto", Box::new(|cs: &CommandState| { cs.request_cancel_goto(); }) as Box<dyn Fn(&CommandState)>,
             NAV_REASON_GOTO_CANCELLED),
        ] {
            let cs = CommandState::default();
            assert_eq!(cs.nav.nav_state.lock().unwrap().reason, None,
                "{label}: premise — at boot `idle` genuinely has no reason, which is the ONE case \
                 a null reason is allowed to mean");
            cs.request_goto((10.0, 20.0, 3.0));
            act(&cs);

            let s = cs.nav.nav_state.lock().unwrap();
            assert_eq!(s.state, "idle", "{label}: still idle");
            assert_eq!(s.reason.as_deref(), Some(want),
                "{label}: #725 B1 — an `idle` with no reason is indistinguishable from a client \
                 that was never asked for anything, so every path to `idle` must name itself");
        }
        assert_ne!(NAV_REASON_STOPPED, NAV_REASON_GOTO_CANCELLED,
            "an explicit /stop and a cancel the caller did not ask for must be distinguishable");
    }

    #[test]
    fn request_then_take_zone_cross_round_trips() {
        let cs = CommandState::default();
        let taken = |cs: &CommandState| cs.take_zone_cross().map(|t| t.zone_id());
        assert_eq!(taken(&cs), None);
        cs.request_zone_cross(0);
        assert_eq!(taken(&cs), Some(0));
        assert_eq!(taken(&cs), None, "a drained zone_cross must not re-fire");

        cs.request_zone_cross(42);
        assert_eq!(taken(&cs), Some(42));
    }

    /// **#600 review round 3: `/stop` must CANCEL a queued `/zone_cross`.** A cross issued while the
    /// zone's assets are still loading is re-queued by `drain_zone_cross` (so it resolves once they
    /// land); if `request_stop` did not clear `nav.zone_cross`, the cross would be uncancellable and
    /// fire anyway after the load — a confident ACTION lie (the agent asked to stop, the client
    /// crossed regardless).
    ///
    /// Mutation check: delete the `*self.nav.zone_cross.lock().unwrap() = None;` line from
    /// `request_stop` → `take_zone_cross()` still returns `Some(30)` here and this goes RED.
    #[test]
    fn request_stop_cancels_a_queued_zone_cross() {
        let cs = CommandState::default();
        cs.request_zone_cross(30);
        cs.request_stop();
        assert_eq!(cs.peek_zone_cross(), None,
            "#600: /stop must clear the queued cross so it cannot fire after the load completes");
    }

    // ───────────────────────── #725: the drained-request obligation ─────────────────────────

    /// **The #725 honesty backstop: a drained `/zone_cross` that publishes NOTHING must still
    /// leave a state an agent can act on.**
    ///
    /// This is the exact shape of the shipped bug, reduced to its essentials: the accept stamps
    /// `pending`, the drain takes the one-shot request out of its slot, and then — as the old
    /// "already on the line" branch did — nothing observable is written. The request now exists
    /// nowhere, so nothing downstream can ever retire the state; measured live, `pending` /
    /// `nav_reason: null` persisted for 45 s, 48 s and 75 s, which the docs told the agent meant
    /// "genuinely in flight".
    ///
    /// After the fix the ticket notices that neither `goal_id` nor `state`/`reason` moved while it
    /// was held, and publishes the honest `idle` + `zone_cross_dropped_unhandled` itself.
    ///
    /// **Mutation check:** delete the `impl Drop for ZoneCrossTicket` block → the state is still
    /// `pending` with `reason: None` and this goes RED on the very first assertion.
    #[test]
    fn a_drained_zone_cross_that_publishes_nothing_still_retires_the_pending_state() {
        let cs = CommandState::default();
        let accepted = cs.request_zone_cross(30);
        assert_eq!(cs.nav.nav_state.lock().unwrap().state, "pending", "the accept stamps pending");

        // Drain it and answer NOTHING — the old shortcut branch, verbatim in behaviour.
        drop(cs.take_zone_cross().expect("the request was queued"));

        let s = cs.nav.nav_state.lock().unwrap();
        assert_ne!(s.state, "pending",
            "#725: a consumed request must not leave `pending` standing — that is the state the \
             agent is told means 'in flight', and nothing can ever retire it once the slot is empty");
        assert_eq!(s.state, "idle");
        assert_eq!(s.reason.as_deref(), Some(NAV_REASON_ZONE_CROSS_UNHANDLED),
            "the retirement must SAY WHY — a bare `idle` is indistinguishable from 'ready for work'");
        assert_eq!(s.goal_id, accepted,
            "the backstop answers THIS goal; it must not bump the id the POST returned");
    }

    /// The backstop must be inert when the drain DID answer — otherwise it would overwrite real
    /// outcomes, which is the same class of lie pointing the other way.
    ///
    /// Covers all three ways an answer is published: a fresh goto (bumps `goal_id`), a state change
    /// (`zone_loading`), and a reason-only change under the same state word.
    ///
    /// **Mutation check:** delete the `if moved { return; }` guard in `ZoneCrossTicket::drop` → every
    /// case here is stomped to `idle`/`zone_cross_dropped_unhandled` and all three go RED.
    #[test]
    fn the_backstop_never_overwrites_an_answer_the_drain_did_publish() {
        // (a) resolved into a concrete walk — `request_goto` bumps the goal id.
        let cs = CommandState::default();
        cs.request_zone_cross(30);
        let t = cs.take_zone_cross().unwrap();
        let walked = cs.request_goto((10.0, 20.0, 0.0));
        drop(t);
        {
            let s = cs.nav.nav_state.lock().unwrap();
            assert_eq!((s.state.as_str(), s.goal_id), ("pending", walked),
                "a resolved cross's own fresh goal must survive the ticket's drop");
            assert_eq!(s.goal, Some([10.0, 20.0, 0.0]));
        }

        // (b) refused with a different state word (the #600 load-window `zone_loading`).
        let cs = CommandState::default();
        cs.request_zone_cross(30);
        let t = cs.take_zone_cross().unwrap();
        { let mut s = cs.nav.nav_state.lock().unwrap(); s.state = "zone_loading".into(); }
        drop(t);
        assert_eq!(cs.nav.nav_state.lock().unwrap().state, "zone_loading");

        // (c) answered with a REASON under the same state word — the subtlest case, and why the
        // ticket compares `reason` and not just `(goal_id, state)`.
        let cs = CommandState::default();
        cs.request_zone_cross(30);
        let t = cs.take_zone_cross().unwrap();
        { let mut s = cs.nav.nav_state.lock().unwrap(); s.reason = Some("waiting_for_zone_points".into()); }
        drop(t);
        let s = cs.nav.nav_state.lock().unwrap();
        assert_eq!((s.state.as_str(), s.reason.as_deref()), ("pending", Some("waiting_for_zone_points")));
    }

    /// A concurrent `POST /v1/move/goto` accepted while the cross is being drained must WIN. The
    /// ticket's backstop is for its own unanswered request, and stamping over a newer goal would be
    /// #349 in reverse — a read seeing an older request's outcome under the newer goal's id.
    #[test]
    fn the_backstop_yields_to_a_newer_goal_accepted_mid_drain() {
        let cs = CommandState::default();
        cs.request_zone_cross(30);
        let t = cs.take_zone_cross().unwrap();
        let newer = cs.request_goto((5.0, 6.0, 7.0)); // the HTTP thread, mid-drain
        drop(t);
        let s = cs.nav.nav_state.lock().unwrap();
        assert_eq!(s.goal_id, newer);
        assert_eq!(s.goal, Some([5.0, 6.0, 7.0]),
            "the newer goal's coordinates must not be cleared by the older request's backstop");
        assert_eq!(s.reason, None);
    }
}
