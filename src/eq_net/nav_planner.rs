//! The pathfinding WORKER THREAD (#340, #337).
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

use crate::assets::{Collision, NoRoute, PlanCtx, PlanOutcome};
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
                    if self.pending.map(|(g, _)| g) == Some(rep.gen) {
                        // Clear the pending request AND its goal together: handing the reply over is
                        // exactly the moment the plan stops being in flight.
                        self.pending = None;
                        fresh = Some(rep);
                    } else {
                        tracing::debug!("nav-planner: discarding STALE plan #{} (now waiting on {:?})",
                            rep.gen, self.pending);
                    }
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
}

/// The worker loop. Blocks on the request channel, coalesces any backlog down to the newest request
/// (an older goal is already superseded — computing it would just delay the one that matters), runs
/// the plan, and ships the outcome back.
fn worker(req_rx: Receiver<PlanRequest>, rep_tx: Sender<PlanReply>) {
    while let Ok(mut req) = req_rx.recv() {
        while let Ok(newer) = req_rx.try_recv() {
            tracing::debug!("nav-planner: superseding queued plan #{} with #{}", req.gen, newer.gen);
            req = newer;
        }
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
        let outcome = plan_path(&req.collision, req.start, req.goal, &req.avoid, req.aggro_buffer, req.goal_region);
        let plan_ms = t0.elapsed().as_millis();
        // The headline number for #340: this is the synchronous stall that used to sit on the net
        // thread (capped at 150 ms per A* call, up to ~2 s per plan). It now sits here, where the
        // only thing waiting on it is the walker.
        tracing::info!("nav-planner: plan #{} ({:.0},{:.0})->({:.0},{:.0}) took {}ms OFF the net thread → {}",
            req.gen, req.start[0], req.start[1], req.goal[0], req.goal[1], plan_ms,
            describe(&outcome));
        if rep_tx.send(PlanReply { gen: req.gen, outcome, plan_ms, goal_snapped_z }).is_err() {
            break; // Navigator gone (zone change / shutdown)
        }
    }
}

fn describe(o: &PlanOutcome) -> String {
    match o {
        PlanOutcome::Route(p) => format!("ROUTE ({} wp)", p.len()),
        PlanOutcome::Unreachable(r) => format!("UNREACHABLE ({})", r.as_str()),
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
) -> PlanOutcome {
    // ONE deadline for the WHOLE plan, not one per A* call (#340). This function makes up to 13 A*
    // calls (the route + a 12-point re-anchor ring); when each armed its OWN 150 ms budget the worst
    // case was ~2 s of stall. It is a plan-wide safety net now, and a generous one, because it no
    // longer has a real-time thread to protect.
    let ctx = PlanCtx::worker().with_goal_region(goal_region);
    let radius = PLAYER_RADIUS;

    let first = col.find_path_ex(start, goal, radius, avoid, 8.0, None, aggro_buffer, ctx);
    match first {
        // A real route, or an honest limit — either way, that's the answer.
        PlanOutcome::Route(_) | PlanOutcome::Exhausted { .. } => return first,
        // A definitive no... unless the START is what's broken, which we can still fix.
        PlanOutcome::Unreachable(NoRoute::StartIsolated) => {}
        PlanOutcome::Unreachable(_) => return first,
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
        let out = col.find_path_ex(anchor, goal, radius, avoid, 8.0, None, aggro_buffer, ctx);
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
            return out;
        }
    }
    // The start is sealed in and no nearby floor gets us out. That IS a definitive no.
    first
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{Collision, MeshData, PlanOutcome, RenderMode, ZoneAssets};

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
    #[test]
    fn an_unreachable_goal_reports_unreachable_not_a_partial_route() {
        let col = plane_with_sealed_box(1000.0, 400.0, 400.0);
        let out = plan_path(&col, [-900.0, -900.0, 0.0], [400.0, 400.0, 0.0], &[], 0.0, None);
        match &out {
            PlanOutcome::Unreachable(NoRoute::SearchClosed) => {}
            other => panic!("a walled-off goal must report a DEFINITIVE Unreachable(SearchClosed), got {other:?}"),
        }
        assert!(out.route().is_none(), "an unreachable goal must yield NO waypoints — a partial route here is the #337 lie");
    }

    /// A reachable goal on the same fixture still routes (the honesty change must not make the
    /// planner timid — the mirror image of the assertion above).
    #[test]
    fn a_reachable_goal_still_routes() {
        let col = plane_with_sealed_box(1000.0, 400.0, 400.0);
        let out = plan_path(&col, [-900.0, -900.0, 0.0], [900.0, 900.0, 0.0], &[], 0.0, None);
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
    /// is posted only once the worker is demonstrably busy with A — so A really is computed and its
    /// reply really does arrive, and the generation check is what has to reject it. An earlier
    /// version of this test posted both back-to-back; the worker's request COALESCING quietly threw
    /// A away before it ever ran, so the test passed without ever exercising the check it names.
    #[test]
    fn a_superseded_plan_is_discarded_not_applied() {
        let col = Arc::new(plane_with_sealed_box(1000.0, 400.0, 400.0));
        let mut planner = Planner::spawn();
        let mk = |goal: [f32; 3], col: &Arc<Collision>| PlanRequest {
            gen: 0, start: [-900.0, -900.0, 0.0], goal,
            avoid: vec![], aggro_buffer: 0.0, goal_region: None, collision: col.clone(),
        };
        // Plan A: the sealed goal — a full-grid search, hundreds of ms.
        let gen_a = planner.request(mk([400.0, 400.0, 0.0], &col));
        // Let the worker actually pick A up and start searching, so it can't be coalesced away.
        std::thread::sleep(std::time::Duration::from_millis(150));
        // The goal changes mid-flight. A's answer is now stale, but it is still being computed and
        // WILL arrive.
        let gen_b = planner.request(mk([-900.0, 900.0, 0.0], &col));
        assert!(gen_b > gen_a, "each request gets a fresh, increasing generation");

        // The first (and only) plan we may apply must be B's.
        let mut applied = None;
        for _ in 0..2000 {
            if let Some(rep) = planner.poll() { applied = Some(rep); break; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let rep = applied.expect("a plan must come back");
        assert_eq!(rep.gen, gen_b,
            "the applied plan must be the CURRENT goal's (#{gen_b}) — the superseded #{gen_a}, which \
             WAS computed and DID arrive, must have been discarded on the way in");
        assert!(!planner.is_planning(), "the reply we applied clears the pending request");
        // And nothing is left queued to be applied later.
        for _ in 0..20 {
            assert!(planner.poll().is_none(), "a stale reply must be DISCARDED, never queued for later");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
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
        let outcome = plan_path(&col, start, goal, &[], 0.0, None);
        let sync_us = t0.elapsed().as_micros();
        assert!(matches!(outcome, PlanOutcome::Unreachable(_)), "fixture sanity: this goal is sealed off");

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
}
