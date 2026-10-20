//! Hand-authored mini-zone fixtures for the swim and depenetration tests.
//!
//! Each scene is geometry built in code, with no game-derived content, so the tests that use it
//! run on any machine without baked zone assets. A scene pins the physical claim a test makes. It
//! does not show that a shipped zone exhibits the same behavior. `tests/water_capability.rs`
//! checks that against baked assets.
//!
//! Each scene is built so the situation under test is the only thing that can produce the asserted
//! outcome, and each control scene (`sealed_pocket_without_lid`, `open_water_with_floor_in_reach`)
//! holds everything else fixed and removes only the feature under test. The builder docs state
//! why the character has no route around that feature.
//!
//! # Coordinates
//!
//! Scene builders take and return **server coords** `[east, north, up]`, the same frame the
//! controller and `Collision` use. The `MeshData` positions underneath are GLB/libeq space
//! `[north, up, east]` (that is the swap `Collision::build` undoes); the helpers here do the
//! conversion so no scene has to think about it.

#![allow(dead_code)] // each test binary uses a subset of the scenes

use eqoxide::assets::{MeshData, RenderMode, ZoneAssets};
use eqoxide_zone_geometry::collision::Collision;
use eqoxide::region_map::RegionMap;

// ─────────────────────────────── primitives ───────────────────────────────

/// One quad from four GLB-space `[north, up, east]` corners.
fn quad(v: [[f32; 3]; 4]) -> MeshData {
    MeshData {
        positions: v.to_vec(),
        normals: vec![],
        uvs: vec![],
        indices: vec![0, 1, 2, 0, 2, 3],
        texture_name: None,
        base_color: [1.0; 4],
        center: [0.0; 3],
        render_mode: RenderMode::Opaque,
        anim: None,
    }
}

/// A horizontal, ZERO-THICKNESS quad at height `z` spanning east `[e0,e1]` × north `[n0,n1]`.
/// Collision is triangles, so a single quad is a fully solid surface from both sides; nothing in
/// this file has a thickness, and no scene depends on one.
pub fn floor(z: f32, e0: f32, e1: f32, n0: f32, n1: f32) -> MeshData {
    quad([[n0, z, e0], [n1, z, e0], [n1, z, e1], [n0, z, e1]])
}

/// A vertical panel in the plane `east = e`, spanning north `[n0,n1]` × height `[z0,z1]`.
pub fn wall_ew(e: f32, n0: f32, n1: f32, z0: f32, z1: f32) -> MeshData {
    quad([[n0, z0, e], [n1, z0, e], [n1, z1, e], [n0, z1, e]])
}

/// A vertical panel in the plane `north = n`, spanning east `[e0,e1]` × height `[z0,z1]`.
pub fn wall_ns(n: f32, e0: f32, e1: f32, z0: f32, z1: f32) -> MeshData {
    quad([[n, z0, e0], [n, z0, e1], [n, z1, e1], [n, z1, e0]])
}

/// The four vertical walls of an axis-aligned room, `z0`..`z1` tall.
pub fn room_walls(e0: f32, e1: f32, n0: f32, n1: f32, z0: f32, z1: f32) -> Vec<MeshData> {
    vec![
        wall_ew(e0, n0, n1, z0, z1),
        wall_ew(e1, n0, n1, z0, z1),
        wall_ns(n0, e0, e1, z0, z1),
        wall_ns(n1, e0, e1, z0, z1),
    ]
}

/// Build a `Collision` from hand-authored meshes, with an optional hand-authored water map.
/// `cell` is the broad-phase grid cell (32.0 everywhere else in the tree).
pub fn collision(meshes: Vec<MeshData>, water: Option<RegionMap>) -> Collision {
    let mut c = Collision::build(&ZoneAssets { terrain: meshes, objects: vec![], textures: vec![] }, 32.0);
    c.set_water(water.map(std::sync::Arc::new));
    c
}

// ───────────────────────── scene 1: the lidded pocket ─────────────────────────

/// The water surface of the lidded pocket. Chosen so the lid sits a hair ABOVE it (below), which
/// is the whole geometry of the #649 strand: the mounted position is DRY by a fraction of a unit.
pub const POCKET_SURFACE: f32 = -55.978;
/// The lid: a single zero-thickness ceiling quad, 0.009 u above the pocket's waterline.
///
/// **This value and [`POCKET_SURFACE`] were COPIED from `tests/water_capability.rs`'s baked-qcat
/// numbers.** They are authored inputs, not measurements, so the fact that a run here ends at
/// −55.969 and a run against baked qcat ends at −55.9687 is arithmetic, not corroboration — do
/// not read it as two independent measurements agreeing. What carries evidential weight is the
/// MECHANISM holding across a band of lid heights rather than at one tuned value: the mount
/// happens iff the lid is within the push-out's `STEP_UP + GROUND_ORIGIN` = 3.0 u upward reach of
/// the swim plane, and the result is dry iff the lid is above the surface. The 0.009 u gap is
/// cosmetic; the strand reproduces anywhere in the ~1 u dry band above the waterline.
pub const POCKET_LID_Z: f32 = -55.969;
/// The pocket floor.
pub const POCKET_FLOOR_Z: f32 = -70.0;
/// The pocket's swim plane — where buoyancy holds a swimmer (`surface − float_depth`, 2.0).
pub const POCKET_SWIM_PLANE: f32 = POCKET_SURFACE - 2.0;

/// **SCENE 1 — A DEEP FLOODED POCKET WITH A LID OVER IT (#649 / #329).**
///
/// A sealed rectangular chamber, floor at [`POCKET_FLOOR_Z`], flooded to [`POCKET_SURFACE`], with
/// a ceiling quad (the "lid") at [`POCKET_LID_Z`] — **0.009 u above the waterline**.
/// A swimmer floating at the pocket's own swim plane is 2.009 u below that lid: inside the
/// `STEP_UP + GROUND_ORIGIN = 3.0` reach of the depenetration push-out's `nearest_floor` probe.
///
/// # Why there is no bypass
///
/// The chamber is **closed on all six sides** — four walls, a floor, and the lid — so there is
/// nowhere to route to. The only thing the test asserts is the character's own final STATE, and
/// the lid is the only geometry anywhere within the push-out's upward reach of the swim plane.
/// A character that ends up dry, grounded, at `POCKET_LID_Z` can only have got there by being
/// placed on the lid: nothing else in the scene is at that height, buoyancy cannot lift a swimmer
/// past its surface, and there is no ledge to step onto.
///
/// The paired control [`sealed_pocket_without_lid`] holds every other element fixed and removes
/// only the lid; a mechanism that produced the strand without the lid would show up there.
pub fn sealed_pocket_with_lid() -> Collision {
    let (e0, e1, n0, n1) = (-40.0, 0.0, -20.0, 20.0);
    let mut m = vec![floor(POCKET_FLOOR_Z, e0, e1, n0, n1), floor(POCKET_LID_Z, e0, e1, n0, n1)];
    // Walls run well above the lid so there is standing room on top of it — which is precisely the
    // trap: the character CAN stand there, and cannot get back down.
    m.extend(room_walls(e0, e1, n0, n1, POCKET_FLOOR_Z, -30.0));
    collision(m, Some(RegionMap::water_boxes(&[[n0, n1, e0, e1, POCKET_FLOOR_Z + 0.5, POCKET_SURFACE]])))
}

/// **THE CONTROL FOR SCENE 1** — byte-identical to [`sealed_pocket_with_lid`] except the lid quad
/// is not placed. Everything else (walls, floor, water volume, surface height) is the same. If the
/// strand were produced by the walls, the depth, or the water map rather than by the lid, it would
/// reproduce here too.
pub fn sealed_pocket_without_lid() -> Collision {
    let (e0, e1, n0, n1) = (-40.0, 0.0, -20.0, 20.0);
    let mut m = vec![floor(POCKET_FLOOR_Z, e0, e1, n0, n1)];
    m.extend(room_walls(e0, e1, n0, n1, POCKET_FLOOR_Z, -30.0));
    collision(m, Some(RegionMap::water_boxes(&[[n0, n1, e0, e1, POCKET_FLOOR_Z + 0.5, POCKET_SURFACE]])))
}

// ─────────────── scene 2: two water columns, different surfaces ───────────────

/// Source column surface (the LOW one) — everything west of [`STEP_EAST`].
pub const LOW_SURFACE: f32 = -56.0;
/// Destination column surface (the HIGH one) — everything east of [`STEP_EAST`].
pub const HIGH_SURFACE: f32 = -40.0;
/// The east coordinate where the two water volumes meet.
pub const STEP_EAST: f32 = 0.0;
/// Shared basin floor for both columns.
pub const BASIN_FLOOR_Z: f32 = -80.0;

/// **SCENE 2 — A WATER SURFACE ADJACENT TO A COLUMN WITH A HIGHER SURFACE (#648's false premise).**
///
/// One open basin, one flat floor at [`BASIN_FLOOR_Z`], **no interior geometry whatsoever** — and a
/// water map with two volumes meeting at `east = `[`STEP_EAST`]: surface [`LOW_SURFACE`] to the
/// west, [`HIGH_SURFACE`] (16 u higher) to the east. Real `.wtr` volumes are independent of the
/// mesh and do exactly this wherever a lidded pocket abuts an open shaft; the architectural
/// plausibility is not the point, the physics is.
///
/// # Why there is no bypass
///
/// The rise under test is **16 u**, eight times the swimmer's whole haul-out reach
/// (`haul_out_up` = 2.0) and six times its swimming step-up (2.5). There is **no geometry between
/// the floor and the walls' tops** for the character to climb, step onto, or hop over — the scene
/// is a bare box — so the ONLY mechanism in the controller that can raise `z` by 16 u here is
/// buoyancy re-reading the water surface at the character's own position each frame. A gate keyed
/// on the SOURCE column's surface (the withdrawn #648 rise gate) cannot let this happen; that is
/// what makes the test discriminating.
pub fn stepped_water_surfaces() -> Collision {
    let (e0, e1, n0, n1) = (-60.0, 60.0, -30.0, 30.0);
    let mut m = vec![floor(BASIN_FLOOR_Z, e0, e1, n0, n1)];
    m.extend(room_walls(e0, e1, n0, n1, BASIN_FLOOR_Z, -10.0));
    collision(m, Some(RegionMap::water_boxes(&[
        [n0, n1, e0, STEP_EAST, BASIN_FLOOR_Z + 0.5, LOW_SURFACE],
        [n0, n1, STEP_EAST, e1, BASIN_FLOOR_Z + 0.5, HIGH_SURFACE],
    ])))
}

// ─────── scene 3: open water with no floor within the push-out's reach (#664) ───────

/// Depth of the scene's only floor — irrelevant to the case, it just needs to exist so
/// [`Collision::has_geometry`](eqoxide_zone_geometry::collision::Collision::has_geometry) is true (an empty
/// scene short-circuits the depenetration net entirely, which would test nothing).
pub const FAR_FLOOR_Z: f32 = -50.0;
/// East coordinate where the scene's only floor begins — **far** outside the depenetration
/// push-out's widest search ring (`movement::PUSHOUT_RADII`'s last entry is 32 u) and outside
/// `PUSHOUT_RADII` doubled several times over, so no ring candidate anywhere near the origin can
/// ever reach it, at any radius the net tries.
pub const FAR_FLOOR_EAST: f32 = 500.0;

/// **SCENE 3 — OPEN WATER, NO FLOOR ANYWHERE NEAR (#649 review finding 1 / #664).**
///
/// Water everywhere (an unbounded [`RegionMap::water_slab`]); the scene's only floor is a strip
/// starting [`FAR_FLOOR_EAST`] units east — far outside anywhere the depenetration push-out's ring
/// search or its `GROUND_DEPTH` vertical probe can reach from the origin. At the origin (and at
/// every ring candidate the push-out tries near it), `footprint_clear` is trivially true — there is
/// no geometry anywhere nearby to pierce it — yet `ground_below` finds nothing at all: the OTHER
/// clause of the net's `is_embedded` predicate (`floor.is_none()`) is what calls this body
/// "embedded" despite its clear footprint. That is the exact combination #664 is about: a body
/// whose only problem is the medium's own geometry, not an overlap the ring push-out could resolve
/// by moving it sideways.
///
/// # Why there is no bypass
///
/// There is no wall, no floor, no ceiling, and no ledge within any ring radius the push-out
/// searches (max 32 u) or any distance a body could walk with wish input in the couple of seconds
/// the tests here drive it (the tests drive ZERO wish input). The only thing in this scene able to
/// move the body from the origin at all is the depenetration net handing back a recovery. A
/// character that ends up displaced can only have gotten there through the net.
pub fn open_water_no_floor_in_reach() -> Collision {
    let m = vec![floor(FAR_FLOOR_Z, FAR_FLOOR_EAST, FAR_FLOOR_EAST + 100.0, -1000.0, 1000.0)];
    collision(m, Some(RegionMap::water_slab(-1000.0, 10.0)))
}

/// **THE CONTROL FOR SCENE 3** — the SAME floor, moved directly under the origin instead of 500 u
/// away, everything else identical (same water slab, same body start point). With a floor in
/// vertical reach the net's `is_embedded` predicate is false at the very first check (nothing to
/// recover from at all) — `step` never even enters the ring search. This is the "nothing happens
/// because there was nothing to fix" baseline scene 3's own case is contrasted against; it is not
/// used to assert a shared numeric outcome, since the two scenes are supposed to diverge (one has a
/// floor below the origin, one does not) — it exists so a future edit that accidentally left a floor
/// under the origin in [`open_water_no_floor_in_reach`] would be easy to tell apart from this one by
/// diffing the two functions.
pub fn open_water_with_floor_in_reach() -> Collision {
    let m = vec![floor(FAR_FLOOR_Z, -50.0, 50.0, -1000.0, 1000.0)];
    collision(m, Some(RegionMap::water_slab(-1000.0, 10.0)))
}
