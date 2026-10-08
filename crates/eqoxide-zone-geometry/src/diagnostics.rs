//! Diagnostic snapshot types for the live traversability probe (#885), shared between
//! eqoxide-nav's A*-search-facing diagnostics and any external agent-harness consumer
//! (docs/specs/2026-09-21-agent-harness-separation-plan-zone-geometry.md, Task 2).

use serde::Serialize;

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

/// The height a [`ClearanceProbe`]'s rays were cast from — **a type, not an `f32`** (#885 review
/// round 2, R2-N1).
///
/// The whole of #885 is that `clearance.body` must be evaluated at the CHARACTER's z while the
/// planner half of the same payload is sampled at the ANCHOR's z. Round 1 shipped that as a
/// comment on one line of `clearance_probe`, and the reviewer's mutation — swapping that line's
/// `ref_z` for `anchor.z()` — compiled and stayed green, republishing exactly the #885 payload.
/// Round 2 pinned it with a test. This closes it one tier higher: `z()` hands back a `CastZ`, which
/// is not an `f32`, so `body_placement([east, north, anchor.z()])` is `error[E0308]` rather than a
/// silent wrong answer. The local the probe actually uses is a `CastZ` for the same reason — a bare
/// `f32` local would have re-admitted the mutant under a different spelling.
///
/// **Its honest limit.** This is a raised bar, not a closed hole. From OUTSIDE this module — which
/// includes `clearance_probe`, the call site that matters — THREE routes back to a plain `f32`
/// compile: [`CastZ::raw`], and the degenerate `+ 0.0` / `- 0.0` forms of the `Add<f32>`/`Sub<f32>`
/// impls (which exist because every legitimate use of a cast height is "offset it and cast from
/// there" — see the spoke loop). All three were measured RED on the tier-2 test (#885 review round
/// 3, re-derived in round 4), so that test is what keeps `body` at the character's z. What the type
/// contributes is narrower: the bare `anchor.z()` substitution is `error[E0308]`, so taking one of
/// the three routes has to be written out deliberately.
///
/// Inside this module the tuple field is a fourth route — ordinary newtype behaviour, and out of
/// reach from the call site: `floor_z.0` in `clearance_probe` is `error[E0616]` (measured).
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct CastZ(f32);

impl CastZ {
    // No `new`. Round 3 reported the `pub fn new(z: f32)` this had as dead API, and round 4 removed
    // it outright: `cargo check --workspace --all-targets --locked` is exit 0 with 0 warnings
    // without it, so there was no call site anywhere, tests included. A public constructor is a
    // FOURTH way to mint a `CastZ` from an arbitrary height — including from `reference_z`, the
    // exact confusion this type exists to prevent. `ProbeAnchor::z` uses the tuple constructor,
    // which is module-private.

    /// The raw height, for publishing and for comparing against a literal in a test. Reaching for
    /// this to feed a placement query is the mutation this type exists to make visible.
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
/// This is the movement controller's own `is_embedded` disjunction, not a nav opinion: the
/// footprint ring is pierced by geometry, **or** there is no floor anywhere within `GROUND_DEPTH`
/// beneath its feet. `Collision::body_placement` is the single definition — `movement::is_embedded`
/// reads it, and so does the published clearance probe. One predicate, two readers.
///
/// **What that does and does not establish.** It removes the second copy of the predicate; it does
/// not by itself make the probe evaluate it at the right POINT. The probe must call it at the
/// character's z rather than the anchor's, and that is a property of one call site, pinned by the
/// test `body_is_measured_at_the_character_not_the_anchor_when_the_two_disagree` (a scene where the
/// two verdicts genuinely differ). Since #885 review round 2 (R2-N1) the type system carries part
/// of it too: [`ProbeAnchor::z`] hands back a [`CastZ`], so feeding the anchor's height to this
/// predicate is `error[E0308]` — measured, both as a substitution and as a wrap. That is a raised
/// bar, not a closed hole; `CastZ::raw` and the degenerate `+ 0.0` / `- 0.0` forms still compile,
/// and the test is what kills them.
///
/// **This is an entry condition, not a freeze.** A non-`Placeable` verdict is what admits a body to
/// the depenetration net; the net usually relocates it and the body keeps moving. Measured on a dry
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
