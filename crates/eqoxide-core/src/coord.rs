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
/// falsehood). A rare *patrolling* Levitating NPC DOES get the offset baked in mid-route; the SPAWN
/// rule here still skips it (stationary-hover assumption), but its position updates are handled by
/// the stricter [`position_update_skips_wire_z_offset`], which subtracts for a moving Levitator
/// (#578 residual b — no longer an accepted under-correction).
pub const FLYMODE_FLYING: u8 = 1;
/// See [`FLYMODE_FLYING`]. Levitating airborne mobs are excepted from the Z-offset the same way.
pub const FLYMODE_LEVITATING: u8 = 2;
/// `LevitateWhileRunning=5` (EQEmu `common/emu_constants.h:297`) — a second levitating variant.
pub const FLYMODE_LEVITATE_WHILE_RUNNING: u8 = 5;

/// True when a `flymode`/GravityBehavior wire code means "levitating" for gravity purposes: either
/// plain `Levitating(2)` or `LevitateWhileRunning(5)`. Used for the SELF-player levitate model
/// (#529: gravity-off hover) — a levitating self-spawn zones in with one of these codes.
///
/// NOTE (#587 review): on the SPAWN-STRUCT path (`FillSpawnStruct`) a *client's* `flymode` byte is
/// only ever baked as `0` or `2` — `LevitateWhileRunning(5)` is a movement/anim distinction the
/// server does not serialize into the spawn struct — so the `== 5` arm is effectively dead there.
/// It is kept because this helper is shared with any wire path that CAN carry 5 (and because
/// treating 5 as levitating is never wrong for gravity), not because a self-spawn will present it.
pub fn is_levitating_flymode(flymode: u8) -> bool {
    flymode == FLYMODE_LEVITATING || flymode == FLYMODE_LEVITATE_WHILE_RUNNING
}

/// True when an entity's wire z is already at the reported datum (no server `GetZOffset` baked in),
/// so it must be stored as-is on ingest (no `WIRE_Z_OFFSET` shift) and not floor-snapped by the
/// renderer: boats (`is_boat`, keyed off race — `Mob::FixZ` early-returns for `GetIsBoat`) and
/// airborne mobs (`flymode` Flying/Levitating — see [`FLYMODE_FLYING`]). This backs the
/// `Entity::floating` classification — DERIVED from the entity's CURRENT `flymode` (refreshed at
/// runtime by `OP_SpawnAppearance` type-19, #578), NOT frozen at spawn. Position updates carry no
/// flymode of their own, so their Z conversion reads the cached `flymode` via the sibling
/// [`position_update_skips_wire_z_offset`].
pub fn skips_wire_z_offset(is_boat: bool, flymode: u8) -> bool {
    is_boat || flymode == FLYMODE_FLYING || flymode == FLYMODE_LEVITATING
}

/// Whether a **position update's** wire z is already at the reported datum (no server `GetZOffset`
/// baked in), so it must be stored as-is rather than shifted `−WIRE_Z_OFFSET`. This is the SIBLING
/// of [`skips_wire_z_offset`] for the ongoing-movement path, and it deliberately differs for
/// **Levitating(2)**:
///
/// `Mob::FixZ`/`GetFixedZ` early-return **only** for `GetIsBoat` and `flymode==Flying(1)` (EQEmu
/// `zone/waypoints.cpp:788-790,836-842`) — Levitating is NOT excepted. So a Levitating NPC that is
/// actively **patrolling** is routed through `UpdatePathGround` (`zone/mob_movement_manager.cpp:1090-1099`,
/// `opts.offset = GetZOffset()`), which bakes `+3.125` into every position it broadcasts, exactly
/// like a Ground NPC. A *stationary-hover* Levitating NPC never moves, so it sends no position
/// updates at all — meaning the mere existence of a position update for a Levitating entity is the
/// movement signal that it went through the offset-adding path. We therefore SKIP the offset for a
/// Levitating spawn z (via [`skips_wire_z_offset`], the stationary-hover case) but SUBTRACT it on its
/// position updates (this function, the patrol case) — resolving the #578 residual where a patrolling
/// Levitator rendered/reported ~3u too high.
///
/// Boats and Flying(1) skip on BOTH paths: `Mob::FixZ` early-returns for both, and a Flying NPC
/// pursuing a target is `PushFlyTo`'d to a raw destination with no offset
/// (`mob_movement_manager.cpp:1070-1077`). (Accepted residual, per EQEmu source review: a Flying NPC
/// that patrols with no line-of-sight target instead falls through `UpdatePathGround` and would
/// carry the offset — rare, and indistinguishable on the wire; we keep Flying an unconditional skip
/// to match `FixZ`'s literal `== Flying` check.)
pub fn position_update_skips_wire_z_offset(is_boat: bool, flymode: u8) -> bool {
    is_boat || flymode == FLYMODE_FLYING
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

#[cfg(test)]
mod tests {
    use super::*;

    /// #578 residual (b): the SPAWN/render rule and the POSITION-UPDATE rule agree on everything
    /// EXCEPT Levitating(2). A stationary-hover Levitator's spawn z has no offset (skip), but a
    /// patrolling Levitator's update z has GetZOffset baked in by UpdatePathGround (subtract). Flying
    /// and boats skip on both paths; Ground subtracts on both. Pinning the divergence here makes the
    /// #578 fix a pure-function property: revert `position_update_skips_wire_z_offset` back to
    /// `skips_wire_z_offset` and the Levitating case flips → RED.
    #[test]
    fn position_update_rule_diverges_from_spawn_rule_only_for_levitating() {
        // Ground: subtract on both.
        assert!(!skips_wire_z_offset(false, 0));
        assert!(!position_update_skips_wire_z_offset(false, 0));
        // Flying: skip on both (Mob::FixZ early-returns for Flying unconditionally).
        assert!(skips_wire_z_offset(false, FLYMODE_FLYING));
        assert!(position_update_skips_wire_z_offset(false, FLYMODE_FLYING));
        // Boat: skip on both (keyed off race, not movement).
        assert!(skips_wire_z_offset(true, 0));
        assert!(position_update_skips_wire_z_offset(true, 0));
        // Levitating: THE divergence — spawn skips (stationary hover), patrol update subtracts.
        assert!(skips_wire_z_offset(false, FLYMODE_LEVITATING),
            "a stationary Levitating spawn z carries no offset — skip");
        assert!(!position_update_skips_wire_z_offset(false, FLYMODE_LEVITATING),
            "#578(b): a PATROLLING Levitator's update z has GetZOffset baked in — must subtract");
    }

    /// #578 residual (a) at the datum level: the Z-datum decision is a pure function of the CURRENT
    /// flymode, so flipping flymode flips the treatment. This is the property that makes a runtime
    /// change honorable — nothing about the datum is frozen once flymode is known.
    #[test]
    fn z_datum_is_a_pure_function_of_current_flymode() {
        // Ground → subtract; take off (Flying) → skip; land again (Ground) → subtract.
        assert_eq!(wire_z_to_foot(100.0, skips_wire_z_offset(false, 0)), 100.0 - WIRE_Z_OFFSET);
        assert_eq!(wire_z_to_foot(100.0, skips_wire_z_offset(false, FLYMODE_FLYING)), 100.0);
        assert_eq!(wire_z_to_foot(100.0, skips_wire_z_offset(false, 0)), 100.0 - WIRE_Z_OFFSET);
    }
}
