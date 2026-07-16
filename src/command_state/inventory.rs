//! Inventory command verbs — Wave-2 migration of the `combat.rs` pattern (see `mod.rs` "HOW TO
//! MIGRATE A DOMAIN").
//!
//! Domain: `/v1/inventory/*` (move/equip/unequip). The one command slot is `self.inventory.move_req`.
//! Named `request_inventory_move`/`take_inventory_move` (not the bare `move`, a Rust keyword and a
//! generic verb another domain — e.g. movement/nav — could plausibly reuse).
//!
//! `self.inventory.inventory` (the live `Vec<InvItem>` snapshot for GET /v1/observe/inventory) is a
//! read-path/published field, not a command — deliberately NOT exposed here (see `mod.rs`).

use super::CommandState;

impl CommandState {
    // ── request_* : the VIEW (UI click-handlers + HTTP handlers) makes these writes ──────────────

    /// Move/equip/unequip an item between inventory slots (POST /v1/inventory/move, the inventory
    /// window's drag-drop). `(from, to)` are Titanium wire slot ids. The drain sends OP_MoveItem.
    pub fn request_inventory_move(&self, from: u32, to: u32) {
        *self.inventory.move_req.lock().unwrap() = Some((from, to));
    }

    // ── take_* : the MODEL (`ActionLoop::drain_move_item`) drains this once per tick ─────────────

    /// Drain a pending move request as `(from_slot, to_slot)`.
    pub fn take_inventory_move(&self) -> Option<(u32, u32)> {
        self.inventory.move_req.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;

    /// The core invariant of the facade: a `request_*` write and the matching `take_*` drain touch
    /// the SAME slot, the drain removes it, and a second drain sees nothing.
    #[test]
    fn request_then_take_round_trips_the_move_slot() {
        let cs = CommandState::default();

        cs.request_inventory_move(23, 19);
        assert_eq!(cs.take_inventory_move(), Some((23, 19)));
        assert_eq!(cs.take_inventory_move(), None, "a drained move must not re-fire");
    }

    /// A slot with nothing queued drains to `None` — no phantom command.
    #[test]
    fn take_on_empty_slot_is_none() {
        let cs = CommandState::default();
        assert_eq!(cs.take_inventory_move(), None);
    }
}
