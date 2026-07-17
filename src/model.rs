//! `Model` — the COLD backend seam of the client's MVC (epic #445, increment B1 / #449).
//!
//! ## What "the Model" is
//!
//! In this client the **Model is the `eq-net` thread** — the SOLE writer of [`GameState`]. It owns
//! the world: it connects, drains the view's COMMANDS (the [`CommandState`] write-path slots), applies
//! inbound world packets to its private `GameState`, and PUBLISHES an immutable snapshot each tick
//! (`eq_net::gameplay::publish_snapshot` → the `ArcSwap` in [`ModelContext::game_state_snapshot`]).
//! Render/HTTP read that snapshot lock-free via `load_full`.
//!
//! Until B1 that owner was a bare free function (`eq_net::run_login_flow`) hard-wired into `main.rs`.
//! B1 puts it behind a trait so the backend becomes SWAPPABLE — B2 (#450) adds a `MockModel` that
//! implements this same trait with NO server, which is what breaks the circular nav-QA loop (a
//! headless test can drive nav/actions against a deterministic world).
//!
//! ## The seam (consume COMMANDS, produce WORLD SNAPSHOTS)
//!
//! [`ModelContext`] IS the seam, and it is backend-AGNOSTIC:
//!   * **in**  — [`CommandState`] (the typed view→model command slots) + the raw camp/respawn/shutdown
//!     lifecycle signals + the per-domain published-roster bundles the owner also fills;
//!   * **out** — [`ModelContext::game_state_snapshot`] (the `ArcSwap<GameState>` the render hot path
//!     reads) and [`ModelContext::net_health`] (the liveness clocks HTTP turns into `connected` etc).
//!
//! A concrete [`Model`] holds only its backend-SPECIFIC configuration ([`ServerModel`] holds the
//! [`LoginConfig`] + retry count; a future `MockModel` would hold its scripted world). Both receive
//! the SAME `ModelContext`, so a test can point either backend at the same snapshot/command plumbing.
//!
//! ## SCOPE / what is deliberately NOT touched (behavior-preserving, per #449)
//!
//! * The `ArcSwap` snapshot publish path is UNCHANGED — `ServerModel::run` delegates verbatim to the
//!   existing `eq_net::run_login_flow`, so the frozen-`connected` class of bug (#343) cannot be
//!   reintroduced by this increment. This module adds a trait + a struct + a context bundle and
//!   re-homes `main.rs`'s call through them; it moves no world logic.
//! * The trait abstracts the COLD backend only. It has NO per-frame read method — the hot read stays
//!   the concrete `GameState` snapshot (`ArcSwap::load_full`), never a trait object. There is no
//!   per-frame dynamic dispatch anywhere in this module by construction.
//! * `run` is a concrete (non-`dyn`) `async fn` in the trait ON PURPOSE: the client only ever holds
//!   ONE backend at a time and drives it inside the `eq-net` thread's `block_on`, so no boxing / no
//!   `Send`-on-`Future` bound / no `Pin<Box<dyn Future>>` ceremony is needed. B2's headless test
//!   holds a concrete `MockModel` (or is generic over `M: Model`); neither needs `dyn`.
//!
//! ## Shared-Arc identity (load-bearing — see epic #445)
//!
//! `ModelContext` holds `.clone()`s of the SAME `ipc` bundle Arcs `main.rs` also clones into
//! `HttpState`/`CommandState`. Those are shallow Arc-handle clones — identity is preserved — so a
//! `request_*` write on the view side and its `take_*` drain inside the owner still hit the identical
//! cell. Re-homing the construction from a flat `run_login_flow(a, b, …)` arg list into a
//! `ModelContext { … }` literal moves only WHERE the clone lands, not the Arc it points at.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::command_state::CommandState;
use crate::config::LoginConfig;

/// The backend-agnostic Model seam: the command slots a backend DRAINS and the snapshot/health it
/// PUBLISHES, plus the lifecycle signals it obeys. Constructed once in `main.rs` (the Controller /
/// wiring role) from `.clone()`s of the bundles it also hands to `HttpState`/`CommandState`, then
/// moved into whichever [`Model`] drives the `eq-net` thread. Every field is a shallow Arc-handle
/// clone — see the module doc's shared-Arc note.
///
/// This mirrors, one-for-one, the parameters `eq_net::run_login_flow` already takes (minus the
/// server-specific `config`/`max_retries`, which live on [`ServerModel`]); B1 introduces no new
/// channels, it only groups the existing ones under the seam they collectively form.
pub struct ModelContext {
    pub nav:             crate::ipc::NavSlots,
    pub world:           crate::ipc::WorldSlots,
    pub quest:           crate::ipc::QuestSlots,
    pub group_slots:     crate::ipc::GroupSlots,
    /// The typed view→model command write-path facade (the "consume COMMANDS" half of the seam).
    pub command:         CommandState,
    pub social:          crate::ipc::SocialSlots,
    pub merchant_slots:  crate::ipc::MerchantSlots,
    pub inventory_slots: crate::ipc::InventorySlots,
    pub interact:        crate::ipc::InteractSlots,
    pub chat:            crate::ipc::ChatSlots,
    pub controller:      crate::ipc::ControllerSlots,
    pub guild_slots:     crate::ipc::GuildSlots,
    pub collision:       crate::nav::collision::SharedCollision,
    pub maps_dir:        PathBuf,
    pub shutdown:        Arc<AtomicBool>,
    pub camp:            crate::ipc::CampReq,
    pub camp_until:      crate::ipc::CampUntil,
    pub respawn:         crate::ipc::RespawnReq,
    /// The `ArcSwap<GameState>` render/HTTP read lock-free (the "produce WORLD SNAPSHOTS" half).
    pub game_state_snapshot: crate::ipc::GameStateSnapshot,
    /// The liveness clocks the owner stamps; HTTP derives `connected`/`world_responsive` from them.
    pub net_health:          crate::ipc::NetHealthShared,
}

/// The cold backend seam. A `Model` OWNS the world: given a [`ModelContext`], it drives the world
/// state to completion on the `eq-net` thread — connecting (or not, for a mock), draining the
/// context's commands, and publishing `GameState` snapshots each tick — until shutdown.
///
/// `run` takes `self` by value: driving the backend consumes it (it runs for the process lifetime).
/// It is intentionally a concrete `async fn` (no `dyn`, no `Send` bound) — see the module doc.
// `async fn` in a public trait warns because a caller can't add its own `Send` bound; that is
// exactly fine here (the single caller drives it on one thread inside `block_on`), so silence it.
#[allow(async_fn_in_trait)]
pub trait Model {
    /// Drive the backend world to completion. `Ok(())` on a clean end (e.g. login retries exhausted
    /// cleanly / shutdown); `Err` on a fatal backend failure the caller logs.
    async fn run(self, ctx: ModelContext) -> Result<(), String>;
}

/// The production [`Model`]: the real EQ server connection. Holds only the server-specific config;
/// its [`Model::run`] delegates VERBATIM to the existing `eq_net::run_login_flow` (login handshake →
/// `run_gameplay_phase`, the sole `GameState` writer / snapshot publisher). This is the concrete
/// backend the client has always run — B1 only names it and routes construction through the trait.
pub struct ServerModel {
    config:      LoginConfig,
    max_retries: u32,
}

impl ServerModel {
    /// `config` is the per-character login profile; `max_retries` is the login-flow retry budget
    /// (`main.rs` has always passed `10`).
    pub fn new(config: LoginConfig, max_retries: u32) -> Self {
        ServerModel { config, max_retries }
    }
}

impl Model for ServerModel {
    async fn run(self, ctx: ModelContext) -> Result<(), String> {
        // Delegate verbatim to the pre-existing owner. Destructure the context back into the exact
        // arg list `run_login_flow` expects, in order — this is pure re-plumbing, no behavior change.
        crate::eq_net::run_login_flow(
            self.config,
            self.max_retries,
            ctx.nav,
            ctx.world,
            ctx.quest,
            ctx.group_slots,
            ctx.command,
            ctx.social,
            ctx.merchant_slots,
            ctx.inventory_slots,
            ctx.interact,
            ctx.chat,
            ctx.controller,
            ctx.guild_slots,
            ctx.collision,
            ctx.maps_dir,
            ctx.shutdown,
            ctx.camp,
            ctx.camp_until,
            ctx.respawn,
            ctx.game_state_snapshot,
            ctx.net_health,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Compile-time proof the production backend implements the seam. If a future edit changed the
    /// trait so `ServerModel` no longer satisfied it, THIS stops compiling — the bad state is
    /// unrepresentable (verification hierarchy tier 1), not merely untested.
    const _: fn() = || {
        fn asserts_impl<M: Model>() {}
        asserts_impl::<ServerModel>();
    };

    /// A server-free [`ModelContext`] for tests: every command/roster bundle is `Default`
    /// (empty slots), the snapshot is seeded with a fresh `GameState`. This is exactly the plumbing
    /// B2's `MockModel` headless test will reuse, which is why the seam was made backend-agnostic.
    fn test_ctx() -> ModelContext {
        ModelContext {
            nav:             Default::default(),
            world:           Default::default(),
            quest:           Default::default(),
            group_slots:     Default::default(),
            command:         CommandState::default(),
            social:          Default::default(),
            merchant_slots:  Default::default(),
            inventory_slots: Default::default(),
            interact:        Default::default(),
            chat:            Default::default(),
            controller:      Default::default(),
            guild_slots:     Default::default(),
            collision:       Default::default(),
            maps_dir:        PathBuf::new(),
            shutdown:        Arc::new(AtomicBool::new(false)),
            camp:            Default::default(),
            camp_until:      Default::default(),
            respawn:         Default::default(),
            game_state_snapshot: Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::game_state::GameState::new(),
            )),
            net_health:      Arc::new(std::sync::Mutex::new(crate::ipc::NetHealth::default())),
        }
    }

    /// A no-server backend that flips a flag and returns — proving the [`Model`] trait is drivable
    /// with NO network at all. This is the B2 (#450) premise ("MockModel breaks the circular nav-QA
    /// loop") reduced to its essence: if the trait had leaked something server-only into its
    /// signature, this mock could not exist and this test would not compile/run.
    struct FlagModel(Arc<AtomicBool>);
    impl Model for FlagModel {
        async fn run(self, ctx: ModelContext) -> Result<(), String> {
            // Touch the seam the way a real backend does: observe a command handle and publish a
            // snapshot — with no server anywhere in sight.
            let _ = ctx.command; // the command write-path is consumable by a backend
            ctx.game_state_snapshot
                .store(Arc::new(crate::game_state::GameState::new()));
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn model_trait_is_drivable_without_a_server() {
        let ran = Arc::new(AtomicBool::new(false));
        let model = FlagModel(ran.clone());
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let out = rt.block_on(model.run(test_ctx()));
        assert_eq!(out, Ok(()));
        assert!(ran.load(Ordering::SeqCst), "the backend's run() body executed");
    }
}
