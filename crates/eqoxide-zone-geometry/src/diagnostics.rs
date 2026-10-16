//! The movement controller's placement verdict, [`Placement`].

/// Whether the CONTROLLER can place a body at a point, and if not, which half of its test failed
/// (#885).
///
/// This is the movement controller's own `is_embedded` disjunction. The footprint ring is pierced by
/// geometry, **or** there is no floor anywhere within `GROUND_DEPTH` beneath its feet.
/// `Collision::body_placement` is the single definition, read by `movement::is_embedded`.
///
/// **This is an entry condition, not a freeze.** A non-`Placeable` verdict admits a body to the
/// depenetration net. The net usually relocates the body and the body keeps moving. Measured on a dry
/// `FootprintPierced` start (real ground, a slot of walls piercing the footprint ring): driving the
/// real `CharacterController` north at a constant 44 u/s wish for 180 steps of 1/60 s — a **132.00 u
/// ceiling** — moved it **131.28 u** with `hold() == None`, and its placement read `Placeable` by
/// the end, because the push-out ring moved it clear on the first frame. That figure is a property
/// of the DRIVER (it ran flat out for the whole run), not of the scene; the only thing it
/// establishes is that this verdict is not a freeze. Whether a character can move is `player.hold`
/// on `/v1/observe/debug`, not this field.
///
/// It is split into named variants rather than a `bool` because the two disjuncts are wildly
/// different worlds — "wedged in a slot" versus "standing over nothing".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
