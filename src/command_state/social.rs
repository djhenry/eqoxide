//! Social command verbs — the `/who all` and friends-presence polls.
//!
//! Domain: `/v1/social/friends` (presence poll half) + `/v1/observe/who`. Both slots are
//! "command with result" — an `Option<oneshot::Sender<Vec<WhoEntry>>>`, mirroring `FrameReq`: the
//! caller builds the channel, hands the sender in via `request_*`, and `ActionLoop`'s
//! `drain_who_friends` takes it, sends the wire request, and fires it later when the reply
//! arrives. This is the SAME oneshot relay the raw slot did — not the `request_*_await` variant
//! the `mod.rs` doc reserves for A3, which would build the channel and await it internally.
//! `self.social.friends_list` (the client-local friends roster) is deliberately NOT exposed here —
//! it's a persistent view-owned list, structurally a roster like `GroupSlots::group`, not a
//! fire-once command (see `mod.rs`).

use super::CommandState;
use crate::game_state::WhoEntry;
use tokio::sync::oneshot::Sender;

impl CommandState {
    // ── request_* : the VIEW (HTTP handlers) makes these writes ───────────────────────────────────

    /// Register a `/who all` request (GET /v1/observe/who). Overwrites (and thereby drops) any
    /// prior undrained sender — a newer request supersedes an in-flight one.
    pub fn request_who(&self, tx: Sender<Vec<WhoEntry>>) {
        *self.social.who_req.lock().unwrap() = Some(tx);
    }

    /// Register a friends-presence poll (GET /v1/social/friends). Overwrites (and thereby drops)
    /// any prior undrained sender.
    pub fn request_friends_who(&self, tx: Sender<Vec<WhoEntry>>) {
        *self.social.friends_req.lock().unwrap() = Some(tx);
    }

    // ── take_* : the MODEL (`ActionLoop::tick`'s `drain_who_friends`) drains these once per tick ──

    /// Drain a pending `/who all` request.
    pub fn take_who_req(&self) -> Option<Sender<Vec<WhoEntry>>> {
        self.social.who_req.lock().unwrap().take()
    }

    /// Drain a pending friends-presence poll request.
    pub fn take_friends_req(&self) -> Option<Sender<Vec<WhoEntry>>> {
        self.social.friends_req.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;
    use tokio::sync::oneshot;

    /// A `request_*` write and the matching `take_*` drain touch the SAME slot, the drain removes
    /// it, and a second drain sees nothing — proven for both social oneshot slots. The relayed
    /// sender itself still works end-to-end (fire it after the drain, receive on the paired rx).
    #[test]
    fn request_then_take_round_trips_each_social_slot() {
        let cs = CommandState::default();

        let (tx, mut rx) = oneshot::channel();
        cs.request_who(tx);
        let taken = cs.take_who_req().expect("who request queued");
        assert!(cs.take_who_req().is_none(), "a drained who request must not re-fire");
        taken.send(vec![]).unwrap();
        assert_eq!(rx.try_recv().unwrap(), vec![]);

        let (tx2, mut rx2) = oneshot::channel();
        cs.request_friends_who(tx2);
        let taken2 = cs.take_friends_req().expect("friends request queued");
        assert!(cs.take_friends_req().is_none());
        taken2.send(vec![]).unwrap();
        assert_eq!(rx2.try_recv().unwrap(), vec![]);
    }

    /// A slot with nothing queued drains to `None` — no phantom command.
    #[test]
    fn take_on_empty_slot_is_none() {
        let cs = CommandState::default();
        assert!(cs.take_who_req().is_none());
        assert!(cs.take_friends_req().is_none());
    }
}
