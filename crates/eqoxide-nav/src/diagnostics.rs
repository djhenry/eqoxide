//! Nav diagnostics: the PUBLISHED debug snapshot (#608, under epic #607).
//!
//! # Publish, don't recompute
//!
//! This module is the single channel through which navigation exposes *what it actually decided*
//! to every diagnostic consumer — the renderer's depth-tested 3D overlay and the
//! `/v1/observe/nav_debug` HTTP endpoint. The old `src/hud.rs::draw_nav_debug` overlay re-raycast
//! the collision grid and re-ran the planner's clearance test to decide what to draw; it only
//! stayed truthful because the planner's `Body` was hand-bound into it (#358/#386). A viewer that
//! recomputes CAN disagree with the planner, and a visualization that disagrees with the planner
//! is a lie about the planner.
//!
//! Here, disagreement is unrepresentable instead:
//!
//! * the A* search RECORDS its own per-edge verdicts as it makes them ([`SearchTrace`], filled by
//!   `collision::astar` at the exact branch that accepts or rejects each edge — the same `continue`
//!   that skips a too-steep climb is what records `Rejected { reason: Grade }`);
//! * the walker publishes its ACTUAL committed route (`Walker::publish_debug` copies
//!   `Walker::path` — the very field it steers along, the #246 property);
//! * consumers receive an [`NavDebugSnapshot`] and render/serialize it VERBATIM. Neither consumer
//!   has access to the collision grid in its encoding path, so a "corrected" or re-derived view is
//!   not just discouraged — the encoder signatures cannot express it.
//!
//! # Honesty: absence means UNEVALUATED
//!
//! The snapshot carries only what the planner evaluated. A cell or edge that is absent from
//! [`SearchTrace`] was NOT evaluated — it must never be drawn (or reported) as walkable OR
//! blocked. An overlay that fills in gaps to look complete is the same lie class in pixels.
//! (Consumer tests pin this: nothing may be emitted for absent cells.)
//!
//! # Budget
//!
//! The trace is bounded ([`TRACE_EDGE_CAP`], shared across every A* call of one plan) so a
//! pathological whole-zone flood cannot balloon memory; hitting the cap sets
//! [`CallTrace::truncated`] — an explicit "recording stopped here", never a silent gap. Recording
//! happens on the planner WORKER thread (never the net thread), and per-tick publication is a
//! couple of small `Vec` clones — see the frame-rate numbers in PR #608's body.

use std::sync::{Arc, Mutex};

use serde::Serialize;

/// Maximum recorded edge evaluations per PLAN (shared across all its A* calls). A typical
/// city-zone plan evaluates a few thousand edges; the cap only bites on whole-zone floods, where
/// the first N edges (best-first order, so clustered along the corridor A* actually pursued) are
/// the diagnostically interesting ones anyway. ~36 B/edge ⇒ ≤ ~2 MB per plan.
pub const TRACE_EDGE_CAP: usize = 60_000;

/// What KIND of edge the planner accepted — which A* edge family emitted it. (The families are
/// documented in `collision.rs`'s search loop; each `Accepted` record is written at that family's
/// `heap.push`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Ordinary terrain-follow walk edge.
    Walk,
    /// Running-jump over a genuine floor gap (eqoxide#190).
    Jump,
    /// Controlled fall off a ledge (last-resort, directional).
    Fall,
    /// Teleport-pad graph edge (#403).
    Pad,
    /// Swim across a water surface (#191).
    SwimSurface,
    /// 3-DOF swim between interior water nodes (water design §6).
    SwimInterior,
    /// Vertical swim within one water column (dive/rise).
    SwimVertical,
    /// Land → water entry (wade or dive-in, design §7.1).
    WaterEntry,
    /// Descent into water past the normal step-down limit.
    WaterDescent,
    /// Water → land haul-out (the #359 contract).
    HaulOut,
}

/// WHY the planner rejected an edge — tagged at the exact branch that `continue`d. This is a
/// record of the decision the search MADE, not a later re-derivation: no extra geometry query runs
/// to produce it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// The neighbour column has no candidate floor at all in the step window.
    NoFloor,
    /// The candidate floor is more than the step-up limit above the current one.
    StepUp,
    /// The candidate floor is more than the step-down limit below the current one.
    StepDown,
    /// The climb's grade (rise/run) exceeds `MAX_WALK_GRADE` (eqoxide#212).
    Grade,
    /// The climb's AVERAGE grade passed, but the floor profile along the hop concentrates the
    /// rise into a local face taller than the controller can actually climb
    /// (`Collision::walk_profile_ok`, eqoxide#630). The average-over-the-hop grade check alone
    /// let a near-vertical 10–16u face "launder" itself into a legal slope — the longer diagonal
    /// run (~11.3u vs 8u) made the same face pass diagonally while failing orthogonally.
    LocalRise,
    /// The body-clearance test refused the edge (`Traversability::can_traverse_fast`, or a water
    /// family's swept `edge_clear`) — a wall, missing margin, or blocked swim band. The hot loop
    /// only knows the boolean; the finer wall/floor/water distinction lives on the COLD
    /// `Blockage` path (`PlanOutcome::Unreachable`), deliberately not re-run per edge here.
    Clearance,
    /// A water-family precondition refused the edge (e.g. the span/surface it needs is absent).
    Water,
    /// A water exit whose lip is above the swimmer's haul-out reach (#359).
    HaulOutTooHigh,
}

/// The planner's verdict on one evaluated edge.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum EdgeVerdict {
    Accepted { kind: EdgeKind },
    Rejected { reason: RejectReason },
}

/// One edge evaluation the search actually performed: `from` → `to` (world coords
/// `[east, north, floor_z]`), and what the planner decided about it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct EdgeEval {
    pub from: [f32; 3],
    pub to: [f32; 3],
    #[serde(flatten)]
    pub verdict: EdgeVerdict,
}

/// The edge evaluations of ONE A* call (one anchor attempt at one clearance tier). A plan makes
/// several calls (generous + minimum tier, char + cell-centre anchors, the StartIsolated
/// re-anchor ring); each records separately so an edge rejected at the generous clearance and
/// accepted at the minimum is visible as exactly that — two honest records — rather than a
/// contradiction.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CallTrace {
    /// The clearance radius this call searched at (`Tier::Preferred`/`Tier::Minimum` units).
    pub clearance: f32,
    /// Grid resolution (8 u coarse).
    pub cell: f32,
    /// Whether the start node was anchored at the character's exact position (vs its cell centre).
    pub char_anchor: bool,
    /// The edge budget ran out during this call: recording stopped, the SEARCH did not. An
    /// explicit "trace incomplete past here" — never a silent gap.
    pub truncated: bool,
    pub edges: Vec<EdgeEval>,
}

/// Every edge evaluation of one PLAN (`plan_path`), across all its A* calls, recorded by the
/// search itself as it ran. **Absence means unevaluated** — see the module docs.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SearchTrace {
    pub calls: Vec<CallTrace>,
    /// Half-open range `[start, end)` into `calls`: **the DECIDING call** — the one A* call whose
    /// `Search` result actually became the plan's returned outcome. Tier retries (a generous pass a
    /// minimum pass superseded), anchor retries, and ring retries that lost sit OUTSIDE this range,
    /// so a consumer drawing "the answer" never paints a losing pass's rejections over the route
    /// the walker is successfully walking (#615 review F4). Stamped by `plan_path_with_ctx` from
    /// the per-call id the search itself reported (`Search::trace_call`); falls back to the whole
    /// invocation's call range only when that id is unavailable.
    pub outcome_calls: (usize, usize),
    /// Remaining shared edge budget (not serialized — an internal bound, surfaced per call as
    /// `truncated`).
    #[serde(skip)]
    budget: usize,
    /// Per-call recording cap (half the original budget — see [`SearchTrace::with_budget`]).
    #[serde(skip)]
    call_cap: usize,
    /// Edges recorded into the CURRENT call (reset by `begin_call`).
    #[serde(skip)]
    cur_call_edges: usize,
    /// Scratch: the call id of the most recent `search_tiered` answer, reported by
    /// `find_path_ex_tiered` for `plan_path_with_ctx` to stamp into `outcome_calls`. Not part of
    /// the published record.
    #[serde(skip)]
    pub last_answer: Option<usize>,
}

impl SearchTrace {
    /// A trace with `budget` total edge records, and a PER-CALL cap of half that budget.
    ///
    /// The per-call cap is the #615-review F3 fix: with only a shared pool drawn down in call
    /// order, a whole-zone generous-tier flood consumed the ENTIRE budget and the minimum-tier
    /// call — the one that actually decides `no_path` — recorded zero edges, every time (the
    /// generous pass always runs first). Capping any single call at half the budget guarantees
    /// the second (deciding) call always has at least half the pool available — the same shape as
    /// `generous_node_cap`'s slice of the node budget.
    pub fn with_budget(budget: usize) -> Self {
        SearchTrace { budget, call_cap: (budget / 2).max(1), ..Default::default() }
    }

    /// Open a new per-call record. Called by `astar` at entry (so even a call that evaluates
    /// nothing — `NoGeometry`, an immediately-unwalkable goal — leaves an honest empty record).
    pub fn begin_call(&mut self, clearance: f32, cell: f32, char_anchor: bool) {
        self.cur_call_edges = 0;
        self.calls.push(CallTrace { clearance, cell, char_anchor, truncated: false, edges: Vec::new() });
    }

    /// Record one edge verdict into the current call, honoring the plan-wide budget AND the
    /// per-call cap (see [`SearchTrace::with_budget`]).
    #[inline]
    pub fn edge(&mut self, from: [f32; 3], to: [f32; 3], verdict: EdgeVerdict) {
        let Some(call) = self.calls.last_mut() else { return };
        if self.budget == 0 || self.cur_call_edges >= self.call_cap {
            call.truncated = true;
            return;
        }
        self.budget -= 1;
        self.cur_call_edges += 1;
        call.edges.push(EdgeEval { from, to, verdict });
    }

    /// Total recorded edges across all calls.
    pub fn edge_count(&self) -> usize {
        self.calls.iter().map(|c| c.edges.len()).sum()
    }

    /// Did ANY call's recording get cut short? Consumers must surface this — a truncated trace
    /// rendered without a marker reads its recording boundary as the planner's real frontier
    /// (#615 review F2), a wrong conclusion about where nav stopped looking.
    pub fn truncated(&self) -> bool {
        self.calls.iter().any(|c| c.truncated)
    }
}

/// Shared handle threaded through `PlanCtx` into every A* call of one plan. Locked ONCE per call
/// (not per edge) — see `collision::astar`.
pub type SearchTraceHandle = Arc<Mutex<SearchTrace>>;

// ─────────────────────────────── the per-plan debug record ───────────────────────────────

/// What one coarse plan DID: the question, the honest outcome, and the full edge trace. Built by
/// the walker from the worker's `PlanReply` — every field is a value the planner itself produced.
#[derive(Clone, Debug, Serialize)]
pub struct PlanDebug {
    /// The plan generation (monotonic per session).
    pub gen: u64,
    pub start: [f32; 3],
    pub goal: [f32; 3],
    /// `"route" | "unreachable" | "exhausted"` — which `PlanOutcome` variant came back.
    pub outcome: String,
    /// The machine-readable reason (`nav_reason` vocabulary: `route`, `search_closed`,
    /// `goal_not_walkable`, `search_node_cap`, …).
    pub reason: String,
    /// Waypoint count of the returned route/partial (0 for a definitive no).
    pub route_len: usize,
    /// How long the search took, on the worker thread.
    pub plan_ms: u64,
    /// The route only existed at MINIMUM clearance (`nav_tier` semantics, #378).
    pub tight: bool,
    /// The planner CHANGED the goal z (snapped to a floor / the water surface) — the
    /// `goal_z_snapped` honesty channel.
    pub goal_snapped: bool,
    pub trace: SearchTrace,
}

// ─────────────────────────────── pad knowledge (#543/#266, #607) ───────────────────────────────

/// What navigation KNOWS about one same-zone teleport pad. This is the agent's first memory
/// surface (#607): "not yet observed" is a first-class state, distinct from every answer, and a
/// wire-advertised destination is labelled as exactly that — advertised, NOT verified (#543: a
/// same-zone pad's true resolution cannot be verified from the wire; the owner-decided learning
/// loop will upgrade entries to the `Learned*` variants when it lands).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "knowledge", rename_all = "snake_case")]
pub enum PadKnowledge {
    /// Nothing known: the pad advertises no usable same-zone destination (e.g. the keep-position
    /// sentinel). Its true behaviour has never been observed.
    Unknown,
    /// The server ADVERTISED this same-zone destination and it passed the honesty gate
    /// (`resolve_teleport_pads`: footprint + destination on walkable floor) — so A* may route
    /// through it. Advertised is not verified: no observation confirms the pad actually lands
    /// there (#543).
    AdvertisedUsable { source: [f32; 3], dest: [f32; 3] },
    /// The server advertised a same-zone destination but the honesty gate REFUSED it (footprint or
    /// destination not on walkable floor) — the planner fabricates no edge for it.
    AdvertisedUnusable,
    /// Reserved for the #543 learning loop: one or more server-driven resolutions were OBSERVED to
    /// stay in this zone, landing at `dest`.
    ///
    /// PROVENANCE is part of the type from day one (#607 §3: every learned fact needs provenance
    /// and a defined invalidation rule, visible to the agent): `observations` = how many times
    /// this resolution was observed; `last_observed_ms` = unix-epoch ms of the most recent one.
    /// Invalidation rule (enforced by the #543 learning loop when it lands, stated here so the
    /// type carries the contract): a contradicting observation or a zone-geometry change resets
    /// the entry to `Unknown` — a stale learned value presented as fact is worse than the
    /// original unverifiable guess.
    LearnedSameZone { dest: [f32; 3], observations: u32, last_observed_ms: u64 },
    /// Reserved for the #543 learning loop: observed to actually cross zones. Same provenance +
    /// invalidation contract as [`PadKnowledge::LearnedSameZone`].
    LearnedCrossZone { target_zone: u16, observations: u32, last_observed_ms: u64 },
}

/// One pad's knowledge state, keyed by its DRNTP zone-point index.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PadDebug {
    pub index: i32,
    #[serde(flatten)]
    pub knowledge: PadKnowledge,
}

// ─────────────────────────────── live traversability probe ───────────────────────────────

/// A live sample of the traversability model around one standing point: the radial wall spokes
/// (the same rays `ClearanceField::wall_at` aggregates into the hug cost) and the footprint ring
/// (the same ring `occupy_wall_ok` consults), plus the two graded field values the planner's
/// margin/hug logic actually reads. Produced by `Collision::clearance_probe` — nav sampling its
/// OWN model at the walker's position; consumers draw the sample, never re-cast the rays.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClearanceProbe {
    /// Where the probe was taken `[east, north, floor_z]`. The sample is meaningless anywhere else.
    pub at: [f32; 3],
    /// 16 radial wall distances (units), CCW from +east, saturating at `cap`.
    pub wall_spokes: Vec<f32>,
    /// The spokes' saturation distance.
    pub cap: f32,
    /// 8 footprint-ring directions (CCW from +east): `true` = clear of walls at the player radius.
    pub footprint_ok: Vec<bool>,
    /// The ring's radius (the player's collision radius).
    pub footprint_radius: f32,
    /// The zone-lifetime clearance field's graded wall distance at this point — the value the hug
    /// cost and standing-room margin actually consult.
    pub field_wall: f32,
    /// The field's graded ground (ledge) distance at this point.
    pub field_ground: f32,
}

/// The swim state the walker acted on THIS tick (the same values that went into its `MoveIntent`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct WaterDebug {
    /// The walker drove a swim intent (`want_swim`).
    pub swimming: bool,
    /// The swim plane (`surface − float_depth`) it steered against, when floating.
    pub swim_plane: Option<f32>,
}

// ─────────────────────────────── the snapshot ───────────────────────────────

/// The one nav diagnostics snapshot (#608): everything a consumer may draw or report, published by
/// the walker. See the module docs for the honesty contract (absence = unevaluated).
#[derive(Clone, Debug, Serialize)]
pub struct NavDebugSnapshot {
    /// Monotonic publish counter — consumers cache their encoding against it.
    pub seq: u64,
    /// Whether the walker HAS a collision grid for this zone. `false` = no world model: nothing
    /// below is a claim about geometry (#579; the HTTP endpoint composes the richer `zone_assets`
    /// load-state object alongside).
    pub zone_model_loaded: bool,
    /// The walker's published nav state/reason at publish time (same values as
    /// `/v1/observe/debug`'s `nav_state`/`nav_reason`).
    pub nav_state: String,
    pub nav_reason: Option<String>,
    /// Player position when published `[east, north, up]` — **`None` when the position was not
    /// known at publish time** (fresh login before the first server position, a zone reset). Never
    /// a made-up `[0,0,0]`: a confident wrong position put the overlay's player marker 985 units
    /// from the character (#615 review F1), which is exactly the falsehood class this snapshot
    /// exists to remove.
    pub player: Option<[f32; 3]>,
    /// When this snapshot was published (monotonic). Not serialized — the HTTP layer computes
    /// `published_age_ms` from it AT READ TIME (the #343 discipline: never cache an age), so a
    /// consumer can always tell a stale snapshot from a fresh one.
    #[serde(skip)]
    pub published_at: std::time::Instant,
    /// The active `/goto` goal, if any.
    pub goal: Option<[f32; 3]>,
    /// **The walker's ACTUAL committed coarse route** (`Walker::path`, verbatim — the #246
    /// property). Never a recompute.
    pub committed_coarse: Vec<[f32; 3]>,
    /// The fine/local plan the walker is steering along (`Walker::local_path`, verbatim).
    pub committed_fine: Vec<[f32; 3]>,
    /// The last coarse plan's full record (outcome + per-edge trace). `None` until a plan runs.
    /// Survives route clears (it is the diagnostic OF a failure), cleared on zone change (it
    /// describes the old zone's geometry).
    pub plan: Option<Arc<PlanDebug>>,
    /// Same-zone teleport-pad knowledge, as of the last plan post (#543/#266/#403).
    pub pads: Vec<PadDebug>,
    /// Live clearance sample near the player (refreshed at a throttled cadence — `at` says where).
    pub clearance: Option<ClearanceProbe>,
    /// The swim state the walker acted on this tick.
    pub water: Option<WaterDebug>,
}

/// The published slot: walker writes, renderer + HTTP read. Same pattern as the old
/// `NavPathView`, which this subsumes (ONE published source for the committed route — two would
/// be a drift channel).
pub type NavDebugView = Arc<Mutex<Option<Arc<NavDebugSnapshot>>>>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget is shared across calls, bites exactly at the cap, and truncation is EXPLICIT.
    /// **The per-call cap (#615 review F3) reserves room for the DECIDING call**: a first
    /// (generous-tier) flood may record at most half the budget, so the second (minimum-tier,
    /// deciding) call can never be starved to zero by call order.
    #[test]
    fn trace_budget_is_shared_capped_per_call_and_truncation_is_explicit() {
        // Budget 4 → per-call cap 2.
        let mut t = SearchTrace::with_budget(4);
        t.begin_call(2.0, 8.0, true); // "the generous flood"
        for i in 0..10 {
            t.edge([0.0; 3], [i as f32; 3], EdgeVerdict::Accepted { kind: EdgeKind::Walk });
        }
        assert_eq!(t.calls[0].edges.len(), 2,
            "a single call may consume at most HALF the budget — the F3 reserve");
        assert!(t.calls[0].truncated, "hitting the per-call cap must be explicit");

        t.begin_call(1.0, 8.0, false); // "the deciding minimum pass"
        t.edge([0.0; 3], [20.0; 3], EdgeVerdict::Rejected { reason: RejectReason::Clearance });
        t.edge([0.0; 3], [21.0; 3], EdgeVerdict::Accepted { kind: EdgeKind::Walk });
        assert_eq!(t.calls[1].edges.len(), 2,
            "the deciding call must still have its reserved half of the budget");
        // Global budget (4) now exhausted: further records refuse, explicitly.
        t.edge([0.0; 3], [22.0; 3], EdgeVerdict::Accepted { kind: EdgeKind::Walk });
        assert_eq!(t.edge_count(), 4, "the global budget still bounds the total");
        assert!(t.calls[1].truncated, "the call that hit the global cap must say so — silence would be a gap that lies");
        assert!(t.truncated(), "the whole-trace flag consumers must surface (F2)");
    }

    /// The JSON encoding of a verdict is the tagged form consumers rely on ("verdict" +
    /// "kind"/"reason") — pinned so the endpoint's wire shape can't silently drift.
    #[test]
    fn edge_verdict_serializes_tagged() {
        let acc = serde_json::to_value(EdgeEval {
            from: [1.0, 2.0, 3.0], to: [4.0, 5.0, 6.0],
            verdict: EdgeVerdict::Accepted { kind: EdgeKind::Walk },
        }).unwrap();
        assert_eq!(acc["verdict"], "accepted");
        assert_eq!(acc["kind"], "walk");
        let rej = serde_json::to_value(EdgeEval {
            from: [0.0; 3], to: [0.0; 3],
            verdict: EdgeVerdict::Rejected { reason: RejectReason::StepUp },
        }).unwrap();
        assert_eq!(rej["verdict"], "rejected");
        assert_eq!(rej["reason"], "step_up");
    }

    /// Pad knowledge keeps "unknown" distinct from every answer, in the serialized form an agent
    /// reads (#607: "not yet observed" must never collapse into either answer).
    #[test]
    fn pad_unknown_is_distinct_from_advertised_and_learned() {
        let states = [
            PadKnowledge::Unknown,
            PadKnowledge::AdvertisedUsable { source: [0.0; 3], dest: [1.0; 3] },
            PadKnowledge::AdvertisedUnusable,
            PadKnowledge::LearnedSameZone { dest: [1.0; 3], observations: 1, last_observed_ms: 1_700_000_000_000 },
            PadKnowledge::LearnedCrossZone { target_zone: 2, observations: 1, last_observed_ms: 1_700_000_000_000 },
        ];
        let tags: Vec<String> = states.iter()
            .map(|k| serde_json::to_value(k).unwrap()["knowledge"].as_str().unwrap().to_string())
            .collect();
        let unique: std::collections::HashSet<&String> = tags.iter().collect();
        assert_eq!(unique.len(), states.len(), "every knowledge state must be distinguishable: {tags:?}");
        assert!(tags.contains(&"unknown".to_string()));
        // #607 §3: learned facts carry their PROVENANCE on the wire, from day one.
        let learned = serde_json::to_value(&states[3]).unwrap();
        assert_eq!(learned["observations"], 1, "a learned fact must say how often it was observed");
        assert!(learned["last_observed_ms"].is_u64(), "…and when it was last observed");
    }
}
