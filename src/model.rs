//! `Model` — the COLD backend seam of the client's MVC (epic #445, increment B1 / #449).
//!
//! ## What "the Model" is
//!
//! In this client the **Model is the `eq-net` thread** — the SOLE writer of
//! [`GameState`](crate::game_state::GameState). It owns
//! the world: it connects, drains the view's COMMANDS (the
//! [`CommandState`](crate::command_state::CommandState) write-path slots), applies
//! inbound world packets to its private `GameState`, and PUBLISHES an immutable snapshot each tick
//! (`eq_net::gameplay::publish_snapshot` → the `ArcSwap` in
//! [`GameplayLifecycle::game_state_snapshot`](crate::eq_net::gameplay::GameplayLifecycle::game_state_snapshot)).
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
//!   * **in**  — [`CommandState`](crate::command_state::CommandState) (the typed view→model command
//!     slots) + the raw camp/respawn/shutdown lifecycle signals + the per-domain published-roster
//!     bundles the owner also fills;
//!   * **out** — [`GameplayLifecycle::game_state_snapshot`](crate::eq_net::gameplay::GameplayLifecycle::game_state_snapshot)
//!     (the `ArcSwap<GameState>` the render hot path reads) and
//!     [`GameplayLifecycle::net_health`](crate::eq_net::gameplay::GameplayLifecycle::net_health)
//!     (the liveness clocks HTTP turns into `connected` etc).
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
//! * `ModelContext`'s two fields ARE, by type, `eq_net`'s own [`ActionLoopSlots`](crate::eq_net::action_loop::ActionLoopSlots)
//!   and [`GameplayLifecycle`](crate::eq_net::gameplay::GameplayLifecycle) — a deliberate trade, not an
//!   oversight: it kept this refactor a pure re-plumbing (no new type, no field-by-field re-derivation
//!   at the `ServerModel::run` boundary) at the cost of `ModelContext` now moving whenever those two
//!   `eq_net` structs do. "Backend-AGNOSTIC" above means every `Model` impl (`ServerModel`, `MockModel`,
//!   ...) is handed the identical bundle — not that the bundle's shape is independent of `eq_net`.
//!
//! ## Shared-Arc identity (load-bearing — see epic #445)
//!
//! `ModelContext` holds `.clone()`s of the SAME `ipc` bundle Arcs `main.rs` also clones into
//! `HttpState`/`CommandState`. Those are shallow Arc-handle clones — identity is preserved — so a
//! `request_*` write on the view side and its `take_*` drain inside the owner still hit the identical
//! cell. Re-homing the construction from a flat `run_login_flow(a, b, …)` arg list into a
//! `ModelContext { … }` literal moves only WHERE the clone lands, not the Arc it points at.

// Used only by the `#[cfg(test)]` mock backend and test-fixture code below — production `model.rs`
// code no longer names these bare (they live inside `ActionLoopSlots`/`GameplayLifecycle` now).
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
use crate::command_state::CommandState;
use crate::config::LoginConfig;

/// The `eq-net` thread's terminal-death observable (#634, agent-honesty). Re-exported from
/// `eqoxide-ipc`, where it moved with #890 so both the writer here and the HTTP readers can share
/// the TYPED form ([`eqoxide_ipc::NetThreadDeath`] = which end + the agent-facing prose) instead of
/// a bare `Option<String>` that every consumer had to collapse to `is_some()`. Written exactly once,
/// by [`run_net_thread`] (or by `main.rs`'s `--testzone` arm, where this thread never starts); read
/// by `eqoxide_http` and served on `GET /v1/observe/debug` as `net_thread_dead`.
pub use eqoxide_ipc::{NetThreadDeadShared, NetThreadDeath, NetThreadEnd};

/// Best-effort human-readable text out of a `catch_unwind` payload. Rust boxes the panic argument as
/// `&'static str` for `panic!("literal")` and `String` for `panic!("{fmt}")`; anything else is opaque.
fn panic_payload_text(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Runs the `eq-net` thread body at its thread boundary and **always** publishes an explicit terminal
/// state into `dead` when it ends, however it ends (#634).
///
/// ## Why this exists
/// `eq-net` is the client's Model: the sole writer of `GameState`, the sole drainer of the command
/// slots, and the sole stamper of the `NetHealth` clocks. Before this wrapper, all three of its exit
/// paths — a panic, a fatal `Err`, and a plain `Ok` return — simply ended the thread, and `main.rs`
/// published NOTHING. The process keeps running, the HTTP server keeps serving, and every cached
/// observable keeps reporting its last known value: position, zone, entities, health, all frozen at
/// plausible pre-death values. That is the agent-honesty failure this project ranks above crashes,
/// because the frozen state is *plausible* rather than empty.
///
/// ## What was ALREADY honest before this (do not oversell the fix)
/// The staleness was not literally invisible: `NetHealth`'s clocks are measured at HTTP READ time
/// (#343), so with the thread gone `snapshot_age_ms` climbs without bound, `connected` flips false
/// after `CONN_STALE_SECS`, and `require_live_session` (#477) starts 503-ing WRITE commands once
/// `snapshot_age_ms` passes `SESSION_STALE_TICK_MS`. What none of those can say is **why**, and —
/// the load-bearing part — whether it is **terminal**: a 5-second-old tick is equally consistent
/// with a busy loop about to recover and with a thread that no longer exists. An age answers "is
/// this stale?"; only this field answers "will it ever un-stale?". They are complementary, and an
/// agent that reads `net_thread_dead != null` can stop retrying immediately instead of polling a
/// corpse.
///
/// ## Exit paths, all four published — each tagged with its [`NetThreadEnd`] (#890)
/// * **panic** ([`NetThreadEnd::Panicked`]) — caught here (`catch_unwind` at the thread boundary,
///   mirroring the zone loader `src/app.rs` #595 and the two #616 workers rather than inventing a
///   new shape).
/// * **`Err(e)`** ([`NetThreadEnd::Fatal`]) — a fatal login failure / retries exhausted. Previously
///   logged (`EQ: fatal:`) and otherwise silent.
/// * **`Ok(())` with `shutdown` set** ([`NetThreadEnd::ReturnedDuringShutdown`]) — the body returned
///   while a shutdown was requested. Still published, because "the net thread is gone" is TRUE in
///   that window too and an agent polling during it deserves the truth rather than a healthy-looking
///   null. **This arm is NOT the ordinary `/v1/lifecycle/exit` teardown** — an earlier revision of
///   this doc said it was, and that was wrong: `run_gameplay_phase` reacts to the shutdown flag by
///   calling `perform_clean_shutdown` and then parking in `loop { sleep(200ms) }`, so on the
///   ordinary teardown this function is never reached and `net_thread_dead` stays `None` until the
///   process dies. Reaching this arm needs the phase to return for some other reason (zone-transition
///   failure, server-side session drop) inside a shutdown window. Read from
///   `crates/eqoxide-net/src/gameplay.rs`; not observed live.
/// * **`Ok(())` without `shutdown`** ([`NetThreadEnd::ReturnedUnexpectedly`]) — the gameplay phase
///   returned on its own. Called out separately in the reason text because it is unexpected.
///
/// A FIFTH state exists that this function cannot produce: [`NetThreadEnd::NeverStarted`], published
/// by `main.rs` in `--testzone` where no `eq-net` thread is spawned at all.
///
/// SCOPE — what `catch_unwind` here does and does not cover: `ServerModel::run` is awaited directly
/// inside `block_on` on this thread, and the production net path spawns no tokio tasks (the only
/// `tokio::spawn` in `eqoxide-net` is inside a `#[cfg(test)]` module), so every panic in the net
/// path unwinds through this frame. A panic on a `eq-net-tokio-worker` pool thread would NOT be
/// caught here — tokio catches those itself — but nothing production runs there today.
///
/// ⚠️ ORDERING, for whoever adds the first production `tokio::spawn` (#647 review, F6): the tokio
/// `Runtime` is constructed INSIDE `body` (see `main.rs`), so it is dropped during the unwind —
/// BEFORE this function reaches its publish. `Runtime::drop` blocks until spawned tasks wind down,
/// so a task that refuses to would stall the unwind here and the death report would never be
/// written, silently restoring the exact bug this function removes. It cannot happen today (no
/// production `tokio::spawn` exists on this path). If you add one, either construct the runtime
/// OUTSIDE `body` or publish before it drops.
pub fn run_net_thread<F>(
    dead:     &NetThreadDeadShared,
    shutdown: &std::sync::atomic::AtomicBool,
    body:     F,
) where
    F: FnOnce() -> Result<(), String>,
{
    let death = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Err(p) => {
            let msg = panic_payload_text(&*p);
            tracing::error!("EQ: eq-net thread panicked: {msg}");
            NetThreadDeath::new(NetThreadEnd::Panicked, format!(
                "the eq-net thread PANICKED ({msg}) — the client is no longer talking to the \
                 server. Every world field in this response is a FROZEN pre-panic snapshot and \
                 will never update again. No reconnect is running; restart the client."
            ))
        }
        Ok(Err(e)) => {
            tracing::error!("EQ: fatal: {e}");
            NetThreadDeath::new(NetThreadEnd::Fatal, format!(
                "the eq-net thread exited with a fatal error ({e}) — the client is no longer \
                 talking to the server. Every world field in this response is a FROZEN snapshot \
                 and will never update again. No reconnect is running; restart the client."
            ))
        }
        Ok(Ok(())) if shutdown.load(std::sync::atomic::Ordering::Relaxed) => {
            NetThreadDeath::new(NetThreadEnd::ReturnedDuringShutdown,
                "the eq-net thread exited normally after a shutdown was requested — the session is \
                 over and the process is shutting down. World fields are a frozen final snapshot.")
        }
        Ok(Ok(())) => {
            tracing::error!("EQ: eq-net thread returned without a shutdown request");
            NetThreadDeath::new(NetThreadEnd::ReturnedUnexpectedly,
                "the eq-net thread returned WITHOUT a shutdown being requested — the client is no \
                 longer talking to the server. Every world field in this response is a FROZEN \
                 snapshot and will never update again. No reconnect is running; restart the client.")
        }
    };
    // `unwrap_or_else(into_inner)` for the same reason #616's workers do it: this runs on a death
    // path, and refusing to publish the death because some other thread poisoned the mutex would be
    // the exact silence the field exists to remove.
    *dead.lock().unwrap_or_else(|e| e.into_inner()) = Some(death);
}

/// Publish the FIFTH state [`run_net_thread`] cannot produce: `--testzone` never spawns an `eq-net`
/// thread, so there is no server session and never will be one (#890/#935 B2c).
///
/// This lives here, next to the four death arms, rather than inline in `main.rs`, for one reason: a
/// value written inline in `main()` is not test-reachable, so nothing goes RED if it is re-tagged.
/// Mis-tagging it as a death is not a cosmetic error — [`NetThreadEnd::NeverStarted`] is precisely
/// what stops `/v1/lifecycle/exit` warning about a LINKDEAD session on a server this process never
/// connected to, and what stops the crash record naming a net thread that never existed.
///
/// NOT PINNED, and it cannot be from here: that `main.rs` calls this on the `--testzone` branch at
/// all. `main()` is not test-reachable; only the value it publishes is.
pub fn publish_never_started(dead: &NetThreadDeadShared) {
    *dead.lock().unwrap_or_else(|e| e.into_inner()) = Some(NetThreadDeath::new(
        NetThreadEnd::NeverStarted,
        "--testzone: the eq-net thread was never started (offline renderer mode). There is no \
         server connection; every world field is local test data, not the game.",
    ));
}

/// The backend-agnostic Model seam: the command slots a backend DRAINS and the snapshot/health it
/// PUBLISHES, plus the lifecycle signals it obeys. Constructed once in `main.rs` (the Controller /
/// wiring role) from `.clone()`s of the bundles it also hands to `HttpState`/`CommandState`, then
/// moved into whichever [`Model`] drives the `eq-net` thread. Every field is a shallow Arc-handle
/// clone — see the module doc's shared-Arc note.
///
/// This IS, one-for-one, the two parameter structs `eq_net::run_login_flow` already takes (minus
/// the server-specific `config`/`max_retries`, which live on [`ServerModel`]) — B1 introduces no
/// new channels, it only names the seam the existing ones already form.
pub struct ModelContext {
    /// The `ActionLoop`-owned slots: nav/world/quest/social/inventory/etc, and the view→model
    /// [`CommandState`](crate::command_state::CommandState) write-path facade (the "consume
    /// COMMANDS" half of the seam).
    pub slots:     crate::eq_net::action_loop::ActionLoopSlots,
    /// The gameplay-phase lifecycle signals (shutdown/camp/respawn) plus the snapshot/health the
    /// owner PUBLISHES (the "produce WORLD SNAPSHOTS" half of the seam).
    pub lifecycle: crate::eq_net::gameplay::GameplayLifecycle,
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
        // Delegate verbatim to the pre-existing owner — `ctx`'s two fields ARE the two parameter
        // structs `run_login_flow` expects, so this is pure re-plumbing, no behavior change.
        crate::eq_net::run_login_flow(self.config, self.max_retries, ctx.slots, ctx.lifecycle).await
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
            slots: crate::eq_net::action_loop::ActionLoopSlots {
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
            },
            lifecycle: crate::eq_net::gameplay::GameplayLifecycle {
                shutdown:        Arc::new(AtomicBool::new(false)),
                camp:            Default::default(),
                camp_until:      Default::default(),
                respawn:         Default::default(),
                game_state_snapshot: Arc::new(arc_swap::ArcSwap::from_pointee(
                    crate::game_state::GameState::new(),
                )),
                net_health:      Arc::new(std::sync::Mutex::new(crate::ipc::NetHealth::default())),
            },
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
            let _ = ctx.slots.command; // the command write-path is consumable by a backend
            ctx.lifecycle.game_state_snapshot
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

    // ── #634: the eq-net thread's terminal-death observable ──────────────────────────────────
    //
    // These drive the REAL production wrapper (`run_net_thread`) with deliberately misbehaving
    // bodies. They are the mutation target: replacing this wrapper's
    // `catch_unwind(AssertUnwindSafe(body))` with a bare `body()` makes the panic test RED (the
    // panic escapes the test), and deleting the final `*dead.lock() = Some(reason)` makes ALL of
    // them RED. On unmodified `origin/main` `run_net_thread` does not exist at all: main spawns the
    // thread with a bare closure whose only failure handling is `if let Err(e) = ... { error!() }`,
    // so all four exit paths below publish nothing anywhere.

    fn dead_slot() -> NetThreadDeadShared {
        Arc::new(std::sync::Mutex::new(None))
    }

    /// The headline #634 case: the thread panics and the agent must be able to SEE it, not infer it.
    #[test]
    fn net_thread_panic_publishes_an_explicit_terminal_reason() {
        let dead = dead_slot();
        let shutdown = AtomicBool::new(false);
        run_net_thread(&dead, &shutdown, || panic!("simulated eq-net panic"));
        let d = dead.lock().unwrap().clone().expect("a panicked net thread must publish");
        assert_eq!(d.end(), NetThreadEnd::Panicked,
            "the DISCRIMINANT must say which end this was — #890's consumers branch on it, and a \
             reason string that merely reads right is what they used to have to parse");
        let published = d.reason().to_string();
        assert!(published.contains("PANICKED"), "reason must name the panic: {published}");
        assert!(published.contains("simulated eq-net panic"),
            "the panic's own message must survive into the observable: {published}");
        assert!(published.contains("FROZEN"),
            "the reason must warn that the rest of the payload is stale: {published}");
    }

    /// A fatal `Err` (login retries exhausted / server-rejected create) ends the thread just as
    /// permanently as a panic, and was equally unobservable before #634 — it only reached a log line.
    #[test]
    fn net_thread_fatal_error_publishes_an_explicit_terminal_reason() {
        let dead = dead_slot();
        let shutdown = AtomicBool::new(false);
        run_net_thread(&dead, &shutdown, || Err("Login failed after 10 attempts".to_string()));
        let d = dead.lock().unwrap().clone().expect("a fatally-erroring net thread must publish");
        assert_eq!(d.end(), NetThreadEnd::Fatal);
        let published = d.reason().to_string();
        assert!(published.contains("Login failed after 10 attempts"),
            "the fatal error text must survive into the observable: {published}");
    }

    /// An `Ok` return with no shutdown requested is the "gameplay phase just... ended" case. It is
    /// still terminal, and must be published — and distinguishable from the clean-shutdown case.
    #[test]
    fn net_thread_unexpected_clean_return_is_published_and_distinguishable() {
        let dead = dead_slot();
        let shutdown = AtomicBool::new(false);
        run_net_thread(&dead, &shutdown, || Ok(()));
        let d = dead.lock().unwrap().clone().expect("an unexpectedly-returning net thread must publish");
        assert_eq!(d.end(), NetThreadEnd::ReturnedUnexpectedly);
        let published = d.reason().to_string();
        assert!(published.contains("WITHOUT a shutdown"), "reason: {published}");

        let dead2 = dead_slot();
        let shutdown2 = AtomicBool::new(true);
        run_net_thread(&dead2, &shutdown2, || Ok(()));
        let d2 = dead2.lock().unwrap().clone().expect("even a clean exit publishes the truth");
        assert_eq!(d2.end(), NetThreadEnd::ReturnedDuringShutdown,
            "the shutdown flag must select the DISCRIMINANT, not only the prose — collapsing these \
             two arms to one is how a `/v1/lifecycle/exit` body came to assert a lost session about \
             a shutdown someone asked for (#890)");
        let clean = d2.reason().to_string();
        assert!(clean.contains("shutdown was requested"), "reason: {clean}");
        assert_ne!(clean, published,
            "a requested shutdown and an unexplained return must not read identically");
    }

    /// `--testzone`'s pre-published state (#935 B4). This was written inline in `main()`, where no
    /// test could reach it: re-tagging it `Panicked` shipped fully GREEN, and that mis-tag makes
    /// `/v1/lifecycle/exit` warn about a LINKDEAD session on a server this process never connected
    /// to. Moving the value into `publish_never_started` is what makes the mutation RED here.
    ///
    /// Still NOT pinned by anything: that `main.rs` calls this on the `--testzone` branch at all.
    #[test]
    fn the_testzone_state_says_never_started_not_died() {
        let dead = dead_slot();
        publish_never_started(&dead);
        let d = dead.lock().unwrap().clone().expect("--testzone must publish a state, not null");
        assert_eq!(d.end(), NetThreadEnd::NeverStarted,
            "no thread ever ran, so no death discriminant is true here");
        let published = d.reason().to_string();
        assert!(published.contains("--testzone"), "reason must name the mode: {published}");
        assert!(published.contains("never started"),
            "the reason must say the thread never started, not that it ended: {published}");
        for died in ["PANICKED", "fatal error", "exited", "returned"] {
            assert!(!published.contains(died),
                "a thread that never started cannot have {died}: {published}");
        }
    }

    /// The slot must NOT be pre-written: a healthy, still-running thread has to read `null`, or the
    /// field would be a constant and could never discriminate anything.
    #[test]
    fn net_thread_slot_stays_none_while_the_body_is_still_running() {
        let dead = dead_slot();
        let d = dead.clone();
        std::thread::spawn(move || {
            let shutdown = AtomicBool::new(false);
            let observed = d.clone();
            run_net_thread(&d, &shutdown, move || {
                // Observed from INSIDE the body, i.e. while the thread is alive.
                assert!(observed.lock().unwrap().is_none(),
                    "net_thread_dead must be null while the net thread is running");
                Ok(())
            });
        })
        .join()
        .unwrap();
        assert!(dead.lock().unwrap().is_some(), "…and Some once it has ended");
    }

    /// A poisoned mutex must not swallow the death report — the one moment publishing matters most
    /// is also a moment other threads may have panicked while holding locks.
    #[test]
    fn net_thread_death_is_published_even_through_a_poisoned_mutex() {
        let dead = dead_slot();
        let d = dead.clone();
        let _ = std::thread::spawn(move || {
            let _g = d.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(dead.is_poisoned(), "precondition: the slot's mutex is poisoned");
        run_net_thread(&dead, &AtomicBool::new(false), || panic!("boom"));
        let published = dead.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(published.is_some_and(|d| d.reason().contains("PANICKED")),
            "a poisoned slot must still receive the death reason");
    }
}
