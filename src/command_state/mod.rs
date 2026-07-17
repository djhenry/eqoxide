//! `CommandState` — the single typed facade over the **view → model COMMAND** (write-path) IPC slots.
//!
//! The write path today is untyped: a UI click-handler or an HTTP handler reaches into a raw
//! request slot (`Arc<Mutex<Option<T>>>`, grouped by domain in [`crate::ipc`]) with
//! `*slot.lock().unwrap() = Some(..)`, and the `eq-net` thread's `ActionLoop::tick` drains it with
//! `slot.lock().unwrap().take()`. `CommandState` puts ONE typed method in front of each of those
//! slots so no call site pokes an `Arc<Mutex<..>>` by hand.
//!
//! It is **behavior-preserving plumbing, not new logic.** `CommandState` holds `.clone()`s of the
//! SAME `ipc` bundle Arcs that are also handed to `ActionLoop` and `HttpState`, so a `request_*`
//! write and the matching `take_*` drain touch the exact same cell the old direct access did. That
//! shared-Arc identity is what lets a domain migrate one file at a time without any behavior change:
//! a migrated `request_*` and an un-migrated `*slot.lock() = ..` still land in the same slot.
//!
//! SCOPE: the **view→model command / action** domains only — Combat, Merchant, Inventory, Interact,
//! Quest, Group, Guild, Trainer, Social, Chat, plus the model-bound movement commands (Nav
//! goto/follow/stop/zone-cross, Lifecycle exit/respawn). It does **not** absorb the read-path
//! published snapshots (`GameState`, merchant/inventory/group rosters, nav status, …) — those stay on
//! the model/read side. A few `ipc` bundles physically carry a snapshot field alongside their command
//! slots (e.g. `MerchantSlots.merchant`); `CommandState` holds the whole bundle for construction
//! convenience but deliberately exposes methods over the COMMAND slots only. Do NOT add snapshot
//! getters here.
//!
//! ## MVC C2 boundary (#452) — what this facade DELIBERATELY does NOT hold
//! C2 tidied the boundary so `CommandState` carries ONLY genuine view→**model** commands. Two things
//! that are not view→model commands were relocated OUT (each is Arc-shared, wiring unchanged):
//!   * **Camera / manual-move → view-only.** The manual-move/jump escape hatch (`ManualMoveReq`) is a
//!     view→**render** command — the render thread's `CharacterController` consumes it, the Model/nav
//!     thread never does — so it lives on `ipc::CameraSlots` (the render-bound bundle, alongside the
//!     orbit-camera `cmd_tx`), NOT here. `App` reads it per frame; `CameraSlots::request_manual_move`
//!     is the typed HTTP write. The orbit **camera angle** itself was already view-only (never on this
//!     facade). The only movement input that IS a model command — the **derived heading** — is
//!     computed render-side (`movement::manual_wish` → `heading_target`) and reaches the Model on the
//!     controller/prediction channel (`ControllerSlots::controller_view.heading`, streamed by
//!     `ActionLoop::stream_position`); it rides that channel atomically with the predicted position
//!     (C1's client-prediction split) and is intentionally not carved into a separate slot.
//!   * **Computed nav path → read-side.** The walker's committed path overlay (`ipc::NavPathView`)
//!     is Model→View DERIVED render state, not a command, so it moved from the `NavSlots` command
//!     bundle to `ipc::ControllerSlots` (the render↔nav integration channels). See those two `ipc`
//!     structs' docs.
//!
//! Forward-compatible with A3 (Command-with-result): A3 will add result-returning variants
//! (`request_*_await`, generalizing the existing `oneshot` reply used by `FrameReq`/`WhoReq`)
//! WITHOUT renaming anything below — that is why the fire-and-forget writes are `request_*`, not a
//! bare verb.
//!
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! HOW TO MIGRATE A DOMAIN  (Wave-2 fan-out: LIGHT migration — leave shared structs/signatures alone)
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! `combat.rs` is the fully-migrated reference — copy its shape. For your domain `<d>`:
//!
//! 1. NAMING. In `command_state/<d>.rs`, fill the `impl CommandState {}` shell with two kinds of
//!    method, all reading `self.<d>.<slot>` (your bundle is already a field on this struct):
//!      • `request_<verb>(args)`  — the write the VIEW makes (UI + HTTP). Sets `Some(..)` into the
//!        slot. Name it after the domain verb; prefix with the domain where a bare verb could
//!        collide with another domain's method (e.g. `request_group_invite`, not `request_invite`).
//!      • `take_<thing>() -> Option<T>` — the drain the `ActionLoop` makes once per tick. Returns
//!        `slot.lock().unwrap().take()`.
//! 2. SLOT LOCATION. The slots already live in your `ipc::<D>Slots` bundle, held as the private
//!    `<d>` field on `CommandState` (see the struct below). You do not add fields or touch this
//!    file's struct — just read `self.<d>.<slot>` from your methods.
//! 3. TWO CALL-SITE EDITS. Point both ends of each slot at the new methods:
//!      • VIEW writes → method: `*s.<d>.<slot>.lock().unwrap() = Some(x)` becomes
//!        `s.command.request_<verb>(x)` in `http/<d>.rs`, and `*cx.acts.<slot>.lock()… = Some(x)`
//!        becomes `cx.acts.command.request_<verb>(x)` in `ui/windows/<d>.rs`.
//!      • MODEL drain → `take_*`: in `action_loop.rs`, `self.<d>.<slot>.lock().unwrap().take()`
//!        becomes `self.command.take_<thing>()` (each domain's drain lives in its own `drain_<d>`
//!        method — non-adjacent, so parallel Wave-2 branches auto-merge here).
//! 4. DO NOT remove the `<d>` bundle field or touch any `fn` signature. Leaving the now-dead field
//!    in place is DELIBERATE: it keeps every Wave-2 branch off the shared lines (`CommandState`'s
//!    struct/`new()`, `main.rs`, `http/mod.rs`, `ui/mod.rs`, `ActionLoop::new`/`run_login_flow`/
//!    `spawn_camera_server` signatures) so the branches don't collide. A SINGLE final cleanup PR
//!    removes all the dead bundle fields + trims those signatures at once, and drops the
//!    `#[allow(dead_code)]`. (`combat.rs` is the reference for the METHOD shape; note combat also
//!    already removed its field — that is the eventual end state, NOT what a Wave-2 domain does.)

mod combat;
pub use combat::CastEnd;
/// A3 Migration 1 (#448): the reusable Command-with-result infra. `CommandResult<T>` is the honest
/// three-way outcome (Resolved/Refused/Unconfirmed) an HTTP handler awaits so it reports the TRUE
/// server outcome instead of a premature queued-action 200. THE reference for A3.2/A3.3 — see its
/// module doc for the full status mapping, invariant, and park→fulfil→timeout flow.
pub mod result;
pub use result::CommandResult;
// Wave-2 fan-out stubs — one file each, empty `impl CommandState {}` shell awaiting migration.
mod merchant;
pub use merchant::{BuyOk, OpenOk};
mod inventory;
mod interact;
pub use interact::GiveOk;
mod quest;
mod group;
mod guild;
mod trainer;
mod social;
mod chat;
mod nav;
mod lifecycle;

/// The typed write-path facade. Holds `.clone()`d handles of the same `ipc` command bundles that
/// `ActionLoop` and `HttpState` hold; every method is a thin typed read/write of one of their slots.
///
/// (#459 stragglers) nav/lifecycle are migrated too now, so every domain field is read by at least
/// one `request_*`/`take_*`/`peek_*` method — no field-level `#[allow(dead_code)]` remains. See
/// `nav.rs`/`lifecycle.rs` for the (narrower, non-`ActionLoop`-drain) shape those domains ended up
/// with, and their module docs for what was deliberately left un-migrated (`nav/walker.rs`'s internal
/// goto state machine; `eq_net::gameplay`'s camp/respawn drain).
///
/// MVC C2 (#452): the lone camera `manual_move` slot that once sat here was relocated to the
/// render-bound `ipc::CameraSlots` (it is a view→render command, not view→model) — see this module's
/// doc. Every field below is now a genuine view→model command bundle.
#[derive(Clone, Default)]
pub struct CommandState {
    combat:    crate::ipc::CombatSlots,
    merchant:  crate::ipc::MerchantSlots,
    inventory: crate::ipc::InventorySlots,
    interact:  crate::ipc::InteractSlots,
    quest:     crate::ipc::QuestSlots,
    group:     crate::ipc::GroupSlots,
    guild:     crate::ipc::GuildSlots,
    trainer:   crate::ipc::TrainerSlots,
    social:    crate::ipc::SocialSlots,
    chat:      crate::ipc::ChatSlots,
    nav:       crate::ipc::NavSlots,
    lifecycle: crate::ipc::LifecycleSlots,
}

impl CommandState {
    /// Wiring only (Controller/`main` role): takes `.clone()`s of the bundles constructed once in
    /// `main.rs` — the SAME Arcs `ActionLoop`/`HttpState` receive, so the facade shares their slots.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        combat:    crate::ipc::CombatSlots,
        merchant:  crate::ipc::MerchantSlots,
        inventory: crate::ipc::InventorySlots,
        interact:  crate::ipc::InteractSlots,
        quest:     crate::ipc::QuestSlots,
        group:     crate::ipc::GroupSlots,
        guild:     crate::ipc::GuildSlots,
        trainer:   crate::ipc::TrainerSlots,
        social:    crate::ipc::SocialSlots,
        chat:      crate::ipc::ChatSlots,
        nav:       crate::ipc::NavSlots,
        lifecycle: crate::ipc::LifecycleSlots,
    ) -> Self {
        CommandState {
            combat, merchant, inventory, interact, quest, group, guild, trainer, social, chat,
            nav, lifecycle,
        }
    }
}
