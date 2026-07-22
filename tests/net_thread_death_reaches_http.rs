//! #634 (agent-honesty): a panicking `eq-net` thread must become a field an AGENT can read.
//!
//! This is the end-to-end proof, deliberately built to defeat the failure mode that broke #616's
//! first attempt: there, the death was published honestly *inside* the app and never wired to
//! `HttpState`, so the agent still saw nothing. Asserting on an internal struct would have passed
//! that broken version. So this test spans both halves and asserts only on the REAL JSON body:
//!
//!   1. a REAL `std::thread` runs the REAL production wrapper (`eqoxide::model::run_net_thread`),
//!      the exact function `src/main.rs` spawns `eq-net` through, with a body that panics;
//!   2. the SAME `Arc` that thread wrote is the one handed to `HttpState`, mirroring `main.rs`'s
//!      single-construction wiring (a second `Arc` would sever them and this test would fail —
//!      which is the point);
//!   3. the assertion is on the decoded body of `GET /v1/observe/debug` served by the real axum
//!      observe router.
//!
//! It lives in the APP crate because `run_net_thread` is app-layer (`src/model.rs`, above
//! `eqoxide-http` in the crate graph) while the `HttpState` builder + `/debug` driver come from
//! `eqoxide-http`'s `test-fixtures`-gated `testkit` — the same split as `http_observe_apply.rs`.
//!
//! On unmodified `origin/main` this file cannot even compile: `run_net_thread`,
//! `NetThreadDeadShared`, `empty_state_with_net_thread_dead` and the `net_thread_dead` JSON key all
//! come from this change. See the PR body for the standalone reproduction of main's actual control
//! flow (bare closure, `if let Err(e) = … { error!() }`, nothing published on any path).

use eqoxide::http::testkit::{debug_json, empty_state_with_net_thread_dead};
use eqoxide::model::{run_net_thread, NetThreadDeadShared};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Run `body` on a real thread through the real wrapper, exactly as `main.rs` does, and hand back
/// the shared slot it published into.
fn run_net_thread_to_completion(
    body: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> NetThreadDeadShared {
    let dead: NetThreadDeadShared = Arc::new(Mutex::new(None));
    let d = dead.clone();
    std::thread::Builder::new()
        .name("eq-net-test".into())
        .spawn(move || {
            let shutdown = AtomicBool::new(false);
            run_net_thread(&d, &shutdown, body);
        })
        .expect("spawn")
        .join()
        .expect("the wrapper must CATCH the panic, not let it escape the thread");
    dead
}

#[tokio::test]
async fn a_panicking_eq_net_thread_is_visible_at_v1_observe_debug() {
    let dead = run_net_thread_to_completion(|| panic!("simulated eq-net panic"));

    let j = debug_json(empty_state_with_net_thread_dead(dead)).await;

    let reported = j["net_thread_dead"]
        .as_str()
        .unwrap_or_else(|| panic!("net_thread_dead must be a reason string, got {:?}", j["net_thread_dead"]));
    assert!(reported.contains("PANICKED"), "reason: {reported}");
    assert!(reported.contains("simulated eq-net panic"), "reason: {reported}");

    // The rest of the payload is unchanged and still fully plausible — that is precisely why the
    // field above has to exist. Pinning it here stops a future "fix" from claiming honesty by
    // blanking the world instead of labelling it.
    assert!(j["player"].is_object(), "the frozen world is still served");
}

/// A LIVE net thread must read `null` here. Without this, a field that was accidentally hard-wired
/// to a constant reason would pass the test above while discriminating nothing.
#[tokio::test]
async fn a_live_eq_net_thread_reports_null() {
    let dead: NetThreadDeadShared = Arc::new(Mutex::new(None));
    let j = debug_json(empty_state_with_net_thread_dead(dead)).await;
    assert_eq!(j["net_thread_dead"], serde_json::Value::Null);
}

/// A fatal `Err` return — login retries exhausted, a server-rejected create — ends the thread just
/// as permanently as a panic. Pre-#634 it reached only a `tracing::error!` line, which no HTTP
/// caller can see.
#[tokio::test]
async fn a_fatally_erroring_eq_net_thread_is_visible_at_v1_observe_debug() {
    let dead = run_net_thread_to_completion(|| Err("Login failed after 10 attempts".to_string()));
    let j = debug_json(empty_state_with_net_thread_dead(dead)).await;
    assert!(
        j["net_thread_dead"].as_str().unwrap().contains("Login failed after 10 attempts"),
        "reason: {}", j["net_thread_dead"]
    );
}
