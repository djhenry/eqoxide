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

/// EQ heading in degrees (0..360) for a movement delta in server axes.
/// EQ convention: heading 0 faces +Y (north) and increases counter-clockwise
/// (90 = -X = west, 180 = -Y = south, 270 = +X = east). A point at heading θ lies
/// at (east, north) = (-sinθ, cosθ), so θ = atan2(-east, north).
pub fn eq_heading(d_east: f32, d_north: f32) -> f32 {
    (-d_east).atan2(d_north).to_degrees().rem_euclid(360.0)
}
