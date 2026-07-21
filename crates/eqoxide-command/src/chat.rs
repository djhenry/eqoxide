//! Chat command verb — the outgoing chat queue.
//!
//! Domain: `/v1/chat/*` (tell/ooc/shout/group/guild — the Chat window's slash commands and the
//! matching HTTP handlers). `self.chat.chat_send` is a FIFO `Vec`, not a single `Option` slot, so
//! `request_chat_send` pushes and `take_chat_send` drains the whole queue at once (the same
//! `std::mem::take` `ActionLoop::tick`'s `drain_chat` already did) — same shape, plural cardinality.
//! `self.chat.chat_events` and `self.chat.messages` are deliberately NOT exposed here: both are
//! read-path feeds the model publishes and the view/HTTP reads (`GET /v1/events/*` and
//! `/v1/observe/messages`), not writes the view makes — see `mod.rs`.

use super::CommandState;
use eqoxide_ipc::ChatSend;

impl CommandState {
    // ── request_* : the VIEW (the Chat window + HTTP handlers) makes this write ────────────────────

    /// Queue one outgoing chat message (POST /v1/chat/{tell,ooc,shout,group,guild}, or a `/tell`
    /// `/ooc` `/shout` `/g` slash command in the Chat window).
    pub fn request_chat_send(&self, msg: ChatSend) {
        self.chat.chat_send.lock().unwrap().push(msg);
    }

    // ── take_* : the MODEL (`ActionLoop::tick`'s `drain_chat`) drains this once per tick ───────────

    /// Drain the whole outgoing-chat queue at once (FIFO order preserved).
    pub fn take_chat_send(&self) -> Vec<ChatSend> {
        std::mem::take(&mut *self.chat.chat_send.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;
    use eqoxide_ipc::ChatSend;

    /// `request_chat_send` pushes are FIFO-preserved, and `take_chat_send` drains the whole queue
    /// at once, leaving it empty for the next drain — the same `std::mem::take` behavior the raw
    /// `drain_chat` site had.
    #[test]
    fn request_then_take_round_trips_the_chat_send_queue() {
        let cs = CommandState::default();

        cs.request_chat_send(ChatSend { chan: 7, to: "Sariel".into(), text: "hi".into() });
        cs.request_chat_send(ChatSend { chan: 5, to: String::new(), text: "ooc line".into() });

        let drained = cs.take_chat_send();
        assert_eq!(drained.len(), 2);
        assert_eq!((drained[0].chan, drained[0].to.as_str(), drained[0].text.as_str()), (7, "Sariel", "hi"));
        assert_eq!((drained[1].chan, drained[1].to.as_str(), drained[1].text.as_str()), (5, "", "ooc line"));

        assert!(cs.take_chat_send().is_empty(), "a drained queue must not re-fire");
    }

    /// An empty queue drains to an empty `Vec` — no phantom command.
    #[test]
    fn take_on_empty_queue_is_empty() {
        let cs = CommandState::default();
        assert!(cs.take_chat_send().is_empty());
    }
}
