//! Camera command verbs (#459 stragglers).
//!
//! Domain: `/v1/camera/*` — specifically the manual-move/jump escape hatch, held here as the lone
//! `self.camera_manual_move` slot (the rest of `ipc::CameraSlots` is read-path / snapshot and is not
//! on `CommandState`). This command is consumed by the RENDER thread (`App::` frame update in
//! `app.rs`), not `ActionLoop` — but `App` already holds a `CommandState` handle via
//! `self.acts.command` (see `ui::Actions`), so wiring it in needed no new plumbing, just routing the
//! existing read/write through it.
//!
//! `manual_move` is CONTINUOUS state, not a one-shot command (unlike combat's slots): the render
//! loop re-reads it every frame until its `until` deadline passes, and never clears it early — a
//! stale-but-expired value is simply filtered out by the reader (`.filter(|m| now < m.until)`), not
//! taken. So `peek_manual_move` is a non-clearing read (`Option<ManualMove>` is `Copy`), matching
//! `app.rs`'s exact prior behavior (`{ *self.manual_move.lock().unwrap() }`) — a real `take_*` here
//! would wrongly consume the value on its first frame instead of driving movement for the whole
//! `duration_ms` window.

use super::CommandState;
use crate::ipc::ManualMove;

impl CommandState {
    /// Drive the controller directly, bypassing `/goto` (POST /v1/move/manual, /v1/move/jump).
    pub fn request_manual_move(&self, m: ManualMove) {
        *self.camera_manual_move.lock().unwrap() = Some(m);
    }

    /// Non-clearing read of the current manual-move request, if any (render-thread per-frame poll).
    /// Deliberately does NOT drain — see module docs for why a `take_*` would be wrong here.
    pub fn peek_manual_move(&self) -> Option<ManualMove> {
        *self.camera_manual_move.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;
    use crate::ipc::ManualMove;
    use std::time::{Duration, Instant};

    #[test]
    fn request_then_peek_manual_move_does_not_clear() {
        let cs = CommandState::default();
        assert!(cs.peek_manual_move().is_none());

        let m = ManualMove { dir: [1.0, 0.0], up: 0.0, jump: false, until: Instant::now() + Duration::from_millis(400) };
        cs.request_manual_move(m);

        let seen = cs.peek_manual_move().expect("manual move queued");
        assert_eq!(seen.dir, [1.0, 0.0]);
        // A second peek still sees it — this is continuous state, not a one-shot drain.
        assert!(cs.peek_manual_move().is_some(), "peek must not clear the slot");
    }
}
