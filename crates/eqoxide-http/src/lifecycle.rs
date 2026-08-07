//! `/v1/lifecycle/*` — session control: camp out (and optionally shut the client down).

use axum::{extract::State, http::StatusCode, routing::post, Router};
use eqoxide_ipc::{NetThreadDeath, NetThreadEnd};
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

/// `post_exit`'s 200 body when the net thread ENDED without anyone having asked it to — a panic, a
/// fatal error, or the gameplay phase returning on its own (#890).
///
/// The original code returned [`EXIT_BODY_CAMPING`] unconditionally, which on this path is a
/// well-formed false success: `CampCmd::Start` is drained by the gameplay/net thread, so with that
/// thread gone no OP_Camp is ever sent. The agent, having no independent channel to reality, had no
/// way to tell that 200 apart from a real camp-out. This body is held to the same standard the WRITE
/// guard already meets (see `require_live_session`): say what will actually happen, say it
/// explicitly, and relay the published reason verbatim rather than paraphrasing it.
///
/// What it deliberately does NOT say (#935 review):
/// - It does not claim the shutdown *causes* the linkdead. `impl Drop for EqStream` only calls
///   `abandon_outstanding()` — no `OP_SessionDisconnect` is sent — so when the thread dies the
///   socket simply goes quiet and the server declares the drop on its own timer. In #890's own log
///   that happened three minutes BEFORE the exit request. This teardown inherits that outcome.
/// - It does not say anything about whether the character can log straight back in. Nobody has
///   measured that, from either side: this repo's `gameplay.rs` states as tracked fact that "the
///   brief linkdead window is harmless because the next login DropClient-kicks the ghost", and an
///   earlier revision of THIS body asserted the opposite. Neither claim was measured, so the body
///   makes neither.
/// - The linkdead sentence is CONDITIONAL because [`NetThreadEnd::Fatal`] reaches this body too, and
///   that end comes from `run_login_flow`'s `Err` arms — i.e. before the gameplay phase, when there
///   may be no session on the server at all.
const EXIT_BODY_SESSION_LOST: &str =
    "NO camp will be sent (nothing is left to drain the camp request), so this shutdown cannot end \
     the session with a clean camp-out. If a session was still live when the thread ended, it was \
     already lost at that moment: the server declares the LINKDEAD drop on its own timer — this \
     shutdown inherits that outcome rather than causing it. The process IS being torn down anyway: \
     tearing a dead session down is what this endpoint is for. With nothing to drain the camp, the \
     process exits via the 45s watchdog rather than the usual ~30s.";

/// `post_exit`'s 200 body when a shutdown was ALREADY requested and the net thread has already
/// finished and exited ([`NetThreadEnd::ReturnedDuringShutdown`], #935 review B2b).
///
/// `post_exit` is documented as idempotent, so a retry is legitimate, and a retry in this window
/// must not be answered with [`EXIT_BODY_SESSION_LOST`]: nothing was lost — a shutdown was asked
/// for and is happening. Nor may it be answered with [`EXIT_BODY_CAMPING`], because THIS request's
/// camp will not be drained by anything.
const EXIT_BODY_SHUTDOWN_UNDER_WAY: &str =
    "a shutdown was ALREADY requested and the network thread has already exited. This request \
     queues a camp that nothing is left to drain, so it sends no OP_Camp of its own; it starts no \
     second shutdown and changes no outcome. Whether this session ended with a clean camp-out was \
     decided by the earlier shutdown, not by this request. The process is already on its way out.";

/// `post_exit`'s 200 body in `--testzone`, where the net thread was never spawned
/// ([`NetThreadEnd::NeverStarted`], #935 review B2c).
///
/// `spawn_camera_server` is called unconditionally, so this endpoint is reachable in offline
/// renderer mode. [`EXIT_BODY_SESSION_LOST`] would be false here in the most basic way: it warns
/// about a LINKDEAD session on a server this process never connected to.
const EXIT_BODY_NO_NET_THREAD: &str =
    "there is no network thread in this process, so there is no server session: no camp is sent, \
     nothing is left LINKDEAD, and no character is logged out because none is logged in. The \
     process IS being torn down; with nothing to drain the camp it exits via the 45s watchdog \
     rather than the usual ~30s.";

/// Which body `post_exit` returns. Keyed on WHICH terminal state the net thread reached, not on
/// whether it reached one at all (#890/#935): `net_thread_dead.is_some()` conflates a panicked
/// thread, a thread that exited as part of a shutdown someone asked for, and a thread that was
/// never started — and the three bodies above are each false about the other two states.
///
/// The published reason is relayed verbatim, exactly as `require_live_session` relays it, so the
/// agent gets the thread's own words rather than this module's paraphrase of them.
fn exit_body(death: Option<&NetThreadDeath>) -> String {
    let Some(death) = death else { return EXIT_BODY_CAMPING.to_string() };
    // Exhaustive ON PURPOSE — no `_` arm. Adding a sixth `NetThreadEnd` must be a compile error
    // here, not a silent inheritance of the session-lost wording, which is the mistake this whole
    // function exists to undo.
    let head = match death.end() {
        NetThreadEnd::NeverStarted           => EXIT_BODY_NO_NET_THREAD,
        NetThreadEnd::ReturnedDuringShutdown => EXIT_BODY_SHUTDOWN_UNDER_WAY,
        NetThreadEnd::Panicked
        | NetThreadEnd::Fatal
        | NetThreadEnd::ReturnedUnexpectedly => EXIT_BODY_SESSION_LOST,
    };
    format!("{head} The network thread reports: {} See GET /v1/observe/debug (`net_thread_dead`).",
            death.reason())
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
/// The net-thread state is read as WHICH end it was, never as `is_some()` — same reason
/// [`exit_body`] does: `net-thread-dead` is a false label for a thread that was never started, and
/// for one that exited because a shutdown was requested.
///
/// - [`NetThreadEnd::NeverStarted`] → `net-thread-never-started`. `--testzone`; nothing died.
/// - a panic / fatal error / unexpected return → `net-thread-dead`. The drainer is gone, so the camp
///   could never be sent. Checked before the camp slot because it is both immediate and terminal —
///   the same ordering (and the same signal) `require_live_session` uses.
/// - [`NetThreadEnd::ReturnedDuringShutdown`] → `shutdown-not-completed`. The net thread ended as
///   part of a requested shutdown and the process was STILL alive 45s later, so whatever is stuck is
///   downstream of the net thread. Naming it that, rather than `net-thread-dead`, is the difference
///   between reporting a fact and blaming a subsystem that did its job.
/// - `camp_undrained` (net thread still running) → `camp-not-drained`. The camp command is still
///   sitting in the `lifecycle.camp` slot, never `take`n by `run_gameplay_phase`. The drainer
///   stopped without publishing a death — a wedge rather than a death.
/// - otherwise → `watchdog-shutdown-timeout`. The camp was drained (or never queued) and the
///   shutdown still did not complete, so something after the camp is stuck.
///
/// ## Why the render loop is never named, even though a frame counter exists
///
/// This build DOES publish a render-loop frame counter, and it is readable from here.
/// `EqRenderer::frames_drawn` ("count of `render_frame` calls that have completed, process-lifetime")
/// is handed out as `DrawnFrame::index` and published every drawn frame into
/// `CameraSnapshot::drawn_frame` / `drawn_at` (#867/#878), served as `camera.drawn_frame` /
/// `camera.drawn_age_ms`, and `observe.rs` documents it as the render-staleness signal. It lives in
/// `HttpState.camera`, the same struct [`watchdog_reason_now`] already takes.
///
/// It is not used here, and the reason is NOT that it is missing — an earlier revision of this doc
/// said "this build publishes no render-loop heartbeat or frame counter", which was false when it
/// was written (#867/#878 were already in this branch's own history). The real reason is that a
/// stalled counter does not mean a wedge: `App::about_to_wait` deliberately renders NOTHING once
/// `active_until` lapses, so a healthy idle loop and a wedged one look identical through this
/// counter. On the dead-net-thread path that is not even a corner case — `camp_until` is set by the
/// gameplay thread, so with that thread gone nothing keeps the loop active and a perfectly healthy
/// render loop can sit at a frozen `drawn_frame` for the entire 45s. A reason keyed on it would
/// reintroduce exactly #890's false `render-loop-wedged`, from a different direction.
///
/// NOT MEASURED: whether the watchdog ever fires with a genuinely wedged render loop. Every
/// measurement in #890 comes from the dead-net-thread path.
fn watchdog_reason(net_end: Option<NetThreadEnd>, camp_undrained: bool) -> &'static str {
    match net_end {
        Some(NetThreadEnd::NeverStarted) => "net-thread-never-started",
        Some(NetThreadEnd::Panicked)
        | Some(NetThreadEnd::Fatal)
        | Some(NetThreadEnd::ReturnedUnexpectedly) => "net-thread-dead",
        Some(NetThreadEnd::ReturnedDuringShutdown) => "shutdown-not-completed",
        None if camp_undrained => "camp-not-drained",
        None => "watchdog-shutdown-timeout",
    }
}

/// Sample the two live signals [`watchdog_reason`] keys on. Deliberately read when the watchdog
/// FIRES, not when the request arrives: the reason must describe what is stuck at kill time, and a
/// net thread can die during the 45s wait.
fn watchdog_reason_now(s: &HttpState) -> &'static str {
    let net_end = s.net_thread_dead.lock().unwrap_or_else(|e| e.into_inner())
        .as_ref().map(NetThreadDeath::end);
    let camp_undrained =
        s.lifecycle.camp.lock().unwrap_or_else(|e| e.into_inner()).is_some();
    watchdog_reason(net_end, camp_undrained)
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
/// branches on WHICH state the net thread is in (see [`exit_body`]); the teardown proceeds
/// identically in every case.
///
/// The watchdog is a last resort; see [`EXIT_WATCHDOG`] for its duration and [`watchdog_reason`]
/// for why its recorded reason is derived rather than constant.
async fn post_exit(State(s): State<HttpState>) -> (StatusCode, String) {
    let death = s.net_thread_dead.lock().unwrap_or_else(|e| e.into_inner()).clone();
    tracing::info!(
        "exit: camp-and-shutdown requested via POST /v1/lifecycle/exit (net_thread_end={:?})",
        death.as_ref().map(NetThreadDeath::end)
    );
    s.command.request_camp(CampCmd::Start);
    tokio::spawn(run_exit_watchdog(s.clone(), EXIT_WATCHDOG, |reason| {
        eqoxide_crash::exit(reason, 0)
    }));
    (StatusCode::OK, exit_body(death.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::{
        exit_body, router, run_exit_watchdog, watchdog_reason, watchdog_reason_now,
        EXIT_BODY_CAMPING, EXIT_WATCHDOG,
    };
    use crate::testkit::empty_state;
    use crate::HttpState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use eqoxide_ipc::{NetThreadDeath, NetThreadEnd};
    use tower::ServiceExt;

    /// Every `NetThreadEnd`, so the loops below cannot silently stop covering one. Written out
    /// rather than derived: a future variant must break these tests, not slip through them.
    const ALL_ENDS: [NetThreadEnd; 5] = [
        NetThreadEnd::Panicked,
        NetThreadEnd::Fatal,
        NetThreadEnd::ReturnedDuringShutdown,
        NetThreadEnd::ReturnedUnexpectedly,
        NetThreadEnd::NeverStarted,
    ];

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

    fn publish(state: &HttpState, end: NetThreadEnd, reason: &str) {
        *state.net_thread_dead.lock().unwrap() = Some(NetThreadDeath::new(end, reason));
    }

    fn mark_net_thread_dead(state: &HttpState) {
        publish(state, NetThreadEnd::Panicked,
                "the eq-net thread PANICKED (synthetic, for this test) — …");
    }

    /// Run the REAL watchdog body to its kill and hand back the reason it passed to the killer.
    /// This is the seam (#799): the reason is pinned where it is USED, so re-hard-coding it at the
    /// call site cannot survive.
    async fn reason_at_kill(state: HttpState) -> &'static str {
        let killed_with = std::sync::Arc::new(std::sync::Mutex::new(None::<&'static str>));
        let sink = killed_with.clone();
        run_exit_watchdog(state, EXIT_WATCHDOG, move |reason| {
            *sink.lock().unwrap() = Some(reason);
        })
        .await;
        let r = *killed_with.lock().unwrap();
        r.expect("the watchdog must always kill with a reason")
    }

    // ── The 200 body ────────────────────────────────────────────────────────────────────────────

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
        assert!(body.contains("LINKDEAD"), "must say the session ends as a drop: {body:?}");
        assert!(
            body.contains("net_thread_dead"),
            "must name the field an agent can check: {body:?}"
        );
        assert!(
            body.contains("PANICKED"),
            "the thread's OWN published reason must be relayed verbatim, as require_live_session \
             relays it — a paraphrase is what has to be re-checked every time the reason changes: \
             {body:?}"
        );
        assert!(
            camp_slot.lock().unwrap().is_some(),
            "the teardown must still be requested — #890 changes the WORDING, not the behaviour"
        );
    }

    /// #935 review B3. The dead-path body used to end with "and this character will not be able to
    /// log straight back in the way a camped-out one can". Nobody measured that, from either side —
    /// and `gameplay.rs` states the opposite as tracked fact ("the next login DropClient-kicks the
    /// ghost"). An unverified claim swapped for another unverified claim is not the fix #890 asks
    /// for, so the body says NOTHING about re-login in any state. Re-add a re-login clause of any
    /// polarity and this goes RED.
    #[tokio::test]
    async fn no_exit_body_makes_a_claim_about_logging_back_in() {
        for end in ALL_ENDS {
            let state = empty_state();
            publish(&state, end, "synthetic reason for this test");
            let body = post_exit_body(state).await;
            let lowered = body.to_lowercase();
            for forbidden in ["log straight back", "log back in", "re-login", "relog"] {
                assert!(!lowered.contains(forbidden),
                    "{end:?}: the body must not predict re-login behaviour nobody measured \
                     (found {forbidden:?}): {body:?}");
            }
        }
        let healthy = post_exit_body(empty_state()).await;
        assert!(!healthy.to_lowercase().contains("log back in"), "…on the healthy path either");
    }

    /// #935 review N1: the causality was backwards. `impl Drop for EqStream` sends no
    /// OP_SessionDisconnect — it only calls `abandon_outstanding()` — so the socket goes quiet and
    /// the SERVER declares the drop on its own timer, three minutes before the exit request in
    /// #890's own log. The shutdown inherits the linkdead; it does not cause it. Restore "this
    /// shutdown will leave a LINKDEAD session" and this goes RED.
    #[tokio::test]
    async fn the_lost_session_body_does_not_claim_the_shutdown_causes_the_linkdead() {
        let state = empty_state();
        mark_net_thread_dead(&state);
        let body = post_exit_body(state).await;
        assert!(!body.contains("this shutdown will leave"),
            "the drop already happened when the thread died: {body:?}");
        assert!(body.contains("inherits that outcome rather than causing it"),
            "the body must say which way the causality runs: {body:?}");
    }

    /// #935 review N3: an agent that polls needs a time to expect. The healthy body says ~30s; the
    /// dead-path body used to say only "IS shutting down anyway" while in fact nothing drains the
    /// camp and the process lives until the 45s watchdog.
    #[tokio::test]
    async fn the_bodies_that_wait_for_the_watchdog_say_so() {
        for end in [NetThreadEnd::Panicked, NetThreadEnd::NeverStarted] {
            let state = empty_state();
            publish(&state, end, "synthetic reason for this test");
            let body = post_exit_body(state).await;
            assert!(body.contains("45s watchdog"),
                "{end:?}: nothing drains this camp, so the agent must be told what it is waiting \
                 for: {body:?}");
        }
    }

    /// The other direction: on a live session the endpoint really does camp, and must keep saying
    /// so. Without this, "always warn about linkdead" would pass the tests above.
    #[tokio::test]
    async fn exit_on_a_live_session_still_promises_the_camp() {
        let body = post_exit_body(empty_state()).await;
        assert_eq!(body, EXIT_BODY_CAMPING);
    }

    /// #935 review B2c. `--testzone` never spawns the net thread and pre-publishes that fact, and
    /// `spawn_camera_server` runs in that mode, so this endpoint IS reachable there. Collapsing the
    /// state to `is_some()` answered it with a warning about a LINKDEAD session on a server this
    /// process never connected to — a new well-formed falsehood on the endpoint #890 exists to make
    /// honest. Point `exit_body`'s `NeverStarted` arm at `EXIT_BODY_SESSION_LOST` and this goes RED.
    #[tokio::test]
    async fn exit_in_testzone_does_not_warn_about_a_session_that_never_existed() {
        let state = empty_state();
        publish(&state, NetThreadEnd::NeverStarted,
                "--testzone: the eq-net thread was never started (offline renderer mode).");
        let body = post_exit_body(state).await;
        assert!(body.contains("no server session"),
            "must say there is no session at all: {body:?}");
        assert!(body.contains("nothing is left LINKDEAD"),
            "must say nothing is left linkdead — the opposite of the lost-session body: {body:?}");
        assert!(!body.contains("was still live"),
            "must not hedge about a session that provably never existed: {body:?}");
        assert!(body.contains("never started"), "the published reason must be relayed: {body:?}");
    }

    /// #935 review B2b. `post_exit` is documented idempotent, so a retry is legitimate. A retry
    /// after the net thread has exited as part of a shutdown someone ASKED for must not be answered
    /// with "the session was lost": nothing was lost. Point the `ReturnedDuringShutdown` arm at
    /// `EXIT_BODY_SESSION_LOST` and this goes RED.
    #[tokio::test]
    async fn exit_during_an_already_requested_shutdown_does_not_report_a_lost_session() {
        let state = empty_state();
        publish(&state, NetThreadEnd::ReturnedDuringShutdown,
                "the eq-net thread exited normally after a shutdown was requested.");
        let body = post_exit_body(state).await;
        assert!(body.contains("ALREADY requested"),
            "must say the shutdown was already under way: {body:?}");
        assert!(!body.contains("was still live"),
            "must not describe this as a session loss: {body:?}");
        assert!(!body.contains("cannot end the session with a clean camp-out"),
            "whether THIS session camped out was decided by the earlier shutdown: {body:?}");
    }

    /// Every state gets its OWN body: no two ends may share one, or the distinction the type exists
    /// to carry is not reaching the wire. (The three unplanned-loss ends legitimately share a head,
    /// so the relayed reason is what separates them — which is the point of relaying it.)
    #[tokio::test]
    async fn each_net_thread_state_produces_a_distinguishable_body() {
        let mut seen: Vec<String> = Vec::new();
        for end in ALL_ENDS {
            let d = NetThreadDeath::new(end, format!("synthetic reason for {end:?}"));
            let body = exit_body(Some(&d));
            assert!(!seen.contains(&body), "{end:?} reuses another state's body verbatim: {body:?}");
            assert_ne!(body, EXIT_BODY_CAMPING, "{end:?} must not reuse the healthy-path promise");
            seen.push(body);
        }
        assert_eq!(exit_body(None), EXIT_BODY_CAMPING, "…and a live thread still camps");
    }

    // ── The watchdog reason ─────────────────────────────────────────────────────────────────────

    /// #890's second falsehood, pinned at the mapping. The watchdog fired on a dead net thread and
    /// recorded `render-loop-wedged` while the renderer was measured at ~46fps throughout. No input
    /// may now produce that string, and each state must get its own reason.
    #[test]
    fn the_watchdog_reason_names_the_stuck_subsystem_and_never_blames_the_render_loop() {
        use NetThreadEnd::*;
        for end in [Panicked, Fatal, ReturnedUnexpectedly] {
            for undrained in [true, false] {
                assert_eq!(watchdog_reason(Some(end), undrained), "net-thread-dead",
                    "{end:?}/{undrained}");
            }
        }
        assert_eq!(watchdog_reason(Some(NeverStarted), true), "net-thread-never-started");
        assert_eq!(watchdog_reason(Some(NeverStarted), false), "net-thread-never-started");
        assert_eq!(watchdog_reason(Some(ReturnedDuringShutdown), true), "shutdown-not-completed");
        assert_eq!(watchdog_reason(Some(ReturnedDuringShutdown), false), "shutdown-not-completed");
        assert_eq!(watchdog_reason(None, true), "camp-not-drained");
        // Nothing observable here says the render loop stalled, so nothing here claims it did.
        assert_eq!(watchdog_reason(None, false), "watchdog-shutdown-timeout");

        let mut inputs: Vec<(Option<NetThreadEnd>, bool)> = vec![(None, true), (None, false)];
        for end in ALL_ENDS { inputs.push((Some(end), true)); inputs.push((Some(end), false)); }
        for (end, undrained) in inputs {
            assert_ne!(
                watchdog_reason(end, undrained), "render-loop-wedged",
                "no reachable state may assert a render-loop wedge (end={end:?}, \
                 camp_undrained={undrained})"
            );
        }
    }

    /// #935 review B2. A net thread that was never started, and one that exited because a shutdown
    /// was requested, are not "dead" — the crash record a later investigation reads as fact must
    /// not say they were. Collapse the match back to `is_some()` and this goes RED while the test
    /// above stays green.
    #[test]
    fn the_watchdog_reason_does_not_call_every_ended_net_thread_dead() {
        for undrained in [true, false] {
            assert_ne!(watchdog_reason(Some(NetThreadEnd::NeverStarted), undrained),
                "net-thread-dead", "--testzone never started a thread; nothing died");
            assert_ne!(watchdog_reason(Some(NetThreadEnd::ReturnedDuringShutdown), undrained),
                "net-thread-dead", "it exited because a shutdown was requested — it did its job");
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

        assert_eq!(
            reason_at_kill(state).await,
            "net-thread-dead",
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

        assert_eq!(reason_at_kill(state).await, "camp-not-drained");
    }

    /// #935 review B4, the measured gap. The seam existed for two of three branches: wrapping the
    /// call site with `"watchdog-shutdown-timeout" => "render-loop-wedged"` shipped fully green,
    /// and that is precisely the branch whose entire justification is that `render-loop-wedged`
    /// must never be claimed. Nothing drove `run_exit_watchdog` with a clean state at all. This
    /// does: no published net-thread state, no camp in the slot.
    #[tokio::test(start_paused = true)]
    async fn the_watchdog_on_a_clean_state_kills_without_naming_any_subsystem() {
        let state = empty_state();
        assert!(state.lifecycle.camp.lock().unwrap().is_none(), "precondition: nothing queued");
        assert!(state.net_thread_dead.lock().unwrap().is_none(), "precondition: nothing published");

        let reason = reason_at_kill(state).await;
        assert_eq!(reason, "watchdog-shutdown-timeout");
        assert_ne!(reason, "render-loop-wedged",
            "this branch exists BECAUSE the render loop cannot be named here (#890); \
             camera.drawn_frame stalls on a healthy idle loop too");
    }

    /// The remaining two branches at the same seam, so every `watchdog_reason` arm is now pinned
    /// where the reason is USED and not only where it is computed. A wrap at the call site
    /// re-scoping ANY arm now reds a test.
    #[tokio::test(start_paused = true)]
    async fn the_watchdog_pins_the_never_started_and_already_shutting_down_branches_too() {
        let state = empty_state();
        publish(&state, NetThreadEnd::NeverStarted, "--testzone: never started.");
        assert_eq!(reason_at_kill(state).await, "net-thread-never-started");

        let state = empty_state();
        publish(&state, NetThreadEnd::ReturnedDuringShutdown, "exited after a requested shutdown.");
        assert_eq!(reason_at_kill(state).await, "shutdown-not-completed");
    }

    /// Every reason the watchdog can emit is distinct and non-empty (#380: the `kill` closure may
    /// never be handed an empty label, or the forced exit reads as an OOM-kill). Enumerated over the
    /// same total input space as the mapping test, so a new arm that duplicates an existing string
    /// — the cheapest way to lose a distinction — is caught here.
    #[test]
    fn every_watchdog_reason_is_a_distinct_non_empty_label() {
        let mut inputs: Vec<(Option<NetThreadEnd>, bool)> = vec![(None, true), (None, false)];
        for end in ALL_ENDS { inputs.push((Some(end), true)); inputs.push((Some(end), false)); }
        let mut seen: Vec<&'static str> = Vec::new();
        for (end, undrained) in inputs {
            let r = watchdog_reason(end, undrained);
            assert!(!r.is_empty(), "an unlabelled forced exit reads as an OOM-kill (#380)");
            if !seen.contains(&r) { seen.push(r); }
        }
        assert_eq!(seen.len(), 5, "one reason per distinguishable state, got {seen:?}");
    }
}
