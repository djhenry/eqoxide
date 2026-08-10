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
    /// A descent edge (controlled fall / water descent / water-entry dive) whose landing lies
    /// beneath an INTERVENING solid surface in the destination column — a stacked lower floor
    /// tier under solid ground, not a real drop. A falling body stops at the first surface below
    /// it, so this landing is unreachable by descending here (`Collision::descent_corridor_clear`,
    /// #693 — the qeynos-street-over-aqueduct phantom descent).
    DescentBlocked,
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
    /// GOAL IDENTITY (#631, mirrors #349's `nav_goal_id`): the `NavStatus::goal_id` of the
    /// `/move/{goto,follow,zone_cross}` this plan was computed FOR — captured when the request was
    /// posted, so it is the command the plan answers, never a later one. This is the fix for gap 1:
    /// a `PlanDebug` survives route clears (it is the diagnostic OF a failure), so a stale plan from a
    /// SUPERSEDED goal keeps riding the snapshot after a `/stop` or a fresh goto. Without an identity
    /// on the plan itself, an agent reading `plan.gen`/`plan.outcome` beside the current
    /// `NavDebugSnapshot::goal_id` cannot tell the plan belongs to a PREVIOUS command — the plan
    /// masquerades as this command's outcome. When `PlanDebug::goal_id != NavDebugSnapshot::goal_id`
    /// the plan is a prior goal's record, not the current one's; equality means it IS this goal's plan.
    pub goal_id: u64,
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
    /// HORIZONTAL goal re-anchoring disclosure (#631 gap 2). The horizontal (east/north) distance,
    /// in units, from the goal the caller NAMED to the point the committed route actually ENDS at.
    /// `goal_snapped` (from #344) only ever covered the VERTICAL case; a route that does not reach
    /// the requested XY — an `Exhausted` partial that stops at its closest approach — left the agent
    /// with `goal_snapped: false` and no way to tell its named coordinates were not where the walker
    /// was headed (the #482 observation: a goto planned to a point 55u horizontally from the ask). A
    /// COMPLETE route ends exactly at the requested XY, so this is `0.0` for a real route to the goal;
    /// a nonzero value means "the destination I committed to is this far, horizontally, from the one
    /// you asked for." Measured from `self.path`, the SAME route the walker steers, so it cannot drift
    /// from what the character actually does.
    pub goal_offset: f32,
    pub trace: SearchTrace,
}

// ─────────────────────────────── pad knowledge (#543/#266, #607) ───────────────────────────────

/// What navigation KNOWS about one same-zone teleport pad. This is the agent's first memory
/// surface (#607): "not yet observed" is a first-class state, distinct from every answer, and a
/// wire-advertised destination is labelled as exactly that — advertised, NOT verified (#543: a
/// same-zone pad's true resolution cannot be verified from the wire; the owner-decided learning
/// loop will upgrade entries to the `Learned*` variants when it lands).
///
/// **There is deliberately no "verified same-zone" state.** Nothing the client can observe from
/// the wire promotes an advertisement into a verified destination, so no variant may claim one.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "knowledge", rename_all = "snake_case")]
pub enum PadKnowledge {
    /// Nothing known: the pad advertises no usable same-zone destination (e.g. the keep-position
    /// sentinel) **and** it has no standable footprint leaf either, so there is nothing the agent
    /// could act on. Its true behaviour has never been observed. (A sentinel pad the agent CAN
    /// stand on is [`PadKnowledge::AdvertisedSameZoneDeclined`] with `advertised_dest: None` — the
    /// footprint is a real, usable fact even when the arrival is not advertised at all.)
    Unknown,
    /// The server ADVERTISED this same-zone destination and it passed the honesty gate
    /// (`resolve_teleport_pads`: footprint + destination on walkable floor) — so A* may route
    /// through it. Advertised is not verified: no observation confirms the pad actually lands
    /// there (#543).
    ///
    /// **Unreachable while [`crate::walker::TRUST_ADVERTISED_SAME_ZONE_CROSSINGS`] is `false`** —
    /// which is the current, owner-decided policy. Kept because the variant states what "the
    /// planner is allowed to route through this" would mean; today every such pad is
    /// [`PadKnowledge::AdvertisedSameZoneDeclined`] instead.
    AdvertisedUsable { source: [f32; 3], dest: [f32; 3] },
    /// The server advertised this pad, but **this client's loaded map has no DRNTP region for that
    /// index at all** — a `.wtr`/map-data gap. There is nothing to point the agent at: no footprint,
    /// no position, nothing that can be walked to. The planner fabricates no edge for it.
    ///
    /// Strictly a verdict about what is (not) in the client's own map. It is **not** a verdict about
    /// the advertised destination, and it is not the #543 policy decline. Deciding "the agent cannot
    /// use this pad" from the advertised *destination*, or from whether anything inside the region
    /// is standable, would hide a pad the agent could actually take — see
    /// [`PadKnowledge::AdvertisedSameZoneDeclined`], which carries both of those as facts rather
    /// than using them as a reason to go quiet.
    AdvertisedUnusable,
    /// **The #543 disclosure state.** The pad is REAL and the agent CAN take it — its footprint is
    /// at `footprint`, a point this client measured in its own collision mesh and knows a character
    /// can stand on — but the client DECLINED to auto-route the walker through it, because whether
    /// entering it keeps you in-zone is **unverifiable from the wire** (see
    /// [`crate::walker::TRUST_ADVERTISED_SAME_ZONE_CROSSINGS`] for the mechanism).
    ///
    /// **One entry per pad INDEX, not per leaf.** A DRNTP region is baked as a BSP and a single
    /// index routinely has dozens of leaves (qeynos2 index 2: 58, measured live) — emitting one
    /// offer each buries the agent in near-identical points instead of informing it. So `footprint`
    /// is the leaf NEAREST the character (the actionable "walk here") and `footprint_count` says how
    /// many there are, which is what the multiplicity actually means to a caller.
    ///
    /// The honest reading, and the only one an agent may take: *a pad exists here; the server said
    /// it leads to `advertised_dest`, in this zone; the client does not know whether that is true,
    /// and does not remember where this pad landed last time.* Taking it is the agent's call, and
    /// only the agent's own observation after arriving establishes where it actually goes.
    AdvertisedSameZoneDeclined {
        /// The nearest point inside the pad's trigger region that this client measured as standable
        /// — the spot to TRY first. Nearest to the character, so it is re-picked as the character
        /// moves.
        ///
        /// **This is a candidate, not a guarantee.** Verified live (#660): walking to one leaf of
        /// qeynos2's pad fired nothing, while another leaf of the SAME pad crossed immediately. Two
        /// reasons, both real: the client's model of which points trigger can disagree with the
        /// server's, and `/goto` stops within its arrival tolerance, which can leave the character
        /// just outside a small region. So "nothing happened" does not mean the pad is inert — see
        /// `alternates`. Claiming otherwise here would be a fresh confident falsehood in the field
        /// this whole disclosure exists to make honest.
        ///
        /// **`None` = this client found no standable point inside the region at all.** Still not a
        /// reason to hide the pad: the region is genuinely there, and this probe is a model.
        footprint: Option<[f32; 3]>,
        /// How many separate standable spots this pad has in total (`0` exactly when `footprint` is
        /// `None`). Real pads have many — 58 for qeynos2's, measured live.
        footprint_count: usize,
        /// Up to 7 MORE standable spots, nearest-first, excluding `footprint` itself. Without these,
        /// `footprint_count: 58` tells the agent 57 other options exist while giving it no way to
        /// reach any of them — a count it cannot act on. Bounded because a pad's full leaf list is
        /// diagnostics, not an offer.
        alternates: Vec<[f32; 3]>,
        /// Where the pad's region is, nearest the character — reported even when nothing in it is
        /// standable, so a pad is never reduced to "somewhere in this zone".
        ///
        /// Precisely: the region's representative point **projected down onto the floor beneath it**
        /// (`Collision::find_zone_line_near`), not the raw region point. For a region that floats
        /// above the ground that is the ground UNDER it — which is where a character walking there
        /// would end up standing, and therefore the useful answer — but it is not the region's own
        /// position, and nothing here should be read as "the trigger is at this height".
        region_at: [f32; 3],
        /// The server's advertised arrival, **verbatim from the wire** (wire z datum, not the
        /// client's foot datum). `None` when the pad carries the keep-position sentinel, i.e. it
        /// advertises no arrival at all — which does not make the pad un-takeable, only unadvertised.
        advertised_dest: Option<[f32; 3]>,
        /// Where that advertisement lands on this client's floor model, when a floor exists in its
        /// column. `None` = no floor was found there. **`None` is not a reason to withhold the pad**
        /// — it is a fact about the ADVERTISEMENT, and the advertisement is the untrustworthy part.
        /// The agent can still walk onto `footprint` and see where it actually goes.
        advertised_dest_floor: Option<[f32; 3]>,
    },
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

/// One radial spoke's answer (#885).
///
/// **Why this is not an `f32`.** It used to be: `Collision::clearance_probe` seeded each spoke at
/// the cap and lowered it on a hit, so "nothing within the cap" and "geometry hit at exactly the
/// cap" left the identical number in the payload. Measured on constructed fixtures at the time of
/// the fix: an open floor and a body ringed by walls standing at exactly 4.0 u produced
/// byte-identical `[4.0; 16]` spoke vectors. Those are different facts — one is a LOWER BOUND, the
/// other is a distance — and a caller had no way to tell them apart.
///
/// So the saturated case is a variant, not a number. A consumer that wants a length to draw asks
/// [`SpokeReading::draw_len`] and gets the cap; a consumer that wants to *reason* has to look at
/// which variant it is.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpokeReading {
    /// Geometry was HIT this far along the spoke. A measured distance in units, `0 ..= cap`.
    Hit { at: f32 },
    /// Nothing was hit anywhere within `cap` along this spoke. This is "≥ cap", a LOWER BOUND —
    /// the probe has no idea how much further the open space runs, and there is deliberately no
    /// number here to be mistaken for one.
    ClearToCap,
}

impl SpokeReading {
    /// The length a viewer should DRAW for this spoke, given the probe's cap: the hit distance,
    /// or the cap for a saturated spoke. Drawing-only — never use it to decide anything, because
    /// it re-collapses exactly the distinction this enum exists to keep.
    #[inline]
    pub fn draw_len(self, cap: f32) -> f32 {
        match self { SpokeReading::Hit { at } => at, SpokeReading::ClearToCap => cap }
    }
    /// The measured distance, or `None` when the spoke saturated (nothing within the cap).
    #[inline]
    pub fn hit_at(self) -> Option<f32> {
        match self { SpokeReading::Hit { at } => Some(at), SpokeReading::ClearToCap => None }
    }
}

/// WHERE the vertical of a [`ClearanceProbe`] came from (#885).
///
/// **Why this is not a bare `z`.** `clearance_probe` casts its rays from the nearest floor, found
/// with `nearest_floor(ref_z, up = 3, down = 8)` — and when that band holds no floor it reached
/// for `.unwrap_or(ref_z)` and published the result as `at: [east, north, floor_z]`, a field
/// documented as a floor height. A caller reading a void column therefore got a "floor" the world
/// does not contain. The two cases are now different variants, so "no floor was found" cannot be
/// served as a floor height.
///
/// Both variants carry `reference_z` — the z the caller (the walker: the character's own height)
/// asked about. It is here because the sample's z is NOT necessarily the character's: a body
/// embedded 1 u under a slab has a floor 1 u ABOVE it, so the whole sample describes a point in
/// the open air over the geometry the character is stuck inside. That gap used to be invisible.
///
/// **On the wire, `z` is a key only on the `floor` variant.** The JSON is internally tagged, so
/// `{"kind":"floor","z":…,"reference_z":…}` versus `{"kind":"no_floor_in_band","reference_z":…}` —
/// there is no `z` key at all in the second. A consumer comparing the sample's height against the
/// character's must branch on `kind` first; `reference_z` is the one field always present. (Rust
/// callers can use [`ProbeAnchor::z`], which states the fallback explicitly.)
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ProbeAnchor {
    /// A floor WAS found in the search band; the rays were cast from it. `z` may differ from
    /// `reference_z` in either direction — compare them before reading the sample as a statement
    /// about where the character is.
    Floor { z: f32, reference_z: f32 },
    /// NO floor in the search band around `reference_z`. The probe had nothing to stand on and
    /// cast from `reference_z` itself, so every value in this sample was measured in whatever
    /// medium the character is in — open air, water, or the inside of a solid.
    NoFloorInBand { reference_z: f32 },
}

impl ProbeAnchor {
    /// The height the rays were actually cast relative to (`Floor`'s floor, or the fallback).
    #[inline]
    pub fn z(self) -> f32 {
        match self {
            ProbeAnchor::Floor { z, .. } => z,
            ProbeAnchor::NoFloorInBand { reference_z } => reference_z,
        }
    }
    /// The z the caller asked about — the character's own height at sample time.
    #[inline]
    pub fn reference_z(self) -> f32 {
        match self {
            ProbeAnchor::Floor { reference_z, .. } | ProbeAnchor::NoFloorInBand { reference_z } => reference_z,
        }
    }
}

/// Whether the CONTROLLER can place a body at a point, and if not, which half of its test failed
/// (#885).
///
/// This is the movement controller's own `is_embedded` disjunction, not a nav opinion: the
/// footprint ring is pierced by geometry, **or** there is no floor anywhere within `GROUND_DEPTH`
/// beneath its feet. `Collision::body_placement` is the single definition — `movement::is_embedded`
/// reads it, and so does the published clearance probe. One predicate, two readers.
///
/// **What that does and does not establish.** It removes the second copy of the predicate; it does
/// not by itself make the probe evaluate it at the right POINT. The probe must call it at the
/// character's z rather than the anchor's, and that is a property of one call site, pinned by the
/// test `body_is_measured_at_the_character_not_the_anchor_when_the_two_disagree` (a scene where the
/// two verdicts genuinely differ), not by anything the type system enforces.
///
/// **This is an entry condition, not a freeze.** A non-`Placeable` verdict is what admits a body to
/// the depenetration net; the net usually relocates it and the body keeps moving. Measured: a dry
/// `FootprintPierced` body driven for 3.0 s travelled **131.98 u** with `hold() == None`, because
/// the push-out ring moved it clear on the first frame. Whether a character can move is
/// `player.hold` on `/v1/observe/debug`, not this field.
///
/// It is split into named variants rather than a `bool` because the two disjuncts are wildly
/// different worlds — "wedged in a slot" versus "standing over nothing" — and `EmbeddedNoRecovery`
/// on `/v1/observe/debug` collapses them into one token an agent cannot act on differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// The body fits here: footprint clear, and floor beneath it.
    Placeable,
    /// Geometry lies within the player radius of the body's torso band. Note the band: like every
    /// caller of `Collision::footprint_clear`, the ring is cast at `foot_z + PLAYER_BODY.ring`
    /// (3.0 u above the feet), so this is not a statement about the whole cylinder.
    FootprintPierced,
    /// The footprint is clear, but there is no floor within `GROUND_DEPTH` below the feet.
    NoFloorBelow,
    /// Both halves failed.
    FootprintPiercedAndNoFloorBelow,
}

impl Placement {
    /// The controller's `is_embedded` verdict — true for every non-`Placeable` variant.
    #[inline]
    pub fn is_embedded(self) -> bool { !matches!(self, Placement::Placeable) }
}

// There is deliberately no `as_str` here. #885 review round 1 (F6) found one: it had no production
// caller — the wire token comes from `#[serde(rename_all = "snake_case")]` above — and two mutants
// rewording its strings stayed GREEN, so it was a second, unpinned definition of the most
// agent-visible string in this change. The tokens are pinned where they are actually produced, by
// `the_json_encoding_keeps_the_distinctions` in `collision.rs`.

/// A live sample of the traversability model around one standing point: the radial wall spokes
/// (the same rays `ClearanceField::wall_at` aggregates into the hug cost) and the footprint ring
/// (the same ring `occupy_wall_ok` consults), plus the two graded field values the planner's
/// margin/hug logic actually reads. Produced by `Collision::clearance_probe` — nav sampling its
/// OWN model at the walker's position; consumers draw the sample, never re-cast the rays.
///
/// # What is authoritative for what (#885)
///
/// This payload was observed live (#885) reporting "open in every direction" — all 16 spokes at
/// the cap, every footprint direction clear — for a character the movement controller was holding
/// frozen with `embedded_no_recovery`, marking neither half as the less trustworthy one. Nothing in it was
/// a re-derivation; the two halves were simply answering different questions at different points,
/// unlabelled. So:
///
/// * [`ClearanceProbe::body`] is the authoritative answer to **"does the controller's placement
///   test pass where this character actually is"**. It is the controller's own predicate, evaluated
///   at `anchor.reference_z()` — the character's actual height. It is **not** a claim about whether
///   the character can move: a non-`Placeable` verdict is the ENTRY CONDITION to the depenetration
///   net, which usually relocates the body and lets it keep going (see [`Placement`] for the
///   measurement). The published answer to "can it move" is `player.hold` on `/v1/observe/debug`.
/// * [`ClearanceProbe::wall_spokes`], [`ClearanceProbe::footprint_ok`] and the two `field_*`
///   values are the PLANNER's model, sampled at [`ClearanceProbe::anchor`]`.z()`. They answer
///   "how much room does the route planner think there is around this standing point". A
///   `body` other than [`Placement::Placeable`] means they are describing a point the character
///   does not occupy, and they must not be read as a statement about the character itself.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClearanceProbe {
    /// The horizontal position the probe was taken at `[east, north]` — exactly the character's,
    /// with no snapping. The vertical lives in [`ClearanceProbe::anchor`] because, unlike these
    /// two, it is not necessarily a measured fact (#885).
    pub at: [f32; 2],
    /// The height the rays were cast from, and where that height came from.
    pub anchor: ProbeAnchor,
    /// The CONTROLLER's placement verdict at `[at[0], at[1], anchor.reference_z()]` — the
    /// character's real position, not the anchor. **This is the authoritative field** for whether
    /// the rest of this sample describes the character's own point; see the type docs. It is not a
    /// claim about whether the character can move (that is `player.hold` on `/v1/observe/debug`).
    pub body: Placement,
    /// 16 radial wall readings, CCW from +east, cast at `anchor.z()` + the planner's probe
    /// heights. A saturated spoke is [`SpokeReading::ClearToCap`], never the number `cap`.
    pub wall_spokes: Vec<SpokeReading>,
    /// The spokes' saturation horizon, in units.
    pub cap: f32,
    /// 8 footprint-ring directions (CCW from +east): `true` = clear of walls at the player radius.
    /// Cast at [`ClearanceProbe::footprint_ring_z`] = `anchor.z() + PLAYER_BODY.ring`. The
    /// controller's own ring — the one [`ClearanceProbe::body`] reports — is cast at
    /// `anchor.reference_z() + PLAYER_BODY.ring` instead, so whenever the anchor snapped away from
    /// the character these two are looking at different bands and may legitimately disagree.
    pub footprint_ok: Vec<bool>,
    /// The ring's radius (the player's collision radius).
    pub footprint_radius: f32,
    /// The absolute height this ring was cast at. Published so the band above is checkable rather
    /// than something a caller has to know `PLAYER_BODY.ring` to reconstruct.
    pub footprint_ring_z: f32,
    /// The zone-lifetime clearance field's graded wall distance at the anchor — the value the hug
    /// cost and standing-room margin actually consult.
    pub field_wall: f32,
    /// The field's graded ground (ledge) distance at the anchor.
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
    /// GOAL IDENTITY (#631 gap 1): the `NavStatus::goal_id` live at publish time — the SAME
    /// monotonic stamp surfaced as `nav_goal_id` on `/v1/observe/debug` (#349), bumped on every
    /// accepted `/move/{goto,follow,zone_cross,stop}`. Carried onto the snapshot so the `plan` below
    /// (which SURVIVES route clears as a failure diagnostic) is always attributable: a `plan` whose
    /// own `goal_id` differs from THIS one is a superseded command's record, not the current one's.
    /// Without it, a `plan` from a prior failed goto rode the snapshot unlabelled after a `/stop` or a
    /// fresh goto and could not be told apart from the current command's outcome (the gap-1 lie).
    pub goal_id: u64,
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
