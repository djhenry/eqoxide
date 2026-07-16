//! The pathfinding WORKER THREADS (#340, #337, #382).
//!
//! Two workers live here, one per nav tier:
//!
//! | tier | type | posted | budget |
//! |---|---|---|---|
//! | **coarse** (8 u, whole route) | [`Planner`] | on a goal change / re-plan | none (5 s safety net) |
//! | **fine** (2 u, 40 u window, steering) | [`LocalPlanner`] | **every nav tick** | none (spatially bounded) |
//!
//! **Neither runs on the network thread, and neither carries a wall clock any more.** The coarse tier
//! came off in #377; the fine tier — the one that actually steers the character — came off in #382,
//! which is also what deleted the last 150 ms budget. See [`LocalPlanner`] for why the fine tier needs
//! a worker of its own rather than sharing the coarse one.
//!
//! # Why this exists
//!
//! `Collision::find_path` used to run **synchronously on the network thread**, inside
//! `Navigator::tick`. That one fact was the root of three separate bugs:
//!
//! * A long A* search stalls the net loop, so no position/keepalive packet goes out and the server
//!   drops the client as **linkdead**. Two linkdead root causes (#257, #302) were exactly this.
//! * To stop that, the search was given a 150 ms wall-clock budget. But a budget makes the answer
//!   **unfalsifiable**: when A* gives up, it cannot tell "no route exists" from "I ran out of
//!   clock" — so it returned a greedy PARTIAL route, the walker drove it into a wall, retried 8×,
//!   and froze at `nav_state: blocked` forever. That silent wedge **disguised the real nav root
//!   cause for months** and caused several false diagnoses (#337).
//! * And a test asserting on that wall-clock budget is flaky under CPU load (#356).
//!
//! All three are downstream of ONE constraint: *the planner must not block the net thread*. So it
//! doesn't any more. `Navigator::tick` POSTS a request here and returns immediately (micro-, not
//! milliseconds); this thread owns the search. Nothing real-time waits on it, so the search runs to
//! COMPLETION and reports an honest [`PlanOutcome`]: a route, a definitive `Unreachable`, or an
//! `Exhausted` "I don't know" — never a timeout dressed up as a "no".
//!
//! # The generation counter
//!
//! A plan takes time, and the goal can change while one is in flight (a new `/goto`, a re-plan, a
//! zone change). Every request carries a monotonically increasing `gen`; the Navigator only applies
//! a reply whose `gen` matches the request it is currently waiting on. **A stale result — one for a
//! goal we have since abandoned — is discarded, never applied.** Applying one would walk the
//! character toward a goal nobody asked for, which is its own quiet lie.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Instant;

use crate::assets::{Collision, LocalOutcome, NoRoute, PlanCtx, PlanOutcome};
use crate::movement::PLAYER_RADIUS;

/// One plan the walker wants computed. Carries its own `Arc<Collision>` so the worker never touches
/// the `SharedCollision` lock the net + render threads use.
pub struct PlanRequest {
    /// Monotonic id. A reply is only applied if this still matches the Navigator's current request.
    pub gen:          u64,
    pub start:        [f32; 3],
    pub goal:         [f32; 3],
    /// NPC positions to skirt (aggro avoidance, #67) and how wide a berth to give them (#242).
    pub avoid:        Vec<[f32; 2]>,
    pub aggro_buffer: f32,
    /// Zone-point index of the DRNTP region we're routing to, when the goal is a zone line (#229).
    pub goal_region:  Option<i32>,
    pub collision:    Arc<Collision>,
}

/// A finished plan, tagged with the generation it answers.
pub struct PlanReply {
    pub gen:     u64,
    pub outcome: PlanOutcome,
    /// How long the search actually took. This is the stall that used to land on the NET THREAD.
    pub plan_ms: u128,
    /// `Some(z)` when the caller's goal z could not be resolved to any tier and the planner SNAPPED
    /// the goal to the nearest floor in its column. The client has changed the agent's goal, so the
    /// agent is told (`nav_reason: goal_z_snapped`) — an accommodation presented as compliance is a
    /// lie, and this one would otherwise report `arrived` at a z nobody asked for (#377 review).
    pub goal_snapped_z: Option<f32>,
    /// The per-route TIER (#378 Phase 2 / design §4c): `true` = this route only existed at the
    /// MINIMUM clearance (a tight door/bridge threaded with no margin — a riskier path). Published
    /// as `nav_tier` on /v1/observe/debug so an agent sees the risk of the route it is walking, a
    /// PER-ROUTE fact the zone-lifetime `nav_tight` counter cannot give.
    pub tight: bool,
}

/// The Navigator's handle on the worker: post requests, poll for the one reply that still matters.
pub struct Planner {
    req_tx:   Sender<PlanRequest>,
    rep_rx:   Receiver<PlanReply>,
    /// Next generation to hand out. Monotonic for the life of the Navigator.
    next_gen: u64,
    /// The generation we are currently waiting on, and the GOAL it was requested for.
    ///
    /// These live together, inside the Planner, on purpose. They used to be two fields in two
    /// places — `pending` here, `plan_goal` on the Navigator — and `poll()` cleared one while only
    /// `apply_plan` cleared the other. A tick that consumed a reply and then DROPPED it (because the
    /// goal had drifted) therefore left `plan_goal` set forever, and the "is a plan in flight?" test
    /// said yes for the rest of the session: the planner stopped posting, and the character sat at
    /// `nav_state: planning` PERMANENTLY, with a live, idle worker. Keeping the pair here makes that
    /// state UNREPRESENTABLE — `poll` clears both, atomically, and no caller can leak one.
    pending:  Option<(u64, [f32; 3])>,
    /// Latched once the worker thread has died (panicked). A dead planner must never masquerade as
    /// a busy one — see [`Planner::poll`].
    dead:     bool,
}

impl Default for Planner {
    fn default() -> Self { Self::spawn() }
}

impl Planner {
    /// Spawn the worker thread. It lives until the `Planner` (and so `req_tx`) is dropped.
    pub fn spawn() -> Self {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<PlanRequest>();
        let (rep_tx, rep_rx) = std::sync::mpsc::channel::<PlanReply>();
        std::thread::Builder::new()
            .name("nav-planner".into())
            .spawn(move || worker(req_rx, rep_tx))
            .expect("spawn nav-planner thread");
        Planner { req_tx, rep_rx, next_gen: 1, pending: None, dead: false }
    }

    /// Test-only (#398): like `spawn`, but also hands back a channel the worker sends on every time
    /// it dequeues a request — after coalescing any backlog, immediately before running the search —
    /// and so has irrevocably committed to computing that generation. This is the deterministic sync
    /// seam `a_superseded_plan_is_discarded_not_applied` uses to know "plan A cannot be coalesced away
    /// any more" without a `sleep` racing the worker's own scheduling.
    #[cfg(test)]
    pub fn spawn_with_dequeue_signal() -> (Self, Receiver<u64>) {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<PlanRequest>();
        let (rep_tx, rep_rx) = std::sync::mpsc::channel::<PlanReply>();
        let (dq_tx, dq_rx) = std::sync::mpsc::channel::<u64>();
        std::thread::Builder::new()
            .name("nav-planner-test".into())
            .spawn(move || worker_with_dequeue_signal(req_rx, rep_tx, dq_tx))
            .expect("spawn nav-planner thread");
        (Planner { req_tx, rep_rx, next_gen: 1, pending: None, dead: false }, dq_rx)
    }

    /// Post a plan request and return its generation. **Never blocks on the search** — this is the
    /// whole point of the module. Any in-flight plan is implicitly superseded: its reply will carry
    /// an older `gen` and be discarded by `poll`.
    pub fn request(&mut self, mut req: PlanRequest) -> u64 {
        let gen = self.next_gen;
        self.next_gen += 1;
        req.gen = gen;
        self.pending = Some((gen, req.goal));
        // A dead worker must not silently freeze navigation. It can only die if the thread panicked;
        // report it rather than leaving the walker waiting on a plan that will never come.
        if self.req_tx.send(req).is_err() {
            if !self.dead {
                tracing::error!("nav-planner: the worker thread is gone — pathfinding is DEAD (no route will \
                    ever be planned again this session)");
            }
            self.dead = true;
            self.pending = None;
        }
        gen
    }

    /// Abandon whatever plan is in flight (the goal changed / nav stopped). The reply, if it ever
    /// arrives, is stale and will be dropped.
    pub fn cancel(&mut self) {
        self.pending = None;
    }

    /// Simulate the worker thread PANICKING: its reply `Sender` drops, so our `Receiver` goes
    /// `Disconnected` — which is exactly what `poll` must notice instead of mistaking it for "no
    /// reply yet". (Test-only; there is no way to panic a real thread on demand from here.)
    #[cfg(test)]
    pub fn kill_worker_for_test(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel::<PlanReply>();
        drop(tx);
        self.rep_rx = rx;
    }

    /// Are we waiting on a plan right now?
    pub fn is_planning(&self) -> bool { self.pending.is_some() }

    /// The GOAL of the plan currently in flight, if any. Cleared by `poll` the instant its reply is
    /// handed over — so it can never outlive the request it describes.
    pub fn in_flight_goal(&self) -> Option<[f32; 3]> { self.pending.map(|(_, g)| g) }

    /// Has the worker thread DIED (panicked)? Once true, no plan will ever be computed again, and
    /// the caller MUST say so out loud — see [`Planner::poll`].
    pub fn is_dead(&self) -> bool { self.dead }

    /// Drain the reply channel and return the plan for the request we are actually waiting on.
    /// **Stale replies (a superseded goal) are discarded, not applied** — see the module docs.
    ///
    /// # A dead worker must be LOUD
    ///
    /// If the worker thread panics, its `Sender` drops and this channel goes `Disconnected`. The
    /// first version of this function treated that exactly like `Empty` — so `poll` returned `None`
    /// forever, `awaiting_first_plan` stayed true, and the character sat reporting
    /// `nav_state: planning` **permanently**, with no log, no timeout and no error.
    ///
    /// That is strictly worse than the crash it replaced. On `main` a panic in the planner took the
    /// network thread down — ugly, but HONEST. Swallowing it converts a loud crash into precisely
    /// the silent lie this project ranks *above* crashes: a character that says "planning…" forever
    /// while nothing is planning at all. So `Disconnected` is now latched and surfaced, and the
    /// Navigator turns it into an honest terminal `no_path` / `planner_dead`.
    pub fn poll(&mut self) -> Option<PlanReply> {
        let mut fresh = None;
        loop {
            match self.rep_rx.try_recv() {
                Ok(rep) => {
                    if let Some(rep) = self.accept_reply(rep) { fresh = Some(rep); }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.dead {
                        tracing::error!("nav-planner: the worker thread is DEAD (panicked) — pathfinding is \
                            permanently broken for this session. Nav will report no_path/planner_dead rather \
                            than pretend to be planning.");
                    }
                    self.dead = true;
                    self.pending = None; // nothing will ever answer it
                    break;
                }
            }
        }
        fresh
    }

    /// The one place that decides "is this reply CURRENT, or STALE?" for a reply already taken off
    /// the channel. Shared by `poll`'s non-blocking drain and (test-only) a blocking wait, so there is
    /// exactly one implementation of the discard rule the module docs describe — not two copies that
    /// could drift apart.
    fn accept_reply(&mut self, rep: PlanReply) -> Option<PlanReply> {
        if self.pending.map(|(g, _)| g) == Some(rep.gen) {
            // Clear the pending request AND its goal together: handing the reply over is exactly the
            // moment the plan stops being in flight.
            self.pending = None;
            Some(rep)
        } else {
            tracing::debug!("nav-planner: discarding STALE plan #{} (now waiting on {:?})", rep.gen, self.pending);
            None
        }
    }

    /// Test-only (#398): block until a reply that matches `pending` is accepted, discarding any stale
    /// ones along the way through the exact same rule `poll()` uses (`accept_reply`) — but by blocking
    /// on the channel instead of busy-polling it on a sleep. Waiting for an arbitrarily slow plan (a
    /// sealed-box search under heavy CPU load) is then bounded by the OS scheduler and `timeout`, not
    /// by a fixed retry-count-times-sleep-interval budget that can run out before the plan does.
    #[cfg(test)]
    fn recv_applied(&mut self, timeout: std::time::Duration) -> PlanReply {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let rep = self.rep_rx.recv_timeout(remaining)
                .expect("a reply must arrive within the timeout");
            if let Some(rep) = self.accept_reply(rep) { return rep; }
        }
    }
}

/// The worker loop. Blocks on the request channel, coalesces any backlog down to the newest request
/// (an older goal is already superseded — computing it would just delay the one that matters), runs
/// the plan, and ships the outcome back.
fn worker(req_rx: Receiver<PlanRequest>, rep_tx: Sender<PlanReply>) {
    worker_impl(req_rx, rep_tx, None)
}

/// Test-only (#398): identical to `worker`, but sends the generation it just dequeued on
/// `on_dequeue`, right after coalescing and right before running the search — see
/// [`Planner::spawn_with_dequeue_signal`].
#[cfg(test)]
fn worker_with_dequeue_signal(req_rx: Receiver<PlanRequest>, rep_tx: Sender<PlanReply>, on_dequeue: Sender<u64>) {
    worker_impl(req_rx, rep_tx, Some(on_dequeue))
}

fn worker_impl(req_rx: Receiver<PlanRequest>, rep_tx: Sender<PlanReply>, on_dequeue: Option<Sender<u64>>) {
    while let Ok(mut req) = req_rx.recv() {
        while let Ok(newer) = req_rx.try_recv() {
            tracing::debug!("nav-planner: superseding queued plan #{} with #{}", req.gen, newer.gen);
            req = newer;
        }
        // Test-only observer: the worker has now committed to `req.gen` — it is no longer sitting in
        // `req_rx` where a future request could coalesce it away. `None` in production; a dropped
        // receiver on the test side is not this thread's problem, hence `let _ =`.
        if let Some(tx) = &on_dequeue { let _ = tx.send(req.gen); }
        let t0 = Instant::now();
        // Is the planner about to change the goal it was given? (A zone-line goal is a VOLUME, not a
        // floor point — it is not "snapped" and must not be reported as such.)
        let goal_snapped_z = req.goal_region
            .is_none()
            .then(|| req.collision.goal_z_was_snapped(req.goal))
            .flatten();
        if let Some(z) = goal_snapped_z {
            tracing::warn!("nav-planner: goal ({:.0},{:.0},{:.1}) has no floor at the z you asked for — \
                SNAPPING it to the floor at z={:.1}. The client is changing your goal; it is not the one \
                you specified.", req.goal[0], req.goal[1], req.goal[2], z);
        }
        let (outcome, tight) = plan_path(&req.collision, req.start, req.goal, &req.avoid, req.aggro_buffer, req.goal_region);
        let plan_ms = t0.elapsed().as_millis();
        // The headline number for #340: this is the synchronous stall that used to sit on the net
        // thread (capped at 150 ms per A* call, up to ~2 s per plan). It now sits here, where the
        // only thing waiting on it is the walker.
        tracing::info!("nav-planner: plan #{} ({:.0},{:.0})->({:.0},{:.0}) took {}ms OFF the net thread → {}",
            req.gen, req.start[0], req.start[1], req.goal[0], req.goal[1], plan_ms,
            describe(&outcome));
        if rep_tx.send(PlanReply { gen: req.gen, outcome, plan_ms, goal_snapped_z, tight }).is_err() {
            break; // Navigator gone (zone change / shutdown)
        }
    }
}

fn describe(o: &PlanOutcome) -> String {
    match o {
        PlanOutcome::Route(p) => format!("ROUTE ({} wp)", p.len()),
        PlanOutcome::Unreachable { reason, .. } => format!("UNREACHABLE ({})", reason.as_str()),
        PlanOutcome::Exhausted { limit, progress: Some(p) } =>
            format!("EXHAUSTED ({}) — walking a PARTIAL route ({} wp) and re-planning from its end", limit.as_str(), p.len()),
        PlanOutcome::Exhausted { limit, progress: None } =>
            format!("EXHAUSTED ({}) — no usable progress; this is 'I DON'T KNOW', not 'no route'", limit.as_str()),
    }
}

/// Plan a route to `goal`. Runs on the worker thread.
///
/// 1. A full-width A* search at the character's real collision radius, run to completion (subject
///    only to the worker's generous safety net).
/// 2. If the START is boxed in (its reachable component is a handful of cells — the char is stood
///    inside a tree trunk / on a slope face where no neighbour resolves a walkable floor, #205),
///    re-anchor to a clean floor a few units away and route from there. The walker heads to that
///    floor first.
///
/// Anything else is reported HONESTLY and immediately: an unreachable goal is `Unreachable`, and a
/// search that hit a limit is `Exhausted` — never a partial route dressed up as a plan (#337).
pub fn plan_path(
    col: &Collision,
    start: [f32; 3],
    goal: [f32; 3],
    avoid: &[[f32; 2]],
    aggro_buffer: f32,
    goal_region: Option<i32>,
) -> (PlanOutcome, bool) {
    // ONE node budget for the WHOLE plan, not one per A* call (#340, #394 review). This function makes
    // up to 13 A* calls (the route + a 12-point re-anchor ring), and `search_tiered` makes up to 2
    // clearance passes inside each — `ensure_budget()` materialises a single shared expansion counter
    // HERE, before the fan-out, so every one of those searches draws from the same `MAX_NODES` (8M)
    // budget. Without this the port to a node cap would have given each call its OWN 8M, and the
    // pathological StartIsolated-in-a-big-zone case would cost ~13 × 8M expansions (minutes) instead of
    // one plan's worth (~28 s worst case, measured). Nothing real-time waits on this thread, but a
    // plan that runs for minutes still blocks the NEXT goal (the worker coalesces only between plans),
    // so the plan-wide bound matters.
    let ctx = PlanCtx::worker().ensure_budget().with_goal_region(goal_region);
    plan_path_with_ctx(col, start, goal, avoid, aggro_buffer, ctx)
}

/// The body of [`plan_path`], taking the (already budgeted) `ctx` so a test can supply a small-capped
/// budget and observe that all 13 A* calls draw from its ONE shared counter (#394 review, FIX 1).
pub(crate) fn plan_path_with_ctx(
    col: &Collision,
    start: [f32; 3],
    goal: [f32; 3],
    avoid: &[[f32; 2]],
    aggro_buffer: f32,
    ctx: PlanCtx,
) -> (PlanOutcome, bool) {
    let radius = PLAYER_RADIUS;

    let (first, first_tier) = col.find_path_ex_tiered(start, goal, radius, avoid, 8.0, None, aggro_buffer, ctx.clone());
    match first {
        // A real route, or an honest limit — either way, that's the answer (with its tier).
        PlanOutcome::Route(_) | PlanOutcome::Exhausted { .. } => return (first, first_tier),
        // A definitive no... unless the START is what's broken, which we can still fix.
        PlanOutcome::Unreachable { reason: NoRoute::StartIsolated, .. } => {}
        PlanOutcome::Unreachable { .. } => return (first, first_tier),
    }

    // Isolated start (#205): A* couldn't leave the start cell. A clean walkable floor is almost
    // always a few units away laterally (usually just downhill). Retry from the nearest such floor;
    // the route then begins there and the walker heads off the face to it first.
    const RING: [(f32, f32); 12] = [
        (-16.0, 0.0), (16.0, 0.0), (0.0, -16.0), (0.0, 16.0),
        (-16.0, -16.0), (16.0, -16.0), (-16.0, 16.0), (16.0, 16.0),
        (-32.0, 0.0), (32.0, 0.0), (0.0, -32.0), (0.0, 32.0),
    ];
    for (dx, dy) in RING {
        let (ax, ay) = (start[0] + dx, start[1] + dy);
        // A floor reachable from the char's height (a generous down-search finds ground below a face).
        let Some(af) = col.nearest_floor(ax, ay, start[2], 20.0, 100.0) else { continue };
        let anchor = [ax, ay, af];
        // Same shared budget (`ctx.clone()` shares the `Arc`): the ring retries draw down the SAME 8M,
        // so 13 calls cost one plan's budget, not 13. The last retry may find the budget already spent
        // by earlier ones and return `Exhausted(NodeCap)` — honest, and bounded.
        let (out, out_tier) = col.find_path_ex_tiered(anchor, goal, radius, avoid, 8.0, None, aggro_buffer, ctx.clone());
        // Only worthwhile if the re-anchored start could actually MOVE (more than the lone start
        // cell A* was stuck on). `> 2`, not `> 1`: every route begins AT its start point (find_path
        // prepends it), so a route that goes nowhere is already len 2.
        let usable = match &out {
            PlanOutcome::Route(p) => p.len() > 2,
            PlanOutcome::Exhausted { progress: Some(p), .. } => p.len() > 2,
            _ => false,
        };
        if usable {
            tracing::warn!("nav: start isolated at ({:.0},{:.0},{:.0}) — re-anchored to clean floor ({:.0},{:.0},{:.0})",
                start[0], start[1], start[2], ax, ay, af);
            return (out, out_tier);
        }
    }
    // The start is sealed in and no nearby floor gets us out. That IS a definitive no.
    (first, first_tier)
}

// ---------------------------------------------------------------------------------------------
// THE FINE LOCAL TIER (#382)
// ---------------------------------------------------------------------------------------------

/// One fine/local steering plan: a bounded 2 u A* from the character to a carrot on the coarse route.
pub struct LocalRequest {
    pub gen:       u64,
    pub start:     [f32; 3],
    /// The carrot: a point ~`LOCAL_REACH` ahead on the committed coarse route.
    pub goal:      [f32; 3],
    pub cell:      f32,
    pub bound:     f32,
    /// How near the carrot counts as REACHING it — see `Collision::find_path_local`.
    pub carrot_tol: f32,
    pub collision: Arc<Collision>,
}

/// A finished fine plan. It carries the `start` and `goal` it was computed FOR, because by the time
/// it lands the walker has moved and the carrot has slid — and every judgement made about this plan
/// ("did it reach its carrot?") must be made against the question it actually answered, not against
/// the question we would ask now.
pub struct LocalReply {
    pub gen:     u64,
    pub start:   [f32; 3],
    pub goal:    [f32; 3],
    pub outcome: LocalOutcome,
    /// Microseconds, not milliseconds: this is the stall that used to land on the NET THREAD every
    /// single nav tick (measured, release, akanon: mean 15.3 ms, worst 358 ms).
    pub plan_us: u128,
}

/// The Navigator's handle on the FINE tier's worker (#382).
///
/// # Why a second worker and not the coarse one
///
/// The two tiers have irreconcilable latency contracts. A coarse plan is posted on a goal change and
/// may legitimately take **seconds** (its safety net is 5 s); a fine plan is posted on **every nav
/// tick** and is worthless if it is not back within a tick or two. Sharing one worker would put every
/// fine plan behind whatever coarse search is running — starving the tier that steers, for the entire
/// duration of the tier that routes — and the coarse worker's request-coalescing (newest wins) would
/// happily throw a coarse plan away in favour of a fine one. So: two workers, two queues, two
/// generations, one shared pattern.
///
/// # The contract that makes this safe
///
/// **The walker never waits on this.** There is no `awaiting_first_local_plan`, and there is
/// deliberately no way to add one: the steering aim is chosen by `crate::nav::steering::steer_target`, a TOTAL
/// pure function of (coarse route, whatever the fine tier last produced). Every state this planner
/// can be in — never asked, in flight, dead, answered with nothing — is just "the fine path is
/// empty", and an empty fine path steers on the coarse carrot. A stall here would be worse than the
/// bug being fixed, and in this codebase a `/follow` deadlock once passed live verification *by
/// luck*; so the no-stall claim is discharged by a property test over that pure function, not by a
/// race we happened to win.
pub struct LocalPlanner {
    req_tx:   Sender<LocalRequest>,
    rep_rx:   Receiver<LocalReply>,
    next_gen: u64,
    /// The generation we are waiting on. Cleared by `poll` the instant its reply is handed over.
    pending:  Option<u64>,
    dead:     bool,
}

impl Default for LocalPlanner {
    fn default() -> Self { Self::spawn() }
}

impl LocalPlanner {
    pub fn spawn() -> Self {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<LocalRequest>();
        let (rep_tx, rep_rx) = std::sync::mpsc::channel::<LocalReply>();
        std::thread::Builder::new()
            .name("nav-local".into())
            .spawn(move || local_worker(req_rx, rep_tx))
            .expect("spawn nav-local thread");
        LocalPlanner { req_tx, rep_rx, next_gen: 1, pending: None, dead: false }
    }

    /// Post a fine plan **if the worker is idle**, and return whether one was posted. A channel send —
    /// microseconds — never a search.
    ///
    /// # There is deliberately no public `is_planning()`
    ///
    /// This is the whole no-stall design, and it is a TYPE decision, not a discipline one. The obvious
    /// API is `is_planning()` + `request()`, and the obvious caller is
    ///
    /// ```text
    /// if planner.is_planning() { return; }   // <-- the walker now waits on the fine tier. Deadlock.
    /// ```
    ///
    /// That single line would reintroduce the class of bug this PR exists to remove — and worse than
    /// the original, because it would be a *stall on a thread boundary* rather than a bounded inline
    /// search. #377 removed its own deadlock the same way: it did not add a check that `plan_goal` was
    /// cleared, it **deleted the field**, so the bad state could not be written down.
    ///
    /// So the "is one in flight?" question is not askable from outside. Posting is idempotent (a second
    /// post while one is in flight is a no-op), and there is nothing to wait on because there is nothing
    /// to ask. Callers get an aim from `crate::nav::steering::steer_target`, which is total.
    pub fn post_if_idle(&mut self, mut req: LocalRequest) -> bool {
        if self.pending.is_some() || self.dead { return false; }
        let gen = self.next_gen;
        self.next_gen += 1;
        req.gen = gen;
        self.pending = Some(gen);
        if self.req_tx.send(req).is_err() {
            if !self.dead {
                tracing::error!("nav-local: the fine-tier worker thread is gone — the walker will steer on the \
                    COARSE route only (it keeps walking; the last ~40u of steering just loses its 2u detail)");
            }
            self.dead = true;
            self.pending = None;
            return false;
        }
        true
    }

    /// Abandon the fine plan in flight (the route was reset / nav stopped).
    pub fn cancel(&mut self) { self.pending = None; }

    /// Test-only. Production code cannot ask this — see [`LocalPlanner::post_if_idle`].
    #[cfg(test)]
    pub fn is_planning(&self) -> bool { self.pending.is_some() }

    /// Has the fine worker DIED? Unlike the coarse planner this is NOT terminal for navigation — the
    /// walker degrades to coarse-only steering — but it must still be said out loud, because an agent
    /// that is being steered without the fine tier is being steered worse and deserves to know.
    pub fn is_dead(&self) -> bool { self.dead }

    #[cfg(test)]
    pub fn kill_worker_for_test(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel::<LocalReply>();
        drop(tx);
        self.rep_rx = rx;
    }

    /// Drain the reply channel and hand back the fine plan for the request we are actually waiting
    /// on. Stale replies (a superseded carrot) are DISCARDED — steering along a plan aimed at a
    /// carrot we have already walked past is its own quiet lie.
    pub fn poll(&mut self) -> Option<LocalReply> {
        let mut fresh = None;
        loop {
            match self.rep_rx.try_recv() {
                Ok(rep) => {
                    if self.pending == Some(rep.gen) {
                        self.pending = None;
                        fresh = Some(rep);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.dead {
                        tracing::error!("nav-local: the fine-tier worker thread is DEAD (panicked) — steering \
                            falls back to the COARSE route for the rest of this session. Reported as \
                            nav_local.state=planner_dead; the walker keeps walking.");
                    }
                    self.dead = true;
                    self.pending = None;
                    break;
                }
            }
        }
        fresh
    }
}

/// The fine worker loop. Same shape as `worker`: block, coalesce the backlog down to the NEWEST
/// request (an older carrot is already stale — the walker has driven past it), plan, reply.
fn local_worker(req_rx: Receiver<LocalRequest>, rep_tx: Sender<LocalReply>) {
    while let Ok(mut req) = req_rx.recv() {
        while let Ok(newer) = req_rx.try_recv() { req = newer; }
        let t0 = Instant::now();
        let outcome = req.collision.find_path_local(req.start, req.goal, req.cell, req.bound, req.carrot_tol);
        let plan_us = t0.elapsed().as_micros();
        if !outcome.threaded() {
            tracing::debug!("nav-local: fine plan #{} ({:.0},{:.0})->({:.0},{:.0}) = {} ({}) in {}us",
                req.gen, req.start[0], req.start[1], req.goal[0], req.goal[1],
                outcome.state(), outcome.reason(), plan_us);
        }
        if rep_tx.send(LocalReply { gen: req.gen, start: req.start, goal: req.goal, outcome, plan_us }).is_err() {
            break; // Navigator gone (zone change / shutdown)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{Collision, LocalOutcome, MeshData, PlanOutcome, RenderMode, ZoneAssets};

    /// GLB-space quad (`positions` are `[north, up, east]`).
    fn quad(v: Vec<[f32; 3]>) -> MeshData {
        MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// A big open plane (±`half` in east and north, at up = 0) with a small SEALED BOX of walls
    /// around `(box_e, box_n)`. A goal inside the box stands on a perfectly good floor — it is just
    /// walled off — so A* cannot dismiss it up front: it has to flood the whole plane and close the
    /// frontier to learn the truth. That makes this fixture both an unreachable goal AND an
    /// expensive plan, which is exactly the pair of properties #337 and #340 are about.
    fn plane_with_sealed_box(half: f32, box_e: f32, box_n: f32) -> Collision {
        let r = 24.0; // half-width of the sealed box
        let (e0, e1, n0, n1) = (box_e - r, box_e + r, box_n - r, box_n + r);
        const H: f32 = 30.0; // wall height — well over the walker's step/chest rays
        let terrain = vec![
            // the floor
            quad(vec![[-half, 0.0, -half], [half, 0.0, -half], [half, 0.0, half], [-half, 0.0, half]]),
            // four walls, meeting at the corners → a sealed room
            quad(vec![[n0, 0.0, e0], [n1, 0.0, e0], [n1, H, e0], [n0, H, e0]]),
            quad(vec![[n0, 0.0, e1], [n1, 0.0, e1], [n1, H, e1], [n0, H, e1]]),
            quad(vec![[n0, 0.0, e0], [n0, 0.0, e1], [n0, H, e1], [n0, H, e0]]),
            quad(vec![[n1, 0.0, e0], [n1, 0.0, e1], [n1, H, e1], [n1, H, e0]]),
        ];
        Collision::build(&ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0)
    }

    /// The #337 invariant, at the planner level: a goal that is genuinely walled off must come back
    /// as an honest, definitive **`Unreachable`** — NOT as a partial route toward it.
    ///
    /// This is the bug that cost the project months. The old planner flooded the grid, gave up, and
    /// handed the walker a greedy partial; the walker drove it into the wall, retried 8×, and froze
    /// at `blocked`, never once saying "there is no way in".
    ///
    /// **This test was INTERMITTENTLY RED on CI before #394** — not flaky, correctly detecting a real
    /// bug: the coarse worker carried a 5 s wall clock, and on a loaded runner the 62,500-cell frontier
    /// close blew it, so the honest `Unreachable(SearchClosed)` degraded to `Exhausted(Deadline)`. The
    /// answer depended on machine speed. #394 replaced the wall clock with a deterministic node cap, so
    /// the frontier now closes to the same answer on every box. The determinism is pinned by
    /// `the_coarse_planner_is_deterministic_under_load` below.
    #[test]
    fn an_unreachable_goal_reports_unreachable_not_a_partial_route() {
        let col = plane_with_sealed_box(1000.0, 400.0, 400.0);
        let (out, _tier) = plan_path(&col, [-900.0, -900.0, 0.0], [400.0, 400.0, 0.0], &[], 0.0, None);
        match &out {
            PlanOutcome::Unreachable { reason: NoRoute::SearchClosed, .. } => {}
            other => panic!("a walled-off goal must report a DEFINITIVE Unreachable(SearchClosed), got {other:?}"),
        }
        assert!(out.route().is_none(), "an unreachable goal must yield NO waypoints — a partial route here is the #337 lie");
    }

    /// # PROPERTY: **THE COARSE PLANNER'S ANSWER DOES NOT DEPEND ON MACHINE SPEED.** (#394)
    ///
    /// This is the universal #394 is about, and a passing quiet run cannot discharge it — the old
    /// wall-clock bug passed 5/5 locally and only failed under CI load. So this test manufactures the
    /// load: it saturates every core with spinner threads and plans the same walled-off goal many
    /// times, and asserts the answer is `Unreachable(SearchClosed)` **every single time**.
    ///
    /// Under a wall-clock budget this test FAILS by construction: the load pushes the frontier close
    /// past the deadline and the outcome flips to `Exhausted(Deadline)`. Under a node cap it CANNOT
    /// fail: the same 62,500-cell frontier is closed after the same number of expansions whatever the
    /// CPU is doing. That difference is the entire point of the fix.
    ///
    /// (Belt and braces: `PlanLimit::Deadline` no longer exists, so `Exhausted(Deadline)` is not even
    /// spellable. This test guards the LAYER ABOVE that — that no *new* wall clock creeps back in and
    /// makes a genuinely-unreachable goal report anything other than the definitive no.)
    #[test]
    fn the_coarse_planner_is_deterministic_under_load() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let col = Arc::new(plane_with_sealed_box(1000.0, 400.0, 400.0));
        // Saturate the box: one busy-spinner per core, so the plan below competes for CPU exactly the
        // way it does on a loaded CI runner (which is where the wall-clock version failed).
        let stop = Arc::new(AtomicBool::new(false));
        let load: Vec<_> = (0..std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || { let mut x = 0u64; while !stop.load(Ordering::Relaxed) { x = x.wrapping_add(1); std::hint::black_box(x); } })
            })
            .collect();

        // The same walled-off goal, planned repeatedly. Every answer must be the definitive no.
        // (4 iterations, not more: each is a full 62,500-cell frontier close — the point is proven by a
        // handful of repeats under load, and a property test must not itself become a CI time sink.)
        for i in 0..4 {
            let (out, _tier) = plan_path(&col, [-900.0, -900.0, 0.0], [400.0, 400.0, 0.0], &[], 0.0, None);
            assert!(matches!(out, PlanOutcome::Unreachable { reason: NoRoute::SearchClosed, .. }),
                "run {i} under full CPU load returned {out:?} — the coarse planner's answer must NOT \
                 depend on machine speed. A wall-clock budget would flip this to Exhausted(Deadline); a \
                 node cap cannot (#394).");
        }
        stop.store(true, Ordering::Relaxed);
        for h in load { let _ = h.join(); }
    }

    /// # THE NODE BUDGET IS PLAN-WIDE, NOT PER-CALL (#394 review, FIX 1).
    ///
    /// `plan_path` fans out to up to 13 A* calls (the primary + a 12-point StartIsolated re-anchor
    /// ring). On `main` the wall-clock deadline was a single shared `Instant`, so the WHOLE plan was
    /// bounded together. The node-cap port must preserve that: one shared, decrementing budget across
    /// all 13 calls — not 13 independent copies of the cap.
    ///
    /// This pins it directly. With a small shared cap, the total expansions across the ENTIRE plan
    /// (read from the shared counter) must not exceed that cap by more than one call's final over-shoot
    /// — proving the ring retries drew DOWN the same budget rather than each getting a fresh one. Under
    /// the per-call bug this fixture spent ~13× the cap; the assertion catches exactly that.
    /// A big open floor with a sealed box around `(box_e, box_n)` (the GOAL, walled off) AND a sealed
    /// box around `(start_e, start_n)` (the START, boxed in). The boxed start makes `find_path_ex`
    /// return `Unreachable(StartIsolated)`, which is what makes `plan_path` fire its 12-point re-anchor
    /// RING; the ring anchors land outside the start box on the open floor and each floods the whole
    /// plane trying to reach the walled-off goal — so every one of the ~13 A* calls is EXPENSIVE, which
    /// is exactly the condition under which per-call vs plan-wide budgeting differs by ~13×.
    fn two_boxes(half: f32, box_e: f32, box_n: f32, start_e: f32, start_n: f32) -> Collision {
        const H: f32 = 30.0;
        let mut terrain = vec![
            quad(vec![[-half, 0.0, -half], [half, 0.0, -half], [half, 0.0, half], [-half, 0.0, half]]),
        ];
        for (ce, cn, r) in [(box_e, box_n, 24.0f32), (start_e, start_n, 6.0f32)] {
            let (e0, e1, n0, n1) = (ce - r, ce + r, cn - r, cn + r);
            terrain.push(quad(vec![[n0, 0.0, e0], [n1, 0.0, e0], [n1, H, e0], [n0, H, e0]]));
            terrain.push(quad(vec![[n0, 0.0, e1], [n1, 0.0, e1], [n1, H, e1], [n0, H, e1]]));
            terrain.push(quad(vec![[n0, 0.0, e0], [n0, 0.0, e1], [n0, H, e1], [n0, H, e0]]));
            terrain.push(quad(vec![[n1, 0.0, e0], [n1, 0.0, e1], [n1, H, e1], [n1, H, e0]]));
        }
        Collision::build(&ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0)
    }

    #[test]
    fn the_node_budget_is_plan_wide_not_per_call() {
        use std::sync::atomic::Ordering;
        // Start boxed in (fires the ring), goal walled off elsewhere (every ring anchor floods and
        // closes) — so all ~13 A* calls run and all are expensive. half=1000 = 62,500 cells, far more
        // than the small cap, so per-call budgeting would blow ~13× past it.
        let col = two_boxes(1000.0, 400.0, 400.0, -400.0, -400.0);
        let cap = 10_000usize;
        let ctx = crate::assets::PlanCtx::with_node_cap(cap).ensure_budget();
        let counter = ctx.expanded.clone().unwrap();

        // Drive the REAL plan_path fan-out (primary + 12-point ring) against the shared budget.
        let (out, _tier) = plan_path_with_ctx(&col, [-400.0, -400.0, 0.0], [400.0, 400.0, 0.0], &[], 0.0, ctx);
        let total = counter.load(Ordering::Relaxed);

        // LOWER bound: the plan really did its work THROUGH this shared counter. If a call quietly
        // created its OWN counter instead (the per-call bug), this one reads ~0 and the plan's cost
        // vanished from view — that is the failure to catch. A real flood fills the shared budget.
        assert!(total >= cap,
            "the plan expanded only {total} nodes on the shared counter — its searches did NOT draw from \
             the plan-wide budget (each made its own). out={out:?}");
        // UPPER bound: all ~13 calls together stopped within one call's final over-shoot of the cap. A
        // per-call cap would let each spend up to `cap` alone — ~{} total.
        assert!(total <= cap + 8192,
            "plan_path expanded {total} nodes against a {cap}-node PLAN-WIDE cap — a per-call cap would \
             allow ~{} across the ~13 calls. The budget is not shared.", cap * 13);
    }

    /// A reachable goal on the same fixture still routes (the honesty change must not make the
    /// planner timid — the mirror image of the assertion above).
    #[test]
    fn a_reachable_goal_still_routes() {
        let col = plane_with_sealed_box(1000.0, 400.0, 400.0);
        let (out, _tier) = plan_path(&col, [-900.0, -900.0, 0.0], [900.0, 900.0, 0.0], &[], 0.0, None);
        let route = out.route().unwrap_or_else(|| panic!("an open-plane goal must route, got {out:?}"));
        let last = *route.last().unwrap();
        assert!((last[0] - 900.0).abs() < 8.0 && (last[1] - 900.0).abs() < 8.0,
            "the route must REACH the goal (not stop short), got {last:?}");
    }

    /// The core stale-plan invariant (#340): if the goal changes while a plan is IN FLIGHT, that
    /// plan's answer must be DISCARDED when it lands, never applied. Applying it would walk the
    /// character toward a goal nobody asked for.
    ///
    /// The plan-A goal is deliberately the SEALED one (an expensive, whole-grid search) and plan B
    /// is posted only once the worker has SIGNALLED that it dequeued A — so A really is computed and
    /// its reply really does arrive, and the generation check is what has to reject it. An earlier
    /// version of this test posted both back-to-back; the worker's request COALESCING quietly threw
    /// A away before it ever ran, so the test passed without ever exercising the check it names.
    ///
    /// # #398: no sleep, no wall clock, anywhere in this test
    ///
    /// The previous version used `sleep(150ms)` to give the worker "enough time" to pick A up before
    /// posting B, then polled for up to ~20s waiting for a reply. Under CPU oversubscription that
    /// failed two ways: the sleep could elapse before the worker was even scheduled (A gets coalesced
    /// away → the test's own premise, "A was computed and arrived stale", never held — a VACUOUS
    /// pass), or A's now-uncapped-by-wall-clock close (#394) could outrun the fixed poll window
    /// (`.expect(...)` panics RED). Both are the same root cause: sequencing derived from *how long
    /// something usually takes* instead of a signal for *when it actually happened*.
    ///
    /// This version replaces both wall-clock dependencies with real synchronization: a
    /// `spawn_with_dequeue_signal` seam (the worker tells us the instant it has committed to A, so
    /// posting B is never a race with coalescing) and a blocking `recv_applied` (waiting for A's
    /// arbitrarily-slow close is bounded by the OS scheduler, not a retry count). Neither depends on
    /// CPU speed for CORRECTNESS — only `recv_applied`'s outer `timeout` is wall-clock, and it is
    /// purely a hang detector, generous enough that no plan on this fixture should ever approach it.
    #[test]
    fn a_superseded_plan_is_discarded_not_applied() {
        let col = Arc::new(plane_with_sealed_box(1000.0, 400.0, 400.0));
        let (mut planner, dequeued) = Planner::spawn_with_dequeue_signal();
        let mk = |goal: [f32; 3], col: &Arc<Collision>| PlanRequest {
            gen: 0, start: [-900.0, -900.0, 0.0], goal,
            avoid: vec![], aggro_buffer: 0.0, goal_region: None, collision: col.clone(),
        };
        // Plan A: the sealed goal — a full-grid search, hundreds of ms (longer still under load).
        let gen_a = planner.request(mk([400.0, 400.0, 0.0], &col));

        // Wait for the WORKER ITSELF to say "I have dequeued A and am past the point where anything
        // can coalesce it away" — the real sync seam (#398), not a sleep. The worker's coalescing
        // loop only ever discards requests still SITTING in the channel; the instant it signals here,
        // A has left that channel for good and WILL run to completion, on any CPU, at any load.
        let dequeued_gen = dequeued.recv_timeout(std::time::Duration::from_secs(60))
            .expect("the worker must dequeue plan A — a timeout here is a liveness bug, not a race");
        assert_eq!(dequeued_gen, gen_a, "the worker must have committed to A specifically");

        // The goal changes mid-flight. A's answer is now stale, but — deterministically, per above —
        // it is still being computed and WILL arrive.
        let gen_b = planner.request(mk([-900.0, 900.0, 0.0], &col));
        assert!(gen_b > gen_a, "each request gets a fresh, increasing generation");

        // The first (and only) plan we may apply must be B's. `recv_applied` blocks on the reply
        // channel and discards anything stale through the SAME rule `poll()` uses in production
        // (`accept_reply`) — so however long A's expensive close takes under whatever load the box is
        // under, this waits exactly that long, never a fixed budget that can run out first.
        let rep = planner.recv_applied(std::time::Duration::from_secs(120));
        assert_eq!(rep.gen, gen_b,
            "the applied plan must be the CURRENT goal's (#{gen_b}) — the superseded #{gen_a}, which \
             WAS computed and DID arrive, must have been discarded on the way in");
        assert!(!planner.is_planning(), "the reply we applied clears the pending request");
        // And nothing is left queued to be applied later.
        assert!(planner.poll().is_none(), "a stale reply must be DISCARDED, never queued for later");
    }

    /// **A DEAD PLANNER MUST BE LOUD.** If the worker thread panics, `poll` used to see
    /// `TryRecvError::Disconnected`, treat it exactly like `Empty`, and return `None` forever — so
    /// the Navigator sat at `nav_state: planning` permanently, with no log and no error. That takes
    /// `main`'s LOUD net-thread panic and converts it into precisely the SILENT LIE this project
    /// ranks above crashes. The death must be detectable, and it must latch.
    #[test]
    fn a_dead_planner_is_reported_not_silently_pending() {
        let col = Arc::new(plane_with_sealed_box(400.0, 200.0, 200.0));
        let mut planner = Planner::spawn();
        assert!(!planner.is_dead(), "a fresh planner is alive");

        // Kill the worker the way a panic would: drop its Sender by making it exit.
        planner.kill_worker_for_test();

        planner.request(PlanRequest {
            gen: 0, start: [-300.0, -300.0, 0.0], goal: [300.0, 300.0, 0.0],
            avoid: vec![], aggro_buffer: 0.0, goal_region: None, collision: col,
        });
        // Poll as the nav tick would. It must NOT sit there pretending a plan is coming.
        for _ in 0..50 {
            if planner.poll().is_some() { panic!("a dead planner cannot produce a plan"); }
            if planner.is_dead() { break; }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(planner.is_dead(),
            "a dead worker MUST be detected — otherwise nav reports `planning` forever, which is a \
             silent lie and strictly worse than the crash it replaced");
        assert!(!planner.is_planning(),
            "and it must not still claim a plan is in flight: nothing will ever answer it");
    }

    /// **THE LIVENESS INVARIANT, made structural.** Handing a reply over MUST clear the in-flight
    /// goal, atomically with the pending generation. They used to live in two places — `pending`
    /// here, `plan_goal` on the Navigator — and a tick that consumed a reply but then DROPPED it
    /// (because the goal had drifted a few units) cleared only one. The stale `plan_goal` then made
    /// "is a plan in flight?" answer yes forever: the planner stopped posting and the character sat
    /// at `nav_state: planning` PERMANENTLY, worker alive and idle, invisible to `is_dead()`.
    ///
    /// Keeping the pair in one place makes that unrepresentable. This pins it.
    #[test]
    fn handing_over_a_reply_clears_the_in_flight_goal() {
        let col = Arc::new(plane_with_sealed_box(400.0, 200.0, 200.0));
        let mut planner = Planner::spawn();
        let goal = [300.0, 300.0, 0.0];
        planner.request(PlanRequest {
            gen: 0, start: [-300.0, -300.0, 0.0], goal,
            avoid: vec![], aggro_buffer: 0.0, goal_region: None, collision: col,
        });
        assert_eq!(planner.in_flight_goal(), Some(goal), "the posted goal is in flight");

        let mut got = None;
        for _ in 0..2000 {
            if let Some(r) = planner.poll() { got = Some(r); break; }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(got.is_some(), "the plan must come back");
        assert_eq!(planner.in_flight_goal(), None,
            "handing the reply over MUST clear the in-flight goal — if it survives, the planner \
             believes a plan is forever in flight, stops posting, and the character is frozen at \
             `nav_state: planning` for the rest of the session");
        assert!(!planner.is_planning(), "and nothing is pending");
    }

    /// `cancel` (nav stopped / goal cleared) must make an in-flight plan un-appliable.
    #[test]
    fn a_cancelled_plan_is_never_applied() {
        let col = Arc::new(plane_with_sealed_box(400.0, 200.0, 200.0));
        let mut planner = Planner::spawn();
        planner.request(PlanRequest {
            gen: 0, start: [-300.0, -300.0, 0.0], goal: [300.0, 300.0, 0.0],
            avoid: vec![], aggro_buffer: 0.0, goal_region: None, collision: col,
        });
        planner.cancel();
        for _ in 0..60 {
            assert!(planner.poll().is_none(), "a cancelled plan's reply must never be applied");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!planner.is_planning());
    }

    /// THE net-thread invariant (#340): posting a plan must return immediately no matter how long
    /// the plan itself takes. Before this change the SAME work ran inline on the net thread — so
    /// this test also MEASURES the win: `plan_path` (what the net thread used to pay, synchronously,
    /// per plan) vs `Planner::request` (what it pays now).
    ///
    /// Note the assertions only get SAFER under CPU load (a slow box makes the synchronous plan
    /// slower, not faster) — unlike the wall-clock-budget test this replaces, which flipped red
    /// under contention (#356).
    #[test]
    fn posting_a_plan_does_not_block_the_caller() {
        let col = Arc::new(plane_with_sealed_box(1000.0, 400.0, 400.0));
        let start = [-900.0, -900.0, 0.0];
        let goal  = [ 400.0,  400.0, 0.0]; // sealed in → the search must close the whole plane

        // What the NET THREAD used to pay, synchronously, for this plan:
        let t0 = Instant::now();
        let (outcome, _tier) = plan_path(&col, start, goal, &[], 0.0, None);
        let sync_us = t0.elapsed().as_micros();
        assert!(matches!(outcome, PlanOutcome::Unreachable { .. }), "fixture sanity: this goal is sealed off");

        // What it pays now:
        let mut planner = Planner::spawn();
        let t1 = Instant::now();
        planner.request(PlanRequest {
            gen: 0, start, goal, avoid: vec![], aggro_buffer: 0.0, goal_region: None,
            collision: col.clone(),
        });
        let post_us = t1.elapsed().as_micros();

        eprintln!("NET-THREAD STALL PER PLAN: synchronous {sync_us}us  ->  posted {post_us}us");
        assert!(sync_us > 5_000,
            "fixture sanity: the synchronous plan must be genuinely expensive ({sync_us}us) or this proves nothing");
        assert!(post_us < 1_000,
            "posting a plan must NOT block the net thread — it must be a channel send, not a search (took {post_us}us)");
    }

    // ------------------------------------------------------------------------------------------
    // THE FINE LOCAL TIER (#382)
    // ------------------------------------------------------------------------------------------

    /// A 200x200 floor with a solid wall across east=0 (no gap). A carrot on the far side of it is
    /// inside the fine tier's 40u window and yet genuinely unreachable — the window's frontier CLOSES.
    /// That is the fixture that separates "there is no way through" from "I stopped looking".
    fn plane_with_a_solid_wall() -> Arc<Collision> {
        const H: f32 = 30.0;
        let terrain = vec![
            quad(vec![[-100.0, 0.0, -100.0], [100.0, 0.0, -100.0], [100.0, 0.0, 100.0], [-100.0, 0.0, 100.0]]),
            // A wall plane at east = 0, spanning the whole north extent. (GLB positions = [n, up, e].)
            quad(vec![[-100.0, 0.0, 0.0], [100.0, 0.0, 0.0], [100.0, H, 0.0], [-100.0, H, 0.0]]),
        ];
        Arc::new(Collision::build(&ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0))
    }

    /// **THE #382 HEADLINE, MEASURED: the fine plan is no longer a search on the calling thread.**
    ///
    /// This tier ran INLINE on the network thread, EVERY nav tick, under a 150 ms wall clock — the last
    /// A* on that thread and the last budget in the client (measured live, release/akanon: mean 15.3 ms,
    /// worst 358 ms). This test pays both costs side by side: what the net thread used to spend per tick
    /// (`find_path_local`), and what it spends now (`LocalPlanner::request`).
    ///
    /// Like its coarse counterpart above, the assertions only get SAFER under CPU load — a busy box
    /// makes the synchronous search slower, not faster.
    #[test]
    fn posting_a_fine_plan_does_not_block_the_caller() {
        let col = plane_with_a_solid_wall();
        let start = [-20.0, 0.0, 0.0];
        let goal  = [ 20.0, 0.0, 0.0]; // through the wall: the window must be fully closed

        // What the NET THREAD used to pay, synchronously, every single nav tick:
        let t0 = Instant::now();
        let outcome = col.find_path_local(start, goal, 2.0, 40.0, 4.0);
        let sync_us = t0.elapsed().as_micros();
        assert!(!outcome.threaded(), "fixture sanity: the carrot is behind a solid wall");

        // What it pays now:
        let mut lp = LocalPlanner::spawn();
        let t1 = Instant::now();
        lp.post_if_idle(LocalRequest {
            gen: 0, start, goal, cell: 2.0, bound: 40.0, carrot_tol: 4.0, collision: col.clone(),
        });
        let post_us = t1.elapsed().as_micros();

        eprintln!("NET-THREAD STALL PER NAV TICK (fine tier): synchronous {sync_us}us  ->  posted {post_us}us");
        assert!(sync_us > 1_000,
            "fixture sanity: the synchronous fine plan must cost real time ({sync_us}us) or this proves nothing");
        assert!(post_us < 500,
            "posting the fine plan must NOT block the net thread — it must be a channel send, not a search \
             (took {post_us}us). This is the ONE assertion that #382 exists to make.");
    }

    /// **THE HONESTY SPLIT.** A carrot behind a solid wall must come back as a CLOSED window
    /// (`NoWayThrough`) — a falsifiable local "no" the walker can act on — and a carrot down an open
    /// corridor must come back `Threaded`. Under the old `find_path_res` both were the same
    /// `Option<Vec<_>>` and the caller could not ask which.
    #[test]
    fn a_closed_fine_window_is_no_way_through_and_an_open_one_is_threaded() {
        let col = plane_with_a_solid_wall();

        let blocked = col.find_path_local([-20.0, 0.0, 0.0], [20.0, 0.0, 0.0], 2.0, 40.0, 4.0);
        match &blocked {
            LocalOutcome::NoWayThrough { why, .. } => {
                assert!(matches!(why, NoRoute::SearchClosed | NoRoute::StartIsolated),
                    "a walled carrot closes the window: got {why:?}");
            }
            other => panic!("a carrot behind a solid wall must CLOSE the window (NoWayThrough), got {other:?}"),
        }
        // It is a LOCAL no, and it must never be able to become the agent-facing `no_path`.
        assert_ne!(blocked.state(), "no_path");

        // The mirror image: the honesty change must not make the fine tier timid.
        let open = col.find_path_local([-30.0, 0.0, 0.0], [-10.0, 0.0, 0.0], 2.0, 40.0, 4.0);
        assert!(open.threaded(), "an open 20u run along the floor must THREAD, got {open:?}");
        assert!(open.steer().len() >= 2, "and it must carry waypoints to steer along");
    }

    /// **THE FINE TIER IS DETERMINISTIC.** With the 150 ms wall clock deleted, the same question from
    /// the same spot yields the same answer no matter what else the box is doing — the search is bounded
    /// SPATIALLY (a 40 u window at 2 u cells), not temporally. A clock-bounded search is a function of
    /// the CPU's mood; this one is a function of the geometry.
    ///
    /// (It is the same wall-clock dependence that made the coarse planner's reachable count flip
    /// 28→26→27 across identical runs before #377.)
    #[test]
    fn the_fine_plan_is_deterministic_because_it_has_no_wall_clock() {
        let col = plane_with_a_solid_wall();
        let first = col.find_path_local([-20.0, 0.0, 0.0], [20.0, 0.0, 0.0], 2.0, 40.0, 4.0);
        for i in 0..12 {
            // Load the box between runs. A wall-clock-bounded search would notice; this one cannot.
            let mut sink = 0u64;
            for k in 0..2_000_000u64 { sink = sink.wrapping_add(k ^ sink.rotate_left(7)); }
            std::hint::black_box(sink);
            let again = col.find_path_local([-20.0, 0.0, 0.0], [20.0, 0.0, 0.0], 2.0, 40.0, 4.0);
            assert_eq!(first, again,
                "run {i}: the fine plan must be a function of the GEOMETRY, not of the clock. A differing \
                 answer here means a wall-clock budget has come back.");
        }
    }

    /// **AN ABANDONED FINE PLAN MUST NEVER BE INSTALLED.** This is the real staleness path in
    /// production: the walker gets a NEW destination (or is teleported), `clear_local_plan` cancels the
    /// fine plan in flight, and the next tick posts a fresh one — but the abandoned plan is still being
    /// computed on the worker and WILL land. It describes a carrot on a route we have thrown away.
    /// Steering along it would aim the walker at ground nobody asked for; the generation check must
    /// reject it.
    ///
    /// (Note `post_if_idle` means the Navigator never *supersedes* an in-flight fine plan — it simply
    /// doesn't post on top of one. Cancellation is therefore the ONLY way a stale fine reply is
    /// produced, so it is the case that has to be pinned.)
    #[test]
    fn an_abandoned_fine_plan_is_discarded_not_applied() {
        let col = plane_with_a_solid_wall();
        let mut lp = LocalPlanner::spawn();
        let mk = |start: [f32; 3], goal: [f32; 3], col: &Arc<Collision>| LocalRequest {
            gen: 0, start, goal, cell: 2.0, bound: 40.0, carrot_tol: 4.0, collision: col.clone(),
        };
        // Plan A: the expensive walled question, so it really is in flight when we abandon it...
        assert!(lp.post_if_idle(mk([-20.0, 0.0, 0.0], [20.0, 0.0, 0.0], &col)), "A must post");
        assert!(!lp.post_if_idle(mk([0.0, 0.0, 0.0], [10.0, 0.0, 0.0], &col)),
            "a second post while one is in flight must be a NO-OP — the walker never queues fine plans");
        // ...the route is reset (a new /goto, a teleport): the plan is abandoned mid-flight.
        lp.cancel();
        // ...and the next tick posts a fresh one for the new route.
        assert!(lp.post_if_idle(mk([-30.0, 0.0, 0.0], [-10.0, 0.0, 0.0], &col)), "B must post after a cancel");

        // A's answer is computed and DOES arrive. It must be dropped on the way in; only B may land.
        let mut applied = None;
        for _ in 0..2000 {
            if let Some(r) = lp.poll() { applied = Some(r); break; }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let rep = applied.expect("a fine plan must come back");
        assert_eq!(rep.start, [-30.0, 0.0, 0.0],
            "the applied plan must be the CURRENT route's — the ABANDONED one, which was computed and did \
             arrive, must have been discarded on the way in");
        assert!(!lp.is_planning(), "handing the reply over clears the pending request");
        for _ in 0..20 {
            assert!(lp.poll().is_none(), "a stale fine reply must be DISCARDED, never queued for later");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// A DEAD fine worker must be detected and must NOT wedge navigation. Unlike the coarse planner —
    /// whose death is terminal, because nothing can route without it — the fine tier only refines an aim
    /// the coarse route already provides. So its death degrades steering to 8 u detail and the walker
    /// KEEPS WALKING. It must still be latched and said out loud (`nav_local.state = planner_dead`): a
    /// character being steered worse than the agent believes is exactly the quiet lie this project ranks
    /// above crashes.
    #[test]
    fn a_dead_fine_planner_is_reported_and_does_not_wedge_the_walker() {
        let col = plane_with_a_solid_wall();
        let mut lp = LocalPlanner::spawn();
        assert!(!lp.is_dead());
        lp.kill_worker_for_test();
        lp.post_if_idle(LocalRequest { gen: 0, start: [-20.0, 0.0, 0.0], goal: [20.0, 0.0, 0.0],
            cell: 2.0, bound: 40.0, carrot_tol: 4.0, collision: col });
        for _ in 0..50 {
            assert!(lp.poll().is_none(), "a dead planner cannot produce a plan");
            if lp.is_dead() { break; }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(lp.is_dead(), "a dead fine worker MUST be detected, not silently 'still thinking'");
        assert!(!lp.is_planning(),
            "and it must not claim a plan is in flight: `tick` only skips POSTING while one is — if that \
             stuck true, the fine tier would never be re-posted even after a restart");
        // The walker's aim does not depend on it at all — see `crate::nav::steering::steer_target`, which is total.
    }
}
