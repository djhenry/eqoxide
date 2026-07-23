//! **Hand-authored mini-zone fixtures** — geometry built in code, containing no game-derived
//! content, that reproduce the SITUATIONS the asset-gated nav tests exist to protect (#659).
//!
//! # Why this exists
//!
//! The asset-gated tests (`tests/water_capability.rs`, `tests/walker_sim.rs`, and the rest of the
//! 32 `#[ignore]`d family) need baked zone assets that live only on a LAN asset server. They have
//! therefore **never run in CI** (#654), and the only proposal to run them — a self-hosted runner
//! on a PUBLIC repo (#657) — is a standing security hazard. Synthetic geometry has no provenance
//! problem and runs on any hosted runner, with no assets, no network, and no credential.
//!
//! # What is kept and what is lost — read this before trusting a scene
//!
//! These scenes pin the **physical claim** ("a swimmer is lifted to the DESTINATION column's swim
//! plane", "the water-blind push-out mounts a swimmer onto a lid and strands it dry"). They do
//! **not** pin "and this specific shipped zone actually exhibits that" — the asset-gated tests
//! keep doing that job and are deliberately left in place, still `#[ignore]`d. A synthetic scene
//! that agrees with the real one is evidence the mechanism is understood; it is not evidence about
//! the shipped geometry.
//!
//! # The trap these scenes are built against
//!
//! A fixture that lets the character route AROUND the thing under test passes green while proving
//! nothing (two PRs shipped exactly that). So each scene here is built so the situation under test
//! is the **only** thing that can produce the asserted outcome — see each builder's doc for the
//! specific argument, and the control scenes (`sealed_pocket_without_lid`, …) that hold everything
//! else fixed and remove only the feature under test.
//!
//! # What this layer does NOT cover — the swimming step-up
//!
//! **There is zero synthetic coverage of the swimming step-up** (`movement.rs`'s
//! `if (self.on_ground || swimming) && low_hit …` branch, the one that exists so a swimmer can
//! haul OUT onto a bank, #191). Disabling it leaves every test in this layer green — that is a
//! deliberately-recorded observation about the LID STRAND's mechanism (it is the depenetration
//! push-out, not the step-up), and it is NOT a statement that the step-up is untested: the tree's
//! coverage of it lives in `tests/walker_sim.rs`
//! (`p1_haul_out_admission_matches_controller_execution`), which does go RED when it is disabled.
//! A future reader must not infer from "these tests stayed green" that the step-up is unprotected.
//!
//! # Coordinates
//!
//! Scene builders take and return **server coords** `[east, north, up]`, the same frame the
//! controller and `Collision` use. The `MeshData` positions underneath are GLB/libeq space
//! `[north, up, east]` (that is the swap `Collision::build` undoes); the helpers here do the
//! conversion so no scene has to think about it.

#![allow(dead_code)] // each test binary uses a subset of the scenes

use eqoxide::assets::{MeshData, RenderMode, ZoneAssets};
use eqoxide::nav::collision::Collision;
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

// ───────────── scene 3: a near-vertical face behind a flat run (#630) ─────────────

/// Height of the near-vertical face — the #617 canal-bank rise, to the unit.
pub const FACE_RISE: f32 = 12.8;
/// The low ground the flat run sits on.
pub const LOW_Z: f32 = 0.0;
/// The plateau on top of the face.
pub const HIGH_Z: f32 = LOW_Z + FACE_RISE;
/// East coordinate of the face.
///
/// **This number is load-bearing and is not arbitrary.** The coarse plan grid's origin is the
/// scene's min corner, so with the scene spanning east `[-64, 64]` the 8 u cell centres fall on
/// `…, −4, +4, …`. #630's laundering only reaches the planner when the near-vertical face sits
/// close to the DESTINATION end of the hop — that is the whole reason the feet ray misses it:
/// the ray interpolates `az + feet_clr → bz + feet_clr`, so it has already gained most of the
/// altitude by the far end and skims over a face near the destination. Concretely, the ray clears
/// the face only when the face's fraction along the hop exceeds
/// `(face_top − az − feet_clr) / rise = (12.8 − 2.5) / 12.8 = 0.805`; at 2.6 that fraction is
/// `(2.6 − (−4)) / 8 = 0.825`. Move the face west and the feet ray blocks the edge instead — the
/// scene then still passes `astar_finds_no_route_up_a_near_vertical_face`, but for the WRONG
/// reason, and stops discriminating #630 at all. (Measured, not assumed: with the #630 check
/// disabled the earlier `FACE_EAST = 0.0` version still found no route.)
pub const FACE_EAST: f32 = 2.6;

/// **SCENE 3 — A NEAR-VERTICAL FACE PRECEDED BY A FLAT RUN (#630, "the laundered grade").**
///
/// Low ground at [`LOW_Z`] for `east ∈ [-64, 0]`; a single vertical face at `east = `[`FACE_EAST`]
/// rising [`FACE_RISE`] u; a plateau at [`HIGH_Z`] for `east ∈ [0, 64]`. Both are wide in north so
/// the coarse 8 u A* grid has real diagonal hops available across the face.
///
/// This is the shape that defeated the pre-#630 walk gate: the endpoint rise divided by the
/// **diagonal** hop length (`12.8 / 11.31 = 1.13`) slips under `MAX_WALK_GRADE = 1.2`, while the
/// same face taken orthogonally (`12.8 / 8 = 1.6`) is correctly refused. The controller's real
/// capability is a 2.0 u step plus a walkable slope — nowhere near 12.8 u of vertical rock.
///
/// # Why there is no bypass
///
/// The face spans the **entire** north extent of the scene and the scene is walled at both ends,
/// so there is no ramp, no stair, no gap, and no way around: every route from the low ground to
/// the plateau must cross this one face. A planner that returns a route to the plateau has
/// accepted the face — there is no other edge it could have used.
pub fn flat_run_into_a_vertical_face() -> Collision {
    let (n0, n1) = (-48.0, 48.0);
    let m = vec![
        floor(LOW_Z, -64.0, FACE_EAST, n0, n1),
        floor(HIGH_Z, FACE_EAST, 64.0, n0, n1),
        wall_ew(FACE_EAST, n0, n1, LOW_Z, HIGH_Z), // the face itself
        // Perimeter: no way round the ends of the face.
        wall_ew(-64.0, n0, n1, LOW_Z, HIGH_Z + 20.0),
        wall_ew(64.0, n0, n1, HIGH_Z, HIGH_Z + 20.0),
        wall_ns(n0, -64.0, 64.0, LOW_Z, HIGH_Z + 20.0),
        wall_ns(n1, -64.0, 64.0, LOW_Z, HIGH_Z + 20.0),
    ];
    collision(m, None)
}

// ─────── scene 4: open water with no floor within the push-out's reach (#664) ───────

/// Depth of the scene's only floor — irrelevant to the case, it just needs to exist so
/// [`Collision::has_geometry`](eqoxide::nav::collision::Collision::has_geometry) is true (an empty
/// scene short-circuits the depenetration net entirely, which would test nothing).
pub const FAR_FLOOR_Z: f32 = -50.0;
/// East coordinate where the scene's only floor begins — **far** outside the depenetration
/// push-out's widest search ring (`movement::PUSHOUT_RADII`'s last entry is 32 u) and outside
/// `PUSHOUT_RADII` doubled several times over, so no ring candidate anywhere near the origin can
/// ever reach it, at any radius the net tries.
pub const FAR_FLOOR_EAST: f32 = 500.0;

/// **SCENE 4 — OPEN WATER, NO FLOOR ANYWHERE NEAR (#649 review finding 1 / #664).**
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

/// **THE CONTROL FOR SCENE 4** — the SAME floor, moved directly under the origin instead of 500 u
/// away, everything else identical (same water slab, same body start point). With a floor in
/// vertical reach the net's `is_embedded` predicate is false at the very first check (nothing to
/// recover from at all) — `step` never even enters the ring search. This is the "nothing happens
/// because there was nothing to fix" baseline scene 4's own case is contrasted against; it is not
/// used to assert a shared numeric outcome, since the two scenes are supposed to diverge (one has a
/// floor below the origin, one does not) — it exists so a future edit that accidentally left a floor
/// under the origin in [`open_water_no_floor_in_reach`] would be easy to tell apart from this one by
/// diffing the two functions.
pub fn open_water_with_floor_in_reach() -> Collision {
    let m = vec![floor(FAR_FLOOR_Z, -50.0, 50.0, -1000.0, 1000.0)];
    collision(m, Some(RegionMap::water_slab(-1000.0, 10.0)))
}

/// **THE CONTROL FOR SCENE 3** — the same two levels joined by a genuine walkable RAMP instead of
/// a vertical face. `FACE_RISE` over a 16 u run is grade 0.8, comfortably inside
/// `MAX_WALK_GRADE = 1.2`, and the profile check has nothing to reject. This is the
/// over-tightening guard: a fix for #630 that also refuses this has broken real terrain.
pub fn flat_run_into_a_walkable_ramp() -> Collision {
    let (n0, n1) = (-48.0, 48.0);
    // Ramp from (east -16, z LOW) up to (east 0, z HIGH): rise 12.8 over run 16 → grade 0.8.
    let ramp = quad([[n0, LOW_Z, -16.0], [n1, LOW_Z, -16.0], [n1, HIGH_Z, FACE_EAST], [n0, HIGH_Z, FACE_EAST]]);
    let m = vec![
        floor(LOW_Z, -64.0, -16.0, n0, n1),
        ramp,
        floor(HIGH_Z, FACE_EAST, 64.0, n0, n1),
        wall_ew(-64.0, n0, n1, LOW_Z, HIGH_Z + 20.0),
        wall_ew(64.0, n0, n1, HIGH_Z, HIGH_Z + 20.0),
        wall_ns(n0, -64.0, 64.0, LOW_Z, HIGH_Z + 20.0),
        wall_ns(n1, -64.0, 64.0, LOW_Z, HIGH_Z + 20.0),
    ];
    collision(m, None)
}

// ─────── scene 5: a flat run, then a final goal ATOP a face (#639) ───────

/// The raised goal patch (the "pillar top"). Deliberately SMALL and OFFSET so it covers **no coarse
/// 8u cell centre**: the coarse grid's origin is the scene's min corner (east −64, north −48), so
/// centres fall on east `…, −4, +4, +12, …` and north `…, −8, 0, +8, …`. A patch over
/// east `[6, 10]` × north `[−4, 4]` sits strictly between the +4 and +12 columns and the 0 row's
/// neighbours, touching NO centre. A\* can therefore only ever stand at the goal cell on the LOW
/// ground; the goal ATOP the patch is reached solely by the goal-append snap — the gap #639 closes.
pub const GOAL_PATCH_E0: f32 = 6.0;
pub const GOAL_PATCH_E1: f32 = 10.0;
pub const GOAL_PATCH_N0: f32 = -4.0;
pub const GOAL_PATCH_N1: f32 = 4.0;
/// The exact goal: centred on the raised patch, atop the face, at [`HIGH_Z`].
pub const GOAL_ATOP_FACE: [f32; 3] = [8.0, 0.0, HIGH_Z];
/// The paired control goal: the SAME XY, but on the low ground UNDER the patch (`LOW_Z`).
pub const GOAL_BESIDE_FACE: [f32; 3] = [8.0, 0.0, LOW_Z];

/// **SCENE 5 — A FLAT RUN, THEN A FINAL GOAL ATOP A NEAR-VERTICAL FACE (#639).**
///
/// Low ground at [`LOW_Z`] across the whole scene, plus a small raised floor patch at [`HIGH_Z`]
/// (12.8 u up) at the goal — a "pillar top" floating over the low ground with a 12.8 u face on every
/// side. The patch is small and offset so no coarse 8 u cell CENTRE lands on it (see the
/// `GOAL_PATCH_*` note); every cell A\* can stand in near the goal is on the low ground.
///
/// This isolates the goal-**append** gap, distinct from scene 3's intermediate-edge case. A\* walks
/// the flat low ground and REACHES the goal cell — its centre is low ground, 12.8 u below the goal's
/// resolved floor, so it is taken as a wrong-tier fallback (`reached_goal = true`). The route is then
/// completed by snapping the final waypoint to the exact goal atop the patch — a penultimate → goal
/// hop of grade ≈ `12.8 / 8` that NO intermediate walk-edge check ever saw. Every hop UP TO the goal
/// cell is a legal flat walk; ONLY the appended final hop is un-walkable.
///
/// # Why there is no bypass
///
/// The patch is a floating slab reachable from nowhere: a 12.8 u face on all sides, no ramp, no
/// stair. The low ground is otherwise open, so the goal CELL is always reachable — the scene cannot
/// pass its assertion by being globally unroutable (that would be scene 3, not #639). The paired
/// control is the same goal XY on the low ground ([`GOAL_BESIDE_FACE`]), which must route: same
/// approach, walkable final tier, so ONLY the tier the goal sits on distinguishes the two.
pub fn flat_run_into_a_final_goal_face() -> Collision {
    let (n0, n1) = (-48.0, 48.0);
    let m = vec![
        floor(LOW_Z, -64.0, 64.0, n0, n1),                                          // open low ground
        floor(HIGH_Z, GOAL_PATCH_E0, GOAL_PATCH_E1, GOAL_PATCH_N0, GOAL_PATCH_N1),   // the raised goal patch
        wall_ew(-64.0, n0, n1, LOW_Z, HIGH_Z + 20.0),
        wall_ew(64.0, n0, n1, LOW_Z, HIGH_Z + 20.0),
        wall_ns(n0, -64.0, 64.0, LOW_Z, HIGH_Z + 20.0),
        wall_ns(n1, -64.0, 64.0, LOW_Z, HIGH_Z + 20.0),
    ];
    collision(m, None)
}
