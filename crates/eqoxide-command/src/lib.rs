//! `eqoxide-command` — `CommandState`, the single typed facade over the **view → model COMMAND**
//! (write-path) IPC slots, extracted into its own workspace crate (#544 Step 2g).
//!
//! Depends ONLY on `eqoxide-core` (`game_state::DialogueChoice`/`WhoEntry`) and `eqoxide-ipc` (the
//! `*Slots` bundles, `CommandResult`/`BuyOk`/`OpenOk`/`GiveOk`/`CastEnd`) plus `tokio::sync::oneshot`
//! for the await-slot reply channels — never on any app-layer crate (no wgpu/winit/egui/eq_net/http/
//! app/renderer/nav/assets). The app crate re-exports this crate as `crate::command_state`
//! (`pub use eqoxide_command as command_state;`) so every existing `crate::command_state::…` call
//! site (`http/*`, `app.rs`, `eq_net/action_loop.rs`, `ui/*`) keeps resolving unchanged. Pure code
//! motion — no behavior change; see the module docs below for the facade's actual contract.
//!
//! The write path today is untyped: a UI click-handler or an HTTP handler reaches into a raw
//! request slot (`Arc<Mutex<Option<T>>>`, grouped by domain in [`eqoxide_ipc`]) with
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
/// A3 Migration 1 (#448): the reusable Command-with-result infra. `CommandResult<T>` is the honest
/// three-way outcome (Resolved/Refused/Unconfirmed) an HTTP handler awaits so it reports the TRUE
/// server outcome instead of a premature queued-action 200. See `eqoxide_ipc::result`'s module doc
/// for the full status mapping, invariant, and park→fulfil→timeout flow.
///
/// (#557) `CommandResult<T>` and its payload types (`BuyOk`, `OpenOk`, `GiveOk`, `CastEnd`) live in
/// `eqoxide_ipc::result` now, NOT here — `ipc`'s own await-slot types reference them, so keeping them
/// in `command_state` (which depends on `ipc`) would be a dependency cycle once the two split into
/// separate crates. Re-exported below so every existing `crate::command_state::CommandResult`/
/// `BuyOk`/`OpenOk`/`GiveOk`/`CastEnd` call site is unaffected — pure code motion, no behavior change.
pub use eqoxide_ipc::result;
pub use eqoxide_ipc::CastEnd;
pub use eqoxide_ipc::CommandResult;
// Wave-2 fan-out stubs — one file each, empty `impl CommandState {}` shell awaiting migration.
mod merchant;
pub use eqoxide_ipc::{BuyOk, OpenOk};
mod inventory;
mod interact;
pub use eqoxide_ipc::GiveOk;
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
    combat:    eqoxide_ipc::CombatSlots,
    merchant:  eqoxide_ipc::MerchantSlots,
    inventory: eqoxide_ipc::InventorySlots,
    interact:  eqoxide_ipc::InteractSlots,
    quest:     eqoxide_ipc::QuestSlots,
    group:     eqoxide_ipc::GroupSlots,
    guild:     eqoxide_ipc::GuildSlots,
    trainer:   eqoxide_ipc::TrainerSlots,
    social:    eqoxide_ipc::SocialSlots,
    chat:      eqoxide_ipc::ChatSlots,
    nav:       eqoxide_ipc::NavSlots,
    lifecycle: eqoxide_ipc::LifecycleSlots,
}

impl CommandState {
    /// Wiring only (Controller/`main` role): takes `.clone()`s of the bundles constructed once in
    /// `main.rs` — the SAME Arcs `ActionLoop`/`HttpState` receive, so the facade shares their slots.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        combat:    eqoxide_ipc::CombatSlots,
        merchant:  eqoxide_ipc::MerchantSlots,
        inventory: eqoxide_ipc::InventorySlots,
        interact:  eqoxide_ipc::InteractSlots,
        quest:     eqoxide_ipc::QuestSlots,
        group:     eqoxide_ipc::GroupSlots,
        guild:     eqoxide_ipc::GuildSlots,
        trainer:   eqoxide_ipc::TrainerSlots,
        social:    eqoxide_ipc::SocialSlots,
        chat:      eqoxide_ipc::ChatSlots,
        nav:       eqoxide_ipc::NavSlots,
        lifecycle: eqoxide_ipc::LifecycleSlots,
    ) -> Self {
        CommandState {
            combat, merchant, inventory, interact, quest, group, guild, trainer, social, chat,
            nav, lifecycle,
        }
    }
}
