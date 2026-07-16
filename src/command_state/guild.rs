//! Guild command verbs — Wave-2 fan-out stub (see `mod.rs` "HOW TO MIGRATE A DOMAIN").
//!
//! Domain: `/v1/guild/*` (the one queued guild action). Slots live in `self.guild`, already a field on `CommandState`. Fill the
//! shell below with `request_<verb>` (view writes: UI + HTTP) and `take_<thing>() -> Option<T>`
//! (the `ActionLoop` drain), each a thin typed read/write of `self.guild.<slot>`. Copy the
//! shape of `combat.rs`.

use super::CommandState;

// TODO(#446-fanout): migrate guild call sites to these methods.
impl CommandState {}
