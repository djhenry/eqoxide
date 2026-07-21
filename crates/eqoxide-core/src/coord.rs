//! Low-level EQ coordinate-space math shared by render, movement, and nav. Deliberately dependency-
//! free (no `assets`/`eq_net`/`nav` types) so it can be called from any of them without creating a
//! layering cycle.

/// The EQ **wire Z datum offset**: how far a character's wire/server `z` coordinate sits ABOVE
/// the floor its feet stand on (#522).
///
/// EQ's character `z` is the position of the **model origin**, which for humanoids is ~3.1u above
/// the feet — it is NOT foot/floor level. Measured ground truth (gfaydark, 2026-07-17):
/// - A native RoF2 player standing on the Kelethin plank (collision top 73.97) reports z = 77.0.
/// - Every static Kelethin NPC reports `wire_z − collision_floor ≈ +3.03` (server FixZ places NPCs
///   at `FindBestZ + Mob::GetZOffset()`, whose default is 3.125 — EQEmu zone/mob.cpp).
/// - EQEmu `#goto` placed a Human PC at 77.0957 = server floor 73.9707 + 3.125.
///
/// eqoxide's `CharacterController` (and `gs.player_z` mirrored from it) is FOOT-level — the height
/// of the collision floor under the character. Every value crossing the wire must therefore be
/// converted: **outbound** `wire_z = foot_z + WIRE_Z_OFFSET`; **inbound self-position**
/// `foot_z = wire_z − WIRE_Z_OFFSET`. Without this, observers (native clients) render the eqoxide
/// character's feet `WIRE_Z_OFFSET` BELOW the floor — the #522 "clips through the Kelethin plank"
/// defect — and server repositions (`#goto`, zone-in) arrive 3.1u above the floor, causing phantom
/// falls and the #516 "contested-Z" standoff (server and controller were both right, in different
/// datums).
///
/// 3.125 is EQEmu's `Mob::GetZOffset()` default (not size-scaled), correct for playable humanoids;
/// per-model refinement (e.g. gnome vs ogre) can layer on later without changing the datum rule.
///
/// **The datum discipline: FOOT everywhere except the wire.** Everything eqoxide holds and reports —
/// `gs.player_z`, the controller, nav, collision, every stored ENTITY z (converted wire→foot on
/// ingest in `packet_handler`), and every agent-facing field (`/player` `pos_up`, `/observe`,
/// `/observe/entities`) — is FOOT. The `± WIRE_Z_OFFSET` conversion happens ONLY at the packet edge.
/// One datum end to end means a position the agent READS can be fed straight back into `goto`/coords
/// with no 3u skew, self reads at the same height as another player on the same plank, and a
/// goto-by-name goal lands on the floor instead of 3u above it (a goal-z/floor-tier mismatch wedges
/// nav). Boats/floating entities skip the server's Z-offset (`Mob::FixZ` early-returns for them), so
/// their wire z is already surface-level and is NOT shifted on ingest.
pub const WIRE_Z_OFFSET: f32 = 3.125;

/// EQ `flymode` (GravityBehavior) wire codes (EQEmu `common/emu_constants.h:297`):
/// Ground=0, Flying=1, Levitating=2, Water=3, Floating=4, LevitateWhileRunning=5.
///
/// A grounded mob's wire z carries the server's `GetZOffset` (+3.125, the model-origin datum) — it
/// is baked in by `Mob::FixZ`/`UpdatePathGround` (EQEmu `zone/waypoints.cpp`,
/// `zone/mob_movement_manager.cpp`). But an **airborne** mob's wire z does NOT: a spawn's z is
/// serialized straight from its DB row with no offset (`Spawn2::Process` / `Mob::FillSpawnStruct`),
/// `Mob::FixZ` early-returns for `flymode==Flying`, and the dominant real-world authoring pattern for
/// both Flying and Levitating NPCs is a stationary hover that never routes through the offset-adding
/// path at all. So Flying(1) and Levitating(2) wire z is already at the reported datum and must NOT
/// have `WIRE_Z_OFFSET` subtracted on ingest — otherwise they decode ~3u LOW (#548, an agent-honesty
/// falsehood). (Residual: a rare *patrolling* Levitating NPC does get the offset baked in mid-route,
/// so the static rule under-corrects for that case — accepted, per the EQEmu source review.)
pub const FLYMODE_FLYING: u8 = 1;
/// See [`FLYMODE_FLYING`]. Levitating airborne mobs are excepted from the Z-offset the same way.
pub const FLYMODE_LEVITATING: u8 = 2;

/// True when an entity's wire z is already at the reported datum (no server `GetZOffset` baked in),
/// so it must be stored as-is on ingest (no `WIRE_Z_OFFSET` shift) and not floor-snapped by the
/// renderer: boats (`is_boat`, keyed off race — `Mob::FixZ` early-returns for `GetIsBoat`) and
/// airborne mobs (`flymode` Flying/Levitating — see [`FLYMODE_FLYING`]). This is the
/// `Entity.floating` classification — computed once at spawn and reused for every later position
/// update, which carry no flymode of their own.
pub fn skips_wire_z_offset(is_boat: bool, flymode: u8) -> bool {
    is_boat || flymode == FLYMODE_FLYING || flymode == FLYMODE_LEVITATING
}

/// Convert an inbound wire z (model-origin datum, `foot + WIRE_Z_OFFSET`) to eqoxide's internal FOOT
/// datum. Entities that skip the server Z-offset (`floating`: boats/flying — see
/// [`skips_wire_z_offset`]) already sit at the correct datum on the wire and pass through unchanged;
/// a non-floating (grounded) entity has the offset subtracted. Centralizing the sign here keeps the
/// spawn and position-update ingest paths from ever diverging.
pub fn wire_z_to_foot(wire_z: f32, floating: bool) -> f32 {
    if floating { wire_z } else { wire_z - WIRE_Z_OFFSET }
}

/// EQ heading in degrees (0..360) for a movement delta in server axes.
/// EQ convention: heading 0 faces +Y (north) and increases counter-clockwise
/// (90 = -X = west, 180 = -Y = south, 270 = +X = east). A point at heading θ lies
/// at (east, north) = (-sinθ, cosθ), so θ = atan2(-east, north).
pub fn eq_heading(d_east: f32, d_north: f32) -> f32 {
    (-d_east).atan2(d_north).to_degrees().rem_euclid(360.0)
}
