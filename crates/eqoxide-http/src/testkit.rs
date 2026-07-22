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

/// An `Instant` `secs` in the past (saturating — a just-booted host can't go below its epoch).
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

/// As [`empty_state`], but wired to a CALLER-OWNED `NetHealthShared` — the same `Arc` the network
/// thread's `EqStream` stamps. Exposed for #612's cross-crate test: eqoxide-net drives a REAL
/// `EqStream` into a REAL send failure and then asserts the failure is visible in THIS crate's
/// `/v1/observe/debug` JSON, i.e. that the fact reaches something the agent can poll rather than
/// merely being published into an internal struct.
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
        command,
        social: Default::default(),
        merchant_slots: Default::default(),
        inventory_slots: Default::default(),
        interact: Default::default(),
        chat: Default::default(),
        spells: std::sync::Arc::new(eqoxide_core::spells::SpellDb::default()),
        game_state: Arc::new(arc_swap::ArcSwap::from_pointee(eqoxide_core::game_state::GameState::new())),
        net_health: Arc::new(Mutex::new(crate::NetHealth::default())),
        frame_profile: Arc::new(Mutex::new(eqoxide_ipc::FrameProfile::default())),
        quest: Default::default(),
        group_slots: Default::default(),
        lifecycle,
        guild_slots: Default::default(),
        nav_debug_view: Default::default(),
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
