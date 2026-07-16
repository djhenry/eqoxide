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
//! SCOPE: the **command / action** domains only — Combat, Merchant, Inventory, Interact, Quest,
//! Group, Guild, Trainer, Social, Chat, plus the movement commands (Nav goto/follow/stop, Camera
//! manual-move, Lifecycle exit/respawn). It does **not** absorb the read-path published snapshots
//! (`GameState`, merchant/inventory/group rosters, nav status, …) — those stay on the model/read
//! side. A few `ipc` bundles physically carry a snapshot field alongside their command slots (e.g.
//! `MerchantSlots.merchant`); `CommandState` holds the whole bundle for construction convenience but
//! deliberately exposes methods over the COMMAND slots only. Do NOT add snapshot getters here.
//!
//! Forward-compatible with A3 (Command-with-result): A3 will add result-returning variants
//! (`request_*_await`, generalizing the existing `oneshot` reply used by `FrameReq`/`WhoReq`)
//! WITHOUT renaming anything below — that is why the fire-and-forget writes are `request_*`, not a
//! bare verb.
//!
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! HOW TO MIGRATE A DOMAIN  (Wave-2 fan-out: one agent per file, near-zero shared-file conflict)
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
//!        becomes `self.command.take_<thing>()`.
//!    Then drop the now-unused `<d>` bundle field from `HttpState`/`ActionLoop`/`ui::Actions` and
//!    their param lists (combat did exactly this — that is the end state to copy).

mod combat;
// Wave-2 fan-out stubs — one file each, empty `impl CommandState {}` shell awaiting migration.
mod merchant;
mod inventory;
mod interact;
mod quest;
mod group;
mod guild;
mod trainer;
mod social;
mod chat;
mod nav;
mod camera;
mod lifecycle;

/// The typed write-path facade. Holds `.clone()`d handles of the same `ipc` command bundles that
/// `ActionLoop` and `HttpState` hold; every method is a thin typed read/write of one of their slots.
///
/// `#[allow(dead_code)]`: only the migrated domains' fields are read today (combat, in `combat.rs`).
/// The rest are pre-declared so Wave-2 fan-out agents fill ONLY their own `command_state/<d>.rs`
/// and its call sites — never this struct — giving conflict-free parallel migration. Each field
/// starts being read the moment its domain's methods land, at which point remove it from the allow
/// (or the whole allow, once every domain has migrated).
#[derive(Clone, Default)]
#[allow(dead_code)]
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
    /// Camera's ONLY command slot (the manual-move/jump escape hatch). `CameraSlots` as a whole is
    /// deliberately NOT held here: it has no `Default` (its snapshot's initial value is meaningful)
    /// and its other fields are read-path. Held as the lone Arc so `CommandState` stays `Default`.
    camera_manual_move: crate::ipc::ManualMoveReq,
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
        camera_manual_move: crate::ipc::ManualMoveReq,
    ) -> Self {
        CommandState {
            combat, merchant, inventory, interact, quest, group, guild, trainer, social, chat,
            nav, lifecycle, camera_manual_move,
        }
    }
}
