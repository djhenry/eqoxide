//! Climbable surfaces (ladders) — volume derivation and the planner's climb edge (#309).
//!
//! # Why this exists
//!
//! Crushbone's moat is a trap for a character with only the native ~2u step-up/haul-out. Measured
//! against the shipped assets: the water surface sits at z ≈ −11, the moat rim at z ≈ −1.0…0.0 —
//! **~10 units of effectively vertical wall**, five times what [`crate::traversability::PLAYER_BODY`]
//! can surmount. There is no haul-out anywhere on the ring. Five `LADDER14` objects are placed
//! evenly around it, each spanning the moat floor (−25.6) to the rim (−0.66).
//!
//! The native RoF2 client CAN climb these — verified by the repo owner running the retail binary.
//! What is NOT known is *how*: the decompiled client contains no `climb` string, no climb state,
//! and no ladder-specific movement path. The one ladder-distinguishing datum in `eqgame.exe` is a
//! name-prefix classification in the zone-object constructor —
//! `__strnicmp(name, "LADDER", 6)` → actor category 6 (default 5, `GRASS_` → 7) — whose *consumer*
//! could not be identified (no type info to resolve the vtable slot). See
//! `eq_kb/ladders-and-climbing.md` for the full evidence trail.
//!
//! # What that means for this module
//!
//! This is a **parity feature with an unverified mechanism**, not an invention:
//!
//! * The **trigger** is not invented. [`is_climbable_name`] matches the `LADDER` prefix because
//!   that is precisely what the client does to classify these actors. Reusing the client's own
//!   classifier is the most defensible choice available while its consumer is unknown.
//! * The **motion constants** ([`CLIMB_SPEED`], the mount tolerance, the dismount window) ARE
//!   guesses. They are behaviorally plausible and marked as such — nothing here should be cited as
//!   client-derived. They are the first thing to correct once someone measures the retail client.
//!
//! Because part of this is unverified, every route that uses a climb edge is COUNTED and surfaced
//! to agents as `nav_climb` (see [`crate::collision::Collision::climb_plans`]). An agent must never
//! be silently handed a route that depends on a mechanic we cannot yet fully justify — the honesty
//! contract that governs `nav_tight`/`nav_support` applies here with more force, not less.
//!
//! # Why a climb EDGE and not walkable geometry
//!
//! The obvious cheap alternative — bake the ladder into the collision mesh as a walkable ramp — was
//! rejected. The renderer draws a 1.15u-thick vertical panel; collision would report a 1.2-grade
//! slope. That is the same class of lie as the ceiling-being-treated-as-floor bug (#229/#329), and
//! it would corrupt every consumer of the collision mesh, not just pathing. Instead the grade test
//! ([`crate::collision::MAX_WALK_GRADE`]) is left strictly alone — it is CORRECT to refuse a
//! vertical face — and A* gains an ADDITIONAL edge type, in the shape of the teleport-pad edge
//! (#403), that expresses the discontinuous link terrain-follow neighbours cannot.

use eqoxide_assets::ObjectModel;
use eqoxide_core::physics::PLAYER_RADIUS;

/// Vertical climb rate, EQ units/second. **UNVERIFIED** — see the module doc.
///
/// Deliberately well under `RUN_SPEED` (44.0): a climb should read as slower than running, and
/// keeping per-tick displacement ordinary also keeps the server's warp detection quiet. Crushbone's
/// 24.9u ladder therefore takes ≈1.8s to ascend.
pub const CLIMB_SPEED: f32 = 14.0;

/// How far a body may be from a climb volume and still mount it, beyond [`PLAYER_RADIUS`].
///
/// A ladder is a ~1.15u-thick panel; requiring precise contact with it would make mounting
/// frustrating and would make the planner's cell-quantised route (8u coarse cells) unable to
/// guarantee a mountable waypoint. Capture is deliberately forgiving.
pub const CLIMB_REACH: f32 = 1.5;

/// Vertical window around a climb volume's top within which a dismount floor is accepted.
///
/// Crushbone: ladder top −0.66, rim −1.0…0.0, so ±2.0 finds the rim comfortably. Tight enough that
/// a ladder ending in mid-air (no floor at the top) yields NO edge rather than a route that strands
/// the character — see [`crate::collision::Collision::resolve_climb_edges`].
pub const DISMOUNT_Z_TOL: f32 = 2.0;

/// Does this object name mark a climbable surface?
///
/// Case-insensitive `LADDER` prefix — the SAME test the native client applies in its zone-object
/// constructor (`eqgame.exe.c:158395`, `__strnicmp(name, "LADDER", 6)`) to assign actor category 6.
/// Matching the client's own classifier is why this is parity rather than invention; see the module
/// doc for what remains unverified.
///
/// Measured over the shipped assets: 82 WLD members across ~40 zones carry `LADDER*` names, so this
/// is a game-wide rule, not a Crushbone special case.
pub fn is_climbable_name(name: &str) -> bool {
    name.len() >= 6 && name[..6].eq_ignore_ascii_case("LADDER")
}

/// A world-space volume a character may climb, in [`crate::collision::Collision`] coordinates.
///
/// Axis-aligned on purpose. The underlying panel is thin and arbitrarily yawed (Crushbone's five
/// ladders span 1.63×5.55 to 5.02×3.19 in world AABB), and the volume is expanded by
/// `PLAYER_RADIUS + CLIMB_REACH` for forgiving capture anyway — an oriented box would add rotation
/// bookkeeping for precision the capture margin immediately discards.
#[derive(Clone, Debug, PartialEq)]
pub struct ClimbVolume {
    /// Object name this came from (e.g. `LADDER14`) — diagnostics and tests.
    pub name: String,
    /// Inclusive horizontal bounds, already expanded for capture: `[a0_lo, a1_lo]`.
    pub lo: [f32; 2],
    /// Inclusive horizontal bounds, already expanded for capture: `[a0_hi, a1_hi]`.
    pub hi: [f32; 2],
    /// Bottom of the climbable span (the raw mesh extent, NOT expanded).
    pub foot_z: f32,
    /// Top of the climbable span (the raw mesh extent, NOT expanded).
    pub top_z: f32,
}

impl ClimbVolume {
    /// Is `p` (world `[a0, a1, up]`) inside the capture footprint and within the climbable span?
    ///
    /// The z test is generous at the bottom: a character FLOATING in the moat sits at the water
    /// surface (−11), far above the ladder's foot (−25.6), and must still be able to mount. Anything
    /// from the foot to the top counts.
    pub fn contains(&self, p: [f32; 3]) -> bool {
        p[0] >= self.lo[0] && p[0] <= self.hi[0]
            && p[1] >= self.lo[1] && p[1] <= self.hi[1]
            && p[2] >= self.foot_z - DISMOUNT_Z_TOL && p[2] <= self.top_z
    }

    /// Horizontal centre of the volume.
    pub fn center(&self) -> [f32; 2] {
        [(self.lo[0] + self.hi[0]) * 0.5, (self.lo[1] + self.hi[1]) * 0.5]
    }
}

/// Transform a model-local point by a column-major 4×4 instance matrix.
///
/// Written out rather than pulled from `glam` on purpose: this crate deliberately carries no
/// linear-algebra dependency (its `Cargo.toml`: "the pathfinding math is all plain `[f32; 3]`
/// arrays, so it needs no glam"), and one point transform is not worth breaking that. Same result
/// as `Mat4::from_cols_array_2d(m).transform_point3(p)` — which is exactly what
/// [`eqoxide_assets::expand_objects`] applies to the triangles the collision mesh is built from, so
/// the volumes and the geometry cannot drift apart.
fn transform_point(m: &[[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * p[0] + m[1][0] * p[1] + m[2][0] * p[2] + m[3][0],
        m[0][1] * p[0] + m[1][1] * p[1] + m[2][1] * p[2] + m[3][1],
        m[0][2] * p[0] + m[1][2] * p[1] + m[2][2] * p[2] + m[3][2],
    ]
}

/// Derive climb volumes from a zone's placed objects.
///
/// Each instance of a climbable-named model contributes one volume: the model-local mesh vertices
/// are transformed by that instance's 4×4 (which carries the placement's rotation AND scale — the
/// Crushbone ladders are scale 1.6, and ignoring it understates them by 60%), then permuted into
/// collision space by the SAME `(x, y, z) → (z, x, y)` map [`crate::collision::Collision::build`]
/// applies to every triangle. Deriving both from one permutation is what keeps the volumes in the
/// collision grid's frame by construction rather than by a comment.
///
/// `MeshData::center` is deliberately NOT added. `expand_objects` — whose output `Collision::build`
/// actually ingests — drops it too (it emits `center: [0,0,0]`), and object primitives come out of
/// `ZoneAssets::from_glb` with a zero center anyway. Adding it here would put the climb volume
/// somewhere the collision triangles are not.
pub fn volumes_from_objects(objects: &[ObjectModel]) -> Vec<ClimbVolume> {
    let mut out = Vec::new();
    let pad = PLAYER_RADIUS + CLIMB_REACH;
    for model in objects.iter().filter(|m| is_climbable_name(&m.name)) {
        for inst in &model.instances {
            let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
            let mut any = false;
            for mesh in &model.meshes {
                for &p in &mesh.positions {
                    let w = transform_point(inst, p);
                    // (x, y, z) -> (z, x, y): identical to Collision::build's triangle ingestion.
                    let w = [w[2], w[0], w[1]];
                    for k in 0..3 {
                        if w[k] < lo[k] { lo[k] = w[k]; }
                        if w[k] > hi[k] { hi[k] = w[k]; }
                    }
                    any = true;
                }
            }
            if !any { continue; }
            out.push(ClimbVolume {
                name: model.name.clone(),
                lo: [lo[0] - pad, lo[1] - pad],
                hi: [hi[0] + pad, hi[1] + pad],
                foot_z: lo[2],
                top_z: hi[2],
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_assets::{MeshData, RenderMode};

    fn panel(name: &str, height: f32) -> ObjectModel {
        // Mesh source order is `[north, up, east]` — the same order `Collision::build`'s own tests
        // state and permute from. A thin vertical panel: 0.7 thick across north, `height` tall,
        // 3.5 wide across east. Centred horizontally and rising from up = 0, so the placement
        // translation IS the volume's horizontal centre and its foot, and the assertions below read
        // as exact placement arithmetic.
        let mut positions = Vec::new();
        for &x in &[-0.35f32, 0.35] {
            for &y in &[0.0f32, height] {
                for &z in &[-1.75f32, 1.75] {
                    positions.push([x, y, z]);
                }
            }
        }
        let n = positions.len();
        ObjectModel {
            name: name.into(),
            meshes: vec![MeshData {
                positions,
                normals: vec![[0.0, 1.0, 0.0]; n],
                uvs: vec![[0.0, 0.0]; n],
                indices: (0..n as u32).collect(),
                texture_name: None,
                base_color: [1.0; 4],
                center: [0.0; 3],
                render_mode: RenderMode::Opaque,
                anim: None,
            }],
            instances: Vec::new(),
        }
    }

    /// A scale+translate instance matrix, in the mesh's own `[north, up, east]` source order.
    fn place(north: f32, up: f32, east: f32, scale: f32) -> [[f32; 4]; 4] {
        [[scale, 0., 0., 0.], [0., scale, 0., 0.], [0., 0., scale, 0.], [north, up, east, 1.]]
    }

    #[test]
    fn ladder_prefix_matches_the_clients_own_classifier() {
        assert!(is_climbable_name("LADDER14"));
        assert!(is_climbable_name("ladder03_dmspritedef"));
        assert!(is_climbable_name("LADDER"));
        // Not ladders — including the sibling category the same client function assigns (GRASS_ → 7).
        assert!(!is_climbable_name("GRASS_01"));
        assert!(!is_climbable_name("LADD"));
        assert!(!is_climbable_name(""));
        assert!(!is_climbable_name("STEPLADDER"), "the client tests a PREFIX, not a substring");
    }

    #[test]
    fn non_climbable_objects_yield_no_volumes() {
        let mut m = panel("URN5", 4.0);
        m.instances.push(place(0.0, 0.0, 0.0, 1.0));
        assert!(volumes_from_objects(&[m]).is_empty());
    }

    /// The scale on the instance matrix MUST be applied: Crushbone's ladders are scale 1.6, and a
    /// volume built from the raw mesh would end 10 units below the rim — under the waterline,
    /// exactly where a climb is useless.
    #[test]
    fn volume_applies_instance_scale_and_lands_in_collision_space() {
        let mut m = panel("LADDER14", 15.58);
        // Crushbone ladder #1: north −16.21, east −73.28, foot z −23.99, scale 1.6.
        m.instances.push(place(-16.21, -23.99, -73.28, 1.6));
        let v = volumes_from_objects(&[m]);
        assert_eq!(v.len(), 1);
        let v = &v[0];
        assert_eq!(v.name, "LADDER14");
        // Height 15.58 * 1.6 = 24.93, from the placement z upward.
        assert!((v.foot_z - (-23.99)).abs() < 0.01, "foot_z {}", v.foot_z);
        assert!((v.top_z - (-23.99 + 15.58 * 1.6)).abs() < 0.01, "top_z {}", v.top_z);
        assert!(v.top_z > -1.0, "a scaled ladder must reach the rim, got {}", v.top_z);
        // Collision space is (x,y,z)->(z,x,y): axis 0 = the source z (east), axis 1 = the source x (north).
        let c = v.center();
        assert!((c[0] - (-73.28)).abs() < 0.5, "axis0 {}", c[0]);
        assert!((c[1] - (-16.21)).abs() < 0.5, "axis1 {}", c[1]);
    }

    #[test]
    fn capture_footprint_is_expanded_but_the_climb_span_is_not() {
        let mut m = panel("LADDER14", 10.0);
        m.instances.push(place(0.0, 0.0, 0.0, 1.0));
        let v = &volumes_from_objects(&[m])[0];
        // Raw panel half-width is 1.75; expanded by PLAYER_RADIUS + CLIMB_REACH.
        assert!((v.hi[0] - (1.75 + PLAYER_RADIUS + CLIMB_REACH)).abs() < 0.01);
        // The vertical span stays the mesh's own — padding it would invent ladder that isn't there.
        assert!((v.foot_z - 0.0).abs() < 0.01);
        assert!((v.top_z - 10.0).abs() < 0.01);
    }

    /// A floating character sits ~14u above the moat floor. It must still read as "on the ladder",
    /// or the moat stays a trap for exactly the case #309 is about.
    #[test]
    fn a_floating_body_well_above_the_foot_can_still_mount() {
        let mut m = panel("LADDER14", 15.58);
        m.instances.push(place(-16.21, -23.99, -73.28, 1.6));
        let v = &volumes_from_objects(&[m])[0];
        assert!(v.contains([-73.28, -16.21, -11.0]), "water-surface mount must be inside the volume");
        assert!(v.contains([-73.28, -16.21, -25.0]), "standing at the foot must be inside");
        assert!(!v.contains([-73.28, -16.21, 40.0]), "far above the top is not on the ladder");
        assert!(!v.contains([0.0, 0.0, -11.0]), "far away horizontally is not on the ladder");
    }

    #[test]
    fn every_instance_becomes_its_own_volume() {
        let mut m = panel("LADDER14", 15.58);
        // The five `LADDER14` placements read out of the shipped crushbone.glb, verbatim.
        for (north, east) in [(-16.21, -73.28), (295.64, 170.11), (-32.27, 334.62),
                              (-269.09, 464.07), (-371.97, 575.02)] {
            m.instances.push(place(north, -23.99, east, 1.6));
        }
        assert_eq!(volumes_from_objects(&[m]).len(), 5, "Crushbone has five ladders");
    }
}
