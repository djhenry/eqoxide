//! The character's collision volume — [`Body`]/[`PLAYER_BODY`] — moved here from
//! `eqoxide-nav::traversability` because [`crate::collision::Collision::build_water_grid`] takes a
//! `&Body` and the lazy water-grid cache in `crate::collision` needs [`PLAYER_BODY`] to build one
//! (#378, design doc `docs/specs/2026-07-14-traversability-design.md`). Everything ELSE that reads
//! from this one truth — the planner's hazard predicates, the walker's contact probes — is
//! A*-search/controller-specific and stays in `eqoxide_nav::traversability`, which re-exports this
//! type as part of its own public surface.

/// The character's collision volume. THE single source of truth (#386 / design §2a-iv).
///
/// The planner's probes and the controller's contact rays are both derived from this one value.
/// Before this existed, the probe heights were re-declared in four places and the planner's top
/// probe (3.0) sat BELOW the controller's chest ray (4.0) — the #386 drift band.
#[derive(Clone, Copy, Debug)]
pub struct Body {
    /// Wall-collision radius, matched to the reference RoF2 client (`movement::PLAYER_RADIUS`).
    pub radius: f32,
    /// The controller's LOW contact ray, just above the feet (`movement::CharacterController::slide`
    /// `contact_probes()[0]`). The planner deliberately does NOT probe at this raw height: a probe
    /// this low would read every ≤2 u stair riser as a wall, and risers up to [`Body::step_up`] are
    /// climbed by the controller's step-up, not collided with. The planner's low probe is instead
    /// [`Body::feet_clr`] = `foot + step_up`, the exact height of the controller's RAISED step-slide
    /// contact ray — so the foot axis is one number, not two hand-tuned ones (#420).
    pub foot: f32,
    /// The controller's step-up reach (`movement::STEP_UP`): to climb a riser the controller raises
    /// its cylinder by `step_up` and re-casts its [`Body::foot`] contact ray, so the tallest LOW
    /// obstacle it can clear tops out at `foot + step_up`. THE shared foot-axis field (#420, the
    /// twin of the #386 chest unification): the planner's low probe [`Body::feet_clr`] is DERIVED
    /// from this same `foot + step_up` sum, so "planner clears the low band" and "controller steps
    /// the low band" cannot drift apart. Before this, the planner's low probe was the literal
    /// `STEP_UP + 0.5` (where the `0.5` was silently `foot`), free to diverge from the controller's
    /// real step capability — #420's permissive-planner lie. (#239)
    pub step_up: f32,
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
    /// SWIM GEOMETRY, half 1 (#359 / water design §2): how far below the water surface a swimmer's
    /// feet rest. The controller's buoyancy target AND the plane the planner must assume a swimmer
    /// occupies are both `surface_z − float_depth`. Before this field existed the value lived as
    /// TWO duplicated local consts in `movement.rs` the planner had never heard of (the #386
    /// disease): the planner sized water exits from the raw surface while the swimmer floated 2 u
    /// lower, so a "legal" exit riser was up to 4.5 u against a ~2.5 u step and the character
    /// bobbed at the waterline forever (#359).
    pub float_depth: f32,
    /// SWIM GEOMETRY, half 2 (#359 / water design §4c, option E3 — THE HAUL-OUT CONTRACT): the
    /// tallest ledge a swimmer can mount, measured from the WATER SURFACE. The planner's water→land
    /// exit cap (`nav::collision` WATER ASCENT edge) and the controller's haul-out capability are
    /// both this one number: the planner admits an exit only when the lip is ≤ `haul_out_up` above
    /// the surface, and the controller — driven to the surface by the nav swim-up (collided, feet
    /// never leave the water column) — mounts the residual riser with the swimming step-up
    /// (`STEP_UP` + `GROUND_SNAP_TOL` = 2.5 u capability, so 2.0 here leaves 0.5 u margin).
    pub haul_out_up: f32,
}

/// The one body every query derives from.
///
/// `chest` = 4.0 is the controller's contact height, VERBATIM (it has been 4.0 in `slide` since
/// the controller landed). The planner moved UP to it (from 3.0) — the conservative direction:
/// the planner may only refuse more than the controller collides with, never less.
pub const PLAYER_BODY: Body = Body {
    radius: eqoxide_core::physics::PLAYER_RADIUS,
    foot: 0.5,
    // The controller's real step-up reach; the planner's low probe is DERIVED as `foot + step_up`
    // (see `feet_clr`), so it is exactly the height of the controller's raised step-slide contact
    // ray. Numerically 0.5 + 2.0 = 2.5 (the historical `feet_clr`) — bound now, not coincidental.
    step_up: eqoxide_core::physics::STEP_UP,
    chest: 4.0,
    ring: 3.0,
    height: 6.0,
    // ~5u standing headroom; the controller's own chest contact ray sits at `chest` = 4.0, so a
    // body needs a shade above that to stand. Was `assets::NAV_AGENT_HEIGHT`, now `nav::collision::NAV_AGENT_HEIGHT`.
    agent_height: 5.0,
    // 1/sqrt(1 + MAX_WALK_GRADE²) with MAX_WALK_GRADE = 1.2. Was `assets::NAV_NEAR_HORIZONTAL`, now `nav::collision::NAV_NEAR_HORIZONTAL`.
    near_horizontal: 0.64,
    // The controller's historical FLOAT_DEPTH (body floats, head clears), verbatim — buoyancy
    // behaviour is numerically identical; declaring it here is the drift-unrepresentable move.
    float_depth: 2.0,
    // = STEP_UP (design §10 decision 1, owner-approved E3 default): within the swimming step-up's
    // 2.5 u capability with 0.5 u margin, and covers the qcat spawn-shaft ledge at
    // surface + 1.03 u (#329). Raising it beyond 2.0 requires a genuine mantle capability the
    // controller does not have — that would be its own design, not a constant tweak.
    haul_out_up: eqoxide_core::physics::STEP_UP,
};

impl Body {
    /// The PLANNER's LOW probe height (#420): the controller's [`Body::foot`] contact ray lifted by
    /// its real [`Body::step_up`] reach. NOT a stored constant — derived, so it is exactly the
    /// height of the controller's raised step-slide contact ray and CANNOT be set to a more
    /// permissive value. A low obstacle taller than this blocks the planner here AND defeats the
    /// controller's step-up; a shorter one passes both. That equality is the foot-axis honesty
    /// guarantee, and it is unrepresentable-as-false because there is one sum, read twice.
    #[inline]
    pub const fn feet_clr(&self) -> f32 { self.foot + self.step_up }
    /// The heights the PLANNER sweeps a walk edge at. Derived, not re-declared.
    #[inline]
    pub const fn planner_probes(&self) -> [f32; 2] { [self.feet_clr(), self.chest] }
    /// The heights the CONTROLLER casts its contact rays at. Derived, not re-declared.
    #[inline]
    pub const fn contact_probes(&self) -> [f32; 2] { [self.foot, self.chest] }
}
