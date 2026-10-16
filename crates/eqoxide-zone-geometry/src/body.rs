//! The character's collision volume, [`Body`] and [`PLAYER_BODY`].
//!
//! [`crate::collision::Collision::build_water_grid`] takes a `&Body`, and the lazy water-grid cache
//! in `crate::collision` builds one from [`PLAYER_BODY`].

/// The character's collision volume, the single source of truth for every body dimension.
///
/// The probe heights of an external planner and the contact rays of the movement controller are
/// both derived from this one value.
#[derive(Clone, Copy, Debug)]
pub struct Body {
    /// Wall-collision radius, matched to the reference RoF2 client (`movement::PLAYER_RADIUS`).
    pub radius: f32,
    /// The controller's LOW contact ray, just above the feet (`movement::CharacterController::slide`
    /// `contact_probes()[0]`). A planner probe this low would read every riser up to 2 u as a wall,
    /// and the controller climbs risers up to [`Body::step_up`] with its step-up instead of
    /// colliding with them. The planner low probe is [`Body::feet_clr`]. That height equals
    /// `foot + step_up`, the height of the controller's raised step-slide contact ray.
    pub foot: f32,
    /// The controller's step-up reach (`movement::STEP_UP`). To climb a riser the controller raises
    /// its cylinder by `step_up` and casts its [`Body::foot`] contact ray again, so the tallest low
    /// obstacle the controller clears tops out at `foot + step_up`. The planner low probe
    /// [`Body::feet_clr`] is derived from the same sum, so the low band the planner clears and the
    /// low band the controller steps cannot differ.
    pub step_up: f32,
    /// The top probe height, shared by two readers. The controller uses it as the chest contact ray
    /// (`movement::CharacterController::slide`) and an external planner uses it as the upper edge
    /// probe.
    pub chest: f32,
    /// The controller's depenetration and footprint ring height (`Collision::footprint_clear`).
    /// The ring height differs from `chest` because the ring samples the torso mid-band while the
    /// contact ray samples the widest blocking band.
    pub ring: f32,
    /// Total cylinder height, for documentation and future headroom probes. Geometry between
    /// `chest` and `height` is invisible to both an external planner and the controller. Neither
    /// refuses it, so very low ceilings are under-modelled. [`Body::agent_height`] covers standing
    /// headroom.
    pub height: f32,
    /// The vertical clearance a standing character needs above a surface for the surface to count
    /// as standing room. A surface with a solid roof closer than this is a ceiling, not ground. The
    /// value exceeds the slab gap of a real ceiling and stays below the height of a real room.
    /// `NAV_AGENT_HEIGHT` in `crate::collision` aliases this field.
    pub agent_height: f32,
    /// The minimum unit-normal `|z|` for a surface to be flat enough to stand on. A surface below
    /// this value is a wall or a steep slope. The value is tied to `MAX_WALK_GRADE` as
    /// `1/sqrt(1+1.2²) ≈ 0.64`. `NAV_NEAR_HORIZONTAL` in `crate::collision` aliases this field.
    pub near_horizontal: f32,
    /// Swim geometry, half 1. The distance below the water surface at which a swimmer's feet rest.
    /// The controller buoyancy target and the plane an external planner assumes a swimmer occupies
    /// are both `surface_z − float_depth`.
    pub float_depth: f32,
    /// Swim geometry, half 2 (the haul-out contract). The tallest ledge a swimmer can mount,
    /// measured from the water surface. An external planner admits a water to land exit only when
    /// the lip is at most `haul_out_up` above the surface. The controller mounts the residual riser
    /// with the swimming step-up (`STEP_UP` + `GROUND_SNAP_TOL` = 2.5 u capability, so 2.0 here
    /// leaves a 0.5 u margin).
    pub haul_out_up: f32,
}

/// The one body every query derives from.
///
/// `chest` = 4.0 is the controller contact height used in `slide`.
pub const PLAYER_BODY: Body = Body {
    radius: eqoxide_core::physics::PLAYER_RADIUS,
    foot: 0.5,
    // the controller's step-up reach, so `feet_clr` equals the height of the controller's raised
    // step-slide contact ray (0.5 + 2.0 = 2.5)
    step_up: eqoxide_core::physics::STEP_UP,
    chest: 4.0,
    ring: 3.0,
    height: 6.0,
    // about 5 u of standing headroom, above the controller chest contact ray at `chest` = 4.0
    agent_height: 5.0,
    // 1/sqrt(1 + MAX_WALK_GRADE²) with MAX_WALK_GRADE = 1.2
    near_horizontal: 0.64,
    // body floats with the head clear of the water
    float_depth: 2.0,
    // equals STEP_UP, within the 2.5 u swimming step-up capability with a 0.5 u margin. A larger
    // value needs a mantle capability the controller does not have.
    haul_out_up: eqoxide_core::physics::STEP_UP,
};

impl Body {
    /// Returns the planner low probe height, the [`Body::foot`] contact ray lifted by the
    /// [`Body::step_up`] reach.
    ///
    /// The value is derived, so it equals the height of the controller's raised step-slide contact
    /// ray. A low obstacle taller than this height blocks the planner and defeats the controller
    /// step-up, and a shorter one passes both.
    #[inline]
    pub const fn feet_clr(&self) -> f32 { self.foot + self.step_up }
    /// Returns the heights an external planner sweeps a walk edge at.
    #[inline]
    pub const fn planner_probes(&self) -> [f32; 2] { [self.feet_clr(), self.chest] }
    /// Returns the heights the controller casts its contact rays at.
    #[inline]
    pub const fn contact_probes(&self) -> [f32; 2] { [self.foot, self.chest] }
}
