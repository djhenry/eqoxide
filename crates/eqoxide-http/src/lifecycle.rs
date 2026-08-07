//! `/v1/lifecycle/*` — session control: camp out (and optionally shut the client down).

use axum::{extract::State, http::StatusCode, routing::post, Router};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/camp", post(post_camp))
        .route("/exit", post(post_exit))
        .route("/respawn", post(post_respawn))
}

/// POST /v1/lifecycle/respawn — revive a slain character at its bind point. The client holds a dead
/// character in the slain state (it no longer auto-respawns) so an agent can inspect `dead` /
/// `killed_by` in /v1/observe/debug and recover its corpse before continuing; this releases it. A
/// no-op (but still 200) if the character isn't currently dead. (#284)
async fn post_respawn(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let dead = s.player().dead;
    s.command.request_respawn();
    if dead {
        tracing::info!("respawn: requested via POST /v1/lifecycle/respawn");
        (StatusCode::OK, "respawning at bind point".into())
    } else {
        (StatusCode::OK, "not currently dead (respawn will apply on the next death)".into())
    }
}

/// POST /v1/lifecycle/camp — toggle a camp. Starts a camp if none is running, or cancels the one in
/// progress (same as the HUD Camp button and the `/camp` chat keyword). A completed camp shuts the
/// client down cleanly with no linkdead; a cancel keeps the client in-world.
///
/// #477: guarded like the other WRITE commands. Camp is DRAINED by the gameplay net thread
/// (`camp_apply` → `camp_expired` → the shutdown flag in `gameplay.rs`); if that thread has exited
/// (the #470/#477 zombie) a camp would return 200 "then shutting down" that never happens — the exact
/// false-success class this fixes. Unlike `/v1/lifecycle/exit`, camp has NO watchdog to force the
/// shutdown, so a dead session must be reported honestly; use `/v1/lifecycle/exit` to tear a zombie
/// session down.
async fn post_camp(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let camping = s.lifecycle.camp_until.lock().unwrap().is_some();
    s.command.request_camp(CampCmd::Toggle);
    if camping {
        tracing::info!("camp: cancel requested via POST /v1/lifecycle/camp");
        (StatusCode::OK, "cancelling camp".into())
    } else {
        tracing::info!("camp: start requested via POST /v1/lifecycle/camp");
        (StatusCode::OK, "camping out (~30s), then shutting down".into())
    }
}

/// How long `post_exit`'s watchdog waits before force-exiting the process. It must outlast the camp
/// (CAMP_DURATION ≈ 30s) so it never force-kills mid-camp (which WOULD linkdead); 45s gives margin.
const EXIT_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(45);

/// `post_exit`'s 200 body on the healthy path: a camp really is going to be sent and drained.
const EXIT_BODY_CAMPING: &str = "camping out, then shutting down (~30s)";

/// `post_exit`'s 200 body when the net thread has already published its own death (#890).
///
/// The old code returned [`EXIT_BODY_CAMPING`] unconditionally, which on this path is a well-formed
/// false success: `CampCmd::Start` is drained by the gameplay/net thread, so with that thread gone
/// no OP_Camp is ever sent and the session ends as a linkdead drop — the exact outcome the endpoint
/// exists to avoid. The agent, having no independent channel to reality, had no way to tell that
/// 200 apart from a real camp-out. This body is held to the same standard the WRITE guard already
/// meets (see `require_live_session`): say what will actually happen, and say it explicitly.
const EXIT_BODY_NET_THREAD_DEAD: &str =
    "the network thread is dead — NO camp will be sent (nothing is left to drain the camp \
     request), so this shutdown will leave a LINKDEAD session on the server instead of a clean \
     camp-out, and this character will not be able to log straight back in the way a camped-out \
     one can. The process IS shutting down anyway: tearing a dead session down is what this \
     endpoint is for. See GET /v1/observe/debug (`net_thread_dead`).";

/// Which body `post_exit` returns, keyed on the one fact that decides whether a camp can be sent.
fn exit_body(net_thread_dead: bool) -> &'static str {
    if net_thread_dead { EXIT_BODY_NET_THREAD_DEAD } else { EXIT_BODY_CAMPING }
}

/// What the 45s watchdog actually caught, DERIVED from live state at the moment it fires rather
/// than hard-coded (#890).
///
/// The constant it replaces was `render-loop-wedged`, and that was measurably wrong. On a
/// dead-net-thread teardown the watchdog fires because nothing is left to DRAIN the camp, while the
/// render loop is perfectly healthy: in #890's log the renderer's wgpu submission index advanced at
/// ~46 per second straight through the watchdog's 45s window and past the forced exit. That reason
/// string is written into the durable crash record, so a later post-mortem reads it as fact about a
/// subsystem that never failed.
///
/// - `net_thread_dead` — the net thread published its own death, so the camp could never be sent.
///   Checked FIRST because it is both immediate and terminal, the same ordering (and the same
///   signal) `require_live_session` uses.
/// - `camp_undrained` — the camp command is still sitting in the `lifecycle.camp` slot, never
///   `take`n by `run_gameplay_phase`. The drainer stopped without publishing a death.
/// - otherwise — the camp WAS drained and the shutdown still did not complete, so something after
///   the camp is stuck. This build publishes no render-loop heartbeat or frame counter, so there is
///   nothing here that can show the render loop is the stuck part, and the reason does not claim
///   it. A non-committal true reason beats a specific false one.
///
/// NOT MEASURED: whether the watchdog ever fires on that third branch at all, and in particular
/// whether it ever fires with a genuinely wedged render loop. Every measurement in #890 comes from
/// the dead-net-thread path.
fn watchdog_reason(net_thread_dead: bool, camp_undrained: bool) -> &'static str {
    if net_thread_dead {
        "net-thread-dead"
    } else if camp_undrained {
        "camp-not-drained"
    } else {
        "watchdog-shutdown-timeout"
    }
}

/// Sample the two live signals [`watchdog_reason`] keys on. Deliberately read when the watchdog
/// FIRES, not when the request arrives: the reason must describe what is stuck at kill time, and a
/// net thread can die during the 45s wait.
fn watchdog_reason_now(s: &HttpState) -> &'static str {
    let net_thread_dead =
        s.net_thread_dead.lock().unwrap_or_else(|e| e.into_inner()).is_some();
    let camp_undrained =
        s.lifecycle.camp.lock().unwrap_or_else(|e| e.into_inner()).is_some();
    watchdog_reason(net_thread_dead, camp_undrained)
}

/// The whole watchdog: wait, then sample the reason and hand it to `kill`.
///
/// `kill` is a parameter purely so a test can observe the reason this actually passes to
/// `eqoxide_crash::exit`. Production hands it the real killer, which never returns. Without this
/// seam the reason could only be pinned on the helper functions, and a mutation that re-hard-coded
/// the reason AT THE CALL SITE would slip past every test — the deletion-only-mutation blind spot
/// tracked as eqoxide#799. Here the call site is inside the tested function.
async fn run_exit_watchdog(
    s: HttpState,
    delay: std::time::Duration,
    kill: impl FnOnce(&'static str),
) {
    tokio::time::sleep(delay).await;
    // Label it (#380): an unlabelled exit(0) here would leave a post-mortem with "no
    // clean-shutdown record, no panic, no signal, fresh heartbeat" — which the crash module
    // documents as meaning OOM-kill, so a watchdog kill would be confidently misreported as an
    // OOM. Labelling it is still the whole point; #890 is about the label also being TRUE.
    let reason = watchdog_reason_now(&s);
    tracing::warn!(
        "exit: watchdog timeout after {}s — forcing process exit (reason={reason})",
        delay.as_secs()
    );
    kill(reason);
}

/// POST /v1/lifecycle/exit — camp out, then cleanly shut down. Requests a camp (`CampCmd::Start`,
/// idempotent): the gameplay loop sends OP_Camp, stays connected ~30s for EQEmu's camp timer to set
/// `instalog`, then sets the shutdown flag so the disconnect leaves NO linkdead ghost (instant
/// re-login). The render loop's `about_to_wait` then exits the winit event loop on the MAIN thread
/// and the process exits via `main`.
///
/// Deliberately NOT gated by `require_live_session`, unlike every other WRITE handler: tearing a
/// zombie session down is this endpoint's job (`post_camp`'s doc nominates it for exactly that) and
/// the watchdog below force-exits even when nothing can drain the camp. #890 did not change that.
/// What it changed is that the endpoint used to DESCRIBE the healthy path unconditionally — a 200
/// promising a camp-out on a net thread that had already published its own death. The body now
/// branches (see [`exit_body`]); the teardown proceeds identically in both cases.
///
/// The watchdog is a last resort; see [`EXIT_WATCHDOG`] for its duration and [`watchdog_reason`]
/// for why its recorded reason is derived rather than constant.
async fn post_exit(State(s): State<HttpState>) -> (StatusCode, &'static str) {
    let net_thread_dead =
        s.net_thread_dead.lock().unwrap_or_else(|e| e.into_inner()).is_some();
    tracing::info!(
        "exit: camp-and-shutdown requested via POST /v1/lifecycle/exit (net_thread_dead={net_thread_dead})"
    );
    s.command.request_camp(CampCmd::Start);
    tokio::spawn(run_exit_watchdog(s.clone(), EXIT_WATCHDOG, |reason| {
        eqoxide_crash::exit(reason, 0)
    }));
    (StatusCode::OK, exit_body(net_thread_dead))
}

#[cfg(test)]
mod tests {
    use super::{
        router, run_exit_watchdog, watchdog_reason, watchdog_reason_now, EXIT_BODY_CAMPING,
        EXIT_WATCHDOG,
    };
    use crate::testkit::empty_state;
    use crate::HttpState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// Drive the real route (not just the helper) and hand back its 200 body. Going through the
    /// router is the point: a branch that exists but is not wired to the handler would still pass a
    /// helper-only test. The 45s watchdog task this spawns is aborted when the test's runtime drops.
    async fn post_exit_body(state: HttpState) -> String {
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/exit").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "the teardown must still be ACCEPTED");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn mark_net_thread_dead(state: &HttpState) {
        *state.net_thread_dead.lock().unwrap() =
            Some("the eq-net thread PANICKED (synthetic, for this test) — …".to_string());
    }

    /// #890's first falsehood. `CampCmd::Start` is drained by the gameplay/net thread; with that
    /// thread dead no OP_Camp is ever sent, so the old unconditional "camping out, then shutting
    /// down (~30s)" was a well-formed false success on the one endpoint an agent uses to end a
    /// session. The body must now say what actually happens — and the teardown must still proceed,
    /// because tearing the zombie down is this endpoint's job.
    #[tokio::test]
    async fn exit_on_a_dead_net_thread_does_not_promise_a_camp_it_cannot_send() {
        let state = empty_state();
        mark_net_thread_dead(&state);
        let camp_slot = state.lifecycle.camp.clone();

        let body = post_exit_body(state).await;

        assert_ne!(body, EXIT_BODY_CAMPING, "the healthy-path promise must not be reused here");
        assert!(!body.contains("camping out"), "must not claim a camp is happening: {body:?}");
        assert!(body.contains("NO camp will be sent"), "must say the camp cannot be sent: {body:?}");
        assert!(body.contains("LINKDEAD"), "must say the shutdown linkdeads: {body:?}");
        assert!(
            body.contains("net_thread_dead"),
            "must name the field an agent can check: {body:?}"
        );
        assert!(
            camp_slot.lock().unwrap().is_some(),
            "the teardown must still be requested — #890 changes the WORDING, not the behaviour"
        );
    }

    /// The other direction: on a live session the endpoint really does camp, and must keep saying
    /// so. Without this, "always warn about linkdead" would pass the test above.
    #[tokio::test]
    async fn exit_on_a_live_session_still_promises_the_camp() {
        let body = post_exit_body(empty_state()).await;
        assert_eq!(body, EXIT_BODY_CAMPING);
    }

    /// #890's second falsehood, pinned at the mapping. The watchdog fired on a dead net thread and
    /// recorded `render-loop-wedged` while the renderer was measured at ~46fps throughout. No input
    /// may now produce that string, and each state must get its own reason.
    #[test]
    fn the_watchdog_reason_names_the_stuck_subsystem_and_never_blames_the_render_loop() {
        assert_eq!(watchdog_reason(true, true), "net-thread-dead");
        assert_eq!(watchdog_reason(true, false), "net-thread-dead");
        assert_eq!(watchdog_reason(false, true), "camp-not-drained");
        // Nothing observable here says the render loop stalled, so nothing here claims it did.
        assert_eq!(watchdog_reason(false, false), "watchdog-shutdown-timeout");
        for (dead, undrained) in [(true, true), (true, false), (false, true), (false, false)] {
            assert_ne!(
                watchdog_reason(dead, undrained), "render-loop-wedged",
                "no reachable state may assert a render-loop wedge (net_thread_dead={dead}, \
                 camp_undrained={undrained})"
            );
        }
    }

    /// The reason must be DERIVED when the watchdog fires, from the same slots production reads —
    /// not captured at request time and not constant. Each transition below moves it, which a
    /// constant (of any value) cannot do.
    #[tokio::test]
    async fn the_watchdog_reason_tracks_live_state_after_the_request_was_accepted() {
        let state = empty_state();
        let body = post_exit_body(state.clone()).await;
        assert_eq!(body, EXIT_BODY_CAMPING, "precondition: this request was accepted as healthy");

        // The camp is queued and no drainer exists in this fixture, so the camp slot is still full.
        assert_eq!(watchdog_reason_now(&state), "camp-not-drained");

        // The net thread dies DURING the 45s wait: the more specific, terminal reason takes over.
        mark_net_thread_dead(&state);
        assert_eq!(watchdog_reason_now(&state), "net-thread-dead");

        // Camp drained and the net thread alive again: nothing observable is stuck, so the reason
        // is honestly non-committal rather than naming a subsystem.
        *state.net_thread_dead.lock().unwrap() = None;
        state.lifecycle.camp.lock().unwrap().take();
        assert_eq!(watchdog_reason_now(&state), "watchdog-shutdown-timeout");
    }

    /// The reason the watchdog HANDS TO THE KILLER, from inside the real watchdog body, after the
    /// real 45s wait has elapsed (in virtual time). The tests above pin the helpers; this one pins
    /// the call site, so re-hard-coding the reason where it is actually used cannot survive.
    #[tokio::test(start_paused = true)]
    async fn the_watchdog_kills_with_the_reason_it_derived_not_a_hard_coded_one() {
        let state = empty_state();
        // #890's measured state EXACTLY: the net thread has published its death AND `post_exit` has
        // queued a camp that nothing will ever drain. Both signals are set, so this also pins that
        // the terminal one wins — the ordering `require_live_session` uses.
        mark_net_thread_dead(&state);
        state.command.request_camp(eqoxide_ipc::CampCmd::Start);

        let killed_with = std::sync::Arc::new(std::sync::Mutex::new(None::<&'static str>));
        let sink = killed_with.clone();
        run_exit_watchdog(state, EXIT_WATCHDOG, move |reason| {
            *sink.lock().unwrap() = Some(reason);
        })
        .await;

        assert_eq!(
            *killed_with.lock().unwrap(),
            Some("net-thread-dead"),
            "the crash record must name what was actually stuck; #890 measured the render loop at \
             ~46fps while this exit recorded `render-loop-wedged`"
        );
    }

    /// The same seam on the other state, so "always say net-thread-dead" cannot pass the test above.
    #[tokio::test(start_paused = true)]
    async fn the_watchdog_reports_an_undrained_camp_on_a_net_thread_that_never_died() {
        let state = empty_state();
        // A camp was requested and no drainer took it — the net thread stopped without publishing.
        state.command.request_camp(eqoxide_ipc::CampCmd::Start);

        let killed_with = std::sync::Arc::new(std::sync::Mutex::new(None::<&'static str>));
        let sink = killed_with.clone();
        run_exit_watchdog(state, EXIT_WATCHDOG, move |reason| {
            *sink.lock().unwrap() = Some(reason);
        })
        .await;

        assert_eq!(*killed_with.lock().unwrap(), Some("camp-not-drained"));
    }
}
