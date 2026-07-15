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
//! free: it is not computed until something has already failed. The two forms MUST agree —
//! `fast == diagnostic.is_ok()` — and the property test in this module pins that. That agreement
//! IS the honesty guarantee: the fast path can never say "clear" about a spot the diagnostic path
//! would refuse, so the diagnosis can never be a confident falsehood about the plan that failed.

use crate::assets::Collision;

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
    /// ceilings; `assets::NAV_AGENT_HEIGHT` (5.0) is what defends standing headroom.
    pub height: f32,
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
/// (The legacy `find_path*` plumbing still carries `radius: f32` for its external callers; it is
/// clamped to `Tier::Minimum.units()` at the ladder — `search_tiered` — which is built from
/// [`Tier::LADDER`]. Migrating those signatures to `Tier` outright is deliberately deferred; the
/// clamp plus this ladder keeps #310 unrepresentable at the only place a tier is chosen.)
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
            Tier::Preferred => crate::assets::NAV_PREFERRED_CLEARANCE,
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

/// Count of cold-path (`diagnose`) evaluations ON THIS THREAD, for the "zero diagnosis on success"
/// proof (design §5c). Test-only, and thread-local on purpose: the cargo test harness runs tests in
/// parallel, and a global counter would let another test's legitimate diagnoses pollute the
/// zero-on-success assertion.
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

    /// `ledge_margin` of ground all around (a drop, a lip, open water all read as "no ground").
    /// `true` when this plan asked for no margin (the minimum tier / a floating plan).
    #[inline]
    fn occupy_margin_ok(&self, p: Point) -> bool {
        if self.ledge_margin <= 0.0 || self.floating {
            return true;
        }
        self.col.ground_margin_ok(p.xy[0], p.xy[1], p.floor_z, self.ledge_margin)
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

    /// Cold-path only: name the hazard that broke the ground margin around `p` (the first probe
    /// direction with no floor, classified water-vs-floor at that spot).
    fn margin_hazard(&self, p: Point) -> HazardKind {
        let m = self.ledge_margin;
        for (dx, dy) in [(m, 0.0), (-m, 0.0), (0.0, m), (0.0, -m)] {
            let (x, y) = (p.xy[0] + dx, p.xy[1] + dy);
            let ok = self.col.nearest_floor(x, y, p.floor_z, 3.0, 8.0)
                .is_some_and(|f| (f - p.floor_z).abs() <= 8.0);
            if !ok {
                return self.ground_hazard([x, y], p.floor_z);
            }
        }
        HazardKind::Floor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{Collision, MeshData, RenderMode, ZoneAssets};
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
