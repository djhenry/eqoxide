//! Combat command verbs — the fully-migrated reference for the Wave-2 fan-out (see `mod.rs`).
//!
//! Domain: `/v1/combat/*` targeting, auto-attack, consider, spell cast/memorize/scribe, plus the
//! one `/v1/pet/command` slot (which rides in `ipc::CombatSlots` — see its doc). Every method is a
//! thin typed read/write of a slot in `self.combat`; validation and packet-building stay where they
//! were (the HTTP handler and `ActionLoop::tick`). No behavior change — just one typed surface.

use super::CommandState;
use crate::ipc::CastRequest;

impl CommandState {
    // ── request_* : the VIEW (UI click-handlers + HTTP handlers) makes these writes ──────────────

    /// Target a spawn (POST /v1/combat/target{,/name}, the Target/Actions windows). The drain
    /// (`take_target`) auto-considers it. Caller validates the id is in-zone first.
    pub fn request_target(&self, spawn_id: u32) {
        *self.combat.target.lock().unwrap() = Some(spawn_id);
    }

    /// Toggle auto-attack (POST/DELETE /v1/combat/attack, the Attack button). `on` = engage.
    pub fn request_attack(&self, on: bool) {
        *self.combat.attack.lock().unwrap() = Some(on);
    }

    /// Consider a spawn (POST /v1/combat/consider, the Consider button) — con color/faction reply.
    pub fn request_consider(&self, spawn_id: u32) {
        *self.combat.consider.lock().unwrap() = Some(spawn_id);
    }

    /// Cast a memorized gem, a spell id, or an item clicky (POST /v1/combat/cast, the spell-gem /
    /// spellbook windows). The handler builds the [`CastRequest`]; the drain resolves the target.
    pub fn request_cast(&self, req: CastRequest) {
        *self.combat.cast.lock().unwrap() = Some(req);
    }

    /// Memorize a known spell (`scribing = 1`) or scribe a scroll (`scribing = 0`) into a book/gem
    /// `slot` (POST /v1/combat/{memorize,scribe}). `from` is the scroll's inventory wire slot for a
    /// scribe (moved to cursor first by the drain), `None` for a memorize. Tuple shape preserved
    /// verbatim from `ipc::MemSpellReq`.
    pub fn request_mem_spell(&self, slot: u32, spell_id: u32, scribing: u32, from: Option<u32>) {
        *self.combat.mem_spell.lock().unwrap() = Some((slot, spell_id, scribing, from));
    }

    /// Queue one OP_PetCommands byte (POST /v1/pet/command, the Pet window). See `PET_*` constants.
    pub fn request_pet_command(&self, cmd: u8) {
        *self.combat.pet_cmd.lock().unwrap() = Some(cmd);
    }

    // ── take_* : the MODEL (`ActionLoop::tick`) drains these once per tick ────────────────────────

    /// Drain a pending target request. `Some(spawn_id)` if one was queued since the last tick.
    pub fn take_target(&self) -> Option<u32> {
        self.combat.target.lock().unwrap().take()
    }

    /// Drain a pending auto-attack toggle. `Some(true)` = engage, `Some(false)` = disengage.
    pub fn take_attack(&self) -> Option<bool> {
        self.combat.attack.lock().unwrap().take()
    }

    /// Drain a pending consider request.
    pub fn take_consider(&self) -> Option<u32> {
        self.combat.consider.lock().unwrap().take()
    }

    /// Drain a pending cast request.
    pub fn take_cast(&self) -> Option<CastRequest> {
        self.combat.cast.lock().unwrap().take()
    }

    /// Drain a pending memorize/scribe request as `(slot, spell_id, scribing, from)`.
    pub fn take_mem_spell(&self) -> Option<(u32, u32, u32, Option<u32>)> {
        self.combat.mem_spell.lock().unwrap().take()
    }

    /// Drain a pending pet command byte.
    pub fn take_pet_command(&self) -> Option<u8> {
        self.combat.pet_cmd.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;

    /// The core invariant of the facade: a `request_*` write and the matching `take_*` drain touch
    /// the SAME slot, the drain removes it (so a stale command can't fire twice), and a second drain
    /// sees nothing. Proven for every combat verb.
    #[test]
    fn request_then_take_round_trips_each_combat_slot() {
        let cs = CommandState::default();

        // request_target → take_target (once), then empty.
        cs.request_target(42);
        assert_eq!(cs.take_target(), Some(42));
        assert_eq!(cs.take_target(), None, "a drained target must not re-fire");

        cs.request_attack(true);
        assert_eq!(cs.take_attack(), Some(true));
        assert_eq!(cs.take_attack(), None);

        cs.request_consider(7);
        assert_eq!(cs.take_consider(), Some(7));
        assert_eq!(cs.take_consider(), None);

        cs.request_cast(crate::ipc::CastRequest { gem: 3, target_id: Some(9), item_slot: None });
        let cast = cs.take_cast().expect("cast queued");
        assert_eq!((cast.gem, cast.target_id, cast.item_slot), (3, Some(9), None));
        assert!(cs.take_cast().is_none());

        cs.request_mem_spell(2, 202, 1, None);
        assert_eq!(cs.take_mem_spell(), Some((2, 202, 1, None)));
        assert_eq!(cs.take_mem_spell(), None);

        cs.request_pet_command(2);
        assert_eq!(cs.take_pet_command(), Some(2));
        assert_eq!(cs.take_pet_command(), None);
    }

    /// A slot with nothing queued drains to `None` — no phantom command.
    #[test]
    fn take_on_empty_slot_is_none() {
        let cs = CommandState::default();
        assert_eq!(cs.take_target(), None);
        assert_eq!(cs.take_attack(), None);
        assert!(cs.take_cast().is_none());
    }
}
