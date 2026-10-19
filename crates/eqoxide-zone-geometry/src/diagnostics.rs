//! Diagnostic snapshot types for the live traversability probe, consumed by the agent harness
//! (docs/specs/2026-09-21-agent-harness-separation-plan-zone-geometry.md, Task 2).

use serde::Serialize;

// ─────────────────────────────── live traversability probe ───────────────────────────────

/// The result of one radial spoke cast by `Collision::clearance_probe`.
///
/// The enum is not an `f32` because "nothing within the cap" and "geometry hit at exactly the cap"
/// are different facts. The first is a lower bound and the second is a distance. The saturated case
/// is a variant rather than a number so a consumer cannot mistake the cap for a distance.
/// [`SpokeReading::draw_len`] returns the length to draw. A consumer that reasons about the spoke
/// matches on the variant.
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

/// The source of the vertical position of a [`ClearanceProbe`].
///
/// `clearance_probe` casts its rays from the nearest floor, found with
/// `nearest_floor(ref_z, up = 3, down = 8)`. The two variants separate a found floor from a band
/// that contains no floor, so "no floor was found" is never reported as a floor height.
///
/// Both variants carry `reference_z`, the z of the character that the caller asked about. The
/// sample z is not necessarily the character z. A body embedded 1 u under a slab has a floor 1 u
/// above it, so the whole sample describes a point in the open air over the geometry that the
/// character is stuck inside.
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

/// The height that a [`ClearanceProbe`] casts its rays from, wrapped in a type that is not an
/// `f32`.
///
/// The field `ClearanceProbe::body` is evaluated at the character z while the remaining fields are
/// sampled at the anchor z. [`ProbeAnchor::z`] returns a `CastZ`, so
/// `body_placement([east, north, anchor.z()])` fails to compile with `error[E0308]` instead of
/// silently using the anchor z. The probe stores its own cast height as a `CastZ` for the same
/// reason.
///
/// The type raises the bar and does not close the hole. Outside this module three routes lead back
/// to a plain `f32`. They are [`CastZ::raw`] and the degenerate `+ 0.0` and `- 0.0` forms of the
/// `Add<f32>` and `Sub<f32>` impls. Those impls exist because a cast height is always offset before
/// a cast. A test keeps `body` at the character z. The type only forces each of the three routes to
/// be written out deliberately.
///
/// Inside this module the tuple field is a fourth route. The call site in `clearance_probe` cannot
/// reach the tuple field.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct CastZ(f32);

impl CastZ {
    // no public constructor, so a `CastZ` cannot be minted from an arbitrary height such as
    // `reference_z`. `ProbeAnchor::z` uses the module-private tuple constructor.

    /// Returns the raw height, for publishing and for comparing against a literal in a test.
    ///
    /// Passing the result to a placement query defeats the purpose of the type.
    #[inline]
    pub fn raw(self) -> f32 { self.0 }
}

impl std::ops::Add<f32> for CastZ {
    type Output = f32;
    #[inline]
    fn add(self, rhs: f32) -> f32 { self.0 + rhs }
}
impl std::ops::Sub<f32> for CastZ {
    type Output = f32;
    #[inline]
    fn sub(self, rhs: f32) -> f32 { self.0 - rhs }
}

impl ProbeAnchor {
    /// The height the rays were actually cast relative to (`Floor`'s floor, or the fallback).
    ///
    /// Returns a [`CastZ`] rather than an `f32` on purpose — see that type. A caller that wants to
    /// ask a question *about the character* must not reach for this value at all; the character's
    /// own height is [`ProbeAnchor::reference_z`].
    #[inline]
    pub fn z(self) -> CastZ {
        CastZ(match self {
            ProbeAnchor::Floor { z, .. } => z,
            ProbeAnchor::NoFloorInBand { reference_z } => reference_z,
        })
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
/// This is the movement controller's own `is_embedded` disjunction. The footprint ring is pierced by
/// geometry, **or** there is no floor anywhere within `GROUND_DEPTH` beneath its feet.
/// `Collision::body_placement` is the single definition, read by `movement::is_embedded` and by the
/// published clearance probe.
///
/// The probe must call the predicate at the character z rather than the anchor z. That property
/// belongs to one call site and the test
/// `body_is_measured_at_the_character_not_the_anchor_when_the_two_disagree` pins it with a scene
/// where the two verdicts differ. [`ProbeAnchor::z`] returns a [`CastZ`], so passing the anchor
/// height to this predicate is `error[E0308]`. `CastZ::raw` and the degenerate `+ 0.0` and `- 0.0`
/// forms still compile, and the test covers them.
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

// the wire token of `Placement` comes from `#[serde(rename_all = "snake_case")]`, so the type has
// no `as_str`. `the_json_encoding_keeps_the_distinctions` in `collision.rs` pins the tokens.

/// A live sample of the traversability model around one standing point, produced by
/// `Collision::clearance_probe`.
///
/// The sample contains the radial wall spokes (the rays that `ClearanceField::wall_at` aggregates
/// into the hug cost), the footprint ring, and the two graded field values that an external
/// planner reads for margin and hug logic. Consumers draw the sample and never cast the rays again.
///
/// # What is authoritative for what
///
/// The two halves of the payload answer different questions at different points, so a sample can
/// report open space in every direction for a character that the movement controller holds frozen
/// with `embedded_no_recovery`. The fields divide as follows.
///
/// * [`ClearanceProbe::body`] is the authoritative answer to **"does the controller's placement
///   test pass where this character actually is"**. It is the controller's own predicate, evaluated
///   at `anchor.reference_z()` — the character's actual height. It is **not** a claim about whether
///   the character can move: a non-`Placeable` verdict is the ENTRY CONDITION to the depenetration
///   net, which usually relocates the body and lets it keep going (see [`Placement`] for the
///   measurement). The published answer to "can it move" is `player.hold` on `/v1/observe/debug`.
/// * [`ClearanceProbe::wall_spokes`], [`ClearanceProbe::footprint_ok`] and the two `field_*`
///   values are the planner model, sampled at [`ClearanceProbe::anchor`]`.z()`. They answer
///   "how much room does an external planner see around this standing point". A
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
    /// 16 radial wall readings, CCW from +east, cast at `anchor.z()` + the planner probe heights
    /// ([`crate::body::Body::planner_probes`]). A saturated spoke is [`SpokeReading::ClearToCap`],
    /// never the number `cap`.
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

/// The swim state that a controller acted on during one tick.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct WaterDebug {
    /// Whether the controller drove a swim intent (`want_swim`).
    pub swimming: bool,
    /// The swim plane (`surface − float_depth`) that the controller steered against, when floating.
    pub swim_plane: Option<f32>,
}
