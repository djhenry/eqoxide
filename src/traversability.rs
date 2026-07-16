//! Traversability: ONE definition of the character's collision volume and ONE authority both the
//! planner and the walker consult about what blocks it (#378, design doc
//! `docs/specs/2026-07-14-traversability-design.md`).
//!
//! # Why this module exists
//!
//! Navigation asks two orthogonal questions — "where do I stand?" (floor/support) and "what blocks
//! my body?" (wall/obstacle) — and until this module the PLANNER and the WALKER answered them with
//! different, mutually-blind predicates. The worst instance (#386, the design doc's §1c): the
//! planner probed walls at 2.5 u and 3.0 u above the floor while the controller collides at 0.5 u
//! and **4.0 u** — so geometry occupying only z ∈ (3.0, 4.0] above the floor (a door lintel, the
//! soffit of a low arch) was CLEAR to A* and SOLID to the walker. The planner handed the walker
//! routes it was physically incapable of following, in the fatal orientation: the planner was the
//! permissive one.
//!
//! The root cause was structural, not a typo: the character's collision volume was re-declared
//! independently in four places (`movement.rs` `slide`, `assets.rs` `sweep`, the A* edge test, the
//! A* `FEET_CLR`), and nothing forced them to agree — so they didn't. The fix is
//! [`Body`]/[`PLAYER_BODY`]: the volume is declared ONCE, and both the planner's probes and the
//! controller's contact rays are derived from it. After this, the #386 drift is not merely fixed —
//! it is UNREPRESENTABLE (verification hierarchy tier 1): to re-introduce it you would have to add
//! a new hardcoded height, and the fixture test in this module goes red if the shared chest probe
//! is bypassed.
//!
//! # What is (and is not) unified — the honest statement
//!
//! The planner and the controller can NOT share one predicate (design §6a): the planner asks a
//! discrete boolean ~10⁷ times per plan; the controller runs a continuous contact solver at ~100 Hz.
//! What they share is the TRUTH the predicates are derived from:
//!
//! * **the body** — this module;
//! * **the hazard set** — [`Traversability`], which the A* edge test, the ledge margin and the
//!   waypoint inset all consult (they used to be four mutually-blind predicates);
//! * **the direction of conservatism** — planner-clear ⇒ controller-passable. The converse is not
//!   required: the controller may pass things the planner refuses. That costs routes, not
//!   correctness, and it is the right direction to be wrong in.
//!
//! # Hot / cold split (the agent-honesty channel)
//!
//! The hot queries ([`Traversability::can_traverse_fast`], [`Traversability::can_occupy_fast`])
//! return `bool`, allocate nothing, and run in the A* inner loop. The cold forms
//! ([`Traversability::can_traverse`], [`Traversability::can_occupy`]) return
//! `Result<(), Blockage>` — WHY the refusal, and WHERE — and are meant to run at most a couple of
//! times per FAILED plan, never on a successful one. `BlockedBy(hazard, position)` is therefore
//! free: it is not computed until something has already failed.
//!
//! The two forms agree — `fast == diagnostic.is_ok()` — and the property test in this module pins
//! that. **Be precise about what that buys, and what it does not.** The agreement holds BY
//! CONSTRUCTION: both forms delegate to the same component predicates (`occupy_floor_ok`,
//! `occupy_wall_ok`, `occupy_margin_ok`, the same swept edge test) in the same order, so it is a
//! *consistency* invariant — a guard against a future edit diverging the two paths — NOT itself the
//! meaningful honesty guarantee. It cannot be, because both forms could agree on a WRONG answer.
//!
//! The meaningful guarantee is **planner ⇒ controller agreement**: every segment the planner emits
//! must be one the controller can actually walk (design §6c). This module delivers that only
//! PARTIALLY. It is closed on the CHEST axis — the planner's top probe and the controller's contact
//! ray are now the one `Body::chest` field (#386 closed on the chest axis, the drift that wedged
//! the walker under lintels). The FOOT axis remains divergent (the planner probes at
//! `Body::feet_clr` ≈ 2.5 while the controller's low contact ray sits at `Body::foot` = 0.5), and
//! that residual is tracked as **#420** — to be closed when the controller is wired to consult this
//! type directly (Phase 2). Do not read the `fast`/`diagnostic` agreement as if it discharged #420.

use crate::nav::collision::Collision;

/// The character's collision volume. THE single source of truth (#386 / design §2a-iv).
///
/// The planner's probes and the controller's contact rays are both derived from this one value.
/// Before this existed, the probe heights were re-declared in four places and the planner's top
/// probe (3.0) sat BELOW the controller's chest ray (4.0) — the #386 drift band.
#[derive(Clone, Copy, Debug)]
pub struct Body {
    /// Wall-collision radius, matched to the reference RoF2 client (`movement::PLAYER_RADIUS`).
    pub radius: f32,
    /// The controller's LOW contact ray, just above the feet. The planner deliberately does NOT
    /// probe here: a probe this low would read every ≤2 u stair riser as a wall, and risers up to
    /// [`crate::movement::STEP_UP`] (+ ground snap) are climbed by the controller's step-up, not
    /// collided with.
    pub foot: f32,
    /// The planner's LOW probe: just above the controller's real maximum step-up
    /// (`STEP_UP` 2.0 + `GROUND_SNAP_TOL` 0.5). A riser the walker cannot mount blocks this ray;
    /// a riser it can mount passes under it. (#239)
    pub feet_clr: f32,
    /// The TOP probe — **the shared one, and the whole point**. This is simultaneously the
    /// controller's chest contact ray (`movement::CharacterController::slide`) and the planner's
    /// upper edge probe (`assets` A*). One field, two readers: the #386 drift (planner 3.0 vs
    /// controller 4.0) is inexpressible as long as both read it from here.
    pub chest: f32,
    /// The controller's depenetration/footprint ring height (`Collision::footprint_clear`), also
    /// used by the waypoint-inset occupancy guard. Kept at its historical value; distinct from
    /// `chest` on purpose (the ring wants the torso mid-band, the contact ray wants the widest
    /// blocking band). Candidate for measurement-driven unification (design Q6).
    pub ring: f32,
    /// Total cylinder height, for documentation and future headroom probes. Geometry between
    /// `chest` and `height` is currently invisible to BOTH planner and controller (consistently —
    /// neither refuses it), which keeps the soundness invariant while under-modelling very low
    /// ceilings; [`Body::agent_height`] is what defends standing headroom.
    pub height: f32,
    /// The vertical clearance a standing character needs above a surface for it to count as
    /// STANDING ROOM (the #375 headroom defence: a surface with a solid roof closer than this is a
    /// ceiling, not ground). It must EXCEED a real ceiling's slab-gap yet stay BELOW a real room's
    /// height. `nav::collision::is_standable` reads this. **This is the single source of truth** — the
    /// `nav::collision::NAV_AGENT_HEIGHT` const is now a thin alias to it (design Q6 / PR-A: the value
    /// belongs on the Body, defined here, aliased there so existing call sites keep compiling).
    pub agent_height: f32,
    /// A surface's unit-normal `|z|` must be at least this to be flat enough to stand on (else it
    /// is a wall/steep slope A*'s grade limit would reject anyway). Tied to `MAX_WALK_GRADE`:
    /// `1/sqrt(1+1.2²) ≈ 0.64`. `nav::collision::is_standable` reads this; `nav::collision::NAV_NEAR_HORIZONTAL` is
    /// now a thin alias. Single source of truth here.
    pub near_horizontal: f32,
}

/// The one body every query derives from.
///
/// `chest` = 4.0 is the controller's contact height, VERBATIM (it has been 4.0 in `slide` since
/// the controller landed). The planner moved UP to it (from 3.0) — the conservative direction:
/// the planner may only refuse more than the controller collides with, never less.
pub const PLAYER_BODY: Body = Body {
    radius: crate::movement::PLAYER_RADIUS,
    foot: 0.5,
    feet_clr: crate::movement::STEP_UP + 0.5,
    chest: 4.0,
    ring: 3.0,
    height: 6.0,
    // ~5u standing headroom; the controller's own chest contact ray sits at `chest` = 4.0, so a
    // body needs a shade above that to stand. Was `assets::NAV_AGENT_HEIGHT`, now `nav::collision::NAV_AGENT_HEIGHT`.
    agent_height: 5.0,
    // 1/sqrt(1 + MAX_WALK_GRADE²) with MAX_WALK_GRADE = 1.2. Was `assets::NAV_NEAR_HORIZONTAL`, now `nav::collision::NAV_NEAR_HORIZONTAL`.
    near_horizontal: 0.64,
};

impl Body {
    /// The heights the PLANNER sweeps a walk edge at. Derived, not re-declared.
    #[inline]
    pub const fn planner_probes(&self) -> [f32; 2] { [self.feet_clr, self.chest] }
    /// The heights the CONTROLLER casts its contact rays at. Derived, not re-declared.
    #[inline]
    pub const fn contact_probes(&self) -> [f32; 2] { [self.foot, self.chest] }
}

/// The only two clearances a route may be planned at (#310 / design §2a-iii).
///
/// A free `f32` clearance is the type that let #310 happen: a `0.5 × PLAYER_RADIUS` "lower tier"
/// planned routes through gaps the character's real collision volume cannot fit, which is not a
/// tier — it is a lie. There is deliberately no float constructor here and no third variant:
/// a sub-radius plan is not expressible.
///
/// (Honest scope note: the legacy `find_path*` plumbing still carries `radius: f32` for its
/// external callers, and `search_tiered` still derives its two rungs as
/// `radius.max(PLAYER_RADIUS)` / `.max(NAV_PREFERRED_CLEARANCE)` — those rungs are exactly
/// [`Tier::Minimum`]/[`Tier::Preferred`]'s units (`tier_ladder_floors_at_player_radius` pins the
/// equality), but the signatures have NOT been migrated to take `Tier`; that is deferred to the
/// controller-wiring phase. Until then this enum is the named truth the f32 plumbing is clamped
/// against, not yet the type it carries.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// `NAV_PREFERRED_CLEARANCE` (2 × `PLAYER_RADIUS`): one radius to fit, one radius of standing
    /// room. The rung normal walking uses.
    Preferred,
    /// Exactly `PLAYER_RADIUS`: the character fits with nothing to spare. THE HARD FLOOR — below
    /// this the honest answer is `no_path`.
    Minimum,
}

impl Tier {
    #[inline]
    pub fn units(self) -> f32 {
        match self {
            Tier::Preferred => crate::nav::collision::NAV_PREFERRED_CLEARANCE,
            Tier::Minimum => crate::movement::PLAYER_RADIUS,
        }
    }
    /// The retry ladder, in order. Exactly two rungs, forever.
    pub const LADDER: [Tier; 2] = [Tier::Preferred, Tier::Minimum];
}

/// A STANDING position: an XY and the walkable floor beneath it. Not a free 3-vector — a free
/// `[f32; 3]` is what let #229 happen (a zone-line volume's interior point treated as a floor
/// height). Constructing one of these says, in the type, that `floor_z` is a surface you stand on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub xy: [f32; 2],
    pub floor_z: f32,
}

impl Point {
    #[inline]
    pub fn new(xy: [f32; 2], floor_z: f32) -> Self { Point { xy, floor_z } }
    #[inline]
    pub fn pos3(&self) -> [f32; 3] { [self.xy[0], self.xy[1], self.floor_z] }
}

/// What refused a position or a segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HazardKind {
    /// The floor runs out (a drop, a bridge lip, a hole) — or never existed.
    Floor,
    /// Solid geometry in the way of the body.
    Wall,
    /// Open water where ground was required.
    Water,
}

impl HazardKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HazardKind::Floor => "floor",
            HazardKind::Wall => "wall",
            HazardKind::Water => "water",
        }
    }
}

/// WHY a position or a segment was refused, and WHERE. The agent-honesty payload: computed only on
/// the COLD path (a failed plan), never in the hot loop, so it costs nothing when routing succeeds.
///
/// Honesty about this type: it names ONE blocking fact, not necessarily the only one and not
/// necessarily the one to fix. Callers surfacing it must name it accordingly
/// (`goal_blocked_by` / `frontier_blocked_by`, never `reason`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Blockage {
    pub hazard: HazardKind,
    pub at: [f32; 3],
}

/// The refusal as returned to a caller: `Result<(), BlockedBy>` — the Ok case is a ZST, so a
/// successful query allocates nothing.
pub type BlockedBy = Blockage;

// ─────────────────────────── the static clearance field (design §3) ───────────────────────────

/// Memo-key lattice: clearances are computed at the centres of a fixed 2 u XY grid (the fine
/// tier's cell) with 2 u floor buckets (A*'s own `qf` quantum). A query is answered for the key
/// cell its point falls in.
const FIELD_CELL: f32 = 2.0;
/// Storage quantum for the u8-packed distances (units per count). Saturates at 63.75 u.
const FIELD_QUANTUM: f32 = 0.25;
/// How far the WALL spokes look. Everything at/above this reads as "roomy": the largest wall
/// threshold anywhere is `Tier::Preferred` (2.0), and the hug cost fades out there too, so a
/// 4 u horizon leaves headroom without paying for long rays.
const WALL_CAP: f32 = 4.0;
/// How far the GROUND probe looks. The largest ground (ledge) margin is `Tier::Preferred` (2.0).
const GROUND_CAP: f32 = 2.0;
/// Ground probe radii, ascending. The 0.5 rung exists so a lip RIGHT at a waypoint reads as ~0.
const GROUND_RADII: [f32; 4] = [0.5, 1.0, 1.5, 2.0];
/// Bound on each memo map (~24 B/entry ⇒ tens of MB worst case, cleared with the zone). At
/// capacity the field keeps ANSWERING correctly — it just recomputes instead of inserting — so the
/// bound degrades speed, never truth, and never unboundedly grows in a huge zone (gfaydark is
/// 5.9 M columns at 2 u; only VISITED cells ever memoise, but a long session visits a lot).
const FIELD_MAX_ENTRIES: usize = 1 << 20;

/// **The static clearance field (`MemoField`, design §3d): for a standing point, the horizontal
/// distance to the nearest thing you cannot be at.** Two graded distances, not booleans:
///
/// * `wall_at` — distance to the nearest SOLID geometry at the body's probe heights, measured
///   RADIALLY (16 spokes). This is what closes #381's structural hole: `path_clear`'s feelers run
///   parallel to travel and can never see a wall the segment runs alongside; a radial spoke
///   crosses it. Used as a hot COST (the hug penalty — never a hard filter below
///   `Tier::Preferred`, per the design's §9 non-negotiable) and as the generous tier's
///   standing-room threshold.
/// * `ground_at` — distance to the nearest spot where the floor RUNS OUT (a drop, a bridge lip,
///   a waterline): the graded form of the old boolean `ground_margin_ok`, probed on the same four
///   axial directions and the same ±band.
///
/// # Determinism (the #394 discipline)
///
/// A memoised value is a PURE FUNCTION OF ITS KEY: it is always computed at the key cell's centre
/// and bucket floor, never at the querying point — so the answer does not depend on which query
/// happened to populate the cache first, and concurrent workers racing to insert write identical
/// values. The price is quantisation (a query point can sit up to ~1.4 u from its key centre);
/// every consumer of this field is a cost or a ladder-guarded threshold, sized for that error.
#[derive(Default)]
pub struct ClearanceField {
    wall: std::sync::RwLock<std::collections::HashMap<(i64, i64, i32), u8>>,
    ground: std::sync::RwLock<std::collections::HashMap<(i64, i64, i32), u8>>,
    /// Entry cap per map (tests shrink it to prove the degrade-not-grow behaviour).
    cap: std::sync::atomic::AtomicUsize,
}

impl ClearanceField {
    fn key(x: f32, y: f32, floor_z: f32) -> (i64, i64, i32) {
        ((x / FIELD_CELL).floor() as i64,
         (y / FIELD_CELL).floor() as i64,
         (floor_z / 2.0).round() as i32)
    }
    fn key_centre(k: (i64, i64, i32)) -> [f32; 3] {
        [(k.0 as f32 + 0.5) * FIELD_CELL, (k.1 as f32 + 0.5) * FIELD_CELL, k.2 as f32 * 2.0]
    }
    fn cap(&self) -> usize {
        match self.cap.load(std::sync::atomic::Ordering::Relaxed) {
            0 => FIELD_MAX_ENTRIES,
            n => n,
        }
    }
    /// Only called from `#[cfg(test)]` (its sole caller lives in `mod tests`, stripped from a
    /// plain build) — gated to match, else it reads as dead code outside `cargo test`.
    #[cfg(test)]
    pub(crate) fn set_cap_for_test(&self, n: usize) {
        self.cap.store(n, std::sync::atomic::Ordering::Relaxed);
    }

    fn cached(map: &std::sync::RwLock<std::collections::HashMap<(i64, i64, i32), u8>>,
              k: (i64, i64, i32)) -> Option<f32> {
        map.read().ok()?.get(&k).map(|&q| q as f32 * FIELD_QUANTUM)
    }
    fn store(&self, map: &std::sync::RwLock<std::collections::HashMap<(i64, i64, i32), u8>>,
             k: (i64, i64, i32), v: f32) -> f32 {
        let q = ((v / FIELD_QUANTUM).round() as i64).clamp(0, u8::MAX as i64) as u8;
        if let Ok(mut m) = map.write() {
            // At capacity: answer correctly, just don't grow. Purity of compute-from-key makes the
            // recompute identical to what the entry would have held.
            if m.len() < self.cap() || m.contains_key(&k) {
                m.insert(k, q);
            }
        }
        q as f32 * FIELD_QUANTUM
    }

    /// Radial distance from the (key cell of) `(x, y, floor_z)` to the nearest solid geometry at
    /// the body's planner probe heights, saturating at [`WALL_CAP`].
    pub fn wall_at(&self, col: &Collision, x: f32, y: f32, floor_z: f32) -> f32 {
        let k = Self::key(x, y, floor_z);
        if let Some(v) = Self::cached(&self.wall, k) { return v; }
        let c = Self::key_centre(k);
        let mut best = WALL_CAP;
        const SPOKES: usize = 16;
        for i in 0..SPOKES {
            let a = (i as f32) / (SPOKES as f32) * std::f32::consts::TAU;
            let (dx, dy) = (a.cos(), a.sin());
            for hz in PLAYER_BODY.planner_probes() {
                let from = [c[0], c[1], c[2] + hz];
                let to = [c[0] + dx * WALL_CAP, c[1] + dy * WALL_CAP, c[2] + hz];
                if let Some(t) = col.nearest_hit_t(from, to) {
                    best = best.min(t * WALL_CAP);
                }
            }
        }
        self.store(&self.wall, k, best)
    }

    /// Distance from the (key cell of) `(x, y, floor_z)` to the nearest missing-floor direction —
    /// the graded `ground_margin_ok`: the first radius (of [`GROUND_RADII`], on the four axial
    /// directions) with no floor in the ±band, saturating at [`GROUND_CAP`].
    pub fn ground_at(&self, col: &Collision, x: f32, y: f32, floor_z: f32) -> f32 {
        let k = Self::key(x, y, floor_z);
        if let Some(v) = Self::cached(&self.ground, k) { return v; }
        let c = Self::key_centre(k);
        let mut clear = GROUND_CAP;
        'radii: for (i, &r) in GROUND_RADII.iter().enumerate() {
            for (dx, dy) in [(r, 0.0), (-r, 0.0), (0.0, r), (0.0, -r)] {
                let ok = col.nearest_floor(c[0] + dx, c[1] + dy, c[2], 3.0, 8.0)
                    .is_some_and(|f| (f - c[2]).abs() <= 8.0);
                if !ok {
                    clear = if i == 0 { 0.0 } else { GROUND_RADII[i - 1] };
                    break 'radii;
                }
            }
        }
        self.store(&self.ground, k, clear)
    }
}

// Count of cold-path (`diagnose`) evaluations ON THIS THREAD, for the "zero diagnosis on success"
// proof (design §5c). Test-only, and thread-local on purpose: the cargo test harness runs tests in
// parallel, and a global counter would let another test's legitimate diagnoses pollute the
// zero-on-success assertion.
#[cfg(test)]
thread_local! {
    pub(crate) static DIAGNOSE_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[inline]
fn note_diagnose() {
    #[cfg(test)]
    DIAGNOSE_CALLS.with(|c| c.set(c.get() + 1));
}

/// The ONE authority on what blocks the character, for one plan.
///
/// Wraps the zone's static hazards (floor / wall / water — all reached through [`Collision`], whose
/// clearance memo caches the expensive lookups for the zone's lifetime) behind exactly two
/// questions, each in a hot and a cold form. The A* walk-edge test, the ledge margin and the
/// waypoint inset all go through here — they used to be four predicates that could not see each
/// other's hazards (#378).
///
/// Scope, stated honestly: dynamic hazards (mobs, players, declared danger zones) are NOT here yet —
/// they remain the `avoid`/`aggro_cost` soft bias in the A* loop (design PR-6 makes them
/// first-class). The controller keeps its own continuous solver; it shares the [`Body`] and the
/// geometry, not this discrete predicate (design §6a).
pub struct Traversability<'a> {
    col: &'a Collision,
    pub body: &'static Body,
    /// The wall clearance this plan was asked for (already clamped ≥ `Tier::Minimum.units()` by
    /// `search_tiered`'s ladder).
    pub radius: f32,
    /// The plan's grid resolution — decides sweep-vs-ray edge validation (`SWEPT_EDGE_MAX_CELL`).
    pub cell: f32,
    /// Ground (ledge) margin required around a standing point. `0.0` at `Tier::Minimum` — the
    /// minimum tier's promise is exactly "the character fits", which is what keeps narrow bridges
    /// and gangplanks routable (#310's mirror; design Q5 keeps a non-zero minimum-margin as a
    /// measured follow-up, not a default).
    pub ledge_margin: f32,
    /// A floating (swimming) plan is exempt from ground margins outright: a floating character has
    /// no ground under it by definition, so the probe would refuse every cell it must cross.
    pub floating: bool,
}

impl<'a> Traversability<'a> {
    pub fn new(col: &'a Collision, radius: f32, cell: f32, ledge_margin: f32, floating: bool) -> Self {
        Traversability { col, body: &PLAYER_BODY, radius, cell, ledge_margin, floating }
    }

    // ── HOT: what A* calls, per edge / per waypoint. No Result, no Option, no allocation. ──

    /// Can the character's volume STAND at `p`? Wall axis: the footprint ring at `body.ring` must
    /// be clear at `radius`. Ground axis: a floor must exist in a tight band of `p.floor_z`
    /// (and `ledge_margin` of ground all around, when this plan asked for one).
    #[inline]
    pub fn can_occupy_fast(&self, p: Point) -> bool {
        self.occupy_floor_ok(p) && self.occupy_wall_ok(p) && self.occupy_margin_ok(p)
    }

    /// Can the character's volume TRAVEL the walk edge `a → b`? This is the A* edge test: the
    /// planner's two probe heights ([`Body::planner_probes`]), each swept across the body's width
    /// on a fine grid / cast as a centre ray on a coarse one (`edge_clear`), plus the destination's
    /// ground margin. One authority — the heights come from the same [`Body`] the controller
    /// collides with, so a "planner-clear, walker-solid" band (#386) cannot re-open here.
    #[inline]
    pub fn can_traverse_fast(&self, a: Point, b: Point) -> bool {
        let [feet, chest] = self.body.planner_probes();
        self.col.edge_clear(
            [a.xy[0], a.xy[1], a.floor_z + chest],
            [b.xy[0], b.xy[1], b.floor_z + chest],
            self.radius, self.cell)
            && self.col.edge_clear(
                [a.xy[0], a.xy[1], a.floor_z + feet],
                [b.xy[0], b.xy[1], b.floor_z + feet],
                self.radius, self.cell)
            && self.occupy_margin_ok(b)
    }

    // ── COLD: the diagnostic forms. Run on FAILED plans (and in tests), never in the inner loop. ──
    // MUST agree with the fast forms: `fast == diagnostic.is_ok()`, property-tested below. Each
    // re-runs the same component predicates in the same order and names the first refusal.

    /// Diagnostic [`Traversability::can_occupy_fast`]: WHY can't the character stand at `p`?
    pub fn can_occupy(&self, p: Point) -> Result<(), BlockedBy> {
        note_diagnose();
        if !self.occupy_floor_ok(p) {
            return Err(Blockage { hazard: self.ground_hazard(p.xy, p.floor_z), at: p.pos3() });
        }
        if !self.occupy_wall_ok(p) {
            return Err(Blockage { hazard: HazardKind::Wall, at: p.pos3() });
        }
        if !self.occupy_margin_ok(p) {
            return Err(Blockage { hazard: self.margin_hazard(p), at: p.pos3() });
        }
        Ok(())
    }

    /// Diagnostic [`Traversability::can_traverse_fast`]: WHY can't the character walk `a → b`?
    /// The blockage position for a wall is the first contact along the failing probe's centre
    /// segment (best effort — a feeler off the centre line may be the one that hit; the centre
    /// contact, or failing that the segment midpoint, is still an actionable "about here").
    pub fn can_traverse(&self, a: Point, b: Point) -> Result<(), BlockedBy> {
        note_diagnose();
        let [feet, chest] = self.body.planner_probes();
        for hz in [chest, feet] {
            let from = [a.xy[0], a.xy[1], a.floor_z + hz];
            let to = [b.xy[0], b.xy[1], b.floor_z + hz];
            if !self.col.edge_clear(from, to, self.radius, self.cell) {
                let at = match self.col.nearest_hit_t(from, to) {
                    Some(t) => [from[0] + (to[0] - from[0]) * t,
                                from[1] + (to[1] - from[1]) * t,
                                from[2] + (to[2] - from[2]) * t],
                    // A feeler (not the centre ray) hit: report the midpoint of the swept band.
                    None => [(from[0] + to[0]) * 0.5, (from[1] + to[1]) * 0.5, (from[2] + to[2]) * 0.5],
                };
                return Err(Blockage { hazard: HazardKind::Wall, at });
            }
        }
        if !self.occupy_margin_ok(b) {
            return Err(Blockage { hazard: self.margin_hazard(b), at: b.pos3() });
        }
        Ok(())
    }

    // ── the shared component predicates (each used by BOTH the fast and diagnostic forms, so the
    //    two cannot disagree by construction — the property test below is the belt to this brace) ──

    /// A walkable floor exists within a tight band of `p.floor_z` (the same ±band the waypoint
    /// inset has always used). Floating plans ask the water instead.
    #[inline]
    fn occupy_floor_ok(&self, p: Point) -> bool {
        if self.floating {
            return self.col.in_water([p.xy[0], p.xy[1], p.floor_z - 1.0])
                || self.col.in_water([p.xy[0], p.xy[1], p.floor_z]);
        }
        self.col
            .nearest_floor(p.xy[0], p.xy[1], p.floor_z, 3.0, 8.0)
            .is_some_and(|f| (f - p.floor_z).abs() <= 8.0)
    }

    /// The body's footprint ring is clear of walls at the plan's radius.
    #[inline]
    fn occupy_wall_ok(&self, p: Point) -> bool {
        self.col.footprint_clear(p.xy[0], p.xy[1], p.floor_z, self.radius, 8)
    }

    /// STANDING ROOM (the tiered promise above `Tier::Minimum`): `ledge_margin` of GROUND all
    /// around (a drop, a lip, open water all read as "no ground") AND `radius` of radial WALL
    /// clearance — the roomy tier keeps its distance from geometry that is missing and geometry
    /// that is in the way, through the one zone-lifetime clearance field.
    ///
    /// `true` when this plan asked for no margin (the minimum tier / a floating plan): at
    /// `Tier::Minimum` the promise is exactly "the character fits" — a threshold here would seal
    /// the narrow bridges and corridors the minimum tier exists to keep routable (the design's §9
    /// non-negotiable: the field is a threshold at Minimum ONLY for what genuinely cannot fit,
    /// which the swept edge test already enforces; above Minimum it is standing room; it is never
    /// a hard filter that survives the ladder's fallback).
    #[inline]
    fn occupy_margin_ok(&self, p: Point) -> bool {
        if self.ledge_margin <= 0.0 || self.floating {
            return true;
        }
        // GROUND half: the substantive new path — the graded field distance replacing the old
        // boolean `ground_margin_ok`. WALL half: a radial belt-and-braces. Note honestly it is
        // largely REDUNDANT with `occupy_wall_ok`, which already casts a footprint ring at exactly
        // `self.radius` (2.0 at Preferred), so a generous point inside the wall standing-room is
        // usually refused there first. The radial `wall_clearance` (16 spokes) can still catch a
        // thin wall that falls in the angular gap between the footprint's 8 rays — a belt for
        // needle geometry real zone art does not contain — and it shares the same zone-lifetime
        // field the hug COST relies on (its load-bearing use). Kept for that belt, not because it
        // adds tier behaviour the footprint lacks.
        self.col.ground_clearance(p.xy[0], p.xy[1], p.floor_z) >= self.ledge_margin
            && self.col.wall_clearance(p.xy[0], p.xy[1], p.floor_z) >= self.radius
    }

    /// Cold-path only: name the ground hazard at a spot with no usable floor — open water reads
    /// as `Water`, anything else as `Floor`.
    fn ground_hazard(&self, xy: [f32; 2], z: f32) -> HazardKind {
        if self.col.in_water([xy[0], xy[1], z]) || self.col.in_water([xy[0], xy[1], z - 2.0]) {
            HazardKind::Water
        } else {
            HazardKind::Floor
        }
    }

    /// Cold-path only: name the hazard that broke the standing-room test around `p`. If the
    /// GROUND half failed, walk the axial probes outward to the failing spot and classify it
    /// water-vs-floor there; otherwise the WALL half failed.
    fn margin_hazard(&self, p: Point) -> HazardKind {
        if self.col.ground_clearance(p.xy[0], p.xy[1], p.floor_z) < self.ledge_margin {
            let m = self.ledge_margin;
            for (dx, dy) in [(m, 0.0), (-m, 0.0), (0.0, m), (0.0, -m)] {
                let (x, y) = (p.xy[0] + dx, p.xy[1] + dy);
                let ok = self.col.nearest_floor(x, y, p.floor_z, 3.0, 8.0)
                    .is_some_and(|f| (f - p.floor_z).abs() <= 8.0);
                if !ok {
                    return self.ground_hazard([x, y], p.floor_z);
                }
            }
            return HazardKind::Floor;
        }
        HazardKind::Wall
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{MeshData, RenderMode, ZoneAssets};
    use crate::nav::collision::Collision;
    use crate::movement::{CharacterController, MoveIntent, PLAYER_RADIUS};

    fn mesh(positions: Vec<[f32; 3]>) -> MeshData {
        MeshData {
            positions,
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0; 3],
            render_mode: RenderMode::Opaque,
            anim: None,
        }
    }
    /// Floor at height `z` over east [e0,e1] × north [n0,n1]. libeq pos = [north, height, east].
    fn floor_at(z: f32, e0: f32, e1: f32, n0: f32, n1: f32) -> MeshData {
        mesh(vec![[n0, z, e0], [n1, z, e0], [n1, z, e1], [n0, z, e1]])
    }
    /// Vertical east-facing panel at east=`e`, north [n0,n1], height [h0,h1].
    fn panel(e: f32, n0: f32, n1: f32, h0: f32, h1: f32) -> MeshData {
        mesh(vec![[n0, h0, e], [n1, h0, e], [n1, h1, e], [n0, h1, e]])
    }
    fn col(meshes: Vec<MeshData>) -> Collision {
        Collision::build(&ZoneAssets { terrain: meshes, objects: vec![], textures: vec![] }, 32.0)
    }

    /// A corridor (floor + side walls, east-going) with a LINTEL: an overhead barrier spanning the
    /// full corridor cross-section from 3.5 u to 6.5 u above the floor. The #386 drift band made
    /// exactly this shape a trap: the old planner probes (2.5, 3.0) pass UNDER it, the controller's
    /// chest contact ray (4.0) hits it, and its step-up re-probe (4.0 + 2.0) hits it too — clear to
    /// A*, solid to the walker.
    fn lintel_corridor() -> Collision {
        col(vec![
            floor_at(0.0, -40.0, 40.0, -8.0, 8.0),
            panel(0.0, -8.0, 8.0, 3.5, 6.5), // the lintel, sealing the corridor at chest height
            // side walls so no detour exists — the ONLY way east is under the lintel
            mesh(vec![[-8.0, 0.0, -40.0], [-8.0, 10.0, -40.0], [-8.0, 10.0, 40.0], [-8.0, 0.0, 40.0]]),
            mesh(vec![[8.0, 0.0, -40.0], [8.0, 10.0, -40.0], [8.0, 10.0, 40.0], [8.0, 0.0, 40.0]]),
        ])
    }

    /// **THE #386 DRIFT FIXTURE (RED on pre-Body main).** Every route the planner emits must be one
    /// the real controller can actually walk. On main the planner (probes 2.5/3.0) routed straight
    /// under the 3.5–6.5 lintel that the controller's 4.0 chest ray refuses — the walker pressed
    /// into it forever. With the shared [`Body`] the planner probes at the controller's own chest
    /// height and refuses the corridor, so it emits no un-walkable route.
    ///
    /// Mutation check: set the planner's chest probe back below 3.5 (e.g. the old 3.0) and this
    /// MUST go red — verified at authoring time.
    #[test]
    fn planner_never_routes_under_a_lintel_the_walker_collides_with() {
        let c = lintel_corridor();
        let start = [-20.0, 0.0, 0.0];
        let goal = [20.0, 0.0, 0.0];

        // Pin the fixture premise: the controller genuinely cannot cross the lintel.
        let mut ctrl = CharacterController::new(start);
        ctrl.on_ground = true;
        for _ in 0..600 {
            ctrl.step(MoveIntent { wish_dir: [1.0, 0.0], speed: 44.0, ..Default::default() },
                      1.0 / 60.0, &c);
        }
        assert!(ctrl.pos[0] < 0.0,
            "fixture premise: the controller must be blocked by the lintel (east={})", ctrl.pos[0]);

        // The invariant: whatever the planner answers, it must not be a route through the lintel.
        // (An honest "no route" is fine; a confident route the walker cannot walk is the #386 lie.)
        if let Some(route) = c.find_path(start, goal, PLAYER_RADIUS, &[], false) {
            let crossed = route.iter().any(|w| w[0] > 2.0);
            assert!(!crossed,
                "planner routed through a lintel the controller collides with (#386): {route:?}");
        }
    }

    /// The planner's probe heights and the controller's contact heights come from ONE body, and the
    /// planner's top probe is the controller's chest ray. This is the drift-direction invariant in
    /// its cheapest form: if someone re-declares either height locally, the lintel fixture above
    /// catches the behaviour; this catches the structure.
    #[test]
    fn planner_top_probe_is_the_controllers_chest_ray() {
        let planner = PLAYER_BODY.planner_probes();
        let contact = PLAYER_BODY.contact_probes();
        assert_eq!(planner[1], contact[1], "the top probe must be shared (the #386 axis)");
        assert!(planner[0] > crate::movement::STEP_UP,
            "the planner's low probe must clear the step-up band, or every stair reads as a wall");
        assert!(PLAYER_BODY.radius >= crate::movement::PLAYER_RADIUS);
    }

    /// **THE FIELD IS DETERMINISTIC AND HISTORY-BLIND (the #394 discipline).** A memoised value is
    /// a pure function of its key: two queries anywhere inside one 2 u key cell get the identical
    /// answer, in either order, memo warm or cold — so a plan's outcome can never depend on which
    /// earlier plan happened to populate the cache.
    #[test]
    fn clearance_field_answers_are_a_pure_function_of_the_key() {
        let c = col(vec![
            floor_at(0.0, -30.0, 30.0, -30.0, 30.0),
            panel(10.0, -30.0, 30.0, 0.0, 10.0), // a wall east of the origin
        ]);
        // Two distinct points in the same 2u key cell (keys floor(x/2): 4.1 and 5.3 both → 2).
        let a = c.wall_clearance(4.1, 0.2, 0.0);
        let b = c.wall_clearance(5.3, 1.7, 0.0);
        assert_eq!(a, b, "same key cell must give the identical (key-centre) answer");
        // Warm-vs-cold: a fresh Collision over the same geometry, queried in a different order.
        let c2 = col(vec![
            floor_at(0.0, -30.0, 30.0, -30.0, 30.0),
            panel(10.0, -30.0, 30.0, 0.0, 10.0),
        ]);
        let _ = c2.wall_clearance(-20.0, -20.0, 0.0); // unrelated first query
        assert_eq!(c2.wall_clearance(5.3, 1.7, 0.0), a, "history must not change the answer");
        // And the value is sane: the key centre (5,1) is 5u from the wall at east=10, but capped
        // sampling quantises — just require it sees the wall inside the cap and not through it.
        assert!(a > 3.0 && a <= 5.5, "wall at east=10 from key-centre ~(5,1): {a}");
        let g = c.ground_clearance(0.0, 0.0, 0.0);
        assert!(g >= 2.0, "mid-plateau has full ground clearance: {g}");
        let edge = c.ground_clearance(-29.5, 0.0, 0.0);
        assert!(edge < 2.0, "the plateau lip has reduced ground clearance: {edge}");
    }

    /// **THE MEMO IS BOUNDED, AND THE BOUND DEGRADES SPEED, NEVER TRUTH.** At capacity the field
    /// answers from a fresh compute instead of growing — same values, map never exceeds the cap.
    #[test]
    fn clearance_field_capacity_degrades_not_grows_or_lies() {
        let c = col(vec![floor_at(0.0, -60.0, 60.0, -60.0, 60.0)]);
        // Record truth uncapped.
        let pts: Vec<(f32, f32)> = (0..40).map(|i| (-58.0 + 3.0 * i as f32, 2.0 * i as f32 - 40.0)).collect();
        let truth: Vec<f32> = pts.iter().map(|&(x, y)| c.wall_clearance(x, y, 0.0)).collect();
        // A fresh, tightly-capped field over the same geometry.
        let c2 = col(vec![floor_at(0.0, -60.0, 60.0, -60.0, 60.0)]);
        c2.clearance_field_for_test().set_cap_for_test(8);
        for (i, &(x, y)) in pts.iter().enumerate() {
            assert_eq!(c2.wall_clearance(x, y, 0.0), truth[i], "capped field lied at {:?}", (x, y));
        }
        // Repeat pass: still correct (whether served from the 8 kept entries or recomputed).
        for (i, &(x, y)) in pts.iter().enumerate() {
            assert_eq!(c2.wall_clearance(x, y, 0.0), truth[i]);
        }
    }

    /// **THE HALLWAY HUG (the qcat symptom, #381).** A fine plan whose start and carrot both sit
    /// in a lane close to one wall must swing to the roomier lane for the ride, instead of
    /// emitting a lane that skims the wall for the walker to press into. The corridor is
    /// asymmetric on purpose (walls at north −2.5 and +4.5): the start/carrot lane (n = −1) has
    /// only 1.5 u of wall clearance, a full-clearance lane exists two cells north, and ONLY the
    /// hug cost distinguishes them — both lanes are wall-legal at the minimum tier the fine
    /// planner runs at, and without the cost the straight hugging lane is strictly shorter.
    ///
    /// (The lane deliberately sits at 1.5 u, not wall-TANGENT 1.0 u: at exact tangency the swept
    /// edge test itself refuses DIAGONAL entry into the lane — the body corner grazes the wall —
    /// so a tangent carrot is only reachable along the hug lane and no cost can move the route.
    /// That tangent-carrot case is the coarse tier's to avoid creating, not this cost's to fix.)
    ///
    /// Mutation check: zero the hug cost (`hug_cost` → 0.0) and this MUST go red — verified at
    /// authoring time.
    #[test]
    fn fine_plan_swings_off_the_hugged_wall_when_a_freer_lane_exists() {
        // Walls run EAST-WEST (north = -2.5 and +4.5), corridor open east [-30, 30].
        let wall_ns = |n: f32| mesh(vec![[n, 0.0, -30.0], [n, 8.0, -30.0], [n, 8.0, 30.0], [n, 0.0, 30.0]]);
        let c = col(vec![
            floor_at(0.0, -30.0, 30.0, -12.0, 12.0),
            wall_ns(-2.5),
            wall_ns(4.5),
        ]);
        // Start and carrot both in the near-wall lane (1.5 u off the south wall).
        let out = c.find_path_local([-16.0, -1.0, 0.0], [16.0, -1.0, 0.0], 2.0, 40.0, 4.0);
        let steer = out.steer();
        assert!(steer.len() >= 3, "the corridor must still be threaded: {out:?}");
        // Mid-route (away from the endpoints, which are pinned to the ask), the lane must have
        // pulled off the south wall toward the roomy side.
        let mid: Vec<_> = steer.iter().filter(|w| w[0] > -8.0 && w[0] < 8.0).collect();
        assert!(!mid.is_empty(), "route must cross the corridor middle: {steer:?}");
        let worst = mid.iter().map(|w| w[1] + 2.5).fold(f32::MAX, f32::min); // distance from south wall
        assert!(worst > 2.0,
            "mid-route lane still hugs the south wall (nearest approach {worst:.2}u): {steer:?}");
    }

    /// **THE GRADED STANDING-ROOM MARGIN (the new `occupy_margin_ok` path, #378 field wiring).**
    /// The field-wiring commit replaced the boolean `ground_margin_ok` with the field's GRADED
    /// `ground_clearance`, and added a `wall_clearance >= radius` standing-room half. This pins that
    /// path directly — not through a route hash (it is invisible on the parity fixtures) but through
    /// the predicate itself, at both tiers, so the intended behaviour change rests on more than the
    /// single hug fixture.
    ///
    /// Fixture: an open plateau. A point 1.5 u inside the NORTH edge stands on continuous floor
    /// (a floor edge is not geometry, so `occupy_wall_ok`'s footprint ring is clear and
    /// `occupy_floor_ok` finds floor) — the ONLY thing that can refuse it is the graded ground
    /// margin. It must be:
    ///   * REFUSED at `Tier::Preferred` (margin 2.0 > its ~1.5 u of ground clearance), naming Floor;
    ///   * OCCUPIABLE at `Tier::Minimum` (no standing-room requirement — the tier's promise is only
    ///     "the character fits", which keeps narrow ledges routable, #310's mirror).
    /// A mid-plateau point passes at both.
    ///
    /// Mutation check (verified at authoring time): drop the `ground_clearance >= ledge_margin`
    /// clause from `occupy_margin_ok` and the near-edge Preferred case wrongly PASSES → red. (The
    /// `wall_clearance >= radius` clause is deliberately NOT mutation-bitable here: it is redundant
    /// with `occupy_wall_ok`'s footprint ring at the tier radius — see that clause's comment — so
    /// the near-wall refusal below is enforced by the footprint whether or not the margin clause
    /// fires. The wall STANDING-ROOM behaviour is still pinned, as the tiered pass/fail below.)
    #[test]
    fn graded_standing_room_margin_refuses_near_edge_at_preferred_not_minimum() {
        let c = col(vec![floor_at(0.0, -40.0, 40.0, -40.0, 40.0)]);
        // A point 1.5u inside the north floor edge (edge at north=40), mid-span in east.
        let near_edge = Point::new([0.0, 38.5], 0.0);
        let mid = Point::new([0.0, 0.0], 0.0);

        // The graded field genuinely sees the edge as reduced ground clearance here, and full
        // clearance mid-plateau — that is the boolean→graded swap this test exists to pin.
        assert!(c.ground_clearance(0.0, 38.5, 0.0) < Tier::Preferred.units(),
            "near the edge, graded ground clearance must be below the preferred margin");
        assert!(c.ground_clearance(0.0, 0.0, 0.0) >= Tier::Preferred.units(),
            "mid-plateau must have full graded ground clearance");

        let pref = Traversability::new(&c, Tier::Preferred.units(), 2.0, Tier::Preferred.units(), false);
        let minm = Traversability::new(&c, Tier::Minimum.units(), 2.0, 0.0, false);

        // Preferred: the near-edge point lacks standing room and is refused, naming the ground.
        assert!(pref.can_occupy_fast(mid), "mid-plateau is occupiable at the preferred tier");
        match pref.can_occupy(near_edge) {
            Err(b) => assert_eq!(b.hazard, HazardKind::Floor,
                "a point short of the preferred ground margin names Floor: {b:?}"),
            Ok(()) => panic!("near the edge must be refused at the preferred tier (standing room)"),
        }
        // Minimum: the SAME point fits (no standing-room requirement) — the tiered distinction.
        assert!(minm.can_occupy_fast(near_edge),
            "the near-edge point must remain occupiable at the minimum tier (it fits)");
        assert!(minm.can_occupy(near_edge).is_ok());

        // WALL STANDING-ROOM, tiered. A point 1u from a wall on continuous floor: the field sees
        // the wall (graded `wall_clearance` below the preferred radius), and the point is REFUSED
        // at Preferred (radius 2.0) yet OCCUPIABLE at Minimum (radius 1.0 — it fits with nothing to
        // spare). This is the same tier distinction as the ground half, on the wall axis; it is
        // enforced by the footprint at the tier radius (with the margin clause as a radial belt).
        let cw = col(vec![
            floor_at(0.0, -40.0, 40.0, -40.0, 40.0),
            panel(8.0, -40.0, 40.0, 0.0, 10.0), // a north-south wall at east=8
        ]);
        // 1.2u from the wall (east=6.8): fits at radius 1.0 (Minimum), not at radius 2.0 (Preferred).
        let near_wall = Point::new([6.8, 0.0], 0.0);
        assert!(cw.wall_clearance(6.8, 0.0, 0.0) < Tier::Preferred.units(),
            "~1u from the wall, graded wall clearance is below the preferred radius");
        let prefw = Traversability::new(&cw, Tier::Preferred.units(), 2.0, Tier::Preferred.units(), false);
        let minw = Traversability::new(&cw, Tier::Minimum.units(), 2.0, 0.0, false);
        assert!(prefw.can_occupy(near_wall).is_err(),
            "a point inside the preferred wall standing-room must be refused at Preferred");
        assert!(minw.can_occupy_fast(near_wall),
            "the same point fits at the minimum tier (radius 1.0)");
    }

    /// Tier is the ladder and the ladder is closed: two rungs, minimum == the controller's radius,
    /// nothing below it expressible.
    #[test]
    fn tier_ladder_floors_at_player_radius() {
        assert_eq!(Tier::LADDER, [Tier::Preferred, Tier::Minimum]);
        assert_eq!(Tier::Minimum.units(), crate::movement::PLAYER_RADIUS);
        assert!(Tier::Preferred.units() > Tier::Minimum.units());
    }

    /// **THE AGREEMENT PROPERTY (design §5c #1).** For a spread of points and segments over a
    /// fixture with all three static hazards (walls, a floor edge, water), the hot and cold forms
    /// must give the same verdict: `fast == diagnostic.is_ok()`. This is the invariant that makes
    /// the zero-cost hot path safe to trust — mutation-checked at authoring time by perturbing
    /// `occupy_wall_ok` in one form only (the test went red).
    #[test]
    fn hot_and_cold_forms_never_disagree() {
        // Floor plateau ending at east=20 (a drop beyond), a wall panel at east=-10 (north<0),
        // and water south of north=-30 via a flat region map.
        let mut c = col(vec![
            floor_at(0.0, -30.0, 20.0, -60.0, 30.0),
            panel(-10.0, -30.0, 0.0, 0.0, 10.0),
        ]);
        c.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::flat_below(-5.0))));

        let mut seed: u64 = 0xB0D1_C0DE;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as u32 as f32 / u32::MAX as f32
        };
        let mut checked = 0usize;
        for tier in Tier::LADDER {
            let ledge = if tier == Tier::Preferred { tier.units() } else { 0.0 };
            for cell in [2.0f32, 8.0] {
                let t = Traversability::new(&c, tier.units(), cell, ledge, false);
                for _ in 0..200 {
                    let a = Point::new([rnd() * 70.0 - 35.0, rnd() * 100.0 - 65.0], 0.0);
                    let b = Point::new(
                        [a.xy[0] + rnd() * 16.0 - 8.0, a.xy[1] + rnd() * 16.0 - 8.0], 0.0);
                    assert_eq!(t.can_occupy_fast(a), t.can_occupy(a).is_ok(),
                        "occupy fast/cold disagreement at {a:?} tier {tier:?}");
                    assert_eq!(t.can_traverse_fast(a, b), t.can_traverse(a, b).is_ok(),
                        "traverse fast/cold disagreement {a:?} -> {b:?} tier {tier:?}");
                    checked += 2;
                }
            }
        }
        assert!(checked >= 1600);
    }

    /// The cold path names the RIGHT hazard, with a position: a wall reads Wall, a floor edge reads
    /// Floor, open water reads Water.
    #[test]
    fn diagnosis_names_the_hazard_and_the_position() {
        let mut c = col(vec![
            floor_at(0.0, -30.0, 20.0, -60.0, 30.0),
            panel(-10.0, -30.0, 0.0, 0.0, 10.0),
        ]);
        c.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::flat_below(-5.0))));

        // Wall: traversing east through the panel at east=-10.
        let t = Traversability::new(&c, PLAYER_RADIUS, 2.0, 0.0, false);
        let a = Point::new([-14.0, -10.0], 0.0);
        let b = Point::new([-6.0, -10.0], 0.0);
        let e = t.can_traverse(a, b).unwrap_err();
        assert_eq!(e.hazard, HazardKind::Wall);
        assert!((e.at[0] - (-10.0)).abs() < 1.5, "blockage should be at the panel: {:?}", e.at);

        // Floor: standing out past the plateau edge (east > 20, dry land side).
        let off = Point::new([28.0, 10.0], 0.0);
        let e = t.can_occupy(off).unwrap_err();
        assert_eq!(e.hazard, HazardKind::Floor);

        // Water: a preferred-tier margin probe reaching over the waterline names Water.
        let tp = Traversability::new(&c, Tier::Preferred.units(), 8.0, Tier::Preferred.units(), false);
        let shore = Point::new([0.0, -59.5], 0.0);
        match tp.can_occupy(shore) {
            Err(e) => assert!(matches!(e.hazard, HazardKind::Water | HazardKind::Floor),
                "shore refusal names a ground hazard: {e:?}"),
            Ok(()) => panic!("a point with water/void within the preferred margin must be refused"),
        }
    }

    /// **ROUTE-PARITY PIN (design PR-2's gate).** Re-plumbing the A* edge test / ledge margin /
    /// waypoint inset through `Traversability` must be BYTE-IDENTICAL re-plumbing: the emitted
    /// waypoints over these fixtures may not move by a single bit. The hash below was first recorded
    /// in the façade commit (3033ed4) — the re-plumbing itself, where routes were required NOT to
    /// change; on these fixtures the later field-wiring commit (17b101e: graded margin + hug cost)
    /// left it unchanged too, which is why the intended route change there is pinned by the hug and
    /// margin fixtures instead of by this hash. If this test goes red, the re-plumbing did something
    /// it was not supposed to.
    ///
    /// If a LATER change legitimately alters routes ON THESE FIXTURES (a new probe, a margin
    /// change), re-record the hash IN THE SAME COMMIT and say so in its message — this pin is for
    /// silent drift, not a freeze on tuning.
    #[test]
    fn facade_replumbing_is_byte_identical_route_parity() {
        fn hash_route(h: &mut u64, r: Option<Vec<[f32; 3]>>) {
            // FNV-1a over the exact f32 bit patterns; Option-ness folded in.
            const P: u64 = 0x100000001b3;
            match r {
                None => { *h ^= 0xdead; *h = h.wrapping_mul(P); }
                Some(ws) => for w in ws {
                    for c in w {
                        *h ^= c.to_bits() as u64;
                        *h = h.wrapping_mul(P);
                    }
                },
            }
        }
        // Fixture A: open plateau with real edges (generous tier's ledge margin is live near them).
        let plateau = col(vec![floor_at(0.0, -40.0, 40.0, -40.0, 40.0)]);
        // Fixture B: a 3u-wide corridor (generous pass fails, minimum tier threads it).
        let corridor = col(vec![
            floor_at(0.0, -40.0, 40.0, -20.0, 20.0),
            panel(0.0, -20.0, -1.5, 0.0, 10.0), // wall with a 3u slot at north ∈ (-1.5, 1.5)
            panel(0.0, 1.5, 20.0, 0.0, 10.0),
        ]);
        // Fixture C: a bend — a wall the route must detour around (the inset works its corner).
        let bend = col(vec![
            floor_at(0.0, -50.0, 50.0, -100.0, 100.0),
            panel(0.0, -100.0, 12.0, 0.0, 20.0),
        ]);

        let mut h: u64 = 0xcbf29ce484222325;
        for (c, pairs) in [
            (&plateau, [([-30.0, 0.0, 0.0], [30.0, 0.0, 0.0]), ([-30.0, -35.0, 0.0], [30.0, 35.0, 0.0])]),
            (&corridor, [([-20.0, 0.0, 0.0], [20.0, 0.0, 0.0]), ([-20.0, -10.0, 0.0], [20.0, 10.0, 0.0])]),
            (&bend, [([-40.0, 0.0, 0.0], [40.0, 0.0, 0.0]), ([-40.0, -50.0, 0.0], [40.0, 60.0, 0.0])]),
        ] {
            for (s, g) in pairs {
                hash_route(&mut h, c.find_path(s, g, PLAYER_RADIUS, &[], false));
                hash_route(&mut h, Some(c.find_path_local(s, g, 2.0, 40.0, 4.0).steer().to_vec()));
            }
        }
        assert_eq!(h, 0xb1e7db81be9e74ad_u64, "route parity broken: waypoints moved (new hash {h:#x})");
    }

    /// **ZERO DIAGNOSIS ON SUCCESS (design §5c #2).** A successful plan must never pay for a
    /// diagnosis: `find_path` on an open floor routes fine and this thread's cold-path counter does
    /// not move. (`find_path` runs on the calling thread, so a thread-local counter sees every
    /// diagnose it could trigger while staying blind to other tests running in parallel.)
    #[test]
    fn a_successful_plan_never_runs_the_cold_path() {
        let c = col(vec![floor_at(0.0, -60.0, 60.0, -60.0, 60.0)]);
        let before = DIAGNOSE_CALLS.with(|c| c.get());
        let route = c.find_path([-30.0, 0.0, 0.0], [30.0, 0.0, 0.0], PLAYER_RADIUS, &[], false);
        assert!(route.is_some(), "open floor must route");
        let after = DIAGNOSE_CALLS.with(|c| c.get());
        assert_eq!(before, after, "a successful plan ran the cold diagnose path {} times", after - before);
    }
}
