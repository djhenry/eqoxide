//! Interact command verbs — Wave-2 fan-out stub (see `mod.rs` "HOW TO MIGRATE A DOMAIN").
//!
//! Domain: `/v1/interact/*` (hail/say/loot/give/doors/sit/dialogue/read). Slots live in `self.interact`, already a field on `CommandState`. Fill the
//! shell below with `request_<verb>` (view writes: UI + HTTP) and `take_<thing>() -> Option<T>`
//! (the `ActionLoop` drain), each a thin typed read/write of `self.interact.<slot>`. Copy the
//! shape of `combat.rs`.

use super::CommandState;

// TODO(#446-fanout): migrate interact call sites to these methods.
impl CommandState {}
