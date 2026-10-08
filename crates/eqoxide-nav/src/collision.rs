//! A*-only pathfinding over the zone's collision grid: [`CollisionAStar`] extends
//! [`eqoxide_zone_geometry::collision::Collision`] with `find_path`/`find_path_ex`/`find_path_res`
//! and their support (goal snapping, tiered search, teleport pads, climbable edges). Everything
//! here is agent/planner-only — spatial queries that render/movement code also needs (floor_z,
//! nearest_hit_t, path_clear, ...) live on `Collision` itself, in `eqoxide-zone-geometry`, not here.
//! See `docs/specs/2026-09-21-agent-harness-separation-plan-zone-geometry.md`.

use eqoxide_zone_geometry::collision::{Collision, MAX_WALK_GRADE, SWEPT_EDGE_MAX_CELL};
// NAV_AGENT_HEIGHT/NAV_NEAR_HORIZONTAL are only read by this file's own #[cfg(test)] mod tests
// (relocated test bodies quoting the flatness/headroom thresholds) — no production call site here
// uses them unqualified, so an unconditional import warns as unused.
#[cfg(test)]
use eqoxide_zone_geometry::collision::{NAV_AGENT_HEIGHT, NAV_NEAR_HORIZONTAL};

/// The DETERMINISTIC runaway bound for a whole plan: a maximum number of node expansions (#394).
///
/// # A node cap, not a wall clock
///
/// This replaced `WORKER_PLAN_BUDGET_MS = 5_000`. A wall clock makes the planner's answer a
/// function of how fast the machine is: on a loaded runner the 5 s expired before a big zone's
/// frontier closed, so a genuinely-unreachable goal came back `Exhausted(Deadline)` ("I don't
/// know") on a slow box and `Unreachable(SearchClosed)` ("no route") on a fast one — which is why
/// `main`'s CI was intermittently red on
/// `an_unreachable_goal_reports_unreachable_not_a_partial_route`. A node cap has the identical
/// runaway protection and is machine-independent. (#377's claim that the budget was "deleted" was
/// false: it was raised 150 ms → 5 s and moved off the net thread, not removed.)
///
/// It is the cap for the ENTIRE plan (`plan_path` makes up to 13 A* calls sharing one `PlanCtx`
/// budget), so a plan is bounded by one budget, not one-per-call (#340).
///
/// # The measurements this constant rests on
///
/// | measurement | status |
/// |---|---|
/// | everfrost 1,121,438, no region map — the figure that originally set this cap, and the reason it moved: it EXCEEDED the then-shipped `MAX_NODES = 1_000_000`, so `main` was at that point silently converting everfrost's honest `SearchClosed` closes into false `Exhausted` | MEASURED (pre-#849) |
/// | butcher 352,493, no region map, full workload | MEASURED (#849 review, base) |
/// | `highpass` 7,229 → 7,393 (+2.3%), full workload both sides, region map the only change — **the two arms are measured at DIFFERENT code states**, see below | MEASURED (mapless #849 review; mapped re-measured #907) |
/// | butcher **4,583,785** WITH region map, `dev`-confirmed, butcher-only, full workload, 10 h 24 m, exit 0 | MEASURED (#856) |
/// | butcher 4,583,748 WITH region map, three-zone — profile NOT captured, wall time inconsistent with the other two runs; valid as a LOWER BOUND | MEASURED (#849 review) |
/// | why those two differ by 37 nodes | **UNRESOLVED** — see below |
///
/// ## The `highpass` pair's two code states (#907)
///
/// The mapped arm read **7,394** when #849 measured it and **7,393** when #907 re-measured it on
/// this branch. #855 changed `nearest_floor`'s ray-hit acceptance window, which feeds this
/// corpus's start/goal sampling, so both figures were right at the code state they were taken
/// against. 7,393 is re-measured here
/// (`ZONES=highpass`, `worst_case_reachable_component`, reproduced twice, **`dev` profile: at
/// `0c37ca0` the `--release` form of that command did not compile — #990; #994 is the fix and
/// was open at that sha**); the mapless **7,229** is the original pre-#855 figure and was NOT
/// re-run, because
/// `open_corpus_zone` always attaches the region map, so the mapless arm cannot be reproduced
/// without editing the corpus. Read the pair as "same zone, region map the only *intended*
/// difference, arms taken either side of #855". The ratio survives that: 7393/7229 = +2.27%, which
/// is what the +2.3% above rounds from.
///
/// `water_grid.rs` restates this pair for its own argument, and #907 found the two files
/// disagreeing after only one was corrected. `both_files_state_the_same_re_measured_highpass_figure`
/// now pins them to each other, so the next re-measurement cannot update one and miss the other.
///
/// The old "~7× headroom" claim is retired **on measurement, not inference**: it was taken on a
/// `Collision` with no region data attached, and `build_zone_collision` (`src/app.rs`) is the
/// client's ONE production construction of a zone's collision grid and ALWAYS calls
/// `set_region_data`. With the map attached, `astar`'s water-descent, haul-out and
/// surface-crossing edge families are live; the growth is strongly zone-dependent and both ends of
/// the observed range are in the table (+2.3% for `highpass`, 13.0× for `butcher`).
///
/// **The production-config figure for `butcher` is 4,583,785 — 57.3% of this cap, 1.75× headroom.**
///
/// ## The unresolved 37 nodes (0.00081%)
///
/// Three mechanisms are EXCLUDED, and the list is not complete:
///
/// * **profile** — the #849 determinism audit found no profile-sensitive mechanism in the search,
///   and the earlier run's slowest search ran at 151.7 µs/node against this run's `dev`-confirmed
///   123.7, so it shows no release-like speed. (Per-node cost is not itself a profile signature:
///   the two `dev`-confirmed runs differ by 1.87×, 66.0 vs 123.7. It excludes only a *faster*
///   build.)
/// * **corpus composition** — `let mut seed: u64 = 99;` is re-seeded INSIDE `for zone in &zones`,
///   so butcher draws identical pairs whether it is one zone of three or the only zone. Excluded
///   at source. This fixes WHICH pairs, not HOW MANY.
/// * **the grid and the assets** — all three runs print `xy_cells@8u = 751120` for butcher, which
///   is derived from the loaded geometry: content evidence that they built the same grid from the
///   same assets, stronger than the file mtimes an earlier revision relied on.
///
/// **OPEN, and the leading candidate: the earlier run executed FEWER SEARCHES.** Its total was
/// 1,620.16 s, of which everfrost's and gfaydark's figure-setting searches alone are 124.34 s,
/// leaving its butcher zone ≤ 1,495.82 s — ≤ 2.08 s per pair at the nominal 720, against 4.92 s
/// for the no-region-map base and 52.02 s for this run. It would have to run at less than half the
/// per-pair cost of the mapless base while reporting a 13× larger maximum, and profile cannot
/// rescue that. A commit-range argument does not close it either. The earlier run's stdout bounds
/// its code to between `99c45b3` and `3eb8e3a`, and across that window the construct-and-search
/// path is unchanged (`eqoxide-assets` untouched, `water_grid.rs` zero non-comment changes,
/// `collision.rs`'s forty non-comment changed lines all inside this test's own body) — but that is
/// a diff over COMMITTED revisions, and the run came from a working tree whose state was never
/// recorded. An edit to the probe loop bound prints no distinguishing string, so a reduced reach
/// probe is invisible to every artefact that survives.
///
/// The obvious objection — a reduced sample should undershoot by far more than 0.00081% — has only
/// a reasoned answer: `Key = (col, row, floor_bucket)` would make the maximum a broad PLATEAU, so
/// 37 nodes is endpoint jitter. That is reasoning from the key type, not a measurement.
///
/// **None of that makes 4,583,748 wrong.** The test reports a MAX over sampled pairs, and a max
/// over any subset is a LOWER BOUND on the max over the whole: 4,583,748 < 4,583,785, the right
/// way round. Three mechanism stories have been asserted here and three were refuted. One is named
/// because its inputs survive in this doc and it is therefore re-derivable by the next editor: the
/// `dev`-vs-`release` story built by dividing the two runs' TOTAL wall times (37,453 / 1,620 =
/// 23.1×). It is refuted by the per-node figures above and **must not be reinstated**. State what
/// is excluded, state what is open, and do not supply a fourth.
///
/// # THE DECISION (#856): stays at 8,000,000
///
/// 1. **The failure mode does not call for a pre-emptive raise.** Exceeding the cap costs
///    precision (`Exhausted(NodeCap)`, "I don't know"), never correctness (never a false
///    `Unreachable(SearchClosed)`). A REACHABLE goal is found by goal-directed A* long before the
///    cap, so a bigger zone cannot make a reachable goal false-`Exhausted`. 8M is a precision
///    floor, not a safety floor.
/// 2. **Raising it has an unmeasured cost.** `MAX_NODES` is also the runaway bound: the full
///    120-start × 6-probe workload on a `ZONES=butcher` corpus already runs 10 h 24 m at the
///    current cap in a `dev` build.
/// 3. **1.75× is a fact about `butcher`, not about RoF2.** #856's second half — widen the corpus
///    with the wet Kunark/Velious ocean and lake zones that stress the water edge families this
///    cap gates — is NOT done; no such zone is baked and measured. Tracked as **#888**, which is
///    what should reopen this decision.
///
/// So margin erosion is a WATCHED risk. Two mechanisms watch it, and they watch different things:
///
/// * **`max_nodes_headroom_claim_stays_true`** — fast, every `cargo test`, no assets. It checks
///   `MEASURED_WORST_BUTCHER_PRODUCTION` against this constant using two-decimal figures
///   hand-transcribed into its own body. It guarantees **nothing** about whether the measured
///   figure still describes the client — change what `astar` admits, rebake butcher, or edit the
///   corpus and it stays green while the figures above go false.
/// * **`the_headroom_claim_window_is_closed_at_both_ends`** — fast. Pins the four boundary points
///   of the window those two tolerances jointly admit, and which bound owns each end, so the
///   interval `butcher_headroom_claim_check`'s rustdoc states is held by execution rather than by
///   prose (#909 — it was stated open at the low end and is closed there).
/// * **`the_max_nodes_prose_figures_are_anchored_to_the_arithmetic_they_restate`** — fast, in
///   `steering.rs`'s #882 citation-guard corpus. Until #910 nothing read the prose at all: #880's
///   review mutated both figures in this doc comment (to 60.0% and 2.10×), touched no code
///   literal, and got a green run on a real rebuild. The guard derives both figures from this
///   constant and the pinned measurement, then requires the derived text verbatim at each site it
///   NAMES — so editing an anchored sentence without the literals is RED, and editing a literal
///   without the prose is RED.
///
///   **It holds a NAMED LIST of sentences, not a region.** Inside this doc comment it holds
///   exactly two: the production-config headline above, and item 3 of THE DECISION. #1038's
///   round-2 review measured the previous revision of this bullet FALSE — it claimed prose-only
///   edits here were RED, and a mutation of item 3 alone, with no code literal touched, ran
///   GREEN, because item 3 was not an anchor. It is one now. Restatements of the same pair
///   elsewhere in the tree — in `worst_case_reachable_component`, in the rustdocs of
///   `MEASURED_WORST_BUTCHER_PRODUCTION`, `butcher_headroom_claim_check` and the two fast
///   checkers above, and in `water_grid.rs`, which the citation corpus does not contain — are
///   enumerated as measured-and-unheld in the guard's own rustdoc. This bullet does not cover
///   them. The `60.0%`/`2.10×` above is a record of what #880's review typed, not a restatement
///   of the arithmetic, and is deliberately left unanchored so that it stays true as history.
/// * **`worst_case_reachable_component`** — the `#[ignore]`d ~10 h corpus run, the only thing that
///   can re-derive the number. It asserts `worst < MAX_NODES` and, since #880, its freshly measured
///   `butcher` close against `MEASURED_WORST_BUTCHER_PRODUCTION`. **That is the only comparison
///   between this constant and the world, and it exists only when someone performs the run.**
///
/// Neither could have caught the "~7×" claim: 8,000,000 / 1,121,438 = 7.13 was arithmetically
/// consistent with its own constant for its entire life and false about the WORLD. Demonstrated by
/// execution in #880's review, which rebuilt the pin around 1,121,438 with `14.0`/`7.13` literals
/// and got a GREEN run.
pub const MAX_NODES: usize = 8_000_000;

/// Deterministic node cap for the FINE local tier (#394).
///
/// The fine search is bounded SPATIALLY — a 40 u window at 2 u cells, ~1257 XY cells × a few z-tiers —
/// so its frontier genuinely closes at ~800–3700 nodes in practice (measured). This cap is therefore a
/// pure runaway backstop that a real fine plan never hits. Node count rather than clock for the
/// reason [`MAX_NODES`] gives.
///
/// **Why #382 moves this tier off the net thread even though it is already deterministic:** the fine
/// search's cost is dominated by PER-NODE collision work (`column_floors` + capsule sweeps), NOT by node
/// count. Measured worst case (release, corpus): a 1.34 s fine plan that closed just ~3681 nodes —
/// ~366 µs/node in dense stacked geometry. So there is **no node cap that bounds this search's WALL TIME
/// without cutting legitimate routes** (normal fine searches close ~800–1200 nodes). A cap keeps the
/// answer honest and deterministic; only moving OFF the net thread keeps an occasional 1.3 s fine plan
/// from stalling the network loop. Nothing waits on the fine worker, and the walker keeps steering on
/// the last good plan meanwhile (#382).
pub const NET_TIER_NODE_CAP: usize = 40_000;

/// A planner edge for an intra-zone teleport pad (#403). Stepping into the `index` DRNTP footprint
/// relocates the character to `dest` — a DISCONTINUOUS spatial link terrain-follow A* cannot express
/// as cell-to-cell connectivity. Modelled as an edge `source cell → dest cell` so a goal reachable
/// ONLY across a pad plans a complete `Route` instead of flooding a component that can't contain it
/// and returning a false `Unreachable(SearchClosed)` (an agent-honesty violation — the goal really
/// IS reachable).
///
/// Both ends are FLOOR points, resolved (and honesty-gated) once by [`Collision::resolve_teleport_pads`]:
/// `source` is a reachable interior floor point of the footprint (the cell A* must reach for the edge
/// to fire), `dest` is the pad's advertised same-zone arrival snapped to walkable floor. This is the
/// PLANNER side of the pad; the MOVEMENT side (the auto-cross that fires when the character physically
/// stands on the footprint) is the pre-existing #368/#503 machinery.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PadEdge {
    /// DRNTP zone-point index of the pad footprint region (the `OP_SendZonepoints` `iterator`).
    pub index: i32,
    /// A reachable FLOOR point INSIDE the footprint — the edge SOURCE (server coords `[east, north, z]`).
    pub source: [f32; 3],
    /// The pad's advertised same-zone arrival, snapped to walkable floor — the edge TARGET.
    pub dest: [f32; 3],
}

/// A planner edge for a climbable surface — a ladder (#309). Entering the volume and climbing lifts
/// the character from anywhere in its span to a dismount floor at the top: like [`PadEdge`], a
/// DISCONTINUOUS link terrain-follow A* cannot express, because the connecting geometry is a
/// VERTICAL face and [`MAX_WALK_GRADE`] correctly refuses it.
///
/// Crushbone's moat is the motivating case: ~10 units of vertical wall between the waterline and the
/// rim against a ~2u haul-out, with five `LADDER14` objects placed around it, and no other way out.
/// Without this edge A* floods the moat, closes its frontier and returns `Unreachable(SearchClosed)`
/// for a goal the native client can plainly reach — the same class of honesty violation #403 fixed
/// for pads.
///
/// The DISMOUNT end is resolved and honesty-gated once by [`Collision::resolve_climb_edges`]: a
/// ladder whose top has no standable floor beside it yields NO edge, so the planner never routes a
/// character up something it would then be stuck on top of. See [`crate::climb`] for what about this
/// mechanic is client-derived (the `LADDER` name trigger) and what is an unverified guess (the
/// motion constants) — every route that uses one of these edges is counted and disclosed.
#[derive(Clone, Debug, PartialEq)]
pub struct ClimbEdge {
    /// The climbable volume: where the character must be to mount, and how far up it goes.
    pub volume: eqoxide_zone_geometry::climb::ClimbVolume,
    /// Standable floor at the top, beside the ladder — the edge TARGET (`[east, north, z]`).
    pub dismount: [f32; 3],
}

/// Per-plan context for `find_path_res`: the things that must be shared across the several A* calls
/// one logical plan makes, rather than re-armed per call.
#[derive(Clone, Default)]
pub struct PlanCtx {
    /// The plan's runaway bound: a maximum number of node expansions **across the WHOLE plan** (#340,
    /// #394). `plan_path` makes up to 13 A* calls (1 primary + a 12-point `StartIsolated` re-anchor
    /// ring), and `search_tiered` makes up to 2 clearance passes inside each — this is the budget for
    /// ALL of them together, not one each.
    ///
    /// A node count, not a wall clock, for the reason [`MAX_NODES`] gives — and deliberately
    /// UNREPRESENTABLE as one: there is no `Option<Instant>` field here and no method that builds
    /// one, so a clock-dependent search is not merely discouraged (#394).
    ///
    /// `None` = the global `MAX_NODES` backstop. A caller may set a TIGHTER cap (the tiers do — see
    /// [`NET_TIER_NODE_CAP`]). Whichever bites, the outcome is `Exhausted(NodeCap)` — an honest
    /// "I stopped looking", never a "no route".
    pub node_cap: Option<usize>,
    /// The plan-wide RUNNING TOTAL of node expansions, shared by every A* call in the plan (#394 review).
    ///
    /// This is what makes `node_cap` a WHOLE-PLAN bound rather than a per-call one. The wall-clock
    /// version got that for free — one absolute `Instant`, checked by all 13 calls — but expansions
    /// accumulate, so the running total must be shared explicitly. The first plan owner
    /// (`plan_path`, or a standalone `find_path_ex`/`find_path_res`) materialises it via
    /// [`PlanCtx::ensure_budget`]; every call it spawns clones the same `Arc`.
    ///
    /// `None` only in a `PlanCtx` that has not yet entered a plan (e.g. `PlanCtx::worker()` before it
    /// reaches `find_path_ex`); the plan owner fills it in before the first search runs.
    pub expanded: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    /// Zone-point index of a `DRNTP` zone-line region we are routing to. When set, A* accepts
    /// arrival at ANY cell whose (XY, floor) lies inside that region — not just the one goal cell
    /// at the right tier. A region's representative point is an interior point of a VOLUME, so its
    /// z is structurally never a floor height and a single cell+tier test on it is unsound (#229).
    pub goal_region: Option<i32>,
    /// Intra-zone teleport-pad edges available to this plan (#403). Each is a same-zone DRNTP
    /// translocator A* may route THROUGH as a discontinuous graph edge. Resolved and honesty-gated
    /// by [`Collision::resolve_teleport_pads`] from the caller's `OP_SendZonepoints` list (only pads
    /// with a REAL advertised same-zone destination that lands on walkable floor appear here — a pad
    /// with no destination creates no edge, so the planner never fabricates reachability). Empty for
    /// the vast majority of plans (zones with no same-zone pads), which pay nothing.
    pub teleport_pads: Vec<PadEdge>,
    /// The plan's DIAGNOSTIC edge trace (#608): when present, every A* call records the per-edge
    /// accept/reject verdicts it actually made into this shared trace (locked ONCE per call, never
    /// per edge). `None` (the default — every legacy caller and the fine local tier) records
    /// nothing and pays one `Option` check per recording site. The coarse worker
    /// (`planner::worker_impl`) arms it so the published `NavDebugSnapshot` carries what the
    /// planner DID rather than a consumer's re-derivation — see `crate::diagnostics`.
    pub trace: Option<crate::diagnostics::SearchTraceHandle>,
}

impl PlanCtx {
    /// A context bounded by a fresh node cap, shared across the plan's A* calls.
    pub fn with_node_cap(cap: usize) -> Self {
        PlanCtx { node_cap: Some(cap), ..Default::default() }
    }
    /// Materialise the plan-wide expansion counter if it isn't already present, and return the ctx.
    ///
    /// Called by the PLAN OWNERS — `plan_path`, and standalone `find_path_ex`/`find_path_res` — so that
    /// every A* call in one logical plan shares ONE running total. Idempotent: a ctx that already has a
    /// counter (because `plan_path` set it before fanning out to `find_path_ex`) keeps that same shared
    /// `Arc`, which is exactly how the 13 calls come to share a budget.
    pub fn ensure_budget(mut self) -> Self {
        if self.expanded.is_none() {
            self.expanded = Some(std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        }
        self
    }
    /// The pathfinding worker's context: bounded only by the global [`MAX_NODES`] backstop, which is
    /// generous enough that a real whole-zone close reaches `SearchClosed` (chosen by measurement). Its
    /// answer no longer depends on the clock (#394); nothing real-time waits on this thread.
    pub fn worker() -> Self { Self::default() }
    /// The fine local tier's context: a node-cap backstop (see [`NET_TIER_NODE_CAP`]); the tier is
    /// really bounded by its 40u spatial window.
    pub fn net_tier() -> Self { Self::with_node_cap(NET_TIER_NODE_CAP) }
    pub fn with_goal_region(mut self, idx: Option<i32>) -> Self {
        self.goal_region = idx;
        self
    }
    /// Attach the plan's intra-zone teleport-pad edges (#403). See [`PadEdge`] /
    /// [`Collision::resolve_teleport_pads`].
    pub fn with_teleport_pads(mut self, pads: Vec<PadEdge>) -> Self {
        self.teleport_pads = pads;
        self
    }
    /// Arm the plan's diagnostic edge trace (#608). See [`PlanCtx::trace`].
    pub fn with_trace(mut self, trace: crate::diagnostics::SearchTraceHandle) -> Self {
        self.trace = Some(trace);
        self
    }
    /// How many A* calls the trace has recorded so far (0 when untraced). Used by
    /// `planner::plan_path_with_ctx` to stamp which calls produced the RETURNED outcome.
    pub fn trace_calls_len(&self) -> usize {
        self.trace.as_ref().map_or(0, |t| t.lock().unwrap().calls.len())
    }
    /// Stamp the half-open call range whose outcome the plan returned. No-op when untraced.
    pub fn trace_stamp_outcome(&self, range: (usize, usize)) {
        if let Some(t) = &self.trace {
            t.lock().unwrap().outcome_calls = range;
        }
    }
    /// The call index of the most recent `find_path_ex_tiered` invocation's ANSWERING search
    /// (the winning tier/anchor retry — see `Search::trace_call`). `None` when untraced.
    pub fn trace_last_answer(&self) -> Option<usize> {
        self.trace.as_ref().and_then(|t| t.lock().unwrap().last_answer)
    }
}

/// Why a search stopped WITHOUT closing its frontier: it hit its node cap. Means "I don't know",
/// never "no".
///
/// A second variant, `Deadline` (a wall-clock timeout), was deleted on purpose (#394 — see
/// [`MAX_NODES`]). The one-variant enum is kept rather than folded away so `PlanOutcome::Exhausted`
/// has a clean place to name future *deterministic* limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanLimit {
    /// The node cap (`PlanCtx::node_cap`, or the global `MAX_NODES`) was hit.
    NodeCap,
}

impl PlanLimit {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanLimit::NodeCap => "search_node_cap",
        }
    }
}

/// Why no route exists. Every variant is a DEFINITIVE, falsifiable "no" — the search either never
/// had a valid question to answer, or it closed its whole reachable frontier without finding the
/// goal. A timeout is NEVER one of these (that's [`PlanLimit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoRoute {
    /// No collision geometry loaded (still zoning) — nothing can be planned at all.
    NoGeometry,
    /// The GOAL has no walkable floor under or near it: it is inside solid rock, off the mesh, or
    /// floating in the air far above any ground. No amount of searching can accept arrival there,
    /// so we fail immediately and loudly instead of flooding the grid and returning a greedy
    /// partial that the walker drives into a wall (#337).
    GoalNotWalkable,
    /// The START's reachable component is a handful of cells — the character is boxed in (standing
    /// inside a tree trunk / on a slope face). The caller (`plan_path`) retries from a re-anchored
    /// start before believing this.
    StartIsolated,
    /// The search CLOSED its entire reachable frontier and the goal was not in it. This is the real
    /// "you cannot walk there from here".
    SearchClosed,
}

impl NoRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            NoRoute::NoGeometry      => "no_geometry",
            NoRoute::GoalNotWalkable => "goal_not_walkable",
            NoRoute::StartIsolated   => "start_isolated",
            NoRoute::SearchClosed    => "search_closed",
        }
    }
}

/// The HONEST outcome of a path plan (#337, #356).
///
/// The old planner returned `Option<Vec<Waypoint>>`, which conflated three completely different
/// answers into one `None`/partial: "here is your route", "there is no route", and "I gave up".
/// The walker could not tell them apart, so it silently walked a timed-out partial route into a
/// wall, retried 8×, and froze at `nav_state: blocked` — a lie that disguised the real nav root
/// cause for months. These three variants are the whole point of the change.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanOutcome {
    /// A COMPLETE route that reaches the goal. The only variant the walker may treat as a plan.
    Route(Vec<[f32; 3]>),
    /// DEFINITIVE: no route exists. An honest, falsifiable "no" the agent can act on.
    ///
    /// `goal_blocked_by` / `frontier_blocked_by` are the agent-honesty payload (#378 Phase 2,
    /// design §5a), computed on the COLD path — only when a plan has already failed, never in the
    /// hot A* loop. They are two DIFFERENT, actionable facts:
    ///
    /// * `goal_blocked_by` — one `Traversability::can_occupy(goal)` — "is my goal itself
    ///   impossible to stand at?" Definitive: if it is `Some`, no search could ever have succeeded.
    ///   This is what explains `GoalNotWalkable`.
    /// * `frontier_blocked_by` — one `can_traverse(best_toward → the next step toward the goal)` —
    ///   "I got as close as *here*; the thing between me and the goal is a **Wall** at (x,y,z)."
    ///   This explains `SearchClosed`, the common wedge where the goal is perfectly walkable but the
    ///   character's component does not contain it (a sealed corridor).
    ///
    /// Honesty about the honesty channel: `frontier_blocked_by` is ONE blocking fact, not
    /// necessarily the only one and not necessarily the one to fix — it is *named* as such in the
    /// API (`frontier_blocked_by`, not `reason`). Both are `None` when the diagnosis could not be
    /// computed (e.g. no `best_toward`); a missing diagnosis is honest silence, never a fabrication.
    Unreachable {
        reason: NoRoute,
        goal_blocked_by: Option<crate::traversability::Blockage>,
        frontier_blocked_by: Option<crate::traversability::Blockage>,
    },
    /// The search was cut short (`limit`) before closing its frontier: "I DON'T KNOW", not "no".
    /// `progress` is a partial route toward the reachable frontier, present ONLY when it makes
    /// GENUINE goal-ward progress (see `PARTIAL_MIN_UNITS`) — walk it and re-plan from the far end.
    ///
    /// **Deliberately carries NO blockage.** A cut-short search did not close its frontier, so it
    /// does not KNOW what stopped it; inventing a blockage here would be a fabrication. "I don't
    /// know" stays "I don't know" (#337/#356 discipline).
    Exhausted { limit: PlanLimit, progress: Option<Vec<[f32; 3]>> },
}

impl PlanOutcome {
    /// The COMPLETE route, if this outcome is one. A partial route is deliberately NOT returned
    /// here: treating it as a plan is exactly the #337 lie.
    pub fn route(&self) -> Option<&Vec<[f32; 3]>> {
        match self { PlanOutcome::Route(p) => Some(p), _ => None }
    }
    /// A machine-readable reason, surfaced to agents via `nav_reason` on GET /v1/observe/debug.
    pub fn reason(&self) -> &'static str {
        match self {
            PlanOutcome::Route(_) => "route",
            PlanOutcome::Unreachable { reason, .. } => reason.as_str(),
            PlanOutcome::Exhausted { limit, .. } => limit.as_str(),
        }
    }
}

/// The HONEST outcome of the FINE LOCAL steering search — the bounded 2 u tier that actually steers
/// the character along the last ~40 u of the committed coarse route (#382).
///
/// # Why this is NOT `PlanOutcome`
///
/// Two differences, and both of them are safety properties rather than taste:
///
/// 1. **A bounded search's "no" is a statement about its WINDOW, never about the goal.** The fine
///    search only ever closes the frontier *inside* `LOCAL_BOUND` (40 u). "I could not reach the
///    carrot" therefore means "not through this 40 u window" — it is *not* evidence that the goal is
///    unreachable, and it must never be able to become `nav_state: no_path`. Giving this tier a
///    `PlanOutcome` would put an `Unreachable` variant in the hands of the steering loop, and
///    `Unreachable` is the one word in this codebase that means a **definitive, falsifiable no**.
///    There is deliberately no way to spell that here.
/// 2. **Every variant carries `steer`.** `PlanOutcome::Unreachable` carries no waypoints on purpose
///    (walking a partial you have proven leads nowhere is the #337 lie). But the fine tier's partial
///    is not a route proposal — it is a *steering hint*, re-planned continuously, and it is load-
///    bearing: with it wiped, a halas swimmer floating at the water's edge stopped swimming and
///    wedged at the shoreline while the coarse planner cheerfully re-issued a perfect 78-waypoint
///    route across the water, every tick, for 8 attempts (#377 review, N1). "I cannot reach the
///    carrot" does not imply "I cannot usefully move."
///
/// # The distinction that matters
///
/// [`LocalOutcome::NoWayThrough`] (the window's frontier CLOSED) and [`LocalOutcome::Exhausted`] (the
/// search was CUT SHORT) look identical from outside — both are "the steer path stops short of the
/// carrot" — and for as long as the fine tier ran under a 150 ms wall clock they *were* identical:
/// one `Option<Vec<_>>`, no way to ask which. The walker armed the proactive coarse re-plan (#246) on
/// both, so **a timeout was silently laundered into "the coarse route ahead is blocked"**. Under CPU
/// load that fired on routes that were perfectly threadable. Telling the two apart is the whole point
/// of this type — see `nav::steering::arms_coarse_replan`.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalOutcome {
    /// A complete fine route from the character to the carrot. The healthy case.
    Threaded(Vec<[f32; 3]>),
    /// The window's frontier CLOSED without reaching the carrot: inside this 40 u window there is
    /// genuinely no way through to it (the coarse corridor skims something the 8 u grid missed).
    /// A falsifiable *local* no — and the ONLY outcome that may arm a proactive coarse re-plan.
    NoWayThrough {
        steer: Vec<[f32; 3]>,
        /// Which flavour of local dead-end (`search_closed`, `start_isolated`, `goal_not_walkable`,
        /// `no_geometry`). Reported verbatim so an agent can tell "the corridor is walled" from
        /// "*I* am the one who is wedged".
        why:   NoRoute,
    },
    /// The search was CUT SHORT by `MAX_NODES` before closing its window: "**I don't know**", not
    /// "no". It must never arm a coarse re-plan, and it must never reach the agent as `no_path`.
    ///
    /// There is no `PlanLimit::Deadline` here in practice — the fine tier arms no wall clock
    /// (`PlanCtx::default()`), which is exactly what #382 deleted — but the variant is typed on
    /// `PlanLimit` so that a limit, whatever its kind, can only ever be spelled as "I stopped
    /// looking".
    Exhausted { limit: PlanLimit, steer: Vec<[f32; 3]> },
}

impl LocalOutcome {
    /// The waypoints to STEER along this tick — a complete fine route, or the best partial toward the
    /// carrot. Always available (possibly empty); the walker never has to wait for it.
    pub fn steer(&self) -> &[[f32; 3]] {
        match self {
            LocalOutcome::Threaded(p) => p,
            LocalOutcome::NoWayThrough { steer, .. } | LocalOutcome::Exhausted { steer, .. } => steer,
        }
    }
    /// Did the fine plan actually REACH its carrot?
    pub fn threaded(&self) -> bool { matches!(self, LocalOutcome::Threaded(_)) }
    /// The state word published as `nav_local.state` on GET /v1/observe/debug.
    ///
    /// **None of these is `no_path`, and none of them can become it.** A bounded window cannot prove
    /// a goal unreachable, so this tier is structurally incapable of saying so — see the type docs.
    pub fn state(&self) -> &'static str {
        match self {
            LocalOutcome::Threaded(_)       => "threaded",
            LocalOutcome::NoWayThrough { .. } => "no_way_through",
            LocalOutcome::Exhausted { .. }  => "exhausted",
        }
    }
    /// The machine-readable WHY, surfaced as `nav_local.reason`.
    pub fn reason(&self) -> &'static str {
        match self {
            LocalOutcome::Threaded(_)          => "threaded",
            LocalOutcome::NoWayThrough { why, .. } => why.as_str(),
            LocalOutcome::Exhausted { limit, .. }  => limit.as_str(),
        }
    }
}

/// The raw result of ONE A* run, before it is turned into an honest [`PlanOutcome`].
#[derive(Debug, Default)]
pub struct Search {
    /// `(route, reached_goal)`. `reached_goal == false` = a PARTIAL route toward the frontier.
    path:     Option<(Vec<[f32; 3]>, bool)>,
    /// `Some` = the search was CUT SHORT and its frontier is NOT closed, so "the goal was not
    /// reached" means *I don't know*. `None` = the frontier closed (or the question was invalid) —
    /// only then may a missing route be reported as "no route exists".
    limit:    Option<PlanLimit>,
    /// Set when we can name WHY there is no route (invalid goal / boxed-in start). `None` with
    /// `limit: None` and no path = the frontier simply closed without the goal in it.
    no_route: Option<NoRoute>,
    /// Straight-line ground (units) toward the goal that a partial route actually closes.
    progress: f32,
    /// Nodes whose expansion completed — how big the explored component is.
    closed_n: usize,
    /// The CLOSEST-to-goal standing position the search actually reached (`best_toward`), in world
    /// space `[east, north, floor_z]`. Carried so the COLD honesty path (`find_path_ex`) can name
    /// the obstruction that ended the journey — the wall between the frontier and the goal (#378
    /// Phase 2, design §5a). `None` when the search closed nothing useful.
    best_toward: Option<[f32; 3]>,
    /// The diagnostic-trace call index THIS search recorded into (#608 / #615 review F4). Because
    /// tier retries (`search_tiered`) and anchor retries (`search`) each run their own `astar`
    /// call, the `Search` that wins carries the id of ITS OWN call — so `plan_path_with_ctx` can
    /// stamp `outcome_calls` to exactly the DECIDING call, never a losing pass whose rejections
    /// would then be drawn over the route the walker is successfully walking. `None` when untraced.
    trace_call: Option<usize>,
}

impl Search {
    fn no_route(r: NoRoute) -> Self { Search { no_route: Some(r), ..Default::default() } }
}

/// The minimum straight-line ground (units) a PARTIAL route must close toward the goal before the
/// walker is allowed to walk it. The old bar was ONE nav cell (8u) — so a search that inched a
/// single cell toward an unreachable goal produced a "route" the walker drove into a wall and then
/// wedged on (#337). A partial exists to let a long journey be walked in stages, not to let a
/// wedged character shuffle; 48u = 6 nav cells is a stage, 8u is a shuffle.
pub const PARTIAL_MIN_UNITS: f32 = 48.0;

/// The clearance a route is planned at BY DEFAULT — deliberately larger than the character.
///
/// Fitting is not walking. A route planned at exactly `PLAYER_RADIUS` is allowed to skim a wall, a
/// cliff lip or the edge of a bridge with *zero* margin, and the walker — which slides on contact
/// and gets shoved around by server position corrections — falls off it. So plan with room, and
/// fall back to the minimum only where the roomy route does not exist (`search_tiered`).
///
/// **2 × `PLAYER_RADIUS`**: one radius to fit, one radius of margin. Chosen by measuring the
/// fallback rate over 1200 start/goal pairs in the cached zones — the fraction of routes that only
/// exist at the minimum clearance, i.e. where the second A* pass is spent for nothing:
///
/// | preferred | qeynos2 | gfaydark | freportw | akanon |
/// |-----------|---------|----------|----------|--------|
/// | 1.5 ×     |   0 %   |    0 %   |    8 %   |  32 %  |
/// | **2.0 ×** | **0 %** |  **3 %** | **16 %** |**33 %**|
/// | 2.5 ×     |   0 %   |     —    |   18 %   |  39 %  |
/// | 3.0 ×     |   0 %   |     —    |   28 %   |  48 %  |
///
/// Routability is identical at every value (the fallback guarantees it) — what moves is how often
/// the roomy tier fails. At 2× the generous tier still carries the large majority of routes in the
/// open and city zones; by 3× the fallback is close to a coin-flip in the tight indoor ones
/// (Ak'Anon's gnome tunnels), which is two searches to answer what one could. 2× buys a full
/// body-width of standing room without making the exception the rule.
pub const NAV_PREFERRED_CLEARANCE: f32 = eqoxide_core::physics::PLAYER_RADIUS * 2.0;

/// A floor this close under a point in water = STANDING (wading), not floating. Shared by the
/// floating START anchor (#329/#197p2) and the floating GOAL anchor (water-nav design §4d) so the
/// two ends of a swim plan agree on what "floating" means.
pub const FOOTING: f32 = 4.0;

/// A reached floor within this of the goal's floor counts as the SAME tier. It is the single
/// tolerance that has to agree in three places or the nav layer lies to itself: (1) `astar` accepts
/// a searched cell as the goal only when its floor is within this of `goal_floor`; (2)
/// `goal_z_was_snapped` uses it to decide the caller's z IS a real tier; and now (3) the arrival
/// predicate (`steering::arrival_action`) uses it to decide the walker actually reached the goal's
/// FLOOR, not a floor above/below it (#344). 8u is deliberately narrower than the 20u walk step-up:
/// a zone-line region point measured 12.9u above its floor (gfaydark→felwithea) was a DIFFERENT tier
/// than that floor, and the old ±20 window wrongly fused the two into a phantom tier (#229). 8u
/// rejects that wrong tier while still tolerating standing height, water float, and a single step-up
/// (the native STEP_UP is 2u). See the long note in `astar` where `goal_floor` is resolved.
pub const GOAL_TIER_TOL: f32 = 8.0;

/// How the planner is about to CHANGE the goal it was given (#337 honesty / water-nav design §4d).
/// `Some` means the character will NOT arrive at the z the caller asked for — an accommodation,
/// and an accommodation presented as compliance is a lie, so it rides `PlanReply` out to the agent
/// (`nav_reason: goal_z_snapped` + the message log), never quietly performed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GoalSnap {
    /// Dry: the asked z is on no floor anywhere near — the goal moves to the column's floor at `z`.
    ToColumnFloor { z: f32 },
    /// Water: the asked z is SUBMERGED (a real underwater floor the caller named, or a point deep
    /// in open water). The plan honours the XY, but the walker cannot dive and hold depth —
    /// buoyancy only rises, and its arrival test is 2D — so it arrives FLOATING at the water
    /// surface `surface_z` above the goal. Claiming `arrived` at a depth never reached, without
    /// this qualifier, is exactly the silent lie the agent-honesty invariant forbids.
    ToWaterSurface { surface_z: f32 },
}

/// The share of a plan's node budget the GENEROUS clearance pass may spend before it is abandoned in
/// favour of the minimum-clearance pass that actually decides the answer.
///
/// The roomy tier is an OPTIMISATION — a nicer route when one is cheaply available. The minimum tier
/// is the one that knows whether a route exists at all, so it must never be starved by the tier that
/// merely prefers a better one. Both passes share the CALLER'S single node budget (see `search_tiered`):
/// giving each its own would make one plan cost two budgets.
const GENEROUS_BUDGET_SHARE: f32 = 0.4;

/// The generous pass's node cap: a SLICE of the caller's budget, **never a fresh one** (#394).
///
/// One plan, one budget. A pass that arms its own cap makes a plan cost N budgets instead of one. The
/// cap is subdivided ONCE by the caller of this function so the two passes together stay within the one
/// budget the caller set.
///
/// `None` in → `None` out: an unbudgeted plan stays unbudgeted (bounded only by the global `MAX_NODES`
/// backstop); this function must never INVENT a cap, only subdivide one. A node cap, unlike the
/// wall-clock deadline this replaced, does not "run down" between the passes — it is a fixed budget,
/// so the split is a plain fraction with no clock to drift.
fn generous_node_cap(caller: Option<usize>) -> Option<usize> {
    caller.map(|cap| ((cap as f32) * GENEROUS_BUDGET_SHARE) as usize)
}

pub trait CollisionAStar {
    fn resolve_teleport_pads(&self, advertised: &[(i32, [f32; 3])]) -> Vec<PadEdge>;

    fn teleport_pad_footprints(&self, index: i32) -> Vec<[f32; 3]>;

    fn inflate_route_off_corners(&self, route: &mut [[f32; 3]], radius: f32, buffer: f32);

    fn walk_profile_ok(&self, a: [f32; 2], az: f32, b: [f32; 2], bz: f32,
        probe_down: f32) -> bool;

    fn find_path(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]], allow_partial: bool) -> Option<Vec<[f32; 3]>>;

    fn snap_goal_to_column_floor(&self, goal: [f32; 3]) -> Option<f32>;

    fn goal_z_was_snapped(&self, goal: [f32; 3]) -> Option<GoalSnap>;

    fn resolve_goal_floor(&self, goal: [f32; 3]) -> Option<f32>;

    #[allow(clippy::too_many_arguments)]
    fn find_path_res(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        allow_partial: bool, cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> Option<Vec<[f32; 3]>>;

    #[allow(clippy::too_many_arguments)]
    fn find_path_ex(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> PlanOutcome;

    #[allow(clippy::too_many_arguments)]
    fn find_path_ex_tiered(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> (PlanOutcome, bool);

    fn find_path_local(&self, start: [f32; 3], goal: [f32; 3], cell: f32, bound: f32, carrot_tol: f32)
        -> LocalOutcome;

    fn climb_edges(&self) -> Vec<ClimbEdge>;
}

impl CollisionAStar for Collision {
    /// Resolve advertised intra-zone teleport pads into planner edges (#403). `advertised` =
    /// `(zone_point_index, dest [east, north, z])` for `OP_SendZonepoints` entries whose target is
    /// THIS zone — the caller (the walker) filters `zone_points` to `zp.zone_id == gs.world.zone_id` and
    /// drops the keep-position sentinel, so only pads that REALLY relocate the character within the
    /// zone reach here. A cross-zone line is never in this list, so it can never be turned into an
    /// intra-zone teleport.
    ///
    /// **HONESTY GUARD (#403).** A pad becomes a [`PadEdge`] ONLY when BOTH ends resolve to walkable
    /// floor:
    /// * a footprint LEAF has a standable trigger floor — a floor in its column a character could
    ///   stand on such that the auto-cross would fire ([`teleport_pad_source`], which validates the
    ///   SAME `zone_line_at`-at-standing-height condition the mover uses), AND
    /// * the advertised destination snaps to a floor.
    ///
    /// **One edge PER LEAF (#403 review A).** A same-index DRNTP footprint can be baked as several
    /// horizontally-separated leaves, and A* may only be able to reach SOME of them; emitting an edge
    /// for every leaf with a standable trigger (all sharing the one destination) means a leaf in the
    /// character's own reachable component always fires — a single arbitrary leaf could sit in a
    /// component A* can't reach and give a FALSE `Unreachable`.
    ///
    /// An `index` that is not a DRNTP region in this zone's `.wtr`, a leaf whose trigger volume FLOATS
    /// above its floor (no standable point fires the cross — a #266-style translocator), or a
    /// destination out over the void (no floor in its column), yields NO edge for that case — the
    /// planner must never invent a link that would strand the character (the inverse honesty bug:
    /// unreachable reported reachable). O(leaves) cache reads + a couple of floor probes each.
    fn resolve_teleport_pads(&self, advertised: &[(i32, [f32; 3])]) -> Vec<PadEdge> {
        const STEP_UP: f32 = 20.0;
        const MAX_DROP: f32 = 100.0;
        let mut out = Vec::new();
        for &(index, dest) in advertised {
            // DEST: snap the advertised arrival onto walkable floor. `None` (void — no floor anywhere
            // in the destination column) → no edge for this pad, rather than a link that drops the
            // character into nothing.
            let Some(dz) = self.nearest_floor(dest[0], dest[1], dest[2], STEP_UP, MAX_DROP)
                .or_else(|| self.snap_goal_to_column_floor(dest)) else { continue };
            let dest_floor = [dest[0], dest[1], dz];
            // SOURCE: one edge per footprint LEAF of this index that has a standable trigger floor.
            for &(idx, p) in self.zone_line_regions().iter().filter(|(i, _)| *i == index) {
                if let Some(source) = teleport_pad_source(self, idx, p) {
                    out.push(PadEdge { index, source, dest: dest_floor });
                }
            }
        }
        out
    }


    /// **Every standable footprint leaf of one DRNTP index — independent of any advertised
    /// destination (#543).** One point per leaf a character could actually stand on such that the
    /// crossing fires; empty when the index has no standable leaf at all (a floating / #266 region).
    ///
    /// This is the "can the agent physically take this pad?" question, and it is deliberately
    /// separate from [`Collision::resolve_teleport_pads`], which answers the different question "may
    /// A* route THROUGH this pad?" and needs BOTH ends to resolve. Conflating them hides a pad the
    /// agent could take behind a verdict about the advertised *destination* — which is precisely the
    /// datum #543 established the client cannot trust. The disclosure path uses this; the planner
    /// path uses `resolve_teleport_pads`.
    fn teleport_pad_footprints(&self, index: i32) -> Vec<[f32; 3]> {
        self.zone_line_regions().iter()
            .filter(|(i, _)| *i == index)
            .filter_map(|&(idx, p)| teleport_pad_source(self, idx, p))
            .collect()
    }


    /// **Corner-buffer route inflation (#685, owner-directed).** Push each INTERIOR route waypoint
    /// AWAY from a nearby convex wall corner so the walker takes ONE smooth wider arc with clearance,
    /// instead of hugging the apex and wiggling through (the sloppy/slow behaviour of the pure
    /// carrot-shorten clamp). This is agent-radius inflation done with the zone's clearance spokes —
    /// the same 16 radial rays `clearance_probe` / the F11 overlay draw.
    ///
    /// At each interior waypoint: the nearest wall's direction + distance come from the min spoke. If
    /// the waypoint sits within `radius + buffer` of that wall, it is offset OUTWARD (away from the
    /// wall) by the deficit — but **never past the midpoint** toward the OPPOSITE wall: the antipodal
    /// spoke bounds the push, so a narrow-but-passable corridor is CENTRED, never sealed (the owner's
    /// "only offset when there's room" discipline). An offset that would break the route (an adjacent
    /// segment no longer `path_clear`) is reverted, so inflation can only ever improve or no-op a
    /// waypoint — it cannot invent a wall-crossing the spokes' 4u horizon did not see.
    ///
    /// Endpoints (start = live position, goal) are fixed. Removes the CAUSE (route grazing the corner)
    /// rather than reacting to it; the carrot LOS clamp (`carrot_los_clear`) stays as a light backstop.
    fn inflate_route_off_corners(&self, route: &mut [[f32; 3]], radius: f32, buffer: f32) {
        if self.cols == 0 || route.len() < 3 { return; }
        use eqoxide_zone_geometry::body::PLAYER_BODY;
        const SPOKES: usize = 16;
        const CAP: f32 = 4.0; // the ClearanceField's WALL_CAP horizon (spokes saturate here)
        let want = radius + buffer;
        // Radial wall clearance at (x,y): distance to nearest solid at the body probe heights, per
        // spoke. Returns the 16 spoke distances (index k ↔ angle k/16·τ).
        let spokes_at = |x: f32, y: f32, floor_z: f32| -> [f32; SPOKES] {
            let mut d = [CAP; SPOKES];
            for (k, dk) in d.iter_mut().enumerate() {
                let a = (k as f32) / (SPOKES as f32) * std::f32::consts::TAU;
                let (dx, dy) = (a.cos(), a.sin());
                for hz in PLAYER_BODY.planner_probes() {
                    let from = [x, y, floor_z + hz];
                    let to = [x + dx * CAP, y + dy * CAP, floor_z + hz];
                    if let Some(t) = self.nearest_hit_t(from, to) { *dk = dk.min(t * CAP); }
                }
            }
            d
        };
        for i in 1..route.len() - 1 {
            let p = route[i];
            let floor_z = self.nearest_floor(p[0], p[1], p[2], 3.0, 8.0).unwrap_or(p[2]);
            let d = spokes_at(p[0], p[1], floor_z);
            // Nearest wall = the min spoke.
            let (mut imin, mut dmin) = (0usize, f32::MAX);
            for (k, &dk) in d.iter().enumerate() { if dk < dmin { dmin = dk; imin = k; } }
            if dmin >= want { continue; } // already ≥ radius+buffer of any wall → nothing to do (open ground)
            let a = (imin as f32) / (SPOKES as f32) * std::f32::consts::TAU;
            let wall_dir = [a.cos(), a.sin()];              // toward the nearest wall
            let d_opp = d[(imin + SPOKES / 2) % SPOKES];    // room on the far side
            let deficit = want - dmin;
            // Room to push out. When the far side is OPEN (its spoke saturated at the horizon) there is
            // no opposite wall to worry about, so we may take the full deficit — reaching radius+buffer
            // at the corner. When the far side is a real WALL (spoke < CAP) we never push past the
            // midpoint between the two walls, so a narrow corridor CENTRES and is never widened into
            // the far wall.
            let opp_room = if d_opp >= CAP - 1e-3 { deficit } else { ((d_opp - dmin) * 0.5).max(0.0) };
            let push = deficit.min(opp_room);
            if push <= 1e-3 { continue; }
            let np = [p[0] - wall_dir[0] * push, p[1] - wall_dir[1] * push, p[2]];
            // Only accept an offset whose adjacent segments do not cross a wall — validated with the
            // SAME chest-height LOS the walker steers by (`carrot_los_clear`), not the foot-height
            // swept `path_clear`: a neighbour waypoint may itself legitimately sit within a radius of a
            // wall (a goal against a wall), and path_clear's radius-extended shoulder would spuriously
            // reject the reconnecting segment there even though the centreline never crosses. The push
            // is already bounded away from BOTH walls (near wall + midpoint), so a centreline check is
            // the right conservatism — it rejects an offset that would chord across a *different* wall.
            if self.carrot_los_clear(route[i - 1], np, radius) && self.carrot_los_clear(np, route[i + 1], radius) {
                route[i] = np;
            }
        }
    }


    /// #630 — the MAXIMUM-LOCAL-RISE check on a rising walk edge: does the floor PROFILE along the
    /// hop `a → b` stay within what the controller can actually climb, or does it concentrate the
    /// rise into a near-vertical face the whole-hop AVERAGE grade hides?
    ///
    /// The defect this closes (#617/#309/#329/#482): the walk edge's grade check divides the
    /// endpoint rise by the whole hop length (8u orthogonal, ~11.3u diagonal), so a flat approach
    /// ending in a 12.8u vertical face averages to grade 1.13 and passes `MAX_WALK_GRADE = 1.2` —
    /// the same face is correctly rejected orthogonally (12.8/8 = 1.6). The feet-clearance ray
    /// doesn't catch it either: that ray interpolates from `az + feet` to `bz + feet`, so on a
    /// steeply-rising hop it has already gained most of the altitude by the far end and skims over
    /// a face near the destination. The controller's real capability is `physics::STEP_UP = 2.0` of
    /// discrete riser plus walking a grade-`MAX_WALK_GRADE` slope — nowhere near a 12.8u face.
    ///
    /// So: sample the floor columns at ~`WALK_PROFILE_SPACING` intervals along the hop and ask
    /// whether ANY floor sequence from `az` to `bz` fits the controller's envelope on every
    /// sub-segment:
    ///
    /// ```text
    /// local_rise ≤ seg_run · MAX_WALK_GRADE + step_up
    /// ```
    ///
    /// — a slope it can walk plus one discrete step it can climb. Two deliberate permissive
    /// choices, both anti-over-tightening (rejecting a genuinely walkable slope would turn a wedge
    /// into a `no_path` — a different regression):
    ///
    /// * The envelope is the UNION of the two real capabilities, so a steep stair-stepped profile
    ///   stays legal.
    /// * At each probe the check advances to the HIGHEST floor reachable within the envelope
    ///   (greedy max-altitude — equivalent to the exhaustive search over floor sequences, since a
    ///   higher `prev_z` never reaches less later). Tracking the floor NEAREST the previous sample
    ///   instead would follow a ground plane running UNDERNEATH a walkable ramp and falsely reject
    ///   the ramp. A probe over a void (no floor in the window) is skipped: its run rolls into the
    ///   next sub-segment, never a free pass.
    ///
    /// The check can therefore only reject a hop where NO within-envelope profile exists — strictly
    /// tighter than main's average-only test, never looser — and a single vertical face taller than
    /// `spacing · MAX_WALK_GRADE + step_up ≈ 6.8u` cannot hide anywhere along the hop, in ANY
    /// direction: the fixed probe spacing is what removes the diagonal's extra permissiveness (its
    /// longer run no longer launders the same face).
    ///
    /// **Where the rejection actually happens — read this before editing the loop.** No INTERMEDIATE
    /// probe ever returns `false`. The envelope is applied to them as the CAP on how far `prev_z` may
    /// climb at each step (`column_floors` is windowed to `prev_z + allow`), so a probe whose only
    /// floor sits above the envelope simply finds nothing and leaves `prev_z` where it was — exactly
    /// like a probe over a void. The single verdict is the FINAL comparison, `bz - prev_z <= allow`:
    /// a face too tall to be climbed en route shows up as a `prev_z` that never got off the low
    /// ground, and the hop is refused at its destination. This is deliberate (it is what makes the
    /// greedy walk equivalent to the exhaustive search over floor sequences); a per-segment
    /// early-`false` would reject legal profiles whose floors are simply sampled unevenly.
    ///
    /// Callers gate on `rise > step_up` — a hop whose endpoint rise the controller can plainly step
    /// has nothing to launder, and flat terrain pays zero extra queries.
    fn walk_profile_ok(&self, a: [f32; 2], az: f32, b: [f32; 2], bz: f32,
        probe_down: f32) -> bool {
        /// Probe spacing (u): half a coarse cell. Bounds the tallest hideable face at
        /// `spacing · MAX_WALK_GRADE + step_up ≈ 6.8u` while tolerating the local roughness of
        /// baked zone terrain (rocky slopes, stair runs) that a per-2u check would over-reject.
        /// On the fine 2u tier a whole hop is under one spacing, so this check adds nothing there
        /// (the whole-hop grade cap is already tighter) — no fine-tier behaviour change.
        const WALK_PROFILE_SPACING: f32 = 4.0;
        let step_up = eqoxide_zone_geometry::body::PLAYER_BODY.step_up;
        let run = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
        let segs = (run / WALK_PROFILE_SPACING).ceil().max(1.0) as i32;
        let mut prev_z = az;
        let mut prev_t = 0.0_f32;
        for i in 1..=segs {
            let t = i as f32 / segs as f32;
            let allow = (t - prev_t) * run * MAX_WALK_GRADE + step_up;
            if i == segs {
                // The final sample IS the candidate destination floor — never re-derived, so the
                // profile check and the edge it guards cannot disagree about where the hop ends.
                return bz - prev_z <= allow;
            }
            // Highest floor reachable within the envelope (column_floors returns high→low, already
            // bounded to [prev_z - probe_down, prev_z + allow]).
            let fz = self.column_floors(a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t,
                prev_z, allow, probe_down).into_iter().next();
            let Some(fz) = fz else { continue }; // void under the probe: fold into the next segment
            prev_z = fz;
            prev_t = t;
        }
        true
    }


    /// A* over the collision grid: a walkable waypoint path from `start` to `goal` that routes
    /// AROUND walls (a plain collide-and-slide toward the goal only slides along one). Returns
    /// cell-center waypoints `[east, north]` (start-exclusive, goal-inclusive) or None if no geometry / no route.
    /// Walkability = a floor exists under the cell; an edge needs a small floor-height step and
    /// a clear chest-height segment between cell centers.
    /// `avoid` is a set of XY points (nearby NPC positions) the route should skirt — see the
    /// aggro-avoidance note below (#67). Pass `&[]` for a pure geometric route.
    /// A* over the walkable-floor grid. `radius` is the clearance the route must keep from geometry
    /// (smaller threads narrower gaps). When `allow_partial` is true and the goal cell is
    /// unreachable, returns a path to the nearest-reachable cell toward the goal instead of `None`
    /// (so a stranded character still makes progress, #188); when false, only a route that reaches
    /// the goal cell is returned. `None` = no progress possible (truly boxed in).
    /// Default nav plan: the standard 8u grid over the WHOLE zone (long-range routing). This is the
    /// coarse tier of the two-tier planner (#nav-multires) — cheap over big distances but blind to
    /// sub-8u detail (thin ramps, narrow openings). The FINE local tier calls `find_path_res` with a
    /// small cell + a search bound to thread that detail near the walker.
    fn find_path(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]], allow_partial: bool) -> Option<Vec<[f32; 3]>> {
        self.find_path_res(start, goal, radius, avoid, allow_partial, 8.0, None, 0.0, PlanCtx::default())
    }


    /// The goal's floor when the caller's `z` could NOT be resolved to a tier — i.e. it sits below
    /// every floor in the goal's column (an agent passing a rough `z`, usually 0, or a map coord).
    /// Returns the nearest floor anywhere in that column: the goal the caller meant.
    ///
    /// `Some(z)` here means **the client is about to change the caller's goal**. That is an
    /// accommodation, and an accommodation presented as compliance is a lie — so it is reported, not
    /// quietly performed: the planner surfaces it as `nav_reason: goal_z_snapped` and says so in the
    /// message log, rather than letting an agent that asked for `z: 0` be told `arrived` at `z: 47`
    /// without ever learning its goal was moved.
    ///
    /// `None` = there is no floor anywhere in the column: the goal is off the mesh, and that is a
    /// genuine `GoalNotWalkable` (fail loudly, don't search).
    fn snap_goal_to_column_floor(&self, goal: [f32; 3]) -> Option<f32> {
        const COLUMN: f32 = 1000.0; // the whole column: "is there ANY floor at this XY?"
        self.nearest_floor(goal[0], goal[1], goal[2], COLUMN, COLUMN)
    }


    /// Did resolving `goal` require CHANGING what the caller asked for? `Some` = yes — see
    /// [`GoalSnap`] and [`Collision::snap_goal_to_column_floor`]. Used by the planner to tell the
    /// agent, so the accommodation is never silent. Mirrors `astar`'s resolution order EXACTLY
    /// (tier → floating water surface → floor-beneath projection → column snap): a report that
    /// disagrees with what the search actually anchored to would be its own lie — the old shape of
    /// this function warned "SNAPPING to floor z=<pool bottom>" for a water goal the search was
    /// about to anchor to the surface.
    fn goal_z_was_snapped(&self, goal: [f32; 3]) -> Option<GoalSnap> {
        const GOAL_DROP: f32 = 400.0;
        // 1. A real tier at the caller's z is honoured as asked (`astar` aims the plan at it) —
        //    UNLESS that floor is UNDERWATER deeper than the arrival tolerance: the walker cannot
        //    dive and hold it, so it will arrive FLOATING at the surface, more than GOAL_TIER_TOL
        //    above the asked depth. That is an accommodation and gets the water qualifier (§4d).
        //    (`surface_z` is `None` unless `f + 1` is actually in water, and `None` for an
        //    unbounded column — both read as "nothing to report" here.)
        if let Some(f) = self.nearest_floor(goal[0], goal[1], goal[2], GOAL_TIER_TOL, GOAL_TIER_TOL) {
            if let Some(s) = self.region_map().and_then(|w| w.surface_z(goal[0], goal[1], f + 1.0)) {
                if s - f > GOAL_TIER_TOL {
                    return Some(GoalSnap::ToWaterSurface { surface_z: s });
                }
            }
            return None;
        }
        // 1.5 A navigable mid-water NODE at the asked depth is NOT an accommodation — the plan
        //      arrives THERE, at the asked depth (Slice 2, design §7.3). Report no snap, so the agent
        //      is never told its goal moved to the surface when the planner honoured the depth.
        if water_node_goal_z(self, goal).is_some() { return None; }
        // 2. A floating water goal anchors to the surface. Within arrival tolerance of the asked z
        //    that IS the goal the caller named (same ±GOAL_TIER_TOL fuzz a dry tier enjoys);
        //    further away — a point asked for DEEP in open water — it is an accommodation.
        if let Some(s) = floating_goal_surface(self, goal) {
            return ((s - goal[2]).abs() > GOAL_TIER_TOL)
                .then_some(GoalSnap::ToWaterSurface { surface_z: s });
        }
        // 3. A volume point over a floor projects DOWN onto it — the tier the caller meant (#229),
        //    not a change of goal.
        if self.floor_beneath(goal[0], goal[1], goal[2], GOAL_TIER_TOL, GOAL_DROP).is_some() {
            return None;
        }
        // 4. No floor near the asked z at all: the dry column snap, reported.
        self.snap_goal_to_column_floor(goal).map(|z| GoalSnap::ToColumnFloor { z })
    }


    /// The z the walker PHYSICALLY comes to rest at when it has arrived at `goal` — the anchor the
    /// arrival test (`steering::arrival_action`) compares the walker's z against, NOT the caller's
    /// raw z. Its resolution order mirrors [`Collision::goal_z_was_snapped`] EXACTLY (the same chain
    /// reported to the agent as `goal_z_snapped`), so what the planner PROMISED the agent and what
    /// arrival DEMANDS cannot drift apart:
    ///   • a sloppy goal z (0, a map coordinate) that `astar` projected onto a real floor does NOT
    ///     cause a false "not arrived" — both ends agree on the resolved floor;
    ///   • a goal genuinely on a DIFFERENT dry floor (an NPC one storey up, whose z IS a real tier
    ///     here) resolves to THAT storey's floor, so the walker standing a storey below is
    ///     > GOAL_TIER_TOL away in z and is honestly NOT arrived (#344); and
    ///   • a goal on a SUBMERGED floor deeper than GOAL_TIER_TOL below the surface resolves to the
    ///     WATER SURFACE, because the walker cannot dive and hold that depth — buoyancy floats it at
    ///     the surface (design §4d). This is the `ToWaterSurface` accommodation `goal_z_was_snapped`
    ///     reports; anchoring arrival to the submerged floor instead would demand a depth the walker
    ///     physically never reaches, turning §4d's honest "arrived at the surface" into a permanent
    ///     false "not arrived" / dishonest `blocked` (#344 review).
    /// `None` only when there is no walkable floor anywhere in the goal's column — the same
    /// `GoalNotWalkable` case `astar` fails loudly on; the caller falls back to the raw goal z.
    fn resolve_goal_floor(&self, goal: [f32; 3]) -> Option<f32> {
        const GOAL_DROP: f32 = 400.0; // a volume point can sit far above its floor
        // 1. A real tier at the caller's z — but a tier submerged deeper than the arrival tolerance
        //    is one the walker floats above, so arrival is at the SURFACE (mirrors clause 1 of
        //    `goal_z_was_snapped`). `surface_z` is `None` for a dry tier or an unbounded column.
        if let Some(f) = self.nearest_floor(goal[0], goal[1], goal[2], GOAL_TIER_TOL, GOAL_TIER_TOL) {
            if let Some(s) = self.region_map().and_then(|w| w.surface_z(goal[0], goal[1], f + 1.0)) {
                if s - f > GOAL_TIER_TOL { return Some(s); }
            }
            return Some(f);
        }
        // 1.5 A navigable mid-water NODE at the asked depth is a real arrival tier (Slice 2, design
        //     §7.3): the walker's target IS that depth, not the surface projection. Mirrors `astar`'s
        //     resolution and `goal_z_was_snapped` so plan-target ≡ arrival-anchor ≡ what the agent is
        //     told — the one-chain contract this function exists to hold.
        if let Some(z) = water_node_goal_z(self, goal) { return Some(z); }
        // 2. A goal floating in water anchors to the surface it floats on.
        // 3. A volume point over a floor projects DOWN onto that floor.
        // 4. No floor near the asked z: the nearest floor anywhere in the column.
        floating_goal_surface(self, goal)
            .or_else(|| self.floor_beneath(goal[0], goal[1], goal[2], GOAL_TIER_TOL, GOAL_DROP))
            .or_else(|| self.snap_goal_to_column_floor(goal))
    }


    /// BEST-EFFORT route at an arbitrary grid resolution `cell`, optionally bounded to `max_search`
    /// units of the start (so a FINE plan stays local + cheap even if it hits an obstacle).
    /// `cell` = 8.0 + `max_search` = None reproduces the classic whole-zone nav grid.
    ///
    /// This returns "the best waypoints I have" — a complete route, or (with `allow_partial`) a
    /// partial one toward the frontier — and CANNOT say why it has none. It is for LOCAL STEERING
    /// (the fine 2u tier, whose partials are a 40u steering hint the walker re-plans every tick),
    /// never for answering an agent's "can I get there?". For that, use [`Collision::find_path_ex`],
    /// which distinguishes "no route" from "I gave up" (#337/#356).
    #[allow(clippy::too_many_arguments)]
    fn find_path_res(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        allow_partial: bool, cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> Option<Vec<[f32; 3]>> {
        // Plan owner: materialise the plan-wide budget so this call's internal A* passes share one cap.
        let ctx = ctx.ensure_budget();
        let (s, _tight) = search_tiered(self, start, goal, radius, avoid, cell, max_search, aggro_buffer, ctx);
        match s.path {
            Some((p, true)) => Some(p),
            Some((p, false)) if allow_partial => Some(p),
            _ => None,
        }
    }


    /// The HONEST plan (#337, #356): run A* and report which of the three genuinely-different
    /// answers came back — a complete `Route`, a definitive `Unreachable`, or an `Exhausted` search
    /// that hit a limit and therefore does not know.
    ///
    /// There is no `allow_partial` flag: a partial route is not an answer to "route me to the goal",
    /// it is a consolation prize, so it can only ever ride along inside `Exhausted` — and only when
    /// it makes real progress (`PARTIAL_MIN_UNITS`). A search that CLOSES its frontier without
    /// reaching the goal now returns `Unreachable` and NO waypoints at all: walking a partial in
    /// that case is exactly the silent wedge of #337.
    #[allow(clippy::too_many_arguments)]
    fn find_path_ex(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> PlanOutcome {
        self.find_path_ex_tiered(start, goal, radius, avoid, cell, max_search, aggro_buffer, ctx).0
    }


    /// [`find_path_ex`] plus the per-route TIER (#378 Phase 2 / design §4c): `true` = the route only
    /// existed at the MINIMUM clearance (a tight door/bridge threaded with no margin — a riskier
    /// path), `false` = the roomy preferred tier carried it (or there is no route). This is the
    /// per-route fact the zone-lifetime `nav_tight` counter cannot give; the worker puts it on
    /// `PlanReply` so an agent sees `nav_tier` for the route it is actually walking.
    #[allow(clippy::too_many_arguments)]
    fn find_path_ex_tiered(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> (PlanOutcome, bool) {
        // Plan owner (when called standalone). When `plan_path` drives this, `ctx` already carries a
        // shared counter, so `ensure_budget` is a no-op and all 13 calls keep sharing the one budget.
        let ctx = ctx.ensure_budget();
        let (s, tight) = search_tiered(self, start, goal, radius, avoid, cell, max_search, aggro_buffer, ctx.clone());
        // Report which trace call ANSWERED (the winning tier/anchor retry) so the plan owner can
        // stamp `outcome_calls` to exactly the deciding call (#615 review F4).
        if let Some(t) = &ctx.trace {
            t.lock().unwrap().last_answer = s.trace_call;
        }
        let best_toward = s.best_toward;
        let outcome = match s.path {
            Some((p, true)) => PlanOutcome::Route(p),
            other => match s.limit {
                // Cut short → "I don't know". Hand back the partial ONLY if it makes genuine
                // goal-ward progress, so the walker can advance a stage and re-plan from there.
                Some(limit) => {
                    let progress = other
                        .filter(|_| s.progress >= PARTIAL_MIN_UNITS)
                        .map(|(p, _)| p);
                    PlanOutcome::Exhausted { limit, progress }
                }
                // Frontier CLOSED → a definitive no. Now, and ONLY now (the COLD path, after a
                // plan has already failed), name WHY and WHERE (#378 Phase 2). No partial.
                None => {
                    let reason = s.no_route.unwrap_or(NoRoute::SearchClosed);
                    let (goal_blocked_by, frontier_blocked_by) =
                        diagnose_unreachable(self, goal, radius, cell, best_toward);
                    PlanOutcome::Unreachable { reason, goal_blocked_by, frontier_blocked_by }
                }
            },
        };
        // `tight` is only meaningful for a real route; a no-route/exhausted answer has no tier.
        (outcome, tight)
    }


    /// The FINE LOCAL STEERING plan (#382): a bounded A* at `cell` resolution (2 u) within `bound`
    /// units (40 u) of the character, aimed at a carrot on the committed coarse route.
    ///
    /// # It runs to COMPLETION. There is no wall clock.
    ///
    /// `PlanCtx::default()` arms **no deadline**, and that is the point of #382. This search used to
    /// run inline on the network thread under a 150 ms budget — a residual net-thread stall of the
    /// same class that caused the #257/#302 linkdead bugs (measured, release, akanon: mean 15.3 ms,
    /// worst **358 ms**), and, worse, a budget that made its answer unfalsifiable: a search that ran
    /// out of clock and one that proved the corridor impassable came back as the same short
    /// `Option<Vec<_>>`. It now runs on `crate::planner::LocalPlanner`, where nothing real-time waits on
    /// it, so it can afford the truth.
    ///
    /// Termination is **spatial, not temporal**: `max_search = Some(bound)` confines the frontier to a
    /// 40 u disc at 2 u cells — a few thousand cells — so the search genuinely closes. `MAX_NODES`
    /// remains as an absolute backstop, and hitting it yields [`LocalOutcome::Exhausted`]: a
    /// *distinguishable* "I stopped looking", never a silent "there is no way through". A consequence
    /// worth stating plainly: with the clock gone, this tier is **deterministic** — the same character
    /// in the same spot now steers the same way whatever else the box is doing.
    ///
    /// Clearance: always the MINIMUM (`PLAYER_RADIUS`). See `search_tiered` — a bounded plan does not
    /// choose a route, it threads one the coarse planner already chose with room, so the generous pass
    /// buys nothing and costs a second search.
    /// `carrot_tol` is how near the carrot counts as REACHING it. This is not slop — it is the
    /// question. A carrot is an interpolated point on a line between two 8 u COARSE cell centres, so
    /// its z is a coarse floor height and its XY routinely lands a couple of units off the fine grid's
    /// walkable floor (or inside a wall corner the 8 u grid cut). A* would then never accept arrival at
    /// the goal *cell*, and a plan that gets the walker exactly where it needs to go would be reported
    /// as "there is no way through" — which is not merely pessimistic, it would arm a spurious coarse
    /// re-plan (#246) and publish a false `nav_local`. Measured: judging on A*'s strict goal-cell flag
    /// instead of this tolerance loses **16 of 1447** real carrots across the zone corpus.
    ///
    /// So the fine tier's success criterion is the walker's: *did the plan get me to the carrot?* —
    /// the same test the walker itself applied before this change (`LOCAL_CELL * 2`).
    fn find_path_local(&self, start: [f32; 3], goal: [f32; 3], cell: f32, bound: f32, carrot_tol: f32)
        -> LocalOutcome
    {
        // Plan owner: the fine tier's node cap, with a shared plan-wide counter so its two anchor
        // searches (char + cell-centre retry) draw from one budget (#394). The cap is a runaway
        // backstop; the 40u `bound` is what really terminates this search.
        let ctx = PlanCtx { node_cap: Some(NET_TIER_NODE_CAP), ..PlanCtx::default() }.ensure_budget();
        let (s, _tight) = search_tiered(self, 
            start, goal, eqoxide_core::physics::PLAYER_RADIUS, &[], cell, Some(bound), 0.0, ctx);
        // The partial rides along in EVERY variant — it is a steering hint, not a route proposal.
        let steer: Vec<[f32; 3]> = s.path.map(|(p, _)| p).unwrap_or_default();
        let reached = steer.last()
            .is_some_and(|w| (w[0] - goal[0]).hypot(w[1] - goal[1]) <= carrot_tol);
        if reached { return LocalOutcome::Threaded(steer); }
        match s.limit {
            // Cut short → "I don't know". Never "the corridor is blocked".
            Some(limit) => LocalOutcome::Exhausted { limit, steer },
            // Frontier CLOSED inside the window → a falsifiable local "no way through".
            None => LocalOutcome::NoWayThrough { steer, why: s.no_route.unwrap_or(NoRoute::SearchClosed) },
        }
    }


    fn climb_edges(&self) -> Vec<ClimbEdge> {
        resolve_climb_edges(self)
    }
}

    /// Which of this zone's climbable surfaces may A* actually route through, and to where (#309).
    ///
    /// An edge exists only when the ladder's top has floor a character can STAND on within
    /// [`climb::DISMOUNT_Z_TOL`] of it — probed on a ring just outside the volume, because you step
    /// OFF a ladder sideways, not through it. A ladder into a sealed ceiling, one whose top is a
    /// sheer face, or one that ends in mid-air therefore produces no edge at all.
    ///
    /// That gate is the whole point, and it is the same honesty rule
    /// [`Collision::resolve_teleport_pads`] applies to a pad whose destination is over the void:
    /// **a link the planner cannot show leads somewhere standable is never offered**. Reporting a
    /// goal reachable and then stranding the character on top of a ladder is a worse failure than
    /// reporting it unreachable, because the agent has no way to tell it happened.
    fn resolve_climb_edges(col: &Collision) -> Vec<ClimbEdge> {
        use eqoxide_zone_geometry::climb::DISMOUNT_Z_TOL;
        use crate::traversability::{Point, Traversability, Tier};
        if col.cols == 0 { return Vec::new(); }
        // Minimum clearance, matching the fine tier: a dismount ledge beside a ladder is a
        // "does the character fit" question, and demanding the generous margin would refuse the
        // narrow walkways ladders characteristically serve.
        let trav = Traversability::new(col, Tier::Minimum.units(), col.cell_size, 0.0, false);
        let mut out = Vec::new();
        for v in col.climb_volumes() {
            let c = v.center();
            // Step outward past the volume's own footprint, plus the body's width, so the probe
            // lands on ground BESIDE the ladder rather than in the panel it just climbed.
            let pr = eqoxide_core::physics::PLAYER_RADIUS;
            let reach = [(v.hi[0] - v.lo[0]) * 0.5 + pr, (v.hi[1] - v.lo[1]) * 0.5 + pr];
            let mut best: Option<[f32; 3]> = None;
            for (da, db) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0),
                             (0.7, 0.7), (0.7, -0.7), (-0.7, 0.7), (-0.7, -0.7)] {
                let (a, b) = (c[0] + da * reach[0], c[1] + db * reach[1]);
                let Some(fz) = col.nearest_floor(a, b, v.top_z, DISMOUNT_Z_TOL, DISMOUNT_Z_TOL) else { continue };
                if (fz - v.top_z).abs() > DISMOUNT_Z_TOL { continue }
                if !trav.can_occupy_fast(Point::new([a, b], fz)) { continue }
                // Prefer the highest qualifying ledge: with several in the window, the one nearest
                // the ladder's top is the one a climb actually delivers you onto.
                if best.is_none_or(|p| fz > p[2]) { best = Some([a, b, fz]); }
            }
            if let Some(dismount) = best {
                out.push(ClimbEdge { volume: v.clone(), dismount });
            }
        }
        out
    }

    /// The interior water-NODE z a mid-water `goal` resolves to, or `None` when the goal is not a
    /// genuinely navigable point inside a stored water span (design §7.3). This is the single
    /// authority the three goal-resolution sites share (`astar`, [`Self::resolve_goal_floor`],
    /// [`Self::goal_z_was_snapped`]) so what A* plans to, what arrival demands, and what the agent is
    /// told cannot drift apart.
    ///
    /// `Some(z)` ONLY when the asked z lies inside a navigable span — i.e. a real mid-water state the
    /// #534/#540 carving already proved free of solid. A sloppy z above the swim plane (the classic
    /// `z = 0` over a pool) is NOT in a span → `None` → the caller falls through to the existing
    /// floating-surface anchor, unchanged. So this REFINES the water-goal chain (a truly submerged,
    /// navigable ask now arrives at depth instead of being projected to the surface) without
    /// disturbing the surface-projection path for every non-navigable ask.
    fn water_node_goal_z(col: &Collision, goal: [f32; 3]) -> Option<f32> {
        let col = col.water_grid_active()?.column_at(goal[0], goal[1])?;
        col.span_containing(goal[2])?;      // only a genuinely navigable, in-water ask
        col.nearest_node_z(goal[2])
    }

    /// The standable trigger floor point inside a DRNTP footprint leaf whose representative interior
    /// point is `p` (server coords), or `None` if no floor a character could stand on is inside the
    /// region (a floating / #266 leaf).
    ///
    /// This validator accepts a floor `fz` only when **standing on it (feet + 1u) is INSIDE the
    /// region** (`zone_line_at(p0, p1, fz + 1.0) == index`), then REPORTS the footprint at the floor
    /// `fz` — the point a character actually stands on — NOT at feet+1. The auto-cross MOVER
    /// (`eqoxide-net action_loop`) fires via [`Collision::zone_line_at_standing`], which sweeps the
    /// standing capsule span `[feet, feet + height]`; feet+1 lies within that span, so a footprint
    /// accepted here is one the mover will fire on when the character stands on it — the two agree.
    ///
    /// That agreement is load-bearing and was once absent (#266): while the mover probed the FEET
    /// only, a DRNTP volume whose lower face floats just above the floor — the qeynos2 KoT waterfall
    /// — validated here at feet+1 but never crossed when stood on; only a jump did.
    ///
    /// It scans the footprint's whole COLUMN for a walkable floor (not the tight 2u probe
    /// `zone_line_floor_point` uses — that helper serves a zone-line GOAL, and its ±2u window can
    /// miss the real trigger floor of a footprint whose representative point sits low in a tall
    /// straddling volume, #403 review C) and keeps the first (highest) floor where standing
    /// (feet + 1u) is still inside the region.
    fn teleport_pad_source(col: &Collision, index: i32, p: [f32; 3]) -> Option<[f32; 3]> {
        const STEP_H: f32 = 20.0;
        const REGION_DROP: f32 = 400.0;
        col.column_floors(p[0], p[1], p[2], STEP_H, REGION_DROP).into_iter()
            .find(|&fz| col.region_map()
                .and_then(|w| w.zone_line_at(p[0], p[1], fz + 1.0)) == Some(index))
            .map(|fz| [p[0], p[1], fz])
    }

    /// #639: is a ROUTE'S FINAL HOP — the penultimate waypoint (`from`) onto the exact goal —
    /// walkable by the SAME predicate every intermediate A* edge passes ([`Self::walk_profile_ok`])?
    ///
    /// The goal waypoint is APPENDED to a route: the reconstruction snaps the last waypoint from the
    /// reached goal-cell centre (or, for a same-cell walk, the start) to the EXACT goal XY. Before
    /// #639 that appended hop skipped the walk-edge profile check every *other* edge gets, so a route
    /// could end in a near-vertical final face the controller cannot climb — measured in `permafrost`,
    /// final hops to grade **6.61** against `MAX_WALK_GRADE = 1.2` — while the planner still reported a
    /// COMPLETE route. That is the #630 lie reintroduced at exactly the point the agent is most likely
    /// to believe it has arrived. This restores the invariant: EVERY consecutive waypoint pair in a
    /// returned route, including the last, satisfies the walk-edge predicate.
    ///
    /// Mirrors the intermediate edge's gate EXACTLY (`astar`: `rise > step_up` before the profile
    /// check): only a RISING hop past the controller's discrete step-up can hide a face, so a flat /
    /// gentle / downhill final approach is always walkable and pays no probe — the anti-over-tightening
    /// choice, identical to the one #630 made for intermediate edges. `goal_floor` is the floor the
    /// walker actually comes to rest on (the reached goal cell's tier), NEVER the caller's raw z: a
    /// sloppy goal z (0, a map coord) would otherwise inflate the apparent rise and reject a walkable
    /// goal. Callers exempt SWIM (water/floating) and ZONE-LINE (`goal_region`) goals — walking grade
    /// governs neither.
    fn final_hop_walkable(col: &Collision, from: [f32; 3], goal_xy: [f32; 2], goal_floor: f32) -> bool {
        // = astar's local MAX_STEP_DOWN: how far down `walk_profile_ok` may look for the floor
        // profile. A drop the walker can take; it never affects the CLIMB rejection this guards.
        const FINAL_HOP_PROBE_DOWN: f32 = 60.0;
        let step_up = eqoxide_zone_geometry::body::PLAYER_BODY.step_up;
        if goal_floor - from[2] <= step_up { return true; } // nothing to launder — same gate as #630
        col.walk_profile_ok([from[0], from[1]], from[2], goal_xy, goal_floor, FINAL_HOP_PROBE_DOWN)
    }

    /// FLOATING GOAL ANCHOR (#197p2 / water-nav design §4d): the goal-side mirror of the floating
    /// START anchor in `astar`. If `goal` is a point FLOATING in water — in water (or a short
    /// downward probe under it finds water: agents routinely pass z=0 over a pool whose surface
    /// sits lower), with NO footing within [`FOOTING`] beneath it — then "goto that point" means
    /// "swim to that spot", and the only tier the character can hold there is the WATER SURFACE
    /// (buoyancy rises; it cannot dive and stay). Returns that surface: the EXACT `surface_z`
    /// plane, which is the same tier the WATER SURFACE TRAVERSAL edges emit, so A* arrival at the
    /// goal cell matches it by construction (`|fz − goal_floor| ≤ GOAL_TIER_TOL`).
    ///
    /// `None` = not a floating water goal (dry, wading with footing, no water map) — or the column
    /// is UNBOUNDED (water 200u+ up, `surface_z` = `None`): there is no surface to anchor to, and
    /// the caller must fall through to the ordinary dry resolution, never unwrap.
    fn floating_goal_surface(col: &Collision, goal: [f32; 3]) -> Option<f32> {
        let w = col.region_map()?;
        let (x, y, z) = (goal[0], goal[1], goal[2]);
        // A point IN water (checked at z and a hair under, for a goal exactly at the waterline) —
        // or ABOVE water found by a short downward probe, the same shape as the WATER SURFACE
        // TRAVERSAL probe: a pool's surface often sits below the z the caller passed (z=0 over a
        // surface at −8 must still anchor).
        const PROBE_DOWN: f32 = 20.0; // = STEP_H, the planner's step/probe range
        let probe_z = [z, z - 1.0].into_iter().find(|&pz| w.is_water(x, y, pz)).or_else(|| {
            let mut pz = z - 1.0;
            while pz >= z - PROBE_DOWN {
                if w.is_water(x, y, pz) { return Some(pz); }
                pz -= 4.0;
            }
            None
        })?;
        // Footing right under the asked point = standing (wading), not floating — exactly the
        // START anchor's test, so both ends of a plan classify the same point the same way.
        if col.nearest_floor(x, y, z, 2.0, FOOTING).is_some() { return None; }
        w.surface_z(x, y, probe_z)
    }

    /// The COLD honesty diagnosis behind an `Unreachable` (#378 Phase 2, design §5a). Runs at most
    /// twice, and only after a plan has already failed — never in the hot A* loop, and never on a
    /// successful plan.
    ///
    /// * `goal_blocked_by`: is the GOAL itself impossible to stand at? Resolve a floor anywhere in
    ///   the goal's column; if there is none, the goal is off the mesh (a `Floor` blockage). If
    ///   there is one, ask the one authority whether the body can occupy it.
    /// * `frontier_blocked_by`: from the closest-to-goal standing position the search actually
    ///   reached (`best_toward`), step one plan-cell toward the goal and ask `can_traverse` — the
    ///   wall (or drop, or waterline) that ended the journey's closest approach.
    ///
    /// Both use `Tier::Minimum` clearance (the hard floor): a blockage is only reported when even
    /// the character's own collision radius does not fit, so the diagnosis can never over-claim a
    /// wall the walker could actually have squeezed past.
    fn diagnose_unreachable(col: &Collision, goal: [f32; 3], radius: f32, cell: f32,
        best_toward: Option<[f32; 3]>)
        -> (Option<crate::traversability::Blockage>, Option<crate::traversability::Blockage>)
    {
        use crate::traversability::{Blockage, HazardKind, Point, Traversability, Tier};
        let r = radius.max(Tier::Minimum.units());
        // DRY-PLAN diagnosis: `floating: false`. A water-START failed plan could read inconsistently
        // here, but planner water nav is an unimplemented gap (#359/#197/#423) and out of scope — the
        // diagnosis is only ever consulted for dry plans until that lands.
        let trav = Traversability::new(col, r, cell, 0.0, false);

        // GOAL: definitive. A floor anywhere in the column, then the occupancy test; else off-mesh.
        let goal_blocked_by = match col.snap_goal_to_column_floor(goal) {
            Some(gz) => trav.can_occupy(Point::new([goal[0], goal[1]], gz)).err(),
            None => Some(Blockage { hazard: HazardKind::Floor, at: goal }),
        };

        // FRONTIER: the obstruction at the search's closest approach. Step one cell toward the goal
        // and ask can_traverse; the failure names the wall/drop/water between here and the goal.
        let frontier_blocked_by = best_toward.and_then(|bt| {
            let dir = [goal[0] - bt[0], goal[1] - bt[1]];
            let d = (dir[0] * dir[0] + dir[1] * dir[1]).sqrt();
            if d < 1e-3 { return None; }
            let step = cell.max(2.0);
            let to_xy = [bt[0] + dir[0] / d * step, bt[1] + dir[1] / d * step];
            // The next step's floor tier, following the terrain from the frontier's height.
            let to_z = col.nearest_floor(to_xy[0], to_xy[1], bt[2], 20.0, 60.0).unwrap_or(bt[2]);
            trav.can_traverse(Point::new([bt[0], bt[1]], bt[2]), Point::new(to_xy, to_z)).err()
        });

        (goal_blocked_by, frontier_blocked_by)
    }

    /// TIERED CLEARANCE (#358). Search at a GENEROUS clearance (`NAV_PREFERRED_CLEARANCE`) and fall
    /// back to the MINIMUM one — exactly `movement::PLAYER_RADIUS` — only when no generous route
    /// exists. Returns the answering search plus `tight`: the route only exists at the minimum, i.e.
    /// it threads a narrow door or a tight bridge with no margin to spare.
    ///
    /// Why default above the character's own size: fitting is not walking. A route planned at exactly
    /// `PLAYER_RADIUS` may skim a wall, a cliff lip or the edge of a bridge with zero margin, and the
    /// walker — which slides on contact and is shoved around by server position corrections — falls
    /// off it. Normal walking should have room.
    ///
    /// **The floor is `PLAYER_RADIUS` and it is not negotiable.** #310 removed a fallback that planned
    /// at 0.5x and 0.25x `PLAYER_RADIUS`, threading gaps narrower than the character's real collision
    /// radius and handing the walker routes it could not fit through. This is the mirror image, and the
    /// distinction is the whole design: the DEFAULT is above the radius, the FALLBACK is AT it, never
    /// under. If no route exists even at `PLAYER_RADIUS`, the honest answer is no route.
    ///
    /// **The generous tier is strictly BEST-EFFORT and can never starve the minimum tier.** The two
    /// searches share ONE budget — the caller's — and the generous pass gets a slice of it, never a
    /// budget of its own. Arming a fresh deadline per pass is how a plan quietly costs two budgets,
    /// which on the net-thread local tier is the #302 stall disease; `PlanCtx` exists precisely so a
    /// plan is bounded by one deadline no matter how many A* calls it makes. The deadline is
    /// materialised ONCE, up front, so the split cannot drift as the clock runs.
    ///
    /// Only a COMPLETE generous route is accepted. A generous partial is not evidence the goal is
    /// unreachable, only that it is unreachable *with room* — and the honest `Unreachable` /
    /// `Exhausted` verdict (#337/#356) must always come from the minimum tier, which is the one that
    /// knows whether a route exists at all.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn search_tiered_for_test(col: &Collision, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> (Search, bool) {
        search_tiered(col, start, goal, radius, avoid, cell, max_search, aggro_buffer, ctx)
    }

    #[allow(clippy::too_many_arguments)]
    fn search_tiered(col: &Collision, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> (Search, bool) {
        // The hard floor (#310). Never search below the radius the controller actually moves with.
        let minimum = radius.max(eqoxide_core::physics::PLAYER_RADIUS);
        // Tiering is a ROUTE-CHOICE mechanism, and only a plan that actually chooses a route should
        // pay for it. A BOUNDED plan (`max_search: Some`) is the fine local tier: it follows a carrot
        // on a coarse route that was ALREADY chosen with room, inside a 40u window where there is no
        // meaningful alternative to choose. Asking it for a roomy route buys nothing and costs a
        // second search, every nav tick. Measured on the production call (2u cell / 40u bound): the
        // second pass adds ~30-60% mean on top of the sweep and DOUBLES the plans that overrun a
        // 150 ms budget (blackburrow 17 -> 30 of 240). So the local tier plans at the MINIMUM
        // clearance and spends its time on the question it exists to answer — does the character FIT
        // — while the coarse planner picks the roomy route. (Both tiers are off the net thread now:
        // the coarse one since #377, the fine one since #382.)
        let chooses_a_route = max_search.is_none();
        let preferred = if chooses_a_route { minimum.max(NAV_PREFERRED_CLEARANCE) } else { minimum };
        if preferred > minimum {
            // Give the generous pass a SLICE of the budget that REMAINS at this point in the plan, and
            // let it draw from the same plan-wide counter (`..ctx.clone()` shares the `Arc`). Since both
            // passes share one running total, the minimum pass — which gets the FULL cap — always has
            // whatever the generous pass did not spend, so the roomy tier can never starve the tier that
            // actually decides whether a route exists (#302). The slice is computed on the budget LEFT,
            // not the original cap, exactly as `main`'s `generous_deadline` sliced the remaining time.
            let cap = ctx.node_cap.unwrap_or(MAX_NODES).min(MAX_NODES);
            let used = ctx.expanded.as_ref().map(|a| a.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0);
            let generous_cap = used + generous_node_cap(Some(cap.saturating_sub(used))).unwrap_or(0);
            let generous_ctx = PlanCtx { node_cap: Some(generous_cap), ..ctx.clone() };
            let s = search(col, start, goal, preferred, avoid, cell, max_search, aggro_buffer, generous_ctx);
            if matches!(s.path, Some((_, true))) { return (s, false); }
        }
        let s = search(col, start, goal, minimum, avoid, cell, max_search, aggro_buffer, ctx);
        let tight = preferred > minimum && matches!(s.path, Some((_, true)));
        // Honesty: a tight route is a RISKIER route — no margin from the walls and drops it passes.
        // An agent must be able to tell it is walking one, so count it; `/v1/observe/debug` reports
        // it as `nav_tight`. A degraded mode must never be silent.
        if tight { col.record_tight_plan(); }
        (s, tight)
    }

    /// One logical A* run: the char-anchored search, with the cell-centre-anchored retry for a
    /// boxed-in start folded in.
    ///
    /// Anchor A* at the character's true position (so its first leg is one the walker can actually
    /// take). But if the character's EXACT spot is BOXED IN — standing inside a tree trunk's or a
    /// wall's footprint, where the rays out of it are blocked or lead into a sealed pocket — that
    /// anchor strands the search in a handful of nodes. That is precisely the isolated start the
    /// cell-centre anchor + start-cell hop exist to rescue (#2/#205), so fall back to them.
    ///
    /// The retry is gated on the failed search having explored almost NOTHING, so it fires only for
    /// a boxed-in start (a few nodes, microseconds) and never doubles the cost of a genuine long
    /// search that failed on its merits.
    #[allow(clippy::too_many_arguments)]
    fn search(col: &Collision, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> Search {
        const BOXED_IN_NODES: usize = 64;
        // Is this search's verdict really "the goal is unreachable", or is it "I couldn't get OUT of
        // where the character is standing"? The two look identical from the outside — both close the
        // frontier without reaching the goal — and telling them apart is the whole job here.
        //
        // The tell is the SIZE of the component the search closed. A search that explored a handful
        // of cells did not survey the zone and rule the goal out; it never left the doorstep. The
        // character is boxed in (stood inside a tree trunk, wedged on a slope face) and the fix is to
        // re-anchor the START (#205) — not to tell the agent "there is no route", which would be a
        // FALSE definitive no, the single worst thing this planner can say.
        //
        // Note this deliberately ignores any PARTIAL route the search dribbled out. Live gfaydark:
        // the char wedged on terrain, A* closed after 1 node, the cell-centre retry crawled 5 nodes
        // and produced a 2-cell partial — and an earlier version of this function took the existence
        // of that stub as proof the search had really surveyed the zone, and reported `search_closed`
        // on a goal that was perfectly reachable from 16u away. A 2-cell stub is not a survey. Only a
        // COMPLETE route (`reached`) proves the search got anywhere.
        let boxed_in = |s: &Search| {
            s.limit.is_none()
                && s.closed_n < BOXED_IN_NODES
                && !s.path.as_ref().is_some_and(|(_, reached)| *reached)
                && s.no_route != Some(NoRoute::GoalNotWalkable)
                && s.no_route != Some(NoRoute::NoGeometry)
        };
        // Both anchors share the plan-wide budget (same `ctx.expanded` Arc): the retry draws down what
        // the first anchor left, so the two together cost one budget, not two.
        let s = astar(col, start, goal, radius, avoid, cell, max_search, aggro_buffer, ctx.clone(), true);
        if !boxed_in(&s) { return s; }
        // Anchoring at the character's exact position got nowhere. Retry from the cell centre (the
        // classic #2/#205 rescue) before believing anything.
        let mut retry = astar(col, start, goal, radius, avoid, cell, max_search, aggro_buffer, ctx, false);
        // Never LOSE a partial route by retrying. If the char-anchored search produced one and the
        // cell-centre retry produced nothing, keep the one we had: the fine local tier steers on it,
        // and dropping it is the same steering-starvation that stopped the halas swimmer — just
        // reached through the other anchor (#377 review, N1).
        if retry.path.is_none() && s.path.is_some() {
            retry = Search { path: s.path, ..retry };
        }
        if boxed_in(&retry) {
            // Both anchors are sealed in: the START is the problem, not the goal. Name it, so
            // `plan_path` re-anchors (#205) and `find_path_ex` reports `start_isolated` rather than
            // the false "no route to the goal" this used to collapse into.
            //
            // The partial route is deliberately KEPT here. `find_path_ex` already drops it on the
            // `Unreachable` path (an honest "no" carries no waypoints), so wiping it here bought
            // nothing — but it also starved `find_path_res`, the FINE LOCAL STEERING tier, which
            // legitimately runs tiny bounded searches whose component is *always* small and which
            // needs its partial as a steering hint. Live halas: a swimmer floating at the water's
            // edge has exactly such a search, and with the partial wiped the walker stopped swimming
            // and wedged at the shoreline while the coarse planner cheerfully re-issued a perfect
            // 78-waypoint route across the water, every tick, for 8 attempts.
            return Search { no_route: Some(NoRoute::StartIsolated), ..retry };
        }
        retry
    }

    /// The A* itself. `prefer_char_anchor` expands the START node from the character's own (x, y)
    /// rather than its cell centre (see `anchor_at_char`).
    ///
    /// It reports everything the honest-outcome layer needs to tell "no route exists" from "I gave
    /// up": whether the frontier was CLOSED or the search was cut short by a limit, how many nodes
    /// it closed (a handful = a boxed-in start), and how much ground a partial route actually
    /// covers. The old version returned a bare `Option`, which is why a timeout and a genuine
    /// no-route were indistinguishable for months (#337/#356).
    #[allow(clippy::too_many_arguments)]
    fn astar(col: &Collision, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx,
        prefer_char_anchor: bool) -> Search {
        use std::collections::BinaryHeap;
        use std::cmp::Ordering;
        // DIAGNOSTIC TRACE (#608): when the plan armed one, this call records the per-edge verdicts
        // it makes, AT the branch that makes them — never a re-derivation. Locked ONCE for the whole
        // call (every A* call of a plan runs sequentially on one worker thread, so this never
        // contends); untraced callers (`tr == None`) pay a single Option check per recording site.
        use crate::diagnostics::{EdgeKind, EdgeVerdict, RejectReason};
        let mut tr = ctx.trace.as_ref().map(|t| t.lock().unwrap());
        if let Some(t) = tr.as_deref_mut() { t.begin_call(radius, cell.max(1.0), prefer_char_anchor); }
        // The call THIS search records into — carried out on the Search so the winning tier/anchor
        // retry can be identified as THE deciding call (#615 review F4).
        let trace_call = tr.as_deref().map(|t| t.calls.len() - 1);
        if col.cols == 0 || col.rows == 0 {
            return Search { trace_call, ..Search::no_route(NoRoute::NoGeometry) };
        }
        // Navigate on a FINER grid than the collision broad-phase buckets (col.cell_size, ~32u).
        // At 32u, cell centers fall inside walls in tight corridors, so A* sees a fragmented graph,
        // finds no route, and the caller straight-lines into walls. An 8u nav grid keeps cell
        // centers inside corridors so A* can actually route around them; a finer cell (the local
        // tier) resolves thin ramps/openings. (The collision triangle lookup via floor_z/path_clear
        // works at any query point regardless of bucket size.)
        let cell = cell.max(1.0);
        let cols = (col.cols as f32 * col.cell_size / cell).ceil() as i32;
        let rows = (col.rows as f32 * col.cell_size / cell).ceil() as i32;
        let to_cell = |e: f32, n: f32| -> (i32, i32) {
            let c = (((e - col.origin[0]) / cell) as i32).clamp(0, cols - 1);
            let r = (((n - col.origin[1]) / cell) as i32).clamp(0, rows - 1);
            (c, r)
        };
        let center = |c: i32, r: i32| -> [f32; 2] {
            [col.origin[0] + (c as f32 + 0.5) * cell,
             col.origin[1] + (r as f32 + 0.5) * cell]
        };
        // The probe FOLLOWS the terrain: each cell's floor is found relative to the floor of the
        // cell we reached it from (so multi-level dungeons work even when the caller's start z is
        // stale, the common case). `nearest_floor` gathers ALL surfaces in the vertical column and
        // snaps to the one closest to `ref_z` — so an overhang/awning/bridge ABOVE the walkable
        // floor is never mistaken for the floor (the old single top-down ray grabbed the first hit
        // and got trapped on ceiling geometry). `up` = max step-up onto a ledge; `down` = max drop.
        const STEP_UP: f32 = 20.0;
        const MAX_DROP: f32 = 100.0;
        let floor_near = |c: i32, r: i32, ref_z: f32| -> Option<f32> {
            let p = center(c, r);
            col.nearest_floor(p[0], p[1], ref_z, STEP_UP, MAX_DROP)
        };
        let (sc, sr) = to_cell(start[0], start[1]);
        let (gc, gr) = to_cell(goal[0], goal[1]);
        // Same cell as the goal: a straight walk. Still start the route AT the character (see the
        // `path.insert(0, ...)` note at the end of the search) so pure pursuit steers along it.
        if (sc, sr) == (gc, gr) {
            // #639: even a same-cell straight walk APPENDS the goal without the walk-edge check every
            // A* edge passes — a goal on a ledge inside the start's own 8u cell is still an un-walkable
            // final face. Validate `start → goal` with the same predicate (SWIM and ZONE-LINE goals
            // exempt: `resolve_goal_floor` anchors a floating goal to the surface and a region point is
            // an interior volume point, neither governed by walking grade).
            if ctx.goal_region.is_none() && floating_goal_surface(col, goal).is_none()
                && water_node_goal_z(col, goal).is_none()
            {
                let sf = col.nearest_floor(start[0], start[1], start[2], STEP_UP, MAX_DROP).unwrap_or(start[2]);
                let gf = col.resolve_goal_floor(goal).unwrap_or(goal[2]);
                if !final_hop_walkable(col, [start[0], start[1], sf], [goal[0], goal[1]], gf) {
                    tracing::info!("find_path: goal ({:.0},{:.0},{:.1}) is in the start cell but the walk \
                        onto it exceeds the walk envelope (#639) — no walkable route (goal_not_walkable)",
                        goal[0], goal[1], goal[2]);
                    return Search { trace_call, ..Search::no_route(NoRoute::GoalNotWalkable) };
                }
                // #693: the descent mirror of #639. A goal on a LOWER STACKED tier of the start's
                // own cell (the aqueduct floor under the street the character stands on) used to
                // return a straight-down two-point "route" through solid ground — `final_hop_walkable`
                // passes every descent by design. A drop steeper than the step-down is only real if
                // the goal's column is open between the tiers; otherwise it is not walkable from here.
                let body = &eqoxide_zone_geometry::body::PLAYER_BODY;
                if gf < sf - body.step_up
                    && !col.descent_corridor_clear(goal[0], goal[1], sf, gf)
                {
                    tracing::info!("find_path: goal ({:.0},{:.0},{:.1}) is a stacked tier {:.1}u beneath \
                        the start's floor with solid ground in between (#693) — no walkable route \
                        (goal_not_walkable)", goal[0], goal[1], goal[2], sf - gf);
                    return Search { trace_call, ..Search::no_route(NoRoute::GoalNotWalkable) };
                }
            }
            return Search {
                path: Some((vec![[start[0], start[1], start[2]], [goal[0], goal[1], goal[2]]], true)),
                trace_call,
                ..Default::default()
            };
        }
        // GOAL_TIER_TOL (module const): a reached floor within this of goal_floor == the right tier.
        // The goal's TIER: the walkable surface at the goal XY the caller means. On a zone with
        // stacked levels (neriakc, a walkway over a lower floor) the goal cell exists at several
        // heights; A* must finish on the one the caller asked for, else it routes the whole approach
        // along the wrong tier and the walker stalls / lands a level off (#35).
        //
        // The goal z is NOT always a floor height, and assuming it is was the #229 wedge: a
        // `zone_cross` aims at a `DRNTP` region's representative point, whose z is an interior point
        // of the region VOLUME — measured 1.5u to 127u ABOVE the real floor on every zone line in
        // the shipped assets. Snapping that to the "nearest surface within ±20" produced a PHANTOM
        // tier (or, pre-normal-filter, a ceiling), and then GOAL_TIER_TOL rejected every cell A*
        // reached at the TRUE floor — so A* could never accept arrival, flooded the grid, timed out,
        // and returned a greedy partial that wedged into a wall.
        //
        // So: snap to a floor only when the goal z really IS one (within GOAL_TIER_TOL of a
        // surface). Otherwise the goal is a point in the air / in a volume — project it DOWN onto
        // the walkable floor beneath it, however far below that is.
        const GOAL_DROP: f32 = 400.0; // a volume point can sit far above its floor
        // The snap window is GOAL_TIER_TOL (8), deliberately NARROWER than the old +/-STEP_UP (20).
        // It has to be: the two clauses below disagree about what a goal that is 8..20u ABOVE a floor
        // means, and only one of them can win. The old +/-20 said "that's still this floor" — which is
        // exactly how a zone-line region point 12.9u above its floor (gfaydark->felwithea) got snapped
        // onto a phantom tier. Tying the window to the SAME tolerance A* later uses to accept arrival
        // (GOAL_TIER_TOL) makes the two agree by construction: if the goal z is within tier tolerance
        // of a real floor it IS that tier; otherwise it is a point in the air and gets projected down.
        // Cost: a goal reported 8..20u BELOW its floor now resolves to the floor beneath it instead of
        // stepping up to the one above. That is the rarer and more conservative error (walk to the
        // ground under the target, not to a tier the caller never named), and callers that report a z
        // that far under their own floor are already lying to us.
        // FLOATING GOAL ANCHOR (#197p2 / water-nav design §4d) — the mirror of the floating START
        // anchor below. The ORDER of this chain is the fix:
        //   1. A REAL tier at the caller's z wins, unchanged — a dock over water stays a dock, and
        //      an explicitly-asked-for submerged floor (z within tier tolerance of the pool
        //      bottom) is honoured. The walker can't dive-and-hold that depth, so the planner
        //      reports the surface accommodation via `goal_z_was_snapped` — never silently.
        //   2. A goal FLOATING in water (in/over water, no footing beneath) anchors to the WATER
        //      SURFACE — the exact tier the WATER SURFACE TRAVERSAL edges emit, so arrival at the
        //      goal cell matches by construction. This MUST run before `floor_beneath` (and the
        //      column snap below): either of those grabs the pool BOTTOM (up to GOAL_DROP=400u
        //      down), and GOAL_TIER_TOL then rejects every surface arrival — A* plans a dive the
        //      controller's buoyancy won't hold (the halas-pool / blackburrow-lake `[WAT-ROUTE]`
        //      wedges in the walker-drift corpus).
        //   3. Only a genuinely dry unresolved goal falls through to the volume-point projection.
        // An UNBOUNDED water column (`surface_z` = None) yields no anchor and falls through the
        // same way — the chain tolerates it by shape, nothing to unwrap.
        let goal_tier_floor = col.nearest_floor(goal[0], goal[1], goal[2], GOAL_TIER_TOL, GOAL_TIER_TOL);
        // MID-WATER GOAL NODE (Slice 2, design §7.3): a goal asked for INSIDE a navigable water span
        // resolves to its own depth — the nearest interior lattice node — NOT the water surface. This
        // is the first owner case: "swim to a submerged point". It MUST run before the floating-surface
        // anchor below, which would otherwise project every sub-surface ask up to the surface (the
        // legacy projection hack this subsumes). It fires ONLY for a genuinely navigable ask (inside a
        // stored, carved span, #534/#540), so a sloppy z above the swim plane still falls through to
        // the surface anchor unchanged.
        let water_goal = if goal_tier_floor.is_none() { water_node_goal_z(col, goal) } else { None };
        // `Some` = the goal is anchored to the water surface. Kept separate from the chain below
        // because the final waypoint must then carry THIS tier, not the caller's raw z.
        let floating_goal = if goal_tier_floor.is_none() && water_goal.is_none() {
            floating_goal_surface(col, goal)
        } else { None };
        let resolved_goal_floor = goal_tier_floor
            .or(water_goal)
            .or(floating_goal)
            .or_else(|| col.floor_beneath(goal[0], goal[1], goal[2], GOAL_TIER_TOL, GOAL_DROP));
        // AN UNACCEPTABLE GOAL FAILS IMMEDIATELY AND LOUDLY (#337). If there is no walkable floor at
        // or beneath the goal, no cell A* can ever reach will satisfy the arrival test — the search
        // is guaranteed to flood the entire grid and come back with a greedy partial that the walker
        // drives into a wall. That is not a search problem, it is an invalid question, and answering
        // it with a 2-second flood and a wedge is the exact dishonesty this issue is about. Say so
        // now, in microseconds, with a reason the agent can act on.
        //
        // EXCEPT for a zone-line goal (`goal_region`): arrival there is decided by "am I standing
        // INSIDE the region volume?", not by the goal cell's floor tier — a region's representative
        // point is an interior point of a VOLUME whose z is structurally never a floor height (#229).
        // Its walkability is not ours to judge here, so let the search answer it.
        //
        // A SLOPPY Z IS NOT AN UNREACHABLE GOAL. Agents routinely pass a rough z (often 0, or a map
        // coordinate), and the goal's real floor can sit well above it — `floor_beneath` only looks
        // DOWN, so it finds nothing. An earlier version hard-failed those as `goal_not_walkable`,
        // which is a FALSE definitive no: the XY is perfectly walkable and `main` routed to it fine
        // (its wrong-tier `goal_fallback` accepted the goal cell at whatever tier it really had).
        // Live North Qeynos: `goto (-40,250,z=0)` refused to move at all. So before giving up, snap
        // the goal to the nearest floor ANYWHERE in its column — that is the goal the caller meant.
        let goal_floor = match resolved_goal_floor
            .or_else(|| col.snap_goal_to_column_floor(goal))
        {
            Some(f) => f,
            // NO floor anywhere in the goal's column: off the mesh, or out over a void. THAT is an
            // unacceptable goal, and it fails immediately and loudly — no flooding the grid for
            // seconds and handing back a stub to wedge on (#337).
            None if ctx.goal_region.is_none() => {
                tracing::info!("find_path: goal ({:.0},{:.0},{:.1}) has NO walkable floor anywhere in its column \
                    — unreachable by construction (not searching)", goal[0], goal[1], goal[2]);
                return Search { trace_call, ..Search::no_route(NoRoute::GoalNotWalkable) };
            }
            None => goal[2],
        };
        // Start floor: anchor to the caller's EXACT (x,y), NOT the 8u cell center. Near a wall the
        // cell center can fall on the wall's footprint, whose only surface is the wall-TOP — so the
        // center probe would start the char up on the wall and route the whole path along it, a
        // height the walker can't scale from a standstill (it wedges → 0 progress, #2). The exact
        // start point sits on the real floor (e.g. the street) the char is actually standing on.
        // The caller's z can still be stale, so try several reference levels; fall back to the cell
        // center only if the exact point has no floor at any of them.
        //
        // WATER (#329, #197p2): a character FLOATING in water has no floor under it in any
        // meaningful sense — its support is the WATER SURFACE, and buoyancy holds it there. Anchoring
        // it to a slab instead planned routes it physically cannot follow: in the flooded qcat spawn
        // corridor the nearest surface to a floater was the CEILING (the water line is flush with
        // it), so A* planned across the ceiling plane; in the Halas pool the nearest surface was the
        // pool BOTTOM 128u down, so A* planned along the bottom and the swimmer dived and stranded.
        // If the char is in water with no footing directly beneath it, anchor A* to the surface it is
        // actually floating on — which is exactly the tier the WATER SURFACE TRAVERSAL edges connect.
        // `FOOTING` (module const): a floor this close under the feet = standing (wading), not
        // floating — shared with the floating GOAL anchor (`floating_goal_surface`, design §4d).
        let floating_surface = col.region_map().and_then(|w| {
            let (x, y, z) = (start[0], start[1], start[2]);
            let wet = w.is_water(x, y, z) || w.is_water(x, y, z - 1.0);
            let footed = col.nearest_floor(x, y, z, 2.0, FOOTING).is_some();
            if wet && !footed { w.surface_z(x, y, z - 1.0) } else { None }
        });
        // The floor under the character's EXACT position (not its cell centre). When this resolves,
        // A* is anchored geometrically AT THE CHARACTER (see `anchor_at_char` below) and needs no
        // cell-centre fallback at all.
        let exact_start_floor = floating_surface
            .or_else(|| [start[2], goal[2], 0.0, -60.0, -120.0]
                .into_iter()
                .find_map(|rz| col.nearest_floor(start[0], start[1], rz, STEP_UP, MAX_DROP)));
        // TRUE-POSITION START ANCHOR (#229's last mile). A* expands each node from its CELL CENTRE,
        // including the start — but the walker drives from where the character actually STANDS. When
        // the character is pressed against a wall, its own 8u cell centre can lie INSIDE the wall's
        // footprint; the start cell then gets hopped to a neighbour (below), A* plans a perfectly
        // valid chain from THAT centre, and the walker charges from its real position straight at the
        // first waypoint — through the wall in between. Live everfrost→blackburrow: a 99-waypoint
        // route in which every A*-validated cell-centre segment was clear and ONLY the
        // character→waypoint[0] leg was blocked; the walker pressed into the wall, made no progress,
        // re-planned the identical route, and stalled out after 8 attempts.
        //
        // So when the character's exact position has a floor, expand the START node from THE
        // CHARACTER'S OWN (x, y) instead of the cell centre. Every first-hop edge is then clearance-
        // tested from where the walker really is, so the route's first leg is always one it can
        // actually take — and the cell-centre-in-a-wall hop becomes unnecessary (the start node's
        // floor is the character's real floor, never the wall-top the hop existed to dodge).
        let anchor_at_char = prefer_char_anchor && exact_start_floor.is_some();
        let start_floor = exact_start_floor
            .or_else(|| [start[2], goal[2], 0.0, -60.0, -120.0].into_iter().find_map(|rz| floor_near(sc, sr, rz)))
            .unwrap_or(start[2]);
        const STEP_H: f32 = 20.0;        // vertical SEARCH range for column_floors + per-cell rise cap
        // What actually enforces "nav climbs only what a WASD player can" (#239) is NOT a per-cell
        // rise cap (that would reject legitimate smooth ramps) — it's the FEET-level `path_clear`
        // below: a discrete riser taller than the walker's ~2.5u step blocks the low ray, so A* routes
        // around it, while a smooth ramp (surface stays under the ray) passes and is governed by
        // MAX_WALK_GRADE. Paired with the controller's native STEP_UP cap, nav cannot scale the
        // boundary-wall lips it would otherwise climb onto the high side of.
        const MAX_STEP_DOWN: f32 = 60.0; // max DROP between adjacent cells (fall/hop down a level)
        // Grade limit (eqoxide#212): STEP_H=20 over an 8u cell is a 250% grade. A discrete vertical
        // step that tall is USUALLY blocked here by the feet-ray path_clear (its riser is a wall) —
        // but NOT always: the feet ray is interpolated from `cz + feet` to `nf + feet`, so on a
        // steeply-RISING hop the ray has already gained most of the altitude by the far end and
        // skims OVER a near-vertical face that sits near the destination (#630, the #617 canal
        // bank). The climbs that pass both rays are smooth RAMPS or laundered faces — the grade
        // check below rejects ramps steeper than the controller can walk (it would slide, #205),
        // and `walk_profile_ok` (#630) rejects the laundered faces the average hides.
        // (MAX_WALK_GRADE = 1.2 is now a module const, shared with walk_profile_ok.)
        // Jump-edges (eqoxide#190): let A* leap a GENUINE horizontal floor gap a running jump can
        // clear. NAV_RUN_SPEED matches action_loop::RUN_SPEED (the speed the walker drives a jump at);
        // reach is derived from it via movement::running_jump_reach (~22.7u). JUMP_UP_TOL caps how
        // much higher a landing may sit (a running jump's apex ≈ JUMP_VELOCITY²/2·GRAVITY ≈ 4u).
        // JUMP_PENALTY makes a jump cost more than the equivalent walk so A* only jumps when a gap
        // would otherwise block the route.
        const NAV_RUN_SPEED: f32 = 44.0;
        const JUMP_UP_TOL: f32 = 4.0;
        const JUMP_PENALTY: f32 = 30.0;
        // Snap the start CELL onto the surface the char is really on. When the char stands at a
        // cell's edge next to a wall, the 8u cell CENTER can fall on the wall's footprint — whose
        // only floor is the wall-TOP — so A* run from that cell would begin up on the wall and
        // either route along it (a height the walker can't scale → 0 progress) or find no route at
        // all. If the start cell's column has no floor at the char's true height (`start_floor`),
        // hop to the nearest neighbouring cell that does. (#2)
        // When the start anchor is a WATER SURFACE (a floating character), there is no solid floor at
        // that height by definition — the cell is valid if it is swimmable water there instead.
        //
        // Only needed when we could NOT anchor at the character's exact position: with
        // `anchor_at_char` the start node's floor is the character's real floor (never the wall-top)
        // and its edges are cast from the character's real (x, y), so hopping the cell would only
        // move the plan's origin away from the walker — which is the very bug it used to cause.
        let cell_has_start_floor = |c: i32, r: i32| -> bool {
            let ctr = center(c, r);
            if floating_surface.is_some() {
                return col.region_map().is_some_and(|w| w.is_water(ctr[0], ctr[1], start_floor - 1.0));
            }
            col.column_floors(ctr[0], ctr[1], start_floor, STEP_H, MAX_STEP_DOWN)
                .into_iter().any(|z| (z - start_floor).abs() <= GOAL_TIER_TOL)
        };
        let (sc, sr) = if anchor_at_char || cell_has_start_floor(sc, sr) {
            (sc, sr)
        } else {
            let mut best: Option<(i32, i32, i32)> = None; // (col, row, dist²)
            for rad in 1i32..=3 {
                for dc in -rad..=rad {
                    for dr in -rad..=rad {
                        if dc.abs() != rad && dr.abs() != rad { continue; } // ring only
                        let (nc, nr) = (sc + dc, sr + dr);
                        if nc < 0 || nr < 0 || nc >= cols || nr >= rows { continue; }
                        if cell_has_start_floor(nc, nr) {
                            let d2 = dc * dc + dr * dr;
                            if best.is_none_or(|(_, _, bd)| d2 < bd) { best = Some((nc, nr, d2)); }
                        }
                    }
                }
                if best.is_some() { break; } // nearest ring wins
            }
            best.map(|(c, r, _)| (c, r)).unwrap_or((sc, sr))
        };
        // The planner's UPPER edge probe. This is `Body::chest` — the SAME height the controller's
        // contact ray collides at (`movement::CharacterController::slide`) — not a locally-invented
        // constant. It was 3.0 here for months while the controller collided at 4.0, so geometry
        // occupying only z ∈ (3.0, 4.0] above the floor (a door lintel, a low arch soffit) was clear
        // to A* and solid to the walker: the planner handed out routes the walker physically could
        // not follow (#386, design §1c). Deriving both from the one PLAYER_BODY makes that drift
        // unrepresentable rather than merely fixed.
        const CHEST: f32 = eqoxide_zone_geometry::body::PLAYER_BODY.chest;
        // The node cap is the ONLY bound on a search, so it also decides whether the definitive verdict
        // `Unreachable(SearchClosed)` can ever be reached. Too tight and a whole-zone flood hits the cap
        // and downgrades to `Exhausted` ("I don't know") even when the frontier really was closable — a
        // false "I don't know" in place of an honest "no". So it is chosen by MEASUREMENT, above.
        //
        // The module-level `MAX_NODES` (8M, chosen so everfrost's 1.12M-node whole-zone close still
        // reaches SearchClosed, #394) is the ABSOLUTE backstop; a caller may set a TIGHTER `ctx.node_cap`
        // (the fine tier does). It is a NODE COUNT, not a wall clock: whichever cap bites, the same query
        // hits it after the same number of expansions on every machine, so the outcome is reproducible.
        // A wall-clock budget used to sit here too and made the answer machine-speed-dependent — deleted.
        let node_cap = ctx.node_cap.unwrap_or(MAX_NODES).min(MAX_NODES);
        let mut limit: Option<PlanLimit> = None;
        // How much walkable ground the route must keep around it (the LEDGE margin, enforced on the
        // neighbour cell below). Asked for ONLY above the minimum clearance: at `PLAYER_RADIUS` the
        // plan promises exactly "the character fits" — which is the promise that keeps a narrow
        // bridge, gangplank or catwalk routable at all — and the GENEROUS tier layers standing room
        // on top of it (`search_tiered`). A swim plan is exempt outright: a floating character has no
        // ground under it by definition, so the probe would reject every cell it must cross.
        let ledge_margin = if radius > eqoxide_core::physics::PLAYER_RADIUS && floating_surface.is_none() {
            radius
        } else {
            0.0
        };
        // THE ONE AUTHORITY on what blocks the body, for this plan (#378). The walk-edge test and
        // the waypoint inset below both consult it — they used to be independent predicates that
        // could not see each other's hazards. It owns the per-plan ground-margin memo that used to
        // live here as a bare HashMap.
        let trav = crate::traversability::Traversability::new(
            col, radius, cell, ledge_margin, floating_surface.is_some());
        // Aggro-avoidance (#67): softly bias A* AWAY from cells near NPCs so long routes skirt mob
        // camps instead of plowing through them and getting the player killed. Proactive (before
        // aggro) and faction-agnostic — the client has no broad faction data, so it avoids ALL
        // nearby NPCs; the penalty is MILD and fades to 0 at AGGRO_RADIUS, so a route is only
        // nudged around a camp when a clear alternative exists — it never becomes "no route".
        // `aggro_buffer` (#242) WIDENS that radius when the caller asks to route more conservatively
        // around hostile pulls (`avoid_aggro` on /v1/move/*), and scales the penalty up with it so a
        // bigger buffer gives real berth — while staying soft (still fades to 0 at the edge, so an
        // unavoidable disc is threaded at shortest exposure rather than failing the route).
        let aggro_radius  = 50.0 + aggro_buffer.max(0.0);       // ~a low-level mob's aggro range + buffer
        let aggro_penalty = 60.0 * (1.0 + aggro_buffer.max(0.0) / 50.0); // firmer with a wider buffer
        let aggro_cost = |x: f32, y: f32| -> f32 {
            let mut worst = 0.0f32;
            for p in avoid {
                let d2 = (x - p[0]) * (x - p[0]) + (y - p[1]) * (y - p[1]);
                if d2 < aggro_radius * aggro_radius {
                    worst = worst.max(aggro_penalty * (1.0 - d2.sqrt() / aggro_radius));
                }
            }
            worst
        };
        // WALL-HUG COST (#381, via the zone-lifetime clearance field). `path_clear`'s feelers run
        // parallel to travel, so an edge that RUNS ALONGSIDE a wall — never crossing it — reads
        // clear at every feeler; the emitted lane then hugs the wall and the walker presses into it
        // (the qcat hallway symptom). The field's RADIAL wall distance sees exactly that wall, and
        // it enters here as a COST, never a filter: a lane inside the preferred clearance pays up
        // to one extra cell-length per step, so A* swings to the corridor middle whenever a freer
        // lane exists and still threads the tight lane when nothing else does (a cost can never
        // become no_route — the §9 non-negotiable, same contract as `aggro_cost`).
        //
        // FINE TIER ONLY (`cell <= SWEPT_EDGE_MAX_CELL`): the fine lane is the one the walker
        // actually steers along. The coarse 8 u tier stays a pure optimistic corridor SELECTOR —
        // its centres sit near walls in every city corridor as a matter of geometry, and biasing it
        // buys no walkability (the walker never walks the coarse line) at real routing drift.
        let hug_cost = |x: f32, y: f32, fz: f32| -> f32 {
            if cell > SWEPT_EDGE_MAX_CELL { return 0.0; }
            let w = col.wall_clearance(x, y, fz);
            if w >= NAV_PREFERRED_CLEARANCE { 0.0 } else { cell * (1.0 - w / NAV_PREFERRED_CLEARANCE) }
        };
        // MULTI-FLOOR A*: the node is (cell, floor), not just cell — so a single cell can be visited
        // at several heights (a ramp or lower floor sitting UNDER an upper ledge). Single-floor A*
        // snapped every cell to the surface nearest the current z and could never step down onto a
        // floor beneath an overhang, so overlapping multi-level zones (e.g. qcat's sewer under the
        // upper walkway) were unreachable. Floor is quantized to 2u buckets for the hash key.
        let qf = |z: f32| (z / 2.0).round() as i32;
        type Key = (i32, i32, i32); // (col, row, floor_bucket)
        // TELEPORT-PAD EDGES (#403): resolve each pad's (source, dest) FLOOR points to this grid's
        // cells ONCE, so the per-node test in the search loop below is a cheap integer compare — no
        // BSP walk in the hot loop (the honesty-gated resolution already happened in
        // `resolve_teleport_pads`). `(src_col, src_row, src_z, dst_col, dst_row, dst_z)`; empty for
        // the overwhelming majority of zones, which then pay nothing.
        let pad_edges: Vec<(i32, i32, f32, i32, i32, f32)> = ctx.teleport_pads.iter().map(|p| {
            let (psc, psr) = to_cell(p.source[0], p.source[1]);
            let (pdc, pdr) = to_cell(p.dest[0], p.dest[1]);
            (psc, psr, p.source[2], pdc, pdr, p.dest[2])
        }).collect();
        // CLIMB EDGES (#309): the same one-off materialisation as the pads above, resolving each
        // ladder onto THIS grid's cells so the search loop stays an integer compare.
        //
        // Two things differ from a pad, both forced by what a ladder is. The source is a cell
        // RECTANGLE rather than one cell — a ladder's capture footprint is ~10u wide once padded,
        // which is several cells at 8u and more at 2u. And the source Z is a RANGE spanning the
        // whole ladder, not one tier: in the Crushbone moat a character floating at the waterline
        // (z ≈ −11) and one standing on the moat floor (z ≈ −24) are 14 units apart and BOTH have to
        // find the same ladder, which a `GOAL_TIER_TOL` compare against a single height cannot do.
        struct GridClimb { c0: i32, c1: i32, r0: i32, r1: i32, z_lo: f32, z_hi: f32,
                           dc: i32, dr: i32, dz: f32, mount: [f32; 2] }
        let climb_edges: Vec<GridClimb> = col.climb_edges().iter().map(|e| {
            let (c0, r0) = to_cell(e.volume.lo[0], e.volume.lo[1]);
            let (c1, r1) = to_cell(e.volume.hi[0], e.volume.hi[1]);
            let (dc, dr) = to_cell(e.dismount[0], e.dismount[1]);
            GridClimb { c0: c0.min(c1), c1: c0.max(c1), r0: r0.min(r1), r1: r0.max(r1),
                z_lo: e.volume.foot_z - eqoxide_zone_geometry::climb::DISMOUNT_Z_TOL, z_hi: e.volume.top_z,
                dc, dr, dz: e.dismount[2], mount: e.volume.center() }
        }).collect();
        // Did the search reach a node by CLIMBING? Collected here and checked against the returned
        // route below, so `nav_climb` counts routes an agent is actually handed — not every ladder
        // the frontier happened to touch, which would make the disclosure meaningless.
        let mut climbed: std::collections::HashSet<Key> = std::collections::HashSet::new();
        let skey: Key = (sc, sr, qf(start_floor));
        let mut g_score: std::collections::HashMap<Key, f32> = std::collections::HashMap::new();
        let mut came:    std::collections::HashMap<Key, Key> = std::collections::HashMap::new();
        let mut closed:  std::collections::HashSet<Key> = std::collections::HashSet::new();
        let mut floor_of: std::collections::HashMap<Key, f32> = std::collections::HashMap::new();
        struct Node { f: f32, c: i32, r: i32, fz: f32 }
        impl PartialEq for Node { fn eq(&self, o: &Self) -> bool { self.f == o.f } }
        impl Eq for Node {}
        impl Ord for Node { fn cmp(&self, o: &Self) -> Ordering { o.f.partial_cmp(&self.f).unwrap_or(Ordering::Equal) } }
        impl PartialOrd for Node { fn partial_cmp(&self, o: &Self) -> Option<Ordering> { Some(self.cmp(o)) } }
        let h = |c: i32, r: i32| (((c - gc) as f32).powi(2) + ((r - gr) as f32).powi(2)).sqrt() * cell;
        g_score.insert(skey, 0.0);
        floor_of.insert(skey, start_floor);
        let mut heap = BinaryHeap::new();
        heap.push(Node { f: h(sc, sr), c: sc, r: sr, fz: start_floor });
        // The PLAN-WIDE running total of expansions (#394 review). `astar` increments the shared
        // counter (materialised by the plan owner via `PlanCtx::ensure_budget`) so that a plan fanning
        // out to up to 13 A* calls is bounded by ONE `node_cap`, not `node_cap` per call. A ctx with no
        // shared counter (only a raw unit test constructs one) falls back to a local count, which for a
        // single call is the same thing.
        let mut local_expanded = 0usize;
        let mut goal_key: Option<Key> = None;
        // A goal-cell node reached at the WRONG tier — kept as a last resort so we never regress to
        // "no route" when the requested tier is unreachable (better a wrong-tier path than none).
        let mut goal_fallback: Option<Key> = None;
        // Closest-to-goal cell we actually reach, for a partial-path fallback (#188): if the goal
        // cell itself is unreachable, still walk AS FAR toward it as we can rather than not moving at
        // all — the walker's re-path loop then makes further incremental progress from there.
        let mut best_toward: Option<Key> = None;
        let mut best_toward_h = f32::MAX;
        // Slice 2 (design §6): the water-span grid this search may route THROUGH, lazily built on the
        // first water plan in this zone. `None` in a dry zone (no `.wtr`) → every water family below
        // is skipped and land A* is byte-for-byte unchanged. Bound once, read per node.
        let water_grid = col.water_grid_active();
        while let Some(Node { c, r, fz, .. }) = heap.pop() {
            let ckey = (c, r, qf(fz));
            if !closed.insert(ckey) { continue; } // already expanded
            // ZONE-LINE ARRIVAL (#229): a zone line is a VOLUME (a DRNTP BSP region), not a point.
            // Its representative point's z is an interior point of that volume — structurally never a
            // floor height — so "did I reach the goal cell at the goal's tier?" is the wrong question
            // to ask of it. Ask the right one instead: am I STANDING INSIDE the region? That is
            // exactly the predicate the native auto-cross fires on. Only tested near the goal cell,
            // so it costs a handful of BSP walks per plan, not one per node.
            if let (Some(want), Some(water)) = (ctx.goal_region, col.region_map()) {
                if h(c, r) <= 2.0 * cell {
                    let p = center(c, r);
                    if water.zone_line_at(p[0], p[1], fz + 1.0) == Some(want) {
                        goal_key = Some(ckey);
                        break;
                    }
                }
            }
            if (c, r) == (gc, gr) {
                // HONESTY GATE (Slice 2, #359 contract): a WATER node at the goal cell is NOT arrival
                // at a LAND goal. A swimmer floating in the water column BELOW a dry goal is within
                // `GOAL_TIER_TOL` of it, but it has not reached it — it must still HAUL OUT (a route to
                // the dry tier via the exit edge, whose #359 cap it must satisfy). Accepting the water
                // node as arrival (or even as a wrong-tier fallback that reports `reached`) would be
                // exactly the false-`arrived` the haul-out contract forbids. So exclude a water node
                // from both goal_key AND goal_fallback UNLESS the goal itself resolved to a water node
                // (`water_goal`), in which case the mid-water depth IS the destination (design §7.3).
                let node_is_water = water_goal.is_none() && water_grid
                    .and_then(|g| { let p = center(c, r); g.column_at(p[0], p[1]) })
                    .is_some_and(|col| col.span_containing(fz).is_some());
                if !node_is_water {
                    if (fz - goal_floor).abs() <= GOAL_TIER_TOL {
                        goal_key = Some(ckey); // reached the goal cell on the requested tier — done
                        break;
                    }
                    // Wrong tier: remember the first (cheapest) one, but keep searching — the right
                    // tier may be reachable by climbing to it at an adjacent cell. Fall through.
                    if goal_fallback.is_none() { goal_fallback = Some(ckey); }
                }
            }
            // The ONE runaway bound, and it is a deterministic, PLAN-WIDE node count (#394 + review).
            // Increment the shared running total (or a local one for a bare unit-test ctx) and stop
            // when it passes `node_cap`. Because every A* call in the plan shares this counter, a plan
            // that fans out to 13 calls still costs at most `node_cap` expansions in total, not per
            // call — which is what makes the "one plan, one budget" contract (#340) true rather than
            // merely documented. Whichever cap bites, the same query hits it after the same number of
            // expansions on every machine — `Exhausted(NodeCap)` reproducibly, instead of `SearchClosed`
            // on a fast box and `Exhausted(Deadline)` on a slow one. The wall-clock check that used to
            // sit here is deleted; there is no `Instant::now()` in the search at all now.
            let total = match &ctx.expanded {
                Some(shared) => shared.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
                None => { local_expanded += 1; local_expanded }
            };
            if total > node_cap { limit = Some(PlanLimit::NodeCap); break; }
            // Track the closest-to-goal cell reached (heuristic = straight-line cells to the goal),
            // for the partial-path fallback below.
            let hd = h(c, r);
            if hd < best_toward_h { best_toward_h = hd; best_toward = Some(ckey); }
            let cz = fz;
            let g_cur = *g_score.get(&ckey).unwrap_or(&f32::MAX);
            // Expand from the CHARACTER'S OWN position for the start node (see `anchor_at_char`), so
            // every first-hop edge is clearance-tested from where the walker actually stands rather
            // than from a cell centre it may not be able to reach. Every other node expands from its
            // cell centre as before.
            let a = if anchor_at_char && ckey == skey { [start[0], start[1]] } else { center(c, r) };
            // ── WATER-VOLUME NODE (Slice 2, design §6) ──────────────────────────────────────────
            // Is the node being expanded an INTERIOR water node — a swimmable point inside a stored,
            // carved span (#534/#540 guarantee it holds no solid)? A land/surface/pool-bottom node is
            // NEVER in a span by construction (a span's feet-interval sits strictly BETWEEN the floor
            // and the swim plane, `nav_lo = floor+ε`, `nav_hi = surface−float_depth`), so this test
            // partitions the graph cleanly: legacy nodes route by the land families, interior nodes by
            // the water families. The water families are thus purely ADDITIVE — reached only via the
            // new interior nodes — leaving every existing land / surface-swim / haul-out path intact.
            let cur_water: Option<(&eqoxide_zone_geometry::water_grid::WaterColumn, (f32, f32))> =
                water_grid.and_then(|g| g.column_at(a[0], a[1]))
                    .and_then(|col| col.span_containing(cz).map(|s| (col, s)));
            // VERTICAL swim within the current column: up / down one VRES step, staying in the SAME
            // carved span. A span is FREE water by construction (bounded by the very solids the carver
            // removed), so no lateral ray is needed — this is the dive/rise leg a mid-water route needs
            // and the up-column climb to the swim plane before a haul-out. Once per node (not per dir).
            if let Some((_, (slo, shi))) = cur_water {
                use eqoxide_zone_geometry::water_grid::VRES;
                for d in [-1.0f32, 1.0] {
                    let nz = cz + d * VRES;
                    if nz < slo - 0.01 || nz > shi + 0.01 { continue; } // stay inside this span
                    let nkey = (c, r, qf(nz));
                    if closed.contains(&nkey) { continue; }
                    if let Some(t) = tr.as_deref_mut() {
                        t.edge([a[0], a[1], cz], [a[0], a[1], nz],
                            EdgeVerdict::Accepted { kind: EdgeKind::SwimVertical });
                    }
                    let tentative = g_cur + VRES;
                    if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                        g_score.insert(nkey, tentative);
                        came.insert(nkey, ckey);
                        floor_of.insert(nkey, nz);
                        heap.push(Node { f: tentative + h(c, r), c, r, fz: nz });
                    }
                }
            }
            // TELEPORT-PAD EDGE (#403): if this node sits on a pad footprint (its source cell, at the
            // footprint's floor tier), add the DISCONTINUOUS edge to the pad's arrival cell — the one
            // link terrain-follow neighbours cannot express. This is what lets A* route THROUGH a pad
            // instead of flooding a component that can't reach the goal and returning a false
            // `SearchClosed`. Emitted at EXPANSION (once per node, guarded by `closed` above), an
            // O(pads) integer test. A fixed `PAD_PENALTY` makes A* prefer a walkable route when one
            // exists (a pad crossing interrupts the walk) but readily take the pad when it is the only
            // way to the goal's component — a cost, never a filter, so it can never itself cause a
            // false `no_path`.
            for &(psc, psr, psz, pdc, pdr, pdz) in &pad_edges {
                if c == psc && r == psr && (fz - psz).abs() <= GOAL_TIER_TOL {
                    const PAD_PENALTY: f32 = 100.0;
                    let dkey = (pdc, pdr, qf(pdz));
                    if closed.contains(&dkey) { continue; }
                    if let Some(t) = tr.as_deref_mut() {
                        let dcen = center(pdc, pdr);
                        t.edge([a[0], a[1], fz], [dcen[0], dcen[1], pdz],
                            EdgeVerdict::Accepted { kind: EdgeKind::Pad });
                    }
                    let tentative = g_cur + PAD_PENALTY;
                    if tentative < *g_score.get(&dkey).unwrap_or(&f32::MAX) {
                        g_score.insert(dkey, tentative);
                        came.insert(dkey, ckey);
                        floor_of.insert(dkey, pdz);
                        heap.push(Node { f: tentative + h(pdc, pdr), c: pdc, r: pdr, fz: pdz });
                    }
                }
            }
            // CLIMB EDGE (#309): if this node stands inside a ladder's footprint, anywhere along the
            // ladder's height, add the edge to the dismount floor at its top. Like the pad edge above
            // this expresses a link the terrain-follow families structurally cannot: the connecting
            // surface is VERTICAL, and `MAX_WALK_GRADE` is right to refuse it. Refusing it is exactly
            // what strands a character in the Crushbone moat, where ~10u of sheer wall stands between
            // the waterline and the rim, the haul-out is 2u, and five ladders are the way out.
            //
            // ASCENT ONLY, deliberately. A descent edge would need a validated standable target at the
            // ladder's FOOT, which `resolve_climb_edges` does not resolve — and offering a link whose
            // far end has not been shown standable is the precise failure this edge exists to avoid.
            // Downward travel keeps whatever the existing fall edge (lethality-checked) already allows.
            for e in &climb_edges {
                if c < e.c0 || c > e.c1 || r < e.r0 || r > e.r1 { continue; }
                if fz < e.z_lo || fz > e.z_hi { continue; }
                let dkey = (e.dc, e.dr, qf(e.dz));
                if closed.contains(&dkey) { continue; }
                // Time-equivalent cost: the rise at `CLIMB_SPEED`, re-expressed as the walking
                // distance that takes the same time, so it is commensurable with the walk edges'
                // metres. Plus a flat penalty in the `PAD_PENALTY` mould, which keeps a walkable
                // route preferred where one exists. A cost, never a filter — it can no more cause a
                // false `no_path` than the pad penalty can.
                const CLIMB_PENALTY: f32 = 60.0;
                let rise = (e.dz - fz).max(0.0);
                let tentative = g_cur + CLIMB_PENALTY + rise / eqoxide_zone_geometry::climb::CLIMB_SPEED * NAV_RUN_SPEED;
                if let Some(t) = tr.as_deref_mut() {
                    t.edge([a[0], a[1], fz], [e.mount[0], e.mount[1], e.dz],
                        EdgeVerdict::Accepted { kind: EdgeKind::Climb });
                }
                if tentative < *g_score.get(&dkey).unwrap_or(&f32::MAX) {
                    g_score.insert(dkey, tentative);
                    came.insert(dkey, ckey);
                    floor_of.insert(dkey, e.dz);
                    climbed.insert(dkey);
                    heap.push(Node { f: tentative + h(e.dc, e.dr), c: e.dc, r: e.dr, fz: e.dz });
                }
            }
            for (dc, dr) in [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (-1, 1), (1, -1), (1, 1)] {
                let (nc, nr) = (c + dc, r + dr);
                if nc < 0 || nr < 0 || nc >= cols || nr >= rows { continue; }
                let b = center(nc, nr);
                // Local-tier bound: keep a FINE plan within `max_search` units of the start so its
                // cost stays small even when it has to detour around an obstacle (#nav-multires).
                if let Some(maxr) = max_search {
                    if (b[0] - start[0]).hypot(b[1] - start[1]) > maxr { continue; }
                }
                // ── WATER-VOLUME neighbour edges (Slice 2, design §6.2/§7.2) ────────────────────
                // A water node routes ONLY by water edges (the land families below assume `cz` is a
                // walkable floor). Emit the 3D interior + haul-out edges for this neighbour, then
                // `continue` past the land families entirely.
                if let Some((cur_col, _)) = cur_water {
                    use eqoxide_zone_geometry::water_grid::VRES;
                    let body = &eqoxide_zone_geometry::body::PLAYER_BODY;
                    // HORIZONTAL 26-neighbour (design §6.2): connect to the neighbour column's node at
                    // z−VRES, z, z+VRES — full 3-DOF, smoother chords than 6-connectivity. Lateral
                    // clearance is per-edge at TWO body heights (feet + head): a swimmer's blocking
                    // band is its whole body, not the standing chest band (design §6.3).
                    if let Some(ncol) = water_grid.and_then(|g| g.column_at(b[0], b[1])) {
                        for d in [-1.0f32, 0.0, 1.0] {
                            let nz = cz + d * VRES;
                            if ncol.span_containing(nz).is_none() {
                                // Evaluated and REFUSED: the neighbour column holds no navigable
                                // water span at that depth. Record it (#615 review F5) — leaving
                                // it silent would make an evaluated refusal read as unevaluated.
                                if let Some(t) = tr.as_deref_mut() {
                                    t.edge([a[0], a[1], cz], [b[0], b[1], nz],
                                        EdgeVerdict::Rejected { reason: RejectReason::Water });
                                }
                                continue;
                            }
                            let nkey = (nc, nr, qf(nz));
                            if closed.contains(&nkey) { continue; }
                            let feet = 0.5;
                            let head = body.height - 0.5;
                            if !col.edge_clear([a[0], a[1], cz + feet], [b[0], b[1], nz + feet], radius, cell)
                                || !col.edge_clear([a[0], a[1], cz + head], [b[0], b[1], nz + head], radius, cell) {
                                if let Some(t) = tr.as_deref_mut() {
                                    t.edge([a[0], a[1], cz], [b[0], b[1], nz],
                                        EdgeVerdict::Rejected { reason: RejectReason::Clearance });
                                }
                                continue;
                            }
                            if let Some(t) = tr.as_deref_mut() {
                                t.edge([a[0], a[1], cz], [b[0], b[1], nz],
                                    EdgeVerdict::Accepted { kind: EdgeKind::SwimInterior });
                            }
                            // Cost = full 3D Euclidean length (design §6.3): no depth penalty, no
                            // surface bonus, so the straight chord to a mid-water goal is optimal and a
                            // bottom/surface detour is strictly longer (design §6.5). The horizontal-
                            // only heuristic (design §6.4, LOCKED) stays admissible: every water edge
                            // costs ≥ its horizontal projection.
                            let horiz = (((dc * dc + dr * dr) as f32).sqrt()) * cell;
                            let dz = nz - cz;
                            let step = (horiz * horiz + dz * dz).sqrt() + aggro_cost(b[0], b[1]);
                            let tentative = g_cur + step;
                            if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                                g_score.insert(nkey, tentative);
                                came.insert(nkey, ckey);
                                floor_of.insert(nkey, nz);
                                heap.push(Node { f: tentative + h(nc, nr), c: nc, r: nr, fz: nz });
                            }
                        }
                    }
                    // EXIT (water → land): ONLY from the column's TOP node, under the #359 haul-out
                    // contract verbatim (design §7.2). Interior nodes have no land edges — a route out
                    // of water always rises up-column to the swim plane first, exactly what the
                    // controller can execute. (The legacy WATER ASCENT family still serves land-
                    // anchored floaters; it is unreachable from a water node, which `continue`s below.)
                    if cur_col.top_node_z().is_some_and(|t| (t - cz).abs() < 0.5) {
                        if let Some(water) = col.region_map() {
                            if let Some(surface) = water.surface_z(a[0], a[1], cz).map(|s| s.max(cz)) {
                                let haul_out_up = eqoxide_zone_geometry::body::PLAYER_BODY.haul_out_up;
                                for nf in col.column_floors(b[0], b[1], surface, STEP_H, surface - cz) {
                                    if nf <= cz + 1.0 { continue; }              // ascents only
                                    if nf > surface + haul_out_up {              // too high to haul out
                                        if let Some(t) = tr.as_deref_mut() {
                                            t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                                EdgeVerdict::Rejected { reason: RejectReason::HaulOutTooHigh });
                                        }
                                        continue;
                                    }
                                    let nkey = (nc, nr, qf(nf));
                                    if closed.contains(&nkey) { continue; }
                                    let ray_z = surface.max(nf - STEP_H);
                                    if !col.edge_clear([a[0], a[1], ray_z + CHEST], [b[0], b[1], nf + CHEST], radius, cell) {
                                        if let Some(t) = tr.as_deref_mut() {
                                            t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                                EdgeVerdict::Rejected { reason: RejectReason::Clearance });
                                        }
                                        continue;
                                    }
                                    if let Some(t) = tr.as_deref_mut() {
                                        t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                            EdgeVerdict::Accepted { kind: EdgeKind::HaulOut });
                                    }
                                    let step = (((dc * dc + dr * dr) as f32).sqrt()) * cell + (nf - cz) * 0.5 + aggro_cost(b[0], b[1]);
                                    let tentative = g_cur + step;
                                    if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                                        g_score.insert(nkey, tentative);
                                        came.insert(nkey, ckey);
                                        floor_of.insert(nkey, nf);
                                        heap.push(Node { f: tentative + h(nc, nr), c: nc, r: nr, fz: nf });
                                    }
                                }
                            }
                        }
                    }
                    continue; // a water node routes ONLY by water edges — skip the land families
                }
                // Consider EVERY surface in the neighbor column reachable by climbing <=STEP_H or
                // dropping <=MAX_STEP_DOWN — this is what lets A* descend onto a lower floor under an
                // overhang (the multi-level connection) instead of staying on the upper surface.
                //
                // TRACE (#608): each reject `continue` below records ITS OWN reason first — the
                // record IS the branch taken, so trace and decision cannot disagree. An empty
                // candidate list records `no_floor` (the edge WAS evaluated: the answer was "no
                // floor there"). A neighbour skipped because it is already CLOSED records nothing —
                // that node's own expansion recorded its edges; absence stays "unevaluated".
                let land_floors = col.column_floors(b[0], b[1], cz, STEP_H, MAX_STEP_DOWN);
                if land_floors.is_empty() {
                    if let Some(t) = tr.as_deref_mut() {
                        t.edge([a[0], a[1], cz], [b[0], b[1], cz],
                            EdgeVerdict::Rejected { reason: RejectReason::NoFloor });
                    }
                }
                for nf in land_floors {
                    if nf - cz > STEP_H {
                        if let Some(t) = tr.as_deref_mut() {
                            t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                EdgeVerdict::Rejected { reason: RejectReason::StepUp });
                        }
                        continue;
                    }
                    if cz - nf > MAX_STEP_DOWN {
                        if let Some(t) = tr.as_deref_mut() {
                            t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                EdgeVerdict::Rejected { reason: RejectReason::StepDown });
                        }
                        continue;
                    }
                    // Grade limit: skip a climb too steep to walk (rise/run > MAX_WALK_GRADE) —
                    // A* then routes around the slope face instead of wedging on it. (eqoxide#212)
                    let rise = nf - cz;
                    if rise > 0.0 {
                        let run = (((dc * dc + dr * dr) as f32).sqrt()) * cell; // 8u orth / ~11.3u diag
                        if rise / run > MAX_WALK_GRADE {
                            if let Some(t) = tr.as_deref_mut() {
                                t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                    EdgeVerdict::Rejected { reason: RejectReason::Grade });
                            }
                            continue;
                        }
                    }
                    let nkey = (nc, nr, qf(nf));
                    if closed.contains(&nkey) { continue; }
                    // #630: the grade above is an AVERAGE over the whole hop — a near-vertical
                    // 10–16u face plus a flat approach averages into a "legal" slope (the diagonal's
                    // ~11.3u run is what launders it: 12.8/11.3 = 1.13 passes where 12.8/8 = 1.6 is
                    // rejected), and the controller's real 2u step-up cannot climb it (#617's canal
                    // bank, #309's moat wall). Probe the floor PROFILE along the hop and reject a
                    // rise concentrated into a face taller than the controller's local envelope.
                    // Gated on the endpoint rise exceeding the real step-up: anything shallower has
                    // nothing to launder, so flat terrain pays no extra floor queries.
                    if rise > eqoxide_zone_geometry::body::PLAYER_BODY.step_up
                        && !col.walk_profile_ok([a[0], a[1]], cz, [b[0], b[1]], nf, MAX_STEP_DOWN) {
                        if let Some(t) = tr.as_deref_mut() {
                            t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                EdgeVerdict::Rejected { reason: RejectReason::LocalRise });
                        }
                        continue;
                    }
                    // THE WALK-EDGE TEST, through the one authority (#378). Two probe heights from
                    // the shared Body — the CHEST ray (the controller's own contact height, #386)
                    // and a FEET ray just above the walker's real max step-up (a low invisible-
                    // boundary lip the chest ray skims over blocks the feet ray, #239) — each swept
                    // across the body's width (fine grid) or cast as a centre ray (coarse), plus
                    // the LEDGE margin: the one test that sees geometry that is MISSING (a drop, a
                    // bridge edge, a waterline). The margin is asked for only above the minimum
                    // tier — at `PLAYER_RADIUS` the promise is exactly "the character fits", which
                    // is what keeps a narrow bridge or gangplank routable at all (`search_tiered`)
                    // — and is memoised per (cell, floor) inside `trav` for this plan.
                    if !trav.can_traverse_fast(
                        crate::traversability::Point::new([a[0], a[1]], cz),
                        crate::traversability::Point::new([b[0], b[1]], nf)) {
                        if let Some(t) = tr.as_deref_mut() {
                            t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                EdgeVerdict::Rejected { reason: RejectReason::Clearance });
                        }
                        continue;
                    }
                    // #700: the coarse tier validates the walk edge with a centre RAY (to keep narrow
                    // corridors routable, `edge_clear`); but a STEEP step-DOWN is a body-WIDTH question,
                    // and that centre ray OVER-ACCEPTS a lip the swept body cannot descend — the open
                    // qeynos canal lip, where the ray threads a notch in the lip the shoulders hit, so
                    // coarse commits to a ~14u drop the fine 2u swept tier and the controller then
                    // refuse. Re-validate a steep drop with the SWEPT body on the coarse tier so the
                    // two agree. Gated tightly to keep blast radius near zero: only the COARSE tier
                    // (the fine tier already sweeps here), only a DROP, and only one STEEPER than
                    // `MAX_WALK_GRADE` — the grade band the walk edge otherwise skips entirely for a
                    // descent (the `rise > 0.0` guard above), i.e. exactly the lip/cliff faces a
                    // walking body cannot stroll down. Gentle ramps/stairs stay on the ray, unchanged.
                    if cell > SWEPT_EDGE_MAX_CELL && rise < 0.0 {
                        let run = (((dc * dc + dr * dr) as f32).sqrt()) * cell;
                        if (cz - nf) / run > MAX_WALK_GRADE
                            && !trav.can_descend_swept(
                                crate::traversability::Point::new([a[0], a[1]], cz),
                                crate::traversability::Point::new([b[0], b[1]], nf)) {
                            if let Some(t) = tr.as_deref_mut() {
                                t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                    EdgeVerdict::Rejected { reason: RejectReason::Clearance });
                            }
                            continue;
                        }
                    }
                    if let Some(t) = tr.as_deref_mut() {
                        t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                            EdgeVerdict::Accepted { kind: EdgeKind::Walk });
                    }
                    let step = (((dc * dc + dr * dr) as f32).sqrt()) * cell + (nf - cz).abs() * 0.5
                        + aggro_cost(b[0], b[1]) + hug_cost(b[0], b[1], nf);
                    let tentative = g_cur + step;
                    if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                        g_score.insert(nkey, tentative);
                        came.insert(nkey, ckey);
                        floor_of.insert(nkey, nf);
                        heap.push(Node { f: tentative + h(nc, nr), c: nc, r: nr, fz: nf });
                    }
                }

                // JUMP-EDGE (eqoxide#190): a running jump crosses a GENUINE horizontal floor gap —
                // wider than one cell (so normal walk edges can't bridge it) but within jump reach.
                // Only fires in a CARDINAL direction whose ADJACENT cell is a gap (no walkable floor
                // to step to — otherwise it's just walking). Land on the nearest cell within reach
                // whose floor is at ~takeoff height or lower, with a clear arc. Costs more than
                // walking (JUMP_PENALTY) so A* prefers real routes and only leaps when a gap blocks it.
                if (dc == 0) != (dr == 0) {
                    let walkable_at = |x: f32, y: f32| {
                        col.column_floors(x, y, cz, STEP_H, MAX_STEP_DOWN)
                            .into_iter().any(|f| f - cz <= STEP_H && cz - f <= MAX_STEP_DOWN)
                    };
                    if !walkable_at(b[0], b[1]) {
                        let reach = eqoxide_core::physics::running_jump_reach(NAV_RUN_SPEED);
                        let max_k = (reach / cell).floor() as i32;
                        for k in 2..=max_k.max(2) {
                            let (jc, jr) = (c + dc * k, r + dr * k);
                            if jc < 0 || jr < 0 || jc >= cols || jr >= rows { break; }
                            if (k as f32) * cell > reach { break; }
                            // every intermediate cell must be a gap (no walkable floor near cz);
                            // if there's ground between, it's not a real jump gap.
                            let all_gap = (1..k).all(|j| {
                                let m = center(c + dc * j, r + dr * j);
                                !walkable_at(m[0], m[1])
                            });
                            if !all_gap { break; }
                            // landing floor: at ~takeoff height (a jump gains ≤ JUMP_UP_TOL) or lower.
                            let land = center(jc, jr);
                            let landing = col.column_floors(land[0], land[1], cz, JUMP_UP_TOL, MAX_STEP_DOWN)
                                .into_iter()
                                .filter(|&nf| nf - cz <= JUMP_UP_TOL && cz - nf <= MAX_STEP_DOWN)
                                .max_by(|x, y| x.partial_cmp(y).unwrap_or(Ordering::Equal));
                            let Some(nf) = landing else { continue };
                            // arc clear: no wall between takeoff and landing (chest height).
                            if !col.edge_clear([a[0], a[1], cz + CHEST], [land[0], land[1], nf + CHEST], radius, cell) {
                                continue;
                            }
                            let nkey = (jc, jr, qf(nf));
                            if closed.contains(&nkey) { continue; }
                            if let Some(t) = tr.as_deref_mut() {
                                t.edge([a[0], a[1], cz], [land[0], land[1], nf],
                                    EdgeVerdict::Accepted { kind: EdgeKind::Jump });
                            }
                            let tentative = g_cur + (k as f32) * cell + JUMP_PENALTY;
                            if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                                g_score.insert(nkey, tentative);
                                came.insert(nkey, ckey);
                                floor_of.insert(nkey, nf);
                                heap.push(Node { f: tentative + h(jc, jr), c: jc, r: jr, fz: nf });
                            }
                            break; // nearest valid landing in this direction wins
                        }
                    }
                }
                // WATER DESCENT: if the neighbor column holds water below the current floor, allow
                // dropping/swimming down to the floor beneath it even past MAX_STEP_DOWN and without
                // a clear chest-height walking segment — you fall into the water and sink/swim. This
                // connects an upper walkway to a flooded lower level (e.g. qcat's canal → sewer).
                if let Some(water) = col.region_map() {
                    // Is there water somewhere in the column between here and far below?
                    let has_water = (1..=12).any(|k| water.is_water(b[0], b[1], cz - k as f32 * 8.0));
                    if has_water {
                        // Take the deepest floor in a deep probe that sits in/just under water.
                        for nf in col.column_floors(b[0], b[1], cz, STEP_H, 200.0) {
                            if nf >= cz - 1.0 { continue; } // descents only (the normal loop above
                            // handles same-level/climbs; a walkable shallow drop it already added)
                            // require the column at/just above this floor to be water (a real swim
                            // landing, not a dry lethal fall)
                            if !water.is_water(b[0], b[1], nf + 3.0) && !water.is_water(b[0], b[1], nf + 12.0) {
                                // Evaluated and REFUSED: the candidate landing is DRY — a lethal
                                // fall, not a swim landing. Record the refusal (#615 review F5).
                                if let Some(t) = tr.as_deref_mut() {
                                    t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                        EdgeVerdict::Rejected { reason: RejectReason::Water });
                                }
                                continue;
                            }
                            // #693: this loop iterates EVERY floor below, including tiers stacked
                            // beneath solid ground the body cannot fall through. Water is no
                            // obstruction (you sink through it), solid geometry is: require the
                            // destination column to be open between the takeoff band and this
                            // landing, exactly like the controlled fall.
                            if !col.descent_corridor_clear(b[0], b[1], cz, nf) {
                                if let Some(t) = tr.as_deref_mut() {
                                    t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                        EdgeVerdict::Rejected { reason: RejectReason::DescentBlocked });
                                }
                                continue;
                            }
                            let nkey = (nc, nr, qf(nf));
                            if closed.contains(&nkey) { continue; }
                            if let Some(t) = tr.as_deref_mut() {
                                t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                    EdgeVerdict::Accepted { kind: EdgeKind::WaterDescent });
                            }
                            // Steep per-depth cost so descending to a pool BOTTOM is a last resort:
                            // A* should cross a surface pool at the top (cheap surface-traversal edge
                            // above) and only dive when reaching a genuinely lower level is the only
                            // way (a flooded sewer). Without this bias A* dove straight to the floor
                            // of the Halas pool and the swimmer got stranded there (#191).
                            let step = (((dc * dc + dr * dr) as f32).sqrt()) * cell + (cz - nf) * 4.0;
                            let tentative = g_cur + step;
                            if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                                g_score.insert(nkey, tentative);
                                came.insert(nkey, ckey);
                                floor_of.insert(nkey, nf);
                                heap.push(Node { f: tentative + h(nc, nr), c: nc, r: nr, fz: nf });
                            }
                        }
                    }
                }

                // WATER ASCENT: the reverse of the descent — if THIS column is submerged
                // (water above the current floor), the character can swim up to the surface
                // and haul out onto a neighbor floor at or below surface + STEP_H. Without
                // this, flooded pits (qeynos2's moat) are one-way traps: descent gets you in,
                // and the normal climb's chest ray hits the pit wall on the way out.
                if let Some(water) = col.region_map() {
                    // Submerged (water ABOVE us), or floating AT the surface — a start anchored to
                    // the water surface (#329/#197p2) has air above it, so the old submerged-only
                    // test never fired for it and a floating swimmer had no way OUT of the water.
                    let probe = (0..=3).map(|k| cz + 2.0 + k as f32 * 4.0)
                        .find(|&z| water.is_water(a[0], a[1], z))
                        .or_else(|| water.is_water(a[0], a[1], cz - 1.0).then_some(cz - 1.0));
                    // The REAL surface of the water column (region-map binary search), NOT the old
                    // 2u upward scan: the haul-out cap below is measured from this plane, and a
                    // surface quantized 0..2u LOW silently shrank the cap by the same amount — a
                    // false `no_path` for a legal exit near the cap. `None` = an unbounded column
                    // (water for 200u+ up): there is no surface, so there is nothing to haul out at.
                    // Clamped to ≥ cz (the old scan started AT the node's floor): a floating node's
                    // own key IS the surface, and re-deriving it must never land a float-noise hair
                    // below and disable its haul-outs.
                    let surface = probe.and_then(|pz| water.surface_z(a[0], a[1], pz)).map(|s| s.max(cz));
                    if let Some(surface) = surface {
                        // THE HAUL-OUT CONTRACT (#359, design §4c option E3). The planner's exit
                        // cap and the controller's swim geometry are the SAME two Body fields:
                        //   • planner (here): admit a water→land exit only when the lip is
                        //     ≤ `haul_out_up` above the surface;
                        //   • controller (`movement.rs`): floats at `surface − float_depth`, rises
                        //     — collided, feet clamped to the water column — to the surface under
                        //     the walker's swim-up drive as it closes on the lip, then mounts it
                        //     with the swimming step-up (STEP_UP + GROUND_SNAP_TOL = 2.5u, so the
                        //     2.0u cap leaves 0.5u of margin).
                        // Before the contract the two sides drifted (#386-style): the planner
                        // measured `surface + 2.5` while the swimmer floated at `surface − 2.0`,
                        // so a "legal" riser was up to 4.5u against a 2.5u step and the character
                        // bobbed at the waterline forever (#359). A genuinely walkable exit is a
                        // beach/ramp, handled by the normal ground edges, not here.
                        let haul_out_up = eqoxide_zone_geometry::body::PLAYER_BODY.haul_out_up;
                        for nf in col.column_floors(b[0], b[1], surface, STEP_H, surface - cz) {
                            if nf <= cz + 1.0 { continue; }              // ascents only
                            if nf > surface + haul_out_up {              // too high to haul out of water
                                if let Some(t) = tr.as_deref_mut() {
                                    t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                        EdgeVerdict::Rejected { reason: RejectReason::HaulOutTooHigh });
                                }
                                continue;
                            }
                            let nkey = (nc, nr, qf(nf));
                            if closed.contains(&nkey) { continue; }
                            // Swim at the surface, then the usual chest clearance for the
                            // step out — the ray starts at swim height, so it passes over
                            // the pit lip that blocks the ground-level climb ray.
                            let ray_z = surface.max(nf - STEP_H);
                            if !col.edge_clear([a[0], a[1], ray_z + CHEST], [b[0], b[1], nf + CHEST], radius, cell) {
                                if let Some(t) = tr.as_deref_mut() {
                                    t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                        EdgeVerdict::Rejected { reason: RejectReason::Clearance });
                                }
                                continue;
                            }
                            if let Some(t) = tr.as_deref_mut() {
                                t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                    EdgeVerdict::Accepted { kind: EdgeKind::HaulOut });
                            }
                            let step = (((dc * dc + dr * dr) as f32).sqrt()) * cell + (nf - cz) * 0.5
                                + aggro_cost(b[0], b[1]);
                            let tentative = g_cur + step;
                            if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                                g_score.insert(nkey, tentative);
                                came.insert(nkey, ckey);
                                floor_of.insert(nkey, nf);
                                heap.push(Node { f: tentative + h(nc, nr), c: nc, r: nr, fz: nf });
                            }
                        }
                    }
                }

                // WATER SURFACE TRAVERSAL: swim ACROSS a body of water at its surface (#191). If the
                // neighbor column is swimmable water whose surface sits roughly level with our current
                // height (a ground-level pool, or the next cell of one we're already swimming), connect
                // at that surface — so A* crosses the TOP of a pool instead of diving to the bottom and
                // back (which fights the controller's buoyancy toward the surface). This makes a
                // surface pool (e.g. the Halas central pool on the way to the Everfrost line) a
                // crossable swim rather than a drop to the pool floor the fall-guard refuses.
                if let Some(water) = col.region_map() {
                    // Probe downward for the first swimmable water within a step of the current
                    // floor — a pool's surface often sits a little BELOW the shore you wade in from
                    // (Halas's central pool surface is ~5u under the ice), so a 1u probe would miss
                    // it. Take that water's surface as the swim height.
                    let mut surf = None;
                    let mut z = cz - 1.0;
                    while z >= cz - STEP_H {
                        if water.is_water(b[0], b[1], z) { surf = water.surface_z(b[0], b[1], z); break; }
                        z -= 4.0;
                    }
                    if let Some(surf) = surf {
                        let nkey = (nc, nr, qf(surf));
                        // No chest-clearance requirement (like WATER DESCENT): you swim across open
                        // water at the surface, and a dry-walk clearance ray from the shore floor
                        // would snag on the ice/rock lip at the pool's edge — which is exactly what
                        // pushed A* to dive to the bottom instead. Cheaper than the descent, so A*
                        // now prefers crossing at the top; the controller collide-and-slides off any
                        // wall that happens to sit in the water.
                        // The out-of-step-range refusal records as Water (#615 review F5): the
                        // neighbour IS water, but its surface sits beyond a step from here —
                        // evaluated and refused, not unevaluated.
                        if (surf - cz).abs() > STEP_H {
                            if let Some(t) = tr.as_deref_mut() {
                                t.edge([a[0], a[1], cz], [b[0], b[1], surf],
                                    EdgeVerdict::Rejected { reason: RejectReason::Water });
                            }
                        } else if surf < cz - eqoxide_zone_geometry::body::PLAYER_BODY.step_up
                            && !col.descent_corridor_clear(b[0], b[1], cz, surf)
                        {
                            // #693: a DOWNWARD entry onto a water surface must not pass through
                            // solid ground stacked between the shore floor and the water (a flooded
                            // tunnel under a street). This family deliberately casts no clearance
                            // ray (a dry ray would snag the pool lip), which also made it blind to
                            // an entire intervening floor — the corridor guard restores exactly that
                            // check and nothing else.
                            if let Some(t) = tr.as_deref_mut() {
                                t.edge([a[0], a[1], cz], [b[0], b[1], surf],
                                    EdgeVerdict::Rejected { reason: RejectReason::DescentBlocked });
                            }
                        } else if !closed.contains(&nkey) {
                            if let Some(t) = tr.as_deref_mut() {
                                t.edge([a[0], a[1], cz], [b[0], b[1], surf],
                                    EdgeVerdict::Accepted { kind: EdgeKind::SwimSurface });
                            }
                            let step = (((dc * dc + dr * dr) as f32).sqrt()) * cell + (surf - cz).abs() * 0.5;
                            let tentative = g_cur + step;
                            if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                                g_score.insert(nkey, tentative);
                                came.insert(nkey, ckey);
                                floor_of.insert(nkey, surf);
                                heap.push(Node { f: tentative + h(nc, nr), c: nc, r: nr, fz: surf });
                            }
                        }
                    }
                }

                // CONTROLLED FALL: step off a ledge / through a hole and fall to the floor below.
                // Allowed when (a) you can move horizontally off the edge at your CURRENT height (open
                // air beyond the ledge reads as clear), and (b) there's a landing floor within
                // MAX_FALL below. This is how levels joined by a drop (no walkable ramp, e.g. qcat's
                // dry sewer) connect. It's directional (you fall DOWN); climbing back needs a real
                // path. A per-unit fall cost makes A* prefer walking/stairs when a route exists.
                const MAX_FALL: f32 = 120.0;
                if col.edge_clear([a[0], a[1], cz + CHEST], [b[0], b[1], cz + CHEST], radius, cell) {
                    // The first surface you'd land on falling at b = highest floor below a real-step
                    // drop. (column_floors returns high→low, so `find` gives the first landing.)
                    if let Some(nf) = col.column_floors(b[0], b[1], cz, 0.0, MAX_FALL)
                        .into_iter().find(|&z| z < cz - STEP_H)
                    {
                        // #693: `find` skipped every floor above `nf` in b's column — but a falling
                        // body CANNOT skip a surface. If anything solid sits between the takeoff
                        // band and this landing (the qeynos street stacked over the aqueduct), the
                        // fall is a phantom: reject it, loudly, instead of routing the walker
                        // straight down through solid ground.
                        if !col.descent_corridor_clear(b[0], b[1], cz, nf) {
                            if let Some(t) = tr.as_deref_mut() {
                                t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                    EdgeVerdict::Rejected { reason: RejectReason::DescentBlocked });
                            }
                        } else if !closed.contains(&(nc, nr, qf(nf))) {
                            let nkey = (nc, nr, qf(nf));
                            if let Some(t) = tr.as_deref_mut() {
                                t.edge([a[0], a[1], cz], [b[0], b[1], nf],
                                    EdgeVerdict::Accepted { kind: EdgeKind::Fall });
                            }
                            // Huge flat cost: a controlled fall is a LAST RESORT. A* will only take
                            // it when there's no walkable route to the goal (e.g. a sealed lower
                            // level), never as a 2D "shortcut" that dives into a pit and climbs back.
                            const FALL_PENALTY: f32 = 50_000.0;
                            let step = FALL_PENALTY + (cz - nf) * 2.0;
                            let tentative = g_cur + step;
                            if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                                g_score.insert(nkey, tentative);
                                came.insert(nkey, ckey);
                                floor_of.insert(nkey, nf);
                                heap.push(Node { f: tentative + h(nc, nr), c: nc, r: nr, fz: nf });
                            }
                        }
                    }
                }

                // ── WATER ENTRY (land → water interior, Slice 2, design §7.1) ───────────────────
                // From a land floor (this node is not a water node — it fell through the water block
                // above), step into the neighbour column's TOP swim-plane node, so the 3D interior
                // volume becomes reachable. WADE when the swim plane is within a step of the floor;
                // DIVE-in when the water lies below (descents BELOW the top node are then ordinary
                // interior edges). No chest ray — like the surface-traversal edge, a dry-walk ray from
                // the shore floor would snag the pool lip; probe at swim height instead.
                if let Some(ncol) = water_grid.and_then(|g| g.column_at(b[0], b[1])) {
                    if let Some(top) = ncol.top_node_z() {
                        let wade = (top - cz).abs() <= STEP_H;
                        let dive = top < cz - 1.0;
                        let nkey = (nc, nr, qf(top));
                        // The old single combined condition is split so each REFUSAL records its
                        // own reason (#615 review F5): an unenterable swim plane (too far above
                        // to wade onto, not below to dive to) is Water; a blocked swim-height ray
                        // is Clearance. `closed` stays silent (that node's own expansion recorded
                        // its edges), and the ray is still cast exactly once.
                        if !(wade || dive) {
                            if let Some(t) = tr.as_deref_mut() {
                                t.edge([a[0], a[1], cz], [b[0], b[1], top],
                                    EdgeVerdict::Rejected { reason: RejectReason::Water });
                            }
                        } else if !closed.contains(&nkey) {
                            // #693: a DIVE-in reaches a swim plane BELOW the current floor — which
                            // must not tunnel through solid ground stacked in between (a flooded
                            // aqueduct under a street). Same phantom-descent guard as the fall
                            // families; a wade (plane within a step) has no gap and passes trivially.
                            if !col.descent_corridor_clear(b[0], b[1], cz, top) {
                                if let Some(t) = tr.as_deref_mut() {
                                    t.edge([a[0], a[1], cz], [b[0], b[1], top],
                                        EdgeVerdict::Rejected { reason: RejectReason::DescentBlocked });
                                }
                            } else if !col.edge_clear([a[0], a[1], top + 0.5], [b[0], b[1], top + 0.5], radius, cell) {
                                if let Some(t) = tr.as_deref_mut() {
                                    t.edge([a[0], a[1], cz], [b[0], b[1], top],
                                        EdgeVerdict::Rejected { reason: RejectReason::Clearance });
                                }
                            } else {
                                if let Some(t) = tr.as_deref_mut() {
                                    t.edge([a[0], a[1], cz], [b[0], b[1], top],
                                        EdgeVerdict::Accepted { kind: EdgeKind::WaterEntry });
                                }
                                let horiz = (((dc * dc + dr * dr) as f32).sqrt()) * cell;
                                let dz = top - cz;
                                // Bias toward wading in over leaping into the deep (design §6.5/§7.1):
                                // the vertical drop is charged at full (not half) rate so A* prefers a
                                // shallow entry when one exists, while a dive stays available where it
                                // is the way in.
                                let step = (horiz * horiz + dz * dz).sqrt() + dz.abs() * 0.5 + aggro_cost(b[0], b[1]);
                                let tentative = g_cur + step;
                                if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                                    g_score.insert(nkey, tentative);
                                    came.insert(nkey, ckey);
                                    floor_of.insert(nkey, top);
                                    heap.push(Node { f: tentative + h(nc, nr), c: nc, r: nr, fz: top });
                                }
                            }
                        }
                    }
                }
            }
        }
        // The ANCHORS the whole search hung off. Log them on the SUCCESS path too, not just on
        // failure: every anchor bug in this file's history (#229 goal-on-a-phantom-tier, #329
        // start-on-the-ceiling, #197p2 start-on-the-pool-bottom) presented as "A* returns a route
        // the walker can't follow" — a success with visibly wrong anchors. Without this line each
        // one cost days; with it, each is a 30-second read. Coarse whole-zone plans only (the fine
        // local tier re-plans every tick and would spam).
        if max_search.is_none() {
            tracing::info!("find_path: start=({:.0},{:.0},{:.1}) start_floor={:.2}{} goal=({:.0},{:.0},{:.1}) goal_floor={:.2}{}",
                start[0], start[1], start[2], start_floor,
                if floating_surface.is_some() { " (WATER SURFACE — floating)" } else { "" },
                goal[0], goal[1], goal[2], goal_floor,
                match ctx.goal_region { Some(i) => format!(" (zone-line region {i})"), None => String::new() });
        }
        // The closest-to-goal STANDING POSITION the search reached, in world space — carried out so
        // the cold honesty path can name the obstruction that ended the journey (#378 Phase 2).
        let best_toward_xyz = best_toward.map(|bk| {
            let ctr = center(bk.0, bk.1);
            [ctr[0], ctr[1], *floor_of.get(&bk).unwrap_or(&goal_floor)]
        });
        // How much straight-line ground toward the goal the best-reached cell actually closes. The
        // caller uses this to decide whether a partial route is a STAGE of a journey (worth walking,
        // then re-planning) or a SHUFFLE into a wall (#337) — see `PARTIAL_MIN_UNITS`.
        let progress = (h(sc, sr) - best_toward_h).max(0.0);
        // Prefer the requested tier; fall back to a wrong-tier goal only if the right tier is
        // unreachable (keeps the old "reach the goal cell at all" behaviour as a floor).
        let (goal_key, reached_goal) = match goal_key.or(goal_fallback) {
            Some(k) => (k, true),
            None => {
                // Partial route toward the frontier (#188). Built whenever the search made ANY
                // progress; whether it may be WALKED is the caller's call, and hinges on the one
                // question that used to be unanswerable: was the frontier closed, or did we just run
                // out of clock? `find_path_ex` walks it only under `Exhausted` and only past
                // `PARTIAL_MIN_UNITS`; a CLOSED search now reports `Unreachable` and no waypoints
                // at all, instead of handing the walker a greedy stub to wedge on (#337).
                let progressed = best_toward.is_some() && best_toward_h + 1.0 < h(sc, sr);
                match best_toward {
                    Some(bk) if progressed => {
                        tracing::info!("find_path: partial route toward goal (this-call expansions={}, {:.0}->{:.0} UNITS from goal, {})",
                            closed.len(), h(sc, sr), best_toward_h,
                            match limit { Some(l) => l.as_str(), None => "frontier CLOSED — goal is UNREACHABLE" });
                        (bk, false)
                    }
                    _ => {
                        match limit {
                            Some(l) => tracing::warn!("find_path: search hit a limit ({}) after {} nodes with no usable \
                                route — start_floor={start_floor} goal_floor={goal_floor}. This is NOT 'no route': the \
                                frontier was never closed.", l.as_str(), closed.len()),
                            None => tracing::info!("find_path: NO ROUTE — frontier closed after {} nodes \
                                (start_floor={start_floor}, goal_floor={goal_floor})", closed.len()),
                        }
                        return Search { limit, progress, closed_n: closed.len(), best_toward: best_toward_xyz, trace_call, ..Default::default() };
                    }
                }
            }
        };
        let mut path = Vec::new();
        let mut cur = goal_key;
        let mut used_climb = false;
        while cur != skey {
            let (c, r, fb) = cur;
            let ctr = center(c, r);
            // A TELEPORT-PAD endpoint waypoint (#403) is snapped to its EXACT resolved point, not the
            // 8u cell centre: the SOURCE must be a point KNOWN to be inside the trigger footprint (a
            // cell centre can fall just outside a sub-cell footprint, so the auto-cross never fires and
            // the walker walks the discontinuity off-mesh), and the DEST is the real arrival the route
            // continues from. Both were resolved+validated by `resolve_teleport_pads`. Match the FULL
            // key — cell AND floor bucket (#403 review B): a multi-level zone can route through the
            // pad's (col,row) at an UNRELATED z-tier, and snapping that waypoint to the pad endpoint's
            // z would inject a spurious vertical jump the walker clips/falls on.
            if climbed.contains(&cur) { used_climb = true; }
            let wp = ctx.teleport_pads.iter().find_map(|p| {
                let dc = to_cell(p.dest[0], p.dest[1]);
                let sc = to_cell(p.source[0], p.source[1]);
                if (c, r) == dc && fb == qf(p.dest[2])        { Some(p.dest) }
                else if (c, r) == sc && fb == qf(p.source[2]) { Some(p.source) }
                else { None }
            }).or_else(|| {
                // A CLIMB dismount waypoint is snapped to its EXACT resolved point for the same
                // reason a pad endpoint is (#403 review B, full key — cell AND floor bucket): the
                // dismount is a sub-cell ledge beside the ladder, and an 8u cell centre can easily
                // land back in the moat the character just climbed out of.
                col.climb_edges().iter().find_map(|e| {
                    let dc = to_cell(e.dismount[0], e.dismount[1]);
                    if (c, r) == dc && fb == qf(e.dismount[2]) { Some(e.dismount) } else { None }
                })
            }).unwrap_or_else(|| {
                // Carry each waypoint's actual floor height so the walker moves + collision-checks at
                // the right z while climbing/descending (instead of the goal's z, which clips walls).
                [ctr[0], ctr[1], *floor_of.get(&cur).unwrap_or(&goal[2])]
            });
            path.push(wp);
            match came.get(&cur) { Some(&p) => cur = p, None => break }
        }
        // `nav_climb` (#309): count the ROUTE, once, and only when the route actually handed back
        // traverses a climb edge. The climb mechanic is a reconstruction — the native client has one,
        // but how it triggers is not known from the decompile (see `crate::climb`) — so an agent must
        // be able to see that this particular route leans on it, the way `nav_tight` discloses a
        // minimum-clearance route. Counting frontier touches instead would report ladders the route
        // never uses and make the signal worthless.
        if used_climb {
            col.record_climb_plan();
        }
        path.reverse();
        // Edge margin (#312): A* routes through 8u cell CENTERS, so a cell that merely touches a
        // wall/ledge/waterline still counts as walkable and the followed straight line hugs that
        // boundary — the walker then clips the edge and falls off or wedges (the #314 city-wall
        // corner is exactly this). Inset each waypoint away from any unwalkable side by ~the
        // collision radius: sample the floor a margin out in ±E/±N; a side with no floor in a tight
        // band around the waypoint's z is a wall/drop/water edge, so nudge away from it. Opposing
        // walls (a narrow corridor) cancel out — the centre line is kept. A corner pushes away from
        // BOTH sides, unwedging it.
        //
        // NOTE (#358): this only sees sides where the FLOOR RUNS OUT — ledges, drops, waterlines. A
        // wall standing on continuous floor is invisible to it (there is still floor a margin out,
        // at the wall's foot), so the inset is not, and never was, what keeps a route off a WALL.
        // Measured on the live gfaydark→butcher wedge: the inset moved not one waypoint, and the
        // routes before and after widening the margin were byte-identical. Clearance from walls is
        // enforced by the edge test (`edge_clear`), not here.
        let margin = radius.max(1.0).min(cell * 0.45);
        // Walkable a margin out = a floor exists within a tight vertical band of the waypoint (so a
        // wall lip above, a drop below, or water all read as "edge" and get avoided).
        let edge_ok = |x: f32, y: f32, z: f32| -> bool {
            col.nearest_floor(x, y, z, 3.0, 8.0).is_some_and(|f| (f - z).abs() <= 8.0)
        };
        // WALL-AWARE half (#378 Phase 2 / the qcat L-corner). `edge_ok` sees only geometry that is
        // MISSING (drops, ledges, waterlines). A WALL standing on continuous floor is invisible to
        // it — there is floor at the wall's foot — so a route along a narrow ledge (wall one side,
        // drop the other) got pushed off the DROP but never off the WALL, and the walker pressed
        // into the wall trying to round the corner (the live qcat symptom). Probe for a solid face
        // at the body's own probe heights within its standing-room distance: a wall that close in a
        // direction is a hazard to push AWAY from, exactly as a missing floor is. On a narrow ledge
        // the wall push and the drop push point the SAME way — toward the ledge centre — so the
        // waypoint (and the carrot the walker steers at) sits down the middle and the corner is
        // roundable.
        //
        // DETECTION distance is the body `radius` (standing room = keep the body off the wall), not
        // the smaller in-cell `margin` — a cell centre sits ~1u from a wall on the 2u fine grid, and
        // a `margin`-only probe (0.9u there) would miss it. NUDGE magnitude stays `margin` (capped
        // to `cell*0.45` so a push never overshoots into the next cell). A corridor exactly
        // `2·radius` wide has a wall at `radius` on BOTH sides → the two pushes cancel → the centre
        // line is kept, so a minimum-width corridor is never sealed.
        let body = &eqoxide_zone_geometry::body::PLAYER_BODY;
        let wall_probe = radius.max(margin);
        let wall_near = |x: f32, y: f32, z: f32, ex: f32, ny: f32| -> bool {
            let (dx, dy) = (ex * wall_probe, ny * wall_probe);
            body.planner_probes().iter().any(|&hz|
                col.nearest_hit_t([x, y, z + hz], [x + dx, y + dy, z + hz]).is_some())
        };
        // The inset's occupancy guard, through the one authority (#378) — with NO ledge margin:
        // the inset asks "can the body stand exactly here", not "does this spot have route-choice
        // standing room". Margins are the search's concern; one here would refuse nudges the
        // search accepted.
        let inset_trav = crate::traversability::Traversability::new(col, radius, cell, 0.0, false);
        for wp in path.iter_mut() {
            let [x, y, z] = *wp;
            // A TELEPORT-PAD endpoint (#403 review D) must NOT be inset: near a footprint/ledge the
            // inset could nudge the SOURCE back OUT of the trigger volume (so the auto-cross never
            // fires) or move the DEST off its resolved arrival. These points were already validated
            // occupiable/standable by `resolve_teleport_pads`; leave them exactly where they are.
            if ctx.teleport_pads.iter().any(|p| *wp == p.source || *wp == p.dest) { continue; }
            let mut push = [0.0f32, 0.0f32];
            // A side is a hazard if the FLOOR runs out there (edge) OR a WALL stands there. Both
            // push away; opposing hazards (a corridor's two walls, a bridge's two rails) CANCEL, so
            // the centre line is kept and only genuinely one-sided hazards move the waypoint.
            if !edge_ok(x + margin, y, z) || wall_near(x, y, z, 1.0, 0.0) { push[0] -= 1.0; }
            if !edge_ok(x - margin, y, z) || wall_near(x, y, z, -1.0, 0.0) { push[0] += 1.0; }
            if !edge_ok(x, y + margin, z) || wall_near(x, y, z, 0.0, 1.0) { push[1] -= 1.0; }
            if !edge_ok(x, y - margin, z) || wall_near(x, y, z, 0.0, -1.0) { push[1] += 1.0; }
            let len = (push[0] * push[0] + push[1] * push[1]).sqrt();
            if len > 0.0 {
                let (nx, ny) = (x + push[0] / len * margin, y + push[1] / len * margin);
                // The nudge destination must be OCCUPIABLE — floor AND wall axes, through the one
                // authority (#378). The floor half is what stops the nudge shoving a waypoint off
                // the mesh; the wall half stops it nudging straight INTO the far wall of a corridor
                // narrower than 2·margin (where a one-sided push would otherwise cross the centre).
                if inset_trav.can_occupy_fast(crate::traversability::Point::new([nx, ny], z)) {
                    *wp = [nx, ny, z];
                }
            }
        }
        // Snap the final waypoint to the exact goal only when we actually reached the goal cell; a
        // partial path must end at the reachable cell, not clip toward an unreachable goal.
        if reached_goal {
            // #639: THE FINAL-HOP WALK-EDGE CHECK. Snapping the last waypoint from the reached
            // goal-cell centre to the exact goal APPENDS a hop — penultimate → goal — that skipped
            // the walk-edge predicate every A* edge passes. Validate it with the SAME predicate
            // (`final_hop_walkable` → `walk_profile_ok`, #630); if the final approach onto the goal
            // exceeds the controller's walk envelope, there is no walkable route to the EXACT goal
            // and claiming a complete route would reintroduce the #630 lie at the goal. Fail loudly
            // as `GoalNotWalkable` (→ nav_reason goal_not_walkable: re-aim, don't retry), exactly as
            // an intermediate edge that fails the predicate is dropped. Exemptions mirror the checks
            // above: a SWIM goal (water/floating waypoint z) is governed by the haul-out/buoyancy
            // logic, not walking grade, and a ZONE-LINE goal (`goal_region`) arrives by region-volume
            // containment, not a floor tier (same reason it is exempt from the immediate goal-floor
            // fail earlier in this function).
            if ctx.goal_region.is_none() && water_goal.is_none() && floating_goal.is_none() {
                // bz = the RESOLVED goal floor: the tier the walker physically settles on at the
                // goal XY (the same `goal_floor` A* aimed arrival at, and what `resolve_goal_floor`
                // reports), never the caller's raw z nor the 8u cell-centre floor. The walker
                // pure-pursues pen → goal following the real terrain, so this asks the true question:
                // can it walk that terrain up onto the goal's floor?
                let pen = if path.len() >= 2 { path[path.len() - 2] }
                          else { [start[0], start[1], start_floor] };
                if !final_hop_walkable(col, pen, [goal[0], goal[1]], goal_floor) {
                    tracing::info!("find_path: reached the goal cell but the FINAL hop onto goal \
                        ({:.0},{:.0},{:.1}) exceeds the walk envelope (#639) — no walkable route to \
                        the exact goal (goal_not_walkable)", goal[0], goal[1], goal[2]);
                    return Search { trace_call, ..Search::no_route(NoRoute::GoalNotWalkable) };
                }
                // #693: the DESCENT mirror. `final_hop_walkable` passes every descent by design
                // (only rises can hide an unclimbable face) — so a WRONG-TIER `goal_fallback`
                // arrival (goal cell reached on the tier ABOVE the requested one, e.g. the street
                // over the aqueduct) used to append a final waypoint that dives through the solid
                // ground between the tiers. A final drop past the step-down is only walkable if
                // the goal's column is open between the penultimate tier and the goal floor.
                let step_up = eqoxide_zone_geometry::body::PLAYER_BODY.step_up;
                if goal_floor < pen[2] - step_up
                    && !col.descent_corridor_clear(goal[0], goal[1], pen[2], goal_floor)
                {
                    tracing::info!("find_path: reached the goal cell {:.1}u ABOVE the goal tier with \
                        solid ground in between (#693) — the requested tier is not reachable by \
                        descending here (goal_not_walkable)", pen[2] - goal_floor);
                    return Search { trace_call, ..Search::no_route(NoRoute::GoalNotWalkable) };
                }
            }
            // A FLOATING water goal's final waypoint carries the anchored SURFACE tier, not the
            // caller's raw z — which may be airborne (z=0 over a lower pool surface) or deep
            // below it. Every water waypoint of a route carries the height the character can
            // actually hold there (design §4d), so the walker's final approach moves and
            // collision-checks at swim height instead of an air/underwater z.
            let gz = water_goal.or(floating_goal).unwrap_or(goal[2]);
            if let Some(last) = path.last_mut() { *last = [goal[0], goal[1], gz]; }
        } else {
            // A PARTIAL route can end inside a submerged dead-end pocket (e.g. a sunken pool whose
            // walls exceed the climb-out grade): `best_toward` picks the cell with the best
            // straight-line heuristic to the goal, with no way to know that continuing from a water
            // cell dead-ends (the water-ascent haul-out cap makes it topologically a trap). Walking
            // the char IN gets it stuck oscillating until it stalls out — see #259. Trim trailing
            // submerged waypoints so a partial route stops at the dry edge instead of driving into
            // the trap; an honest "no route" beats a one-way walk into a pit.
            while path.last().is_some_and(|&wp| col.in_water(wp)) {
                path.pop();
            }
            if path.is_empty() {
                return Search { limit, progress, closed_n: closed.len(), trace_call, ..Default::default() };
            }
        }
        // THE ROUTE BEGINS AT THE CHARACTER (#229's last mile, part 2). The walker follows the route
        // with pure pursuit, which steers along the segment (path[i], path[i+1]) — it ASSUMES path[0]
        // is where the character is standing. A* returns a start-EXCLUSIVE route, so the walker never
        // actually aims at the first waypoint: it projects itself onto the path[0]→path[1] segment
        // and steers along THAT, cutting the corner between them.
        //
        // When the first leg is a real manoeuvre, that corner-cut is fatal. Live everfrost: the
        // character is pressed against a wall, A* correctly routes NORTH off the wall and then west
        // around it — but pure pursuit projected the character onto the (north-waypoint → west-
        // waypoint) segment, aimed south-west along it, and drove straight back into the wall. It
        // made no progress, re-planned the identical (correct) route, and stalled out after 8
        // attempts. Prepending the character's own position makes the first pursuit segment
        // character→first-waypoint, so the route's first leg is actually walked.
        path.insert(0, [start[0], start[1], start_floor]);
        Search {
            path: Some((path, reached_goal)),
            limit,
            no_route: None,
            progress: if reached_goal { 0.0 } else { progress },
            closed_n: closed.len(),
            best_toward: best_toward_xyz,
            trace_call,
        }
    }

#[cfg(test)]
mod tests {
    use super::*;

    use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};

    /// Test helper: the HONEST outcome of the default whole-zone plan (#337/#356). Mirrors
    /// [`Collision::find_path`]'s simple signature, but returns the [`PlanOutcome`] that distinguishes
    /// a definitive `Unreachable` ("no route exists") from an `Exhausted` search ("I gave up") — the
    /// distinction a bare `Option<Vec<_>>` throws away. A negative assertion phrased as
    /// `plan(..).is_none()` passes for the WRONG REASON on a cut-short search AND gives zero protection
    /// against an over-restrictive A* regression; asserting the variant fixes both.
    fn plan(col: &Collision, start: [f32; 3], goal: [f32; 3], radius: f32) -> PlanOutcome {
        col.find_path_ex(start, goal, radius, &[], 8.0, None, 0.0, PlanCtx::default())
    }

    // ──────────────────── #693: the phantom stacked-tier descent (qeynos/qcat) ────────────────────
    /// An up-facing floor plate at height `z` over east [e0,e1] × north [n0,n1].
    /// (libeq pos = [north, height, east]; winding matches `floor_up`, verified up-facing.)
    fn plate(z: f32, e0: f32, e1: f32, n0: f32, n1: f32) -> MeshData {
        MeshData {
            positions: vec![[n0, z, e0], [n1, z, e0], [n1, z, e1], [n0, z, e1]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 2, 1, 0, 3, 2], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// TWO FULL STACKED FLOORS, no opening anywhere: an upper walkable surface (z=0) directly over
    /// a lower one (z=-30) — the qeynos street over the qcat aqueduct, distilled. There is NO
    /// walkable connection between the tiers at any XY.
    fn stacked_floors() -> Collision {
        Collision::build(&ZoneAssets {
            terrain: vec![plate(0.0, -60.0, 60.0, -60.0, 60.0), plate(-30.0, -60.0, 60.0, -60.0, 60.0)],
            objects: vec![], textures: vec![],
        }, 32.0)
    }

    /// **THE #693 REGRESSION — RED ON MAIN.** A goal on a lower floor tier stacked directly BENEATH
    /// solid ground must NOT be answered with a route that descends through that ground. On main the
    /// CONTROLLED-FALL edge picked the deep tier as a fall landing while skipping the very floor the
    /// character stands on (`find(|z| z < cz - STEP_H)` — a falling body cannot skip a surface), so
    /// the live qeynos→qcat-crossing walk got a route that dove through the street at
    /// ~(-502,-103) and the walker wedged there re-pathing forever (`nav_state: blocked`). The
    /// honest answer is `goal_not_walkable`: the goal's tier has no walkable link from anywhere in
    /// the start's component.
    ///
    /// MUTATION: revert the `descent_corridor_clear` guards (fall family + final-hop) → the plan
    /// returns a Route diving through the upper plate → RED. Verified at authoring time against
    /// unmodified main.
    #[test]
    fn a_stacked_tier_beneath_solid_ground_is_not_a_descent_landing() {
        let c = stacked_floors();
        // Multi-cell: approach across the upper tier toward a goal on the sealed lower tier.
        let out = plan(&c, [-40.0, 0.0, 0.0], [40.0, 0.0, -30.0], eqoxide_core::physics::PLAYER_RADIUS);
        assert!(out.route().is_none(),
            "#693: got a route to a sealed stacked tier — it can only descend THROUGH the upper \
             floor: {:?}", out.route());
        assert!(matches!(out, PlanOutcome::Unreachable { reason: NoRoute::GoalNotWalkable, .. }),
            "the honest verdict for a vertically sealed goal tier is goal_not_walkable, got {out:?}");
    }

    /// **THE #693 SAME-CELL VARIANT — RED ON MAIN.** A goal on the sealed lower tier of the START'S
    /// OWN cell used to return a straight-down two-point "route" (`final_hop_walkable` passes every
    /// descent by design) — the walker then walks in place on the street trying to sink through it.
    #[test]
    fn a_stacked_tier_beneath_the_start_cell_is_not_a_straight_walk_down() {
        let c = stacked_floors();
        let out = plan(&c, [4.0, 0.0, 0.0], [4.5, 0.5, -30.0], eqoxide_core::physics::PLAYER_RADIUS);
        assert!(out.route().is_none(),
            "#693 (same-cell): got a straight-down route through the upper floor: {:?}", out.route());
        assert!(matches!(out, PlanOutcome::Unreachable { reason: NoRoute::GoalNotWalkable, .. }),
            "same-cell sealed descent must be goal_not_walkable, got {out:?}");
    }

    /// **THE #693 OVER-TIGHTENING CONTROL.** The same two stacked floors WITH a real 16×16 opening
    /// in the upper plate: the descent guard must keep the genuine fall-through-the-hole route —
    /// the exact "descend at the real entrance" behaviour the fix exists to preserve. The route
    /// must reach the lower tier, and its descent step must land INSIDE the opening (a fall
    /// corridor is only clear there), not through the solid part of the plate.
    #[test]
    fn a_real_opening_in_the_upper_tier_still_descends_through_it() {
        // Upper plate with a hole over east [8,24] × north [-8,8]; lower plate solid.
        let c = Collision::build(&ZoneAssets {
            terrain: vec![
                plate(0.0, -60.0, 8.0, -60.0, 60.0),   // west of the hole
                plate(0.0, 24.0, 60.0, -60.0, 60.0),   // east of the hole
                plate(0.0, 8.0, 24.0, -60.0, -8.0),    // south of the hole
                plate(0.0, 8.0, 24.0, 8.0, 60.0),      // north of the hole
                plate(-30.0, -60.0, 60.0, -60.0, 60.0), // the lower tier
            ],
            objects: vec![], textures: vec![],
        }, 32.0);
        let out = plan(&c, [-40.0, 0.0, 0.0], [40.0, 0.0, -30.0], eqoxide_core::physics::PLAYER_RADIUS);
        let route = out.route().unwrap_or_else(|| panic!(
            "#693 over-tightened: a lower tier with a REAL opening must still route (fall through \
             the hole), got {out:?}"));
        assert!(route.last().is_some_and(|w| (w[2] - (-30.0)).abs() < 1.0),
            "route must end on the lower tier: {route:?}");
        // The drop must happen AT the opening: find the descending step and check its landing.
        let drop = route.windows(2).find(|w| w[0][2] - w[1][2] > 20.0)
            .unwrap_or_else(|| panic!("route has no descent step: {route:?}"));
        let land = drop[1];
        assert!((4.0..=28.0).contains(&land[0]) && (-12.0..=12.0).contains(&land[1]),
            "the descent must land inside/at the opening (east 8..24, north -8..8), landed at \
             ({:.1},{:.1},{:.1}) — a phantom through-the-plate descent", land[0], land[1], land[2]);
    }

    /// **THE #693 WATER-FAMILY VARIANT — RED ON MAIN.** The same stacked seal, but the lower tier
    /// is FLOODED (water fills −30..−10 under the upper plate). The WATER-DESCENT and WATER-ENTRY
    /// families had no corridor/clearance check at all ("you fall into the water and sink"), so
    /// they tunnelled through the upper floor exactly like the fall family. Water is not an
    /// obstruction to a descent — but the solid floor above it is.
    #[test]
    fn a_flooded_stacked_tier_beneath_solid_ground_is_not_a_descent_landing() {
        // Vertical wall panels sealing the tunnel's perimeter, so the flooded lower tier has no
        // genuine way in around the plate edges (without them the fixture's world-wide water lets
        // A* legitimately swim in from beyond the plates — a REAL route, not the phantom).
        let wall_e = |e: f32| MeshData {
            positions: vec![[-60.0, -32.0, e], [60.0, -32.0, e], [60.0, 2.0, e], [-60.0, 2.0, e]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        };
        let wall_n = |n: f32| MeshData {
            positions: vec![[n, -32.0, -60.0], [n, -32.0, 60.0], [n, 2.0, 60.0], [n, 2.0, -60.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        };
        let mut c = Collision::build(&ZoneAssets {
            terrain: vec![
                plate(0.0, -60.0, 60.0, -60.0, 60.0), plate(-30.0, -60.0, 60.0, -60.0, 60.0),
                wall_e(-60.0), wall_e(60.0), wall_n(-60.0), wall_n(60.0),
            ],
            objects: vec![], textures: vec![],
        }, 32.0);
        // Water bounded to the tunnel interior (−30..−10 between the plates) — NOT world-wide
        // (`flat_below` water outside the plates would let A* legitimately swim in around the
        // edge, which is a real route, not the phantom).
        c.set_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::water_boxes(&[[-59.0, 59.0, -59.0, 59.0, -29.5, -10.0]]))));
        let out = plan(&c, [-40.0, 0.0, 0.0], [40.0, 0.0, -30.0], eqoxide_core::physics::PLAYER_RADIUS);
        assert!(out.route().is_none(),
            "#693 (water): got a route diving through the upper floor into the flooded tier: {:?}",
            out.route());
        assert!(matches!(out, PlanOutcome::Unreachable { reason: NoRoute::GoalNotWalkable, .. }),
            "a flooded sealed tier must be goal_not_walkable, got {out:?}");
    }

    /// A flooded pit must be exitable by SWIMMING UP: pit floor at z=0, a cliff wall
    /// up to the bank at z=10, water filling the pit to z=9. Without water the chest
    /// ray for the climb crosses the cliff face and the pit is sealed (qeynos2 moat,
    /// asset-server#14 / eqoxide#2 Case B). With water, A* must swim to the surface
    /// and haul out onto the bank.
    #[test]
    fn find_path_swims_up_out_of_a_flooded_pit() {
        let mesh = |positions: Vec<[f32; 3]>| MeshData {
            positions, normals: vec![], uvs: vec![],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // EQ WLD pos = [north, height, east].
        let pit_floor = mesh(vec![[0.0, 0.0, 0.0], [0.0, 0.0, 24.0], [24.0, 0.0, 24.0], [24.0, 0.0, 0.0]]);
        let cliff     = mesh(vec![[0.0, 0.0, 24.0], [24.0, 0.0, 24.0], [24.0, 10.0, 24.0], [0.0, 10.0, 24.0]]);
        let bank      = mesh(vec![[0.0, 10.0, 24.0], [0.0, 10.0, 48.0], [24.0, 10.0, 48.0], [24.0, 10.0, 24.0]]);
        let assets = ZoneAssets { terrain: vec![pit_floor, cliff, bank], objects: vec![], textures: vec![] };

        let start = [8.0, 12.0, 0.0];   // pit floor
        let goal  = [40.0, 12.0, 10.0]; // bank

        // Dry pit: sealed — the climb's chest ray crosses the cliff face. This is a DEFINITIVE
        // Unreachable (frontier closed), NOT an Exhausted "I gave up" — a bare `.is_none()` here
        // could not tell the two apart and would pass even on a cut-short search (#356).
        let dry = Collision::build(&assets, 4.0);
        assert!(matches!(plan(&dry, start, goal, 1.0), PlanOutcome::Unreachable { .. }),
            "dry pit should be sealed (Unreachable — no walkable exit), got {:?}", plan(&dry, start, goal, 1.0));

        // Flooded to z=9: swim up and haul out onto the bank.
        let mut wet = Collision::build(&assets, 4.0);
        wet.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::flat_below(9.0))));
        let path = wet.find_path(start, goal, 1.0, &[], false);
        assert!(path.is_some(), "flooded pit must be exitable by swimming up to the bank");
        let last = *path.unwrap().last().unwrap();
        assert!((last[0] - goal[0]).abs() < 8.0 && (last[1] - goal[1]).abs() < 8.0,
            "path should end at the bank goal, got {last:?}");
    }

    /// eqoxide#212: A* must refuse a ramp too steep to walk (it slides/wedges), while taking a
    /// gentle ramp of the same rise. Same geometry, only the ramp's run (steepness) differs.
    #[test]
    fn find_path_rejects_too_steep_ramp() {
        // MeshData pos = [north, up, east]; Collision maps to world [east, north, up].
        let quad = |v: Vec<[f32; 3]>| MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // Build: low floor (east -40..0, z=0), a ramp (east 0..RUN, z 0->30), high plateau
        // (east RUN..RUN+40, z=30). Start on the low floor, goal on the plateau. North spans 0..40.
        let scene = |run: f32| {
            let low  = quad(vec![[0.0, 0.0, -40.0], [40.0, 0.0, -40.0], [40.0, 0.0, 0.0], [0.0, 0.0, 0.0]]);
            let ramp = quad(vec![[0.0, 0.0, 0.0], [40.0, 0.0, 0.0], [40.0, 30.0, run], [0.0, 30.0, run]]);
            let high = quad(vec![[0.0, 30.0, run], [40.0, 30.0, run], [40.0, 30.0, run + 40.0], [0.0, 30.0, run + 40.0]]);
            ZoneAssets { terrain: vec![low, ramp, high], objects: vec![], textures: vec![] }
        };
        let start = [-20.0, 20.0, 0.0]; // low floor (world [east,north,up])

        // Gentle ramp: 30u rise over 48u run = grade 0.625 < 1.2 → walkable.
        let gentle = Collision::build(&scene(48.0), 4.0);
        let goal_g = [48.0 + 20.0, 20.0, 30.0];
        let p_gentle = gentle.find_path(start, goal_g, 1.0, &[], false);
        assert!(p_gentle.is_some(), "a gentle (0.625) ramp must be walkable");
        let last = *p_gentle.unwrap().last().unwrap();
        assert!(last[2] > 20.0, "gentle path should reach the high plateau (z~30), got {last:?}");

        // Steep ramp: 30u rise over 16u run = grade 1.875 > 1.2 → A* must refuse to climb it, so
        // the plateau is unreachable (no partial route reaches the top tier).
        let steep = Collision::build(&scene(16.0), 4.0);
        let goal_s = [16.0 + 20.0, 20.0, 30.0];
        // Unreachable, NOT a bare None: the plateau's tier is genuinely sealed off (frontier closed),
        // and asserting the variant guards against an over-restrictive A* regression too (#356).
        assert!(matches!(plan(&steep, start, goal_s, 1.0), PlanOutcome::Unreachable { .. }),
            "a 1.875-grade ramp is too steep — A* must refuse it (Unreachable), got {:?}",
            plan(&steep, start, goal_s, 1.0));
    }

    // ───────────────────────── #403: intra-zone teleport-pad edges ─────────────────────────
    // A pad is a DISCONTINUOUS link terrain-follow A* cannot express: two walkable components joined
    // ONLY by a teleport. Before the pad edge the planner floods the start's component, never reaches
    // the goal's, and returns a false `Unreachable(SearchClosed)` — the exact agent-honesty violation
    // #403 is about (the goal really IS reachable). These fixtures reproduce that topology on a
    // synthetic scene (deterministic, CI-runnable — the real-zone qeynos2 variant is env-gated below),
    // and are mutation-checked: revert the astar pad-edge emission and `pad_two_components_*` go RED.
    /// A scene of TWO disconnected floor slabs (A near the origin, B ~400u east across a gap far wider
    /// than any jump) with a DRNTP pad footprint on slab A. Returns
    /// `(collision, start_on_A, goal_on_B, pad_index, footprint_xy)`; the caller supplies the pad's
    /// advertised destination (the scene geometry is the same either way).
    /// MeshData pos = `[north, up, east]`; Collision maps to world `[east, north, up]`.
    fn pad_scene() -> (Collision, [f32; 3], [f32; 3], i32, [f32; 2]) {
        let quad = |v: Vec<[f32; 3]>| MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // Slab A: east[-120,0] × north[0,80] @ z=0.  Slab B: east[400,480] × north[0,80] @ z=0.
        // 400u of empty air between them — no walk, no jump (~22u reach) can bridge it. Slab A is
        // deliberately LARGE (≥64 nav cells) so the start's reachable component is a whole surveyed
        // region: the no-pad baseline then closes its frontier and reports the issue's exact symptom,
        // `SearchClosed` ("no route from here"), not `StartIsolated` ("boxed in").
        let slab_a = quad(vec![[0.0, 0.0, -120.0], [80.0, 0.0, -120.0], [80.0, 0.0, 0.0], [0.0, 0.0, 0.0]]);
        let slab_b = quad(vec![[0.0, 0.0, 400.0], [80.0, 0.0, 400.0], [80.0, 0.0, 480.0], [0.0, 0.0, 480.0]]);
        let mut col = Collision::build(
            &ZoneAssets { terrain: vec![slab_a, slab_b], objects: vec![], textures: vec![] }, 8.0);
        const PAD_INDEX: i32 = 42;
        // Pad footprint = a DRNTP box on slab A: north[30,50] × east[-40,-16], z-slab[-5,5] (straddles
        // the z=0 floor so standing on it — feet at z≈1 — is inside the region and would fire the
        // crossing). set_water precomputes its representative floor point (centroid ≈ east -28, north 40).
        col.set_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_box(30.0, 50.0, -40.0, -16.0, -5.0, 5.0, PAD_INDEX))));
        let start = [-112.0, 40.0, 0.0]; // slab A, clear of the footprint
        let goal  = [450.0, 40.0, 0.0];  // slab B
        (col, start, goal, PAD_INDEX, [-28.0, 40.0])
    }

    /// THE #403 GATE (mutation-checked). A goal reachable ONLY across a pad:
    /// * with NO pad edge → `Unreachable` (the two components are genuinely disconnected by terrain);
    /// * with the pad edge → a complete `Route` that TRAVERSES the pad (the destination point appears
    ///   in the path), proving A* routed THROUGH the discontinuous link rather than around it.
    ///   Revert the astar pad-edge emission and the with-pad case falls back to `Unreachable` → RED.
    #[test]
    fn pad_two_components_route_only_through_the_pad() {
        let dest = [430.0, 40.0, 0.0]; // on slab B, distinct from the goal
        let (col, start, goal, index, fp) = pad_scene();

        // Baseline: no pad edges → the goal's component is unreachable, and honestly so — the exact
        // #403 symptom: the frontier CLOSES (SearchClosed), it is not a boxed-in start.
        let bare = col.find_path_ex(start, goal, 1.0, &[], 8.0, None, 0.0, PlanCtx::default());
        assert!(matches!(bare, PlanOutcome::Unreachable { reason: NoRoute::SearchClosed, .. }),
            "with NO pad edge the two slabs are disconnected — must be Unreachable(SearchClosed), got {bare:?}");

        // The honesty-gated resolver turns the advertised same-zone pad into exactly one edge.
        let pads = col.resolve_teleport_pads(&[(index, dest)]);
        assert_eq!(pads.len(), 1, "the advertised same-zone pad must resolve to one edge, got {pads:?}");

        let ctx = PlanCtx::default().with_teleport_pads(pads);
        let out = col.find_path_ex(start, goal, 1.0, &[], 8.0, None, 0.0, ctx);
        let route = match out {
            PlanOutcome::Route(p) => p,
            other => panic!("with the pad edge the goal is reachable — expected a Route, got {other:?}"),
        };
        // The route must actually STEP ON the pad footprint and CONTINUE FROM the arrival point:
        // both the source (on slab A) and the destination (on slab B) appear as waypoints.
        let near = |wp: &[f32; 3], x: f32, y: f32| (wp[0] - x).abs() <= 8.0 && (wp[1] - y).abs() <= 8.0;
        assert!(route.iter().any(|wp| near(wp, fp[0], fp[1])),
            "route must reach the pad footprint on slab A (~{},{}): {route:?}", fp[0], fp[1]);
        assert!(route.iter().any(|wp| near(wp, dest[0], dest[1])),
            "route must traverse the pad — the arrival point (~{},{}) must appear: {route:?}", dest[0], dest[1]);
        let last = *route.last().unwrap();
        assert!(near(&last, goal[0], goal[1]), "route must end at the goal on slab B, got {last:?}");
    }

    /// HONESTY GUARD (#403): a pad edge must never FABRICATE reachability. An advertised pad whose
    /// destination is out over the void (no floor anywhere in its column) resolves to NO edge, so a
    /// goal reachable only via that (non-existent) link stays honestly `Unreachable` — the inverse
    /// bug (unreachable reported reachable) is what this pins against.
    #[test]
    fn pad_with_void_destination_creates_no_edge() {
        let void_dest = [200.0, 40.0, 0.0]; // in the 400u gap — no floor beneath it
        let (col, start, goal, index, _fp) = pad_scene();

        let pads = col.resolve_teleport_pads(&[(index, void_dest)]);
        assert!(pads.is_empty(),
            "a pad whose destination has no walkable floor must create NO edge (never fabricate \
             reachability), got {pads:?}");

        // And an index that is not a DRNTP region in this zone at all also yields no edge.
        let unknown = col.resolve_teleport_pads(&[(999, [430.0, 40.0, 0.0])]);
        assert!(unknown.is_empty(), "an index with no footprint region must create no edge, got {unknown:?}");

        // With no resolved edge, the disconnected goal is still an honest Unreachable — not a Route
        // across a link that would strand the character in the void.
        let ctx = PlanCtx::default().with_teleport_pads(pads);
        let out = col.find_path_ex(start, goal, 1.0, &[], 8.0, None, 0.0, ctx);
        assert!(matches!(out, PlanOutcome::Unreachable { .. }),
            "a void-destination pad must not make the goal reachable, got {out:?}");
    }

    /// HONESTY GUARD (#403, #266): a DRNTP footprint that FLOATS in a z-slab above its floor — a
    /// character standing at the XY is BELOW the trigger volume, so the auto-cross never fires — must
    /// create NO planner edge, even though the region IS present in the map. Otherwise the planner
    /// reports a `Route` whose pad the walker can't trigger: it arrives, nothing teleports, and it
    /// walks the discontinuity off-mesh (false reachability). This is exactly the ungated
    /// `find_zone_line_near` raw-point fallback the review flagged; `resolve_teleport_pads` gates the
    /// SOURCE through `zone_line_floor_point` (inside-region at standing height) instead.
    #[test]
    fn pad_with_a_floating_footprint_creates_no_edge() {
        let quad = |v: Vec<[f32; 3]>| MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let slab_a = quad(vec![[0.0, 0.0, -120.0], [80.0, 0.0, -120.0], [80.0, 0.0, 0.0], [0.0, 0.0, 0.0]]);
        let slab_b = quad(vec![[0.0, 0.0, 400.0], [80.0, 0.0, 400.0], [80.0, 0.0, 480.0], [0.0, 0.0, 480.0]]);
        // A tall thin wall AWAY from the footprint XY (east=-100) only extends the zone's z-extent so
        // the floating footprint region falls within the precompute bounds — it is NOT a floor under
        // the pad. Without it the region would be missed for a different reason and the test vacuous.
        let wall = quad(vec![[0.0, 0.0, -100.0], [80.0, 0.0, -100.0], [80.0, 40.0, -100.0], [0.0, 40.0, -100.0]]);
        let mut col = Collision::build(
            &ZoneAssets { terrain: vec![slab_a, slab_b, wall], objects: vec![], textures: vec![] }, 8.0);
        const IDX: i32 = 42;
        // Footprint floats in z[30,40] over slab A's z=0 floor — nothing standable is inside it.
        col.set_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_box(30.0, 50.0, -40.0, -16.0, 30.0, 40.0, IDX))));
        // The region IS present (precompute found it): the OLD ungated resolver would have taken this
        // raw, non-standable region point as a source. `is_some` here is what makes the assertion
        // below about the GATE, not about a missing region.
        assert!(col.find_zone_line_near(Some(IDX), [430.0, 40.0, 0.0]).is_some(),
            "fixture sanity: the floating footprint region must be present in the map");
        let pads = col.resolve_teleport_pads(&[(IDX, [430.0, 40.0, 0.0])]);
        assert!(pads.is_empty(),
            "a floating footprint with no standable trigger floor must create NO edge (its auto-cross \
             would never fire — a Route the walker can't take), got {pads:?}");
    }

    /// #266 — the qeynos2 Knights-of-Truth waterfall geometry: a DRNTP trigger volume whose lower
    /// face floats ~0.4u ABOVE the flat vault floor. A resting character stands with its feet at the
    /// floor, BELOW the volume, so the auto-cross MOVER's old feet-only `zone_line_at([x,y,feet])`
    /// probe never fired while standing on the disclosed footprint (only a jump, lifting the feet
    /// into the volume, crossed). The footprint VALIDATOR (`teleport_pad_source`) meanwhile validates
    /// at feet+1 (inside the volume) yet REPORTS at the floor — so the disclosed footprint validated
    /// but standing on it did nothing: the feet-vs-feet+1 divergence.
    ///
    /// `zone_line_at_standing` sweeps the standing capsule span `[feet, feet+height]`, which INCLUDES
    /// feet+1, so a character standing exactly on the disclosed footprint now detects the pad.
    ///
    /// Mutation check: revert `zone_line_at_standing` to a feet-only probe → the two
    /// `zone_line_at_standing(...) == Some(IDX)` assertions go RED (they collapse to the feet-only
    /// `zone_line_at` which returns `None` here). And on unmodified `origin/main` the method does not
    /// exist; the equivalent main-mover probe is `zone_line_at(footprint)`, which the `NOTE` assertion
    /// below shows returns `None` — i.e. main's mover misses the disclosed footprint (the #266 bug).
    #[test]
    fn standing_on_266_waterfall_footprint_fires_the_crossing() {
        let quad = |v: Vec<[f32; 3]>| MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // Flat vault floor at up=-14 (MeshData pos = [north, up, east]), spanning the footprint XY.
        let floor = quad(vec![[0.0, -14.0, -120.0], [80.0, -14.0, -120.0], [80.0, -14.0, 0.0], [0.0, -14.0, 0.0]]);
        // A vertical wall AWAY from the footprint XY (east=-100) that only extends the zone's z-extent
        // up to -4 so the region-point precompute's bounds (derived from the mesh AABB) include the
        // trigger volume's z-slab. It is NOT floor/ceiling under the pad — same device the floating-
        // footprint test above uses. Without it the region is never sampled and the test is vacuous.
        let wall = quad(vec![[0.0, -14.0, -100.0], [80.0, -14.0, -100.0], [80.0, -4.0, -100.0], [0.0, -4.0, -100.0]]);
        let mut col = Collision::build(
            &ZoneAssets { terrain: vec![floor, wall], objects: vec![], textures: vec![] }, 8.0);
        const IDX: i32 = 2; // the qeynos2 waterfall's zone-point index
        // DRNTP trigger volume: north[30,50] × east[-40,-16], z-slab [-13.6, -6.0]. Its LOWER FACE
        // (-13.6) floats 0.4u above the -14.0 floor, exactly the #266 geometry: feet (-14.0) are
        // below it; feet+1 (-13.0) and the rest of the standing body (up to -8.0) are inside it.
        col.set_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_box(30.0, 50.0, -40.0, -16.0, -13.6, -6.0, IDX))));

        const FEET: f32 = -14.0;
        let xy = [-28.0_f32, 40.0]; // centroid of the footprint box

        // GEOMETRY (documents the divergence; true on main AND branch):
        // the feet point is below the trigger, feet+1 is inside it.
        assert_eq!(col.zone_line_at([xy[0], xy[1], FEET]), None,
            "feet-only probe (what main's mover used) must MISS — feet sit below the trigger's lower face");
        assert_eq!(col.zone_line_at([xy[0], xy[1], FEET + 1.0]), Some(IDX),
            "feet+1 (what the validator checks) must be INSIDE the trigger");

        // THE FIX: a standing character's capsule span reaches the trigger, so the crossing fires.
        assert_eq!(col.zone_line_at_standing([xy[0], xy[1], FEET]), Some(IDX),
            "zone_line_at_standing must detect the pad a character standing on the footprint occupies");

        // TIE TO THE DISCLOSURE CONTRACT: the footprint the client DISCLOSES (validated + reported by
        // `teleport_pad_source`, via `teleport_pad_footprints`) sits on the floor — BELOW the trigger —
        // yet standing there must now fire the mover's probe.
        let fps = col.teleport_pad_footprints(IDX);
        assert_eq!(fps.len(), 1, "the standable footprint leaf must resolve to exactly one disclosed point, got {fps:?}");
        let disclosed = fps[0];
        assert!((disclosed[2] - FEET).abs() < 0.5,
            "the disclosed footprint is reported at the floor z (~{FEET}), below the trigger — got {disclosed:?}");
        // NOTE (the #266 bug on main): the disclosed footprint z is NOT detected by the feet-only
        // probe main's mover used — this is precisely why standing on it never crossed.
        assert_eq!(col.zone_line_at(disclosed), None,
            "main's feet-only mover probe MISSES the disclosed footprint (the #266 bug this fix closes)");
        // AFTER THE FIX: the capsule-span mover probe fires on the disclosed footprint.
        assert_eq!(col.zone_line_at_standing(disclosed), Some(IDX),
            "the standing mover probe must fire the crossing when the agent stands on the disclosed footprint");
    }

    /// HONESTY (#403 review A): a same-index footprint baked as SEVERAL horizontally-separated leaves
    /// must emit an edge for EVERY standable leaf, not just the first — else A* could be handed the
    /// one leaf sitting in a component it can't reach and report a FALSE `Unreachable` while a
    /// reachable leaf of the SAME pad exists.
    #[test]
    fn pad_emits_one_edge_per_footprint_leaf() {
        let quad = |v: Vec<[f32; 3]>| MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // One big walkable slab (east[-120,0] × north[0,80]) so both footprint bands sit on floor.
        let slab = quad(vec![[0.0, 0.0, -120.0], [80.0, 0.0, -120.0], [80.0, 0.0, 0.0], [0.0, 0.0, 0.0]]);
        let mut col = Collision::build(
            &ZoneAssets { terrain: vec![slab], objects: vec![], textures: vec![] }, 8.0);
        const IDX: i32 = 42;
        // Two footprint leaves of the SAME index: north[10,25] and north[45,60], both east[-40,-16].
        col.set_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_two_boxes(10.0, 25.0, 45.0, 60.0, -40.0, -16.0, -5.0, 5.0, IDX))));
        // Destination on the same slab (a walkable floor) so the DEST guard is satisfied and the test
        // isolates the SOURCE per-leaf behaviour.
        let edges = col.resolve_teleport_pads(&[(IDX, [-70.0, 40.0, 0.0])]);
        assert_eq!(edges.len(), 2,
            "a two-leaf footprint must yield one PadEdge PER leaf (both standable), got {edges:?}");
        // The two sources sit in the two distinct north bands; both share the one destination.
        let norths: Vec<f32> = edges.iter().map(|e| e.source[1]).collect();
        assert!(norths.iter().any(|&n| (10.0..=25.0).contains(&n)), "a source in band A: {norths:?}");
        assert!(norths.iter().any(|&n| (45.0..=60.0).contains(&n)), "a source in band B: {norths:?}");
    }

    // ───────────────────── #309: the ladder climb edge (crushbone's moat) ─────────────────────
    // A ladder is the SECOND discontinuous link terrain-follow A* cannot express (the first was the
    // teleport pad, #403). A vertical face is CORRECTLY refused by the walk test — a 24u rise is far
    // over `STEP_H`, and its grade far over `MAX_WALK_GRADE` — so a pit whose only exit is a ladder
    // floods the frontier, closes it, and reports `Unreachable`, while the native client climbs
    // straight out (demonstrated by the repo owner against the retail binary; see `eqoxide_zone_geometry::climb` for
    // what is derived from the client and what is a guess). The fix is a new EDGE KIND, never a
    // loosened walk test: baking a ladder into walkable ground would be the #229/#329 class of lie.
    // These fixtures reproduce Crushbone's moat topology synthetically and are mutation-checked —
    // revert the astar climb-edge emission and `moat_with_a_ladder_routes_out` goes RED.
    /// A walled pit (floor at up=0, east[0,80] × north[0,80]) whose east wall rises 24u to a rim
    /// (up=24, east[80,200]). `ladder_east` optionally stands a `LADDER14` panel, floor-to-rim, at
    /// that east coordinate. The pit is deliberately ≥64 nav cells so the ladderless baseline closes
    /// a whole surveyed frontier and reports `SearchClosed` ("no way out of here"), not the weaker
    /// `StartIsolated` ("boxed in where you stand").
    /// MeshData pos = `[north, up, east]`; Collision maps to world `[east, north, up]`.
    fn moat_scene(ladder_east: Option<f32>) -> (Collision, [f32; 3], [f32; 3]) {
        let quad = |v: Vec<[f32; 3]>| MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let floor = quad(vec![[0.0, 0.0, 0.0], [80.0, 0.0, 0.0], [80.0, 0.0, 80.0], [0.0, 0.0, 80.0]]);
        let rim   = quad(vec![[0.0, 24.0, 80.0], [80.0, 24.0, 80.0], [80.0, 24.0, 200.0], [0.0, 24.0, 200.0]]);
        let wall  = quad(vec![[0.0, 0.0, 80.0], [80.0, 0.0, 80.0], [80.0, 24.0, 80.0], [0.0, 24.0, 80.0]]);
        let objects = ladder_east.map(|e| eqoxide_assets::ObjectModel {
            name: "LADDER14".into(),
            // Floor-to-rim, 8u wide across north, centred on north 40 — the same shape as Crushbone's
            // `LADDER14`, whose 24.93u scaled height likewise spans moat floor to rim.
            meshes: vec![quad(vec![[36.0, 0.0, e], [44.0, 0.0, e], [44.0, 24.0, e], [36.0, 24.0, e]])],
            instances: vec![[[1., 0., 0., 0.], [0., 1., 0., 0.], [0., 0., 1., 0.], [0., 0., 0., 1.]]],
        }).into_iter().collect();
        let col = Collision::build(
            &ZoneAssets { terrain: vec![floor, rim, wall], objects, textures: vec![] }, 8.0);
        (col, [20.0, 40.0, 0.0], [160.0, 40.0, 24.0])
    }

    /// The baseline this whole feature exists to change: with no ladder the pit is a genuine trap,
    /// and A* says so definitively (`SearchClosed`) rather than guessing.
    #[test]
    fn moat_without_a_ladder_is_a_sealed_trap() {
        let (col, start, goal) = moat_scene(None);
        assert!(col.climb_edges().is_empty(), "no ladder object — no climb edge");
        let out = plan(&col, start, goal, 1.0);
        assert!(matches!(out, PlanOutcome::Unreachable { reason: NoRoute::SearchClosed, .. }),
            "a 24u wall with no ladder seals the pit — expected Unreachable(SearchClosed), got {out:?}");
    }

    /// THE #309 GATE (mutation-checked). The same pit, with a `LADDER14` against the wall: a complete
    /// route out, ending on the rim, and counted as a climb so `nav_climb` can disclose that the
    /// route leans on an UNVERIFIED motion model.
    #[test]
    fn moat_with_a_ladder_routes_out() {
        let (col, start, goal) = moat_scene(Some(79.0));
        let edges = col.climb_edges();
        assert_eq!(edges.len(), 1, "one ladder standing on validated rim floor → one edge, got {edges:?}");
        let d = edges[0].dismount;
        assert!(d[0] > 80.0, "the dismount must land on the RIM side of the wall, got {d:?}");
        assert!((d[2] - 24.0).abs() <= eqoxide_zone_geometry::climb::DISMOUNT_Z_TOL,
            "the dismount must be at rim height, got {d:?}");

        assert_eq!(col.climb_plans(), 0, "no route planned yet");
        let out = plan(&col, start, goal, 1.0);
        let route = match out {
            PlanOutcome::Route(p) => p,
            other => panic!("the ladder makes the rim reachable — expected a Route, got {other:?}"),
        };
        let last = *route.last().unwrap();
        assert!((last[0] - goal[0]).abs() <= 8.0 && (last[1] - goal[1]).abs() <= 8.0,
            "route must end at the rim goal, got {last:?}");
        assert!(route.iter().any(|wp| (wp[2] - 24.0).abs() <= 2.0 && wp[0] > 80.0),
            "route must actually top out on the rim: {route:?}");
        assert_eq!(col.climb_plans(), 1, "a route that climbs must be counted for nav_climb");
    }

    /// HONESTY GUARD, the mirror of `pad_with_void_destination_creates_no_edge`: a climb edge must
    /// never FABRICATE reachability. A ladder standing in open pit — nothing standable within
    /// `DISMOUNT_Z_TOL` of its top — resolves to NO edge, so the pit stays honestly `Unreachable`.
    /// Offering a link whose far end has not been shown standable is exactly the failure the
    /// both-ends-resolve gate exists to prevent.
    #[test]
    fn a_ladder_with_no_landing_at_its_top_yields_no_edge() {
        let (col, start, goal) = moat_scene(Some(40.0)); // mid-pit: its top is over open air
        assert!(!col.climb_volumes().is_empty(),
            "the object IS a climbable volume — the body may still mount it");
        assert!(col.climb_edges().is_empty(),
            "…but with no floor at its top it is NOT a routable edge, got {:?}", col.climb_edges());
        let out = plan(&col, start, goal, 1.0);
        assert!(matches!(out, PlanOutcome::Unreachable { .. }),
            "a ladder to nowhere must not make the rim reachable, got {out:?}");
    }

    /// `nav_climb` must disclose the routes that actually lean on the unverified climb motion, and
    /// only those: a plain walk across the pit floor of a laddered zone leaves the counter alone.
    #[test]
    fn climb_counter_ignores_routes_that_never_climb() {
        let (col, start, _goal) = moat_scene(Some(79.0));
        let out = plan(&col, start, [60.0, 60.0, 0.0], 1.0);
        assert!(matches!(out, PlanOutcome::Route(_)),
            "a walk across the pit floor must route, got {out:?}");
        assert_eq!(col.climb_plans(), 0, "a route that never touches the ladder must not be counted");
    }

    /// #257: the net-thread wall-clock budget (PLAN_BUDGET_MS) must be generous enough that a
    /// large but ordinary open-terrain plan still routes ALL the way to the goal — the cap only
    /// truncates pathological searches, never a legitimate long walk. A big empty floor is the
    /// cheap-per-node case, so a full corner-to-corner route completes far under the budget.
    #[test]
    fn find_path_large_open_plan_is_not_truncated_by_time_cap() {
        let big = MeshData {
            positions: vec![
                [-320.0, 0.0, -320.0], [320.0, 0.0, -320.0], [320.0, 0.0, 320.0], [-320.0, 0.0, 320.0],
            ],
            normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![big], objects: vec![], textures: vec![] }, 32.0);
        // World [east, north, up]: opposite corners of the plane, ~800u apart (~100 nav cells).
        // Assert the HONEST outcome (#356): a COMPLETE `Route`, never an `Exhausted` truncated
        // partial. The old `.expect()` on a bare `Option` could not tell "the route reached the far
        // corner" from "the search gave up and handed back a stub" — under load the deterministic
        // node cap will not fire here, but a bare Option would still hide an over-restrictive A*
        // regression that downgraded this full route to a partial.
        let path = match plan(&col, [-300.0, -300.0, 0.0], [300.0, 300.0, 0.0], 1.0) {
            PlanOutcome::Route(p) => p,
            other => panic!("a large open plane must route fully corner-to-corner as a complete \
                Route (not truncated by any cap), got {other:?}"),
        };
        let last = *path.last().unwrap();
        assert!((last[0] - 300.0).abs() < 8.0 && (last[1] - 300.0).abs() < 8.0,
            "route must reach the far corner (not a truncated partial), got {last:?}");
    }

    /// eqoxide#190: A* must route across a horizontal floor GAP a running jump can clear (a
    /// jump-edge), and must NOT invent a route across a gap wider than the jump reach.
    #[test]
    fn find_path_jumps_a_horizontal_gap() {
        let quad = |v: Vec<[f32; 3]>| MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // pos = [north, up, east]. Two z=0 platforms separated by a gap along east; north -40..40.
        let platform = |e0: f32, e1: f32| quad(vec![
            [-40.0, 0.0, e0], [40.0, 0.0, e0], [40.0, 0.0, e1], [-40.0, 0.0, e1]]);

        // find_path uses an 8u nav cell; reach ≈ 22.7u lands ≤ 2 cells (16u) out. Jumpable: an 8u
        // gap (east 8..16) — the far platform's cell sits 16u from the near edge. Only connection.
        let ok = ZoneAssets { terrain: vec![platform(-48.0, 8.0), platform(16.0, 64.0)], objects: vec![], textures: vec![] };
        let col = Collision::build(&ok, 4.0);
        let start = [-20.0, 0.0, 0.0]; // world [east, north, up], on platform A
        let goal  = [40.0, 0.0, 0.0];  // on platform B
        let path = col.find_path(start, goal, 1.0, &[], false)
            .expect("an 8u gap within jump reach must be routable via a jump-edge");
        let last = *path.last().unwrap();
        assert!((last[0] - goal[0]).abs() < 8.0, "path reaches platform B, got {last:?}");
        // The route must contain a jump segment: a hop bigger than any adjacent-cell step
        // (≤ 8·√2 ≈ 11.3u at the 8u nav cell) — the gap crossing is ~16u.
        let has_jump = path.windows(2).any(|w| {
            ((w[1][0] - w[0][0]).powi(2) + (w[1][1] - w[0][1]).powi(2)).sqrt() > 12.0
        });
        assert!(has_jump, "route should include a jump segment across the gap: {path:?}");

        // Too wide: a 32u gap exceeds the jump reach → no route (must not fabricate one).
        let wide = Collision::build(
            &ZoneAssets { terrain: vec![platform(-48.0, 8.0), platform(40.0, 88.0)], objects: vec![], textures: vec![] },
            4.0);
        assert!(matches!(plan(&wide, [-20.0, 0.0, 0.0], [60.0, 0.0, 0.0], 1.0), PlanOutcome::Unreachable { .. }),
            "a 32u gap exceeds jump reach — A* must refuse it (Unreachable), got {:?}",
            plan(&wide, [-20.0, 0.0, 0.0], [60.0, 0.0, 0.0], 1.0));
    }

    // ── nav z-anchor regression tests (#229, #329, #197p2) ───────────────────────────────────────
    // A quad in EQ WLD space (pos = [north, height, east]). `up` picks the winding, and therefore
    // the face normal: an up-facing FLOOR you can stand on, or a down-facing CEILING you cannot.
    fn slab(z: f32, n0: f32, n1: f32, e0: f32, e1: f32, up: bool) -> MeshData {
        MeshData {
            positions: vec![[n0, z, e0], [n0, z, e1], [n1, z, e1], [n1, z, e0]],
            normals: vec![], uvs: vec![],
            indices: if up { vec![0, 1, 2, 0, 2, 3] } else { vec![0, 2, 1, 0, 3, 2] },
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// A high floor east of 0 and a floor `drop` units lower west of it, meeting at e = 0.
    fn floor_with_drop(drop: f32) -> Collision {
        Collision::build(&ZoneAssets {
            terrain: vec![slab(0.0, -10.0, 10.0, -20.0, 0.0, true),
                          slab(-drop, -10.0, 10.0, 0.0, 20.0, true)],
            objects: vec![], textures: vec![],
        }, 32.0)
    }

    /// **The ascent envelope is `step_up` alone, and it is NARROWER than `walk_profile_ok`'s**
    /// (#727 round 5, non-blocking finding 3).
    ///
    /// The probe window opens `step_up` above `prev_z` and `allow` below it, so climbing is capped
    /// at a grade of `step_up / PROBE_SPACING = 1.0` while `MAX_WALK_GRADE = 1.2`. Grades in the
    /// band `(1.0, 1.2]` are walkable, `walk_profile_ok` accepts them, and `ground_continuous`
    /// refuses them. Both halves are asserted here, on the SAME hop, because the claim being pinned
    /// is a DISAGREEMENT between two predicates — asserting only the refusal would leave "…that
    /// `walk_profile_ok` accepts" as one more unmeasured sentence, which is the exact defect class
    /// this PR has spent five rounds on.
    ///
    /// The direction matters: this is a false NEGATIVE. A refused hop leaves the cursor where it
    /// was, so the cost is coverage of the #727 fix, never a resync onto ground the character
    /// cannot walk. The code is therefore deliberately unchanged — see `ground_continuous`'s doc.
    #[test]
    fn ground_continuous_ascent_is_capped_at_step_up_not_the_walk_grade() {
        // A 2 u run rising 2.2 u: grade 1.1, inside MAX_WALK_GRADE = 1.2, outside step_up = 2.0.
        const RISE: f32 = 2.2;
        // floor_with_drop(RISE) is low (z = -RISE) east of 0 and high (z = 0) west of it, so walking
        // WEST from the low side is the climb.
        let c = floor_with_drop(RISE);
        assert!(!c.ground_continuous([1.0, 0.0, -RISE], [-1.0, 0.0, 0.0]),
            "grade {} is inside MAX_WALK_GRADE yet ground_continuous refuses it — this refusal IS \
             the documented asymmetry; if it now accepts, the ascent window was widened and the \
             rustdoc's disclosure is stale", RISE / 2.0);
        assert!(c.walk_profile_ok([1.0, 0.0], -RISE, [-1.0, 0.0], 0.0, 60.0),
            "walk_profile_ok must accept the same hop — without this half, the claim that the two \
             predicates DISAGREE is unmeasured");
    }

    /// **Every `ground_continuous` test name cited in a doc comment still resolves** (#727 round 5).
    ///
    /// STAYS-side twin (docs/specs/2026-09-21-agent-harness-separation-plan-zone-geometry.md, Task
    /// 2 / #32): see the SHARED-side twin of this fn (same name) in
    /// `eqoxide-zone-geometry`'s `collision.rs` for the rest of the originally-cited names and the
    /// full rationale for the split.
    ///
    /// Add a name to this list whenever a doc comment in this module starts citing a STAYS-bucket
    /// test (one that references an A*-only symbol).
    #[test]
    fn every_ground_continuous_test_name_cited_in_a_doc_comment_still_exists() {
        let _cited: &[fn()] = &[
            ground_continuous_ascent_is_capped_at_step_up_not_the_walk_grade,
            floating_swimmer_is_anchored_to_the_water_surface_not_the_pool_bottom,
            worst_case_reachable_component,
            max_nodes_headroom_claim_stays_true,
            the_headroom_claim_window_is_closed_at_both_ends,
        ];
    }


    /// A whole zone can be the degenerate case of `column_whose_only_surface_is_inverted_still_finds_a_floor`
    /// above: EVERY column's only surface is inverted (a mesh whose winding convention is entirely
    /// backwards), not just one isolated patch. There is no whole-zone gate anymore to catch or
    /// report this — the per-column fallback in `column_hits` handles it uniformly, one column at a
    /// time, with no distinction between "one bad column" and "every column is bad". The zone must
    /// stay navigable either way.
    #[test]
    fn a_fully_inverted_zone_keeps_every_floor_via_the_per_column_fallback() {
        // Every face inverted (down-facing) — a mesh whose winding convention is backwards.
        let assets = ZoneAssets {
            terrain: vec![slab(0.0, 0.0, 96.0, 0.0, 96.0, false)],
            objects: vec![], textures: vec![],
        };
        let col = Collision::build(&assets, 8.0);
        // The filter would delete this single, down-facing surface — but that's the only ground in
        // every column here, so the fallback keeps it (fail-old), rather than deleting it and
        // leaving the zone unnavigable (fail-empty).
        assert!(col.nearest_floor(20.0, 40.0, 1.0, 20.0, 100.0).is_some(),
            "an inverted mesh must keep its floors via the per-column fallback");
        assert!(col.find_path([8.0, 8.0, 0.0], [56.0, 56.0, 0.0], 1.0, &[], false).is_some(),
            "and must still route");
    }

    /// #229: a `zone_cross` aims at a DRNTP region's interior point, whose z is a point in a VOLUME
    /// — on the shipped zones 1.5u to 127u ABOVE the real floor, and never a floor height. Such a
    /// goal must be projected DOWN onto the floor beneath it; snapping it to the "nearest surface
    /// within ±20" found nothing to grab (or a phantom), and the goal tier was then unreachable.
    #[test]
    fn goal_high_in_a_volume_projects_onto_the_floor_beneath_it() {
        let assets = ZoneAssets { terrain: vec![slab(0.0, 0.0, 64.0, 0.0, 64.0, true)], objects: vec![], textures: vec![] };
        let col = Collision::build(&assets, 8.0);
        // 47u up — the gfaydark→butcher region z. The old ±STEP_UP(20) window can't even see the floor.
        assert_eq!(col.nearest_floor(32.0, 32.0, 47.28, 20.0, 20.0), None,
            "the old ±20 snap window has no surface to grab at a volume point");
        let f = col.floor_beneath(32.0, 32.0, 47.28, 2.0, 400.0).expect("the floor is beneath it");
        assert!(f.abs() < 0.01, "the goal must project onto the floor at 0, got {f}");
        // And a route to that airborne goal now completes (A* accepts arrival on the real floor).
        assert!(col.find_path([8.0, 8.0, 0.0], [56.0, 56.0, 47.28], 1.0, &[], false).is_some(),
            "a goal 47u above the floor must still route to the floor under it");
    }

    /// #329 / #197p2: a character FLOATING in water is supported by the WATER SURFACE, not by a
    /// slab. Anchoring it to the nearest surface sent A* along the pool BOTTOM (halas: 128u down),
    /// so the walker dived to the planned floor and stranded. The route must stay at the surface.
    #[test]
    fn floating_swimmer_is_anchored_to_the_water_surface_not_the_pool_bottom() {
        let assets = ZoneAssets {
            terrain: vec![slab(-100.0, 0.0, 64.0, 0.0, 48.0, true),  // deep pool bottom
                          slab(0.0, 0.0, 64.0, 48.0, 96.0, true)],   // dry bank, at the waterline
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::flat_below(0.0))));

        // Floating 2u under the surface, 98u above the only solid floor.
        let start = [8.0, 32.0, -2.0];
        let goal  = [72.0, 32.0, 0.0]; // the bank
        let path = col.find_path(start, goal, 1.0, &[], false).expect("a swimmer must reach the bank");
        let deepest = path.iter().fold(f32::MAX, |m, w| m.min(w[2]));
        assert!(deepest > -10.0,
            "the route must cross AT THE SURFACE, not dive to the -100 pool bottom (deepest wp z = {deepest})");
        let last = path.last().unwrap();
        assert!((last[0] - goal[0]).abs() < 8.0 && (last[1] - goal[1]).abs() < 8.0, "and it must reach the bank");
    }

    /// A character WADING (feet on the bottom, water shallow) is still anchored to the ground — the
    /// water-surface anchor is only for a character with no footing under it.
    #[test]
    fn wading_character_is_still_anchored_to_the_ground() {
        let assets = ZoneAssets { terrain: vec![slab(0.0, 0.0, 64.0, 0.0, 64.0, true)], objects: vec![], textures: vec![] };
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::flat_below(3.0))));
        // Standing on the floor in 3u of water: there IS footing, so no surface anchor.
        let path = col.find_path([8.0, 32.0, 0.0], [56.0, 32.0, 0.0], 1.0, &[], false)
            .expect("a wader must still route across the shallows");
        assert!(path.iter().all(|w| w[2] < 1.0), "waypoints stay on the ground, not up on the waterline");
    }

    /// Water-nav §4d Increment 2 (#197): the GOAL-side mirror of
    /// `floating_swimmer_is_anchored_to_the_water_surface_not_the_pool_bottom`. A `/goto` INTO deep
    /// water used to resolve the goal via `floor_beneath`'s 400u dive to the pool BOTTOM, so "swim
    /// to X" planned a dive the controller's buoyancy won't hold — and `GOAL_TIER_TOL` then
    /// rejected every surface arrival. A goal floating in water (no footing beneath it) must
    /// anchor to the WATER SURFACE: the route's final waypoint is at the surface tier, never the
    /// bottom. Mutation check: drop `floating_goal_surface` from `astar`'s resolution chain and
    /// this goes RED (the route ends at the −100 bottom tier).
    #[test]
    fn floating_goal_is_anchored_to_the_water_surface_not_the_pool_bottom() {
        let assets = ZoneAssets {
            terrain: vec![slab(-100.0, 0.0, 64.0, 0.0, 48.0, true),  // deep pool bottom
                          slab(0.0, 0.0, 64.0, 48.0, 96.0, true)],   // dry bank, at the waterline
            objects: vec![], textures: vec![],
        };

        // Case 1: the goal z is IN the water, just under the surface (surface at 0).
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::flat_below(0.0))));
        let start = [8.0, 72.0, 0.0];  // on the bank
        let goal  = [40.0, 24.0, -2.0]; // floating mid-pool, 98u above the only solid floor
        let path = col.find_path(start, goal, 1.0, &[], false).expect("a route into the pool must exist");
        let last = *path.last().unwrap();
        assert!((last[0] - goal[0]).abs() < 8.0 && (last[1] - goal[1]).abs() < 8.0,
            "the route must reach the goal cell (last = {last:?})");
        assert!(last[2] > -10.0,
            "the goal must anchor AT THE SURFACE, never the -100 pool bottom (final wp z = {})", last[2]);
        let deepest = path.iter().fold(f32::MAX, |m, w| m.min(w[2]));
        assert!(deepest > -10.0, "and no waypoint may dive toward the bottom (deepest wp z = {deepest})");

        // Case 2: the goal z is ABOVE the water — the agent passed z=0 over a pool whose surface
        // sits at −8 (the short downward probe must still find and anchor to that surface).
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::flat_below(-8.0))));
        let goal = [40.0, 24.0, 0.0];
        let path = col.find_path(start, goal, 1.0, &[], false)
            .expect("a z-above-the-surface water goal must still route");
        let last = *path.last().unwrap();
        assert!((last[0] - goal[0]).abs() < 8.0 && (last[1] - goal[1]).abs() < 8.0,
            "the route must reach the goal cell (last = {last:?})");
        assert!(last[2] > -16.0 && last[2] < -1.0,
            "the goal must anchor at the -8 surface tier, not z=0 air or the -100 bottom (final wp z = {})", last[2]);
    }

    /// The #1 landmine the resolution ORDER defuses (design §4d step 3): with a floating goal
    /// mis-anchored to the pool bottom (`floor_beneath`'s 400u dive), `GOAL_TIER_TOL` rejects
    /// every surface arrival — and when the bottom tier is genuinely unreachable (deeper than the
    /// 200u water-descent probe, as lake floors are), A* can NEVER accept arrival: it floods the
    /// zone until a node cap bites and comes back `Exhausted` — the live halas/blackburrow
    /// `[WAT-ROUTE]` wedge shape. With the surface anchor the same bounded search accepts arrival
    /// on the tier the swim edges actually emit and returns a `Route` in a few hundred nodes.
    /// This is the assertion the wrong-tier `goal_fallback` cannot rescue (a fallback only helps
    /// once the frontier CLOSES, which a real zone's node budget never allows).
    #[test]
    fn floating_goal_over_an_unreachable_bottom_routes_instead_of_flooding() {
        let assets = ZoneAssets {
            terrain: vec![slab(-250.0, 0.0, 512.0, 0.0, 448.0, true),  // lake floor, 250u down —
                                                                       // beyond the descent probe
                          slab(0.0, 0.0, 512.0, 448.0, 512.0, true)],  // dry bank at the waterline
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::flat_below(0.0))));
        let start = [256.0, 480.0, 0.0];  // on the bank
        let goal  = [256.0, 224.0, -2.0]; // floating mid-lake, 248u above the unreachable floor
        // Ask the raw search so the expansion count is observable. With the surface anchor, A*
        // accepts arrival the moment the goal cell pops at the swim tier: a few hundred nodes.
        // Mis-anchored to the -250 bottom it can never accept, closes the whole ~4100-node zone,
        // and only then a wrong-tier fallback dresses the flood up as a route — the node count is
        // the honest, deterministic tell (no wall clock: same fixture, same count, every machine).
        let s = astar(&col, start, goal, 1.0, &[], 8.0, None, 0.0, PlanCtx::default(), true);
        let (p, complete) = s.path.expect("a route to the floating goal must exist");
        assert!(complete, "and it must be a COMPLETE route, not a partial");
        let last = *p.last().unwrap();
        assert!(last[2] > -10.0, "the route ends at the surface tier (last wp z = {})", last[2]);
        assert!(s.closed_n < 1500,
            "arrival must be ACCEPTED at the surface tier, not rejected until the zone floods \
             (expanded {} nodes; a mis-anchored goal closes ~4100)", s.closed_n);
    }

    /// The NEGATIVE CONTROL for the floating goal anchor (design §4d): a goal EXPLICITLY on a
    /// submerged floor — the asked z within tier tolerance of the pool bottom — still resolves to
    /// THAT floor (the ask is honoured; a caller naming the bottom means the bottom). But the
    /// walker cannot dive and hold it (buoyancy rises; arrival is 2D), so the accommodation is
    /// REPORTED via the `goal_z_snapped` channel with the water qualifier — never silently.
    #[test]
    fn submerged_floor_goal_resolves_to_that_floor_and_reports_the_surface_accommodation() {
        let assets = ZoneAssets {
            terrain: vec![slab(-20.0, 0.0, 64.0, 0.0, 48.0, true),   // pool bottom, 20u under water
                          slab(0.0, 0.0, 64.0, 48.0, 96.0, true)],   // dry bank at the waterline
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::flat_below(0.0))));
        let goal = [40.0, 24.0, -20.0]; // the caller NAMES the pool bottom
        // The tier logic honours the asked floor: the plan aims at the bottom tier...
        let path = col.find_path([8.0, 72.0, 0.0], goal, 1.0, &[], false)
            .expect("an explicitly submerged floor goal must still route");
        let last = *path.last().unwrap();
        assert!((last[2] + 20.0).abs() <= 8.0,
            "the asked-for submerged floor is honoured (final wp z = {}, wanted ~-20)", last[2]);
        // ...and the accommodation is reported: the walker will FLOAT at the surface above it.
        match col.goal_z_was_snapped(goal) {
            Some(GoalSnap::ToWaterSurface { surface_z }) =>
                assert!(surface_z.abs() < 1.0, "the reported surface must be the waterline, got {surface_z}"),
            other => panic!("a submerged-floor goal must report the surface accommodation, got {other:?}"),
        }
        // Control-of-the-control: the same asked z on a DRY floor reports nothing.
        let dry = Collision::build(&assets, 8.0); // no water map at all
        assert_eq!(dry.goal_z_was_snapped(goal), None,
            "with no water the same goal is an ordinary honoured tier — no report");
    }

    /// #344 REVIEW: the arrival anchor must be the walker's PHYSICAL rest point, not `astar`'s
    /// routing target. A goal on a submerged floor deeper than GOAL_TIER_TOL below the surface is
    /// one the walker floats ABOVE (buoyancy rises; it cannot dive and hold depth). `astar` still
    /// routes toward the submerged tier, but the walker comes to rest at `surface − float_depth`.
    /// So `resolve_goal_floor` must yield the SURFACE, and arrival there must read `Arrived` — the
    /// §4d "arrived at the surface, not the asked depth" promise. Anchoring to the submerged floor
    /// instead makes a deep pool an eternal `Drive`, then a FALSE `blocked` claiming the goal "IS
    /// reachable" — the exact honesty regression this test locks out. Mutation check: return the
    /// submerged tier `f` from `resolve_goal_floor` (drop the `s − f > GOAL_TIER_TOL` clause) and
    /// this goes RED (gdz ≈ 38 → Drive).
    #[test]
    fn deep_water_goal_arrives_at_the_surface_not_the_submerged_floor() {
        use crate::steering::{arrival_action, ArrivalAction};
        let assets = ZoneAssets {
            terrain: vec![slab(-40.0, 0.0, 64.0, 0.0, 48.0, true),   // pool bottom, 40u under water
                          slab(0.0, 0.0, 64.0, 48.0, 96.0, true)],   // dry bank at the waterline
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::flat_below(0.0))));
        let goal = [40.0, 24.0, -40.0]; // the caller NAMES the pool bottom, 40u down

        // The anchor is the SURFACE (~0), never the -40 bottom.
        let anchor = col.resolve_goal_floor(goal).expect("a submerged goal still resolves a floor");
        assert!(anchor.abs() < 1.0,
            "arrival must anchor to the waterline surface, not the -40 pool bottom (got {anchor})");

        // The walker floats at surface − float_depth (2.0). With the surface anchor it has ARRIVED.
        let walker_z = 0.0 - crate::traversability::PLAYER_BODY.float_depth;
        let gdz = anchor - walker_z;
        assert!(gdz.abs() <= crate::collision::GOAL_TIER_TOL,
            "floating at the surface is within the arrival tolerance of the surface anchor (gdz={gdz})");
        assert_eq!(arrival_action(0.5, gdz, false), ArrivalAction::Arrived,
            "a swimmer at the surface of a deep pool must ARRIVE, not churn to a false 'blocked'");

        // And `goal_z_was_snapped` reports the very same surface — the two agree by construction.
        match col.goal_z_was_snapped(goal) {
            Some(GoalSnap::ToWaterSurface { surface_z }) =>
                assert!((surface_z - anchor).abs() < 1e-3,
                    "resolve_goal_floor and goal_z_was_snapped must name the same surface"),
            other => panic!("a deep submerged goal must report ToWaterSurface, got {other:?}"),
        }
    }

    /// `surface_z` returns `None` for an UNBOUNDED water column (still water 200u+ up). The goal
    /// anchor must TOLERATE that — fall through to the ordinary dry resolution (`floor_beneath`)
    /// — never unwrap, never refuse the goal.
    #[test]
    fn floating_goal_in_an_unbounded_water_column_falls_through_to_the_dry_path() {
        let assets = ZoneAssets {
            terrain: vec![slab(-300.0, 0.0, 64.0, 0.0, 64.0, true)], // abyss floor, 300u down
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::flat_below(0.0))));
        // Goal 280u deep: in water, no footing within FOOTING — but the surface is 280u up, past
        // `surface_z`'s 200u bound → None. The anchor must yield and let `floor_beneath` project
        // the goal onto the abyss floor (the pre-existing volume-point behaviour).
        let goal = [40.0, 32.0, -280.0];
        assert_eq!(floating_goal_surface(&col, goal), None,
            "an unbounded column has no surface to anchor to");
        let path = col.find_path([8.0, 32.0, -300.0], goal, 1.0, &[], false)
            .expect("the dry fallback must still resolve and route this goal");
        // The route travels along the abyss floor tier (`floor_beneath`'s projection). The final
        // waypoint keeps the caller's raw z, as every dry volume-point goal does (#229).
        for w in &path[..path.len() - 1] {
            assert!((w[2] + 300.0).abs() <= 8.0,
                "the route runs on the abyss floor at -300, not an imaginary surface (wp = {w:?})");
        }
        assert_eq!(col.goal_z_was_snapped(goal), None,
            "a floor-beneath projection is the tier the caller meant — not a reported snap");
    }

    /// The FINE local tier shares the goal resolution: a carrot over deep water must THREAD to the
    /// surface tier — not dive to the bottom, not report `NoWayThrough`. Without the floating goal
    /// anchor the carrot resolved to the −100 bottom and the fine plan's last waypoint dove with it.
    #[test]
    fn fine_tier_carrot_over_deep_water_threads_at_the_surface() {
        let assets = ZoneAssets {
            terrain: vec![slab(-100.0, 0.0, 64.0, 0.0, 48.0, true),  // deep pool bottom
                          slab(0.0, 0.0, 64.0, 48.0, 96.0, true)],   // dry bank, at the waterline
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::flat_below(0.0))));
        // From the bank, a carrot 28u out over the deep water at the coarse route's z.
        match col.find_path_local([56.0, 52.0, 0.0], [56.0, 24.0, 0.0], 2.0, 60.0, 4.0) {
            LocalOutcome::Threaded(p) => {
                let last = *p.last().unwrap();
                assert!(last[2] > -10.0,
                    "the fine tier must thread AT THE SURFACE, not dive (last wp z = {})", last[2]);
            }
            other => panic!("a carrot over deep water must be Threaded, got {other:?}"),
        }
    }

    /// #229's last mile: A* expands each node from its CELL CENTRE, but the walker drives from where
    /// the character actually STANDS. Anchoring the start at the cell centre let A* emit a route
    /// whose every cell-centre segment was clear while the character→waypoint[0] leg ran straight
    /// through a wall — the walker pressed into it, made no progress, re-planned the identical route
    /// and stalled out. (Live everfrost→blackburrow: exactly 1 of 99 segments blocked, the first.)
    /// EVERY segment of a returned route, INCLUDING the first one from the character's real
    /// position, must be clearance-clear.
    #[test]
    fn route_first_leg_is_walkable_from_the_characters_real_position() {
        // A wall running north–south at east=44 (north 0..52), with the way around it to the north.
        let wall = MeshData {
            positions: vec![[0.0, 0.0, 44.0], [52.0, 0.0, 44.0], [52.0, 12.0, 44.0], [0.0, 12.0, 44.0]],
            normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let assets = ZoneAssets {
            terrain: vec![slab(0.0, 0.0, 96.0, 0.0, 96.0, true), wall],
            objects: vec![], textures: vec![],
        };
        let col = Collision::build(&assets, 8.0);

        // The character stands just WEST of the wall, off-centre in its nav cell; the goal is EAST of
        // the wall and south, so any route must first go north around the wall's end.
        let start = [40.0, 50.0, 0.0];
        let goal  = [60.0, 10.0, 0.0];
        let path = col.find_path(start, goal, 1.0, &[], false).expect("a route around the wall exists");

        let mut prev = start;
        for (i, w) in path.iter().enumerate() {
            assert!(col.path_clear([prev[0], prev[1], prev[2] + 3.0], [w[0], w[1], w[2] + 3.0], 1.0),
                "segment {i} ({prev:?} -> {w:?}) crosses the wall — the walker cannot follow it");
            prev = *w;
        }
    }

    /// #340/#394: the search must honour the CALLER's NODE CAP instead of re-arming a fresh one per
    /// call. `plan_path` makes up to 13 A* calls per plan; a per-call budget let one plan cost 13× its
    /// intended bound. `PlanCtx::default()` must arm NO tight cap (only the global `MAX_NODES`
    /// backstop): a bare call runs to completion.
    #[test]
    fn find_path_honours_a_caller_supplied_node_cap() {
        // A long open corridor — the goal is far enough that A* must expand well over a handful of
        // nodes, so a tiny node cap must actually cut the search short before it can reach the goal.
        let assets = ZoneAssets { terrain: vec![slab(0.0, 0.0, 64.0, 0.0, 1600.0, true)], objects: vec![], textures: vec![] };
        let col = Collision::build(&assets, 32.0);
        let (start, goal) = ([8.0, 32.0, 0.0], [1560.0, 32.0, 0.0]);

        let fresh = col.find_path_res(start, goal, 1.0, &[], false, 8.0, None, 0.0, PlanCtx::worker());
        assert!(fresh.is_some(), "with the worker's (generous) cap the route is found");
        assert!(col.find_path_res(start, goal, 1.0, &[], false, 8.0, None, 0.0, PlanCtx::default()).is_some(),
            "and with the default (MAX_NODES backstop) it is found too — a bare search runs to completion");

        // A cap of 4 nodes cannot reach a goal ~190 cells away. Note the outcome is DETERMINISTIC:
        // this asserts identically on a fast box and a slow one, which the old wall-clock version
        // could not (that was the #394 bug).
        let tiny = PlanCtx { node_cap: Some(4), ..PlanCtx::default() };
        let out = col.find_path_res(start, goal, 1.0, &[], false, 8.0, None, 0.0, tiny);
        assert!(out.is_none(), "a 4-node cap must abort the search before it reaches a far goal");
    }

    /// **#337/#356/#394 — the honesty invariant, made DETERMINISTIC.** A search that hit its NODE CAP
    /// must report `Exhausted` ("I don't know"), and a search that CLOSED its frontier without finding
    /// the goal must report `Unreachable` ("no"). Collapsing those two is what made the walker drive a
    /// stub into a wall and freeze at `blocked` for months.
    ///
    /// Same geometry, same goal, same code path: only the node cap differs. The answers must differ
    /// too — and the cut-short search must NEVER be the one that says "no route". Unlike the wall-clock
    /// version this replaced (#394), the answer here does not depend on machine speed: a 4-node cap is
    /// hit after exactly 4 expansions on every machine.
    #[test]
    fn a_node_cap_is_never_reported_as_no_route() {
        let assets = ZoneAssets { terrain: vec![slab(0.0, 0.0, 64.0, 0.0, 1600.0, true)], objects: vec![], textures: vec![] };
        let col = Collision::build(&assets, 32.0);
        let start = [8.0, 32.0, 0.0];

        // (a) Reachable goal, generous cap → a complete Route.
        let reachable = [1560.0, 32.0, 0.0];
        assert!(matches!(col.find_path_ex(start, reachable, 1.0, &[], 8.0, None, 0.0, PlanCtx::default()),
            PlanOutcome::Route(_)), "a reachable goal with a generous cap must produce a complete Route");

        // (b) The SAME reachable goal, with a cap too small to reach it → Exhausted(NodeCap), NEVER
        //     Unreachable. The search stopped LOOKING; it did not prove there is no route.
        let tiny = PlanCtx { node_cap: Some(4), ..PlanCtx::default() };
        match col.find_path_ex(start, reachable, 1.0, &[], 8.0, None, 0.0, tiny) {
            PlanOutcome::Exhausted { limit: PlanLimit::NodeCap, .. } => {}
            other => panic!("a search cut short by its node cap must report Exhausted(NodeCap) — reporting \
                             {other:?} for a goal that IS reachable is the #337 lie"),
        }

        // (c) A goal OFF the mesh entirely, generous cap → a definitive Unreachable, and no waypoints.
        let off_mesh = [1560.0, 3000.0, 0.0]; // far outside the slab: no walkable floor at all
        let out = col.find_path_ex(start, off_mesh, 1.0, &[], 8.0, None, 0.0, PlanCtx::default());
        match &out {
            PlanOutcome::Unreachable { reason: NoRoute::GoalNotWalkable, .. } => {}
            other => panic!("a goal with no walkable floor must fail IMMEDIATELY as Unreachable(GoalNotWalkable), got {other:?}"),
        }
        assert!(out.route().is_none(), "an unreachable goal must hand back NO route");
    }

    /// **A SLOPPY GOAL Z IS NOT AN UNREACHABLE GOAL.** Agents routinely pass a rough z (0, or a map
    /// coordinate) for a goal whose real floor sits well above it. Rejecting those as
    /// `goal_not_walkable` is a FALSE definitive no — the XY is perfectly walkable, and `main`
    /// routed to it fine. Caught live in North Qeynos: `goto (-40,250,z=0)` refused to move at all.
    ///
    /// The honest line is "is there ANY floor at this XY?": a bad z snaps to the real floor; a goal
    /// off the mesh entirely still fails hard (asserted above).
    #[test]
    fn a_goal_with_a_sloppy_z_still_routes_to_its_real_floor() {
        // Floor at z = 40. The caller asks for z = 0 — 40u BELOW it, far outside any tier tolerance,
        // and `floor_beneath` only ever looks DOWN, so the old code resolved nothing and hard-failed.
        let col = Collision::build(
            &ZoneAssets { terrain: vec![slab(40.0, 0.0, 200.0, 0.0, 200.0, true)], objects: vec![], textures: vec![] },
            32.0);
        let out = col.find_path_ex([16.0, 16.0, 40.0], [180.0, 180.0, 0.0], 1.0, &[], 8.0, None, 0.0, PlanCtx::default());
        let route = match &out {
            PlanOutcome::Route(p) => p,
            other => panic!("a walkable XY with a sloppy z must still ROUTE (snapping to its real \
                             floor), not be dismissed as unreachable — got {other:?}"),
        };
        let last = *route.last().unwrap();
        assert!((last[0] - 180.0).abs() < 8.0 && (last[1] - 180.0).abs() < 8.0,
            "and the route must reach the goal XY, got {last:?}");
    }

    /// **A BOXED-IN START MUST NEVER BE REPORTED AS "NO ROUTE TO THE GOAL".**
    ///
    /// The two failures look identical from outside — the frontier closes, the goal isn't in it —
    /// and conflating them produces a *false definitive no*, which is worse than the silent wedge
    /// this PR set out to kill: the agent is told, with confidence, something untrue.
    ///
    /// Caught LIVE, not by a test: in gfaydark the walker wedged on terrain, A* closed after ONE
    /// node, and the cell-centre retry dribbled out a 2-cell partial — which an earlier version of
    /// `search()` mistook for evidence that the zone had really been surveyed. It reported
    /// `no_path: search_closed` for a goal that was perfectly reachable from 16u away.
    #[test]
    fn a_boxed_in_start_is_start_isolated_not_no_route() {
        // A big open plane the goal sits on, plus a tiny sealed box around the START only.
        let wall = |n0: f32, e0: f32, n1: f32, e1: f32| MeshData {
            positions: vec![[n0, 0.0, e0], [n1, 0.0, e1], [n1, 40.0, e1], [n0, 40.0, e0]],
            normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // The pocket is a FEW cells across, not one — which is what makes this bite. The character
        // can shuffle a couple of cells inside it, so the search dribbles out a small partial route
        // "toward" the goal. That stub is exactly what fooled the earlier version into believing the
        // search had surveyed the zone. (A single-cell box produces no partial at all and would let
        // the bug through.)
        let (n0, n1, e0, e1) = (88.0f32, 120.0f32, 88.0f32, 120.0f32); // ~4 nav cells across
        let terrain = vec![
            slab(0.0, 0.0, 400.0, 0.0, 400.0, true),
            wall(n0, e0, n0, e1),
            wall(n1, e0, n1, e1),
            wall(n0, e0, n1, e0),
            wall(n0, e1, n1, e1),
        ];
        let col = Collision::build(&ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0);

        // Start in the pocket's FAR corner, so shuffling across it gains real ground on the goal —
        // the search will produce a partial. The goal is wide open and obviously walkable: it is the
        // START that is sealed in.
        let out = col.find_path_ex([92.0, 92.0, 0.0], [350.0, 350.0, 0.0], 1.0, &[], 8.0, None, 0.0, PlanCtx::default());
        match &out {
            PlanOutcome::Unreachable { reason: NoRoute::StartIsolated, .. } => {}
            PlanOutcome::Unreachable { reason: NoRoute::SearchClosed, .. } => panic!(
                "a boxed-in START reported as `search_closed` — that is a FALSE definitive 'no route to \
                 the goal' for a goal that is perfectly reachable. The character is stuck, not the goal."),
            other => panic!("expected Unreachable(StartIsolated), got {other:?}"),
        }
        assert!(out.route().is_none(), "and no stub route out of the sealed cell — the walker must not drive it");
    }

    /// **The FINE LOCAL STEERING tier must keep its partial route even from a boxed-in start.**
    ///
    /// This is the test that was missing for live-bug #2 (the halas swimmer). `find_path_res` is the
    /// fine 2u tier the walker steers on; its searches are bounded to 40u, so their explored
    /// component is *always* small and they look "boxed in" by construction — a floating swimmer at
    /// a shoreline especially so. An earlier `search()` wiped the partial on that path, the swimmer
    /// lost its steering hint, stopped swimming, and wedged at the water's edge for 8 attempts while
    /// the coarse planner cheerfully re-issued a perfect 78-waypoint route across the water.
    ///
    /// The honest planner API (`find_path_ex`) must still say `start_isolated` and hand back NO
    /// route — an "Unreachable" carries no waypoints. Both halves are asserted here: they are
    /// different questions, and this is exactly where they diverge.
    #[test]
    fn a_boxed_in_start_still_yields_a_partial_for_local_steering() {
        let wall = |n0: f32, e0: f32, n1: f32, e1: f32| MeshData {
            positions: vec![[n0, 0.0, e0], [n1, 0.0, e1], [n1, 40.0, e1], [n0, 40.0, e0]],
            normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let (n0, n1, e0, e1) = (88.0f32, 120.0f32, 88.0f32, 120.0f32);
        let terrain = vec![
            slab(0.0, 0.0, 400.0, 0.0, 400.0, true),
            wall(n0, e0, n0, e1), wall(n1, e0, n1, e1),
            wall(n0, e0, n1, e0), wall(n0, e1, n1, e1),
        ];
        let col = Collision::build(&ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0);
        let (start, goal) = ([92.0, 92.0, 0.0], [350.0, 350.0, 0.0]);

        // The steering tier: it asked for a best-effort route and it must GET one. Starving it here
        // is what stopped the swimmer.
        let steer = col.find_path_res(start, goal, 1.0, &[], true, 8.0, None, 0.0, PlanCtx::default());
        assert!(steer.is_some(),
            "the FINE LOCAL STEERING tier (allow_partial) must still get a partial route from a boxed-in \
             start — wiping it is what stopped the halas swimmer dead at the water's edge");
        assert!(steer.unwrap().len() >= 2, "and it must be something the walker can actually steer along");

        // The honest planner API, on the very same search, must still refuse to call that a route.
        let out = col.find_path_ex(start, goal, 1.0, &[], 8.0, None, 0.0, PlanCtx::default());
        assert!(matches!(out, PlanOutcome::Unreachable { reason: NoRoute::StartIsolated, .. }),
            "the honest API must still report start_isolated, got {out:?}");
        assert!(out.route().is_none(), "an Unreachable must carry no waypoints");
    }

    /// A partial route may only be walked when it makes GENUINE progress toward the goal. The old
    /// bar was one nav cell (8u) — enough for a wedged character to shuffle a single cell into a
    /// wall and call it a plan (#337). Under a frontier-CLOSED search there is no partial at all.
    #[test]
    fn a_closed_search_yields_no_partial_route() {
        // A slab with a sealed pocket at the far corner: the goal has a perfectly good floor (so it
        // is NOT dismissed up front), but two walls seal it off — so the search has to close the
        // whole slab to learn there is no way in.
        // A vertical wall (n0,e0)->(n1,e1), 30u tall.
        let wall = |n0: f32, e0: f32, n1: f32, e1: f32| MeshData {
            positions: vec![[n0, 0.0, e0], [n1, 0.0, e1], [n1, 30.0, e1], [n0, 30.0, e0]],
            normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let terrain = vec![
            slab(0.0, 0.0, 200.0, 0.0, 200.0, true),
            wall(160.0, 160.0, 160.0, 200.0), // along east, at north=160
            wall(160.0, 160.0, 200.0, 160.0), // along north, at east=160
        ];
        let col = Collision::build(&ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0);

        let out = col.find_path_ex([16.0, 16.0, 0.0], [180.0, 180.0, 0.0], 1.0, &[], 8.0, None, 0.0, PlanCtx::default());
        assert!(matches!(out, PlanOutcome::Unreachable { .. }),
            "a sealed goal, searched to completion, is UNREACHABLE — got {out:?}");
        assert!(out.route().is_none(), "and it must hand back no waypoints at all (the #337 lie is a partial here)");
        // The old code walked this: `find_path(.., allow_partial=true)` would have returned a greedy
        // stub toward the pocket. It is still available for LOCAL STEERING, which is the only place
        // a partial belongs — but the honest planner API above refuses to call it a route.
    }

    /// ONE PLAN, ONE BUDGET (#302/#394). The generous clearance pass must take a SLICE of the caller's
    /// NODE budget, never arm a fresh one — two passes each arming the full cap is a plan that quietly
    /// costs two budgets, exactly the disease `PlanCtx` exists to prevent. A node cap, unlike the
    /// wall-clock deadline this replaced, has no "now" to measure from — the split is a plain fraction.
    #[test]
    fn the_generous_pass_takes_a_slice_of_the_budget_never_a_fresh_one() {
        let caller = 1000usize;
        let g = generous_node_cap(Some(caller)).expect("a budgeted plan keeps a budget");
        assert!(g < caller,
            "the generous pass may never get the FULL cap — that is two budgets for one plan (#302)");
        assert_eq!(g, (caller as f32 * GENEROUS_BUDGET_SHARE) as usize,
            "the generous pass gets exactly GENEROUS_BUDGET_SHARE of the caller's node budget");

        // An UNBUDGETED plan (only the MAX_NODES backstop) stays unbudgeted — never invent a cap.
        assert_eq!(generous_node_cap(None), None);
    }

    /// Tiering is a route-CHOICE mechanism, so only a plan that chooses a route pays for it. The
    /// BOUNDED local tier (`max_search: Some`) follows a carrot on a coarse route that was already
    /// chosen with room, inside a 40u window with no meaningful alternative — so it plans at the
    /// MINIMUM clearance and spends its budget on the question it exists to answer: does the
    /// character FIT. Measured on the production call (2u cell / 40u bound / 150 ms, ON the net
    /// thread): the second pass adds ~30-60% mean on top of the sweep and DOUBLES the plans that
    /// overrun the budget (blackburrow 17 -> 30 of 240) while buying nothing (#382).
    #[test]
    fn the_bounded_local_tier_plans_at_the_minimum_clearance_not_the_generous_one() {
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        let col = slotted_wall(2.0 * r + 0.5); // fits the character, not the preferred margin
        let (start, goal) = ([5.0, 9.0, 0.0], [15.0, 9.0, 0.0]);

        // UNBOUNDED = route-choosing (the coarse planner, off-thread): the generous tier is tried,
        // finds nothing, and the minimum-clearance fallback answers — and REPORTS itself as tight.
        let (s, tight) = search_tiered(&col, start, goal, r, &[], 2.0, None, 0.0, PlanCtx::default());
        assert!(matches!(s.path, Some((_, true))), "the route exists at the minimum clearance");
        assert!(tight, "a route that only exists at the minimum must report as tight");

        // BOUNDED = local steering (the net-thread tier): straight to the minimum. No second search,
        // and nothing to call "tight" — no roomier route was ever asked for, so none was denied.
        let (s, tight) = search_tiered(&col, start, goal, r, &[], 2.0, Some(60.0), 0.0, PlanCtx::default());
        assert!(matches!(s.path, Some((_, true))),
            "the local tier must still find the route the character fits through");
        assert!(!tight, "a bounded local plan never asks for the generous tier, so it cannot be tight");
    }

    /// A COMPLETE tiered route, or `None` — the `allow_partial = false` question these tests ask.
    /// Returns `(waypoints, tight)`; `tight` = the route only exists at the MINIMUM clearance.
    fn tiered_route(col: &Collision, start: [f32; 3], goal: [f32; 3], radius: f32, cell: f32)
        -> Option<(Vec<[f32; 3]>, bool)> {
        let (s, tight) = search_tiered(col, start, goal, radius, &[], cell, None, 0.0, PlanCtx::default());
        match s.path { Some((p, true)) => Some((p, tight)), _ => None }
    }

    fn slotted_wall(gap: f32) -> Collision {
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [20.0, 0.0, 20.0], [0.0, 0.0, 20.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // GLB axes -> world: east = p[2], north = p[0], height = p[1].
        let (lo, hi) = (9.0 - gap / 2.0, 9.0 + gap / 2.0);
        let panel = |n0: f32, n1: f32| MeshData {
            positions: vec![[n0, 0.0, 10.0], [n1, 0.0, 10.0], [n1, 10.0, 10.0], [n0, 10.0, 10.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        Collision::build(&ZoneAssets {
            terrain: vec![floor, panel(0.0, lo), panel(hi, 20.0)], objects: vec![], textures: vec![],
        }, 2.0)
    }

    /// **#685 (corner-cut) — the LOS-clamped carrot rounds a corner against the REAL `path_clear`.**
    ///
    /// The steering module pins the clamp mechanism with an analytic LOS closure
    /// (`steering::los_clamp_rounds_a_convex_corner_instead_of_cutting_the_chord`); this pins it with
    /// the ACTUAL production predicate, `Collision::path_clear` — the same volume-sweep the walker
    /// clamps against live. An L-path bends around a wall panel that juts into the inside of the turn
    /// (a convex corner). The plain carrot's straight aim is the chord across the corner and
    /// `path_clear` REJECTS it (it crosses the panel); the LOS-clamped carrot stops at the corner and
    /// `path_clear` ACCEPTS its aim.
    ///
    /// MUTATION-DISCRIMINATING: make `carrot_along_los` ignore its `los` arg and the "clamped aim is
    /// path_clear" assertion goes RED — the clamped carrot collapses back to the corner-cutting chord.
    #[test]
    fn los_clamp_rounds_a_baked_l_corner() {
        use crate::steering::{carrot_along, carrot_along_los};
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        // Floor east[-5,20] north[-5,15] at up=0. (GLB space is [north, up, east].)
        let floor = MeshData {
            positions: vec![[-5.0, 0.0, -5.0], [15.0, 0.0, -5.0], [15.0, 0.0, 20.0], [-5.0, 0.0, 20.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // A wall panel at east=9, north∈[2,6], height 0..8 — the convex obstacle jutting into the
        // inside of the L-turn. Its near edge (north=2) is kept a full radius clear of leg1 (north=0)
        // so the STRAIGHT approach to the corner is unobstructed; it blocks ONLY the corner-cutting
        // chord (which passes ~north 3.3 at east 9). It touches neither path leg.
        let wall = MeshData {
            positions: vec![[2.0, 0.0, 9.0], [6.0, 0.0, 9.0], [6.0, 8.0, 9.0], [2.0, 8.0, 9.0]],
            normals: vec![[-1.0, 0.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor, wall], objects: vec![], textures: vec![] }, 2.0);

        // L-path: east to the corner (10,0), then north. Feet at z=1 (just above the floor).
        let path: Vec<[f32; 3]> = vec![[0.0, 0.0, 1.0], [10.0, 0.0, 1.0], [10.0, 10.0, 1.0]];
        let from = [4.0, 0.0, 1.0];
        let reach = 10.0;
        let los = |a: [f32; 3], b: [f32; 3]| col.carrot_los_clear(a, b, r);

        // The UNCLAMPED carrot cuts the corner: its straight aim crosses the wall (scene reproduces #685).
        let plain = carrot_along(&path, 0, from, reach).unwrap();
        assert!(!col.carrot_los_clear(from, plain, r),
            "sanity: the unclamped carrot {plain:?} must chord across the corner — carrot_los_clear \
             should reject the straight aim, else this scene does not reproduce #685");

        // The CLAMPED carrot rounds the corner: carrot_los_clear ACCEPTS its straight aim.
        let clamped = carrot_along_los(&path, 0, from, reach, los).unwrap();
        assert!(col.carrot_los_clear(from, clamped, r),
            "the LOS-clamped carrot {clamped:?} must be reachable by the real carrot_los_clear ray — the \
             walker rounds the corner instead of chording into the wall. MUTATION: ignore `los` in \
             carrot_along_los and this goes RED.");
        // Anti-crawl: still leads forward toward the corner, does not retreat behind the walker.
        assert!((clamped[0] - from[0]).hypot(clamped[1] - from[1]) >= 4.0,
            "the clamped carrot {clamped:?} must still lead the walker forward toward the corner, not crawl in place");
    }

    /// **#685 (owner-directed corner-buffer offset) — inflate a wall-grazing route waypoint OFF the
    /// wall by the buffer, with room on the far side.** A route runs 1u (= PLAYER_RADIUS) from a wall;
    /// `inflate_route_off_corners` must push the interior waypoint out to `radius + buffer` clearance
    /// (open space beyond), while leaving the fixed endpoints put and keeping the route walkable. This
    /// is the PRIMARY fix — the walker takes one smooth wider arc instead of hugging the apex.
    ///
    /// MUTATION-DISCRIMINATING: make `inflate_route_off_corners` a no-op (or delete the offset) and the
    /// "waypoint moved to ~radius+buffer" assertion goes RED — the waypoint stays grazing the wall.
    #[test]
    fn inflate_route_pushes_a_wall_grazing_waypoint_off_the_wall() {
        let r = eqoxide_core::physics::PLAYER_RADIUS; // 1.0
        let buffer = 2.0;
        // Floor east[-20,20] north[-20,20]; a wall along east=10 (open space to the west).
        let floor = MeshData {
            positions: vec![[-20.0, 0.0, -20.0], [20.0, 0.0, -20.0], [20.0, 0.0, 20.0], [-20.0, 0.0, 20.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let wall = MeshData {
            positions: vec![[-20.0, 0.0, 10.0], [20.0, 0.0, 10.0], [20.0, 8.0, 10.0], [-20.0, 8.0, 10.0]],
            normals: vec![[-1.0, 0.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor, wall], objects: vec![], textures: vec![] }, 2.0);

        // Interior waypoint grazes the wall at east=9 (1u = radius from the wall at east=10); the fixed
        // endpoints sit 2u off the wall (east=8) so the reconnect walkability check below is meaningful.
        let mut route = vec![[8.0f32, -8.0, 1.0], [9.0, 0.0, 1.0], [8.0, 8.0, 1.0]];
        // Pre: the middle waypoint grazes the wall (1u away).
        assert!((10.0 - route[1][0] - r).abs() < 0.3, "sanity: the middle waypoint starts ~radius from the wall");

        col.inflate_route_off_corners(&mut route, r, buffer);

        // Post: the middle waypoint pushed WEST to ~radius+buffer (3u) of clearance; endpoints unchanged.
        let clearance = 10.0 - route[1][0];
        assert!(clearance >= r + buffer - 0.4,
            "inflation must push the wall-grazing waypoint out to ~radius+buffer clearance (got {clearance:.2}u, \
             waypoint x={:.2}). MUTATION: no-op inflate_route_off_corners and this goes RED (x stays 9).", route[1][0]);
        assert!(route[1][0] < 8.5, "the middle waypoint must have moved off the wall (x={:.2})", route[1][0]);
        assert_eq!(route[0], [8.0, -8.0, 1.0], "the fixed start endpoint must not move");
        assert_eq!(route[2], [8.0, 8.0, 1.0], "the fixed goal endpoint must not move");
        // And the inflated route stays walkable end to end.
        assert!(col.path_clear(route[0], route[1], r) && col.path_clear(route[1], route[2], r),
            "the inflated route must stay walkable (both segments path_clear)");
    }

    /// **The corner-buffer offset must NOT widen a narrow corridor into the far wall** (#685 discipline).
    /// In a corridor only `2·(radius+small)` wide, a centred waypoint has a wall close on BOTH sides;
    /// inflation must CENTRE it (bounded by the midpoint), never shove it into the opposite wall.
    #[test]
    fn inflate_route_centres_a_narrow_corridor_never_seals_it() {
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        let buffer = 2.0;
        // A 5u-wide corridor: walls at east=0 and east=5, running along north. Centre is east=2.5.
        let floor = MeshData {
            positions: vec![[-20.0, 0.0, -2.0], [20.0, 0.0, -2.0], [20.0, 0.0, 7.0], [-20.0, 0.0, 7.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let wall = |e: f32| MeshData {
            positions: vec![[-20.0, 0.0, e], [20.0, 0.0, e], [20.0, 8.0, e], [-20.0, 8.0, e]],
            normals: vec![[1.0, 0.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor, wall(0.0), wall(5.0)], objects: vec![], textures: vec![] }, 2.0);

        // A waypoint pushed slightly off-centre toward the east wall (east=3.2 → 1.8u from that wall).
        let mut route = vec![[2.5f32, -8.0, 1.0], [3.2, 0.0, 1.0], [2.5, 8.0, 1.0]];
        col.inflate_route_off_corners(&mut route, r, buffer);
        // It must move toward the CENTRE (west, ~2.5), never past it into the west wall (never < ~2.5).
        assert!(route[1][0] <= 3.2 + 1e-3 && route[1][0] >= 2.4,
            "narrow-corridor inflation must centre the waypoint (moved to x={:.2}), never shove it past \
             the midpoint into the opposite wall", route[1][0]);
        assert!(col.path_clear(route[0], route[1], r) && col.path_clear(route[1], route[2], r),
            "the corridor route must stay walkable — inflation must never seal a passable corridor");
    }

    /// The planner must not hand the walker a route through a gap its own collision volume cannot
    /// pass. With the slot as the ONLY way through, the honest answer is "no route" — not a route
    /// the character will wedge in. (Run on the FINE 2u tier: that is the tier that can express a
    /// sub-capsule gap at all, and the tier the walker actually steers along.)
    #[test]
    fn find_path_refuses_a_gap_narrower_than_the_character() {
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        let narrow = slotted_wall(1.5);
        let route = narrow.find_path_res([5.0, 9.0, 0.0], [15.0, 9.0, 0.0], r, &[], false, 2.0,
            None, 0.0, PlanCtx::default());
        assert!(route.is_none(),
            "A* threaded a gap the character cannot fit through: {route:?}");
        // Control: widen the slot past the character's diameter and the same route appears.
        let wide = slotted_wall(2.0 * r + 1.0);
        let route = wide.find_path_res([5.0, 9.0, 0.0], [15.0, 9.0, 0.0], r, &[], false, 2.0,
            None, 0.0, PlanCtx::default());
        assert!(route.is_some(), "a gap the character DOES fit through must stay routable");
    }

    /// The waypoint inset (#312) must never nudge a waypoint INTO a wall.
    ///
    /// Its original guard, `edge_ok(nudged)`, cannot prevent that — and not by accident: `is_standable`
    /// (in `column_hits`) rejects a near-vertical face via its flatness test `|nz| < NAV_NEAR_HORIZONTAL`,
    /// so a wall is *structurally incapable* of being a `nearest_floor` hit. There is floor at a wall's
    /// foot, so `edge_ok` reports "walkable" right up against one. That was survivable while the
    /// inset was small; the tiered planner now plans at a GENEROUS radius, which scales `margin` and
    /// so the push. A bigger push behind a wall-blind guard walks the character CLOSER to walls than
    /// before — so the push is also checked against the character's own collision volume.
    #[test]
    fn the_waypoint_inset_never_nudges_the_character_into_a_wall() {
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        // A corridor with a DROP on one side and a WALL on the other. The floor runs out at
        // north 13.5; a wall stands on the floor at north 10. The route runs east between them, so
        // the inset — pushing away from the drop, the only hazard `edge_ok` can see — is aimed
        // squarely at the wall, which it cannot see at all.
        let floor = MeshData { // east 0..40, north 0..12.5
            positions: vec![[0.0, 0.0, 0.0], [12.5, 0.0, 0.0], [12.5, 0.0, 40.0], [0.0, 0.0, 40.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let wall = MeshData { // vertical plane at north = 10, spanning the corridor's whole length
            positions: vec![[10.0, 0.0, 0.0], [10.0, 0.0, 40.0], [10.0, 10.0, 40.0], [10.0, 10.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets {
            terrain: vec![floor, wall], objects: vec![], textures: vec![] }, 8.0);
        // The blind spot itself: there is floor at the wall's FOOT, so the inset's only guard reads
        // "walkable" right up against it. `is_standable`'s flatness test (`|nz| < NAV_NEAR_HORIZONTAL`)
        // rejects the near-vertical face, so a wall can never be a `nearest_floor` hit — by construction.
        assert!(col.nearest_floor(20.0, 10.0, 0.0, 3.0, 8.0).is_some(),
            "nearest_floor is structurally blind to the wall — that is the whole point");

        let path = col.find_path([4.0, 12.0, 0.0], [36.0, 12.0, 0.0], r, &[], true)
            .expect("a route along the corridor exists");
        for p in &path {
            assert!(col.footprint_clear(p[0], p[1], p[2], r, 8),
                "the inset pushed a waypoint into the character's own collision volume at {p:?} — \
                 it nudged away from the drop it CAN see, straight into the wall it CANNOT");
        }
    }

    /// The swept-edge cell threshold must actually COVER the tier the walker steers along. These two
    /// numbers live in different modules and were coupled by a comment; a comment does not fail a
    /// build. If `LOCAL_CELL` were raised above `SWEPT_EDGE_MAX_CELL`, the fine tier would silently
    /// fall back to ray clearance and #358 would be un-fixed with every test still green.
    #[test]
    fn the_swept_edge_test_covers_the_tier_the_walker_actually_steers_along() {
        const {
            assert!(crate::steering::LOCAL_CELL <= SWEPT_EDGE_MAX_CELL,
                "the local tier is not covered by the swept edge test (<=) — the walker would \
                 be steered along ray-validated edges again (#358)");
        }
        // ...and the coarse whole-zone grid (8u) must stay OUTSIDE it — sweeping an 8u lattice line
        // seals corridors (Ak'Anon: 90/120 routable pairs -> 55/120).
        const { assert!(SWEPT_EDGE_MAX_CELL < 8.0, "the coarse tier must remain a ray-validated selector"); }
    }

    /// The coarse/fine asymmetry of `edge_clear` is a deliberate, MEASURED compromise, not an
    /// oversight — pin it so it can't be "tidied" into either extreme. Sweeping the volume on the
    /// coarse 8u lattice seals narrow corridors (Ak'Anon: 90/120 routable pairs → 55/120); casting
    /// a ray on the fine tier is what let the walker be handed the unwalkable route in the first
    /// place.
    #[test]
    fn edge_clear_sweeps_the_volume_at_the_resolution_the_walker_steers_along() {
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        let col = slotted_wall(1.5); // narrower than the character (2 * PLAYER_RADIUS)
        let (from, to) = ([5.0, 9.0, 3.0], [15.0, 9.0, 3.0]);
        // FINE tier (2u = nav::steering::LOCAL_CELL): the plan the walker actually steers along. The
        // character's volume must fit, and a corridor here has lateral cells to detour into.
        assert!(!col.edge_clear(from, to, r, 2.0),
            "the fine tier must validate the character's collision VOLUME");
        // COARSE tier (8u): a corridor SELECTOR over a lattice whose centre line is not the walked
        // line — a ray. See `edge_clear` for the routability measurement behind this.
        assert!(col.edge_clear(from, to, r, 8.0),
            "the coarse tier must stay a ray — sweeping an 8u lattice line seals corridors");
    }

    /// A wall at east=10 with TWO ways through: a `narrow` slot centred on north=3 and a `wide` one
    /// centred on north=15. Floor is 20x20 at z=0.
    fn two_slot_wall(narrow: f32, wide: f32) -> Collision {
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [20.0, 0.0, 20.0], [0.0, 0.0, 20.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let panel = |n0: f32, n1: f32| MeshData {
            positions: vec![[n0, 0.0, 10.0], [n1, 0.0, 10.0], [n1, 10.0, 10.0], [n0, 10.0, 10.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        Collision::build(&ZoneAssets {
            terrain: vec![
                floor,
                panel(0.0, 3.0 - narrow / 2.0),                 // ..narrow slot @ north 3..
                panel(3.0 + narrow / 2.0, 15.0 - wide / 2.0),   // ..wide slot @ north 15..
                panel(15.0 + wide / 2.0, 20.0),
            ],
            objects: vec![], textures: vec![],
        }, 2.0)
    }

    /// The owner's requirement: walk with ROOM by default. Given a narrow door and a wide one, the
    /// planner takes the wide one even though the narrow one is closer to the straight line — a
    /// route that skims a wall is a route the walker slides into.
    #[test]
    fn find_path_prefers_the_route_with_room_when_one_exists() {
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        // Narrow: passable by the character (> 2r) but NOT with the preferred margin (< 2 * 2r).
        // Wide: comfortably passable with the preferred margin.
        let col = two_slot_wall(2.0 * r + 0.5, 2.0 * NAV_PREFERRED_CLEARANCE + 2.0);
        let (path, tight) = tiered_route(&col, [5.0, 3.0, 0.0], [15.0, 3.0, 0.0], r, 2.0)
            .expect("a route exists through both slots");
        assert!(!tight, "a roomy route exists, so the planner must not report a tight one");
        // It detoured NORTH to the wide slot instead of squeezing through the near, narrow one.
        assert!(path.iter().any(|p| p[1] > 10.0),
            "planner squeezed through the narrow slot instead of taking the roomy one: {path:?}");
    }

    /// ...but a narrow door must stay ROUTABLE. When the roomy route does not exist, fall back to the
    /// minimum clearance and say so — a tight route is walkable, just riskier.
    #[test]
    fn find_path_falls_back_to_the_minimum_clearance_when_only_a_tight_route_exists() {
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        let col = slotted_wall(2.0 * r + 0.5); // fits the character, not the preferred margin
        let before = col.tight_plans();
        let (path, tight) = tiered_route(&col, [5.0, 9.0, 0.0], [15.0, 9.0, 0.0], r, 2.0)
            .expect("a tight door must stay routable — sealing it is a worse bug (#310)");
        assert!(!path.is_empty());
        assert!(tight, "the planner must REPORT that this route only exists at minimum clearance");
        assert_eq!(col.tight_plans(), before + 1,
            "a tight route must be counted so `/v1/observe/debug` can surface `nav_tight` — a \
             degraded mode must never be silent");
    }

    /// #310, the repeat offence: the fallback floor is `PLAYER_RADIUS` and NOTHING gets planned below
    /// it. A gap narrower than the character is not a tight route, it is NO route — and saying so is
    /// the whole point of #358.
    #[test]
    fn find_path_never_plans_below_the_player_radius() {
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        let col = slotted_wall(2.0 * r - 0.5); // narrower than the character
        for asked in [r, r * 0.5, r * 0.25, 0.0] {
            let route = tiered_route(&col, [5.0, 9.0, 0.0], [15.0, 9.0, 0.0], asked, 2.0);
            assert!(route.is_none(),
                "radius={asked}: planner threaded a gap the character cannot fit through. The \
                 minimum clearance is PLAYER_RADIUS ({r}) and a caller asking for less does not \
                 lower it (#310).");
        }
    }

    /// The OTHER hazard. `edge_clear` sees geometry that is in the way; only `ground_margin_ok` sees
    /// geometry that is MISSING. Route around the inside corner of a sheer drop and require the
    /// route to keep its standing room from the brink — "walking near edges is a good way to fall
    /// off them".
    #[test]
    fn find_path_keeps_its_standing_room_from_a_drop() {
        // Floor is an L: east 0..8 (all north), plus east 8..20 for north 12..20.
        // The rest — east 8..20, north 0..12 — is a VOID the character would fall into.
        let quad = |e0: f32, e1: f32, n0: f32, n1: f32| MeshData {
            positions: vec![[n0, 0.0, e0], [n1, 0.0, e0], [n1, 0.0, e1], [n0, 0.0, e1]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets {
            terrain: vec![quad(0.0, 8.0, 0.0, 20.0), quad(8.0, 20.0, 12.0, 20.0)],
            objects: vec![], textures: vec![],
        }, 2.0);
        assert!(!col.ground_margin_ok(9.0, 13.0, 0.0, NAV_PREFERRED_CLEARANCE),
            "a point 1u from the brink has no standing room");
        assert!(col.ground_margin_ok(12.0, 17.0, 0.0, NAV_PREFERRED_CLEARANCE),
            "a point well inside the floor does");

        // The straight line (4,4) -> (16,16) runs clean through the void, so A* must round the
        // inside corner at (8,12) — the exact place a route hugs a drop.
        let (path, _) = tiered_route(&col, [4.0, 4.0, 0.0], [16.0, 16.0, 0.0],
            eqoxide_core::physics::PLAYER_RADIUS, 2.0).expect("a route around the void exists");
        // Every waypoint the walker is asked to stand on has a body-width of ground around it.
        // (The final waypoint is snapped to the caller's exact goal, which is the caller's problem.)
        for p in &path[..path.len() - 1] {
            assert!(col.ground_margin_ok(p[0], p[1], p[2], NAV_PREFERRED_CLEARANCE),
                "route hugs the brink of the drop at {p:?} — no standing room");
        }
    }

    /// The case the waypoint inset CANNOT rescue, and therefore the case that justifies testing the
    /// ledge margin inside the SEARCH rather than nudging waypoints afterwards.
    ///
    /// Two platforms joined by a short 3u CATWALK and a long 8u BRIDGE. The catwalk is wide enough
    /// for the character (> 2 · PLAYER_RADIUS) and it is the direct line — but standing on it puts a
    /// sheer drop barely a step away on both sides. The inset cannot fix that: nudging away from one
    /// brink walks into the other, so the pushes cancel and the waypoint stays on the brink. Only the
    /// search can fix it, by going round.
    #[test]
    fn find_path_takes_the_long_wide_bridge_over_the_short_narrow_catwalk() {
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        let quad = |e0: f32, e1: f32, n0: f32, n1: f32| MeshData {
            positions: vec![[n0, 0.0, e0], [n1, 0.0, e0], [n1, 0.0, e1], [n0, 0.0, e1]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets {
            terrain: vec![
                quad(0.0, 10.0, 0.0, 20.0),    // platform A
                quad(20.0, 30.0, 0.0, 20.0),   // platform B
                quad(10.0, 20.0, 10.0, 13.0),  // CATWALK: 3u wide, on the direct line
                quad(10.0, 20.0, 0.0, 6.0),    // BRIDGE: 6u wide, a detour to the south
                                               // (void between them: north 6..10)
            ],
            objects: vec![], textures: vec![],
        }, 2.0);
        // The catwalk fits the character but has no standing room; the bridge has both.
        assert!(col.ground_margin_ok(15.0, 11.5, 0.0, r), "the catwalk fits the character");
        assert!(!col.ground_margin_ok(15.0, 11.5, 0.0, NAV_PREFERRED_CLEARANCE),
            "...but there is a drop a step away on both sides");
        assert!(col.ground_margin_ok(15.0, 3.0, 0.0, NAV_PREFERRED_CLEARANCE), "the bridge has room");

        let (path, tight) = tiered_route(&col, [5.0, 11.0, 0.0], [25.0, 11.0, 0.0], r, 2.0)
            .expect("both crossings exist");
        assert!(!tight, "a roomy crossing exists, so the route must not be a tight one");
        // It took the BRIDGE, not the direct catwalk.
        assert!(path.iter().any(|p| p[1] < 6.0),
            "planner walked the brink of the catwalk instead of detouring to the wide bridge — and \
             the waypoint inset cannot save it (both sides are a drop, so the nudges cancel): {path:?}");
        for p in &path[..path.len() - 1] {
            assert!(col.ground_margin_ok(p[0], p[1], p[2], NAV_PREFERRED_CLEARANCE),
                "route has no standing room at {p:?}");
        }
        // ...and when the catwalk is the ONLY crossing, it stays routable — as a TIGHT route.
        let only_catwalk = Collision::build(&ZoneAssets {
            terrain: vec![
                quad(0.0, 10.0, 0.0, 20.0), quad(20.0, 30.0, 0.0, 20.0), quad(10.0, 20.0, 10.0, 13.0),
            ],
            objects: vec![], textures: vec![],
        }, 2.0);
        let (_, tight) = tiered_route(&only_catwalk, [5.0, 11.0, 0.0], [25.0, 11.0, 0.0], r, 2.0)
            .expect("the only crossing must stay routable (#310) — sealing it is the worse bug");
        assert!(tight, "a catwalk-only crossing is walkable, but it must REPORT as tight");
    }

    /// #358 drift guard: the clearance the PLANNER validates with and the radius the CONTROLLER
    /// moves with are the same number, and the waypoint inset (#312) is never smaller than it.
    /// An inset below the collision radius puts the capsule's shoulder inside the wall by
    /// construction — which is what the old `.min(cell * 0.45)` clamp did on the fine 2u tier
    ///
    /// The inset must deliver its margin from BOTH walls at an inside corner. A normalised diagonal
    /// push spends the margin on the diagonal and leaves only `margin / √2` per wall — under the
    ///
    #[test]
    fn find_path_routes_around_a_partial_wall() {
        // 20x20 floor at z=0.
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [20.0, 0.0, 20.0], [0.0, 0.0, 20.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // Partial wall at world east=10, spanning north 0..14 (gap at north 14..20), height 0..10.
        let wall = MeshData {
            positions: vec![[0.0, 0.0, 10.0], [14.0, 0.0, 10.0], [14.0, 10.0, 10.0], [0.0, 10.0, 10.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor, wall], objects: vec![], textures: vec![] }, 2.0);
        // The direct line (5,5)->(15,5) crosses the wall (north 5 < 14) → blocked.
        assert!(col.segment_blocked([5.0, 5.0, 3.0], [15.0, 5.0, 3.0]));
        // find_path routes AROUND the wall through the northern gap.
        let path = col.find_path([5.0, 5.0, 0.0], [15.0, 5.0, 0.0], 1.0, &[], false)
            .expect("a route around the wall should exist");
        let last = *path.last().unwrap();
        assert!((last[0] - 15.0).abs() < 1.5 && (last[1] - 5.0).abs() < 1.5, "ends at goal: {last:?}");
        assert!(path.iter().any(|p| p[1] > 12.0), "path must detour north through the gap: {path:?}");
    }

    #[test]
    fn find_path_edge_margin_keeps_waypoints_on_mesh() {
        // #312 safety: the waypoint edge-inset must never shove a point OFF the floor (it only nudges
        // toward walkable interior, guarded by `edge_ok(nudged)`). Route across the same 20x20 floor
        // the wall test uses and assert every waypoint stays on real floor. (The real edge-hug fix is
        // validated live against #314; a flat synthetic floor is already covered by path_clear.)
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [20.0, 0.0, 20.0], [0.0, 0.0, 20.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }, 2.0);
        let path = col.find_path([3.0, 3.0, 0.0], [17.0, 17.0, 0.0], 1.0, &[], false)
            .expect("a route across the floor should exist");
        for p in &path {
            assert!(col.nearest_floor(p[0], p[1], 0.0, 3.0, 8.0).is_some(),
                "edge-inset pushed a waypoint off-mesh: {p:?}");
        }
    }

    #[test]
    fn find_path_returns_partial_route_when_goal_is_walled_off() {
        // 200x200 floor at z=0 (big enough for the 8u nav grid to make real progress).
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [200.0, 0.0, 0.0], [200.0, 0.0, 200.0], [0.0, 0.0, 200.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // FULL wall at east=100 spanning the whole north extent (0..200) — no gap, so the goal is
        // sealed off with no route to it.
        let wall = MeshData {
            positions: vec![[0.0, 0.0, 100.0], [200.0, 0.0, 100.0], [200.0, 20.0, 100.0], [0.0, 20.0, 100.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor, wall], objects: vec![], textures: vec![] }, 8.0);
        let start = [20.0, 100.0, 0.0];
        let goal  = [180.0, 100.0, 0.0]; // sealed behind the wall at east=100
        // No full route exists — and it is DEFINITIVELY Unreachable (the frontier closed with the
        // goal sealed behind the wall), not an `Exhausted` "I gave up". The honest API says so; the
        // steering partial below rides on `find_path_res`, never on this answer (#337/#356).
        assert!(matches!(plan(&col, start, goal, 1.0), PlanOutcome::Unreachable { .. }),
            "goal is walled off — Unreachable, got {:?}", plan(&col, start, goal, 1.0));
        // But a partial route toward the goal does (#188): it advances east toward the wall and
        // stops on the near side (never crossing east=100) instead of returning "no route".
        let partial = col.find_path(start, goal, 1.0, &[], true).expect("partial route toward the goal");
        let last = *partial.last().unwrap();
        assert!(last[0] > start[0] + 30.0, "partial route makes real progress toward the goal: {last:?}");
        assert!(last[0] < 100.0, "partial route stops on the near side of the wall: {last:?}");
    }

    /// **THE COLD BLOCKAGE DIAGNOSIS (#378 Phase 2, design §5a).** A sealed component: the goal is
    /// perfectly WALKABLE (floor under it), but a full wall stands between the start's component and
    /// it. So `goal_blocked_by` must be `None` (the goal itself is fine — this is the case where the
    /// goal-only diagnosis would teach the agent nothing), and `frontier_blocked_by` must NAME THE
    /// WALL, at a position on the wall plane — the obstruction that ended the closest approach.
    #[test]
    fn unreachable_frontier_blocked_by_names_the_wall_that_sealed_the_component() {
        use crate::traversability::HazardKind;
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [200.0, 0.0, 0.0], [200.0, 0.0, 200.0], [0.0, 0.0, 200.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let wall = MeshData {
            positions: vec![[0.0, 0.0, 100.0], [200.0, 0.0, 100.0], [200.0, 20.0, 100.0], [0.0, 20.0, 100.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor, wall], objects: vec![], textures: vec![] }, 8.0);
        let start = [20.0, 100.0, 0.0];
        let goal  = [180.0, 100.0, 0.0]; // walkable floor under it, but sealed behind the wall @east=100
        match col.find_path_ex(start, goal, 1.0, &[], 8.0, None, 0.0, PlanCtx::default()) {
            PlanOutcome::Unreachable { reason: NoRoute::SearchClosed, goal_blocked_by, frontier_blocked_by } => {
                assert!(goal_blocked_by.is_none(),
                    "the goal itself is walkable — goal_blocked_by must be None, got {goal_blocked_by:?}");
                let f = frontier_blocked_by.expect("the frontier obstruction (the wall) must be named");
                assert_eq!(f.hazard, HazardKind::Wall, "the sealing obstruction is a Wall, got {f:?}");
                assert!((f.at[0] - 100.0).abs() < 8.0,
                    "the blockage should sit on the wall plane @east=100, got {:?}", f.at);
            }
            other => panic!("a sealed component must be Unreachable(SearchClosed) with a frontier blockage, got {other:?}"),
        }
    }

    /// **GOAL-BLOCKED IS DEFINITIVE.** A goal off the mesh (no floor anywhere in its column) must set
    /// `goal_blocked_by` (a Floor hazard) — the highest-value, definitive fact: if the goal cannot be
    /// stood at, no search could have succeeded. Pairs with the `GoalNotWalkable` reason.
    #[test]
    fn unreachable_goal_off_the_mesh_is_named_by_goal_blocked_by() {
        use crate::traversability::HazardKind;
        let assets = ZoneAssets { terrain: vec![slab(0.0, 0.0, 200.0, 0.0, 200.0, true)], objects: vec![], textures: vec![] };
        let col = Collision::build(&assets, 32.0);
        let out = col.find_path_ex([16.0, 16.0, 0.0], [4000.0, 4000.0, 0.0], 1.0, &[], 8.0, None, 0.0, PlanCtx::default());
        match out {
            PlanOutcome::Unreachable { reason: NoRoute::GoalNotWalkable, goal_blocked_by, .. } => {
                let g = goal_blocked_by.expect("an off-mesh goal must set goal_blocked_by (definitive)");
                assert_eq!(g.hazard, HazardKind::Floor, "off-mesh goal is a Floor blockage, got {g:?}");
            }
            other => panic!("an off-mesh goal must be Unreachable(GoalNotWalkable) with goal_blocked_by, got {other:?}"),
        }
    }

    /// **#337 DISCIPLINE: `Exhausted` CARRIES NO BLOCKAGE.** A search cut short by its node cap did
    /// not close its frontier, so it does not KNOW what stopped it — inventing a blockage there would
    /// be a fabrication. `Exhausted` has no blockage fields AT ALL (it is a different variant); this
    /// pins that a cut-short search is never dressed up with a diagnosis it did not earn.
    #[test]
    fn exhausted_never_carries_a_blockage() {
        let assets = ZoneAssets { terrain: vec![slab(0.0, 0.0, 64.0, 0.0, 1600.0, true)], objects: vec![], textures: vec![] };
        let col = Collision::build(&assets, 32.0);
        let tiny = PlanCtx { node_cap: Some(4), ..PlanCtx::default() };
        match col.find_path_ex([8.0, 32.0, 0.0], [1560.0, 32.0, 0.0], 1.0, &[], 8.0, None, 0.0, tiny) {
            PlanOutcome::Exhausted { limit: PlanLimit::NodeCap, .. } => {} // no blockage field exists — good
            other => panic!("a node-capped search must be Exhausted (no blockage), got {other:?}"),
        }
    }

    /// **NEVER A FALSE BLOCKAGE (the honesty invariant for the new payload).** Two guarantees:
    /// (1) a SUCCESSFUL plan (`Route`) never carries any blockage — a blockage on a route the walker
    /// can walk would be a confident falsehood; (2) any `frontier_blocked_by` a failed plan reports
    /// must be a REAL obstruction — re-asking the one authority (`can_traverse` at the minimum tier)
    /// from the frontier toward the goal must AGREE that it is blocked. The diagnosis is derived from
    /// exactly that predicate, so it can never over-claim a wall the body could have squeezed past.
    #[test]
    fn the_blockage_diagnosis_is_never_a_false_positive() {
        // (1) Open floor, a reachable goal → a Route, and a Route has no blockage fields at all.
        let open = Collision::build(
            &ZoneAssets { terrain: vec![slab(0.0, 0.0, 200.0, 0.0, 200.0, true)], objects: vec![], textures: vec![] }, 32.0);
        match open.find_path_ex([16.0, 16.0, 0.0], [180.0, 180.0, 0.0], 1.0, &[], 8.0, None, 0.0, PlanCtx::default()) {
            PlanOutcome::Route(_) => {} // the variant has no blockage — a successful plan cannot carry one
            other => panic!("open floor must route, got {other:?}"),
        }

        // (2) A sealed component's frontier_blocked_by must be corroborated by the one authority.
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [200.0, 0.0, 0.0], [200.0, 0.0, 200.0], [0.0, 0.0, 200.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let wall = MeshData {
            positions: vec![[0.0, 0.0, 100.0], [200.0, 0.0, 100.0], [200.0, 20.0, 100.0], [0.0, 20.0, 100.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor, wall], objects: vec![], textures: vec![] }, 8.0);
        if let PlanOutcome::Unreachable { frontier_blocked_by: Some(f), .. } =
            col.find_path_ex([20.0, 100.0, 0.0], [180.0, 100.0, 0.0], 1.0, &[], 8.0, None, 0.0, PlanCtx::default())
        {
            // Re-ask the authority: from the blockage's own approach, a step toward the goal (east)
            // at the minimum tier must genuinely be refused. A blockage the body could pass would be
            // a false positive.
            use crate::traversability::{Traversability, Point, Tier};
            let trav = Traversability::new(&col, Tier::Minimum.units(), 8.0, 0.0, false);
            let from = Point::new([f.at[0] - 4.0, f.at[1]], 0.0);
            let to   = Point::new([f.at[0] + 4.0, f.at[1]], 0.0);
            assert!(trav.can_traverse(from, to).is_err(),
                "a reported frontier blockage at {:?} must be a REAL obstruction the authority also refuses", f.at);
        }
    }

    #[test]
    fn find_path_swims_across_a_surface_pool_instead_of_diving() {
        // Positions are [north, up, east]. A deep pool bottom under the whole span, with dry shores
        // laid on top at z=0 at each end, and a surface-level water body between them.
        // Wound UP-FACING (same vertex order as `slab`) — these are FLOORS, which is what the
        // `normals: [0,1,0]` below has always claimed. The winding used to be reversed here, so
        // every "floor" in this fixture was really a down-facing face; the test only passed because
        // an all-inverted mesh failed the old whole-zone winding gate, which switched the
        // floor-normal filter off entirely. With the filter always on, a floor has to be wound like
        // one.
        let quad = |n0: f32, n1: f32, e0: f32, e1: f32, up: f32| MeshData {
            positions: vec![[n0, up, e0], [n0, up, e1], [n1, up, e1], [n1, up, e0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let pool_bottom = quad(0.0, 40.0, 0.0, 160.0, -92.0); // deep floor, east 0..160
        let near_shore  = quad(0.0, 40.0, 0.0, 40.0, 0.0);    // dry, east 0..40
        let far_shore   = quad(0.0, 40.0, 120.0, 160.0, 0.0); // dry, east 120..160
        let mut col = Collision::build(
            &ZoneAssets { terrain: vec![pool_bottom, near_shore, far_shore], objects: vec![], textures: vec![] }, 8.0);
        // SUNKEN pool: water surface at z=-8, a few units BELOW the z=0 shores you wade in from
        // (like Halas's central pool under the ice) — so the swim edge has to probe down to find it.
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::flat_below(-8.0))));

        let start = [20.0, 20.0, 0.0];   // near shore
        let goal  = [140.0, 20.0, 0.0];  // far shore, across the pool
        let path = col.find_path(start, goal, eqoxide_core::physics::PLAYER_RADIUS, &[], false)
            .expect("a swim route across the surface pool should exist");
        // It reaches the far shore...
        let last = *path.last().unwrap();
        assert!((last[0] - 140.0).abs() < 12.0, "ends at the far shore: {last:?}");
        // ...at the SURFACE (~ -8), never diving toward the -92 pool bottom.
        let deepest = path.iter().map(|w| w[2]).fold(f32::MAX, f32::min);
        assert!(deepest > -20.0, "route stays near the surface, not the pool bottom: deepest={deepest} path={path:?}");
    }

    // ─── 3D water-volume nav (design §6/§7, Slice 2) — the interior water-node families ─────────
    //
    // These drive the WIRED search over BOUNDED (`water_slab`) water — the shape real `.wtr` water
    // has, and the only shape that stores span-grid columns (unbounded `flat_below` water reports
    // `UnboundedBelow` and stores nothing, so every existing swim test above exercises the legacy
    // surface/haul-out families untouched). Every expected bound is the FIXTURE's analytic geometry,
    // never read back from the span grid under test (design §10 non-circular verification).
    /// Slice 2 owner case (a) — a MID-WATER GOAL. A walled pool (floor −44, surface −4) with a goal
    /// asked for at −24 (20u under the surface, 20u off the bottom). The route must END at the asked
    /// depth — its interior water node — NOT be projected up to the surface (the legacy hack) NOR
    /// dive to the −44 bottom. Mutation check: revert the Slice-2 `water_goal` clause and the goal is
    /// projected to the surface (−4), so the final-depth assertion goes RED (last z ≈ −4). Revert the
    /// interior edge families and the depth node is unreachable, so the route no longer ends at −24.
    #[test]
    fn find_path_reaches_a_mid_water_goal_node_at_depth() {
        use eqoxide_zone_geometry::water_grid::VRES;
        let assets = ZoneAssets {
            terrain: vec![
                slab(-44.0, 0.0, 64.0, 0.0, 64.0, true),                 // pool floor
                wall_east(0.0, -44.0, 0.0), wall_east(64.0, -44.0, 0.0), // z-extent + confinement
            ],
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::water_slab(-44.0, -4.0))));

        let start = [30.0, 10.0, -6.0];   // floating just under the surface (no footing → surface anchor)
        let goal  = [30.0, 46.0, -24.0];  // MID-WATER: 20u below the surface, 20u above the −44 floor
        let path = col.find_path(start, goal, eqoxide_core::physics::PLAYER_RADIUS, &[], false)
            .expect("a route to the mid-water goal must exist");
        let last = *path.last().unwrap();
        assert!((last[0] - goal[0]).abs() < 8.0 && (last[1] - goal[1]).abs() < 8.0,
            "the route reaches the goal column (last = {last:?})");
        assert!((last[2] - (-24.0)).abs() <= VRES + 0.01,
            "the route ENDS at the asked mid-water depth −24, not the surface (final wp z = {})", last[2]);
        // No bottom detour: the route stays comfortably above the −44 pool bottom.
        let deepest = path.iter().fold(f32::MAX, |m, w| m.min(w[2]));
        assert!(deepest > -30.0, "no waypoint dives toward the −44 bottom (deepest = {deepest})");
        // P-3D-1 in-volume (design §10, non-circular — checked against the FIXTURE geometry, not the
        // span grid): every waypoint that dropped below the shore sits inside the analytic water
        // volume (feet above the −44 floor, body below the −4 surface). No node in solid, no node in
        // air — the #534/#540 honesty guarantee carried into the emitted route.
        for w in &path[1..] {
            if w[2] < -4.5 {
                assert!(w[2] > -44.0 && w[2] < -4.0,
                    "an in-water waypoint must lie inside the fixture's water volume (wp = {w:?})");
            }
        }
        // Honesty (design §7.3): the depth was HONOURED, not accommodated to the surface, and arrival
        // anchors to the same depth — the plan/arrival/report one-chain.
        assert_eq!(col.goal_z_was_snapped(goal), None,
            "a navigable mid-water goal is planned at depth — no ToWaterSurface accommodation");
        assert!((col.resolve_goal_floor(goal).unwrap() - (-24.0)).abs() <= VRES + 0.01,
            "arrival anchors to the mid-water node depth, mirroring the plan");
    }

    /// Slice 2 honesty NEGATIVE control — a water node near a LAND goal is NOT a false arrival. Same
    /// walled pool, but the goal is a submerged FLOOR the walker would have to reach by descending/
    /// hauling, deeper than the swim plane. The planner must NOT report `arrived` by floating a
    /// swimmer in the water column within `GOAL_TIER_TOL` of the dry floor — that is the #359 lie the
    /// arrival gate forbids. Here the goal at the −44 floor is honoured as a submerged tier and
    /// reported via `ToWaterSurface` (the walker floats above it), exactly as before Slice 2 — the
    /// interior nodes must not quietly turn it into a bogus mid-water "arrival".
    #[test]
    fn interior_water_nodes_do_not_fake_arrival_at_a_submerged_floor_goal() {
        let assets = ZoneAssets {
            terrain: vec![
                slab(-44.0, 0.0, 64.0, 0.0, 64.0, true),
                wall_east(0.0, -44.0, 0.0), wall_east(64.0, -44.0, 0.0),
            ],
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::water_slab(-44.0, -4.0))));
        let goal = [30.0, 46.0, -44.0]; // the pool FLOOR (a dry tier under the water), not a swim node
        // The floor is a real tier the caller named, but it is > GOAL_TIER_TOL below the surface, so
        // arrival is the surface accommodation — never a mid-water node masquerading as the floor.
        assert!((col.resolve_goal_floor(goal).unwrap() - (-4.0)).abs() < 1.0,
            "a submerged floor goal still resolves to the water surface (walker floats above it)");
        assert!(matches!(col.goal_z_was_snapped(goal), Some(GoalSnap::ToWaterSurface { .. })),
            "and reports the surface accommodation — the interior nodes did not fake a depth arrival");
    }

    #[test]
    fn find_path_skirts_npc_camps_when_given_avoid_points() {
        // Big open floor (no walls) so routing is driven purely by the aggro cost (#67).
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [200.0, 0.0, 0.0], [200.0, 0.0, 200.0], [0.0, 0.0, 200.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }, 16.0);
        let start = [20.0, 100.0, 0.0];
        let goal  = [180.0, 100.0, 0.0];
        // An NPC sitting dead-centre on the straight route.
        let npc = [[100.0, 100.0f32]];
        let min_to_npc = |path: &[[f32; 3]]| path.iter()
            .map(|w| ((w[0] - npc[0][0]).powi(2) + (w[1] - npc[0][1]).powi(2)).sqrt())
            .fold(f32::MAX, f32::min);

        let direct = col.find_path(start, goal, 1.0, &[], false).expect("open route exists");
        let skirt  = col.find_path(start, goal, 1.0, &npc, false).expect("aggro route still exists (mild penalty)");

        // The plain route runs right past the NPC; the aggro route bows away from it.
        assert!(min_to_npc(&direct) < 10.0, "plain route passes through the NPC: {}", min_to_npc(&direct));
        assert!(min_to_npc(&skirt) > min_to_npc(&direct) + 8.0,
            "aggro route should skirt the NPC (min dist {} vs {})", min_to_npc(&skirt), min_to_npc(&direct));
        // …and still arrive.
        let last = *skirt.last().unwrap();
        assert!((last[0] - goal[0]).abs() < 3.0 && (last[1] - goal[1]).abs() < 3.0, "reaches goal: {last:?}");
    }

    #[test]
    fn aggro_buffer_widens_the_berth_around_npcs() {
        // #242: a larger `aggro_buffer` on find_path_res gives the NPC MORE berth (route bows wider),
        // while still reaching the goal — the avoidance stays soft (never fails).
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [200.0, 0.0, 0.0], [200.0, 0.0, 200.0], [0.0, 0.0, 200.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }, 16.0);
        let start = [20.0, 100.0, 0.0];
        let goal  = [180.0, 100.0, 0.0];
        let npc = [[100.0, 100.0f32]];
        let min_to_npc = |path: &[[f32; 3]]| path.iter()
            .map(|w| ((w[0] - npc[0][0]).powi(2) + (w[1] - npc[0][1]).powi(2)).sqrt())
            .fold(f32::MAX, f32::min);

        let narrow = col.find_path_res(start, goal, 1.0, &npc, false, 8.0, None, 0.0, PlanCtx::default()).expect("route exists");
        let wide   = col.find_path_res(start, goal, 1.0, &npc, false, 8.0, None, 60.0, PlanCtx::default()).expect("wider route still exists");

        assert!(min_to_npc(&wide) > min_to_npc(&narrow) + 8.0,
            "a bigger aggro_buffer should widen the berth (wide {} vs narrow {})",
            min_to_npc(&wide), min_to_npc(&narrow));
        let last = *wide.last().unwrap();
        assert!((last[0] - goal[0]).abs() < 3.0 && (last[1] - goal[1]).abs() < 3.0, "still reaches goal: {last:?}");
    }

    /// Build a vertical wall plane at world east=`e`, spanning north [-100,100] and height [h0,h1].
    fn wall_east(e: f32, h0: f32, h1: f32) -> MeshData {
        MeshData {
            positions: vec![[-100.0, h0, e], [100.0, h0, e], [100.0, h1, e], [-100.0, h1, e]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// Build a horizontal floor at height `z` covering east [e0,e1] and north [-100,100].
    fn floor_band(z: f32, e0: f32, e1: f32) -> MeshData {
        MeshData {
            positions: vec![[-100.0, z, e0], [100.0, z, e0], [100.0, z, e1], [-100.0, z, e1]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// **THE WALL-AWARE INSET (#378 Phase 2 / the qcat L-corner).** A route running along a ledge
    /// with a WALL on one side and continuous floor at its foot must be nudged OFF the wall — the
    /// old floor-only inset could not see a wall standing on floor (#358), so the walker pressed
    /// into it. Isolated here from the other two wall-avoidance mechanisms so it is the ONLY thing
    /// that can move the route: a MINIMUM-tier BOUNDED plan at `cell = 8` runs with no standing-room
    /// field filter (bounded ⇒ minimum tier) AND no hug cost (`cell > SWEPT_EDGE_MAX_CELL`). That is
    /// exactly the qcat case — a single lane the field/hug cost cannot improve, only the inset can.
    ///
    /// Mutation check (verified at authoring time): drop the `wall_near(..)` disjunct from the inset
    /// push and the interior waypoints sit ~1u from the wall (unmoved) → RED.
    #[test]
    fn wall_aware_inset_pushes_a_ledge_route_off_the_wall() {
        // Continuous floor east[-5,20]; a wall at east=8 standing ON it (floor at its foot, so the
        // floor-only `edge_ok` is blind to it). The east=7 coarse cell centre sits 1u from the wall.
        let col = Collision::build(&ZoneAssets {
            terrain: vec![floor_band(0.0, -5.0, 20.0), wall_east(8.0, 0.0, 10.0)],
            objects: vec![], textures: vec![] }, 32.0);
        let path = col.find_path_res([7.0, -30.0, 0.0], [7.0, 30.0, 0.0], 1.0, &[], true,
            8.0, Some(200.0), 0.0, PlanCtx::default()).expect("route north along the ledge");
        // Interior waypoints (start is prepended at the char's real pos; the goal-cell end is pinned)
        // must be pushed clear of the wall@8. Pre-inset they sit at east=7 (1u clearance); the
        // wall-aware inset moves them ~1u west → >=1.5u clearance.
        let interior: Vec<_> = path.iter().skip(1).take(path.len().saturating_sub(2)).collect();
        assert!(!interior.is_empty(), "route should have interior waypoints: {path:?}");
        let closest = interior.iter().map(|w| 8.0 - w[0]).fold(f32::MAX, f32::min);
        assert!(closest >= 1.5,
            "wall-aware inset must push interior waypoints >=1.5u off the wall@8 \
             (closest {closest:.2}): {path:?}");
    }

    /// **PICK THE NODE CAP BY MEASUREMENT (#394).** The worker's node cap must be generous enough that
    /// a legitimate WHOLE-ZONE "no route" still reaches `SearchClosed` — a cap that truncates a real
    /// full-frontier close into `Exhausted(NodeCap)` would be a new honesty bug ("I don't know" where
    /// the truth is "no route"). So this measures the LARGEST reachable component across the MEASURED
    /// corpus (the baked zones available locally — not all of RoF2; see the `MAX_NODES` caveat): for
    /// each zone, a start on real floor routed to far corners that force A* to flood the reachable
    /// component. `closed_n` at that point IS the component size — the worst case the cap must clear.
    /// Run with a HIGH cap so nothing truncates.
    ///
    /// ```text
    /// cargo test -p eqoxide-nav --release --lib worst_case_reachable_component -- --ignored --nocapture
    /// ```
    ///
    /// **#990: at `0c37ca0` that `--release` form did not compile.** `walker.rs`'s `_cited` array
    /// is ungated and names a `#[cfg(debug_assertions)]` test, so under `--release` that name does
    /// not exist and the whole `eqoxide-nav` lib-test target failed to build there, before any
    /// measurement ran. #994 is the fix and was open at that sha. Whether you must drop `--release`
    /// therefore depends on whether #994 is in the tree you are reading — which this sentence
    /// cannot tell you, so check `walker.rs` for the gate rather than trusting this line. The #907
    /// re-measurement above was taken on the `dev` profile for that reason, which is why its wall
    /// times are what they are. (Not a claim about every row of the table: one `butcher` run's
    /// profile was never captured, and that row says so.)
    ///
    /// **`-p eqoxide-nav` is load-bearing, not decoration** (#994 review). Without it, run from the
    /// workspace root, `--lib` resolves to the ROOT package's lib target — which holds none of this
    /// file's tests — and the run prints `running 0 tests`, `241 filtered out` and exits 0. That is
    /// a vacuous green an exit code cannot distinguish from a measurement. Measured from the repo
    /// root before the flag was added. `every_documented_repro_command_in_this_file_names_its_package`
    /// holds every `--lib` recipe in this file to it.
    #[test]
    #[ignore = "requires baked zone glbs; measurement for the #394 node cap"]
    fn worst_case_reachable_component() {
        use std::time::Instant;
        let dir = format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap());
        // The biggest grids first — those are where a full close is largest.
        // #839: the default list is unchanged; the `ZONES` override is new here, and exists for the
        // same reason its four sibling corpora in this file already have one — this corpus is
        // otherwise unrunnable in a debug build (three of the largest zones in the tree, 120 starts
        // x 6 off-mesh probes each at an 8M node cap), so its accounting could not be
        // mutation-checked at all. Overriding it measures a DIFFERENT corpus than the default, and
        // the printed WORST number is then not the one this test's doc claims.
        const DEFAULT_ZONES: [&str; 3] = ["everfrost", "butcher", "gfaydark"];
        let override_zones = std::env::var("ZONES").ok();
        let zones: Vec<String> = override_zones.as_deref()
            .map(|z| z.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| DEFAULT_ZONES.into_iter().map(str::to_string).collect());
        // #849 review (non-blocking 2): the rollup's coverage line reports `1/1 zones` for a
        // ZONES-overridden run, which is a true statement about what was ATTEMPTED and a misleading
        // one about what the corpus IS — and the line above it says "across corpus". The accounting
        // added by #839 covers zones dropped inside the loop; it cannot see a corpus narrowed before
        // the loop starts. Say so in the output, where the reader of the number is.
        if let Some(z) = &override_zones {
            println!("\n*** CORPUS OVERRIDDEN by ZONES={z} — default is {DEFAULT_ZONES:?}. The WORST \
                      number below is NOT this corpus's headline figure and must not be quoted as \
                      one; `butcher` is the zone that sets it. ***");
        }
        println!("\n{:<12} {:>12} {:>12} {:>10}", "zone", "xy_cells@8u", "MAX_closed", "ms");
        let mut worst = 0usize;
        // #839: a missing `.glb` or an empty collision grid used to drop a zone through a bare
        // `continue` that touched no ledger, so a host missing one zone's asset silently shrank
        // "WORST reachable-component close across corpus" to whatever subset it had, with no trace
        // anywhere. `open_corpus_zone` is the SAME single-owner prologue #807's other five corpora
        // use; this corpus reports no water NUMBER, so it still closes through
        // `cover.add(zone, &zw.tally())` (a zero-valued tally) and the rollup's denominator stays
        // honest without a second accounting mechanism.
        //
        // **`open_corpus_zone` ATTACHES THE REGION MAP, AND THAT IS THE POINT — it is not incidental
        // to this measurement, it is what makes the measurement about production** (#849 review,
        // blocking finding). The body below contains no `in_water` call, but `astar` gates whole
        // edge families on `self.region_map()` being `Some` — water descent, haul-out/ascent,
        // surface crossing, floating-start anchoring — so with a region map attached the "reachable
        // component" this floods includes navigable water volume, and without one it does not.
        //
        // A draft of this comment claimed the corpus "has no water dependency at all". FALSE, and
        // load-bearing: the predicate actually measured was "zero `in_water` calls in the function
        // body", which does not entail "no water dependency" when the dependency lives inside the
        // `astar` this body calls.
        //
        // Which configuration is honest to measure is settled by `build_zone_collision`
        // (`src/app.rs`), the client's ONE production construction of a zone's collision grid: it
        // ALWAYS calls `set_region_data`. A grid with no region data attached is a configuration the
        // shipped client never builds, so a component size measured on one is a smaller number about
        // a grid that does not exist.
        //
        // MAGNITUDE, on `butcher`, MEASURED: with the region map attached **4,583,785** nodes;
        // without it 352,493. **13.0x.** The attached arm is a full-workload `dev`-profile run over
        // a `ZONES=butcher` corpus (10h24m, exit 0) — 57.3% of `MAX_NODES`, 1.75x headroom. An
        // independent earlier run reported 4,583,748 for this zone; that 37-node gap is unresolved
        // and is analysed once, on `MAX_NODES`'s own rustdoc — do not restate it here. Both
        // round to 57.3% and 1.75x, so the cap's conclusion is unaffected either way.
        //
        // The other measured points, kept because they bound how zone-dependent this is: on
        // `highpass` the close count went 7,229 -> 7,393 (+2.3%) — three orders of magnitude smaller
        // a move than butcher's. The two arms straddle #855 and only the mapped one has been
        // re-measured; `MAX_NODES`' own rustdoc carries that caveat once (#907) — do not restate it
        // here. And a ZERO move is not merely conceivable, it is OBSERVED: under this same
        // attachment two of the four converted corpora (`q1_headroom_seal_measurement`,
        // `floor_model_disagreement_scan`) printed BYTE-IDENTICAL tables either way. The size of the
        // effect is a property of the zone, not of the attachment.
        //
        // What DOES survive independently of any of that is the reason to attach: `build_zone_collision`
        // is the client's one production construction site and always attaches, so the pre-#839
        // reading was not "perturbed" by this change — it was measuring a grid the client never builds.
        let mut cover = eqoxide_zone_geometry::water_grid::WaterRollup::new();
        // #880 review (BLOCKING): the per-zone figure for `butcher` specifically, so this run can be
        // cross-checked against `MEASURED_WORST_BUTCHER_PRODUCTION`. `worst` is a max over the whole
        // corpus and butcher happens to set it today; that is a fact about the corpus, not something
        // to assert against a butcher-specific constant.
        let mut butcher_closed: Option<usize> = None;
        for zone in &zones {
            let (col, zw) = match eqoxide_zone_geometry::water_grid::open_corpus_zone(
                &mut cover, std::path::Path::new(&dir), zone, 32.0) {
                Ok(ready) => ready,
                Err(why) => { println!("{zone:<12}  ({why})"); continue }
            };
            let mut seed: u64 = 99;
            let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };
            // Sample several starts; force each to an OFF-MESH goal (far outside the grid, no floor)
            // so A* must close the whole reachable component. Take the largest closed_n seen.
            let (mut max_closed, mut max_ms) = (0usize, 0u128);
            let ext_e = col.cols as f32 * col.cell_size;
            let ext_n = col.rows as f32 * col.cell_size;
            // The four grid corners, on real floor: a route to the FARTHEST reachable floor explores
            // the largest span of the component. Whether it reaches or closes, `closed_n` at a HIGH cap
            // is the worst-case number the node cap must clear.
            let corners = [(0.05, 0.05), (0.95, 0.05), (0.05, 0.95), (0.95, 0.95), (0.5, 0.5)];
            for _ in 0..120 {
                let e = col.origin[0] + (rnd() as f32 / u32::MAX as f32) * ext_e;
                let n = col.origin[1] + (rnd() as f32 / u32::MAX as f32) * ext_n;
                let Some(z) = col.nearest_floor(e, n, col.z_max, 10.0, 4000.0) else { continue };
                // corners + one fully-random goal, so we don't systematically miss a hard interior pair
                let rc = (rnd() as f32 / u32::MAX as f32, rnd() as f32 / u32::MAX as f32);
                let probes = corners.iter().copied().chain(std::iter::once(rc));
                for (fe, fn_) in probes {
                    let (ge, gn) = (col.origin[0] + fe * ext_e, col.origin[1] + fn_ * ext_n);
                    let Some(gz) = col.nearest_floor(ge, gn, col.z_max, 10.0, 4000.0) else { continue };
                    let t = Instant::now();
                    let (sr, _t) = search_tiered_for_test(&col, 
                        [e, n, z], [ge, gn, gz], eqoxide_core::physics::PLAYER_RADIUS, &[], 8.0, None, 0.0,
                        // #849: was a hardcoded `8_000_000`. The whole point of this corpus is to
                        // bound `MAX_NODES`, so it now reads the constant it is measuring against —
                        // a literal here can drift away from the constant and silently turn the
                        // measurement into one about a cap the client does not use.
                        PlanCtx { node_cap: Some(MAX_NODES), ..PlanCtx::default() });
                    let ms = t.elapsed().as_millis();
                    if sr.closed_n > max_closed { max_closed = sr.closed_n; max_ms = ms; }
                }
            }
            worst = worst.max(max_closed);
            if zone == "butcher" { butcher_closed = Some(max_closed); }
            let xy = (col.cols as f32 * col.cell_size / 8.0).ceil() * (col.rows as f32 * col.cell_size / 8.0).ceil();
            println!("{zone:<12} {:>12.0} {:>12} {:>10}", xy, max_closed, max_ms);
            cover.add(zone, &zw.tally()); // #839: CLOSE the zone — forgetting makes it `unaccounted`
        }
        println!("\nWORST reachable-component close across corpus: {worst} nodes");
        // #849: this line used to read "the coarse MAX_NODES backstop must be comfortably above
        // this (it is: 8M)" — an ADJECTIVE and a hardcoded restatement of the constant, where the
        // reader needs the ratio. "Comfortably" was written when this corpus measured a grid with
        // no region map attached and reported ~352k for butcher; the production configuration
        // floods a strictly larger component, and at the time that amount was NOT measured on
        // butcher — so "comfortably" WAS resting on an unknown, which is why it went. #859 has
        // since measured it: butcher floods 4,583,785 nodes with the region map attached, 13.0x the
        // mapless 352,493, leaving 1.75x headroom. So the adjective would now be defensible on the
        // numbers, and the line still does not use one — print the margin and let the reader judge
        // it, because a ratio stays true as the corpus grows and an adjective quietly stops being.
        let pct = worst as f64 * 100.0 / MAX_NODES as f64;
        let headroom = MAX_NODES as f64 / (worst.max(1)) as f64;
        println!("=> the coarse MAX_NODES backstop is {MAX_NODES} nodes: this corpus consumes \
                  {pct:.1}% of it, i.e. {headroom:.2}x headroom.");
        println!("   (a component LARGER than the cap is still HONEST — it reports \
                  Exhausted(NodeCap), \"I don't know\", rather than a false Unreachable — but it is \
                  less precise than the SearchClosed \"no\" this cap exists to preserve. See \
                  MAX_NODES' doc for that trade.)");
        // #839: `WaterRollup`'s Display leads with its water TOTAL. This corpus reports no water
        // number, so that total is a `tally()` zero by construction, not a finding — label it, or a
        // reader takes "0" for "0 wet things found here". #849: it does NOT mean the search ran dry
        // — `open_corpus_zone` attaches the region map and `astar`'s water edge families are live,
        // which is deliberate and is what production does. "Reports no water number" is the claim;
        // "has no water dependency" was the earlier claim and it was false.
        println!("zone coverage [leading 0 = water total; this corpus reports no water number — \
                  but it DOES search with the region map attached]: {cover}");
        // #849: the correctness condition this corpus exists to check, asserted rather than left to
        // a human reading a printed line and judging an adjective.
        //
        // **What this assert can and cannot catch, stated precisely.** The searches above run AT
        // `MAX_NODES`, so `closed_n` can never meaningfully exceed it — `astar` breaks with
        // `PlanLimit::NodeCap` once the cap is passed. This is therefore a TRUNCATION DETECTOR: it
        // fires when some zone's whole-component close actually hit the cap, which is exactly the
        // honesty bug `MAX_NODES` was sized to avoid. It is NOT a margin check and will not fire at
        // 50%, or at 90%. The early warning for margin erosion is the printed percentage above, not
        // this line — which is why the percentage is printed on every run rather than only asserted.
        //
        // Deliberately `<` against the cap and NOT against some invented safety factor: picking a
        // required margin is a production-constant decision, and inventing one here would be this
        // file asserting a policy nobody chose. Whether 8M should move is tracked as #856 rather
        // than settled by a test author. #856 was once blocked on the production-config figure for
        // `butcher`; #859 measured it at 4,583,785 (57.3% of the cap, 1.75x headroom), so what is
        // left there is the DECISION, not the measurement (see `MAX_NODES`' doc).
        assert!(worst < MAX_NODES,
            "#849: the worst measured whole-zone reachable-component close ({worst}) is at or above \
             MAX_NODES ({MAX_NODES}) — a legitimate whole-zone \"no route\" in this corpus now \
             truncates into Exhausted(NodeCap) instead of Unreachable(SearchClosed). That is still \
             honest but strictly less precise, and it is the condition MAX_NODES was sized to avoid.");
        assert!(cover.is_complete(),
            "#839: this run is not a complete corpus measurement — it covered {}/{} of the zones it \
             was asked for. unmeasured (the .wtr was read and did not load): {:?}; skipped (dropped \
             before the water check ran — no glb / no grid): {:?}; unaccounted (left the loop body \
             without reaching add or skip — a corpus WIRING bug, not an asset problem): {:?}. The \
             WORST number above is only over the zones that were actually measured.",
            cover.measured_zones(), cover.attempted_zones(),
            cover.unmeasured_zones(), cover.skipped_zones(), cover.unaccounted_zones());
        // ── #880 review, BLOCKING finding: cross-check the constant against the world ────────────
        //
        // `MEASURED_WORST_BUTCHER_PRODUCTION` is a hardcoded copy of THIS test's butcher figure, and
        // `MAX_NODES`' doc comment states two derived figures (57.3%, 1.75x) in prose. Before #880
        // the only fast guard on those was `max_nodes_headroom_claim_stays_true`, which compares the
        // constant to `MAX_NODES` — it cannot see that the constant has stopped describing the
        // client, and the review PROVED that by rebuilding it around the old "~7x" world (a claim
        // that was self-consistent with its own constant for its entire false life) and getting a
        // GREEN run. This test is the only thing in the tree that can re-derive the real number, and
        // it was *printing* it while asserting only `worst < MAX_NODES` — a drifted constant gave a
        // green run and a line a human had to notice. That is the exact "printed adjective vs
        // asserted fact" gap #849 spent its effort removing everywhere else in this same function.
        //
        // **What this closes and what it does not.** It closes it ONLY when this ~10h `#[ignore]`d
        // run is actually performed. Nothing in CI and nothing in `cargo test --lib` performs it, so
        // between runs the constant and the prose still rest on a measurement rather than a guard.
        // Anything that moves what `astar` admits on butcher invalidates them with no fast signal.
        //
        // The tolerance is not invented here: it is `butcher_headroom_claim_check`, the same
        // predicate the fast pin applies to the constant, so what is asserted is precisely "the
        // two-decimal prose in `MAX_NODES`' doc is still true of what this run just measured". Its
        // window is narrow (+742 / -3,785 around the pinned figure) and that is intentional; see
        // that fn's doc. For scale, the two independent butcher runs on record differ by 37 nodes.
        match butcher_closed {
            Some(fresh) => {
                let pinned = MEASURED_WORST_BUTCHER_PRODUCTION;
                let delta = fresh as i64 - pinned as i64;
                match butcher_headroom_claim_check(fresh) {
                    Ok((pct, headroom)) => println!(
                        "butcher cross-check: this run {fresh} ({pct:.4}% of MAX_NODES, \
                         {headroom:.4}x headroom) vs pinned MEASURED_WORST_BUTCHER_PRODUCTION \
                         {pinned}, delta {delta:+} — still reproduces MAX_NODES' 57.3%/1.75x."),
                    Err(why) => panic!(
                        "#880: this run's MEASURED butcher close no longer reproduces the figures \
                         tracked for it. {why}\n  this run: {fresh}; pinned constant: {pinned}; \
                         delta {delta:+}. The constant and MAX_NODES' prose describe the world as \
                         it was in #859 and this run says the world moved (an astar admission \
                         change, a butcher rebake, or a corpus edit are the usual causes). Update \
                         MEASURED_WORST_BUTCHER_PRODUCTION and every figure derived from it — do \
                         NOT relax this check."),
                }
            }
            None => {
                // #908: this assert's message used to say that a run which silently SKIPS `butcher`
                // "did not run" the cross-check and must not pass. It cannot catch that state, and
                // the reason is assertion ORDER in this same fn — traced, not argued:
                //
                //   * `butcher` drops (no glb / no grid) => `open_corpus_zone` records
                //     `cover.skip(zone, ..)` and returns `Err`, so `cover.add` never runs =>
                //     `cover.is_complete()` is false => the `#839` assert ABOVE fires first and
                //     execution never reaches this `match` at all. Reproduced in #880's round-2
                //     review with a `ZONES=` corpus of one nonexistent zone: the `#839` assert is
                //     the killer and this one is never evaluated.
                //   * `butcher` succeeds => `butcher_closed` is `Some`, so this arm is not taken.
                //   * a `ZONES=`-overridden run without `butcher` => `override_zones.is_some()` is
                //     trivially true, so the assert passes and proves nothing.
                //
                // The ONE state that reaches this line with `override_zones` unset is `DEFAULT_ZONES`
                // above edited to drop `butcher` — the corpus narrowed at SOURCE rather than a zone
                // dropped at RUNTIME. That is what the message now names. The assert is kept rather
                // than deleted (#880's "dead code presented as coverage" treatment) because that
                // state is genuinely reachable and worth a loud failure; what was dead was the
                // description, not the check.
                assert!(override_zones.is_some(),
                    "#908: this run produced no `butcher` figure and no ZONES override was set, so \
                     the cross-check against MEASURED_WORST_BUTCHER_PRODUCTION did not run. A \
                     RUNTIME drop of `butcher` cannot reach this line — the `#839` completeness \
                     assert above fires first — so what this reports is that DEFAULT_ZONES has been \
                     edited to no longer contain `butcher`. `butcher` is the zone that sets this \
                     corpus's number and pins MEASURED_WORST_BUTCHER_PRODUCTION; a default corpus \
                     without it is not a measurement of this constant. (zones asked for: {zones:?})");
                println!("butcher cross-check: SKIPPED — `butcher` is not in this ZONES-overridden \
                          run ({zones:?}), so MEASURED_WORST_BUTCHER_PRODUCTION was NOT verified \
                          against the world by this run.");
            }
        }
    }

    /// The production-config `butcher` whole-zone reachable-component close, MEASURED in #859:
    /// `dev` profile, `ZONES=butcher` corpus (`worst_case_reachable_component` above), full
    /// 120-start × 6-probe workload, 10h24m, exit 0 — **57.3% of `MAX_NODES`, 1.75× headroom**. An
    /// independent three-zone run (profile never captured) measured 4,583,748 for the same zone,
    /// 37 nodes apart for reasons tracked and NOT resolved as #860; both figures round to the same
    /// 57.3%/1.75×, so this constant (the run with a captured compile sentinel) is the one pinned.
    ///
    /// This is **not** re-derived by any fast test — the run that measures it takes ~10h and needs
    /// baked zone glbs. Two things check it, and they check different things:
    ///
    /// * `max_nodes_headroom_claim_stays_true`, on every `cargo test --lib`: that this constant,
    ///   `MAX_NODES` and `MAX_NODES`' prose are arithmetically consistent **with each other**. It
    ///   cannot tell that this constant has stopped describing the client (see its own doc — the
    ///   "~7×" claim it replaces was self-consistent for its whole life).
    /// * `worst_case_reachable_component` itself, when someone pays the ~10h: since #880 it asserts
    ///   its freshly measured `butcher` close against this constant through
    ///   `butcher_headroom_claim_check`, so a stale value makes that run RED instead of printing a
    ///   number a human has to notice. That is the only mechanism that compares this constant to the
    ///   world, and it only fires when the run happens.
    ///
    /// So: anything that changes what `astar` admits on `butcher` — `can_traverse`,
    /// `ground_continuous`, the ray-hit acceptance window, water-edge admission — or a butcher
    /// rebake, or a corpus edit, invalidates this constant with **no fast signal anywhere**. If you
    /// are landing such a change, this constant is one of the things it can silently falsify.
    const MEASURED_WORST_BUTCHER_PRODUCTION: usize = 4_583_785;

    /// **The ONE definition of what `MAX_NODES`' doc comment claims about `butcher`** — that a
    /// whole-zone close of `n` nodes is "57.3% of the cap, 1.75× headroom" — so the two checkers of
    /// that claim cannot drift apart from each other. `max_nodes_headroom_claim_stays_true` applies
    /// it to the PINNED constant on every `cargo test`; `worst_case_reachable_component` applies it
    /// to the FRESHLY MEASURED close, once per ~10h run. Duplicating the four literals at two sites
    /// would be the same defect class this whole pin exists for, one level down.
    ///
    /// `Ok` carries the recomputed figures (for printing); `Err` carries a message naming which of
    /// the two prose figures the input fails and what it recomputed instead.
    ///
    /// **The window this admits is narrow and OFF-CENTRE**, which is a property of two-decimal
    /// rounding checks and is stated rather than discovered: jointly the two bounds accept exactly
    /// `n ∈ [4,580,000, 4,584,527]` — **closed at BOTH ends** — i.e. **+742 / −3,785** around the
    /// pinned 4,583,785. The headroom bound is the tight side and sits at 94.3% of its own
    /// tolerance. That is deliberate — the check exists to say "the two-decimal prose above is still
    /// true", not "the number is roughly right" — but anyone re-measuring should expect a real move
    /// to land RED and should fix the constant and the prose, not the tolerance.
    ///
    /// **#909 — the low end is CLOSED, and this doc used to say it was open.** Both bounds are `>=`
    /// comparisons, so an input is accepted when its deviation is strictly BELOW the tolerance; at
    /// `n = 4,580,000` the percentage deviation is not the `0.05` it is algebraically, it is the f64
    /// value of `57.25 - 57.3`, `0.049999999999997158`, which is strictly below `0.05`. That
    /// endpoint is therefore ACCEPTED. Which end of a rounding window is open is exactly the kind of
    /// figure this file states in prose and nothing checks, so all four boundary points are now
    /// pinned by execution in `the_headroom_claim_window_is_closed_at_both_ends`.
    fn butcher_headroom_claim_check(measured: usize) -> Result<(f64, f64), String> {
        let pct = measured as f64 * 100.0 / MAX_NODES as f64;
        let headroom = MAX_NODES as f64 / measured.max(1) as f64;
        if (pct - 57.3).abs() >= 0.05 {
            return Err(format!(
                "MAX_NODES' doc comment states butcher consumes 57.3% of the cap; recomputed \
                 {pct:.4}% from a butcher close of {measured} and MAX_NODES={MAX_NODES}. Update the \
                 doc comment's prose (and the literal here) to match — do not just widen this \
                 tolerance."));
        }
        if (headroom - 1.75).abs() >= 0.005 {
            return Err(format!(
                "MAX_NODES' doc comment states 1.75x headroom over butcher; recomputed \
                 {headroom:.4}x from a butcher close of {measured} and MAX_NODES={MAX_NODES}. \
                 Update the doc comment before changing this assertion."));
        }
        Ok((pct, headroom))
    }

    /// **PIN for #856 — and read the next paragraph before trusting it.** #856 existed because a
    /// headroom figure was quoted in a doc comment (`MAX_NODES`, "~7× headroom") that was true of a
    /// grid the client never builds — a false claim that sat in a tracked file, contested by nobody,
    /// until someone re-measured it.
    ///
    /// **This test could NOT have caught that, and it cannot catch its recurrence.** Demonstrated by
    /// execution in #880's review, not argued: 8,000,000 / 1,121,438 = 7.13, so the "~7×" claim was
    /// *arithmetically consistent with its own constant* for its entire life. Rebuild this test
    /// around that world — `MEASURED_… = 1_121_438`, literals `14.0`/`7.13` — and it is GREEN. The
    /// old claim never drifted; it was self-consistent and wrong about the WORLD. What this test
    /// guards is exactly one thing: **that `MEASURED_WORST_BUTCHER_PRODUCTION`, `MAX_NODES` and the
    /// prose in `MAX_NODES`' doc comment stay arithmetically consistent with each other**, plus that
    /// `MAX_NODES` is still the literal #856 decided on. It has nothing to say about whether the
    /// pinned measurement still describes the client — change what `astar` admits (`can_traverse`,
    /// `ground_continuous`, the ray-hit acceptance window), rebake butcher, or widen the corpus, and
    /// this test stays green while the figures it checks go false.
    ///
    /// The only thing that can re-derive the measurement is `worst_case_reachable_component`
    /// (`#[ignore]`d, ~10h, needs baked zone glbs), and since #880 it **asserts** its fresh butcher
    /// close through `butcher_headroom_claim_check` rather than only printing it. So world-drift is
    /// caught **when that run is performed** — nothing in CI or a normal `cargo test` performs it.
    /// Between such runs the 57.3%/1.75× figures rest on a measurement, not on a guard.
    ///
    /// Mutation-checked — transcripts in the #856 PR description. `MAX_NODES` → 9,000,000 and a
    /// proportional scale of BOTH constants (which was green before #880's review) both go red on
    /// the decision pin; `MEASURED_… → 352_493` goes red on the percentage bound and
    /// `MEASURED_… → 4_586_000` on the headroom bound, so both arms of
    /// `butcher_headroom_claim_check` are reached by execution. And the pre-#849 world
    /// (`1_121_438` with `14.0`/`7.13`) is **green**, which is the paragraph above, re-measured
    /// rather than quoted.
    #[test]
    fn max_nodes_headroom_claim_stays_true() {
        // #880 review (non-blocking 1): the ratio checks below constrain only the RATIO — scaling
        // both constants together left this test AND the full lib suite green while falsifying
        // "THE DECISION (#856): stays at 8,000,000" three lines into `MAX_NODES`' doc comment, six
        // other sites in this file, two tracked docs, and a design spec that cites the literal by
        // source text with nothing behind it. A decision stated in a tracked file with no guard is
        // the shape this issue exists to stop, so the decision is pinned too. It is checked FIRST so
        // it is the assertion that fires, rather than a confusing ratio failure downstream.
        assert_eq!(MAX_NODES, 8_000_000,
            "MAX_NODES' doc comment records THE DECISION of #856 — it stays at 8,000,000. Changing \
             it is a production-constant decision that needs its own issue and its own measurement, \
             not a test edit. If that decision has genuinely been retaken: update this literal, \
             `MAX_NODES`' doc comment (the DECISION block and the figures above it), \
             MEASURED_WORST_BUTCHER_PRODUCTION's doc, docs/autonomous-play.md, \
             docs/collision-system.md, and docs/specs/2026-07-17-3d-water-volume-nav-design.md, \
             which cites the literal by source text.");
        // NOTE: there is deliberately no `assert!(measured < MAX_NODES)` here. An earlier revision
        // had one and both this doc and the PR body called it an independent truncation check; it
        // is not. The ratio check below already forces measured/MAX_NODES < 0.5735, asserts run in
        // order, and so no assignment of the two constants exists where that third assertion could
        // ever be the one to fire. Dead code presented as coverage is worse than no code.
        if let Err(why) = butcher_headroom_claim_check(MEASURED_WORST_BUTCHER_PRODUCTION) {
            panic!("{why}\n  (checked against the PINNED constant \
                    MEASURED_WORST_BUTCHER_PRODUCTION={}; this test does NOT re-measure it — only \
                    `worst_case_reachable_component` can.)", MEASURED_WORST_BUTCHER_PRODUCTION);
        }
    }

    /// **#909 — the ONE pin on WHICH END of `butcher_headroom_claim_check`'s window is open.**
    ///
    /// That fn's rustdoc states the accepted interval in prose. Before this test the prose said
    /// `(4,580,000, 4,584,527]`, open at the low end, and the low end is **closed**: both bounds are
    /// `>=` comparisons, so acceptance is "deviation strictly below tolerance", and at
    /// `n = 4,580,000` the percentage deviation is the f64 value of `57.25 - 57.3`,
    /// `0.049999999999997158`, strictly below `0.05`. A one-character prose slip about an interval's
    /// openness was invisible to every other test in this file, because nothing else ever feeds the
    /// predicate anything but the pinned constant.
    ///
    /// So the window is pinned by EXECUTION at all four boundary points — the last accepted and the
    /// first rejected value on each side — rather than restated. WHICH bound rejects each outer
    /// point is asserted too, so a change that moves the responsibility from one bound to the other
    /// cannot pass by coincidence, and the rustdoc's "the headroom bound is the tight side" is
    /// held with it.
    ///
    /// **What this does NOT do.** Like `max_nodes_headroom_claim_stays_true`, it constrains only the
    /// predicate's arithmetic against `MAX_NODES`; it says nothing about whether the pinned
    /// measurement still describes the client. And it is deliberately NOT a restatement of the
    /// prose: if the tolerances are legitimately retaken, this test is what goes red first, and the
    /// prose is then re-derived from it — not the other way round.
    #[test]
    fn the_headroom_claim_window_is_closed_at_both_ends() {
        const LAST_REJECTED_BELOW:  usize = 4_579_999;
        const FIRST_ACCEPTED:       usize = 4_580_000;
        const LAST_ACCEPTED:        usize = 4_584_527;
        const FIRST_REJECTED_ABOVE: usize = 4_584_528;

        for (n, which) in [(FIRST_ACCEPTED, "LOW"), (LAST_ACCEPTED, "HIGH")] {
            let verdict = butcher_headroom_claim_check(n);
            assert!(verdict.is_ok(),
                "#909: {n} is the {which} end of `butcher_headroom_claim_check`'s accepted window \
                 and that end is CLOSED, but it was REJECTED. The window has moved: re-derive the \
                 interval in that fn's rustdoc from the two tolerances, correct it there, and \
                 update this test — do NOT widen a tolerance to make this pass. (verdict: \
                 {verdict:?})");
        }

        // The outer points, each named with the bound that must be the one to reject it. A window
        // that accepted these would make the rustdoc's "+742 / −3,785" figure false.
        let below = butcher_headroom_claim_check(LAST_REJECTED_BELOW).expect_err(
            "#909: one below the window's closed low end must be REJECTED; accepting it makes the \
             −3,785 figure in `butcher_headroom_claim_check`'s rustdoc false");
        assert!(below.contains("57.3% of the cap"),
            "#909: the value one below the window must be refused by the PERCENTAGE bound — the low \
             end is that bound's side. It was refused by something else, so the two bounds have \
             swapped sides and the rustdoc's \"the headroom bound is the tight side\" is now false: \
             {below}");

        let above = butcher_headroom_claim_check(FIRST_REJECTED_ABOVE).expect_err(
            "#909: one above the window's closed high end must be REJECTED; accepting it makes the \
             +742 figure in `butcher_headroom_claim_check`'s rustdoc false");
        assert!(above.contains("1.75x headroom"),
            "#909: the value one above the window must be refused by the HEADROOM bound — that is \
             what makes it \"the tight side\" in the rustdoc. It was refused by something else: \
             {above}");

        // REACH CONTROL. The four points above are only meaningful if the pinned constant actually
        // sits INSIDE the window they bound. A window that had drifted wholesale would still pass
        // every assert above while describing an interval bracketing no real measurement.
        // Keep this a runtime reach control so Clippy does not reduce the intentionally pinned
        // measurement to a constant assertion. The contract is STRICT interior membership: either
        // accepted endpoint would satisfy the prose-rounding check but would no longer be bracketed.
        let pinned_measurement = std::hint::black_box(MEASURED_WORST_BUTCHER_PRODUCTION);
        assert!(pinned_measurement > FIRST_ACCEPTED && pinned_measurement < LAST_ACCEPTED,
            "#909: the pinned MEASURED_WORST_BUTCHER_PRODUCTION \
             ({pinned_measurement}) is not strictly inside the window this test pins \
             ([{FIRST_ACCEPTED}, {LAST_ACCEPTED}]), so the four boundary asserts above are about an \
             interval that no longer brackets the measurement they exist to bracket");
    }

    /// **THE #382 CORPUS MEASUREMENT.** Fine-tier route success and cost, OLD (inline, net-thread) vs
    /// NEW (`find_path_local`, off-thread), over real baked zones — because every previous tightening of
    /// nav has SEALED ZONES (the coarse capsule sweep cost −29% route success in akanon, see
    /// `path_clear`), so "it should be strictly better" is not good enough to ship on.
    ///
    /// Note the baseline is NOT a wall clock: #394 already deleted that. Both sides use the SAME
    /// node-capped search — the only difference #382 makes is WHERE it runs (net thread vs worker) and
    /// that `find_path_local` returns an HONEST `LocalOutcome` instead of a bare `Option`. So this gate
    /// proves the new API + off-thread move does not change which carrots get threaded.
    ///
    /// * **OLD** = `find_path_res(.., allow_partial: true, .., PlanCtx::net_tier())` — verbatim what
    ///   `navigation.rs` called inline on the network thread before this change.
    /// * **NEW** = `find_path_local(..)` — the same node-capped search, off the net thread.
    ///
    /// Both are run back-to-back on identical (start, carrot) pairs sampled the way production
    /// generates them: walk a real coarse route and take carrots `LOCAL_REACH` (24 u) ahead of points
    /// along it. Reports threaded-count, disagreements, and the timing distribution — the same
    /// per-tick cost that used to land on the net thread.
    ///
    /// ```text
    /// ZONE_DIR=~/.local/share/eqoxide/assets/models \
    ///   cargo test -p eqoxide-nav --lib fine_tier_corpus -- --ignored --nocapture
    /// ```
    /// SLICE-1 MEASUREMENT HARNESS (3D-water-volume nav design §5.4 / §11): for the gate zones, report
    /// the water-span grid's wet-column count, total span count, ESTIMATED MEMORY (bytes, design §5.4
    /// accounting model — a model estimate, not measured RSS) and BUILD COST (median wall-clock ms). These are the eager-vs-lazy build
    /// numbers that decide owner decision #5 (§5.3). `unbounded↓` counts candidate columns whose water
    /// had no queryable bottom (`bottom_z` == None) — a design-premise signal (§5.2), expected 0.
    ///
    /// Collision is built at the PRODUCTION zone-load cell size (32.0, `app.rs`) so the build cost is
    /// representative of what a real zone load would pay. Run:
    ///
    /// ```text
    /// ZONE_DIR=~/.local/share/eqoxide/assets/models \
    ///   cargo test -p eqoxide-nav --lib water_grid_budget_measurement -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires baked zone glbs + .wtr at $ZONE_DIR"]
    fn water_grid_budget_measurement() {
        use std::time::Instant;
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let zones: Vec<String> = std::env::var("ZONES").ok()
            .map(|z| z.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| vec!["halas".into(), "blackburrow".into(), "qcat".into()]);
        let body = crate::traversability::PLAYER_BODY;

        // #762: a zone whose `.wtr` did not load is not "0 wet columns" — it is UNMEASURED, and this
        // whole table is a water measurement.
        //
        // #807: the `.wtr` hole was the only one this run refused. A zone with no baked `.glb`, or
        // one whose glb built an empty grid, was dropped by a bare `continue` that touched no
        // ledger, so it left the denominator as well as the numerator and the table below read as a
        // completed budget measurement of whatever subset happened to be on the host. Opening the
        // zone now goes through `open_corpus_zone`, which owns all three drop paths and accounts for
        // each of them; closing it is the `cover.add` at the bottom of the loop, and a zone this
        // body abandons without reaching that line is recorded as `unaccounted` by the rollup
        // itself — no per-`continue` wiring to forget.
        let mut cover = eqoxide_zone_geometry::water_grid::WaterRollup::new();
        println!("\n{:<12} {:>10} {:>8} {:>12} {:>10} {:>11}",
            "zone", "wet cols", "spans", "est bytes", "build ms", "unbounded↓");
        for zone in &zones {
            let (col, zw) = match eqoxide_zone_geometry::water_grid::open_corpus_zone(
                &mut cover, std::path::Path::new(&dir), zone, 32.0) {
                Ok(ready) => ready,
                Err(why) => { println!("{zone:<12}  ({why})"); continue }
            };

            // BUILD COST: median of 5 timed builds (a warm-up build precedes them). The grid is
            // deterministic, so all builds are identical — we time the same work the net thread would.
            let mut grid = col.build_water_grid(&body); // warm-up
            let mut times: Vec<u128> = Vec::new();
            for _ in 0..5 {
                let t0 = Instant::now();
                grid = col.build_water_grid(&body);
                times.push(t0.elapsed().as_micros());
            }
            times.sort_unstable();
            let median_ms = times[times.len() / 2] as f64 / 1000.0;
            println!("{zone:<12} {:>10} {:>8} {:>12} {:>10.1} {:>11}",
                grid.wet_column_count(), grid.span_count(), grid.estimated_bytes(),
                median_ms, grid.unbounded_below_count());
            cover.add(zone, &zw.measure(|_| grid.wet_column_count()));
        }
        // #762/#807: this table is only a budget measurement for the zones it actually measured. A
        // run with a hole in it — of ANY of the three kinds — must not read as a completed
        // measurement, however clean the rows look.
        println!("\nwet columns: {cover}");
        assert!(cover.is_complete(),
            "#807: this run is not a water budget result — it measured {}/{} of the zones it was \
             asked for. unmeasured (the .wtr was read and did not load): {:?}; skipped (dropped \
             before the water check ran — no glb / no grid): {:?}; unaccounted (left the loop body \
             without reaching add or skip — a corpus WIRING bug, not an asset problem): {:?}. \
             Bake or fetch the missing assets and re-run.",
            cover.measured_zones(), cover.attempted_zones(),
            cover.unmeasured_zones(), cover.skipped_zones(), cover.unaccounted_zones());
    }

    #[test]
    #[ignore = "requires baked zone glbs at $ZONE_DIR"]
    fn fine_tier_corpus_route_success_and_cost() {
        use std::time::Instant;
        // #819: LOCAL_REACH and LOCAL_CELL used to be private copies of the literals below. Both
        // names are `pub const` in `steering` (LOCAL_REACH promoted by #818/#733; LOCAL_CELL was
        // already pub), so a change to either shared constant would have silently left this corpus
        // sampling the OLD number instead of the one production now uses. IMPORTED rather than
        // copied so the two cannot drift apart.
        //
        // LOCAL_BOUND is NOT imported here: unlike LOCAL_REACH/LOCAL_CELL, it has no promoted `pub`
        // home — `walker::Walker::drive_walk` still declares it as a private `const` local to that
        // function, verbatim `const LOCAL_BOUND: f32 = 40.0;` (#919: was a walker.rs line number,
        // exact at the commit that wrote it and rotted by later insertions above the const), the
        // same way every other site that needs it (steering.rs's own
        // `fixture_run` corpus, `tests/walker_sim.rs`) restates the literal with a comment pointing
        // back to walker.rs. So this is a private-copy-of-a-private-value, not the shape #819 is
        // about; there is nothing shared to import it from without a design decision this issue does
        // not make.
        use crate::steering::{LOCAL_CELL, LOCAL_REACH};
        const LOCAL_BOUND: f32 = 40.0; // walker.rs `drive_walk` (private there — see note above)

        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let zones: Vec<String> = std::env::var("ZONES").ok()
            .map(|z| z.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| vec![
                // A deliberately mixed corpus: the zone #382's own numbers came from (akanon), the
                // one whose route success the last nav tightening cost 29% (akanon again), a dense
                // city, a big outdoor zone, dungeons, and the gfaydark corner an earlier budget cut
                // broke.
                //
                // #849: this list used to be described as "a deliberately mixed **DRY** corpus", and
                // that word is now wrong in a way that matters. #839 routes every zone here through
                // `open_corpus_zone`, which attaches the zone's region map, so the water-nav
                // machinery is live for all ten of these zones — `butcher` and `everfrost` in
                // particular are not dry. Nothing about the SELECTION changed; the description was
                // never load-bearing for the selection, and it is corrected rather than deleted
                // because a future reader would otherwise use "DRY corpus" to conclude the corpus is
                // water-free by construction, which it is not and now visibly is not.
                //
                // **qcat is still deliberately NOT here**, and that reason is unchanged and is NOT
                // about dryness: it is confounded by known bugs (#423 walk-through-walls-into-water,
                // #329 spawn-pocket dead-end, and unimplemented water nav #359/#197), so a
                // route-success/cost number there is not clean evidence about this refactor. Note
                // the exclusion is about *confounded* zones, not *wet* ones — wet-but-unconfounded
                // zones are in the list and always were. Pass ZONES=qcat to look at it in isolation.
                "akanon", "blackburrow", "qeynos2", "gfaydark", "crushbone", "neriaka", "felwithea",
                "highpass", "everfrost", "butcher",
            ].into_iter().map(str::to_string).collect());

        // A seeded LCG: a failure here must be reproducible, and an unseeded sample is not evidence.
        let mut seed: u64 = 0x3820_0F1E;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };

        let (mut tot_old_ok, mut tot_new_ok, mut tot_pairs) = (0usize, 0usize, 0usize);
        let (mut tot_new_only, mut tot_old_only) = (0usize, 0usize);
        let mut old_us: Vec<u128> = Vec::new();
        let mut new_us: Vec<u128> = Vec::new();

        println!("\n{:<12} {:>6} {:>10} {:>10} {:>9} {:>9} {:>10} {:>10}",
            "zone", "pairs", "old ok", "new ok", "new-only", "old-only", "old mean", "new mean");
        // #839 prologue: `open_corpus_zone` is the single-owner corpus prologue — see
        // `worst_case_reachable_component` for the bare-`continue` accounting bug it replaced, and
        // for why attaching the region map is what makes a measurement about production. This
        // corpus reports no water NUMBER, so it still closes through `cover.add(zone, &zw.tally())`.
        //
        // MEASURED here (#849 review): over a 4-zone reduced-budget run this corpus produced
        // identical pair counts and success rates on both sides (23 pairs, 100%/100%). No change was
        // demonstrated, which is NOT the same as demonstrating there is none — the full 10-zone
        // default at full budget was not run on either side, and the attachment's effect is a
        // property of the zone (13.0x on `butcher`, +2.3% on `highpass`, exactly nothing on two of
        // the four converted corpora), so a figure from elsewhere predicts nothing here.
        let mut cover = eqoxide_zone_geometry::water_grid::WaterRollup::new();
        for zone in &zones {
            let (col, zw) = match eqoxide_zone_geometry::water_grid::open_corpus_zone(
                &mut cover, std::path::Path::new(&dir), zone, 32.0) {
                Ok(ready) => ready,
                Err(why) => { println!("{zone:<12}  ({why})"); continue }
            };

            // Sample (start, carrot) pairs the way production makes them: real coarse routes, carrots
            // 24u ahead along them. A carrot invented out of thin air would not be the question the
            // fine tier is actually asked.
            let mut pairs: Vec<([f32; 3], [f32; 3])> = Vec::new();
            let mut tries = 0;
            while pairs.len() < 240 && tries < 900 {
                tries += 1;
                // A random point on walkable floor...
                let e = col.origin[0] + (rnd() as f32 / u32::MAX as f32) * (col.cols as f32 * col.cell_size);
                let n = col.origin[1] + (rnd() as f32 / u32::MAX as f32) * (col.rows as f32 * col.cell_size);
                // Anchor the probe at the TOP of the zone and search down. NOT at the midpoint of
                // [z_min, z_max]: several zones (gfaydark, everfrost, butcher) carry invisible-boundary
                // art at z ~= -32768, which drags the midpoint 16,000 units below the world and made the
                // first version of this sampler find NO floor at all in exactly the big outdoor zones
                // that matter most here (gfaydark is the zone a tighter fine-tier budget once broke).
                let Some(z) = col.nearest_floor(e, n, col.z_max, 10.0, 4000.0) else { continue };
                let s = [e, n, z];
                // ...and a goal 120-400u away in a random direction. (Two INDEPENDENT random points in
                // a big outdoor zone are almost never mutually routable, which is why the first version
                // of this sampler produced zero pairs for gfaydark/everfrost/butcher and only 7 for
                // akanon — the very zone #382's numbers came from. A displaced goal is both far more
                // productive and a better model of what an agent actually asks for.)
                let ang = (rnd() as f32 / u32::MAX as f32) * std::f32::consts::TAU;
                let d = 120.0 + (rnd() as f32 / u32::MAX as f32) * 280.0;
                let (ge, gn) = (e + d * ang.cos(), n + d * ang.sin());
                let Some(gz) = col.nearest_floor(ge, gn, z, 400.0, 400.0) else { continue };
                // A real coarse route (8u, the off-thread contract), then carrots along it.
                let PlanOutcome::Route(route) = col.find_path_ex(
                    s, [ge, gn, gz], eqoxide_core::physics::PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker())
                    else { continue };
                if route.len() < 3 { continue; }
                // Take a spread of carrots along the route, not just its head, so the sample covers the
                // whole journey (corners, doorways, stairs) rather than only its easy first stride.
                for i in (0..route.len().saturating_sub(2)).step_by(3) {
                    if pairs.len() >= 240 { break; }
                    let from = route[i];
                    let Some(carrot) = crate::steering::carrot_along(&route, i, from, LOCAL_REACH)
                        else { continue };
                    pairs.push((from, carrot));
                }
            }
            // #849 review (non-blocking 1): **this is the ELEVENTH drop site**, and #839's recount
            // of "ten drop sites in these five loops" missed it — it is disclosed here rather than
            // left for the next reader to rediscover by running a red corpus.
            //
            // It has exactly the shape #839 is about: it printed a per-zone line and then vanished
            // from the denominator, so the `=== FINE-TIER CORPUS (N pairs) ===` total below covered
            // fewer zones than the corpus names, and said nothing. It is a genuinely different
            // DROP KIND from `open_corpus_zone`'s three — the assets loaded fine, the grid is real,
            // the `.wtr` is fine; the zone simply yielded no start/carrot pair to score. That is why
            // it is wired here at the call site rather than inside the prologue.
            //
            // **This is a real behaviour change, stated because it is not free:** `skip` fails
            // `is_complete()`, so a host where any corpus zone produces zero pairs now goes RED
            // where before it went green with a quietly smaller denominator. MEASURED (#849 review):
            // on the reviewer's host at reduced per-zone budget `akanon` yields zero pairs, and this
            // corpus therefore fails there — base passed, head fails. At full default budget on that
            // same host it does not. Both outcomes are correct reports; only the second is a passing
            // one, and a zero-pair zone is exactly the case where "23 pairs, 100%" was never a
            // statement about the corpus it named.
            if pairs.is_empty() {
                println!("{zone:<12}  (no routable pairs — skipped)");
                cover.skip(zone, "no routable pairs");
                continue;
            }

            let (mut old_ok, mut new_ok, mut new_only, mut old_only) = (0usize, 0usize, 0usize, 0usize);
            let (mut zo, mut zn) = (Vec::new(), Vec::new());
            for (s, c) in &pairs {
                // OLD: exactly the call navigation.rs made inline on the net thread (node-capped).
                let t0 = Instant::now();
                let old = col.find_path_res(*s, *c, eqoxide_core::physics::PLAYER_RADIUS, &[], true,
                    LOCAL_CELL, Some(LOCAL_BOUND), 0.0, PlanCtx::net_tier());
                let ot = t0.elapsed().as_micros();
                // The old API cannot say whether it reached the carrot — it returns partials as routes.
                // That is the #382 honesty gap. Reconstruct "reached" the way the walker had to: measure.
                let o_reached = old.as_ref().and_then(|p| p.last()).is_some_and(|w|
                    (w[0] - c[0]).hypot(w[1] - c[1]) <= LOCAL_CELL * 2.0);

                // NEW: the same node-capped search, off the net thread, with an outcome that says which
                // answer it is (threaded / no-way-through / exhausted).
                let t1 = Instant::now();
                let new = col.find_path_local(*s, *c, LOCAL_CELL, LOCAL_BOUND, LOCAL_CELL * 2.0);
                let nt = t1.elapsed().as_micros();

                if o_reached { old_ok += 1; }
                if new.threaded() { new_ok += 1; }
                if new.threaded() && !o_reached { new_only += 1; }
                if o_reached && !new.threaded() { old_only += 1; }
                zo.push(ot); zn.push(nt);
            }
            let mean = |v: &[u128]| if v.is_empty() { 0 } else { (v.iter().sum::<u128>() / v.len() as u128) as usize };
            println!("{zone:<12} {:>6} {:>9}  {:>9}  {:>8}  {:>8}  {:>8}us {:>8}us",
                pairs.len(), old_ok, new_ok, new_only, old_only, mean(&zo), mean(&zn));
            tot_pairs += pairs.len(); tot_old_ok += old_ok; tot_new_ok += new_ok;
            tot_new_only += new_only; tot_old_only += old_only;
            old_us.extend(zo); new_us.extend(zn);
            cover.add(zone, &zw.tally()); // #839: CLOSE the zone — forgetting makes it `unaccounted`
        }

        old_us.sort_unstable(); new_us.sort_unstable();
        let pct = |v: &[u128], p: usize| if v.is_empty() { 0 } else { v[(v.len() - 1) * p / 100] };
        let mean = |v: &[u128]| if v.is_empty() { 0 } else { (v.iter().sum::<u128>() / v.len() as u128) as usize };
        println!("\n=== FINE-TIER CORPUS ({tot_pairs} pairs) ===");
        println!("route success  OLD (inline, net-thread) : {tot_old_ok}/{tot_pairs} = {:.2}%",
            100.0 * tot_old_ok as f32 / tot_pairs as f32);
        println!("route success  NEW (off-thread worker)  : {tot_new_ok}/{tot_pairs} = {:.2}%",
            100.0 * tot_new_ok as f32 / tot_pairs as f32);
        println!("  NEW threads what OLD could not : {tot_new_only}");
        println!("  OLD threads what NEW could not : {tot_old_only}   <-- MUST BE 0 (a regression)");
        println!("cost/plan  OLD (inline/net-thread)  mean {}us  p50 {}us  p99 {}us  max {}us",
            mean(&old_us), pct(&old_us, 50), pct(&old_us, 99), old_us.last().copied().unwrap_or(0));
        println!("cost/plan  NEW  mean {}us  p50 {}us  p99 {}us  max {}us  (paid on the fine WORKER, not the net thread)",
            mean(&new_us), pct(&new_us, 50), pct(&new_us, 99), new_us.last().copied().unwrap_or(0));

        // #839: the accounting assert fires BEFORE `tot_pairs > 0` — a run that dropped every zone
        // is still RED either way, but this ordering names which zones dropped and why instead of
        // only saying the corpus produced no pairs.
        // #839/#849: the leading 0 is a `tally()` zero by construction, not a finding, and it does
        // NOT mean the search ran dry — same note as in `worst_case_reachable_component`.
        println!("zone coverage [leading 0 = water total; this corpus reports no water number — \
                  but it DOES search with the region map attached]: {cover}");
        assert!(cover.is_complete(),
            "#839: the FINE-TIER CORPUS numbers above are not this corpus — they cover {}/{} of the \
             zones it was asked for. unmeasured (the .wtr was read and did not load): {:?}; skipped \
             (dropped before the water check ran — no glb / no grid / no routable pairs): {:?}; \
             unaccounted (left the loop body without reaching add or skip — a corpus WIRING bug, not \
             an asset problem): {:?}",
            cover.measured_zones(), cover.attempted_zones(),
            cover.unmeasured_zones(), cover.skipped_zones(), cover.unaccounted_zones());
        assert!(tot_pairs > 0, "the corpus produced no pairs — check $ZONE_DIR");
        // THE REGRESSION GATE. Deleting a deadline can only ADD completed searches: any search that
        // finished inside 150ms finishes identically without it (A* is deterministic given the same
        // inputs; the deadline only ever ABORTS). So a route the old tier threaded and the new one
        // cannot is not a rounding difference — it is a real regression, and this must catch it.
        assert_eq!(tot_old_only, 0,
            "REGRESSION: {tot_old_only} carrots the OLD budgeted fine tier could thread and the new one \
             cannot. Deleting the wall clock must be monotone.");
        assert!(tot_new_ok >= tot_old_ok, "fine-tier route success must not go down");
    }

    // ─────────────────────── PR-D / D-1: support-axis drift (#375) fixtures ───────────────────────
    // These prove the support-axis bug and gate the fix. The RED-on-main test reproduces the LIVE qcat
    // wedge (the planner deletes the floor the controller stands on). The two #329 guards are GREEN on
    // main (the facing filter incidentally satisfies them) and MUST stay green through D-2, when the
    // facing-blind `is_standable` classifier must reject ceilings via headroom+anchoring instead.
    /// An UP-facing floor plane at height `z` over east [e0,e1] × north [-100,100] (`tri_nz > 0`, seen
    /// by `nearest_floor`). NB: the older `floor_band` helper's winding is actually *down*-facing — it
    /// is only ever used with facing-BLIND queries (`path_clear`/`line_clear`), so its facing never
    /// mattered; these PR-D fixtures DO care about facing, so they use these explicit helpers instead.
    ///
    /// ⚠️ **Correction (#727 round 6 review, non-blocking 1).** This line used to end "(both windings
    /// verified by \[a winding-sanity test\])". **No such test exists anywhere in the workspace** — it
    /// was the "winding-sanity companion" retracted at D-2 along with the open-air-ceiling fixture
    /// (see the RETRACTED note ~20 lines below), and the citation was left behind pointing at nothing.
    /// So the windings here are asserted by these helpers' own `normals`/`indices`, not verified by a
    /// test. Found by the mechanical citation scan added in this round, which is the point of it:
    /// a hand-maintained list of citations cannot catch a citation nobody remembered to list.
    fn floor_up(z: f32, e0: f32, e1: f32) -> MeshData {
        MeshData {
            positions: vec![[-100.0, z, e0], [100.0, z, e0], [100.0, z, e1], [-100.0, z, e1]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 2, 1, 0, 3, 2], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// A DOWN-facing ceiling plane at height `z` (`tri_nz < 0`, discarded by the facing filter).
    fn ceiling_down(z: f32, e0: f32, e1: f32) -> MeshData {
        MeshData {
            positions: vec![[-100.0, z, e0], [100.0, z, e0], [100.0, z, e1], [-100.0, z, e1]],
            normals: vec![[0.0, -1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// **#693 DIAGNOSIS PROBE (temporary): which edge family descends through the qcat street?**
    /// Live wedge: walker on the street at ~(-502.5, -103.4, z≈0), fine waypoint snapped to a
    /// stacked lower tier (z≈-25.75) at the same XY. This dumps the wedge column (floors + water),
    /// then runs a traced coarse plan from the street toward the aqueduct-depth goal and prints
    /// every accepted NON-WALK edge (fall / water descent / water entry) near the wedge.
    #[test]
    #[ignore = "requires the cached qcat glb + .wtr; #693 diagnosis probe"]
    fn probe_693_qcat_street_phantom_descent() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let zone = std::env::var("PROBE_ZONE").unwrap_or_else(|_| "qeynos".into());
        let za = ZoneAssets::from_glb(&std::path::Path::new(&dir).join(format!("{zone}.glb"))).unwrap();
        let mut col = Collision::build(&za, 32.0);
        // #762: `water loaded: false` used to be a printed aside; the probe then went on to report
        // in_water=false for every column as if it had checked. Refuse instead.
        eqoxide_zone_geometry::water_grid::ZoneWater::load(&std::path::Path::new(&dir).join("maps/water"), &zone)
            .install(&mut col)
            .unwrap_or_else(|e| panic!("{zone}: {e} — every water answer below would be fabricated (#762)"));
        println!("zone={zone} water loaded: {}", col.region_map().is_some());
        println!("grid: origin={:?} cols={} rows={} cell={} z=[{:.1},{:.1}] extent e=[{:.0},{:.0}] n=[{:.0},{:.0}]",
            col.origin, col.cols, col.rows, col.cell_size, col.z_min(), col.z_max,
            col.origin[0], col.origin[0] + col.cols as f32 * col.cell_size,
            col.origin[1], col.origin[1] + col.rows as f32 * col.cell_size);

        // 1) The wedge column and its surroundings: stacked tiers + water?
        // Try BOTH axis orders — the live report may be in server (y,x) order.
        for (x, y) in [(-502.5f32, -103.4f32), (-510.0, -103.4), (-495.0, -103.4),
                       (-502.5, -95.0), (-502.5, -110.0), (-480.0, -103.4), (-520.0, -103.4),
                       (-103.4, -502.5), (-103.4, -510.0), (-103.4, -495.0),
                       (-95.0, -502.5), (-110.0, -502.5)] {
            let floors = col.column_floors(x, y, 0.0, 25.0, 120.0);
            let surfaces = col.column_surfaces(x, y, 0.0, 25.0, 120.0);
            let wet: Vec<f32> = (0..30).map(|k| -(k as f32) * 4.0)
                .filter(|&z| col.in_water([x, y, z])).collect();
            println!("col ({x:7.1},{y:7.1}): floors={floors:?}");
            println!("    surfaces={surfaces:?}");
            println!("    water at z={:?}", wet);
        }

        // 1b) Where is the aqueduct OPEN (deep floor, no street above) vs STACKED? Maps the real
        // entrances near the wedge.
        let mut open = Vec::new();
        let mut stacked = 0usize;
        for gc in 0..(col.cols * 4) {
            for gr in 0..(col.rows * 4) {
                let x = col.origin[0] + (gc as f32 + 0.5) * col.cell_size / 4.0;
                let y = col.origin[1] + (gr as f32 + 0.5) * col.cell_size / 4.0;
                let fs = col.column_floors(x, y, 0.0, 25.0, 120.0);
                let deep = fs.iter().any(|&z| z < -20.0);
                let street = fs.iter().any(|&z| z > -8.0);
                if deep && !street { open.push((x, y)); }
                if deep && street { stacked += 1; }
            }
        }
        println!("stacked columns: {stacked}; open-deep columns: {}", open.len());
        for (x, y) in open.iter().filter(|(x, y)| (x + 502.5).hypot(y + 103.4) < 150.0) {
            println!("  OPEN deep near wedge: ({x:.0},{y:.0}) floors={:?}",
                col.column_floors(*x, *y, 0.0, 25.0, 120.0));
        }

        // 2) Traced plan: street start near the wedge, goal at aqueduct depth (the live ask).
        let start = [-480.0f32, -103.4, 0.0];
        let goal = [-520.0f32, -103.4, -28.0];
        let trace: crate::diagnostics::SearchTraceHandle = std::sync::Arc::new(
            std::sync::Mutex::new(crate::diagnostics::SearchTrace::with_budget(60_000)));
        let ctx = PlanCtx::worker().ensure_budget().with_trace(trace.clone());
        let (out, _tier) = crate::planner::plan_path_with_ctx(&col, start, goal, &[], 0.0, ctx);
        println!("outcome: {:?} route: {:?}", out.reason(), out.route().map(|r| r.len()));
        if let Some(r) = out.route() {
            for w in r { println!("  wp ({:7.1},{:7.1},{:7.2})", w[0], w[1], w[2]); }
        }
        let tr = trace.lock().unwrap();
        for (ci, call) in tr.calls.iter().enumerate() {
            for e in &call.edges {
                if let crate::diagnostics::EdgeVerdict::Accepted { kind } = e.verdict {
                    use crate::diagnostics::EdgeKind::*;
                    if !matches!(kind, Walk) {
                        let near = (e.from[0] - start[0]).hypot(e.from[1] - start[1]) < 60.0;
                        if near || (e.from[2] - e.to[2]).abs() > 8.0 {
                            println!("  call {ci}: {:?} ({:7.1},{:7.1},{:7.2}) -> ({:7.1},{:7.1},{:7.2})",
                                kind, e.from[0], e.from[1], e.from[2], e.to[0], e.to[1], e.to[2]);
                        }
                    }
                }
            }
        }
    }

    /// #693 live-follow-up probe: the fine local tier at the qeynos canal drop (-582,133)->(-566,136,-14).
    #[test]
    #[ignore = "requires the cached qeynos glb; #693 fine-tier canal-drop probe"]
    fn probe_693_fine_canal_drop() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let za = ZoneAssets::from_glb(&std::path::Path::new(&dir).join("qeynos.glb")).unwrap();
        let mut col = Collision::build(&za, 32.0);
        eqoxide_zone_geometry::water_grid::ZoneWater::load(&std::path::Path::new(&dir).join("maps/water"), "qeynos")
            .install(&mut col).expect("qeynos .wtr must load — the canal drop is a water feature (#762)");
        // columns across the drop
        for (x, y) in [(-582.0f32, 133.0f32), (-575.7, 144.4), (-570.0, 140.0), (-566.7, 136.4), (-566.0, 130.0), (-560.0, 130.0)] {
            println!("col ({x:6.1},{y:6.1}) floors={:?} surfaces={:?}",
                col.column_floors(x, y, 0.0, 25.0, 60.0), col.column_surfaces(x, y, 0.0, 25.0, 60.0));
        }
        let start = [-582.0f32, 133.2, 0.0];
        for carrot in [[-566.7f32, 136.4, -14.0], [-558.7, 126.9, -14.0], [-550.7, 120.4, -14.0]] {
            let out = col.find_path_local(start, carrot, 2.0, 40.0, 4.0);
            println!("find_path_local {:?} -> state={} reason={} steer_len={}",
                carrot, out.state(), out.reason(), out.steer().len());
        }
    }

    /// #700 blast-radius A/B, PLANNER level (fast — no controller). Seeds the SAME pairs as
    /// `walker_sim::descent_guard_blast_radius` and dumps per-pair `AB <zone> <i> routed maxdrop len`.
    /// Run on the gate-ON build and a gate-OFF build (revert the swept-drop gate), diff the AB lines:
    /// a pair that flips routed 1→0 is a route the fix removed; controller-drive only those to prove
    /// each removed a body-impassable edge (never a false no_path). ZONE_DIR + optional ZONES/PAIRS.
    #[test]
    #[ignore = "requires baked zone glbs at $ZONE_DIR; #700 planner-level blast-radius A/B"]
    fn fix_700_planner_ab_corpus() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let zones: Vec<String> = std::env::var("ZONES").ok()
            .map(|z| z.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| ["qeynos", "qcat", "halas", "akanon", "blackburrow", "qeynos2", "gfaydark",
                "crushbone", "neriaka", "felwithea", "highpass", "everfrost", "butcher", "cazicthule", "oasis"]
                .into_iter().map(str::to_string).collect());
        let pairs_per_zone: usize = std::env::var("PAIRS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
        let mut seed: u64 = 0x693A_11CE;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };
        let unit = |r: u32| r as f32 / u32::MAX as f32;
        let (mut g_pairs, mut g_routed, mut g_steep) = (0usize, 0usize, 0usize);
        // #839: this used to keep its OWN `Vec<String> unmeasured` for the `.wtr` path — a second,
        // parallel accounting scheme alongside `WaterRollup` (which every sibling corpus in this
        // family already uses), and one that only ever covered the third drop path: a missing
        // `.glb` printed "(no glb)" and a bare `continue` with no ledger entry at all, same as an
        // empty collision grid. Folded into the SAME rollup `open_corpus_zone` owns for the other
        // four corpora: `skip` (no glb / no grid) and the `.wtr`-load `unmeasured` bucket are both
        // its job now, so there is one accounting scheme in this function, not two.
        let mut cover = eqoxide_zone_geometry::water_grid::WaterRollup::new();
        for zone in &zones {
            // #762/#839: water is this corpus' PAIR FILTER (`in_water(s) || in_water(g) → skip`).
            // Without the region map the filter silently passes everything, so wet pairs get scored
            // as dry land and the AB numbers are about a different corpus than the one they claim —
            // `open_corpus_zone`'s DROP 3 refuses that zone as `unmeasured` rather than installing a
            // fabricated-dry region map.
            let (col, zw) = match eqoxide_zone_geometry::water_grid::open_corpus_zone(
                &mut cover, std::path::Path::new(&dir), zone, 32.0) {
                Ok(ready) => ready,
                Err(why) => { println!("AB {zone} {why}"); continue }
            };
            let (mut z_pairs, mut tries) = (0usize, 0usize);
            let mut z_wet = 0usize; // #839: the wet pairs this corpus's filter dropped — a REAL number
            while z_pairs < pairs_per_zone && tries < pairs_per_zone * 70 + 500 {
                tries += 1;
                let e = col.origin[0] + unit(rnd()) * (col.cols as f32 * col.cell_size);
                let n = col.origin[1] + unit(rnd()) * (col.rows as f32 * col.cell_size);
                let Some(z) = col.nearest_floor(e, n, col.z_max, 10.0, 4000.0) else { continue };
                let ang = unit(rnd()) * std::f32::consts::TAU;
                let d = 80.0 + unit(rnd()) * 320.0;
                let (ge, gn) = (e + d * ang.cos(), n + d * ang.sin());
                let Some(gz) = col.nearest_floor(ge, gn, z, 400.0, 400.0) else { continue };
                let (s, g) = ([e, n, z], [ge, gn, gz]);
                if col.in_water(s) || col.in_water(g) { z_wet += 1; continue; }
                z_pairs += 1; g_pairs += 1;
                match col.find_path_ex(s, g, eqoxide_core::physics::PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker()) {
                    PlanOutcome::Route(route) => {
                        g_routed += 1;
                        let maxdrop = route.windows(2).map(|w| w[0][2] - w[1][2]).fold(0.0f32, f32::max);
                        // steep = the route has a drop family the #700 gate can touch
                        let steep = route.windows(2).any(|w| {
                            let run = (w[1][0]-w[0][0]).hypot(w[1][1]-w[0][1]).max(0.01);
                            (w[0][2]-w[1][2]) > 8.0 && (w[0][2]-w[1][2])/run > MAX_WALK_GRADE
                        });
                        if steep { g_steep += 1; }
                        println!("AB {zone} {z_pairs} routed=1 maxdrop={maxdrop:.1} len={} steep={} s[{:.0},{:.0},{:.0}] g[{:.0},{:.0},{:.0}]",
                            route.len(), steep as u8, s[0],s[1],s[2], g[0],g[1],g[2]);
                    }
                    other => println!("AB {zone} {z_pairs} routed=0 reason={} s[{:.0},{:.0},{:.0}] g[{:.0},{:.0},{:.0}]",
                        other.reason(), s[0],s[1],s[2], g[0],g[1],g[2]),
                }
            }
            cover.add(zone, &zw.measure(|_| z_wet)); // #839: CLOSE the zone — forgetting makes it `unaccounted`
        }
        println!("AB_TOTAL pairs={g_pairs} routed={g_routed} steep_drop_routes={g_steep}");
        // #839: the accounting assert fires BEFORE `g_pairs > 0`, same as the other four corpora in
        // this family — it names which zones dropped and why instead of just saying no zones loaded.
        // #839: unlike the other four corpora in this family, this one DOES use water — as its
        // pair filter — so the leading number is a real count of wet start/goal pairs excluded.
        println!("wet start/goal pairs excluded by the water filter: {cover}");
        assert!(cover.is_complete(),
            "#839 (refs #762): the AB_TOTAL above is not this corpus — it covers {}/{} of the zones \
             it was asked for. unmeasured (the .wtr was read and did not load): {:?}; skipped \
             (dropped before the water check ran — no glb / no grid): {:?}; unaccounted (left the \
             loop body without reaching add or skip — a corpus WIRING bug, not an asset problem): \
             {:?}",
            cover.measured_zones(), cover.attempted_zones(),
            cover.unmeasured_zones(), cover.skipped_zones(), cover.unaccounted_zones());
        assert!(g_pairs > 0, "no zones loaded — set ZONE_DIR");
    }

    /// A vertical wall segment at constant `east`, spanning north `[n0,n1]` and height `[h0,h1]`.
    /// (`wall_east` spans the full north range; this leaves an authored gap.)
    #[cfg(test)]
    fn wall_east_seg(e: f32, n0: f32, n1: f32, h0: f32, h1: f32) -> MeshData {
        MeshData {
            positions: vec![[n0, h0, e], [n1, h0, e], [n1, h1, e], [n0, h1, e]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// **#700: the coarse tier must not commit to a steep drop the swept body cannot descend.**
    ///
    /// At the qeynos canal lip the COARSE walk-edge test (a centre RAY on the 8u grid — deliberately,
    /// so it does not reject narrow *corridors*, see [`Collision::edge_clear`]) ACCEPTS a ~14u lip
    /// step-down that the FINE 2u swept tier — and the real controller — REFUSE: the ray threads a
    /// notch in the lip the character's shoulders hit. Coarse then commits to a lip descent the fine
    /// stage cannot realize (`local_no_way_through`). A step-DOWN is a body-WIDTH question, not a
    /// corridor-selection one, so the fix re-validates a STEEP drop with the swept body on the coarse
    /// tier too (only drops steeper than `MAX_WALK_GRADE` — the band the walk edge otherwise skips
    /// for a descent — so gentle ramps/stairs are untouched).
    ///
    /// Synthetic repro: a 14u drop crossed by a wall with a body-narrow (3u) centre notch. The narrow
    /// north strip forces the only crossing cell to sit at the notch, which is ray-clear but
    /// swept-blocked. Coarse must refuse the drop. **Mutation check: delete the swept-drop gate in
    /// `astar` and coarse routes it → RED** (verified at authoring). The `_open_` sibling is the
    /// over-tightening guard.
    #[test]
    fn coarse_refuses_a_steep_drop_the_swept_body_cannot_descend() {
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        // Narrow north strip [-4,4] so the sole crossing cell centres at north 0 (grid-robust).
        let col = Collision::build(&ZoneAssets {
            terrain: vec![
                slab(0.0, -4.0, 4.0, -40.0, -3.0, true),     // top plateau, z=0 (edge at east -3)
                slab(-14.0, -4.0, 4.0, 3.0, 40.0, true),     // pit floor 14u below (edge at east 3)
                wall_east_seg(0.0, 0.8, 40.0, -14.0, 8.0),   // lip wall N half
                wall_east_seg(0.0, -40.0, -0.8, -14.0, 8.0), // lip wall S half → gap north[-0.8,0.8]
                                                             // (< body diameter 2.0 → body can't fit)
            ], objects: vec![], textures: vec![],
        }, 8.0);
        // Predicate level: the coarse centre ray threads the notch (the over-accept); the swept body
        // does not fit the 3u notch.
        let a = crate::traversability::Point::new([-4.0, 0.0], 0.0);
        let b = crate::traversability::Point::new([4.0, 0.0], -14.0);
        let coarse = crate::traversability::Traversability::new(&col, r, 8.0, 0.0, false);
        assert!(coarse.can_traverse_fast(a, b),
            "#700 premise: the coarse centre ray threads the body-narrow lip notch (the over-accept)");
        assert!(!coarse.can_descend_swept(a, b),
            "#700 premise: the swept body cannot descend the 3u notch — the fine tier's verdict");
        // Route level: after the fix coarse must NOT hand back a route over the notch.
        let route = col.find_path([-16.0, 0.0, 0.0], [16.0, 0.0, -14.0], r, &[], false);
        assert!(route.is_none(),
            "#700: coarse must refuse a steep drop the swept body cannot descend (the only ray-clear \
             crossing is the body-narrow notch). Revert the swept-drop gate → a route appears → RED. \
             got {route:?}");
    }

    /// **#700 over-tightening guard (the dominant risk — the #693 lesson).** A genuinely OPEN steep
    /// 14u drop the body CAN descend must STILL route after the swept-drop gate: the gate rejects only
    /// lips the shoulders hit, never open drops. Same scene as the sibling above but with NO lip wall.
    #[test]
    fn coarse_still_routes_a_genuinely_open_steep_drop() {
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        let col = Collision::build(&ZoneAssets {
            terrain: vec![
                slab(0.0, -4.0, 4.0, -40.0, -3.0, true),
                slab(-14.0, -4.0, 4.0, 3.0, 40.0, true), // fully open 14u drop, no wall
            ], objects: vec![], textures: vec![],
        }, 8.0);
        let a = crate::traversability::Point::new([-4.0, 0.0], 0.0);
        let b = crate::traversability::Point::new([4.0, 0.0], -14.0);
        let coarse = crate::traversability::Traversability::new(&col, r, 8.0, 0.0, false);
        assert!(coarse.can_descend_swept(a, b),
            "an OPEN steep drop must pass the swept body test — nothing for the shoulders to hit");
        let route = col.find_path([-16.0, 0.0, 0.0], [16.0, 0.0, -14.0], r, &[], false);
        assert!(route.is_some(),
            "#700 over-tightening guard: a genuinely open 14u drop the body CAN descend must still \
             route — the swept-drop gate must never reject an open drop");
    }

    /// #700 LIVE PIN (ignored — needs the cached qeynos glb): at the real canal lip the coarse
    /// planner must no longer COMMIT to the steep lip step-down that the fine/swept tier refuses.
    /// Before the fix `find_path` from the lip top returns a short route whose descent is a steep
    /// (>`MAX_WALK_GRADE`) >8u lip drop; after the fix it routes around the walkable ramp instead.
    /// Run (one line; `ZONE_DIR` points at the cached zone GLBs):
    /// `ZONE_DIR=<models-dir> cargo test -p eqoxide-nav --lib fix_700_coarse_agrees_with_fine_at_canal_lip -- --ignored --nocapture`
    #[test]
    #[ignore = "requires the cached qeynos glb; #700 canal-lip coarse/fine agreement pin"]
    fn fix_700_coarse_agrees_with_fine_at_canal_lip() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let za = ZoneAssets::from_glb(&std::path::Path::new(&dir).join("qeynos.glb")).unwrap();
        let mut col = Collision::build(&za, 32.0);
        eqoxide_zone_geometry::water_grid::ZoneWater::load(&std::path::Path::new(&dir).join("maps/water"), "qeynos")
            .install(&mut col).expect("qeynos .wtr must load — this pin is about a canal lip (#762)");
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        // From the lip top just above the covered aqueduct, to the tier below it.
        let route = col.find_path([-574.0, 140.0, 0.0], [-566.0, 140.0, -14.0], r, &[], false)
            .expect("the lower tier is reachable via the walkable ramp");
        let commits_steep_lip = route.windows(2).any(|w| {
            let run = (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]).max(0.01);
            let drop = w[0][2] - w[1][2];
            drop > 8.0 && drop / run > MAX_WALK_GRADE
        });
        assert!(!commits_steep_lip,
            "#700: coarse must not commit to the steep canal-lip step-down the swept/fine tier refuses \
             — it must route the walkable ramp. Route ({} wp): {route:?}", route.len());
    }

    /// **ACCEPTANCE TEST — the residual #329 band, owner-signed-off 2026-07-15 (review Fix D).**
    ///
    /// This is NOT a bug reproduction — it pins INTENDED behaviour. A flat DOWN-facing surface at
    /// mid-height with OPEN SKY above it (floor@0 + roof@10, nothing on top) IS admitted as STANDABLE
    /// ground. That is the unavoidable cost of facing-blind ground detection: this geometry is
    /// IDENTICAL to qcat's walkable −42.97 walkway (down-facing, open above, a floor below), so no
    /// per-surface rule can accept the qcat floor (the #375 fix) while rejecting this — the reviewer
    /// proved the two are indistinguishable, and the owner accepted the band.
    ///
    /// UPDATED (#639): the admission is of STANDABILITY (`column_floors` includes the surface), NOT of
    /// a complete ROUTE onto it. Where the surface floats above the floor with no walkable connection,
    /// A* no longer returns a route that snaps its final waypoint onto it — that appended final hop was
    /// a lie (the walker cannot climb the gap). See the assertions below; the reachability-layer
    /// refusal is exactly the escalation this note's last paragraph names.
    ///
    /// The two #329 cases that ARE still defended (see `close_roof_ceiling_is_rejected_by_headroom` and
    /// `qcat_pocket_nearest_floor_is_never_the_ceiling`):
    ///   * **far roof** — excluded by the caller's `ref_z ± window` (a character never queries at roof
    ///     height);
    ///   * **close roof** — a solid roof within `NAV_AGENT_HEIGHT` above → `headroom` rejects it.
    ///     Only the OPEN-TOPPED MID-HEIGHT band gets through, knowingly. If it ever bites a real zone, the
    ///     escalation is a connectivity/reachability mitigation (does a *route* lead the character onto it?
    ///     — a graph-level question), NOT a per-surface classifier rule, which the reviewer proved
    ///     impossible. Refs #375 / #329.
    ///
    /// This test exists so the accepted state is explicit and greppable: ceiling-as-tier in THIS band
    /// is DELIBERATE, not a regression — do not "fix" it with a per-surface heuristic.
    #[test]
    fn open_topped_midheight_surface_is_admitted_as_the_accepted_cost_of_facing_blindness() {
        // The reviewer's fixture: a floor at z=0 and a flat DOWN-facing surface at z=10, open sky above.
        let col = Collision::build(&ZoneAssets {
            terrain: vec![floor_up(0.0, -100.0, 100.0), ceiling_down(10.0, -100.0, 100.0)],
            objects: vec![], textures: vec![],
        }, 32.0);
        // Queried at its own level, the mid-height surface IS standable (facing-blind + open headroom) —
        // exactly as qcat's -42.97 walkway is. This is the accepted band.
        let floors = col.column_floors(0.0, 0.0, 10.0, 3.0, 20.0);
        assert!(floors.iter().any(|&z| (z - 10.0).abs() < 0.5),
            "ACCEPTED (#375, owner 2026-07-15): an open-topped mid-height down-facing surface IS \
             standable — indistinguishable from qcat's walkable inverted floor. Set: {floors:?}");
        // …but A* does NOT hand back a COMPLETE route CLAIMING to reach it (#639). The surface floats
        // 10u above the floor with no connecting geometry — a character on the floor cannot walk the
        // 10u vertical face onto it (STEP_UP = 2). Before #639 the planner reached the goal cell on the
        // z=0 floor (wrong tier → fallback) and then APPENDED the goal by snapping the final waypoint to
        // z=10 — a route that says "arrived at z=10" while the walker is stuck at z=0, the exact
        // agent-honesty lie #639 closes. The goal-append walk-edge check now refuses that final hop
        // (`goal_not_walkable`). This is precisely the "mitigate at the REACHABILITY layer" escalation
        // the #375 owner note names (a graph-level walk-edge check, NOT the per-surface classifier rule
        // proven impossible): the surface stays STANDABLE (the `column_floors` assert above is
        // unchanged) — it is only no longer reported as WALKABLE-TO across a gap the controller cannot
        // cross.
        let path = col.find_path([0.0, -20.0, 0.0], [0.0, 20.0, 10.0], eqoxide_core::physics::PLAYER_RADIUS, &[], true);
        assert!(path.is_none(),
            "#639: A* must NOT return a complete route onto a surface floating 10u above the floor with \
             no walkable connection — that route's appended final hop is a lie (walker stays at z=0). \
             The surface remains standable (classifier unchanged); only the un-walkable APPROACH is \
             refused, at the reachability layer, exactly as the #375 owner note anticipated.");
        // Contrast: the far-roof and close-roof cases ARE still rejected — see close_roof_ceiling_* and
        // qcat_pocket_*. Only THIS open-topped mid-height band is knowingly admitted (as STANDABLE).
    }

    /// **§C REVIEW CONTRACT (D-2, mutation-relevant): the destination is judged on its OWN column,
    /// never vetoed for being far from the source `ref_z`.** A large DROP's landing floor must be
    /// standable when the planner probes it from the SOURCE cell's height — otherwise the controlled-
    /// fall edge vanishes and nav can descend into a level it cannot leave. `is_standable` is a property
    /// of the destination surface's own column (flatness + headroom to the next solid above it); `ref_z`
    /// only *windows* which surfaces are in range, it does NOT gate standability. If a future edit made
    /// standability depend on `|surface_z − ref_z|` (the tight-anchoring mistake §C warns against — it
    /// also seals ramps, which A* climbs ~9.6u/cell at MAX_WALK_GRADE), this fixture fires.
    #[test]
    fn is_standable_judges_the_destination_on_its_own_column_not_the_source_z() {
        // High floor over east[-100,0] at z=0; low floor over east[0,100] at z=-40 — a 40u drop at east=0.
        let col = Collision::build(&ZoneAssets {
            terrain: vec![floor_up(0.0, -100.0, 0.0), floor_up(-40.0, 0.0, 100.0)],
            objects: vec![], textures: vec![],
        }, 32.0);
        // Probed from the SOURCE (high) floor's z=0 with a drop-sized `down` window: the landing floor
        // 40u BELOW ref_z must be standable. A tight `|surface_z − ref_z|` gate would wrongly veto it.
        let from_source = col.column_floors(20.0, 0.0, 0.0, 5.0, 60.0);
        assert!(from_source.iter().any(|&z| (z - (-40.0)).abs() < 0.5),
            "§C: the drop landing (z=-40) must be standable when probed from the SOURCE z=0 (set \
             {from_source:?}) — is_standable must judge the destination on its own column, not veto it \
             for distance from ref_z (that would delete the fall edge AND seal ramps).");
        // And the controlled-fall edge actually forms: A* routes the high floor → low floor.
        let path = col.find_path([-20.0, 0.0, 0.0], [20.0, 0.0, -40.0], eqoxide_core::physics::PLAYER_RADIUS, &[], true);
        assert!(path.is_some(), "A* must route the 40u drop (high floor → low floor); the fall edge must survive D-2");
    }

    /// **Q1 SEAL MEASUREMENT (#375, D-2 crux).** The owner's Q1: does the anchoring-first `headroom`
    /// re-delete *legitimately-standable inverted ledges*? For each inverted-art zone, sample surfaces
    /// facing-blind (the physical truth of what geometry exists), keep the ones a body fits on
    /// (`footprint_clear`), and split them by what `is_standable` decides:
    ///   * **RECOVERED** — `is_standable` accepts it (a floor the old facing filter would have deleted
    ///     for being down-facing, now correctly kept). This is the #375 win.
    ///   * **HEADROOM-REJECT** — `is_standable` rejects it because `headroom < NAV_AGENT_HEIGHT` (a
    ///     solid surface close above). **These are NOT all proven to be ceilings.** A mutation-checked
    ///     test (`close_roof_ceiling_is_rejected_by_headroom`) confirms real close-roof CEILINGS are
    ///     among them; and a sub-`NAV_AGENT_HEIGHT` space is correctly rejected as below body height
    ///     (a 5u body cannot stand in <5u clearance — `NAV_AGENT_HEIGHT = 5.0` matches the controller's
    ///     `foot+4` chest ray). But the corpus CANNOT distinguish a rejected ceiling from a fully
    ///     sealed pocket: route-success samples start/goal via `nearest_floor`, so a sealed pocket
    ///     yields no pairs there and is invisible to it — route-success holding is NOT proof every
    ///     reject is a ceiling. The seal MECHANISM is real (`floor@0 + roof@4` → `column_floors`
    ///     returns only `[4.0]`, deleting the real floor for <5u headroom). The `<5u` rejection is
    ///     defensible (below body height); we do NOT claim all rejects are ceilings.
    ///   * **STEEP-REJECT** — rejected for `|nz| < NAV_NEAR_HORIZONTAL` (a wall/steep slope A*'s grade
    ///     limit rejects anyway).
    ///
    /// ```text
    /// ZONE_DIR=~/.local/share/eqoxide/assets/models \
    ///   cargo test -p eqoxide-nav --release --lib q1_headroom_seal_measurement -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires baked zone glbs at $ZONE_DIR; the Q1 seal measurement (#375)"]
    fn q1_headroom_seal_measurement() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let zones: Vec<String> = std::env::var("ZONES").ok()
            .map(|z| z.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| ["highpass", "permafrost", "neriakc", "qcat"].iter().map(|s| s.to_string()).collect());
        let mut seed: u64 = 0x0155_EA1D;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };
        println!("\n{:<12} {:>10} {:>10} {:>12} {:>10}", "zone", "fits", "recovered", "headroom-rej", "steep-rej");
        // #839 prologue: the same single-owner `open_corpus_zone` as the other four corpora in this
        // family — see `worst_case_reachable_component` for the accounting bug it replaced and for
        // why the region map is attached. Without it, the Q1 numbers above read as a complete seal
        // measurement over whatever subset of `zones` happened to load. MEASURED (#849 review):
        // this corpus printed a BYTE-IDENTICAL table with and without the attachment.
        let mut cover = eqoxide_zone_geometry::water_grid::WaterRollup::new();
        for zone in &zones {
            let (col, zw) = match eqoxide_zone_geometry::water_grid::open_corpus_zone(
                &mut cover, std::path::Path::new(&dir), zone, 32.0) {
                Ok(ready) => ready,
                Err(why) => { println!("{zone:<12}  ({why})"); continue }
            };
            let (ext_e, ext_n) = (col.cols as f32 * col.cell_size, col.rows as f32 * col.cell_size);
            let (mut fits, mut recovered, mut head_rej, mut steep_rej) = (0usize, 0usize, 0usize, 0usize);
            let mut cols = 0;
            while cols < 12000 && fits < 8000 {
                cols += 1;
                let e = col.origin[0] + (rnd() as f32 / u32::MAX as f32) * ext_e;
                let n = col.origin[1] + (rnd() as f32 / u32::MAX as f32) * ext_n;
                // Every surface in the column, facing-blind (physical geometry).
                let surfs = col.column_surfaces(e, n, 0.5 * (col.z_min() + col.z_max),
                    0.5 * (col.z_max - col.z_min()) + 1.0, 0.5 * (col.z_max - col.z_min()) + 1.0);
                for &(z, nz) in &surfs {
                    if !col.footprint_clear(e, n, z, eqoxide_core::physics::PLAYER_RADIUS, 8) { continue; }
                    fits += 1;
                    // Recompute is_standable's verdict + reason for this surface.
                    if nz.abs() < NAV_NEAR_HORIZONTAL { steep_rej += 1; continue; }
                    // headroom to next SOLID above (facing-blind).
                    let mut head = f32::INFINITY;
                    for &(zz, _) in &surfs { if zz > z + 0.3 { head = head.min(zz - z); } }
                    if head < NAV_AGENT_HEIGHT { head_rej += 1; } else { recovered += 1; }
                }
            }
            println!("{zone:<12} {fits:>10} {recovered:>10} {head_rej:>12} {steep_rej:>10}");
            cover.add(zone, &zw.tally()); // #839: CLOSE the zone — forgetting makes it `unaccounted`
        }
        println!("\n=== Q1: 'recovered' = floor is_standable KEEPS (the #375 win). 'headroom-rej' = \
            rejected for a solid surface < {NAV_AGENT_HEIGHT}u above. A mutation-checked test confirms \
            close-roof CEILINGS are among these; sub-{NAV_AGENT_HEIGHT}u spaces are also rejected as \
            below body height (correct). Route-success held, BUT the corpus cannot distinguish a rejected \
            ceiling from a sealed pocket (it samples via nearest_floor, so a sealed pocket is invisible \
            to it) — this is NOT proof every reject is a ceiling. ===");
        // #839/#849: the leading 0 is a `tally()` zero by construction, not a finding, and it does
        // NOT mean the search ran dry — same note as in `worst_case_reachable_component`.
        println!("zone coverage [leading 0 = water total; this corpus reports no water number — \
                  but it DOES search with the region map attached]: {cover}");
        assert!(cover.is_complete(),
            "#839: the Q1 numbers above are not a complete seal measurement — they cover {}/{} of the \
             zones this run was asked for. unmeasured (the .wtr was read and did not load): {:?}; \
             skipped (dropped before the water check ran — no glb / no grid): {:?}; unaccounted (left \
             the loop body without reaching add or skip — a corpus WIRING bug, not an asset problem): \
             {:?}",
            cover.measured_zones(), cover.attempted_zones(),
            cover.unmeasured_zones(), cover.skipped_zones(), cover.unaccounted_zones());
    }

    /// **THE FLOOR-MODEL DISAGREEMENT SCAN — a corpus indicator for D-2 (#375).** Counts, over a zone
    /// corpus, points where the controller's floor model (`ground_below`, facing-blind) finds a
    /// standable-looking surface the planner's floor model (`column_floors`, facing-filtered) omits.
    /// That disagreement is the support-axis drift; the live qcat wedge is one instance of it.
    ///
    /// **HONEST LIMIT (read before trusting the number).** On `main` this is an **UPPER BOUND**, not a
    /// clean drift count. A simple facing-blind + footprint + headroom filter **cannot distinguish** a
    /// real inverted-art FLOOR the character stands on (qcat's −42.97 walkway) from a genuine
    /// ceiling/overhang UNDERSIDE that merely happens to have open space above it — and telling those
    /// two apart is *exactly what PR-D's `is_standable` (headroom AND anchoring) adds*. So on `main`
    /// this over-counts in ceiling-rich zones (a city like qeynos2 reads high not because half its
    /// floor is inverted, but because it has many undersides). The one zone that is a believable clean
    /// control is a big OPEN-terrain outdoor zone with few ceilings (**gfaydark ≈ 0.2%**). Treat the
    /// per-zone numbers as "how much does the floor model disagree here", not "how many real bugs."
    ///
    /// **Why this is still the right corpus signal for D-2.** After D-2 the planner and the controller
    /// call the SAME `is_standable`, so this disagreement is **0 by construction, in every zone** — a
    /// blunt but genuine regression gate (any nonzero after D-2 means the two sides did not actually
    /// unify). The SHARP gate — that D-2 makes the planner see qcat's real floor WITHOUT admitting
    /// ceilings — is the focused pair below (`qcat_support_floor_is_visible_to_the_planner`, RED→GREEN;
    /// `open_air_ceiling_*` / `qcat_pocket_*`, stay GREEN). This scan is the breadth check; those are
    /// the correctness gate.
    ///
    /// (Why not a swim-simulation scanner: the drift is a STATIC floor-model property — the two floor
    /// queries disagree whether or not anyone is swimming — so measuring it statically is deterministic
    /// and needs no buoyancy fidelity. A swim-capable dynamic scanner would only re-derive, less
    /// reliably, what this and the focused qcat fixture already pin.)
    ///
    /// ```text
    /// ZONE_DIR=~/.local/share/eqoxide/assets/models \
    ///   cargo test -p eqoxide-nav --release --lib floor_model_disagreement_scan -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires baked zone glbs at $ZONE_DIR; the D-2 floor-model-disagreement breadth signal (#375)"]
    fn floor_model_disagreement_scan() {
        const TOL: f32 = 1.5;      // a planner floor within this of the controller's ground = agreement
        const HEADROOM: f32 = 5.0;  // open space (to next SOLID surface) a standable spot needs above it
        const RADIUS: f32 = eqoxide_core::physics::PLAYER_RADIUS;
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        // The inverted-art zones #375 names, plus a few normal zones as a clean-art control (their
        // count should already be ~0 on main, and must stay 0 after D-2 — a regression guard).
        let zones: Vec<String> = std::env::var("ZONES").ok()
            .map(|z| z.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| vec![
                "qcat", "highpass", "permafrost", "neriakc",  // inverted-art zones (#375) — high on main
                "gfaydark",                                   // open outdoor: the believable clean control (~0)
                "qeynos2",                                    // ceiling-rich city: reads high (see HONEST LIMIT)
            ].into_iter().map(str::to_string).collect());

        let mut seed: u64 = 0x5044_0F7D; // distinct stream
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };

        let mut tot_pts = 0usize;
        let mut tot_drift = 0usize;
        println!("\n{:<12} {:>10} {:>12} {:>8}", "zone", "sampled", "drift-pts", "%");
        // #839 prologue: the same single-owner `open_corpus_zone` as the other four corpora in this
        // family — see `worst_case_reachable_component` for the accounting bug it replaced and for
        // why the region map is attached. Without it, the FLOOR-MODEL DISAGREEMENT total below read
        // as complete over whatever subset of `zones` happened to load. MEASURED (#849 review):
        // this corpus printed a BYTE-IDENTICAL table with and without the attachment.
        let mut cover = eqoxide_zone_geometry::water_grid::WaterRollup::new();
        for zone in &zones {
            let (col, zw) = match eqoxide_zone_geometry::water_grid::open_corpus_zone(
                &mut cover, std::path::Path::new(&dir), zone, 32.0) {
                Ok(ready) => ready,
                Err(why) => { println!("{zone:<12}  ({why})"); continue }
            };
            let ext_e = col.cols as f32 * col.cell_size;
            let ext_n = col.rows as f32 * col.cell_size;
            let (mut sampled, mut drift) = (0usize, 0usize);
            let mut cols_probed = 0;
            let zreach = (col.z_max - col.z_min()).max(1.0) + 10.0;
            // Enumerate every surface in a sampled column FACING-BLIND, by walking `ground_below` down
            // from the top — no planner model, no z-sampling artifact (the invisible-boundary art at
            // z≈−32768 that made uniform-z sampling miss gfaydark/akanon is simply never a standable
            // surface, so it is filtered out below rather than dominating the sample space).
            while cols_probed < 20000 && sampled < 6000 {
                cols_probed += 1;
                let e = col.origin[0] + (rnd() as f32 / u32::MAX as f32) * ext_e;
                let n = col.origin[1] + (rnd() as f32 / u32::MAX as f32) * ext_n;
                let mut top = col.z_max;
                for _ in 0..24 { // at most 24 surfaces per column
                    let Some(ground) = col.ground_below(e, n, top, zreach) else { break };
                    top = ground - 0.5; // next probe starts just below this surface
                    // Controller-standable filter: body fits + HEADROOM of open air above (to next SOLID
                    // surface). Excludes ceilings/roof-undersides — a character cannot stand there.
                    if !col.footprint_clear(e, n, ground, RADIUS, 8) { continue; }
                    if col.nearest_hit_t([e, n, ground + 0.5], [e, n, ground + HEADROOM]).is_some() { continue; }
                    sampled += 1;
                    let floors = col.column_floors(e, n, ground, 20.0, 20.0);
                    if !floors.iter().any(|&f| (f - ground).abs() <= TOL) { drift += 1; }
                }
            }
            let pct = if sampled > 0 { 100.0 * drift as f32 / sampled as f32 } else { 0.0 };
            println!("{zone:<12} {sampled:>10} {drift:>12} {pct:>7.2}%", );
            tot_pts += sampled; tot_drift += drift;
            cover.add(zone, &zw.tally()); // #839: CLOSE the zone — forgetting makes it `unaccounted`
        }
        println!("\n=== FLOOR-MODEL DISAGREEMENT: {tot_drift} / {tot_pts} standable-looking surfaces the \
            planner's floor model omits (UPPER BOUND — conflates inverted-art floor with ceilings on main; \
            see HONEST LIMIT). D-2 gate: → 0 by construction (both sides share is_standable). ===");
        // #839: the accounting assert fires BEFORE `tot_pts > 0` — a run that dropped every zone is
        // still RED either way, but this ordering names which zones dropped and why.
        // #839/#849: the leading 0 is a `tally()` zero by construction, not a finding, and it does
        // NOT mean the search ran dry — same note as in `worst_case_reachable_component`.
        println!("zone coverage [leading 0 = water total; this corpus reports no water number — \
                  but it DOES search with the region map attached]: {cover}");
        assert!(cover.is_complete(),
            "#839: the FLOOR-MODEL DISAGREEMENT total above is not a complete scan — it covers {}/{} \
             of the zones this run was asked for. unmeasured (the .wtr was read and did not load): \
             {:?}; skipped (dropped before the water check ran — no glb / no grid): {:?}; \
             unaccounted (left the loop body without reaching add or skip — a corpus WIRING bug, not \
             an asset problem): {:?}",
            cover.measured_zones(), cover.attempted_zones(),
            cover.unmeasured_zones(), cover.skipped_zones(), cover.unaccounted_zones());
        assert!(tot_pts > 0, "no points sampled — check $ZONE_DIR");
    }

    /// Deterministic offline reproduction of the qeynos2 path-following stalls reported on #2,
    /// using the REAL baked collision mesh. Point `ZONE_GLB` at the cached qeynos2 glb, e.g.
    /// `ZONE_GLB=~/.local/share/eqoxide/assets/models/qeynos2.glb cargo test -p eqoxide-nav --lib diagnose_qeynos2_stall -- --ignored --nocapture`
    #[test]
    #[ignore = "requires the cached qeynos2 glb at $ZONE_GLB"]
    fn diagnose_qeynos2_stall() {
        let p = std::env::var("ZONE_GLB").expect("set ZONE_GLB to the cached qeynos2 glb");
        let za = ZoneAssets::from_glb(std::path::Path::new(&p)).unwrap();
        let mut col = Collision::build(&za, 32.0);
        // Attach the zone's water map like production does (app.rs) — the earlier run of
        // this diagnostic skipped it and mis-reported the moat as having no water volume.
        let wtr_dir = std::path::Path::new(&p).parent().unwrap().join("maps/water");
        eqoxide_zone_geometry::water_grid::ZoneWater::load(&wtr_dir, "qeynos2").install(&mut col)
            .expect("qeynos2 .wtr must load — an earlier run of this diagnostic skipped it and \
                     mis-reported the moat as having no water volume, which is exactly #762");
        eprintln!("collision: from_collision_mesh={} grid {}x{} cell={} origin={:?}",
            col.from_collision_mesh, col.cols, col.rows, col.cell_size, col.origin);

        let probe = |label: &str, start: [f32; 3], goal: [f32; 3]| {
            let sf = col.nearest_floor(start[0], start[1], start[2], 20.0, 100.0);
            let gf = col.nearest_floor(goal[0], goal[1], goal[2], 20.0, 100.0);
            eprintln!("\n[{label}] start={start:?} floor={sf:?}  goal={goal:?} floor={gf:?}");
            // What find_path sees at the start CELL CENTER (8u nav grid) vs the exact start point —
            // if these differ, quantization snaps the char onto adjacent (elevated) geometry.
            const NAV_CELL: f32 = 8.0;
            let sc = (((start[0] - col.origin[0]) / NAV_CELL) as i32) as f32;
            let sr = (((start[1] - col.origin[1]) / NAV_CELL) as i32) as f32;
            let ccx = col.origin[0] + (sc + 0.5) * NAV_CELL;
            let ccy = col.origin[1] + (sr + 0.5) * NAV_CELL;
            eprintln!("  start cell center=({ccx:.1},{ccy:.1}) floor@refz={:?}  column={:?}",
                col.nearest_floor(ccx, ccy, start[2], 20.0, 100.0),
                col.column_floors(ccx, ccy, start[2], 20.0, 100.0));
            match col.find_path(start, goal, eqoxide_core::physics::PLAYER_RADIUS, &[], false) {
                Some(path) => {
                    eprintln!("  find_path: {} waypoints", path.len());
                    for (i, w) in path.iter().enumerate().take(6) {
                        eprintln!("    [{i}] ({:.1},{:.1},{:.1})", w[0], w[1], w[2]);
                    }
                    if path.len() > 6 { eprintln!("    ... last ({:.1},{:.1},{:.1})",
                        path.last().unwrap()[0], path.last().unwrap()[1], path.last().unwrap()[2]); }
                }
                None => eprintln!("  find_path: NONE (no route)"),
            }
        };

        // Case A — street-level corner wedge (Kessen). Both reported goals.
        probe("A1 corner-wedge 145u south", [256.4, 324.9, 0.0], [254.0, 180.0, 0.0]);
        probe("A2 corner-wedge 28u NE",     [256.4, 324.9, 0.0], [276.0, 305.0, 0.0]);
        // Case B — the char is in the qeynos2 moat at z=-16. The earlier "sealed pit / no water
        // volume" verdict was wrong on both counts: the zone DOES have water (delivered via
        // maps/water/qeynos2.wtr — this diagnostic just never attached it), and with the WATER
        // ASCENT nav edge the moat is exitable: swim up at the south end and haul out onto the
        // z=+1 bank (see B2), from which the city center is reachable (B5/B7). The original
        // street goal below still probes NONE because that west-gate strip is disconnected from
        // the city center even street→street (B6) — an unreachable GOAL, not a moat problem.
        probe("B moat → west-gate street (goal itself disconnected, see B6)", [-502.3, -141.3, -16.0], [-600.0, -141.0, -5.0]);
        let within = col.find_path([-502.3, -141.3, -16.0], [-490.0, -100.0, -16.0], eqoxide_core::physics::PLAYER_RADIUS, &[], false);
        eprintln!("  moat floor traversable (start → 40u north @z=-16): {}",
            within.map(|p| format!("{} waypoints", p.len())).unwrap_or_else(|| "NONE".into()));
        for gx in [-560.0f32, -580.0, -600.0] {
            let r = col.find_path([-502.3, -141.3, -16.0], [gx, -141.0, -8.0], eqoxide_core::physics::PLAYER_RADIUS, &[], false);
            eprintln!("  moat → street x={gx}: {}",
                r.map(|p| format!("{} waypoints", p.len())).unwrap_or_else(|| "NONE".into()));
        }
        eprintln!("  zone has a water volume: {}", col.region_map().is_some());

        // Moat exit scan: walk the whole moat water region and report every column where a
        // haul-out is geometrically possible (a neighbor floor within STEP_H of the water
        // surface and a clear chest ray from swim height). If this prints nothing, the
        // collision genuinely has no exit and the swim-up nav edge can't help.
        if let Some(w) = col.region_map().cloned() {
            let mut found = 0;
            let mut y = -260.0f32;
            while y < -20.0 {
                let mut x = -520.0f32;
                while x < -420.0 {
                    if w.is_water(x, y, -11.0) {
                        let mut surface = -16.0f32;
                        while surface < 40.0 && w.is_water(x, y, surface + 2.0) { surface += 2.0; }
                        for (dx, dy) in [(-8.0f32,0.0),(8.0,0.0),(0.0,-8.0),(0.0,8.0),(-8.0,-8.0),(8.0,8.0),(-8.0,8.0),(8.0,-8.0)] {
                            let (bx, by) = (x + dx, y + dy);
                            for nf in col.column_floors(bx, by, surface, 20.0, 4.0) {
                                if nf > surface + 20.0 || nf <= -15.0 { continue; }
                                let ray_z = surface.max(nf - 20.0);
                                if col.path_clear([x, y, ray_z + 3.0], [bx, by, nf + 3.0], eqoxide_core::physics::PLAYER_RADIUS) {
                                    eprintln!("  EXIT candidate: swim ({x:.0},{y:.0}) surface {surface:.1} -> floor {nf:.1} at ({bx:.0},{by:.0})");
                                    found += 1;
                                }
                            }
                        }
                    }
                    x += 8.0;
                }
                y += 8.0;
            }
            eprintln!("  moat exit candidates: {found}");
        }
        // Route segments: can A* reach the SE-bank exit, and does the bank connect onward?
        probe("B2 moat → SE bank (swim-up exit)", [-502.3, -141.3, -16.0], [-488.0, -244.0, 1.0]);
        probe("B3 SE bank → west street",         [-488.0, -244.0, 1.0],   [-600.0, -141.0, -8.0]);
        probe("B4 moat floor → SE moat water",    [-502.3, -141.3, -16.0], [-488.0, -236.0, -14.0]);
        probe("B5 SE bank → city center",         [-488.0, -244.0, 1.0],   [0.0, 0.0, 3.0]);
        probe("B6 west street → city center",     [-600.0, -141.0, -8.0],  [0.0, 0.0, 3.0]);
        probe("B7 moat → city center",            [-502.3, -141.3, -16.0], [0.0, 0.0, 3.0]);
    }

    /// #309: the Crushbone moat is NOT a one-way trap — a character on its bottom has a route back
    /// onto dry land from everywhere along it.
    ///
    /// #309 reports that a character which falls into the moat is stranded, because the ladders
    /// meant to let it climb out are non-functional. The ladder half is true and structural: there
    /// is no climbable-surface concept anywhere in this client, and nowhere for one to come from —
    /// a `.wtr` leaf's `special` distinguishes only dry / water / zone-line (`region_map.rs`), so
    /// EQ's own region data never flags a surface as climbable. Ladders are ordinary geometry.
    ///
    /// The stranding it predicts is what this test pins, and it does not happen — but NOT for the
    /// reason one would expect. Measured, not assumed: re-running this with
    /// `PLAYER_BODY.haul_out_up` forced to `-1000.0` (which rejects every water→land haul-out edge
    /// in `neighbors`) leaves all probes escaping, on longer routes. So the moat's exit is not the
    /// haul-out at all; it is ordinary walkable ground. The scan below names it — the lowest dry
    /// bank anywhere along the moat sits at `-0.71 u` relative to the waterline, i.e. a shallow
    /// shelf at/below the surface that the character simply walks out onto. The banks everywhere
    /// else are ~12 u above the water, far past `haul_out_up`, which is why the routes run to
    /// 170-290 waypoints: A* crosses the moat to the shallow end rather than climbing out in place.
    ///
    /// HONEST LIMITS.
    /// * This is the PLANNER's answer — a route exists. That the controller can follow it is a
    ///   separate contract, pinned by `p1_haul_out_admission_matches_controller_execution`
    ///   (`tests/walker_sim.rs`).
    /// * "Escapes" means a route to a goal the planner accepts. Goals are taken from geometry
    ///   first, and a column that fails those is retried against goals already PROVEN reachable
    ///   from the water elsewhere in this same run: hand-picked dry points are frequently rejected
    ///   `GoalNotWalkable` (the probe's floor model disagreeing with the planner's), and counting
    ///   that as a trap is a measurement artefact, not a finding. An earlier revision of this test
    ///   did exactly that and reported a false trap at (16,-40).
    ///
    /// `ZONE_GLB=~/.local/share/eqoxide/assets/models/crushbone.glb \
    ///   cargo test -p eqoxide-nav --lib crushbone_water_is_not_a_one_way_trap -- --ignored --nocapture`
    #[test]
    #[ignore = "requires the cached crushbone glb at $ZONE_GLB"]
    fn crushbone_water_is_not_a_one_way_trap() {
        let p = std::env::var("ZONE_GLB").expect("set ZONE_GLB to the cached crushbone glb");
        let za = ZoneAssets::from_glb(std::path::Path::new(&p)).unwrap();
        let mut col = Collision::build(&za, 32.0);
        let wtr_dir = std::path::Path::new(&p).parent().unwrap().join("maps/water");
        eqoxide_zone_geometry::water_grid::ZoneWater::load(&wtr_dir, "crushbone").install(&mut col)
            .expect("crushbone .wtr must load — without it the moat has no water volume at all (#762) \
                     and every probe below would pass VACUOUSLY against a dry zone");

        let e0 = col.origin[0];
        let n0 = col.origin[1];
        let e1 = e0 + col.cols as f32 * col.cell_size;
        let n1 = n0 + col.rows as f32 * col.cell_size;
        let w = col.region_map().cloned().expect("water map must be installed");
        let aabbs = w.water_region_aabbs((e0, e1, n0, n1, -5000.0, 5000.0));

        // Wet columns on an 8u lattice, each carrying its real surface plane.
        let mut wet: std::collections::HashMap<(i32, i32), f32> = std::collections::HashMap::new();
        for (ae, an, az) in aabbs.iter() {
            let mut n = an[0].max(n0);
            while n <= an[1].min(n1) {
                let mut e = ae[0].max(e0);
                while e <= ae[1].min(e1) {
                    let key = ((e / 8.0).round() as i32, (n / 8.0).round() as i32);
                    if wet.contains_key(&key) { e += 8.0; continue; }
                    // A z known to be inside this leaf. Leaves can be unbounded below (z from
                    // -5000), so clamp before taking the midpoint or the probe lands in the void.
                    let probe_z = 0.5 * (az[0].max(-500.0) + az[1].min(500.0));
                    if w.is_water(e, n, probe_z) {
                        if let Some(s) = w.surface_z(e, n, probe_z) { wet.insert(key, s); }
                    }
                    e += 8.0;
                }
                n += 8.0;
            }
        }
        assert!(wet.len() > 100,
            "crushbone must have a substantial water volume, found only {} wet columns — a near-empty \
             scan would make every escape assertion below vacuous", wet.len());

        // Flood-fill into connected bodies (4-connected, sharing a surface plane). The largest is
        // the moat; the rest are the river arms and the deep pools east of the keep.
        let mut body_of: std::collections::HashMap<(i32, i32), usize> = std::collections::HashMap::new();
        let mut bodies: Vec<Vec<(i32, i32)>> = Vec::new();
        for (&k, &s) in wet.iter() {
            if body_of.contains_key(&k) { continue; }
            let id = bodies.len();
            let (mut cells, mut stack) = (Vec::new(), vec![k]);
            body_of.insert(k, id);
            while let Some(c) = stack.pop() {
                cells.push(c);
                for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                    let nb = (c.0 + dx, c.1 + dy);
                    if body_of.contains_key(&nb) { continue; }
                    if let Some(&ns) = wet.get(&nb) {
                        if (ns - s).abs() <= 1.0 { body_of.insert(nb, id); stack.push(nb); }
                    }
                }
            }
            bodies.push(cells);
        }
        // Sort every body's cells, then the bodies themselves: the flood-fill walks a HashMap, so
        // without this the probe columns differ run to run. That is not cosmetic — the
        // non-deterministic revision of this test alternated green and red on the same tree.
        for c in bodies.iter_mut() { c.sort(); }
        bodies.sort_by_key(|c| (std::cmp::Reverse(c.len()), c[0]));

        // Nearest dry STANDABLE candidates: body fits, and open air above (so a ceiling or a roof
        // underside cannot pose as a bank).
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        const HEADROOM: f32 = 6.0;
        let dry_candidates = |e: f32, n: f32, surface: f32| -> Vec<(f32, [f32; 3])> {
            let mut out: Vec<(f32, [f32; 3])> = Vec::new();
            for ring in 1..=8i32 {
                let d = ring as f32 * 8.0;
                for (de, dn) in [(-d, 0.0f32), (d, 0.0), (0.0, -d), (0.0, d),
                                 (-d, -d), (d, d), (-d, d), (d, -d)] {
                    let (be, bn) = (e + de, n + dn);
                    for nf in col.column_floors(be, bn, surface, 60.0, 60.0) {
                        if w.is_water(be, bn, nf + 0.5) { continue; }   // submerged: not a bank
                        if !col.footprint_clear(be, bn, nf, r, 8) { continue; }
                        if col.nearest_hit_t([be, bn, nf + 0.5], [be, bn, nf + HEADROOM]).is_some() { continue; }
                        out.push((nf - surface, [be, bn, nf]));
                    }
                }
                if out.len() >= 4 { break; }
            }
            out.sort_by(|a, b| a.0.abs().partial_cmp(&b.0.abs()).unwrap_or(std::cmp::Ordering::Equal));
            out.truncate(4);
            out
        };

        let cells = &bodies[0];
        let surface = wet[&cells[0]];
        // Name the moat's actual exit: the lowest dry bank anywhere along it. A value at or below
        // 0 means land meets the waterline — a walk-out, needing neither a ladder nor a haul-out.
        let mut lowest: Option<(f32, [f32; 3], [f32; 2])> = None;
        for c in cells.iter() {
            let (e, n) = (c.0 as f32 * 8.0, c.1 as f32 * 8.0);
            for (lip, g) in dry_candidates(e, n, surface) {
                if lowest.is_none() || lip < lowest.unwrap().0 { lowest = Some((lip, g, [e, n])); }
            }
        }
        let (low_lip, low_at, low_from) = lowest.expect("the moat must border some dry standable land");
        eprintln!("moat: {} columns, surface {surface:.1}; LOWEST dry bank {low_lip:+.2} u at \
            ({:.0},{:.0},{:.1}), reached from water ({:.0},{:.0})",
            cells.len(), low_at[0], low_at[1], low_at[2], low_from[0], low_from[1]);
        assert!(low_lip <= crate::traversability::PLAYER_BODY.haul_out_up,
            "the moat's lowest bank is {low_lip:+.2} u above the waterline, past both the walk-out \
             (<= 0) and the haul-out cap ({}) — with no climb mechanic in this client that WOULD \
             strand a swimmer, which is exactly what #309 predicted",
            crate::traversability::PLAYER_BODY.haul_out_up);

        // The reported scenario: the character is at the BOTTOM of the moat, not bobbing at its
        // surface. Probe 12 columns spread through the body; every one must have a way out.
        // Pass 1 uses each column's own local geometry and banks the goals that worked.
        let step = (cells.len() / 12).max(1);
        let probes: Vec<[f32; 3]> = cells.iter().step_by(step).take(12).map(|c| {
            let (e, n) = (c.0 as f32 * 8.0, c.1 as f32 * 8.0);
            let bottom = w.bottom_z(e, n, surface - 1.0).unwrap_or(surface - 1.0);
            [e, n, bottom + 0.5]
        }).collect();
        let mut proven: Vec<[f32; 3]> = Vec::new();
        let mut retry: Vec<[f32; 3]> = Vec::new();
        for start in &probes {
            let cands = dry_candidates(start[0], start[1], surface);
            match cands.iter().find_map(|(lip, g)| match plan(&col, *start, *g, r) {
                PlanOutcome::Route(path) => Some((path.len(), *lip, *g)),
                _ => None,
            }) {
                Some((wp, lip, g)) => {
                    eprintln!("  bottom ({:>5.0},{:>5.0},{:>6.1}) depth {:>4.1}: ESCAPES via \
                        ({:.0},{:.0},{:.1}) [{lip:+.2} u], {wp} waypoints",
                        start[0], start[1], start[2], surface - (start[2] - 0.5), g[0], g[1], g[2]);
                    if !proven.contains(&g) { proven.push(g); }
                }
                None => retry.push(*start),
            }
        }
        // Pass 2: a column whose own neighbourhood offered no plannable goal is not stranded if it
        // can reach dry land that the water has already been shown to reach.
        let mut stranded = Vec::new();
        for start in retry {
            match proven.iter().find_map(|g| match plan(&col, start, *g, r) {
                PlanOutcome::Route(path) => Some((path.len(), *g)),
                _ => None,
            }) {
                Some((wp, g)) => eprintln!("  bottom ({:>5.0},{:>5.0},{:>6.1}): ESCAPES to a \
                    PROVEN goal ({:.0},{:.0},{:.1}), {wp} waypoints (its own neighbours were all \
                    GoalNotWalkable)", start[0], start[1], start[2], g[0], g[1], g[2]),
                None => {
                    eprintln!("  bottom ({:>5.0},{:>5.0},{:>6.1}): NO ROUTE OUT — not to its own \
                        neighbours, nor to any of the {} goals proven reachable from this water",
                        start[0], start[1], start[2], proven.len());
                    stranded.push(start);
                }
            }
        }
        assert_eq!(probes.len(), 12, "the moat must be large enough to spread 12 probes through");
        assert!(!proven.is_empty(), "no probe reached dry land at all — the run proved nothing");
        assert!(stranded.is_empty(),
            "#309: {} of {} probes from the bottom of the Crushbone moat (surface {surface:.1}) have \
             NO route back onto dry land — it is a one-way trap. The exit this test measured is a \
             walk-out at the moat's shallow end ({low_lip:+.2} u at ({:.0},{:.0})), NOT the haul-out \
             edge; if this went red, check whether that shelf is still walkable, whether the .wtr \
             still loads, and the floor model at the shallow end. Stranded at: {stranded:?}",
            stranded.len(), probes.len(), low_at[0], low_at[1]);
    }

    /// #259: a sunken, water-filled pit in the middle of an otherwise-open street. The pit floor
    /// is a legal one-way DROP from the rim (MAX_STEP_DOWN is generous) but climbing back out is
    /// capped at `WATER_EXIT_UP` (~2.5u above the water surface) — the water surface here sits
    /// 10u below the rim, so once in, there is no walkable way out. With the search radius bounded
    /// (mirroring the real `plan_path` fallback's local-tier cap once every full-route radius has
    /// failed), the far-side goal and any walk-around are both out of reach, forcing a genuine
    /// PARTIAL route — exactly the condition under which `best_toward` used to land inside the pit
    /// (closer by straight-line heuristic than any street cell) and drive the walker into the trap.
    #[test]
    fn find_path_does_not_drive_a_partial_route_into_a_sunken_water_pit() {
        let quad = |v: Vec<[f32; 3]>| MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // MeshData pos = [north, up, east]; Collision maps to world [east, north, up].
        // A street "frame" (four strips) around a 40x40 hole (east 80..120, north 80..120).
        let south = quad(vec![[0.0, 0.0, 0.0],   [0.0, 0.0, 200.0],   [80.0, 0.0, 200.0],   [80.0, 0.0, 0.0]]);
        let north = quad(vec![[120.0, 0.0, 0.0], [120.0, 0.0, 200.0], [200.0, 0.0, 200.0],  [200.0, 0.0, 0.0]]);
        let west  = quad(vec![[80.0, 0.0, 0.0],  [80.0, 0.0, 80.0],   [120.0, 0.0, 80.0],   [120.0, 0.0, 0.0]]);
        let east  = quad(vec![[80.0, 0.0, 120.0],[80.0, 0.0, 200.0],  [120.0, 0.0, 200.0],  [120.0, 0.0, 120.0]]);
        // Pit floor, 20u below the street, extending slightly under the frame's hole edges so the
        // rim cells find it as a column drop (not disconnected geometry).
        let pit = quad(vec![[70.0, -40.0, 70.0], [70.0, -40.0, 130.0], [130.0, -40.0, 130.0], [130.0, -40.0, 70.0]]);
        let assets = ZoneAssets { terrain: vec![south, north, west, east, pit], objects: vec![], textures: vec![] };

        let mut col = Collision::build(&assets, 8.0);
        // Water fills only the pit's own footprint, up to 30u below street level (z=-30) — far too
        // deep for either the dry grade limit or WATER_EXIT_UP to reach the z=0 rim from the
        // surface. Bounded to the pit's XY box (unlike `flat_below`, which is a global z-split) so
        // the "swim across the surface" edge can't misfire against the dry street elsewhere.
        col.set_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::box_below(70.0, 130.0, 70.0, 130.0, -30.0))));

        // Sanity: the pit is reachable (a legal drop) but NOT climbable back out — a genuine
        // one-way trap, the real structural bug independent of the fix under test. Asserted as the
        // HONEST outcomes (#356): the drop-in is a complete `Route`; the climb-out is a DEFINITIVE
        // `Unreachable` (frontier closed — no walkable exit), NOT an `Exhausted` "I gave up". A bare
        // `.is_none()` on the exit could not tell those apart and would pass on a cut-short search.
        assert!(matches!(plan(&col, [60.0, 100.0, 0.0], [100.0, 100.0, -40.0], 1.0), PlanOutcome::Route(_)),
            "the pit floor must be a legal drop from the rim (a complete Route), got {:?}",
            plan(&col, [60.0, 100.0, 0.0], [100.0, 100.0, -40.0], 1.0));
        assert!(matches!(plan(&col, [100.0, 100.0, -40.0], [60.0, 100.0, 0.0], 1.0), PlanOutcome::Unreachable { .. }),
            "the pit must have no walkable exit — a one-way trap (Unreachable), got {:?}",
            plan(&col, [100.0, 100.0, -40.0], [60.0, 100.0, 0.0], 1.0));

        // A full route DOES exist by going around the hole (north or south corridor) — confirms
        // this is a solvable street layout, not a sealed level.
        let start = [60.0f32, 100.0, 0.0];
        let goal  = [140.0f32, 100.0, 0.0];
        assert!(matches!(plan(&col, start, goal, 1.0), PlanOutcome::Route(_)),
            "a full route around the pit must exist when the search isn't radius-bounded (Route), got {:?}",
            plan(&col, start, goal, 1.0));

        // Radius-bound the search (as the real fallback does) so neither the goal nor the
        // walk-around is reachable — forcing a genuine PARTIAL route toward the goal.
        let path = col.find_path_res(start, goal, 1.0, &[], true, 8.0, Some(50.0), 0.0, PlanCtx::default())
            .expect("a partial route toward the goal should still make progress");
        let last = *path.last().unwrap();
        let last_wet = col.in_water(last);
        eprintln!("partial route: {} waypoints, last={last:?} in_water={last_wet}", path.len());
        assert!(!last_wet, "#259: a partial route must never end submerged in the pit — that \
            strands the walker in a one-way trap. last={last:?}");
    }
}


#[cfg(test)]
mod clearance_probe_is_not_lossy_885 {
    /// **#907: the `highpass` pair is written in two files, so pin them to each other.**
    ///
    /// The measurement lives in `MAX_NODES`' rustdoc AND is restated in `water_grid.rs`'s
    /// attachment-cost argument. Nothing tied them together, so #907's re-measurement corrected one
    /// file and left the other asserting the superseded figure — the tree then carried two numbers
    /// for one measurement. A reader has no way to tell which file is stale.
    ///
    /// Every statement of the pair must therefore name the SAME re-measured figure. Both literals
    /// are spelled through `concat!` so this guard is not itself a corpus hit — its corpus contains
    /// its own source, and a scanner that matches its own text can never report clean (#973).
    ///
    /// **Reach.** The tree writes this pair in more than one NOTATION, and round 2 of review caught
    /// this guard reading only the first of them:
    ///
    ///  * **spelling** — with and without the thousands comma. Commas are stripped before matching,
    ///    so both spellings are one token. Keying on the comma form alone left the ratio sentence
    ///    in `MAX_NODES`' own rustdoc unchecked, four lines below a site that WAS checked. (No
    ///    figure is written in this rustdoc: with commas stripped it would be a corpus hit, and the
    ///    guard reported itself as one on the run that added this paragraph.)
    ///  * **order** — the re-measured figure follows the baseline after an arrow (`A -> B`) in the
    ///    tables and the `water_grid.rs` prose, but PRECEDES it in the ratio sentence (`B/A`). Both
    ///    are checked, from the same expected value.
    ///
    /// **What the scan matches — stated as a token, not as coverage.** It reads the two files
    /// named in `corpus` and looks for the BASELINE figure spelled in ASCII digits with optional
    /// `,` separators (commas are stripped from each line first, so the comma and comma-less
    /// spellings are one token). Every occurrence it FINDS is classified arrowed, ratio or prose;
    /// arrowed and ratio sites carry the other half of the pair and are value-checked, while the
    /// prose site is the bare mapless mention, which names no pair to disagree with. The `3/1/1`
    /// totals are asserted because a scan reporting only exceptions cannot distinguish "nothing
    /// wrong" from "nothing looked at" — but they pin THAT SPELLING ONLY. A count over found
    /// occurrences has no term for one that never matched, so no total here is evidence about
    /// anything below.
    ///
    /// UNMATCHED — each measured GREEN with a deliberately-wrong pair planted in the rustdoc above:
    ///
    ///  * the baseline separated by `_`, by a plain space, by a thin space (`U+2009`), or by `.`.
    ///    The underscore form is not hypothetical in this file: the table's first row writes the
    ///    then-shipped cap as `MAX_NODES = 1_000_000`, so that notation is already in use in the
    ///    very rustdoc this guard reads.
    ///  * any sentence naming ONLY the re-measured figure. The scan is anchored on the baseline, so
    ///    such a sentence is not an occurrence at all — including both of them in `MAX_NODES`' "two
    ///    code states" paragraph, one of which exists precisely to give the superseded and
    ///    re-measured figures side by side.
    ///  * the pair restated in any file outside `corpus`. Corpus completeness is not asserted, and
    ///    cannot be from inside a guard whose corpus is a literal list.
    ///
    /// Widening a third time was considered in review round 3 and declined: a widening closes the
    /// notation that was just planted and says nothing about the next one. Naming the token is
    /// checkable by a reader against any sentence; claiming the corpus is covered is not.
    #[test]
    fn both_files_state_the_same_re_measured_highpass_figure() {
        // Commas stripped, so one spelling of each figure covers both (review round 2).
        let bare = |s: &str| s.replace(',', "");
        let baseline = bare(concat!("7,2", "29"));
        let re_measured = bare(concat!("7,3", "93"));
        let corpus = [("collision.rs", include_str!("collision.rs")),
                      ("water_grid.rs", include_str!("../../eqoxide-zone-geometry/src/water_grid.rs"))];

        let (mut arrowed, mut ratio, mut prose) = (Vec::new(), Vec::new(), Vec::new());
        for (file, src) in corpus {
            let lines: Vec<String> = src.lines().map(bare).collect();
            for (i, line) in lines.iter().enumerate() {
                let next = lines.get(i + 1).map(String::as_str).unwrap_or("");
                for (col, _) in line.match_indices(&baseline) {
                    let site = format!("{file}:{}", i + 1);
                    let before = &line[..col];
                    // The pair can wrap a rustdoc line break, so read into the next line too.
                    let after: String = format!("{} {next}", &line[col + baseline.len()..])
                        .chars().take(40).collect();
                    if let Some(a) = after.find("->").or_else(|| after.find('\u{2192}')) {
                        arrowed.push((site, digit_run(after[a..].chars())));
                    } else if let Some(head) = before.strip_suffix('/') {
                        let behind: String = digit_run(head.chars().rev()).chars().rev().collect();
                        ratio.push((site, behind));
                    } else {
                        prose.push(site);
                    }
                }
            }
        }

        for (site, figure) in arrowed.iter().chain(&ratio) {
            assert_eq!(figure, &re_measured,
                "{site} states the `highpass` pair as {baseline}/{figure}, but #907 re-measured the \
                 mapped arm at {re_measured}. Two tracked statements disagreeing about one \
                 measurement is the #907 defect itself — fix the site, do not relax this guard.");
        }
        assert_eq!(arrowed.len(), 3,
            "expected 3 arrowed statements of the pair (2 in collision.rs, 1 in water_grid.rs), \
             found {}: {arrowed:?}. If a site was added or deleted, weigh it — a scan that silently \
             stopped reaching them would otherwise pass by finding nothing.", arrowed.len());
        assert_eq!(ratio.len(), 1,
            "expected 1 ratio statement (`MAX_NODES`' rustdoc), found {}: {ratio:?}", ratio.len());
        assert_eq!(prose.len(), 1,
            "expected exactly 1 bare mention (the mapless arm at `MAX_NODES`), found {}: \
             {prose:?}", prose.len());
    }

    /// The first run of ASCII digits in `it`, as a string. Fed reversed for a look-BEHIND, so the
    /// caller reverses the result back.
    fn digit_run(it: impl Iterator<Item = char>) -> String {
        let mut seen_digit = false;
        let mut out: Vec<char> = Vec::new();
        for c in it {
            if c.is_ascii_digit() {
                seen_digit = true;
                out.push(c);
            } else if seen_digit {
                break;
            }
        }
        out.into_iter().collect()
    }

    // ── the wire contract ───────────────────────────────────────────────────────────────────────
    /// **The citation guard for this module** — STAYS-side twin
    /// (docs/specs/2026-09-21-agent-harness-separation-plan-zone-geometry.md, Task 2 / #32): see the
    /// SHARED-side twin of this fn (same name) in `eqoxide-zone-geometry`'s `collision.rs` for the
    /// rest of the originally-cited names and the full rationale for the split.
    #[test]
    fn every_885_test_name_cited_in_a_doc_comment_still_exists() {
        let _cited: &[fn()] = &[
            both_files_state_the_same_re_measured_highpass_figure,
        ];
    }

}
