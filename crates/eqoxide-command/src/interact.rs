//! Interact command verbs — Wave-2 migration of the `combat.rs` pattern (see `mod.rs` "HOW TO
//! MIGRATE A DOMAIN").
//!
//! Domain: `/v1/interact/*` — hail, say, loot, give (turn-in), door click, sit/stand, dialogue
//! click, and read (book/note). Every method is a thin typed read/write of a slot in
//! `self.interact`; validation and packet-building stay where they were (the HTTP handlers, the UI
//! windows, and `ActionLoop`'s `drain_chat`/`drain_loot`/`drain_doors`/`drain_sit`/
//! `drain_read_book`/`tick_give`). No behavior change — just one typed surface.
//!
//! `self.interact.doors_shared` (GET /v1/observe/doors) and `self.interact.dialogue` (GET
//! /v1/observe/dialogue) are read-path/published snapshots, not commands — deliberately NOT
//! exposed here (see `mod.rs`).

use super::CommandState;
use eqoxide_core::game_state::DialogueChoice;
use eqoxide_ipc::{CommandResult, GiveOk};
use tokio::sync::oneshot;

impl CommandState {
    // ── request_* : the VIEW (UI click-handlers + HTTP handlers) makes these writes ──────────────

    /// Hail an NPC — "Hail, `<name>`" (POST /v1/interact/hail, the Actions window's Hail button,
    /// the NPC-dialogue window's re-hail). `spawn_id`, when known, is targeted first so the
    /// server's EVENT_SAY fires (#130).
    pub fn request_hail(&self, name: String, spawn_id: Option<u32>) {
        *self.interact.hail.lock().unwrap() = Some((name, spawn_id));
    }

    /// Say arbitrary Say-channel text (POST /v1/interact/say, the chat window's say box, a
    /// dialogue keyword follow-up click).
    pub fn request_say(&self, text: String) {
        *self.interact.say.lock().unwrap() = Some(text);
    }

    /// Loot a corpse by spawn id (POST /v1/interact/loot, the Loot window). The drain pushes it
    /// onto the existing auto-loot queue.
    pub fn request_loot(&self, corpse_id: u32) {
        *self.interact.loot.lock().unwrap() = Some(corpse_id);
    }

    /// Give (quest turn-in) inventory slot `from_slot` to NPC `npc_id` — FIRE-AND-FORGET (the UI
    /// turn-in path). The drain runs the multi-tick trade-window state machine. HTTP's POST
    /// /v1/interact/give uses the awaited [`request_give_await`](Self::request_give_await) instead.
    pub fn request_give(&self, npc_id: u32, from_slot: u32) {
        *self.interact.give.lock().unwrap() = Some((npc_id, from_slot));
    }

    /// Command-with-result give (A3 Migration 2, #448): queue the SAME turn-in as `request_give` but
    /// hand in a `oneshot::Sender<CommandResult<GiveOk>>` the net thread fulfils with the TRUE
    /// outcome. POST /v1/interact/give awaits the matching receiver under a timeout so it reports the
    /// real result (OP_FinishTrade → 200, no-ack/no-finish → 202, a give already in flight → 409)
    /// instead of a premature queued-action 200. Writes the sibling `give_await` slot — the
    /// fire-and-forget `give` slot (UI path) is left untouched. See `crate::command_state::result`.
    pub fn request_give_await(
        &self,
        npc_id: u32,
        from_slot: u32,
        tx: oneshot::Sender<CommandResult<GiveOk>>,
    ) {
        *self.interact.give_await.lock().unwrap() = Some((npc_id, from_slot, tx));
    }

    /// Click a door by id (POST /v1/interact/click_door, or a human click in the 3D view). The
    /// drain sends OP_ClickDoor.
    pub fn request_door_click(&self, door_id: u8) {
        *self.interact.door_click.lock().unwrap() = Some(door_id);
    }

    /// Posture: `Some(true)` = sit, `Some(false)` = stand (POST /v1/interact/{sit,stand}, the
    /// Actions window's sit/stand toggle).
    pub fn request_sit(&self, sit: bool) {
        *self.interact.sit.lock().unwrap() = Some(sit);
    }

    /// Run/walk toggle (#625): `Some(true)` = run, `Some(false)` = walk (POST /v1/interact/{run,walk},
    /// the Actions window's Run/Walk button). The drain sends `OP_SetRunMode` (0x009f) and switches
    /// the local movement speed to match.
    pub fn request_run_mode(&self, run: bool) {
        *self.interact.run_mode.lock().unwrap() = Some(run);
    }

    /// Click one of the current NPC-dialogue saylink choices (POST /v1/interact/dialogue, the
    /// NPC-dialogue window). The drain sends OP_ItemLinkClick.
    pub fn request_dialogue_click(&self, choice: DialogueChoice) {
        *self.interact.dialogue_click.lock().unwrap() = Some(choice);
    }

    /// Read a book/note at inventory wire slot `slot` (POST /v1/interact/read). The drain sends
    /// OP_ReadBook. (#288)
    pub fn request_read_book(&self, slot: i32) {
        *self.interact.read_book.lock().unwrap() = Some(slot);
    }

    // ── take_* : the MODEL (`ActionLoop`'s drains) drains these once per tick ─────────────────────

    /// Drain a pending hail request as `(display_name, spawn_id)`.
    pub fn take_hail(&self) -> Option<(String, Option<u32>)> {
        self.interact.hail.lock().unwrap().take()
    }

    /// Drain pending Say-channel text.
    pub fn take_say(&self) -> Option<String> {
        self.interact.say.lock().unwrap().take()
    }

    /// Drain a pending loot request (corpse spawn id).
    pub fn take_loot(&self) -> Option<u32> {
        self.interact.loot.lock().unwrap().take()
    }

    /// Drain a pending give request as `(npc_id, from_slot)`.
    pub fn take_give(&self) -> Option<(u32, u32)> {
        self.interact.give.lock().unwrap().take()
    }

    /// Drain a pending awaited-give request as `(npc_id, from_slot, Sender)` (A3 Migration 2, #448).
    /// `ActionLoop::tick_give` takes this, parks the `Sender` inside its `GiveState`, and runs the
    /// turn-in until the resolving packet lands. Sibling of `take_give`.
    pub fn take_give_await(
        &self,
    ) -> Option<(u32, u32, oneshot::Sender<CommandResult<GiveOk>>)> {
        self.interact.give_await.lock().unwrap().take()
    }

    /// Drain a pending door-click request (door id).
    pub fn take_door_click(&self) -> Option<u8> {
        self.interact.door_click.lock().unwrap().take()
    }

    /// Drain a pending sit/stand request.
    pub fn take_sit(&self) -> Option<bool> {
        self.interact.sit.lock().unwrap().take()
    }

    /// Drain a pending run/walk toggle request (#625).
    pub fn take_run_mode(&self) -> Option<bool> {
        self.interact.run_mode.lock().unwrap().take()
    }

    /// Drain a pending dialogue-click request.
    pub fn take_dialogue_click(&self) -> Option<DialogueChoice> {
        self.interact.dialogue_click.lock().unwrap().take()
    }

    /// Drain a pending read-book request (inventory wire slot).
    pub fn take_read_book(&self) -> Option<i32> {
        self.interact.read_book.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;
    use eqoxide_core::game_state::DialogueChoice;

    /// The core invariant of the facade: a `request_*` write and the matching `take_*` drain touch
    /// the SAME slot, the drain removes it (so a stale command can't fire twice), and a second
    /// drain sees nothing. Proven for every interact verb.
    #[test]
    fn request_then_take_round_trips_each_interact_slot() {
        let cs = CommandState::default();

        cs.request_hail("Guard Phaeton".into(), Some(5));
        assert_eq!(cs.take_hail(), Some(("Guard Phaeton".into(), Some(5))));
        assert_eq!(cs.take_hail(), None, "a drained hail must not re-fire");

        cs.request_say("shipment".into());
        assert_eq!(cs.take_say(), Some("shipment".into()));
        assert_eq!(cs.take_say(), None);

        cs.request_loot(9);
        assert_eq!(cs.take_loot(), Some(9));
        assert_eq!(cs.take_loot(), None);

        cs.request_give(11, 23);
        assert_eq!(cs.take_give(), Some((11, 23)));
        assert_eq!(cs.take_give(), None);

        cs.request_door_click(3);
        assert_eq!(cs.take_door_click(), Some(3));
        assert_eq!(cs.take_door_click(), None);

        cs.request_sit(true);
        assert_eq!(cs.take_sit(), Some(true));
        assert_eq!(cs.take_sit(), None);

        cs.request_run_mode(false);
        assert_eq!(cs.take_run_mode(), Some(false));
        assert_eq!(cs.take_run_mode(), None, "a drained run-mode toggle must not re-fire");

        let choice = DialogueChoice { text: "shipment".into(), item_id: 0xFFFFF, ..Default::default() };
        cs.request_dialogue_click(choice.clone());
        assert_eq!(cs.take_dialogue_click(), Some(choice));
        assert!(cs.take_dialogue_click().is_none());

        cs.request_read_book(23);
        assert_eq!(cs.take_read_book(), Some(23));
        assert_eq!(cs.take_read_book(), None);
    }

    /// A slot with nothing queued drains to `None` — no phantom command.
    #[test]
    fn take_on_empty_slot_is_none() {
        let cs = CommandState::default();
        assert_eq!(cs.take_hail(), None);
        assert_eq!(cs.take_say(), None);
        assert_eq!(cs.take_loot(), None);
        assert_eq!(cs.take_give(), None);
        assert_eq!(cs.take_door_click(), None);
        assert_eq!(cs.take_sit(), None);
        assert!(cs.take_dialogue_click().is_none());
        assert_eq!(cs.take_read_book(), None);
        assert!(cs.take_give_await().is_none());
    }

    /// A3 Migration 2 (#448): the awaited-give slot round-trips the `(npc_id, from_slot, Sender)`
    /// tuple, the drain removes it, and the drained `Sender` still reaches its receiver — proving
    /// `give_await` is a genuine sibling of the fire-and-forget `give` slot and does NOT disturb it.
    #[tokio::test]
    async fn request_give_await_round_trips_the_sender_and_leaves_give_untouched() {
        use super::{CommandResult, GiveOk};
        let cs = CommandState::default();
        let (tx, rx) = tokio::sync::oneshot::channel::<CommandResult<GiveOk>>();

        cs.request_give_await(11, 23, tx);
        // The fire-and-forget UI slot is a genuinely separate cell — an awaited give must not queue it.
        assert_eq!(cs.take_give(), None,
            "the awaited give must not leak into the fire-and-forget UI slot");

        let (npc_id, slot, drained_tx) = cs.take_give_await().expect("awaited give must drain");
        assert_eq!((npc_id, slot), (11, 23));
        assert!(cs.take_give_await().is_none(), "a drained awaited give must not re-fire");

        drained_tx.send(CommandResult::Resolved(GiveOk {
            npc_id: 11, item_name: "Bone Chips".into(),
        })).unwrap();
        assert_eq!(
            rx.await.unwrap(),
            CommandResult::Resolved(GiveOk { npc_id: 11, item_name: "Bone Chips".into() }),
        );
    }
}
