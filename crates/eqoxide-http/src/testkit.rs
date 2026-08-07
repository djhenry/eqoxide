//! Test fixtures shared by this crate's own unit tests AND downstream (app-crate) integration
//! tests. Gated on `any(test, feature = "test-fixtures")` so it never ships in a release build.
//!
//! The app crate enables `eqoxide-http/test-fixtures` in its `[dev-dependencies]` so
//! `tests/http_observe_apply.rs` — the relocated `apply_consider` / `apply_death` → `/observe/debug`
//! tests, which need the app crate's `eq_net::packet_handler` (an app-layer module that sits ABOVE
//! this crate) — can build an [`HttpState`] and drive the observe router from outside this crate.
//!
//! These are the exact `#[cfg(test)]` helpers that used to live in `quests::tests` (`ago`,
//! `set_gs`, `empty_state`); they are simply hoisted here and re-gated so a downstream test build
//! can reach them. No behavior change.
#![allow(private_interfaces)]

use std::sync::{Arc, Mutex};

use crate::HttpState;

/// An `Instant` `secs` in the past, on the WALL clock.
///
/// Fine for stamps that are aged against `Instant::now()` at read time — the asset-sync publisher's
/// `tick_stamped`, for instance. **Not** for a `NetHealth` stamp: `health()` ages those against
/// `NetHealth::clock`, which a fixture pins, so a wall-clock stamp read back against it drifts by
/// however long the test took (#760). Use `h.clock.ago(secs)` there. That ban is **partially**
/// guarded by `observe::tests::no_past_dated_net_health_stamp_is_taken_from_a_clock_other_than_the_
/// one_that_reads_it` — a source scan over four files, one statement at a time. It catches the
/// rustfmt-wrapped and struct-literal spellings. It does **not** catch a stamp aliased through a
/// local, nor one whose value is produced inside a nested block or a `match` arm (the scan cuts
/// statements at `{` and `}`, which separates the field from the call), nor anything in a file it
/// does not scan. Read that test's doc for the full measured list before relying on it: a wrong
/// spelling it misses still compiles and still lands.
pub fn ago(secs: u64) -> std::time::Instant {
    std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(secs))
        .expect("monotonic clock older than the test window")
}

/// Mutate the network thread's published `GameState` — the single source of truth every
/// agent-facing player field is projected from (#343). Tests that used to poke `player_info`
/// directly now seed the snapshot the network thread would have published.
pub fn set_gs(state: &HttpState, f: impl FnOnce(&mut eqoxide_core::game_state::GameState)) {
    let mut gs = (**state.game_state.load()).clone();
    f(&mut gs);
    state.game_state.store(Arc::new(gs));
}

/// As [`empty_state`], but with the health clock left on the REAL WALL CLOCK (#760).
///
/// For the small, deliberate set of tests whose SUBJECT is the clock: the #343 read-time-derivation
/// guards, which stamp a stale source and then assert the projected age is genuinely `>= N` seconds,
/// or that it CLIMBS between two reads with no publisher running. Those properties are meaningless
/// against a pinned clock — an age that cannot move cannot be shown to move — so they opt out here,
/// explicitly and visibly, rather than the whole fixture staying wall-driven for their sake.
///
/// **Everything else must use [`empty_state`].** A handler test on a wall clock is racing
/// `SESSION_STALE_TICK_MS`: if the machine puts 5s between this call and the request, the handler
/// answers `503 stale session` instead of whatever the test asserted (#760). There are exactly seven
/// call sites of this function, all in `observe`'s clock tests.
///
/// If you are adding an eighth, there are **two** shapes to tell apart, and the earlier version of
/// this rule named only the first — which is precisely how a load-fragile test got past review:
///
/// 1. **An age that must MOVE** (`assert!(second > first)` across two reads). Genuinely needs the
///    wall clock: an age that cannot move cannot be shown to move. This function is for these.
/// 2. **An age that must EXCEED A BOUND** (`assert!(age >= N)`, or a stamp placed past a timeout so
///    a verdict fires). These do **not** need the wall clock, and must not use it — stay on
///    [`empty_state`] and derive the stamp from the fixture's own clock (`let c = h.clock;
///    h.last_probe_sent = Some(c.ago(15));`). A wall-clock `ago(15)` read back against a pinned
///    clock ages to `15s − (time since the fixture was built)`, so the margin over the bound is
///    eaten by machine load — #760's failure mode, one level down.
///
/// Shape 2 is documented here and **partially** guarded by
/// `observe::tests::no_past_dated_net_health_stamp_is_taken_from_a_clock_other_than_the_one_that_reads_it`,
/// which names its own measured blind spots. Do not read it as making the mistake unlandable.
pub fn empty_state_wall_clock() -> HttpState {
    let state = empty_state();
    // `NetHealth::default()` is the production constructor: `HealthClock::WALL`, all three stamps at
    // `Instant::now()`. This is exactly what `empty_state` used to hand every test.
    *state.net_health.lock().unwrap() = crate::NetHealth::default();
    state
}

/// As [`empty_state`], but wired to a CALLER-OWNED `NetHealthShared` — the same `Arc` the network
/// thread's `EqStream` stamps. Exposed for #612's cross-crate test: eqoxide-net drives a REAL
/// `EqStream` into a REAL send failure and then asserts the failure is visible in THIS crate's
/// `/v1/observe/debug` JSON, i.e. that the fact reaches something the agent can poll rather than
/// merely being published into an internal struct.
///
/// **This un-pins the health clock (#760/N3).** [`empty_state`] hands out a `NetHealth` frozen at
/// construction so a handler test cannot race `SESSION_STALE_TICK_MS`; the caller-owned `Arc`
/// replaces that wholesale, and callers build theirs with `NetHealth::default()`, i.e. the WALL
/// clock. That is correct for #612's purpose — the subject is a real `EqStream` stamping real
/// times — but it means a stamp written here is aged against the wall clock, so the two shapes in
/// [`empty_state_wall_clock`]'s doc apply to this function too, and the `observe` guard does not
/// scan the caller's crate. `crates/eqoxide-net/src/transport.rs:2679` past-dates
/// `last_send_pressure_at` through this path today; it is correct precisely because the clock is
/// the wall clock, which is the fact this paragraph exists to keep true.
pub fn empty_state_with_net_health(net_health: eqoxide_ipc::NetHealthShared) -> HttpState {
    HttpState { net_health, ..empty_state() }
}

/// As [`empty_state`], but wired to a CALLER-OWNED `net_thread_dead` slot — the same `Arc` the
/// `eq-net` thread's `run_net_thread` wrapper writes on its way out (#634). Exposed for the
/// cross-crate proof: the app crate panics a REAL thread through the REAL production wrapper and
/// then asserts the death is visible in THIS crate's `/v1/observe/debug` JSON, i.e. that it reaches
/// something an agent can poll rather than merely landing in an internal field. Same shape, and same
/// reason, as [`empty_state_with_net_health`] above (#612).
pub fn empty_state_with_net_thread_dead(
    net_thread_dead: Arc<Mutex<Option<String>>>,
) -> HttpState {
    HttpState { net_thread_dead, ..empty_state() }
}

/// The shared world tables (`entity_positions` / `entity_ids` / `entity_poses`) behind an
/// [`HttpState`]. `HttpState.world` is private, so downstream integration tests that need to seed
/// the roster the way the net thread's `sync_entities` does go through this (#643).
pub fn world_slots(state: &HttpState) -> eqoxide_ipc::WorldSlots {
    state.world.clone()
}

/// The asset-sync registry behind an [`HttpState`] (#715). `HttpState.asset_sync` is private, so
/// tests that need to drive the endpoint the way a loader thread would go through this — and get
/// the SAME `Arc` the state holds, which is the point: it proves the handler re-reads the live cell
/// per request rather than serving a snapshot taken when the state was built. Tests write it the
/// only way production does, through an `eqoxide_ipc::AssetSyncGuard`.
pub fn asset_sync_slot(state: &HttpState) -> eqoxide_ipc::AssetSyncShared {
    state.asset_sync.clone()
}

pub fn empty_state() -> HttpState {
    // `CameraSlots` has no `Default` impl (`CameraSnapshot`'s fields aren't Default-able), so
    // it's built by hand; every other bundle is plain `Default::default()`. `nav`, `camera`, and
    // `lifecycle` are bound to locals FIRST (rather than inlined) so `command` below can be
    // built from `.clone()`s of the SAME Arcs — mirroring the shared-identity wiring `main.rs`
    // does for real, and required now that nav/camera/lifecycle route their writes through
    // `command` (#459): an independently-`Default`-constructed `command.nav`/etc. would silently
    // diverge from the `state.nav`/etc. a test reads back.
    let camera = eqoxide_ipc::CameraSlots {
        cmd_tx: Arc::new(Mutex::new(None)),
        snapshot: Arc::new(Mutex::new(eqoxide_ipc::CameraSnapshot {
            mode: eqoxide_ipc::CameraMode::AutoFollow,
            azimuth: 0.0,
            elevation: 0.0,
            radius: 0.0,
            focus: [0.0, 0.0, 0.0],
            eye: [0.0, 0.0, 0.0],
            occluded: false,
            still_blocked: false,
            // #867: `None` = "no frame has been drawn", the same honest state `main.rs` seeds
            // before the event loop exists. A test that needs a drawn-looking snapshot sets these.
            drawn_frame: None,
            drawn_at: None,
        })),
        frame_req: Arc::new(Mutex::new(None)),
        manual_move: Arc::new(Mutex::new(None)),
    };
    let nav: eqoxide_ipc::NavSlots = Default::default();
    let lifecycle: eqoxide_ipc::LifecycleSlots = Default::default();
    let command = eqoxide_command::CommandState::new(
        Default::default(), Default::default(), Default::default(), Default::default(),
        Default::default(), Default::default(), Default::default(), Default::default(),
        Default::default(), Default::default(),
        nav.clone(), lifecycle.clone(),
    );
    HttpState {
        camera,
        nav,
        world: Default::default(),
        shared_collision: Arc::new(std::sync::RwLock::new(None)),
        // Handler tests are about the handler, not about zone loading: default to a state that
        // does NOT gate world endpoints. Tests that exercise the #579 gate set it explicitly.
        zone_assets: Arc::new(Mutex::new(eqoxide_nav::zone_assets::ZoneAssetState::test_ready())),
        // #616: healthy by default, same as `zone_assets` above — tests that exercise a worker
        // failure set these explicitly.
        common_assets_failed: Arc::new(Mutex::new(None)),
        model_sync_dead: Arc::new(Mutex::new(None)),
        // #634: healthy by default — the net thread is alive. Tests that exercise its death set it.
        net_thread_dead: Arc::new(Mutex::new(None)),
        // #715: no sync in progress by default. Tests that exercise the endpoint open a guard on
        // `asset_sync_slot(&state)` — the SAME Arc — rather than rebuilding the state.
        asset_sync: eqoxide_ipc::asset_sync::new_shared(),
        command,
        social: Default::default(),
        merchant_slots: Default::default(),
        inventory_slots: Default::default(),
        interact: Default::default(),
        chat: Default::default(),
        spells: std::sync::Arc::new(eqoxide_core::spells::SpellDb::default()),
        game_state: Arc::new(arc_swap::ArcSwap::from_pointee(eqoxide_core::game_state::GameState::new())),
        // #760: a FROZEN health clock, stamped at the same instant it is pinned to, so every age
        // `HttpState::health()` projects for this fixture is exactly 0 — permanently, and whatever
        // the machine is doing. With `NetHealth::default()` (the real wall clock) the ages were
        // "however long this test has been running", so a handler test that took >5s to reach its
        // request got `503 stale session` from `require_live_session` instead of the status it
        // asserted. That is #760, and it fired on a diff that had nothing to do with it.
        // Tests that WANT a stale session build their own `NetHealth::frozen_at(now, …)`.
        net_health: Arc::new(Mutex::new(crate::NetHealth::frozen_at(
            std::time::Instant::now(), 0, 0, 0,
        ))),
        frame_profile: Arc::new(Mutex::new(eqoxide_ipc::FrameProfile::default())),
        quest: Default::default(),
        group_slots: Default::default(),
        lifecycle,
        guild_slots: Default::default(),
        nav_debug_view: Default::default(),
        skin_cap_downgrades: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
    }
}

/// GET `/debug` against a throwaway observe router built from `state`, decoded to JSON. Exposed so a
/// downstream integration test can assert the observe projection without needing the crate-private
/// `observe` module or naming `HttpState`.
pub async fn debug_json(state: HttpState) -> serde_json::Value {
    observe_json(state, "/debug").await
}

/// GET an arbitrary observe `path` (e.g. `"/entities?labeled=1"`) against a throwaway REAL observe
/// router built from `state`, decoded to JSON. Same rationale as [`debug_json`]: a downstream
/// integration test must be able to assert on the ACTUAL serialized HTTP body, not on an internal
/// struct that might never reach the wire (#643).
pub async fn observe_json(state: HttpState, path: &str) -> serde_json::Value {
    use tower::ServiceExt;
    let app = crate::observe::router().with_state(state);
    let resp = app
        .oneshot(axum::http::Request::get(path).body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Reach control for `observe::tests::no_past_dated_net_health_stamp_is_taken_from_a_clock_other_than_the_one_that_reads_it` (#760/C1).
///
/// That guard scans this file's source text. A scanner that silently stops early — as it did, when
/// a `/*` inside a route glob in a doc comment latched its block-comment state — reports
/// a clean scan of a corpus it never read, which is a confident falsehood. The guard asserts it can
/// SEE this constant; because it is the last item in the file, seeing it proves the scan arrived at
/// the end. **Keep it last.**
#[cfg(test)]
pub(crate) const GUARD_REACH_SENTINEL_TESTKIT: u8 = 0;
