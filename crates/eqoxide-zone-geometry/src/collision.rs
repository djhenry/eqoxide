//! [`Collision`] is the zone's flattened-triangle spatial grid: `build` consumes a loaded
//! [`eqoxide_assets::ZoneAssets`] (mesh/texture loading stays in `eqoxide-assets`; this module
//! only reads its output), and its methods answer every geometric query the render and movement
//! code needs, such as `floor_z`/`nearest_floor` (grounding), `nearest_hit_t`/`segment_blocked`
//! (camera and nameplate occlusion), `path_clear`/`footprint_clear` (movement gating), water, climb
//! and zone-line lookups, and placement.
//!
//! This is a ONE-WAY dependency: this module depends on `eqoxide_assets` for its mesh input types,
//! but nothing in `eqoxide-assets` depends back on this module.

use eqoxide_assets::{expand_objects, MeshData, ZoneAssets, COLLISION_MESH_TAG};

/// Precomputed collision geometry for fast spatial queries against a zone.
///
/// All zone triangles are flattened once into absolute GPU world space
/// `[east, north, height]` and bucketed into a uniform XY grid. Queries (floor
/// raycast for grounding, segment raycast for camera collision and nameplate
/// occlusion) visit only the grid cells their XY footprint overlaps instead of
/// scanning every triangle each frame.
/// Shared handle to the current zone's collision grid. The render thread builds it on
/// zone load and publishes it here; the net thread reads it to gate movement. Inner
/// `Arc<Collision>` so both threads share one grid without cloning the triangle data.
pub type SharedCollision = std::sync::Arc<std::sync::RwLock<Option<std::sync::Arc<Collision>>>>;

/// A swept-collision hit: fraction `t ∈ [0,1]` along the query delta where contact occurs,
/// plus the unit surface normal of the hit triangle (flipped to oppose the motion so callers
/// can project/slide against it). Returned by [`Collision::sweep`].
#[derive(Debug, Clone, Copy)]
pub struct Hit {
    pub t:      f32,
    pub normal: [f32; 3],
}

pub struct Collision {
    tris:      Vec<[[f32; 3]; 3]>,
    /// Per-triangle face-normal Z (normalized), parallel to `tris`. Sign = facing: `> 0` is an
    /// UP-facing surface (a floor you can stand on), `< 0` a DOWN-facing one (a ceiling / the
    /// underside of a bridge). Computed once at build, and the ray tests filter on it. (~4
    /// bytes/tri.)
    tri_nz:    Vec<f32>,
    cells:     Vec<Vec<u32>>,
    // grid geometry, public so callers outside this crate can sample floor points across the grid
    pub origin:    [f32; 2], // (east, north) of cell (0,0) corner
    pub cell_size: f32,
    pub cols:      usize,
    pub rows:      usize,
    /// Z extent of the whole mesh. The floor-normal filter's safety valve (`column_hits`) has to ask
    /// "is there anything BENEATH this surface?" of the FULL COLUMN, not of the caller's query
    /// window — a window is only ~100u tall and a cavern roof's floor is often further down than
    /// that. Bounds let a column probe span the zone regardless of what the caller asked for.
    /// `z_min` is only consumed by `#[cfg(test)]` zone-corpus probes (to size a full-column scan);
    /// production code only ever needs the upper bound, so it's test-only to avoid a dead-code warning.
    #[cfg(any(test, feature = "test-fixtures"))]
    z_min:     f32,
    // `pub` for the same relocated corpus integration test (it scans from the mesh's top z downward).
    pub z_max:     f32,
    /// The zone's region map (from its `.wtr`) — **or the reason there isn't one** (#803).
    ///
    /// `Ok` lets a consumer route through water to a lower floor with no walkable connection, and
    /// [`Collision::zone_line_indices`] enumerates the zone's real exits.
    ///
    /// `Err` means this grid has no region data, and carries the reason. A `Result` keeps a failed
    /// read distinct from a zone that genuinely has no zone lines. Written only by
    /// [`Collision::set_region_data`] and read through [`Collision::region_map`].
    water: Result<std::sync::Arc<eqoxide_core::region_map::RegionMap>,
                  eqoxide_core::region_map::RegionDataAbsent>,
    /// True when the terrain triangles came from a dedicated `__collision__` mesh (SOLID +
    /// INVIS faces, PASSABLE excluded). False for legacy zones with no baked collision mesh,
    /// where the rendered terrain is used as a fallback. Diagnostic/provenance only.
    pub from_collision_mesh: bool,
    /// Precomputed `(zone_line_index, [east, north, z])` for each zone-line region, built once from
    /// the water map at `set_water` (zone load). Lets `find_zone_line_near` be an O(1) cache read
    /// instead of an exhaustive scan.
    zone_line_regions: Vec<(i32, [f32; 3])>,
    /// Every climbable surface in this zone, derived from the placed objects at [`Collision::build`]
    /// (#309). The movement controller asks whether the character is on something it could climb.
    climb_volumes: Vec<crate::climb::ClimbVolume>,
    /// The sparse water-span grid. `None` until built by [`Collision::build_water_grid`] and stored
    /// via [`Collision::set_water_grid`]. Not built in `set_water`, so zone load does not pay the
    /// build cost.
    water_grid: Option<crate::water_grid::WaterGrid>,
    /// Lazily built water-span grid. [`Self::water_grid_active`] builds the grid on the first call
    /// in a water zone instead of at zone load. The `OnceLock` builds the grid once per `Collision`,
    /// and every holder of the `Arc<Collision>` reads the one result. Separate from `water_grid` so
    /// an explicit [`Self::set_water_grid`] always wins over the lazily built grid.
    water_grid_lazy: std::sync::OnceLock<crate::water_grid::WaterGrid>,
}

/// Outcome of building one water-span column. Separates a column with no water (`Dry`) from a
/// column of water with no queryable bottom (`UnboundedBelow`). The builder counts the second case
/// and never invents a floor for it.
enum WaterColumnResult {
    Column(crate::water_grid::WaterColumn),
    UnboundedBelow,
    Dry,
}

/// The two thresholds of the standable-floor test in `column_hits`. A surface is standable ground,
/// regardless of facing, iff `|nz| >= NAV_NEAR_HORIZONTAL` (flat enough to stand on) AND it has
/// `NAV_AGENT_HEIGHT` of open space above it before the next SOLID surface (else it is under a
/// ceiling, not standing room). The winding sign of a surface plays no part, because inverted-art
/// floor is real floor.
///
/// `NAV_NEAR_HORIZONTAL` is the flatness limit: a unit normal's `|z|` for a surface at grade `g` is
/// `1/sqrt(1+g²)`, and a walkable grade of 1.2 (about 50°) gives `1/sqrt(1+1.44) ≈ 0.64`.
///
/// `NAV_AGENT_HEIGHT` is the clearance a standing character needs. It must EXCEED a real ceiling's
/// slab-gap (a room ceiling has its roof right above → tiny headroom → rejected) yet stay BELOW a real
/// room's height (or a low room's floor would be wrongly rejected → seal). The controller's own chest
/// collision ray sits at `foot + 4.0` (`movement.rs`), so ~5u is the clearance a body actually needs.
///
/// These two consts alias `PLAYER_BODY`. The value lives on the shared `Body`
/// (`Body::near_horizontal` and `Body::agent_height`), so change the number on the `Body`.
pub const NAV_NEAR_HORIZONTAL: f32 = crate::body::PLAYER_BODY.near_horizontal;
pub const NAV_AGENT_HEIGHT: f32 = crate::body::PLAYER_BODY.agent_height;

/// Contact tolerance, in ULPs of the largest coordinate involved in a ray/triangle test. See
/// [`contact_tol`].
///
/// The cliff of this constant was measured on the baked-asset corpus of driven swim descents from
/// real submerged floors (`movement::tests::a_driven_swim_descent_never_passes_a_real_zone_floor`).
/// The sweep covered 11 zones, 328 submerged columns and 3936 runs. The test prints its own corpus
/// size, so re-take the sweep on a different asset cache instead of reusing these counts. The
/// through-count by value, for every point run, is:
///
/// | `CONTACT_TOL_ULPS` | 0 | 1 | 2 | 3 | 4 | 5 | 6 | **7** | 8 | 16 | **32** | 64 … 4096 |
/// |---|---|---|---|---|---|---|---|---|---|---|---|---|
/// | ran through the floor | 146 | 22 | 10 | 4 | 4 | 4 | 2 | **0** | 0 | 0 | **0** | 0 |
///
/// The cliff is at `7`. A sample smaller than this corpus puts the cliff lower, so the cliff
/// position depends on the sample size.
///
/// The value `32` sits 4.6× above the measured cliff, and the corpus measures green continuously
/// from `7` to `4096` (585× the cliff). The high side is bounded separately. A measured run at
/// `1e7` ULPs turned 20 tests RED, so a value that is too sticky fails loudly.
///
/// This constant bounds the blind band and does not close it. A dense sample of 94 baked zones
/// near the world origin (599,875 columns within ±24 u of `(0,0)`) measured 108 residual
/// self-consistency misses at `32`, against 582,089 on `main` (a 5390× reduction). The budget of
/// `contact_tol` is set by the *result* point and the triangle's vertices. The reconstruction
/// error that the budget has to cover is set by the floor query's own start height and range
/// (`ground_below`'s `start`/`range` arguments), and `contact_tol` never sees those arguments.
/// The same tilted quad that the cliff table used, queried with a start and range far from the
/// fixture's own `60.0` and `400.0`, misses far more at `32` (up to 1045/1369 at `600` and
/// `4000`). The call-site parameters of `ground_below` therefore bound the invariant that this
/// constant guarantees. Tracked as #875.
///
/// **Why the high side has room to spare.** The two failure directions are not symmetric. A value
/// that is too small lets a body walk through the world with no signal. That is the
/// silent-wrong-answer class that this repo ranks above crashes. A value that is too large makes a
/// face slightly behind the ray report as touched, and every caller here resolves that
/// conservatively (`slide` advances 0, `swim_sink` clamps to 0, depenetration recovers). At the
/// largest ray-origin coordinate in the corpus (`everfrost`'s `−4467`), `32` ULPs is `1.7e-2` world
/// units. That is about a third of `SKIN` (0.05) and 1.7% of `PLAYER_RADIUS` (1.0), so no caller's
/// own back-off is displaced. The scale of a hit also spans the triangle, so the scale can exceed
/// the ray's own coordinate and the bound stays the same order.
pub const CONTACT_TOL_ULPS: f32 = 32.0;

/// Largest absolute coordinate in a point — the magnitude that sets `f32` resolution near it.
#[inline]
pub fn axis_scale(p: [f32; 3]) -> f32 { p[0].abs().max(p[1].abs()).max(p[2].abs()) }

/// The world-unit slack a ray allows *behind* its own origin before a face stops counting as
/// touched, `CONTACT_TOL_ULPS` ULPs of `scale`. The `scale` is the largest coordinate involved in
/// the test (ray origin AND triangle vertices, see the call sites).
///
/// Any slack is needed because a floor query returns a *reconstructed* coordinate, not a
/// coordinate from the mesh. The reconstruction is `gather_top + t·dir_z` (`column_hits`). That
/// `f32` does not generally lie on the triangle's plane and lands an ULP or two either side. When
/// the coordinate lands **below** the plane, a descent ray starting there begins inside the solid
/// and Möller–Trumbore correctly reports the face at a small NEGATIVE `t`. A lower bound of
/// exactly `0` throws that hit away. Production does exactly this, since `movement.rs` assigns
/// `self.pos[2]` straight from a floor query at the ground-snap and depenetration arms.
///
/// The cause is not `f32` cancellation in `tvec = from − v0` scaling with the world coordinate.
/// An axis-aligned synthetic quad at `tox`'s `(2081, 2320, −87)` measures a blind band of `0` even
/// with this slack removed, and the reconstruction error fires at the origin too.
///
/// Measured on a tilted quad over 1369 columns, the table gives the floor-z samples that then
/// report no floor below, and the worst offset needed to see the floor again:
///
/// | fixture | lower bound `t ≥ 0` | with slack |
/// |---|---|---|
/// | quad at the origin | 958/1369, band 1.419e-5 | 0/1369 |
/// | quad at `tox` coords | 882/1369, band 1.907e-5 | 0/1369 |
/// | quad at `everfrost` coords | 623/1369, band 2.289e-5 | 0/1369 |
///
/// The band grows only mildly with the coordinate and the *count* is large everywhere. The problem
/// is therefore a reconstruction resolution that needs a world-unit tolerance, and the tolerance
/// scales with the largest coordinate in the whole test. A ray at `(0,0,0)` querying a triangle
/// whose vertices are 50 u away needs 50 u worth of resolution, not 1. Scaling on the ray origin
/// alone leaves 21/1369 of the origin fixture still missing.
///
/// `max(1.0)` floors the scale so geometry hugging the origin still gets a nonzero bound.
#[inline]
pub fn contact_tol(scale: f32) -> f32 {
    CONTACT_TOL_ULPS * f32::EPSILON * scale.max(1.0)
}

/// **THE ONE RAY-HIT ACCEPTANCE TEST (#855).** A Möller–Trumbore `t` on a ray of world length `len`
/// starting at `from` counts as a hit iff this function accepts it. All three ray scans in this
/// module call it: [`Collision::nearest_hit_t`], [`Collision::nearest_hit`] and
/// [`Collision::column_hits`].
///
/// The lower bound is a world distance and not a fraction of the ray length. A fraction such as
/// `t > 1e-3` is a blind band of `1e-3 × ray length` in each caller's own units, measured exactly
/// (ratio `1.000e-3` at ray lengths 0.56 / 2.5 / 100). Over a floor at z=0, a ray from the face
/// down 2.5 would return `None` from `nearest_hit_t` while `column_surfaces` reports the face
/// twice. `CharacterController::swim_sink` casts a `35·dt` ray, so a swimmer whose feet sat inside
/// such a band would read "open water below" and descend THROUGH the pool floor.
///
/// **Why the lower bound is in world units and the upper bound is not.** The two ends are not the
/// same problem. The lower end is a *contact* test, where `from` and the face coincide and `f32`
/// cancellation is at its worst. The test expresses the lower end as a world distance behind the
/// origin ([`contact_tol`]), the only form that means the same thing at `(0,0,0)` and at
/// `(2081, 2320, −87)`. The upper end is a *range* test against the caller's own endpoint, where
/// no cancellation occurs and `t ≤ 1.0` is exact. **Stated residual.** A face lying within
/// `contact_tol` *beyond* `to` is still rejected. No measurement shows the rejection biting, and
/// the failure self-corrects for the callers here. A body that steps exactly onto such a face
/// arrives at distance ~0 from the face, where the lower bound catches the face on the next frame.
/// Pinned by `a_face_just_past_the_far_end_is_caught_on_the_next_frame`.
///
/// **Why no positive epsilon.** A positive epsilon would read like a self-intersection guard for a
/// ray starting ON a face, but the coplanar case is already rejected one test earlier. A ray lying
/// IN a triangle's plane is parallel to the triangle, so `det.abs() < eps` drops the ray. A ray that
/// starts on a face and heads INTO the face yields `t = 0`, a true zero-distance contact. Every
/// caller carries its own back-off (`SKIN`, `Body::radius`) that turns `t = 0` into "no progress"
/// and never into a negative move. Accepting `t = 0` makes contact detectable instead of silently
/// absent.
///
/// **The tie this cannot break**, measured and deliberate. A face at distance ~zero reports for a
/// ray heading EITHER way off the face. Möller–Trumbore returns `t = -0.0` for the away direction,
/// `-0.0 == 0.0` in IEEE, and the world-unit slack widens that tie from a point to `contact_tol`.
/// No lower bound of any kind separates the two sides at the point of contact, and a *larger*
/// bound than this would only make the tie wider. The tie resolves to "touching", the conservative
/// side for every caller here. Pinned by
/// `only_the_coplanar_case_is_rejected_by_the_parallel_test_the_rest_is_accepted_contact`.
#[inline]
pub fn hit_accepted(t: f32, len: f32, scale: f32) -> bool {
    t <= 1.0 && t * len >= -contact_tol(scale)
}

/// Shortest ray any scan in this module will answer, in world units. Below this length the
/// direction vector is numerically useless and every scan returns "nothing".
///
/// All three scans share this one value. Distinct guards would let the scans disagree on short
/// segments. Over `one_floor()` with a face squarely at the segment midpoint, guards of `|dir| <
/// 1e-3` in one scan and `|dir| < 3.16e-5` in another would give `nearest_hit` `Some(0.5)` and
/// `nearest_hit_t` `None` at segment lengths `1e-4`, `5e-4` and `9.9e-4`. Pinned by
/// `the_three_scans_agree_on_short_segments`.
pub const MIN_RAY_LEN: f32 = 1e-6;

// ── The CONTROLLER's placement geometry (#885) ───────────────────────────────────────────────────
// [`Collision::body_placement`] is the ONE definition of "can a body be placed here", read by the
// controller. These constants live beside it.

/// Ground-probe origin above the feet.
pub const GROUND_ORIGIN: f32 = 1.0;
/// Ground-probe downward range, measured from [`GROUND_ORIGIN`] and **not** from the feet
/// themselves. Prose describing "no floor within N u below the feet" cites
/// [`GROUND_REACH_BELOW_FEET`]. Citing this constant overstates the feet-relative reach by
/// [`GROUND_ORIGIN`].
pub const GROUND_DEPTH: f32 = 200.0;
/// How far below a body's actual feet the ground probe can still find a floor: [`GROUND_DEPTH`]
/// measured from [`GROUND_ORIGIN`] above the feet, so the feet-relative reach is
/// `GROUND_DEPTH - GROUND_ORIGIN`. This is the number every "no floor within N u below the feet"
/// doc/prose site should cite.
pub const GROUND_REACH_BELOW_FEET: f32 = GROUND_DEPTH - GROUND_ORIGIN;
/// Footprint-ring directions the placement test samples. This is the controller's historical
/// `PUSHOUT_DIRS / 2`; `movement` static-asserts that identity so the two cannot drift.
pub const PLACEMENT_RING_DIRS: usize = 8;

/// A vertical search column: `(east, north)` is the XY position, and the column spans
/// `[ref_z - down, ref_z + up]`. `floors_only` restricts the search to standable surfaces
/// (see `Collision::column_hits`).
#[derive(Clone, Copy)]
struct ColumnQuery {
    east: f32,
    north: f32,
    ref_z: f32,
    up: f32,
    down: f32,
    floors_only: bool,
}

impl Collision {
    /// Build the grid from zone geometry. `cell_size` is in EQ units.
    pub fn build(assets: &ZoneAssets, cell_size: f32) -> Self {
        // Flatten every triangle into world space [east, north, height].
        let mut tris: Vec<[[f32; 3]; 3]> = Vec::new();
        let expanded = expand_objects(&assets.objects);

        // Prefer the dedicated `__collision__` mesh when the zone GLB carries one: it holds
        // every SOLID face (visible AND invisible-but-solid: zone boundaries, invisible walls,
        // doorframes) and omits PASSABLE faces (water surfaces, foliage). Older zones baked
        // before this pipeline change have no such mesh — fall back to the rendered terrain so
        // they keep colliding as before. Placed-object collision always comes from
        // `expand_objects`, unchanged in both paths.
        let from_collision_mesh = assets
            .terrain
            .iter()
            .any(|m| m.texture_name.as_deref() == Some(COLLISION_MESH_TAG));
        let terrain_src: Vec<&MeshData> = if from_collision_mesh {
            assets
                .terrain
                .iter()
                .filter(|m| m.texture_name.as_deref() == Some(COLLISION_MESH_TAG))
                .collect()
        } else {
            assets.terrain.iter().collect()
        };

        for m in terrain_src.into_iter().chain(expanded.iter()) {
            let pos = &m.positions;
            let idx = &m.indices;
            let mut k = 0;
            while k + 2 < idx.len() {
                let (ia, ib, ic) = (idx[k] as usize, idx[k + 1] as usize, idx[k + 2] as usize);
                k += 3;
                if ia >= pos.len() || ib >= pos.len() || ic >= pos.len() { continue; }
                // EQ WLD -> world: render.X = server_x = p[2], render.Y = server_y = p[0], up = p[1]
                tris.push([
                    [pos[ia][2] + m.center[2], pos[ia][0] + m.center[0], pos[ia][1] + m.center[1]],
                    [pos[ib][2] + m.center[2], pos[ib][0] + m.center[0], pos[ib][1] + m.center[1]],
                    [pos[ic][2] + m.center[2], pos[ic][0] + m.center[0], pos[ic][1] + m.center[1]],
                ]);
            }
        }

        // XY bounds (for the broad-phase grid) and Z bounds (so a column probe can span the whole
        // mesh — see `z_min`/`z_max`).
        let mut min = [f32::MAX; 2];
        let mut max = [f32::MIN; 2];
        #[cfg(any(test, feature = "test-fixtures"))]
        let mut z_min = f32::MAX;
        let mut z_max = f32::MIN;
        for t in &tris {
            for v in t {
                if v[0] < min[0] { min[0] = v[0]; }
                if v[1] < min[1] { min[1] = v[1]; }
                if v[0] > max[0] { max[0] = v[0]; }
                if v[1] > max[1] { max[1] = v[1]; }
                #[cfg(any(test, feature = "test-fixtures"))]
                if v[2] < z_min { z_min = v[2]; }
                if v[2] > z_max { z_max = v[2]; }
            }
        }
        // Face-normal Z per triangle (see `tri_nz`). The WLD→world map (x,y,z) → (z,x,y) is a cyclic
        // permutation (determinant +1), so it PRESERVES winding — the sign here is the mesh's own.
        //
        // CAVEAT for the next person: placed-object triangles come through `expand_objects`, which
        // applies each instance's 4x4 matrix to the VERTICES. A MIRRORED instance (negative-
        // determinant matrix — e.g. a negative scale on one axis) reverses triangle winding, so its
        // faces would come out normal-INVERTED and this filter would read its floors as ceilings.
        // No shipped zone has one today (the build-time winding check below would catch a zone where
        // enough of them existed to matter, and all 34 cached zones pass), but if mirrored instances
        // ever appear, flip `nz` for triangles whose source instance matrix has det < 0 rather than
        // letting the whole zone fall back to facing-blind.
        let tri_nz: Vec<f32> = tris.iter().map(|t| {
            let e1 = [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]];
            let e2 = [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]];
            let n = [e1[1] * e2[2] - e1[2] * e2[1],
                     e1[2] * e2[0] - e1[0] * e2[2],
                     e1[0] * e2[1] - e1[1] * e2[0]];
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            if len > 1e-9 { n[2] / len } else { 0.0 }
        }).collect();

        let cell_size = cell_size.max(1.0);
        if tris.is_empty() || min[0] == f32::MAX {
            return Collision { water_grid_lazy: std::sync::OnceLock::new(), tris, tri_nz, cells: vec![], origin: [0.0, 0.0], cell_size, cols: 0, rows: 0,
                #[cfg(any(test, feature = "test-fixtures"))]
                z_min: 0.0,
                z_max: 0.0, water_grid: None,
                water: Err(eqoxide_core::region_map::RegionDataAbsent::NotAttached),
                from_collision_mesh, zone_line_regions: Vec::new(),
                climb_volumes: Vec::new() };
        }
        let cols = (((max[0] - min[0]) / cell_size).ceil() as usize + 1).max(1);
        let rows = (((max[1] - min[1]) / cell_size).ceil() as usize + 1).max(1);
        let mut cells: Vec<Vec<u32>> = vec![Vec::new(); cols * rows];

        for (ti, t) in tris.iter().enumerate() {
            let tmin_e = t[0][0].min(t[1][0]).min(t[2][0]);
            let tmax_e = t[0][0].max(t[1][0]).max(t[2][0]);
            let tmin_n = t[0][1].min(t[1][1]).min(t[2][1]);
            let tmax_n = t[0][1].max(t[1][1]).max(t[2][1]);
            let c0 = (((tmin_e - min[0]) / cell_size) as isize).max(0) as usize;
            let c1 = ((((tmax_e - min[0]) / cell_size) as isize).max(0) as usize).min(cols - 1);
            let r0 = (((tmin_n - min[1]) / cell_size) as isize).max(0) as usize;
            let r1 = ((((tmax_n - min[1]) / cell_size) as isize).max(0) as usize).min(rows - 1);
            for r in r0..=r1 {
                for c in c0..=c1 {
                    cells[r * cols + c].push(ti as u32);
                }
            }
        }
        Collision { water_grid_lazy: std::sync::OnceLock::new(), tris, tri_nz, cells, origin: min, cell_size, cols, rows,
            #[cfg(any(test, feature = "test-fixtures"))]
            z_min,
            z_max,
            water: Err(eqoxide_core::region_map::RegionDataAbsent::NotAttached),
            from_collision_mesh, zone_line_regions: Vec::new(),
            climb_volumes: crate::climb::volumes_from_objects(&assets.objects),
            water_grid: None }
    }

    /// Every climbable surface in this zone — the PHYSICAL question (#309). See `climb_volumes`.
    pub fn climb_volumes(&self) -> &[crate::climb::ClimbVolume] { &self.climb_volumes }

    /// Returns whether `p` (`[east, north, up]`) is on a climbable surface. The movement controller
    /// applies this test, and the test reads `climb_volumes` only, so whether a ladder leads anywhere
    /// standable never vetoes what the character's body can do.
    pub fn on_climbable(&self, p: [f32; 3]) -> bool {
        self.climb_volumes.iter().any(|v| v.contains(p))
    }

    /// **The one writer of this grid's region data**: attach the zone's `.wtr` map, or record WHY
    /// there is none. Call after `build`.
    ///
    /// Takes the loader's own `Result` (`RegionMap::try_load`) rather than an `Option` so the
    /// reason for an absent map is never dropped.
    pub fn set_region_data(
        &mut self,
        data: Result<std::sync::Arc<eqoxide_core::region_map::RegionMap>,
                     eqoxide_core::region_map::RegionLoadError>,
    ) {
        self.water = data.map_err(eqoxide_core::region_map::RegionDataAbsent::LoadFailed);
        // precompute the zone-line region points so `find_zone_line_near` is an O(1) cache read
        self.zone_line_regions = self.precompute_zone_line_regions();
    }

    /// Fixture shorthand for synthetic scenes: attach a hand-authored region map, or (with `None`)
    /// leave the grid with no region data at all.
    ///
    /// **Test-only, and gated so on purpose.** For a synthetic scene there is no `.wtr` and so no
    /// load that could have failed. `None` genuinely means [`RegionDataAbsent::NotAttached`], an
    /// explicit absence and not an empty answer. Because this is
    /// `#[cfg(any(test, feature = "test-fixtures"))]` it does not exist in a release build, so no
    /// production path can reach the reason-free spelling. Same gating as the `RegionMap::flat_below`
    /// / `water_slab` constructors it exists to accept.
    ///
    /// [`RegionDataAbsent::NotAttached`]: eqoxide_core::region_map::RegionDataAbsent::NotAttached
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn set_water(&mut self, water: Option<std::sync::Arc<eqoxide_core::region_map::RegionMap>>) {
        self.water = water.ok_or(eqoxide_core::region_map::RegionDataAbsent::NotAttached);
        self.zone_line_regions = self.precompute_zone_line_regions();
    }

    /// Returns this grid's region map when it has one. Every water and zone-line query below reads
    /// the region map through this accessor. A query that consumes `None` here answers "no data, so no water",
    /// which is physically the safe answer. Anything reported back as a fact must go through
    /// [`Collision::zone_line_indices`] (or `region_data_absent`) and surface the reason instead.
    pub fn region_map(&self) -> Option<&std::sync::Arc<eqoxide_core::region_map::RegionMap>> {
        self.water.as_ref().ok()
    }

    /// Returns why this grid has no region data, or `None` when it has some. A consumer refuses
    /// with this value instead of publishing an empty answer.
    pub fn region_data_absent(&self) -> Option<&eqoxide_core::region_map::RegionDataAbsent> {
        self.water.as_ref().err()
    }

    /// Builds the zone-line region cache from the water map, and warns if the build runs long.
    /// Returns an empty `Vec` when there is no water map or no zone-line regions.
    fn precompute_zone_line_regions(&self) -> Vec<(i32, [f32; 3])> {
        let Some(water) = self.region_map() else { return Vec::new(); };
        if self.cols == 0 || self.tris.is_empty() { return Vec::new(); }
        let (mut zmin, mut zmax) = (f32::MAX, f32::MIN);
        for t in &self.tris { for v in t { zmin = zmin.min(v[2]); zmax = zmax.max(v[2]); } }
        let bounds = (
            self.origin[0], self.origin[0] + self.cols as f32 * self.cell_size,
            self.origin[1], self.origin[1] + self.rows as f32 * self.cell_size,
            zmin, zmax,
        );
        let t0 = std::time::Instant::now();
        let regions = water.zone_line_region_points(bounds);
        let ms = t0.elapsed().as_millis();
        if ms > 250 {
            tracing::warn!("nav: zone-line region precompute took {ms}ms ({} region(s)) — slower than expected", regions.len());
        } else if !regions.is_empty() {
            tracing::info!("nav: precomputed {} zone-line region point(s) in {ms}ms", regions.len());
        }
        regions
    }

    /// Stores a prebuilt water-span grid on this collision, replacing any grid set before.
    pub fn set_water_grid(&mut self, grid: Option<crate::water_grid::WaterGrid>) {
        self.water_grid = grid;
    }

    /// The stored water-span grid, if one has been built (`None` until `set_water_grid`).
    pub fn water_grid(&self) -> Option<&crate::water_grid::WaterGrid> {
        self.water_grid.as_ref()
    }

    /// Returns the explicitly set water-span grid if one exists, otherwise the lazily built grid.
    /// Returns `None` for a zone with no `.wtr` water map, without building, so a dry zone pays
    /// nothing.
    ///
    /// The first call in a water zone runs [`Self::build_water_grid`] on the calling thread and
    /// caches the result, and every later call is a pointer read. The build is gated on
    /// `self.region_map().is_some()` before `get_or_init`, so a grid is never cached for a zone
    /// whose water map has not been attached yet.
    pub fn water_grid_active(&self) -> Option<&crate::water_grid::WaterGrid> {
        if let Some(g) = self.water_grid.as_ref() { return Some(g); }
        self.region_map()?;
        Some(self.water_grid_lazy.get_or_init(|| {
            let t0 = std::time::Instant::now();
            let g = self.build_water_grid(&crate::body::PLAYER_BODY);
            let ms = t0.elapsed().as_millis();
            tracing::info!("nav: built water-span grid lazily on first water plan — {} wet column(s), \
                {} node(s), {} unbounded-below, in {ms}ms (design §5.3, lazy per owner decision #5)",
                g.wet_column_count(), g.node_count(), g.unbounded_below_count());
            g
        }))
    }

    /// Builds the sparse **water-span grid** for this zone from the attached `.wtr` water map and
    /// this collision mesh, and returns an empty grid when the zone has no water map. The method
    /// stores nothing, so call `set_water_grid` to keep the result.
    ///
    /// Method: enumerate water-leaf AABBs from the BSP once (`water_region_aabbs`),
    /// iterate the 4u XY column lattice (aligned to this grid's `origin`) inside those AABBs, and for
    /// each candidate column build its navigable spans with [`Self::build_water_column`]. Candidate
    /// columns are de-duplicated (overlapping AABBs) but each is scanned over the water leaves' z-band
    /// so stacked water (a pool over a separate flooded tunnel — the qcat case) yields multiple spans.
    pub fn build_water_grid(&self, body: &crate::body::Body) -> crate::water_grid::WaterGrid {
        const COL: f32 = 4.0; // 4u XY water columns
        let mut grid = crate::water_grid::WaterGrid::new(self.origin, COL);
        let Some(water) = self.region_map() else { return grid; };
        if self.cols == 0 || self.tris.is_empty() { return grid; }

        // Zone z-extent — same source as the zone-line precompute (`precompute_zone_line_regions`).
        let (mut zmin, mut zmax) = (f32::MAX, f32::MIN);
        for t in &self.tris { for v in t { zmin = zmin.min(v[2]); zmax = zmax.max(v[2]); } }
        let bounds = (
            self.origin[0], self.origin[0] + self.cols as f32 * self.cell_size,
            self.origin[1], self.origin[1] + self.rows as f32 * self.cell_size,
            zmin, zmax,
        );
        let aabbs = water.water_region_aabbs(bounds);
        if aabbs.is_empty() { return grid; }

        // The z-band to scan each candidate column over = the UNION of the water leaves' z-ranges,
        // NOT the whole mesh z-extent (which can dip to invisible-boundary art at z ≈ -32768 and blow
        // the per-column probe count up by ~10⁴×). Water only exists inside this band, and it still
        // spans stacked water (a pool over a flooded tunnel) since both bands lie within it.
        let water_zlo = aabbs.iter().map(|(_, _, zr)| zr[0]).fold(f32::MAX, f32::min);
        let water_zhi = aabbs.iter().map(|(_, _, zr)| zr[1]).fold(f32::MIN, f32::max);

        // candidate columns from every water AABB (deduped). A loose (oblique) AABB only widens the
        // candidate set, and the `is_water` probes in `build_water_column` make that safe.
        let mut seen: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
        for (ex, ny, _zr) in &aabbs {
            let ci0 = ((ex[0] - self.origin[0]) / COL).floor() as i32;
            let ci1 = ((ex[1] - self.origin[0]) / COL).ceil() as i32;
            let cj0 = ((ny[0] - self.origin[1]) / COL).floor() as i32;
            let cj1 = ((ny[1] - self.origin[1]) / COL).ceil() as i32;
            for ci in ci0..=ci1 {
                for cj in cj0..=cj1 {
                    if !seen.insert((ci, cj)) { continue; }
                    let e = self.origin[0] + ci as f32 * COL + COL * 0.5; // column centre
                    let n = self.origin[1] + cj as f32 * COL + COL * 0.5;
                    match self.build_water_column(water, body, e, n, (water_zlo, water_zhi)) {
                        WaterColumnResult::Column(col) => grid.insert(ci, cj, col),
                        WaterColumnResult::UnboundedBelow => grid.note_unbounded_below(),
                        WaterColumnResult::Dry => {}
                    }
                }
            }
        }
        grid
    }

    /// Builds one 4u column's swimmable spans, or reports why the column has
    /// none. Scans the lattice z-band top→down for water bands (each `[bottom, surface]` from
    /// `surface_z`/`bottom_z`), then for each band carves the free segments between the collision
    /// solids inside it (`column_surfaces`, facing-blind, both windings) and applies:
    ///
    /// * `nav_hi = min(surface − float_depth, ceiling_above − height − SKIN)`
    /// * `nav_lo = max(water_bottom, floor_below) + ε`
    ///
    /// keeping only spans with `nav_hi ≥ nav_lo`. The face-normal winding (`nz`) is KEPT, not
    /// discarded: a segment whose TOP is an up-facing floor (`nz > 0`) AND whose BOTTOM is a
    /// down-facing ceiling (`nz < 0`) is the INTERIOR of a solid (a thick submerged slab with
    /// `.wtr` water painted through it — the water box does not cut holes around collision
    /// geometry), so no span is emitted there. `UnboundedBelow` when a wet probe's water has
    /// no bottom (`bottom_z` == None). The caller counts that case, and the column never gets a
    /// fabricated floor.
    fn build_water_column(&self, water: &eqoxide_core::region_map::RegionMap,
        body: &crate::body::Body, e: f32, n: f32, zrange: (f32, f32)) -> WaterColumnResult {
        use crate::water_grid::{VRES, WaterColumn};
        const EPS: f32 = 0.05;
        // = `movement::SKIN` (0.05, private there); the body-top must clear a solid ceiling by this,
        // exactly as the collided `swim_rise` stops `SKIN` short of the ceiling.
        const SKIN: f32 = 0.05;
        let (zlo, zhi) = zrange;
        if zhi <= zlo { return WaterColumnResult::Dry; }

        // 1. Find the distinct water bands in this column (top→down), deduped by containment. A
        //    column can hold >1 band (a pool over a flooded tunnel, dry rock between).
        let mut bands: Vec<(f32, f32)> = Vec::new(); // (water_bottom, water_surface)
        let mut unbounded = false;
        let mut z = zhi;
        while z >= zlo {
            if water.is_water(e, n, z) && !bands.iter().any(|&(b, s)| z > b - VRES && z < s + VRES) {
                match (water.surface_z(e, n, z), water.bottom_z(e, n, z)) {
                    (Some(s), Some(b)) if s > b => bands.push((b, s)),
                    (Some(_), None) => unbounded = true, // water with no bottom
                    _ => {}                              // unbounded above / degenerate — skip
                }
            }
            z -= VRES;
        }
        if bands.is_empty() {
            return if unbounded { WaterColumnResult::UnboundedBelow } else { WaterColumnResult::Dry };
        }

        // 2. Per band, carve navigable feet-intervals between the collision solids inside it.
        let mut spans: Vec<(f32, f32)> = Vec::new();
        let mut surface_top = f32::MIN;
        for &(wb, ws) in &bands {
            surface_top = surface_top.max(ws);
            let mid = (wb + ws) * 0.5;
            // Every solid surface (both windings) crossing the water band is a barrier plane, KEPT
            // with its face-normal sign (`nz`; see `column_surfaces`): nz>0 up-facing ⇒ solid
            // immediately BELOW it, nz<0 down-facing ⇒ solid immediately ABOVE it. A free
            // (navigable) segment is enclosed ABOVE by an "opener" (a ceiling nz<0, or the water
            // surface) and BELOW by a "closer" (a floor nz>0, or the water bottom); a segment whose
            // top is up-facing AND whose bottom is down-facing is the INTERIOR of a solid and emits
            // no span. BOTH windings must agree before the span is suppressed, because a mere OR
            // would delete genuine water under EQ's inverted-art normals (a down-facing surface can
            // be a walkable inverted floor, not a ceiling).
            let solids = self.column_surfaces(e, n, mid, ws - mid + 1.0, mid - wb + 1.0);
            // Partition by position. A solid FLUSH with a water bound is NOT dropped (that would
            // replace its real winding with the open-water sentinel and let the enclosed-solid test
            // misfire at the band edge); instead its winding is FOLDED into that
            // bound. A solid whose top is flush with / protrudes through the SURFACE (up-facing at
            // ws) caps the band from above → the top bound becomes a closer, not an opener. A mound
            // RESTING ON the water bottom (down-facing at wb — and water bottoms commonly sit ABOVE
            // the true floor, so this is not exotic) puts solid above the bottom → the bottom bound
            // becomes an opener. Inner solids keep their own winding.
            let mut inner: Vec<(f32, f32)> = Vec::new();
            let (mut top_capped, mut bottom_open) = (false, false);
            for (sz, nz) in solids {
                if sz >= ws - 1e-3 {                    // at / above the surface
                    if nz > 0.0 { top_capped = true; }  //   up-facing ⇒ solid below the surface
                } else if sz <= wb + 1e-3 {             // at / below the bottom
                    if nz < 0.0 { bottom_open = true; } //   down-facing ⇒ solid above the bottom
                } else {
                    inner.push((sz, nz));               // genuinely inside the band
                }
            }
            inner.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)); // desc by z
            // Bound windings, CLAMPED to any flush solid (else the open-water defaults: surface is
            // an opener, bottom is a closer). NOTE: a solid protruding MORE than the scan margin
            // (~1u) above the surface is out of range and falls back to the open-water default — the
            // remaining span is then the accepted inverted-art ambiguity, not fabricated here.
            let ws_nz = if top_capped { 1.0 } else { -1.0 };
            let wb_nz = if bottom_open { -1.0 } else { 1.0 };
            // Free segments = consecutive pairs of [(ws, ws_nz), inner…, (wb, wb_nz)]. A segment
            // whose TOP is `ws` is open to the surface (cap = swim plane); one capped by a barrier
            // must clear it.
            let mut boundaries: Vec<(f32, f32)> = Vec::with_capacity(inner.len() + 2);
            boundaries.push((ws, ws_nz));
            boundaries.extend_from_slice(&inner);
            boundaries.push((wb, wb_nz));
            for w in boundaries.windows(2) {
                let (seg_hi, hi_nz) = w[0];
                let (seg_lo, lo_nz) = w[1];
                if seg_hi - seg_lo < EPS { continue; }
                // Enclosed solid interior: an up-facing floor on top and a down-facing ceiling below
                // — no water here (thick submerged slab, surface-flush solid, bottom-resting mound;
                // #534). Emit nothing.
                if hi_nz > 0.0 && lo_nz < 0.0 { continue; }
                let top_is_surface = (seg_hi - ws).abs() < 1e-3;
                let ceiling_cap = if top_is_surface { f32::INFINITY }
                                  else { seg_hi - body.height - SKIN };
                let nav_hi = (ws - body.float_depth).min(ceiling_cap);
                let nav_lo = seg_lo.max(wb) + EPS;
                if nav_hi >= nav_lo {
                    spans.push((nav_lo, nav_hi));
                }
            }
        }
        if spans.is_empty() {
            return if unbounded { WaterColumnResult::UnboundedBelow } else { WaterColumnResult::Dry };
        }
        // High band first (mirrors `column_surfaces`' high→low convention).
        spans.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        WaterColumnResult::Column(WaterColumn { surface_z: surface_top, spans })
    }

    /// True if `pos` = [east, north, z] (server coords) lies in a water region.
    /// False when the zone has no water map. Used to gate swim (vertical) movement.
    pub fn in_water(&self, pos: [f32; 3]) -> bool {
        self.region_map().is_some_and(|w| w.is_water(pos[0], pos[1], pos[2]))
    }

    /// Water-surface height above a submerged `pos`, or `None` if not in water / no bounded surface.
    /// Used by the controller's buoyancy to float toward the surface (#172).
    pub fn water_surface(&self, pos: [f32; 3]) -> Option<f32> {
        self.region_map().and_then(|w| w.surface_z(pos[0], pos[1], pos[2]))
    }

    /// Returns the zone-point index carried by the zone-line (`DRNTP`) region that contains `pos` =
    /// [east, north, z] (server coords). The index equals the `OP_SendZonepoints` `iterator` for that
    /// line and resolves the destination zone. Returns `None` when not on a zone line, with no region
    /// map, or with a v1 map. The native client triggers a crossing the same way, by detecting the
    /// region from zone geometry rather than a coordinate list.
    pub fn zone_line_at(&self, pos: [f32; 3]) -> Option<i32> {
        self.region_map().and_then(|w| w.zone_line_at(pos[0], pos[1], pos[2]))
    }

    /// Returns the zone-line index that a **standing** character occupies at `(east, north,
    /// feet_z)`, probing the character's vertical body span `[feet_z, feet_z + PLAYER_BODY.height]`
    /// rather than the single feet point that [`Self::zone_line_at`] tests.
    ///
    /// A DRNTP trigger volume whose lower face floats just ABOVE the walkable floor is invisible to a
    /// feet-only probe, because the character stands with its feet *below* the volume while its body
    /// occupies it. The qeynos2 Knights-of-Truth waterfall is an example. The trigger's lower face
    /// sits ≈0.4u over the flat vault floor (feet rest at z≈-14.0 and the volume starts ≈-13.6), so
    /// `zone_line_at([x,y,-14.0])` never detects a character standing there. Sweeping the body span
    /// detects the trigger wherever the body intersects the volume.
    ///
    /// Returns the first (lowest) index found scanning feet→head. Point-samples at ≤1u spacing, so a
    /// zone-line slab thinner than the vertical sample step is the only miss, far below any shipped
    /// DRNTP thickness.
    pub fn zone_line_at_standing(&self, pos: [f32; 3]) -> Option<i32> {
        let w = self.region_map()?;
        const STEP: f32 = 1.0;
        let feet = pos[2];
        let head = feet + crate::body::PLAYER_BODY.height;
        // Inclusive feet→head sweep; the explicit final `head` sample guards a step overshoot.
        let mut z = feet;
        while z < head {
            if let Some(idx) = w.zone_line_at(pos[0], pos[1], z) { return Some(idx); }
            z += STEP;
        }
        w.zone_line_at(pos[0], pos[1], head)
    }

    /// Returns the distinct zone-point indices of every zone-line region in this zone. The indices
    /// form the set of exits. Each links to an entrance via the `OP_SendZonepoints` `iterator`.
    ///
    /// `Ok(vec![])` is a real reading: this zone's region map loaded and contains no zone-line
    /// regions. `Err` means the question could not be answered at all, and names why.
    ///
    /// A v1 map is not an exception to that. v1 records carry no `zone_line_index` field, so every
    /// v1 zone line reads as index **0**, and index 0 is *included*, so this returns `[0]`, not
    /// `[]`. See `RegionMap::zone_line_indices` and
    /// `a_zero_index_zone_line_region_is_still_recognized_683`.
    ///
    /// The `Result` gives a `.wtr` that did not load a different type from a zone with no zone
    /// lines, so every caller handles the second case explicitly.
    pub fn zone_line_indices(&self)
        -> Result<Vec<i32>, eqoxide_core::region_map::RegionDataAbsent>
    {
        match self.water.as_ref() {
            Ok(w) => Ok(w.zone_line_indices()),
            Err(absent) => Err(absent.clone()),
        }
    }

    /// Returns the `(region_index, point)` of the zone-line region nearest to `near` (= [east,
    /// north, z]). When `index` is `Some`, only that zone-point index matches, and `None` matches
    /// any zone line. Returns `None` if the zone has no region map or no matching region exists.
    ///
    /// The lookup is an O(regions) read of the region points precomputed at `set_region_data`, with
    /// no scan. The returned point is projected onto the WALKABLE FLOOR beneath the region, because
    /// a region point's z is an interior point of the region VOLUME. Across the shipped zones that z
    /// sits 1.5u to 127u ABOVE the real floor and is never a floor height (halas is the one line
    /// whose region z is nearly floor-level).
    ///
    /// The projection is only taken when the region STILL CONTAINS the projected floor point, so a
    /// tall vertical translocator whose footprint doesn't reach the ground keeps its original z
    /// instead of leaving its trigger volume. Candidates are tried nearest-first, preferring one
    /// that projects (a region point hanging over a void is skipped).
    pub fn find_zone_line_near(&self, index: Option<i32>, near: [f32; 3]) -> Option<(i32, [f32; 3])> {
        // rank by distance to the PROJECTED (standable) point, not the raw region point, and weight
        // vertical separation far above horizontal. A zone line is often baked as several stacked
        // DRNTP leaves over more than one XY, and gfaydark's butcher line has a leaf whose only floor
        // is an isolated 74u-high ledge and another that sits on the ground.
        const Z_WEIGHT: f32 = 4.0;
        let cost = |p: &[f32; 3]| (p[0] - near[0]).hypot(p[1] - near[1]) + Z_WEIGHT * (p[2] - near[2]).abs();
        let best_projected = self.zone_line_regions
            .iter()
            .filter(|(idx, _)| index.is_none_or(|want| want == *idx))
            .filter_map(|&(idx, p)| self.zone_line_floor_point(idx, p).map(|fp| (idx, fp)))
            .min_by(|a, b| cost(&a.1).total_cmp(&cost(&b.1)));
        best_projected.or_else(|| self.zone_line_regions // nothing projects (no floor under any),
            .iter()                                      // so use the raw region point
            .filter(|(idx, _)| index.is_none_or(|want| want == *idx))
            .min_by(|a, b| cost(&a.1).total_cmp(&cost(&b.1)))
            .copied())
    }

    /// Projects a zone-line region's representative point down onto the walkable floor beneath it.
    /// Returns `None` when there is no floor under it, or when the region does NOT extend down to
    /// that floor (so standing there would not trigger the crossing).
    fn zone_line_floor_point(&self, index: i32, p: [f32; 3]) -> Option<[f32; 3]> {
        const REGION_DROP: f32 = 400.0;
        let fz = self.floor_beneath(p[0], p[1], p[2], 2.0, REGION_DROP)?;
        // standing on that floor must still be inside the region, else the spot never fires a crossing
        let inside = self.region_map()
            .and_then(|w| w.zone_line_at(p[0], p[1], fz + 1.0)) == Some(index);
        inside.then_some([p[0], p[1], fz])
    }

    /// Returns the nearest zone-line region whose index is in `in_zone_idxs` (a same-zone
    /// destination such as an escape translocator, not a normal neighbour-zone exit) AND whose
    /// `DRNTP` footprint is present at the caller's floor height `near[2]` under the region's XY.
    /// Returns `None` if no such reachable in-zone portal exists.
    ///
    /// `zone_line_at` is a z-EXACT BSP test. A tall vertical translocator (the Qeynos guild-vault
    /// waterfall) bakes several DRNTP leaves up its column, and a leaf near the TOP yields an
    /// interior point high up. A character at that XY on the vault floor stands BELOW the leaf and
    /// never triggers it. The filter keeps only regions still present at the character's own height
    /// (`zone_line_at([x, y, near_z]) == idx`), so the footprint reaches the floor the character
    /// stands on.
    pub fn find_reachable_in_zone_line(&self, in_zone_idxs: &[i32], near: [f32; 3]) -> Option<(i32, [f32; 3])> {
        self.zone_line_regions
            .iter()
            .filter(|(idx, loc)| in_zone_idxs.contains(idx)
                && self.zone_line_at([loc[0], loc[1], near[2]]) == Some(*idx))
            .min_by(|a, b| {
                let d2 = |p: &[f32; 3]| (p[0] - near[0]).powi(2) + (p[1] - near[1]).powi(2) + (p[2] - near[2]).powi(2);
                d2(&a.1).partial_cmp(&d2(&b.1)).unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied()
    }

    #[inline]
    fn cell_range(&self, min_e: f32, min_n: f32, max_e: f32, max_n: f32) -> (usize, usize, usize, usize) {
        let c0 = (((min_e - self.origin[0]) / self.cell_size) as isize).clamp(0, self.cols as isize - 1) as usize;
        let c1 = (((max_e - self.origin[0]) / self.cell_size) as isize).clamp(0, self.cols as isize - 1) as usize;
        let r0 = (((min_n - self.origin[1]) / self.cell_size) as isize).clamp(0, self.rows as isize - 1) as usize;
        let r1 = (((max_n - self.origin[1]) / self.cell_size) as isize).clamp(0, self.rows as isize - 1) as usize;
        (c0, c1, r0, r1)
    }

    /// Sample the floor height directly beneath `(east, north)`.
    ///
    /// Casts a true downward ray using Möller–Trumbore so only surfaces *below*
    /// the player are considered. Surfaces above the ray origin (bridges, balcony
    /// undersides) have negative t and are never returned.
    pub fn floor_z(&self, east: f32, north: f32, fallback: f32) -> f32 {
        if self.cols == 0 { return fallback; }
        let ray_start = [east, north, fallback + 2.0];
        let ray_end   = [east, north, fallback - 100.0];
        match self.nearest_hit_t(ray_start, ray_end) {
            Some(t) => ray_start[2] + t * (ray_end[2] - ray_start[2]),
            None    => fallback,
        }
    }

    /// Cast a ray upward from `(east, north, z_start)` and return the height
    /// of the nearest ceiling hit, or `fallback` if none found.
    pub fn ceiling_z(&self, east: f32, north: f32, z_start: f32, max_up: f32, fallback: f32) -> f32 {
        if self.cols == 0 { return fallback; }
        let ray_start = [east, north, z_start];
        let ray_end   = [east, north, z_start + max_up];
        match self.nearest_hit_t(ray_start, ray_end) {
            Some(t) => ray_start[2] + t * (ray_end[2] - ray_start[2]),
            None    => fallback,
        }
    }

    /// EVERY surface a vertical column over `[ref_z - down, ref_z + up]` crosses at `(east, north)`,
    /// as `(height, face_normal_z)`, sorted **high → low**. `face_normal_z > 0` = up-facing (a
    /// floor); `< 0` = down-facing (a ceiling / a bridge's underside). Near-vertical walls are
    /// parallel to the ray and never register. Diagnostic/offline-probe entry point and the shared
    /// primitive behind `nearest_floor`.
    pub fn column_surfaces(&self, east: f32, north: f32, ref_z: f32, up: f32, down: f32) -> Vec<(f32, f32)> {
        let mut hits = Vec::new();
        self.column_hits(ColumnQuery { east, north, ref_z, up, down, floors_only: false }, &mut hits);
        hits
    }

    /// Shared vertical-column raycast (Möller–Trumbore). Appends `(hit_z, face_normal_z)` to `out`,
    /// sorted high→low.
    ///
    /// When `floors_only`, returns only **standable** surfaces. The test is
    /// facing-blind, so a surface is ground iff `|nz| >= NAV_NEAR_HORIZONTAL` AND it has
    /// `NAV_AGENT_HEIGHT` of open space above it before the next SOLID surface. The winding sign
    /// plays no part, because inverted-art floor is real floor. The ceiling defence is HEADROOM (a
    /// ceiling has its roof right above it, so it fails) plus the caller's `ref_z ± window` (a far
    /// roof, e.g. qcat's 391.8, is simply outside the window). Both `column_hits(true)` and
    /// `ground_below` go through this, so the two cannot disagree about where the floor is.
    fn column_hits(&self, query: ColumnQuery, out: &mut Vec<(f32, f32)>) {
        let ColumnQuery { east, north, ref_z, up, down, floors_only } = query;
        out.clear();
        if self.cols == 0 { return; }
        let z_top = ref_z + up.max(0.0);
        let z_bot = ref_z - down.max(0.0);
        let filter = floors_only;
        // the standable test needs each surface's headroom, the distance UP to the next SOLID surface
        // of EITHER winding. That surface can lie ABOVE the caller's window, so gather facing-blind
        // from `z_bot` up to the zone top, classify, then return only in-window standable surfaces.
        // The triangles match the window scan (a cell's list is fixed) and only the ray is longer.
        let gather_top = if filter { self.z_max.max(z_top) + 1.0 } else { z_top };
        let dir_z = z_bot - gather_top; // negative (downward)
        let ray_len = dir_z.abs();
        if ray_len < MIN_RAY_LEN { return; } // the ONE degenerate-ray guard
        let ray_scale = axis_scale([east, north, gather_top]).max(axis_scale([east, north, z_bot]));
        let eps = 1e-6_f32;
        let cross = |a: [f32; 3], b: [f32; 3]| [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ];
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let from = [east, north, gather_top];
        let dir = [0.0, 0.0, dir_z];
        let (c0, c1, r0, r1) = self.cell_range(east, north, east, north);
        // `all` = every surface (both windings) in [z_bot, gather_top]; `out` gets the standable
        // in-window subset. For `filter=false` these coincide (facing-blind, window only).
        let all = out; // reuse the caller's buffer for the raw gather
        for r in r0..=r1 {
            for c in c0..=c1 {
                for &ti in &self.cells[r * self.cols + c] {
                    let nz = self.tri_nz[ti as usize];
                    let tri = &self.tris[ti as usize];
                    let (v0, v1, v2) = (tri[0], tri[1], tri[2]);
                    let e1 = [v1[0] - v0[0], v1[1] - v0[1], v1[2] - v0[2]];
                    let e2 = [v2[0] - v0[0], v2[1] - v0[1], v2[2] - v0[2]];
                    let p = cross(dir, e2);
                    let det = dot(e1, p);
                    if det.abs() < eps { continue; }
                    let inv = 1.0 / det;
                    let tvec = [from[0] - v0[0], from[1] - v0[1], from[2] - v0[2]];
                    let u = dot(tvec, p) * inv;
                    if !(0.0..=1.0).contains(&u) { continue; }
                    let q = cross(tvec, e1);
                    let v = dot(dir, q) * inv;
                    if v < 0.0 || u + v > 1.0 { continue; }
                    let t = dot(e2, q) * inv;
                    // the ONE acceptance test, shared with `nearest_hit*`. The scale spans the
                    // ray origin AND the triangle, because either can be the coarsest `f32` here.
                    let scale = ray_scale.max(axis_scale(v0)).max(axis_scale(v1)).max(axis_scale(v2));
                    if !hit_accepted(t, ray_len, scale) { continue; }
                    all.push((gather_top + t * dir_z, nz));
                }
            }
        }
        all.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)); // high→low
        if !filter { return; } // facing-blind gather = the window scan; done.

        // Classify each surface as standable and retain only the in-window ones. Headroom is the gap up
        // to the next surface MORE than a slab-thickness above (so the two triangles of one quad floor
        // don't read as each other's ceiling). The topmost surface has open sky above → infinite.
        //
        // three ceiling cases, and which are defended
        //   * FAR ROOF (qcat's 391.8 over a −70 floor): DEFENDED by the caller's `ref_z ± window`, since
        //     a character never queries at roof height and the roof is out of range.
        //   * CLOSE ROOF (a room ceiling with its roof right above): DEFENDED by `headroom` below, since
        //     a solid surface within `NAV_AGENT_HEIGHT` means no standing room.
        //   * OPEN-TOPPED MID-HEIGHT down-facing surface (a flat ceiling with open sky above): KNOWINGLY
        //     ADMITTED as walkable. It is GEOMETRICALLY IDENTICAL to qcat's walkable −42.97 walkway
        //     (down-facing, open above, a floor below), so no per-surface rule can accept the qcat floor
        //     while rejecting this. A mitigation belongs in a check that a route leads onto the surface,
        //     not in a per-surface classifier. See the acceptance test
        //     `open_topped_midheight_surface_is_admitted_as_the_accepted_cost_of_facing_blindness`.
        const SAME_SURFACE: f32 = 0.3;
        let n = all.len();
        let mut keep: Vec<(f32, f32)> = Vec::with_capacity(n);
        for i in 0..n {
            let (z, nz) = all[i];
            if z < z_bot - eps || z > z_top + eps { continue; } // out of the caller's window
            if nz.abs() < NAV_NEAR_HORIZONTAL { continue; }     // too steep to stand on
            // Nearest solid strictly above `z` (indices < i are higher, sorted high→low).
            let mut headroom = f32::INFINITY;
            for j in (0..i).rev() {
                if all[j].0 > z + SAME_SURFACE { headroom = all[j].0 - z; break; }
            }
            if headroom < NAV_AGENT_HEIGHT { continue; }         // under a ceiling — not standing room
            keep.push((z, nz));
        }
        *all = keep;
    }

    /// Find the walkable FLOOR height at `(east, north)` nearest to `ref_z`.
    ///
    /// Casts a vertical column over `[ref_z - down, ref_z + up]`, gathers every UP-FACING triangle
    /// it crosses (a ceiling is not a floor), and returns the one whose height is **closest
    /// to `ref_z`**. This is the surface the player would actually stand on (or step to), and —
    /// unlike a single top-down ray — it does NOT mistake an overhang/awning/bridge ABOVE the floor
    /// for the floor itself. `up` bounds how far you can step UP onto a ledge; `down` how far you
    /// can drop. Returns `None` when no floor exists in the band.
    pub fn nearest_floor(&self, east: f32, north: f32, ref_z: f32, up: f32, down: f32) -> Option<f32> {
        let mut hits = Vec::new();
        self.column_hits(ColumnQuery { east, north, ref_z, up, down, floors_only: true }, &mut hits);
        hits.into_iter()
            .map(|(z, _)| z)
            .min_by(|a, b| (a - ref_z).abs().partial_cmp(&(b - ref_z).abs()).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// Returns the highest walkable floor at or below `z`, searching down `down` units and up `up`
    /// units for a point that sits a little UNDER the floor it names. Projects a point that lives
    /// in a VOLUME (a `DRNTP` zone-line region's interior point, whose z is never a floor height)
    /// down onto ground a character can stand on.
    pub fn floor_beneath(&self, east: f32, north: f32, z: f32, up: f32, down: f32) -> Option<f32> {
        let mut hits = Vec::new();
        self.column_hits(ColumnQuery {
            east, north, ref_z: z + up.max(0.0), up: 0.0, down: up.max(0.0) + down.max(0.0), floors_only: true,
        }, &mut hits);
        hits.first().map(|&(z, _)| z) // high→low ⇒ the first is the highest floor at/below z+up
    }

    /// Nearest geometry hit along segment `from → to`, as fraction `t ∈ [0,1]`.
    /// Both points are GPU world space `[east, north, height]`. Möller–Trumbore.
    ///
    /// Acceptance is [`hit_accepted`] — the SAME test [`Collision::nearest_hit`] and
    /// [`Collision::column_hits`] use, and the same [`MIN_RAY_LEN`] degenerate-ray guard.
    /// See [`hit_accepted`] for the bounds of the accepted `t`.
    pub fn nearest_hit_t(&self, from: [f32; 3], to: [f32; 3]) -> Option<f32> {
        if self.cols == 0 { return None; }
        let dir = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        let ray_len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
        if ray_len < MIN_RAY_LEN { return None; }
        let ray_scale = axis_scale(from).max(axis_scale(to));
        let eps = 1e-6_f32;
        let cross = |a: [f32; 3], b: [f32; 3]| [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ];
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let (c0, c1, r0, r1) = self.cell_range(
            from[0].min(to[0]), from[1].min(to[1]), from[0].max(to[0]), from[1].max(to[1]),
        );
        let mut best: Option<f32> = None;
        // A triangle may sit in several cells; testing it more than once is harmless
        // (same t), so we skip dedup bookkeeping for short query segments.
        for r in r0..=r1 {
            for c in c0..=c1 {
                for &ti in &self.cells[r * self.cols + c] {
                    let tri = &self.tris[ti as usize];
                    let (v0, v1, v2) = (tri[0], tri[1], tri[2]);
                    let e1 = [v1[0] - v0[0], v1[1] - v0[1], v1[2] - v0[2]];
                    let e2 = [v2[0] - v0[0], v2[1] - v0[1], v2[2] - v0[2]];
                    let p = cross(dir, e2);
                    let det = dot(e1, p);
                    if det.abs() < eps { continue; }
                    let inv = 1.0 / det;
                    let tvec = [from[0] - v0[0], from[1] - v0[1], from[2] - v0[2]];
                    let u = dot(tvec, p) * inv;
                    if !(0.0..=1.0).contains(&u) { continue; }
                    let q = cross(tvec, e1);
                    let v = dot(dir, q) * inv;
                    if v < 0.0 || u + v > 1.0 { continue; }
                    let t = dot(e2, q) * inv;
                    let scale = ray_scale.max(axis_scale(v0)).max(axis_scale(v1)).max(axis_scale(v2));
                    if hit_accepted(t, ray_len, scale) && best.is_none_or(|b| t < b) {
                        best = Some(t);
                    }
                }
            }
        }
        best
    }

    // ───────────────────────── Component A: additive movement queries ─────────────────────────
    // These operate generically on `tris` and never touch `build`/the struct fields, so they work
    // whether or not Component B has enriched the triangle set (INVIS faces). See design §5.

    /// True when zone geometry is loaded (the broad-phase grid is non-empty). Callers use this to
    /// skip collision/depenetration entirely when no zone mesh is present.
    pub fn has_geometry(&self) -> bool { self.cols != 0 }

    /// The grid has real BOUNDS **and** at least one triangle in it — the same pair of conditions
    /// [`build_water_grid`] already guards itself with, and what
    /// `zone_assets::ZoneAssetState::ready` requires before it calls a zone loaded.
    ///
    /// The predicate is *equivalent* to [`has_geometry`], because [`Collision::build`] already
    /// returns `cols: 0` whenever `tris` is empty. It is its own predicate so the "there is a world
    /// here" test does not depend on that internal coupling (`has_geometry` is a *bounds* proxy).
    /// It does NOT reject a grid whose only triangles are degenerate, because no cheap check
    /// distinguishes those.
    pub fn has_triangles(&self) -> bool { self.cols != 0 && !self.tris.is_empty() }

    /// Like [`nearest_hit_t`] but also returns the hit triangle's **unit normal**, flipped to
    /// oppose the segment direction (so it faces back toward `from`). Used by [`sweep`] to provide
    /// the slide plane for collide-and-slide. Möller–Trumbore over the broad-phase cells.
    ///
    /// Same [`hit_accepted`] and [`MIN_RAY_LEN`] as [`nearest_hit_t`] and
    /// [`Collision::column_hits`].
    pub fn nearest_hit(&self, from: [f32; 3], to: [f32; 3]) -> Option<(f32, [f32; 3])> {
        if self.cols == 0 { return None; }
        let dir = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        let ray_len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
        if ray_len < MIN_RAY_LEN { return None; }
        let ray_scale = axis_scale(from).max(axis_scale(to));
        let eps = 1e-6_f32;
        let cross = |a: [f32; 3], b: [f32; 3]| [
            a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0],
        ];
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let (c0, c1, r0, r1) = self.cell_range(
            from[0].min(to[0]), from[1].min(to[1]), from[0].max(to[0]), from[1].max(to[1]),
        );
        let mut best: Option<(f32, [f32; 3])> = None;
        for r in r0..=r1 {
            for c in c0..=c1 {
                for &ti in &self.cells[r * self.cols + c] {
                    let tri = &self.tris[ti as usize];
                    let (v0, v1, v2) = (tri[0], tri[1], tri[2]);
                    let e1 = [v1[0] - v0[0], v1[1] - v0[1], v1[2] - v0[2]];
                    let e2 = [v2[0] - v0[0], v2[1] - v0[1], v2[2] - v0[2]];
                    let p = cross(dir, e2);
                    let det = dot(e1, p);
                    if det.abs() < eps { continue; }
                    let inv = 1.0 / det;
                    let tvec = [from[0] - v0[0], from[1] - v0[1], from[2] - v0[2]];
                    let u = dot(tvec, p) * inv;
                    if !(0.0..=1.0).contains(&u) { continue; }
                    let q = cross(tvec, e1);
                    let v = dot(dir, q) * inv;
                    if v < 0.0 || u + v > 1.0 { continue; }
                    let t = dot(e2, q) * inv;
                    let scale = ray_scale.max(axis_scale(v0)).max(axis_scale(v1)).max(axis_scale(v2));
                    if hit_accepted(t, ray_len, scale) && best.is_none_or(|(b, _)| t < b) {
                        // Geometric normal e1×e2, normalised, flipped to face back toward `from`.
                        let mut n = cross(e1, e2);
                        let nl = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                        if nl > 1e-9 { n = [n[0] / nl, n[1] / nl, n[2] / nl]; }
                        if dot(n, dir) > 0.0 { n = [-n[0], -n[1], -n[2]]; }
                        best = Some((t, n));
                    }
                }
            }
        }
        best
    }

    /// Returns the height of the nearest **standable** surface at or below `origin_z` within `depth`,
    /// or `None` if there is none. The native ground clamp uses `origin = foot_z + 1.0` and
    /// `depth = 200`.
    ///
    /// This is the controller's floor clamp. It reads `column_hits` with `floors_only` set, so the
    /// clamp and `column_hits` cannot disagree about where the floor is, and it returns the
    /// highest standable surface in `[origin_z − depth, origin_z]`. A surface under a ceiling
    /// (headroom `< NAV_AGENT_HEIGHT`) is not standable, so the clamp never lands on it.
    pub fn ground_below(&self, east: f32, north: f32, origin_z: f32, depth: f32) -> Option<f32> {
        if self.cols == 0 { return None; }
        let mut hits = Vec::new();
        self.column_hits(ColumnQuery {
            east, north, ref_z: origin_z, up: 0.0, down: depth.max(0.0), floors_only: true,
        }, &mut hits);
        hits.first().map(|&(z, _)| z) // sorted high→low ⇒ first = highest standable at/below origin_z
    }

    /// Returns whether the player's cylindrical footprint at `(east, north, foot_z)` is clear of
    /// geometry. Samples a horizontal ring of `n` directions at `radius` (and the centre) at chest
    /// height, returning `true` only when none are blocked. The depenetration net uses it.
    pub fn footprint_clear(&self, east: f32, north: f32, foot_z: f32, radius: f32, n: usize) -> bool {
        if self.cols == 0 { return true; }
        // any spoke hit within the ring means blocked, so a wall AT the radius counts as embedded.
        // A wall flush with the ring CENTRE is reported too, and a body that embedded is the
        // depenetration net's subject, so the direction is toward "recover".
        // `line_of_sight_does_not_see_through_a_wall_it_is_almost_touching` pins this.
        self.ring_nearest_hit(east, north, foot_z + crate::body::PLAYER_BODY.ring, radius, n)
            .is_none()
    }

    /// The radial-ring core of [`footprint_clear`], cast at an EXPLICIT world height `ring_z` instead
    /// of `foot_z + ring`. Casts `n` rays outward from the centre to `radius` and returns the NEAREST
    /// geometry hit as a fraction [0,1] of the spoke length (`None` = every spoke clear).
    ///
    /// Unlike a travel-parallel feeler (`path_clear`'s sweep), a radial spoke fans in EVERY direction,
    /// so it crosses a wall that lies within `radius` in ANY direction, including one that the
    /// caller's motion runs PARALLEL to. `path_clear` samples this ring along the swept segment to
    /// catch such a wall. Returning the fraction (not a bool) lets each caller pick its own
    /// boundary. `footprint_clear` blocks on any hit, and `path_clear` blocks only STRICTLY within
    /// radius (a wall exactly at radius is tangent, touching and not overlapping, matching the
    /// feeler sweep's parallel-tangent-is-clear convention).
    fn ring_nearest_hit(&self, east: f32, north: f32, ring_z: f32, radius: f32, n: usize) -> Option<f32> {
        let c = [east, north, ring_z];
        let n = n.max(1);
        let mut best: Option<f32> = None;
        for i in 0..n {
            let a = (i as f32) / (n as f32) * std::f32::consts::TAU;
            let to = [east + a.cos() * radius, north + a.sin() * radius, ring_z];
            if let Some(t) = self.nearest_hit_t(c, to) {
                best = Some(best.map_or(t, |b| b.min(t)));
            }
        }
        best
    }

    /// Is `from → to` blocked by geometry before ~92% of the way? Used for nameplate
    /// occlusion; the cutoff keeps the NPC's own feet/floor from counting as occluders.
    pub fn segment_blocked(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        self.nearest_hit_t(from, to).is_some_and(|t| t < 0.92)
    }

    /// Is the LINE `from → to` unobstructed? A single centre ray, extended past `to` by `radius`.
    ///
    /// This is a LINE-OF-SIGHT primitive. It answers "can I see/shoot from here to there", NOT
    /// "can the character WALK from here to there". The character is a cylinder, not a line, so use
    /// `path_clear` for anything the character has to physically traverse.
    pub fn line_clear(&self, from: [f32; 3], to: [f32; 3], radius: f32) -> bool {
        let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if dist < 1e-5 { return true; }
        let ext = (dist + radius.max(0.0)) / dist;
        let target = [from[0] + d[0] * ext, from[1] + d[1] * ext, from[2] + d[2] * ext];
        self.nearest_hit_t(from, target).is_none()
    }

    /// Returns whether the player's COLLISION VOLUME can travel from `from` to `to` without
    /// crossing geometry.
    ///
    /// The character is a cylinder of `PLAYER_RADIUS`, so a centre ray is not enough. A corner can
    /// be ray-clear and capsule-blocked, because the ray threads the gap and the shoulder does not.
    /// The method sweeps the volume with feelers across the whole diameter, offset perpendicular to
    /// the horizontal motion.
    ///
    /// The shared artifact with the controller is the body geometry. The probe HEIGHTS both sides
    /// use come from `PLAYER_BODY`, so the top probe IS the controller's chest contact ray. This
    /// method sweeps a strictly WIDER lateral pattern (5 feelers against the mover's centre ray plus
    /// radius back-off), so a clear segment here is passable for the controller laterally, up to the
    /// parallel-wall limit below.
    ///
    /// Cost is 5 feeler rays instead of 1, plus the footprint ring samples along the segment. The
    /// grid broad-phase keeps each ray to a handful of triangles.
    ///
    /// Returns `true` (clear) when there is no zone geometry loaded.
    pub fn path_clear(&self, from: [f32; 3], to: [f32; 3], radius: f32) -> bool {
        let d = [to[0] - from[0], to[1] - from[1]];
        let hlen = (d[0] * d[0] + d[1] * d[1]).sqrt();
        let r = radius.max(0.0);
        // Purely vertical (or zero-length) motion has no horizontal shoulders to sweep.
        if hlen < 1e-5 || r < 1e-5 { return self.line_clear(from, to, radius); }
        let perp = [-d[1] / hlen * r, d[0] / hlen * r];
        // feelers across the whole diameter, not just the two shoulders. Three rays (centre + both
        // shoulders) leak on DIAGONAL motion. The shoulders are offset perpendicular to the travel
        // direction, so against a wall the segment crosses at an angle, one shoulder starts already
        // PAST the wall plane and the other never reaches it. Both slide along the wall instead of
        // across it, and the capsule threads a slot narrower than itself (a diagonal edge threaded a
        // 2.5u slot with a 2.0u clearance while all three rays called it clear).
        //
        // Sampling the diameter at r/2 spacing closes it, since a wall panel that ENDS inside the
        // swept rectangle has a feeler either side of its end. This approximates a swept disc, so a
        // needle thinner than r/2 threading between feelers would still slip.
        //
        // The travel-parallel feelers alone are BLIND to a wall the segment runs ALONGSIDE, because a
        // ray parallel to a plane never intersects it. The parallel case is closed BELOW by ALSO
        // sampling the body footprint ring along the segment.
        const FEELERS: [f32; 5] = [-1.0, -0.5, 0.0, 0.5, 1.0];
        let feelers_clear = FEELERS.iter().all(|&f| self.line_clear(
            [from[0] + perp[0] * f, from[1] + perp[1] * f, from[2]],
            [to[0] + perp[0] * f, to[1] + perp[1] * f, to[2]],
            radius,
        ));
        if !feelers_clear {
            return false;
        }

        // parallel-wall coverage. The character occupies a disc along the WHOLE segment, not just a
        // swept rectangle. Sample the body's footprint RING (`footprint_clear`'s radial-spoke core,
        // reused via `ring_nearest_hit`) at points spaced along the segment. A radial spoke fans in
        // every direction, so it crosses a wall the segment merely runs parallel to. The feelers
        // cannot see such a wall. Cast at the segment's own swept height (`from[2]`/`to[2]`,
        // interpolated), the SAME chest/feet probe heights the feelers use.
        //
        // sample only STRICTLY INTERIOR points, never the two endpoints. Endpoint occupancy is the
        // caller's own responsibility (a goal legitimately placed within a radius of a wall must stay
        // reachable), and a constant-width corridor whose endpoints already `footprint_clear` at
        // `radius` has that same clearance at every interior sample. Only a wall that genuinely
        // comes within `radius` of the swept centreline BETWEEN the endpoints is caught.
        //
        // The ring sampling only ADDS rejections, so it never turns a blocked segment clear. Spacing
        // r/2 mirrors the feeler lattice.
        //
        // A spoke hit at exactly the ring radius is TANGENT (the disc touches, does not overlap) and is
        // treated as CLEAR, the same boundary the feeler sweep uses when running parallel to a wall at
        // exactly `radius`. Only a wall STRICTLY within radius (hit fraction < 1 - eps) blocks.
        const RING_DIRS: usize = 8;
        const RING_TANGENT_EPS: f32 = 1e-3;
        let step = (r * 0.5).max(0.5);
        let mut s = step;
        while s < hlen - 1e-3 {
            let t = s / hlen;
            let e = from[0] + (to[0] - from[0]) * t;
            let n = from[1] + (to[1] - from[1]) * t;
            let z = from[2] + (to[2] - from[2]) * t;
            if let Some(hit) = self.ring_nearest_hit(e, n, z, r, RING_DIRS) {
                if hit < 1.0 - RING_TANGENT_EPS {
                    return false;
                }
            }
            s += step;
        }
        true
    }

    /// Returns the controller's own "can a body be placed here" predicate for the feet position
    /// `p`, as the named halves of a [`crate::diagnostics::Placement`]. The controller reads
    /// this one predicate.
    ///
    /// The disjunction is the controller's. [`Collision::footprint_clear`]'s ring (cast at
    /// `p.z + PLAYER_BODY.ring`) is pierced, **or** there is no floor within [`GROUND_DEPTH`] below
    /// the feet (the body has fallen out of the world, a column with zero triangles over it).
    ///
    /// Naming both halves means both are evaluated, where a `||` would skip the ground probe when
    /// the footprint was already pierced.
    pub fn body_placement(&self, p: [f32; 3]) -> crate::diagnostics::Placement {
        use crate::diagnostics::Placement;
        let pierced = !self.footprint_clear(
            p[0], p[1], p[2], eqoxide_core::physics::PLAYER_RADIUS, PLACEMENT_RING_DIRS);
        let no_floor = self.ground_below(p[0], p[1], p[2] + GROUND_ORIGIN, GROUND_DEPTH).is_none();
        match (pierced, no_floor) {
            (false, false) => Placement::Placeable,
            (true, false)  => Placement::FootprintPierced,
            (false, true)  => Placement::NoFloorBelow,
            (true, true)   => Placement::FootprintPiercedAndNoFloorBelow,
        }
    }

    /// Returns the raw zone-line-region cache, keyed by `(zone_point_index, point)`.
    pub fn zone_line_regions(&self) -> &[(i32, [f32; 3])] {
        &self.zone_line_regions
    }

    /// Returns the private `z_min` field for test-only zone-corpus probes in dependent crates.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn z_min(&self) -> f32 {
        self.z_min
    }
}

#[cfg(test)]
mod b2_glb_tests {
    use super::*;

    use eqoxide_assets::{ZoneAssets, COLLISION_MESH_TAG};

    /// End-to-end: a zone GLB baked with the Component-B pipeline (containing a `__collision__`
    /// mesh) must be ingested so `Collision::build` reports collision-mesh provenance, the
    /// collision mesh is NOT in the render terrain (texture-linked) set, and the grid is
    /// non-empty. Point `ZONE_GLB` at e.g. /tmp/eqoxide_test_gfaydark.glb (the asset-server
    /// `baked_zone_has_collision_mesh_with_invisible_faces` test writes one).
    #[test]
    #[ignore = "requires a baked zone glb (with __collision__) at $ZONE_GLB"]
    fn from_glb_ingests_collision_mesh() {
        let p = std::env::var("ZONE_GLB").expect("set ZONE_GLB to a baked zone glb");
        let za = ZoneAssets::from_glb(std::path::Path::new(&p)).unwrap();
        // The collision mesh is tagged and carried in `terrain` (so the renderer can skip it),
        // but it is never uploaded for drawing.
        let tagged = za.terrain.iter()
            .filter(|m| m.texture_name.as_deref() == Some(COLLISION_MESH_TAG))
            .count();
        assert_eq!(tagged, 1, "exactly one __collision__ mesh expected in the baked zone");
        let col = Collision::build(&za, 32.0);
        assert!(col.from_collision_mesh, "Collision::build must use the __collision__ mesh");
        // Sanity: the floor under a known walkable point resolves to real geometry, and the
        // grid has triangles to query.
        assert!(col.floor_z(0.0, 0.0, 9999.0) < 9999.0 || za.terrain.len() > 1,
            "collision grid should contain queryable geometry");
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};

    /// #522 diagnostic: dump every collision surface in the vertical columns at the two live
    /// measurement points on the Kelethin plank (Acceptancetest at (-126.375,-15.875) holding
    /// z=73.97; native Katie at (-138.5,-17.5) reporting z=77.0). Decides fork (a) controller
    /// floor-selection bug vs (b) collision-model error.
    /// Run: ZONE_GLB=~/.local/share/eqoxide/assets/models/gfaydark.glb \
    ///      cargo test -p eqoxide-zone-geometry --lib diagnose_522_kelethin_plank_columns -- --ignored --nocapture
    #[test]
    #[ignore = "requires the cached gfaydark glb at $ZONE_GLB"]
    fn diagnose_522_kelethin_plank_columns() {
        let p = std::env::var("ZONE_GLB").expect("set ZONE_GLB to the cached gfaydark glb");
        let za = ZoneAssets::from_glb(std::path::Path::new(&p)).unwrap();
        let col = Collision::build(&za, 32.0);
        eprintln!("collision: from_collision_mesh={} grid {}x{} cell={}",
            col.from_collision_mesh, col.cols, col.rows, col.cell_size);
        for (label, e, n) in [
            ("Acceptancetest (-126.375,-15.875)", -126.375f32, -15.875f32),
            ("Katie          (-138.5,-17.5)", -138.5f32, -17.5f32),
        ] {
            eprintln!("\n=== {label} ===");
            // ALL surfaces (facing-blind, unfiltered) in a wide band around the plank.
            let all = col.column_surfaces(e, n, 80.0, 40.0, 120.0);
            eprintln!("  raw surfaces (z, nz): {:?}", all);
            // what the controller's grounded clamp sees from each hypothesis height
            for foot in [77.1f32, 77.0, 76.9, 74.0, 73.97] {
                let g = col.ground_below(e, n, foot + 1.0, 200.0);
                let nf = col.nearest_floor(e, n, foot, 3.0, 8.0);
                eprintln!("  foot={foot:>6.2}: ground_below(origin=foot+1)={g:?} nearest_floor(±3/8)={nf:?}");
            }
        }
    }

    /// #522 diagnostic 2: measure wire-z minus collision-floor for a batch of LIVE server
    /// entities (sampled from /v1/observe/entities in gfaydark, 2026-07-17). EQEmu FixZ places
    /// NPCs at floor + Mob::GetZOffset(); if the deltas cluster ~3.1, the wire z datum is
    /// floor+offset, not foot level.
    #[test]
    #[ignore = "requires the cached gfaydark glb at $ZONE_GLB"]
    fn diagnose_522_npc_wire_z_vs_collision_floor() {
        let p = std::env::var("ZONE_GLB").expect("set ZONE_GLB to the cached gfaydark glb");
        let za = ZoneAssets::from_glb(std::path::Path::new(&p)).unwrap();
        let col = Collision::build(&za, 32.0);
        let samples: &[(&str, f32, f32, f32)] = &[
            ("Katie(native PC)", -138.5, -17.5, 77.0),
            ("Merchant_Nildar", 205.0, 867.0, 77.0),
            ("Zelli_Starsfire", 245.0, -622.0, 77.0),
            ("Astar_Leafsinger", 294.0, -253.0, 77.0),
            ("Merchant_Tilluen", 271.0, -85.0, 77.0),
            ("Serilia_Whistlewind", 268.0, -276.0, 77.0),
            ("Merchant_Lanin", 329.0, 414.0, 77.0),
            ("Hendricks", -550.0, -537.0, 161.0),
            ("Geeda", -145.0, -566.0, 161.0),
            ("Devin_Ashwood", 395.0, -1.25, 161.0),
            ("Laren", -333.0, -362.0, 161.0),
            ("Guard_Treestrider", -528.5, -408.125, 161.0),
            ("Captain_Silverwind", 63.875, -326.125, 118.125),
            ("Cerila_Windrider", 477.0, -430.0, 117.5),
            ("Ran_Sunfire", 496.0, -461.0, 114.875),
            ("a_black_wolf046", 1460.875, -1403.625, -13.375),
            ("a_black_wolf050", 1300.125, -661.375, -4.75),
            ("a_giant_wasp_worker037", 246.5, -891.875, 29.625),
            ("a_decaying_skeleton007", -613.25, 567.375, -37.125),
            ("Guard_Sunblaze", -117.5, 776.375, -5.25),
            ("orc_centurion065", 1575.25, 1706.375, 36.625),
        ];
        for &(name, x, y, z) in samples {
            let nf = col.nearest_floor(x, y, z, 10.0, 20.0);
            let delta = nf.map(|f| z - f);
            eprintln!("{name:>24} wire_z={z:>8.3} floor={nf:?} wire-floor={delta:?}");
        }
    }

    /// A single horizontal floor quad + one vertical wall: the floor raycast must
    /// return the floor height (not the wall), and a ray crossing the wall must hit.
    #[test]
    fn collision_grid_floor_and_occlusion() {
        // Floor quad at z=0 spanning east/north [0,10]; EQ WLD pos = [east, height, north].
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 0.0, 10.0], [0.0, 0.0, 10.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0, 0.0, 0.0],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // Vertical wall at world east=5: EQ p2=5 (render.X), spanning north=p0 [0,10], height=p1 [0,10].
        let wall = MeshData {
            positions: vec![[0.0, 0.0, 5.0], [10.0, 0.0, 5.0], [10.0, 10.0, 5.0], [0.0, 10.0, 5.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0, 0.0, 0.0],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let assets = ZoneAssets { terrain: vec![floor, wall], objects: vec![], textures: vec![] };
        let col = Collision::build(&assets, 4.0);

        // Floor sampled under (3,3) is z=0, never the wall's height.
        let h = col.floor_z(3.0, 3.0, 20.0);
        assert!((h - 0.0).abs() < 1e-3, "expected floor z=0, got {h}");

        // Segment from east=2 to east=8 at height 5 crosses the wall at east=5 → blocked.
        assert!(col.segment_blocked([2.0, 3.0, 5.0], [8.0, 3.0, 5.0]),
            "wall between endpoints should block the segment");
        // Segment entirely on one side of the wall (east 1→4) is clear.
        assert!(!col.segment_blocked([1.0, 3.0, 5.0], [4.0, 3.0, 5.0]),
            "segment not reaching the wall should be clear");

        // Empty collision returns the fallback and never blocks.
        let empty = Collision::build(&ZoneAssets { terrain: vec![], objects: vec![], textures: vec![] }, 8.0);
        assert_eq!(empty.floor_z(0.0, 0.0, -99.0), -99.0);
        assert!(!empty.segment_blocked([0.0, 0.0, 0.0], [10.0, 0.0, 0.0]));
        assert!(empty.path_clear([0.0, 0.0, 0.0], [10.0, 0.0, 0.0], 2.0),
            "no geometry should never block movement");
    }

    // ── z-anchor regression tests ────────────────────────────────────────────────────────────────
    // A quad in EQ WLD space (pos = [north, height, east]). `up` picks the winding, and therefore
    // the face normal: an up-facing FLOOR you can stand on, or a down-facing CEILING you cannot.
    fn slab(z: f32, n0: f32, n1: f32, e0: f32, e1: f32, up: bool) -> MeshData {
        MeshData {
            positions: vec![[n0, z, e0], [n0, z, e1], [n1, z, e1], [n1, z, e0]],
            normals: vec![], uvs: vec![],
            indices: if up { vec![0, 1, 2, 0, 2, 3] } else { vec![0, 2, 1, 0, 3, 2] },
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        }
    }

    // ── #855: the ONE ray-hit acceptance window ──────────────────────────────────────────────────
    /// A single up-facing floor quad at z = 0, 100 x 100 around the origin.
    fn one_floor() -> Collision {
        Collision::build(&ZoneAssets {
            terrain: vec![slab(0.0, -50.0, 50.0, -50.0, 50.0, true)],
            objects: vec![], textures: vec![],
        }, 32.0)
    }

    /// A floor **tilted in both horizontal axes**, centred at `(e0, n0, z)` and 100 u across.
    /// The tilt matters: on a tilted plane the z a floor query returns for an arbitrary column is
    /// an *interpolated* `f32`, which is the input that makes #855 reproducible (see
    /// `a_floor_z_the_module_just_reported_always_has_a_floor_under_it`). A flat slab returns the
    /// mesh's own exact z and cannot show the defect at any coordinate.
    fn tilted_floor(e0: f32, n0: f32, z: f32) -> Collision {
        tilted_floor_sized(e0, n0, z, 50.0)
    }

    /// [`tilted_floor`] with the quad's half-extent as a parameter. `h` is the knob that separates
    /// "how big are the ray's coordinates" from "how big are the TRIANGLE's" — see
    /// `a_ray_near_the_origin_still_finds_a_floor_whose_vertices_are_far_away`.
    fn tilted_floor_sized(e0: f32, n0: f32, z: f32, h: f32) -> Collision {
        let tilt = 0.3f32;
        // EQ WLD space: pos = [north, height, east].
        let p = |dn: f32, de: f32| [n0 + dn, z + tilt * dn + 0.7 * tilt * de, e0 + de];
        Collision::build(&ZoneAssets {
            terrain: vec![MeshData {
                positions: vec![p(-h, -h), p(-h, h), p(h, h), p(h, -h)],
                normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
                texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
                render_mode: RenderMode::Opaque, anim: None,
            }],
            objects: vec![], textures: vec![],
        }, 32.0)
    }

    /// The three coordinate regimes these fixtures are measured at: the world origin, `tox`'s real
    /// submerged floor, and the largest coordinate in the baked corpus (`everfrost`).
    const REGIMES: [(&str, f32, f32, f32); 3] = [
        ("origin", 0.0, 0.0, 0.0),
        ("tox coords", 2081.4, 2320.5, -86.75),
        ("everfrost coords", -4467.2, -1295.9, -249.78),
    ];

    /// **THE #855 REGRESSION, stated as the disagreement it was.** `nearest_hit_t` and the column
    /// scan look at the same triangles down the same vertical line; before #855 they answered
    /// differently about a face flush with the ray origin, because the ray scan's window started at
    /// `t > 1e-3` and the column scan's at `t >= 0`.
    ///
    /// MUTATION-CHECK (run both directions, and the failure MESSAGE observed, not predicted):
    /// restoring `t > 1e-3` in `nearest_hit_t` turns this RED at the first `eps = 0` row it reaches
    /// — measured `len=0.056 eps=0: column=[(0.0, 1.0), (0.0, 1.0)] ray=None`. Restoring the epsilon
    /// in `nearest_hit` instead leaves it GREEN (this test reaches only the `_t` scan) — which is
    /// why `nearest_hit` carries its own pin in
    /// `the_normal_returning_scan_has_the_same_blind_band_free_contact_as_the_t_scan`.
    #[test]
    fn the_ray_scan_and_the_column_scan_agree_about_a_face_flush_with_the_origin() {
        let c = one_floor();
        for &len in &[0.056f32, 0.56, 2.5, 100.0] {
            for &eps in &[0.0f32, 1e-6, 1e-4, 5e-4, 1e-3, 2.4e-3, 0.05] {
                if eps >= len { continue; } // the face is genuinely past the end of the ray
                let column = c.column_surfaces(0.0, 0.0, eps, 0.0, len);
                let ray = c.nearest_hit_t([0.0, 0.0, eps], [0.0, 0.0, eps - len]);
                assert_eq!(!column.is_empty(), ray.is_some(),
                    "len={len} eps={eps}: column={column:?} ray={ray:?} — the two scans must not \
                     disagree about whether the z=0 floor is on this segment");
                assert!(ray.is_some(), "len={len} eps={eps}: the floor IS on this segment");
            }
        }
    }

    /// **THE #855 DEFECT, REPRODUCED SYNTHETICALLY (round 2, review finding 1).** Round 1's
    /// fixtures all sat at the world origin over a flat slab, and review was right that they could
    /// not see the residual. Review's proposed cause was `f32` cancellation scaling with the world
    /// coordinate, and its proposed fixture was `one_floor()` relocated to `(2000, 2300, −87)`.
    ///
    /// **MEASURED: that fixture does not reproduce it.** An axis-aligned synthetic quad at `tox`'s
    /// coordinates measures a blind band of exactly `0` even with the world-unit slack removed
    /// (a *tilted* quad at the same coordinates does not — see the WRAP row of the mutation-check
    /// table below). Coordinate magnitude is not the driver.
    ///
    /// **What actually reproduces it**, and the reason it is a real production state: a floor query
    /// does not hand back a coordinate from the mesh, it hands back a *reconstructed* one —
    /// `gather_top + t·dir_z` in `column_hits`. On a tilted face that `f32` lands an ULP or two
    /// either side of the true plane. When it lands BELOW, a ray starting there begins inside the
    /// solid, Möller–Trumbore reports the face at a small negative `t`, and a lower bound of exactly
    /// `0` discards it — "no floor below me", to a body standing on that floor. `movement.rs`
    /// assigns `self.pos[2]` straight from a floor query at the ground-snap and depenetration arms,
    /// so this is reached without any synthetic placement.
    ///
    /// This test walks 1369 columns of a tilted floor at each of three coordinate regimes, asks the
    /// module for the floor, and then asks the module whether that floor is there.
    ///
    /// MUTATION-CHECK, RUN in both directions (counts are `miss/1369`):
    ///
    /// | mutation | origin | `tox` coords | `everfrost` coords |
    /// |---|---|---|---|
    /// | `hit_accepted`'s lower bound WRAPPED to `t >= 0.0` | **958**, band 1.419e-5 | **882**, band 1.907e-5 | **623**, band 2.289e-5 |
    /// | shipped | 0 | 0 | 0 |
    ///
    /// That row is a WRAP, not a deletion (#799): `hit_accepted` is still called and `contact_tol`
    /// is still computed; only the world-unit slack stops being applied. It does not depend on
    /// `CONTACT_TOL_ULPS` — the mutation removes the term the constant feeds.
    ///
    /// **What this test does NOT pin.** At the shipped `CONTACT_TOL_ULPS = 32` these fixtures are
    /// insensitive to whether the tolerance is scaled on the ray alone or on the triangle too — at
    /// 100 u across, both are the same order. The test that separates them is
    /// `a_ray_near_the_origin_still_finds_a_floor_whose_vertices_are_far_away`;
    /// measured, it is the only one in the suite that does.
    #[test]
    fn a_floor_z_the_module_just_reported_always_has_a_floor_under_it() {
        for (label, e0, n0, z) in REGIMES {
            let c = tilted_floor(e0, n0, z);
            let (mut miss, mut total, mut worst, mut at) = (0usize, 0usize, 0.0f32, [0.0f32; 3]);
            for i in 0..37 {
                for j in 0..37 {
                    // Deliberately irrational-ish strides: never a vertex, never the quad centre.
                    let e = e0 - 40.0 + (i as f32) * 2.1731;
                    let n = n0 - 40.0 + (j as f32) * 2.0917;
                    let Some(fz) = c.ground_below(e, n, z + 60.0, 400.0) else { continue };
                    total += 1;
                    if c.nearest_hit_t([e, n, fz], [e, n, fz - 1.75]).is_some() { continue }
                    miss += 1;
                    let (mut lo, mut hi) = (0.0f32, 0.5f32);
                    for _ in 0..60 {
                        let m = 0.5 * (lo + hi);
                        if c.nearest_hit_t([e, n, fz + m], [e, n, fz + m - 1.75]).is_some() { hi = m } else { lo = m }
                    }
                    if hi > worst { worst = hi; at = [e, n, fz]; }
                }
            }
            assert!(total > 1000, "{label}: positive control — only {total} columns found a floor");
            assert_eq!(miss, 0,
                "{label}: {miss}/{total} columns report NO floor below the floor the module just \
                 returned for them; worst blind band {worst:.4e} world units at {at:?}. A body \
                 grounded there and given a down-wish descends through the world (#855)");
        }

        // #866 round-2 review (PR comment 5201986822, §5): `tol_cliff` — CONTACT_TOL_ULPS 32 → 6,
        // a value MEASURED to let real bodies through real floors — is GREEN on both unit suites
        // above. It is caught only by the `#[ignore]`d, asset-gated real-zone corpus, which a
        // default `cargo test` never runs. This case is a CI-runnable pin for that gap: a smaller
        // quad (`h = 10` vs. the REGIMES fixtures' `h = 50`) sampled over a narrower ±8 u span,
        // which the reviewer measured requires ~12.6 ULPs of tolerance — against ~5.3 ULPs for the
        // REGIMES fixtures above, which is why those stay green at ULPS as low as 6 and this one
        // does not.
        //
        // MUTATION-CHECK, RUN (this exact fixture, not the reviewer's — strides differ so the
        // miss count does too, but the direction is what the pin depends on):
        // CONTACT_TOL_ULPS = 6.0 → RED, 16/1369 misses, worst band 3.1069e-6.
        // CONTACT_TOL_ULPS = 12.0 → GREEN. Restored to the shipped 32.0 → GREEN. (The reviewer's
        // own run, on their fixture, measured 4/1369 at ULPS=6 — see PR comment 5201986822 §5;
        // both runs agree RED at 6, GREEN at 12 and 32, which is the property this pin needs.)
        //
        // **What this does NOT establish.** Its cliff sits somewhere between 6 and 12 ULPs, not at
        // the corpus's measured cliff of 7 (see `CONTACT_TOL_ULPS`'s doc) — it is a different
        // fixture, not a reproduction of that number. So this pins that the shipped constant is
        // not order-of-magnitude wrong (~2.7x margin over this fixture's cliff, vs. the corpus's
        // ~4.6x), not that the blind band is closed: §4.1 of the round-2 review measured 108/599875
        // residual misses on real baked geometry near the world origin at the shipped constant, and
        // that residual is unrelated to this pin (tracked as a follow-up issue referenced from
        // `CONTACT_TOL_ULPS`'s doc).
        {
            let c = tilted_floor_sized(0.0, 0.0, 0.0, 10.0);
            let (mut miss, mut total, mut worst, mut at) = (0usize, 0usize, 0.0f32, [0.0f32; 3]);
            for i in 0..37 {
                for j in 0..37 {
                    // Same irrational-ish strides as the REGIMES loop above, scaled down 5x to
                    // span roughly ±8 u instead of ±40 u.
                    let e = -8.0 + (i as f32) * 0.43462;
                    let n = -8.0 + (j as f32) * 0.41834;
                    let Some(fz) = c.ground_below(e, n, 60.0, 400.0) else { continue };
                    total += 1;
                    if c.nearest_hit_t([e, n, fz], [e, n, fz - 1.75]).is_some() { continue }
                    miss += 1;
                    let (mut lo, mut hi) = (0.0f32, 0.5f32);
                    for _ in 0..60 {
                        let m = 0.5 * (lo + hi);
                        if c.nearest_hit_t([e, n, fz + m], [e, n, fz + m - 1.75]).is_some() { hi = m } else { lo = m }
                    }
                    if hi > worst { worst = hi; at = [e, n, fz]; }
                }
            }
            assert!(total > 1000, "origin, h=10, ±8u span: positive control — only {total} columns found a floor");
            assert_eq!(miss, 0,
                "origin, h=10, ±8u span: {miss}/{total} columns report NO floor below the floor \
                 the module just reported for them; worst blind band {worst:.4e} world units at \
                 {at:?}. This is the tol_cliff pin (#866 round-2 review §5) — if this goes red, \
                 CONTACT_TOL_ULPS has been lowered past the ~12 ULP cliff this fixture measures");
        }
    }

    /// **THE TRIANGLE'S OWN SCALE, PINNED (#855 round 2).** [`contact_tol`] is fed the largest
    /// coordinate in the *whole* ray/triangle test, not just the ray's. That extra `.max()` over
    /// `v0/v1/v2` is a real behavioural clause and it needs a test that fails without it, or it is
    /// exactly the unpinned-but-described-as-load-bearing edit round-1 review blocked on.
    ///
    /// The fixture separates the two magnitudes on purpose: an 8000 u quad, sampled within ±40 u of
    /// the world origin. The ray's coordinates are all under 40; the vertices deciding its `t` are
    /// 4000 out, and it is those that set how coarsely the floor z can be reconstructed. Scaling the
    /// tolerance on the ray alone gives ~1e-5 u of slack where ~1e-2 is needed.
    ///
    /// This is not a contrived shape. Baked zone terrain routinely carries single triangles hundreds
    /// of units across — a lake bed, a plain, a cavern floor — and the character standing on one is
    /// at ordinary local coordinates.
    ///
    /// MUTATION-CHECK, RUN at the shipped `CONTACT_TOL_ULPS = 32`: replacing the per-triangle scale
    /// with `ray_scale` in all three scans (call sites otherwise untouched — a REACH mutation, #799)
    /// → **RED here** (237 of 1369 columns miss, widest surviving band 4.2737e-4), and GREEN across
    /// the rest of the zone-geometry lib tests, and across real-geometry corpora.
    ///
    /// Real zones do NOT catch a ray-only tolerance. Baked terrain in those zones is triangulated
    /// finely enough that the ray's own coordinates already bound the triangle's, so the two scales
    /// agree there. This fixture alone pins the per-triangle scale, and no corpus run notices if
    /// this test is deleted.
    #[test]
    fn a_ray_near_the_origin_still_finds_a_floor_whose_vertices_are_far_away() {
        let c = tilted_floor_sized(0.0, 0.0, 0.0, 4000.0);
        let (mut miss, mut total, mut worst, mut at) = (0usize, 0usize, 0.0f32, [0.0f32; 3]);
        for i in 0..37 {
            for j in 0..37 {
                let e = -40.0 + (i as f32) * 2.1731;
                let n = -40.0 + (j as f32) * 2.0917;
                let Some(fz) = c.ground_below(e, n, 2000.0, 4000.0) else { continue };
                total += 1;
                assert!(axis_scale([e, n, fz]) < 100.0,
                    "fixture control: the RAY's coordinates must stay small ({e}, {n}, {fz})");
                if c.nearest_hit_t([e, n, fz], [e, n, fz - 1.75]).is_some() { continue }
                miss += 1;
                let (mut lo, mut hi) = (0.0f32, 0.5f32);
                for _ in 0..60 {
                    let m = 0.5 * (lo + hi);
                    if c.nearest_hit_t([e, n, fz + m], [e, n, fz + m - 1.75]).is_some() { hi = m } else { lo = m }
                }
                if hi > worst { worst = hi; at = [e, n, fz]; }
            }
        }
        assert!(total > 1000, "positive control — only {total} columns found a floor");
        assert_eq!(miss, 0,
            "{miss}/{total} columns near the origin report NO floor below the floor the module just \
             returned, because the triangle deciding it is 4000 u across; worst blind band \
             {worst:.4e} world units at {at:?} (#855)");
    }

    /// **`nearest_hit`'s OWN pin (#855 round 2, review finding 2).** Round 1 edited the acceptance
    /// test in both scans but pinned only `nearest_hit_t` at the primitive; `nearest_hit`'s edit was
    /// defended solely through a controller test that also carried a second guard, so review could
    /// revert `nearest_hit`'s line with the entire suite still green. `nearest_hit` is the scan
    /// `CharacterController::slide`, `swim_rise` and `swim_sink` all call — the one that matters
    /// most — so it gets the same measurement directly.
    ///
    /// MUTATION-CHECK, RUN: restoring `t > 1e-3 && t <= 1.0` in `Collision::nearest_hit` and
    /// changing nothing else → **RED** here. That is the control finding 2 said was missing.
    #[test]
    fn the_normal_returning_scan_has_the_same_blind_band_free_contact_as_the_t_scan() {
        for (label, e0, n0, z) in REGIMES {
            let c = tilted_floor(e0, n0, z);
            for i in 0..11 {
                for j in 0..11 {
                    let e = e0 - 20.0 + (i as f32) * 3.7131;
                    let n = n0 - 20.0 + (j as f32) * 3.5917;
                    let Some(fz) = c.ground_below(e, n, z + 60.0, 400.0) else { continue };
                    let (from, to) = ([e, n, fz], [e, n, fz - 1.75]);
                    let (t, nrm) = c.nearest_hit(from, to).unwrap_or_else(|| panic!(
                        "{label} at {from:?}: the floor this module just reported must be a \
                         contact for the scan `slide`/`swim_sink` call, not open space"));
                    // The contact must be AT the ray origin for every practical purpose. The
                    // reconstructed floor z lands an ULP either side of the true plane, so the
                    // reported distance is a few ULPs of the FIXTURE's scale, not of `from` —
                    // measured worst 4.395e-6 over these 363 columns. Bounded here at 5e-4: two
                    // orders under `SKIN` (0.05), the smallest back-off any caller applies.
                    assert!(t * 1.75 <= 5e-4,
                        "{label}: contact reported {:.4e} world units along the ray, not at its origin", t * 1.75);
                    assert!(nrm[2] > 0.5, "{label}: the floor normal must face back up the ray, got {nrm:?}");
                    assert!(c.nearest_hit_t(from, to).is_some(),
                        "{label} at {from:?}: `nearest_hit` and `nearest_hit_t` disagree about the same face");
                }
            }
        }
    }

    /// **THE THREE SCANS, ON ONE SEGMENT (#855 round 2, review finding 3).** Unifying the `t` test
    /// did not by itself stop the module disagreeing with itself: three *length* guards survived
    /// round 1 and differed — `nearest_hit_t` rejected `|dir|² < 1e-6` (`|dir| < 1e-3`),
    /// `nearest_hit` rejected `|dir|² < 1e-9` (`|dir| < 3.16e-5`), `column_hits` rejected
    /// `|dir_z| < 1e-6`. Measured consequence, this fixture, face squarely at the midpoint: at
    /// segment lengths `1e-4`, `5e-4`, `9.9e-4`, `nearest_hit` said `Some(0.5)` and `nearest_hit_t`
    /// said `None`. They now share `MIN_RAY_LEN`.
    ///
    /// **Scope, stated so this is not read as more than it is.** This pins that the three scans
    /// agree about *whether a face is on a segment*. They are still not interchangeable: only
    /// `column_hits` classifies standability, and each visits a different set of broad-phase cells.
    ///
    /// MUTATION-CHECK, RUN — all THREE guards, separately, each restored alone, and the message
    /// below is the one the run printed rather than a paraphrase. Each mutation makes exactly its
    /// own scan the odd one out, which is what makes this a per-scan pin and not a single tripwire:
    ///   * `nearest_hit`'s `|dir|² < 1e-9` → **RED**,
    ///     `len=1e-5: nearest_hit_t=true nearest_hit=false column=true`
    ///   * `nearest_hit_t`'s `|dir|² < 1e-6` → **RED**,
    ///     `len=1e-5: nearest_hit_t=false nearest_hit=true column=true`
    ///   * `column_hits`' `1e-3` → **RED**,
    ///     `len=1e-5: nearest_hit_t=true nearest_hit=true column=false`
    ///
    /// An earlier version of this block said "restoring EITHER squared-length guard" and quoted
    /// `len=1e-4: nearest_hit_t=None nearest_hit=Some(0.5) column=1`. Both halves were wrong: there
    /// are three guards, not two, and this assertion formats `bool`s, so it cannot ever have printed
    /// `None`/`Some(0.5)`. The quote was not a measurement. It is recorded here rather than silently
    /// replaced, because a fabricated-looking quote that reads as evidence is the defect class this
    /// project keeps measuring.
    #[test]
    fn the_three_scans_agree_on_short_segments() {
        let c = one_floor();
        for &len in &[1e-5f32, 1e-4, 5e-4, 9.9e-4, 3.16e-5, 1e-3, 0.01, 1.0] {
            let half = 0.5 * len;
            let (from, to) = ([0.0, 0.0, half], [0.0, 0.0, -half]);
            let a = c.nearest_hit_t(from, to).is_some();
            let b = c.nearest_hit(from, to).is_some();
            let d = !c.column_surfaces(0.0, 0.0, half, 0.0, len).is_empty();
            assert!(a == b && b == d,
                "len={len:e}: nearest_hit_t={a} nearest_hit={b} column={d} — one module, one answer");
            assert!(a, "len={len:e}: the floor is squarely at the midpoint of this segment");
        }
    }

    /// **The residual `hit_accepted` deliberately keeps, and why it is safe (#855 round 2).** The
    /// lower bound carries world-unit slack; the UPPER bound does not, so a face lying just past
    /// `to` is rejected. This measures that it is self-correcting rather than a second #855: a body
    /// that steps exactly onto such a face arrives flush with it, where the lower bound sees it.
    ///
    /// MUTATION-CHECK, RUN: widening `hit_accepted`'s upper bound to `t <= 1.0 + tol/len` turns the
    /// first assertion RED, confirming it pins the bound rather than describing it.
    #[test]
    fn a_face_just_past_the_far_end_is_caught_on_the_next_frame() {
        let (_, e0, n0, z) = REGIMES[1]; // tox coords
        let c = tilted_floor(e0, n0, z);
        let (e, n) = (e0 + 3.71, n0 - 2.13);
        let fz = c.ground_below(e, n, z + 60.0, 400.0).expect("positive control: a floor here");
        let tol = crate::collision::contact_tol(crate::collision::axis_scale([e, n, fz]));
        // A ray that stops a hair SHORT of the floor does not see it …
        let short = tol * 0.5;
        assert_eq!(c.nearest_hit_t([e, n, fz + 1.0], [e, n, fz + short]), None,
            "the upper bound is exact: a face beyond `to` is not on this segment");
        // … and the body that lands at `to` is then flush with it, where contact IS reported.
        assert!(c.nearest_hit_t([e, n, fz + short], [e, n, fz + short - 1.0]).is_some(),
            "the very next frame's ray starts on the face and must report contact — this is why \
             the unslacked upper bound is a bounded residual and not a second blind band");
    }

    /// **The load-bearing premise of deleting the epsilon, and the half of it that is NOT true.**
    /// The old `t > 1e-3` looked like a guard for a ray starting ON a face. Only the COPLANAR
    /// subcase is actually rejected by an earlier test (`det.abs() < eps`, the parallel test). Every
    /// other direction out of an on-face origin returns a contact at `t ≈ 0`, deliberately — round-1
    /// review measured this over straight-up, 45°, 1e-3 and 1e-6 tilts, edge and vertex origins, and
    /// a horizontal ray on a sloped face: all `Some`. The origin set is measure-zero; the direction
    /// set is not. So this is "one subcase rejected, the rest accepted as contact", not "handled".
    ///
    /// A face genuinely BEHIND the origin — further than `contact_tol` — is still rejected; the
    /// lower bound is a real bound, not an "accept everything". MUTATION-CHECK: raising
    /// `CONTACT_TOL_ULPS` to `1e7` turns the last assertion RED.
    ///
    /// MEASURED CAVEAT, recorded because it is a real semantic and not an oversight: a face at
    /// distance ~zero is reported for a ray heading either way off it. Möller–Trumbore yields
    /// `t = -0.0` for the away direction and `-0.0 == 0.0` in IEEE, and the world-unit slack widens
    /// that tie from a point to `contact_tol`. No lower bound of any kind separates the two sides at
    /// the point of contact. The tie resolves toward "you are touching it", the conservative
    /// direction for every caller here (an over-reported contact costs a frame of progress; an
    /// under-reported one is #855), and every caller's own back-off (`SKIN`, `Body::radius`) keeps
    /// its ray origin off the face.
    ///
    /// **Reachability, measured (round-1 review):** it can produce a false *blocked* —
    /// `line_clear` from a point exactly on the floor to a point 40 u away and 0.01 u ABOVE it
    /// returns blocked. Every production LOS/probe origin is raised 0.5–4.0 u and `line_clear` was
    /// measured `true` at those heights, so this is latent, not live.
    #[test]
    fn only_the_coplanar_case_is_rejected_by_the_parallel_test_the_rest_is_accepted_contact() {
        let c = one_floor();
        assert_eq!(c.nearest_hit_t([0.0, 0.0, 0.0], [10.0, 0.0, 0.0]), None,
            "COPLANAR: a ray sliding along the face it starts on is parallel to it — dropped by det");
        let t = c.nearest_hit_t([0.0, 0.0, 0.0], [0.0, 0.0, -2.5])
            .expect("a ray from the face heading INTO it is a real zero-distance contact");
        assert!(t.abs() < 1e-6, "contact must be reported at t=0, got {t}");
        // NOT handled, accepted: every non-coplanar direction off the face is a contact, including
        // the ones heading AWAY from it. Three of the directions round-1 review enumerated.
        for (label, to) in [("straight up", [0.0, 0.0, 5.0]),
                            ("up at 45°", [5.0, 0.0, 5.0]),
                            ("up, tilted 1e-6", [5.0, 0.0, 5e-6])] {
            assert!(c.nearest_hit_t([0.0, 0.0, 0.0], to).is_some_and(|t| t.abs() < 1e-6),
                "{label}: a flush face is a contact whichever way the ray leaves it (-0.0 == 0.0)");
        }
        // But a face at a genuinely negative distance — one unit BEHIND the origin, far beyond
        // `contact_tol` — is rejected.
        assert_eq!(c.nearest_hit_t([0.0, 0.0, 1.0], [0.0, 0.0, 6.0]), None,
            "the floor is 1 u behind an upward ray; it must not be reported");
    }

    /// The `line_clear` row of the #855 inventory: a line-of-sight query is normally issued FROM a
    /// body in contact with geometry, which is where a `t`-proportional band is largest in absolute
    /// terms. On a 100-unit ray the old band was ~0.1 u, so a wall you were standing 0.05 u from was
    /// invisible to LOS. MUTATION-CHECK: restoring `t > 1e-3` turns this RED.
    #[test]
    fn line_of_sight_does_not_see_through_a_wall_it_is_almost_touching() {
        // Vertical wall at east = 0 (a slab in the north/height plane), and a viewer 0.05 u west.
        let c = Collision::build(&ZoneAssets {
            terrain: vec![MeshData {
                positions: vec![[-50.0, -50.0, 0.0], [50.0, -50.0, 0.0], [50.0, 50.0, 0.0], [-50.0, 50.0, 0.0]],
                normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
                texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
                render_mode: RenderMode::Opaque, anim: None,
            }],
            objects: vec![], textures: vec![],
        }, 32.0);
        let from = [-0.05, 0.0, 0.0];
        let to = [99.95, 0.0, 0.0]; // 100 units east, straight through the wall
        assert!(c.nearest_hit_t(from, to).is_some(), "positive control: the wall is on this ray");
        assert!(!c.line_clear(from, to, 0.0),
            "LOS from 0.05 u away must be blocked by the wall, not fall inside a 0.1 u blind band");
    }

    // ─── Water-span grid ───────────────────────────────────────────────────────────────────────
    //
    // These assert the built grid against HAND-AUTHORED analytic fixture geometry (the pool/ceiling
    // bounds written in the test), NEVER against the span grid or `column_surfaces` under test — the
    // non-circular verification principle. Every expected number is computed from the
    // fixture constants + the shared `PLAYER_BODY` geometry.
    /// A walled pool: floor at -44, surface at -4, no submerged geometry. The one navigable span's
    /// feet-interval must be `[floor + ε, surface − float_depth]` = `[-43.95, -6.0]` (design §5.1).
    #[test]
    fn water_grid_pool_span_matches_hand_authored_geometry() {
        let body = crate::body::PLAYER_BODY;
        // Floor at -44 + two perimeter walls so the collision mesh z-extent spans the water band
        // [-44, -4] up to the surface (a flat floor alone has ZERO z-extent, which collapses the
        // water-leaf AABB's z-range — real zones always have vertical geometry around water). The
        // walls sit at east 0 / 64; the interior probe column (east 30) never crosses them.
        let assets = ZoneAssets {
            terrain: vec![
                slab(-44.0, 0.0, 64.0, 0.0, 64.0, true), // pool floor
                wall_east(0.0, -44.0, 0.0), wall_east(64.0, -44.0, 0.0), // z-extent walls
            ],
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 4.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::water_slab(-44.0, -4.0))));
        let grid = col.build_water_grid(&body);

        let c = grid.column_at(30.0, 30.0).expect("interior column is wet");
        assert_eq!(c.spans.len(), 1, "open pool = one span, got {:?}", c.spans);
        let (lo, hi) = c.spans[0];
        // Expected from fixture constants — NOT read from the grid.
        let want_lo = -44.0 + 0.05;              // floor + ε
        let want_hi = -4.0 - body.float_depth;   // surface − float_depth = -6.0
        assert!((lo - want_lo).abs() < 0.2, "nav_lo {lo} ≈ {want_lo}");
        assert!((hi - want_hi).abs() < 0.2, "nav_hi {hi} ≈ {want_hi}");
        assert!((c.surface_z - (-4.0)).abs() < 0.2, "surface {} ≈ -4", c.surface_z);
        assert_eq!(grid.unbounded_below_count(), 0, "bounded-below water fixture");
        // No water map ⇒ empty grid (no-regression premise for dry zones, design §10 P-3D-4).
        let dry = Collision::build(&assets, 4.0);
        assert_eq!(dry.build_water_grid(&body).wet_column_count(), 0);
    }

    /// A submerged ceiling at -10 across the pool carves the column into TWO spans: an upper band
    /// (feet just above the ceiling, up to the swim plane) and a lower band (bottom up to where the
    /// body-top just clears the ceiling = `ceiling − height − SKIN`). Mutation-checked on body height.
    #[test]
    fn water_grid_ceiling_carves_two_spans_and_tracks_body_height() {
        let mk = |body: &crate::body::Body| {
            let assets = ZoneAssets {
                terrain: vec![
                    slab(-44.0, 0.0, 64.0, 0.0, 64.0, true),   // pool floor
                    slab(-10.0, 0.0, 64.0, 0.0, 64.0, false),  // submerged ceiling (down-facing)
                    wall_east(0.0, -44.0, 0.0), wall_east(64.0, -44.0, 0.0), // z-extent walls (to surface)
                ],
                objects: vec![], textures: vec![],
            };
            let mut col = Collision::build(&assets, 4.0);
            col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::water_slab(-44.0, -4.0))));
            let grid = col.build_water_grid(body);
            grid.column_at(30.0, 30.0).cloned().expect("interior column is wet")
        };

        let body = crate::body::PLAYER_BODY;
        let c = mk(&body);
        assert_eq!(c.spans.len(), 2, "ceiling carves two spans, got {:?}", c.spans);
        // Spans are high-band first. Lower span's nav_hi = ceiling − height − SKIN (analytic).
        let lower = c.spans.iter().min_by(|a, b| a.1.total_cmp(&b.1)).unwrap();
        let want_lower_hi = -10.0 - body.height - 0.05; // = -16.05 for height 6
        assert!((lower.1 - want_lower_hi).abs() < 0.2, "lower nav_hi {} ≈ {want_lower_hi}", lower.1);

        // Mutation-check: a shorter body clears the ceiling with more room, so the lower span's
        // nav_hi RISES to `ceiling − height − SKIN` with the smaller height. Reads a DIFFERENT number.
        let mut short = crate::body::PLAYER_BODY;
        short.height = 4.0;
        let c2 = mk(&short);
        let lower2 = c2.spans.iter().min_by(|a, b| a.1.total_cmp(&b.1)).unwrap();
        let want2 = -10.0 - 4.0 - 0.05; // = -14.05
        assert!((lower2.1 - want2).abs() < 0.2, "shorter body lower nav_hi {} ≈ {want2}", lower2.1);
        assert!(lower2.1 > lower.1 + 1.0, "shorter body → higher navigable ceiling clearance");
    }

    /// #534 (agent-honesty): a THICK submerged solid — a slab spanning z −10 → −20 — with `.wtr`
    /// water painted straight through it (the water box does not cut holes around collision
    /// geometry, so `is_water` is true even inside the rock). The face-normal windings say the
    /// −10 → −20 gap is the INTERIOR of the solid, not water: NO navigable span may be emitted
    /// there (a node inside solid rock is exactly the falsehood the invariant forbids). The genuine
    /// water ABOVE the slab (−10 → surface) and BELOW it (−44 → −20) MUST still be navigable.
    ///
    /// Mutation check: revert the `nz` winding fix in `build_water_column` (discard `nz`, treat
    /// every inter-solid gap as water) and the false interior span (−19.95, −16.05) — feet inside
    /// the rock — reappears, tripping the "no span inside the solid" assertion below. The count
    /// also jumps 2 → 3. Both go RED, so the test cannot pass both ways.
    #[test]
    fn water_grid_thick_submerged_solid_emits_no_span_inside_the_rock() {
        let body = crate::body::PLAYER_BODY;
        // A solid slab from −20 (down-facing underside) to −10 (up-facing top), inside a −44…−4
        // water band. Same z-extent walls as the pool/ceiling fixtures so the water AABB reaches
        // the surface. The interior probe column (east 30) never crosses the walls.
        let assets = ZoneAssets {
            terrain: vec![
                slab(-44.0, 0.0, 64.0, 0.0, 64.0, true),   // pool floor (up-facing)
                slab(-10.0, 0.0, 64.0, 0.0, 64.0, true),   // TOP of the submerged solid (up-facing floor)
                slab(-20.0, 0.0, 64.0, 0.0, 64.0, false),  // UNDERSIDE of the submerged solid (down-facing)
                wall_east(0.0, -44.0, 0.0), wall_east(64.0, -44.0, 0.0), // z-extent walls (to surface)
            ],
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 4.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::water_slab(-44.0, -4.0))));
        let c = col.build_water_grid(&body).column_at(30.0, 30.0).cloned()
            .expect("the interior column is wet (water is painted through the slab)");

        // THE #534 INVARIANT: no navigable span may intersect the solid interior [−20, −10].
        for &(lo, hi) in &c.spans {
            assert!(hi <= -20.0 + 1e-3 || lo >= -10.0 - 1e-3,
                "span ({lo}, {hi}) intrudes into the solid interior [−20, −10] — a node inside rock (#534)");
        }
        // Water ABOVE the slab is navigable: feet just over the −10 top (−9.95) up to the swim
        // plane (surface − float_depth = −6). Analytic from fixture constants, NOT read back.
        let above = c.spans.iter().find(|&&(lo, _)| lo > -10.0 - 0.2)
            .expect("the water above the slab must be navigable");
        assert!((above.0 - (-9.95)).abs() < 0.2 && (above.1 - (-6.0)).abs() < 0.2,
            "above-slab span ≈ (−9.95, −6.0), got {above:?}");
        // Water BELOW the slab is navigable: feet from the pool floor (−43.95) up to where the
        // body top clears the −20 underside (ceiling − height − SKIN = −26.05). Mutation-sensitive
        // to body.height, mirroring the two-span ceiling test.
        let below = c.spans.iter().find(|&&(_, hi)| hi < -20.0)
            .expect("the water below the slab must be navigable");
        assert!((below.0 - (-43.95)).abs() < 0.2 && (below.1 - (-26.05)).abs() < 0.2,
            "below-slab span ≈ (−43.95, −26.05), got {below:?}");
        assert_eq!(c.spans.len(), 2, "exactly two spans (above + below the solid), got {:?}", c.spans);
    }

    /// #534 review, CASE A — a solid whose top is FLUSH WITH THE SURFACE (a rock breaking the
    /// waterline): slab top at −4 (== surface), underside at −20, in a −44…−4 water band. The
    /// `sz > wb+ε && sz < ws−ε` filter drops the up-facing top (it sits AT ws), so a naive opener
    /// sentinel would re-admit the −4…−20 interior as water. The winding of the flush surface must
    /// be folded into the top bound (up-facing ⇒ solid below ⇒ the surface is a CLOSER), so NO span
    /// is emitted inside the rock; only the water BELOW the slab (−20…−44) is navigable.
    ///
    /// Mutation check: drop the flush-surface clamp (top bound reverts to a plain opener) and the
    /// false span (−19.95, −6.0) — feet inside the rock, spanning from just below the surface down
    /// into the slab — reappears, tripping the interior assertion and the count (1 → 2). RED.
    #[test]
    fn water_grid_solid_flush_with_surface_emits_no_span_inside_it() {
        let body = crate::body::PLAYER_BODY;
        let assets = ZoneAssets {
            terrain: vec![
                slab(-44.0, 0.0, 64.0, 0.0, 64.0, true),   // pool floor (up-facing)
                slab(-4.0, 0.0, 64.0, 0.0, 64.0, true),    // solid TOP flush with the surface (up-facing)
                slab(-20.0, 0.0, 64.0, 0.0, 64.0, false),  // its underside (down-facing)
                wall_east(0.0, -44.0, 0.0), wall_east(64.0, -44.0, 0.0),
            ],
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 4.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::water_slab(-44.0, -4.0))));
        let c = col.build_water_grid(&body).column_at(30.0, 30.0).cloned()
            .expect("the interior column is wet");

        // No span may intersect the solid interior [−20, −4].
        for &(lo, hi) in &c.spans {
            assert!(hi <= -20.0 + 1e-3,
                "span ({lo}, {hi}) intrudes into the surface-flush solid [−20, −4] — a node inside rock (#534)");
        }
        // Only the water below the slab is navigable: floor (−43.95) up to body-top clearance of the
        // −20 underside (ceiling − height − SKIN = −26.05).
        let below = c.spans.iter().find(|&&(_, hi)| hi < -20.0)
            .expect("the water below the surface-flush solid must be navigable");
        assert!((below.0 - (-43.95)).abs() < 0.2 && (below.1 - (-26.05)).abs() < 0.2,
            "below-slab span ≈ (−43.95, −26.05), got {below:?}");
        assert_eq!(c.spans.len(), 1, "exactly one span (below the solid), got {:?}", c.spans);
    }

    /// #534 review, CASE B — a submerged MOUND resting on the water bottom (a lakebed lump): its
    /// up-facing top at −20, its underside FLUSH WITH THE BOTTOM at −44 (water bottoms commonly sit
    /// above the true floor, so a mound bottoms out ON the water floor — the COMMON case). The
    /// filter drops the down-facing underside (it sits AT wb), so a naive closer sentinel would
    /// re-admit the −20…−44 mound interior as water. The flush underside's winding must be folded
    /// into the bottom bound (down-facing ⇒ solid above ⇒ the bottom is an OPENER), so NO span is
    /// emitted inside the mound; only the water ABOVE it (−4…−20) is navigable.
    ///
    /// Mutation check: drop the flush-bottom clamp (bottom bound reverts to a plain closer) and the
    /// false span (−43.95, −26.05) — feet inside the mound — reappears, tripping the interior
    /// assertion and the count (1 → 2). RED.
    #[test]
    fn water_grid_mound_resting_on_the_bottom_emits_no_span_inside_it() {
        let body = crate::body::PLAYER_BODY;
        let assets = ZoneAssets {
            terrain: vec![
                slab(-20.0, 0.0, 64.0, 0.0, 64.0, true),   // mound TOP (up-facing floor)
                slab(-44.0, 0.0, 64.0, 0.0, 64.0, false),  // mound underside flush with the bottom (down-facing)
                wall_east(0.0, -44.0, 0.0), wall_east(64.0, -44.0, 0.0),
            ],
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 4.0);
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::water_slab(-44.0, -4.0))));
        let c = col.build_water_grid(&body).column_at(30.0, 30.0).cloned()
            .expect("the interior column is wet");

        // No span may intersect the mound interior [−44, −20].
        for &(lo, hi) in &c.spans {
            assert!(lo >= -20.0 - 1e-3,
                "span ({lo}, {hi}) intrudes into the mound interior [−44, −20] — a node inside rock (#534)");
        }
        // Only the water above the mound is navigable: feet just over the −20 top (−19.95) up to the
        // swim plane (surface − float_depth = −6).
        let above = c.spans.iter().find(|&&(lo, _)| lo > -20.0 - 0.2)
            .expect("the water above the mound must be navigable");
        assert!((above.0 - (-19.95)).abs() < 0.2 && (above.1 - (-6.0)).abs() < 0.2,
            "above-mound span ≈ (−19.95, −6.0), got {above:?}");
        assert_eq!(c.spans.len(), 1, "exactly one span (above the mound), got {:?}", c.spans);
    }

    // ─── RETRACTED at D-2 (#375): three old #329 guards whose premise qcat falsified ───
    //
    // `nearest_floor_never_returns_a_ceiling`, `fallback_never_admits_a_ceiling_whose_floor_is_below_
    // the_query_window`, and `fallback_admits_the_inverted_ground_but_not_the_ceiling_above_it` all
    // asserted that a DOWN-facing surface with OPEN space above it (a lone ceiling at -55.97 over a
    // floor 14u below; a roof at 391.8 over a floor 462u below) is a ceiling `nearest_floor` must never
    // return. The D-2 shape probe (`probe_qcat_column_vs_fixture`) MEASURED the character's walkable
    // qcat surface at -42.97 to be EXACTLY that shape — down-facing, open above, floor ~13u below — so
    // "down-facing + open above = ceiling" is false: it is walkable floor (the #375 fix). A facing-blind
    // classifier that rejected those synthetic ceilings would also delete qcat's walkway.
    //
    // The genuine #329 protection is preserved and re-tested by:
    //   * `close_roof_ceiling_is_rejected_by_headroom` — a ceiling with a roof CLOSE above (headroom <
    //     NAV_AGENT_HEIGHT) is rejected. That is what makes a ceiling a ceiling — a roof — not its
    //     winding. Mutation-checked.
    //   * `qcat_pocket_nearest_floor_is_never_the_ceiling` — the FAR qcat roof (391.8) is never returned
    //     at a REALISTIC ref_z (the -66 floor), because the `ref_z ± window` excludes it. (The old
    //     `fallback_never_admits…` queried AT roof height, ref_z=391.8 — a position a character is never
    //     in; the window defence is the real one.)
    /// Inverted GROUND: a column whose lowest (and here only) surface is down-facing, with nothing
    /// beneath it. Real zones bake standable ground this way — permafrost loses 131 such columns to
    /// the facing filter and neriakc 14, and deleting it leaves the column with no floor although it
    /// plainly has ground in it. The surface is ground precisely because nothing lies under it, and a
    /// ceiling always has a floor beneath. So the filter must never delete the ONLY ground in a
    /// column, and the rule is scoped to the COLUMN rather than to a whole-zone winding verdict
    /// (which votes on the bottom of each column and so cannot see inversion higher up).
    ///
    /// NOT the highpass shape, despite the resemblance: highpass's inverted band all has correctly
    /// wound ground BENEATH it, so it is a ceiling by this rule and stays deleted. This fallback does
    /// not fire there and does not recover highpass's loss — see the LIMIT note on `column_hits`.
    #[test]
    fn column_whose_only_surface_is_inverted_still_finds_a_floor() {
        let assets = ZoneAssets {
            terrain: vec![
                slab(0.0, 0.0, 256.0, 0.0, 256.0, true),      // large, correctly-wound floor
                slab(50.0, 0.0, 8.0, 300.0, 308.0, false),    // isolated inverted GROUND — nothing under it
            ],
            objects: vec![], textures: vec![],
        };
        let col = Collision::build(&assets, 8.0);

        // Query off the quad's diagonal split (300,0)-(308,8) so the ray crosses exactly one of its
        // two triangles, not the shared edge.
        // Facing-blind: the surface is there, and it is down-facing — mis-wound ground.
        let blind = col.column_surfaces(306.0, 2.0, 50.0, 20.0, 100.0);
        assert_eq!(blind.len(), 1, "exactly one surface lives in this column");
        assert!(blind[0].1 < 0.0, "and it is down-facing — the only ground here is inverted");

        // The filtered query must still find it: this column has nothing beneath it to make it a
        // ceiling, so deleting it leaves NO floor at all where there plainly is ground.
        let f = col.nearest_floor(306.0, 2.0, 50.0, 20.0, 100.0);
        assert!(f.is_some(), "the filter must never delete the only ground in a column");
        assert!((f.unwrap() - 50.0).abs() < 0.1, "expected the inverted surface's own height, got {f:?}");
    }

    /// #229: `find_zone_line_near` must hand back a point a character can STAND on — the region's
    /// own z is an interior point of the trigger volume, and walking to it was never possible.
    /// The projected point must still be inside the region, so the auto-cross still fires there.
    #[test]
    fn zone_line_target_is_projected_onto_the_floor_and_stays_inside_the_region() {
        let assets = ZoneAssets { terrain: vec![slab(0.0, 0.0, 64.0, 0.0, 64.0, true)], objects: vec![], textures: vec![] };
        let mut col = Collision::build(&assets, 8.0);
        // A zone-line region filling everything below z=50 — its representative point is up in the
        // volume, tens of units above the floor at 0 (exactly the shipped-asset shape).
        col.set_water(Some(std::sync::Arc::new(eqoxide_core::region_map::RegionMap::zone_line_below(50.0, 7))));

        let (idx, p) = col.find_zone_line_near(Some(7), [8.0, 8.0, 0.0]).expect("the zone line is found");
        assert_eq!(idx, 7);
        assert!(p[2].abs() < 0.01, "the target must sit on the floor (z=0), not up in the volume: got {}", p[2]);
        assert_eq!(col.zone_line_at([p[0], p[1], p[2] + 1.0]), Some(7),
            "standing on the projected point must still be INSIDE the region, or the cross never fires");
    }

    /// When a zone carries a dedicated `__collision__` mesh, `Collision::build` must collide
    /// against THAT geometry (which includes invisible-but-solid walls) and ignore the rendered
    /// terrain. When absent, it must fall back to the rendered terrain (back-compat). This is
    /// the client half of Component B.
    #[test]
    fn collision_prefers_collision_mesh_and_falls_back() {
        // A visible floor at z=0 (render terrain) plus an INVISIBLE wall at world east=5.
        // In the real pipeline the invisible wall only appears in the `__collision__` mesh
        // (it has no render texture); here we model that by tagging it.
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 0.0, 10.0], [0.0, 0.0, 10.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        };
        // The `__collision__` mesh: the same floor PLUS the invisible wall at east=5, tagged.
        let collision_mesh = MeshData {
            positions: vec![
                // floor
                [0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 0.0, 10.0], [0.0, 0.0, 10.0],
                // invisible wall at world east=5 (libeq p2=5), north 0..10, height 0..10
                [0.0, 0.0, 5.0], [10.0, 0.0, 5.0], [10.0, 10.0, 5.0], [0.0, 10.0, 5.0],
            ],
            normals: vec![[0.0, 1.0, 0.0]; 8], uvs: vec![[0.0, 0.0]; 8],
            indices: vec![0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7],
            texture_name: Some(COLLISION_MESH_TAG.to_string()),
            base_color: [1.0; 4], center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        };

        // With the collision mesh present: the invisible wall blocks movement.
        let with_mesh = Collision::build(
            &ZoneAssets { terrain: vec![floor.clone(), collision_mesh], objects: vec![], textures: vec![] },
            4.0,
        );
        assert!(with_mesh.from_collision_mesh, "should report collision-mesh provenance");
        assert!(!with_mesh.path_clear([3.0, 5.0, 3.0], [7.0, 5.0, 3.0], 0.5),
            "the invisible wall (only in __collision__) must block movement");
        // The floor still grounds correctly.
        assert!((with_mesh.floor_z(3.0, 3.0, 20.0) - 0.0).abs() < 1e-3);

        // Back-compat: a zone with only rendered terrain (no `__collision__`) falls back to it.
        let fallback = Collision::build(
            &ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] },
            4.0,
        );
        assert!(!fallback.from_collision_mesh, "no collision mesh → fallback to rendered terrain");
        // No wall in the rendered terrain, so the same path is clear.
        assert!(fallback.path_clear([3.0, 5.0, 3.0], [7.0, 5.0, 3.0], 0.5),
            "fallback terrain has no invisible wall");
    }

    /// Zone-in reground premise: a player spawned BELOW the floor must be recoverable.
    /// `floor_z` only probes downward and can't see a floor above; `nearest_floor` with an
    /// upward band finds it. (Mirrors the felwithe zone-in burial: spawn z=4, floor ~20.)
    #[test]
    fn nearest_floor_finds_floor_above_a_below_floor_spawn() {
        // Floor quad at height z=10 spanning east/north [0,10]; EQ WLD pos = [east, height, north].
        let floor = MeshData {
            positions: vec![[0.0, 10.0, 0.0], [10.0, 10.0, 0.0], [10.0, 10.0, 10.0], [0.0, 10.0, 10.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0, 0.0, 0.0],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }, 4.0);

        // Player "spawned" at z=2, 8 units BELOW the floor at z=10.
        // Downward-only floor_z can't reach it -> returns the fallback unchanged (buried).
        assert!((col.floor_z(3.0, 3.0, 2.0) - 2.0).abs() < 1e-3,
            "floor_z should not find a floor above the anchor");
        // nearest_floor with an upward band finds the floor at z=10 and lifts the player.
        let f = col.nearest_floor(3.0, 3.0, 2.0, 80.0, 300.0);
        assert!(f.is_some(), "nearest_floor should find the floor above");
        assert!((f.unwrap() - 10.0).abs() < 1e-3, "expected floor z=10, got {:?}", f);
    }

    fn slotted_wall(gap: f32) -> Collision {
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [20.0, 0.0, 20.0], [0.0, 0.0, 20.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // GLB axes -> world: east = p[2], north = p[0], height = p[1].
        let (lo, hi) = (9.0 - gap / 2.0, 9.0 + gap / 2.0);
        let panel = |n0: f32, n1: f32| MeshData {
            positions: vec![[n0, 0.0, 10.0], [n1, 0.0, 10.0], [n1, 10.0, 10.0], [n0, 10.0, 10.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        Collision::build(&ZoneAssets {
            terrain: vec![floor, panel(0.0, lo), panel(hi, 20.0)], objects: vec![], textures: vec![],
        }, 2.0)
    }

    /// The controller moves a CYLINDER of `PLAYER_RADIUS`, so a ray test is not enough. A 1.5u slot is wider than the ray (which has no width at all) and NARROWER
    /// than the character (2 * PLAYER_RADIUS = 2.0u): the ray threads it, the shoulders do not.
    /// `path_clear` must answer for the character's real volume, not for a line.
    #[test]
    fn path_clear_rejects_a_slot_the_ray_fits_through_but_the_character_does_not() {
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        let col = slotted_wall(1.5); // < 2 * PLAYER_RADIUS -> the character cannot fit
        let (from, to) = ([5.0, 9.0, 3.0], [15.0, 9.0, 3.0]); // dead down the middle of the slot
        // the LINE really is unobstructed, so a ray test says "clear"
        assert!(col.line_clear(from, to, r), "the centre ray does thread the slot");
        // ...but the character does not fit, so `path_clear` must NOT call this segment clear
        assert!(!col.path_clear(from, to, r),
            "path_clear must sweep the player's collision volume, not a ray: a {}u slot cannot pass \
             a {}u-radius character", 1.5, r);
        // A slot the character genuinely fits through stays passable — the fix must not seal doors.
        let wide = slotted_wall(2.0 * r + 1.0);
        assert!(wide.path_clear(from, to, r), "a slot wider than the character must stay clear");
    }

    /// The DIAGONAL leak is the reason `path_clear` samples the whole diameter instead of just the
    /// two shoulders.
    ///
    /// A diagonal edge, (9,3) → (11,1), threads a 2.5u slot with a 2.0u clearance, and three rays
    /// (centre + both shoulders) call it clear. On a diagonal the shoulders are offset
    /// PERPENDICULAR to travel, so they slide ALONG the wall rather than across it. One starts
    /// already past the wall plane, and the other never reaches it. The capsule would thread a gap
    /// narrower than itself.
    #[test]
    fn path_clear_does_not_leak_through_a_slot_on_a_diagonal() {
        let col = two_slot_wall(2.5, 6.0); // narrow slot spans north 1.75..4.25
        // The diagonal that leaked: it crosses the wall inside the slot, but a 2.0u-clearance
        // character does not fit through a 2.5u slot.
        assert!(col.line_clear([9.0, 3.0, 3.0], [11.0, 1.0, 3.0], 2.0),
            "the centre line really does thread the slot — that is why the ray test passed it");
        assert!(!col.path_clear([9.0, 3.0, 3.0], [11.0, 1.0, 3.0], 2.0),
            "DIAGONAL LEAK: a 2.0u-clearance character cannot fit a 2.5u slot, on any heading");
        // ...and the orthogonal crossing was already rejected, then and now.
        assert!(!col.path_clear([9.0, 3.0, 3.0], [11.0, 3.0, 3.0], 2.0));
        // The slot IS passable at a clearance it genuinely fits (2.5u slot, 1.0u radius), on the
        // diagonal too — the fix must not seal it.
        assert!(col.path_clear([9.0, 3.0, 3.0], [11.0, 3.0, 3.0], 1.0),
            "a slot the character fits through must stay clear");
    }

    /// A single vertical wall at `north = wall_n`, spanning east 0..20 and up 0..10, plus a floor.
    /// Travel runs ALONG east (parallel to the wall), so the segment never crosses the wall plane —
    /// the exact geometry the travel-parallel feelers are structurally blind to (#381).
    fn parallel_wall(wall_n: f32) -> Collision {
        // GLB axes -> world: east = p[2], north = p[0], height = p[1].
        let floor = MeshData {
            positions: vec![[-5.0, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 0.0, 20.0], [-5.0, 0.0, 20.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let wall = MeshData { // vertical plane at north = wall_n, up 0..10, east 0..20
            positions: vec![[wall_n, 0.0, 0.0], [wall_n, 0.0, 20.0], [wall_n, 10.0, 20.0], [wall_n, 10.0, 0.0]],
            normals: vec![[-1.0, 0.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        Collision::build(&ZoneAssets { terrain: vec![floor, wall], objects: vec![], textures: vec![] }, 2.0)
    }

    /// **#381 — the last clearance hole: a wall the segment runs PARALLEL to.** The travel-parallel
    /// feelers cannot see it (a ray alongside a wall never crosses it), so before this fix `path_clear`
    /// called a segment skimming a wall within the body radius CLEAR even though the character's body
    /// overlaps the wall. The fix samples the body footprint RING along the segment — a radial spoke
    /// fans toward the wall and crosses it.
    ///
    /// MUTATION-DISCRIMINATING: delete the `ring_nearest_hit`-along-the-segment loop in `path_clear`
    /// (the #381 addition) and BOTH `!path_clear` assertions go RED — the feelers alone report clear,
    /// which this test proves directly via `feelers_report_clear`.
    #[test]
    fn path_clear_blocks_a_wall_the_segment_runs_parallel_to() {
        let r = eqoxide_core::physics::PLAYER_RADIUS; // 1.0
        // Wall at north = 5; travel runs east at north = 4.5 — 0.5u from the wall, INSIDE the 1.0u body
        // radius. The character's disc overlaps the wall the whole way, but the segment never crosses it.
        let col = parallel_wall(5.0);
        let from: [f32; 3] = [2.0, 4.5, 3.0];
        let to: [f32; 3] = [18.0, 4.5, 3.0];

        // Mechanism proof: every travel-parallel feeler runs ALONGSIDE the wall and reads clear. This is
        // exactly why the pre-#381 sweep leaked here — reproduce it directly so a mutation that removes
        // the ring sampling has a demonstrated blind spot to fall back into.
        let hlen = ((to[0] - from[0]).powi(2) + (to[1] - from[1]).powi(2)).sqrt();
        let perp = [-(to[1] - from[1]) / hlen * r, (to[0] - from[0]) / hlen * r];
        let feelers_report_clear = [-1.0f32, -0.5, 0.0, 0.5, 1.0].iter().all(|&f| col.line_clear(
            [from[0] + perp[0] * f, from[1] + perp[1] * f, from[2]],
            [to[0] + perp[0] * f, to[1] + perp[1] * f, to[2]], r));
        assert!(feelers_report_clear,
            "sanity: the travel-parallel feelers must ALL read clear here — that is the #381 blind spot \
             this scene reproduces; if a feeler already caught it, the test proves nothing new");

        // AFTER #381: the footprint ring, sampled along the segment, crosses the parallel wall -> BLOCKED.
        assert!(!col.path_clear(from, to, r),
            "path_clear must NOT call a segment clear when a wall lies within the body radius alongside \
             it (#381 parallel-wall hole). MUTATION: remove the ring-along-segment loop -> this goes RED");

        // A wall a full radius away (north = 4.5 + 1.0 = 5.5, so travel at north 4.5 is tangent) is the
        // boundary: TANGENT is clear (the disc touches, does not overlap) — the over-firing boundary the
        // fix deliberately preserves, matching the feeler sweep's parallel-at-exactly-radius convention.
        let tangent = parallel_wall(5.5);
        assert!(tangent.path_clear(from, to, r),
            "a wall EXACTLY a radius away is tangent (touching, not overlapping) and must stay CLEAR — \
             the fix must not over-fire at the boundary");
    }

    /// **#381 over-firing guard — a legitimate body-width parallel corridor stays CLEAR.** The dominant
    /// risk of the ring-sampling fix is that it OVER-rejects a tight-but-passable passage. A corridor
    /// whose walls sit a full radius clear of the centreline must still `path_clear`.
    ///
    /// MUTATION: this whole test stays GREEN when the #381 addition is reverted (the fix does not break
    /// a legitimate passage) — every assertion here holds with OR without the ring loop. It is NOT
    /// vacuously always-clear: the perpendicular walk-into-a-wall assertion shows `path_clear` genuinely
    /// discriminates on this scene (via the pre-existing feeler sweep), independent of the #381 loop. The
    /// fix-DEPENDENT block (a sub-radius parallel wall) is pinned in the companion test above.
    #[test]
    fn path_clear_clears_a_legit_body_width_parallel_corridor() {
        let r = eqoxide_core::physics::PLAYER_RADIUS; // 1.0
        // Walls parallel to travel at north = +half and north = -half, character down the middle at
        // north = 0. `half` comfortably > radius is a passage the character genuinely fits.
        let corridor = |half: f32| -> Collision {
            let floor = MeshData {
                positions: vec![[-half - 2.0, 0.0, 0.0], [half + 2.0, 0.0, 0.0],
                                [half + 2.0, 0.0, 20.0], [-half - 2.0, 0.0, 20.0]],
                normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
                indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
                render_mode: RenderMode::Opaque, anim: None,
            };
            let wall = |n: f32, nx: f32| MeshData {
                positions: vec![[n, 0.0, 0.0], [n, 0.0, 20.0], [n, 10.0, 20.0], [n, 10.0, 0.0]],
                normals: vec![[nx, 0.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
                indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
                render_mode: RenderMode::Opaque, anim: None,
            };
            Collision::build(&ZoneAssets {
                terrain: vec![floor, wall(half, -1.0), wall(-half, 1.0)], objects: vec![], textures: vec![] }, 2.0)
        };
        let roomy = corridor(2.0);

        // Slide down the middle (walls +/-2.0 from the centreline, 2.0u > radius): CLEAR — no over-fire.
        assert!(roomy.path_clear([2.0, 0.0, 3.0], [18.0, 0.0, 3.0], r),
            "a body-width corridor the character genuinely fits (2.0u clearance each side, r={r}) must \
             stay CLEAR — the #381 ring sampling must not over-reject a legitimate tight passage");

        // Non-vacuous: walking PERPENDICULAR straight into the north wall (north 0 -> 1.5, within a
        // radius of the wall at north 2.0) is BLOCKED by the pre-existing feeler sweep — so `path_clear`
        // demonstrably discriminates on this exact scene, with or without the #381 loop.
        assert!(!roomy.path_clear([10.0, 0.0, 3.0], [10.0, 1.5, 3.0], r),
            "walking into the corridor wall must block — proves the CLEAR result above is a real \
             discrimination, not a vacuously-always-clear test");
    }

    /// A wall at east=10 with TWO ways through: a `narrow` slot centred on north=3 and a `wide` one
    /// centred on north=15. Floor is 20x20 at z=0.
    fn two_slot_wall(narrow: f32, wide: f32) -> Collision {
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [20.0, 0.0, 20.0], [0.0, 0.0, 20.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let panel = |n0: f32, n1: f32| MeshData {
            positions: vec![[n0, 0.0, 10.0], [n1, 0.0, 10.0], [n1, 10.0, 10.0], [n0, 10.0, 10.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        Collision::build(&ZoneAssets {
            terrain: vec![
                floor,
                panel(0.0, 3.0 - narrow / 2.0),                 // ..narrow slot @ north 3..
                panel(3.0 + narrow / 2.0, 15.0 - wide / 2.0),   // ..wide slot @ north 15..
                panel(15.0 + wide / 2.0, 20.0),
            ],
            objects: vec![], textures: vec![],
        }, 2.0)
    }

    #[test]
    fn collision_path_clear_blocks_walking_into_wall() {
        // Vertical wall at world east=5: EQ p2=5 (render.X), north=p0 [0,10], height=p1 [0,10].
        let wall = MeshData {
            positions: vec![[0.0, 0.0, 5.0], [10.0, 0.0, 5.0], [10.0, 10.0, 5.0], [0.0, 10.0, 5.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0, 0.0, 0.0],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![wall], objects: vec![], textures: vec![] }, 4.0);
        let chest = 3.0_f32;

        // Standing at east=3, stepping east toward the wall (to east=4.5) within the
        // 2-unit radius reaches the wall at east=5 → blocked.
        assert!(!col.path_clear([3.0, 5.0, chest], [4.5, 5.0, chest], 2.0),
            "stepping into the wall should be blocked");
        // Stepping along the wall (north) at east=3 is clear.
        assert!(col.path_clear([3.0, 5.0, chest], [3.0, 7.0, chest], 2.0),
            "sliding parallel to the wall should be clear");
        // Stepping away from the wall (west) is clear.
        assert!(col.path_clear([3.0, 5.0, chest], [1.5, 5.0, chest], 2.0),
            "stepping away from the wall should be clear");
    }

    /// Build a vertical wall plane at world east=`e`, spanning north [-100,100] and height [h0,h1].
    fn wall_east(e: f32, h0: f32, h1: f32) -> MeshData {
        MeshData {
            positions: vec![[-100.0, h0, e], [100.0, h0, e], [100.0, h1, e], [-100.0, h1, e]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// Build a horizontal floor at height `z` covering east [e0,e1] and north [-100,100].
    fn floor_band(z: f32, e0: f32, e1: f32) -> MeshData {
        MeshData {
            positions: vec![[-100.0, z, e0], [100.0, z, e0], [100.0, z, e1], [-100.0, z, e1]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// **Shape probe.** Dumps every surface in the qcat wedge column facing-blind, with the distance
    /// UP to the next SOLID surface (headroom) and whether water sits above, to show whether
    /// the standable-floor test in `column_hits` (facing-blind + headroom + anchoring) can accept
    /// -42.97 while rejecting a ceiling. The probe asserts nothing.
    #[test]
    #[ignore = "requires qcat glb; D-2 shape probe"]
    fn probe_qcat_column_vs_fixture() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let za = ZoneAssets::from_glb(&std::path::Path::new(&dir).join("qcat.glb")).unwrap();
        let mut col = Collision::build(&za, 32.0);
        // this probe reports water-adjacent column shape, and without the region map it would
        // print confident dry answers, so refuse to run rather than measure nothing
        crate::water_grid::ZoneWater::load(&std::path::Path::new(&dir).join("maps/water"), "qcat")
            .install(&mut col).expect("qcat .wtr must load — a dry qcat probe measures nothing");
        let (x, y) = (4.0f32, 809.8);
        println!("qcat column at ({x},{y}):  z_min={:.1} z_max={:.1}", col.z_min, col.z_max);
        // Enumerate surfaces facing-blind, top→down, via ground_below stepping.
        let mut top = col.z_max;
        for _ in 0..30 {
            let Some(s) = col.ground_below(x, y, top, col.z_max - col.z_min + 10.0) else { break };
            top = s - 0.5;
            // headroom UP: distance to next solid surface above `s`.
            let head = (1..400).map(|k| s + k as f32 * 0.5)
                .find(|&z| col.nearest_hit_t([x, y, z - 0.25], [x, y, z + 0.25]).is_some())
                .map(|z| z - s);
            let up_facing = col.nearest_floor(x, y, s, 0.6, 0.6).is_some(); // filtered → up-facing (or valve)
            let water_above = col.in_water([x, y, s + 1.0]) || col.in_water([x, y, s + 3.0]);
            println!("  surface z={s:8.2}  up_facing(filtered)={up_facing:5}  headroom_to_solid_above={head:?}  water_above={water_above}");
        }
        // The two D-2-relevant surfaces the gates care about:
        println!("controller ground_below@-42: {:?}", col.ground_below(x, y, -42.0, 200.0));
    }

    // ─────────────────────── support-axis fixtures ───────────────────────
    // These pin that the
    // facing-blind standable classifier in `column_hits` rejects ceilings via headroom and anchoring.
    /// An UP-facing floor plane at height `z` over east [e0,e1] × north [-100,100] (`tri_nz > 0`, seen
    /// by `nearest_floor`). The `floor_band` helper's winding is *down*-facing and only serves
    /// facing-BLIND queries (`path_clear`/`line_clear`), while fixtures that depend on facing use
    /// these explicit helpers. No test verifies the windings. The windings come from the `normals`
    /// and `indices` of these helpers.
    fn floor_up(z: f32, e0: f32, e1: f32) -> MeshData {
        MeshData {
            positions: vec![[-100.0, z, e0], [100.0, z, e0], [100.0, z, e1], [-100.0, z, e1]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 2, 1, 0, 3, 2], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// A DOWN-facing ceiling plane at height `z` (`tri_nz < 0`, discarded by the facing filter).
    fn ceiling_down(z: f32, e0: f32, e1: f32) -> MeshData {
        MeshData {
            positions: vec![[-100.0, z, e0], [100.0, z, e0], [100.0, z, e1], [-100.0, z, e1]],
            normals: vec![[0.0, -1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// **RETRACTED at D-2: `open_air_ceiling_is_never_returned_as_floor`** (the owner-approved D-1
    /// fixture) and its winding-sanity companion. Both asserted that a down-facing surface with OPEN
    /// SKY above it (floor at z=0, ceiling at z=8, nothing on top) is a *ceiling* `nearest_floor` must
    /// never return.
    ///
    /// **That premise is FALSIFIED by qcat.** The D-2 shape probe (`probe_qcat_column_vs_fixture`,
    /// measured 2026-07-14) found the character's walkable −42.97 surface is DOWN-facing, with NOTHING
    /// solid above it, and an up-facing floor 13u below — geometrically *identical* to the fixture's
    /// z=8. So "down-facing + open above = ceiling" is wrong: qcat proves such a surface is walkable
    /// floor. A facing-blind classifier that rejected the fixture's z=8 would also delete qcat's
    /// walkway — the very bug #375 fixes. The owner reviewed this measurement and RETRACTED the fixture.
    ///
    /// The genuine #329 ceilings are caught by the two gates that replaced it:
    /// `close_roof_ceiling_is_rejected_by_headroom` (a ceiling with a roof close above → low headroom)
    /// and `qcat_pocket_nearest_floor_is_never_the_ceiling` (a far roof, excluded by the `ref_z`
    /// window). See the design doc's "RETRACTED" note.
    ///
    /// **THE #329 CLOSE-ROOF GATE (D-2, mutation-checked).** A realistic ceiling — a down-facing
    /// surface with a solid roof CLOSE above it — must be rejected by the headroom test
    /// (`headroom_to_next_solid_above < NAV_AGENT_HEIGHT`). This is a two-storey sandwich: room-A floor
    /// at z=0 (10u of headroom → standable), room-A ceiling at z=10 (down-facing, only 1u below
    /// room-B's floor → headroom 1 → REJECTED), room-B floor at z=11 (standable). The ceiling at z=10
    /// must NEVER be returned; both real floors (0 and 11) must be.
    ///
    /// This is NOT the #372 "decorative rock slab" cheat — there the slab was cosmetic while the
    /// classifier still used winding; here the roof-above IS the classifier's real input (a ceiling has
    /// a roof; that is what makes it a ceiling, not its winding). Mutation-check: drop the headroom test
    /// (see the commented line) → the ceiling at z=10 becomes "standable" → `nearest_floor@10` returns
    /// ~10 → RED.
    #[test]
    fn close_roof_ceiling_is_rejected_by_headroom() {
        let col = Collision::build(&ZoneAssets {
            terrain: vec![
                floor_up(0.0, -100.0, 100.0),      // room-A floor (10u headroom → standable)
                ceiling_down(10.0, -100.0, 100.0), // room-A ceiling (1u below room-B floor → rejected)
                floor_up(11.0, -100.0, 100.0),     // room-B floor (open above → standable)
            ], objects: vec![], textures: vec![],
        }, 32.0);
        // The low-headroom ceiling (10) must never be reported as a floor, even from a reference
        // point 1u away from it; the two real floors (0 and 11) must be found.
        let near = |ref_z: f32| col.nearest_floor(0.0, 0.0, ref_z, 0.6, 0.6);
        assert!(near(10.0).is_none_or(|z| (z - 10.0).abs() >= 0.5),
            "nearest_floor reported the low-headroom ceiling z=10 (#329). MUTATION: drop the headroom check and this fires.");
        assert!(near(0.0).is_some_and(|z| z.abs() < 0.5), "room-A floor z=0 (10u headroom) must be standable");
        assert!(near(11.0).is_some_and(|z| (z - 11.0).abs() < 0.5), "room-B floor z=11 (open above) must be standable");
    }

    /// **D-2 winding sanity (facing-blind now):** after D-2 `nearest_floor` is FACING-BLIND, so it
    /// accepts an inverted (down-facing) floor with clearance above — that is the whole #375 fix. This
    /// pins that: a lone `floor_up` is standable AND a lone `ceiling_down` with open air above is ALSO
    /// standable now (it is walkable floor, per qcat). Contrast `close_roof_ceiling_*`: only a ceiling
    /// with a *roof close above* is rejected.
    #[test]
    fn nearest_floor_is_facing_blind_after_d2() {
        let up = Collision::build(&ZoneAssets {
            terrain: vec![floor_up(0.0, -100.0, 100.0)], objects: vec![], textures: vec![] }, 32.0);
        assert!(up.nearest_floor(0.0, 0.0, 0.0, 5.0, 20.0).is_some_and(|z| z.abs() < 0.5),
            "an up-facing floor is standable");
        let down = Collision::build(&ZoneAssets {
            terrain: vec![ceiling_down(0.0, -100.0, 100.0)], objects: vec![], textures: vec![] }, 32.0);
        assert!(down.nearest_floor(0.0, 0.0, 0.0, 5.0, 20.0).is_some_and(|z| z.abs() < 0.5),
            "a down-facing surface with open air above is ALSO standable now (facing-blind) — the qcat fix");
    }

    /// **The qcat pocket gate.** At the qcat spawn pocket the column is `[roof 391.8, floor -70.0]`.
    /// The 391.8 roof is never ground, because the roof has rock above it and fails headroom.
    #[test]
    #[ignore = "requires the cached qcat glb at $ZONE_DIR"]
    fn qcat_pocket_nearest_floor_is_never_the_ceiling() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let za = ZoneAssets::from_glb(&std::path::Path::new(&dir).join("qcat.glb")).unwrap();
        let col = Collision::build(&za, 32.0);
        // the spawn pocket XY, where the floor is ~-70 and the catacombs roof is ~+391.8
        let f = col.nearest_floor(-48.0, 1058.0, -66.0, 20.0, 100.0);
        assert!(f.is_some_and(|z| z < 100.0),
            "qcat pocket nearest_floor returned the ceiling (got {f:?})");
    }

    #[test]
    fn ground_below_uses_origin_and_depth() {
        let col = Collision::build(
            &ZoneAssets { terrain: vec![floor_band(0.0, -100.0, 100.0)], objects: vec![], textures: vec![] }, 8.0);
        // Foot at z=5, probe from foot+1=6 down 200 → finds floor at z=0.
        let f = col.ground_below(0.0, 0.0, 6.0, 200.0).expect("floor below within probe");
        assert!((f - 0.0).abs() < 1e-2, "expected floor z=0, got {f}");
        // Shallow probe that doesn't reach the floor returns None.
        assert!(col.ground_below(0.0, 0.0, 6.0, 3.0).is_none(),
            "a 3u probe from z=6 cannot reach the floor at z=0");
    }

    #[test]
    fn footprint_clear_detects_embedded_vs_free() {
        // Two walls forming a tight box around the origin at east ±0.8 — radius-1 footprint at the
        // centre pokes through both, so it is NOT clear.
        let boxed = Collision::build(&ZoneAssets {
            terrain: vec![wall_east(0.8, 0.0, 10.0), wall_east(-0.8, 0.0, 10.0)], objects: vec![], textures: vec![],
        }, 4.0);
        assert!(!boxed.footprint_clear(0.0, 0.0, 0.0, 1.0, 8),
            "footprint wedged between two close walls should read as blocked");
        // Open floor: a radius-1 footprint is clear.
        let open = Collision::build(
            &ZoneAssets { terrain: vec![floor_band(0.0, -100.0, 100.0)], objects: vec![], textures: vec![] }, 8.0);
        assert!(open.footprint_clear(0.0, 0.0, 0.0, 1.0, 8),
            "footprint on open floor should be clear");
    }
}


#[cfg(test)]
mod zone_line_indices_is_not_lossy_803 {
    use super::*;

    use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};

    use eqoxide_core::region_map::{RegionDataAbsent, RegionLoadError, RegionMap};

    fn grid() -> Collision {
        let floor = MeshData {
            positions: vec![[-100.0, 0.0, -100.0], [100.0, 0.0, -100.0],
                            [100.0, 0.0, 100.0],   [-100.0, 0.0, 100.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        };
        Collision::build(&ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }, 8.0)
    }

    /// **The honest empty, which must stay green.** A region map that LOADED and genuinely contains
    /// no zone-line regions still answers `Ok(vec![])` — `/zone_exits` must keep serving `[]`/200
    /// for the common real zone that has no exits baked. If the fix had been "refuse whenever the
    /// list is empty", this is the test that would have caught it.
    #[test]
    fn a_loaded_map_with_no_zone_lines_still_answers_the_empty_list() {
        let mut c = grid();
        c.set_region_data(Ok(std::sync::Arc::new(RegionMap::flat_below(-10.0))));
        assert_eq!(c.zone_line_indices(), Ok(vec![]),
            "a map that loaded and has no zone-line regions is an ANSWER, not a refusal");
    }

    /// A loaded map WITH a zone line still enumerates it — the `Ok` arm is not a stub.
    #[test]
    fn a_loaded_map_with_a_zone_line_enumerates_it() {
        let mut c = grid();
        c.set_region_data(Ok(std::sync::Arc::new(
            RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, 7))));
        assert_eq!(c.zone_line_indices(), Ok(vec![7]));
    }

    /// **The falsehood, as a test.** Every way the `.wtr` can fail to load must come back as `Err`
    /// carrying WHICH way — never as the empty list. The four `RegionLoadError`s are looped rather
    /// than represented by one, because the old code collapsed all of them into a single `None` and
    /// a fix that only handled the one in the issue title would rebuild the same lie for the others.
    #[test]
    fn every_load_failure_is_an_error_not_an_empty_list() {
        for e in [
            RegionLoadError::Missing,
            RegionLoadError::NotRegionData,
            RegionLoadError::UnsupportedVersion(99),
            RegionLoadError::Truncated { declared_nodes: 400, bytes: 12 },
        ] {
            let mut c = grid();
            c.set_region_data(Err(e.clone()));
            assert_eq!(c.zone_line_indices(), Err(RegionDataAbsent::LoadFailed(e.clone())),
                "{e:?}: a failed READ published as an empty exit list tells the agent it is sealed \
                 in a zone, from a response it has no way to doubt (#803)");
        }
    }

    /// The fourth case, distinct from all of the above: nothing was ever attached to this grid (a
    /// synthetic scene, or a zone loaded before the region map). Also not an empty exit list.
    #[test]
    fn a_grid_with_no_region_data_attached_is_an_error_too() {
        assert_eq!(grid().zone_line_indices(), Err(RegionDataAbsent::NotAttached));
        assert_eq!(grid().region_data_absent(), Some(&RegionDataAbsent::NotAttached));
    }

    /// The reason has to survive the trip to the caller INTACT: `/zone_exits` reports
    /// `absent.as_str()`, so a `set_region_data` that stored one canned failure for all of them
    /// would leave the endpoint naming the wrong cause while still looking "explicit".
    #[test]
    fn the_reported_reason_names_the_actual_failure() {
        let mut c = grid();
        c.set_region_data(Err(RegionLoadError::Truncated { declared_nodes: 400, bytes: 12 }));
        assert_eq!(c.region_data_absent().unwrap().as_str(), "region_data_truncated");
        let mut c = grid();
        c.set_region_data(Err(RegionLoadError::Missing));
        assert_eq!(c.region_data_absent().unwrap().as_str(), "region_data_missing");
    }
}


#[cfg(test)]
mod placement_tests {
    use super::*;

    use crate::diagnostics::Placement;

    use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};

    // EQ WLD space: pos = [north, height, east].
    fn quad(p: [[f32; 3]; 4]) -> MeshData {
        MeshData {
            positions: p.to_vec(), normals: vec![], uvs: vec![],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// Up-facing floor at height `z`, covering `|east|, |north| <= half`.
    fn floor(z: f32, half: f32) -> MeshData {
        quad([[-half, z, -half], [-half, z, half], [half, z, half], [half, z, -half]])
    }

    /// Vertical wall at world east `e`, north `[n0,n1]`, height `[h0,h1]`.
    fn wall_e(e: f32, n0: f32, n1: f32, h0: f32, h1: f32) -> MeshData {
        quad([[n0, h0, e], [n1, h0, e], [n1, h1, e], [n0, h1, e]])
    }

    fn build(m: Vec<MeshData>) -> Collision {
        Collision::build(&ZoneAssets { terrain: m, objects: vec![], textures: vec![] }, 8.0)
    }


    /// **#994 review: a documented repro command that selects NOTHING is worse than a broken one.**
    ///
    /// This file's `--ignored` measurements are documented as `cargo test … --lib <name>`. Run from
    /// the workspace root — the repo's default cwd — `--lib` without `-p` selects the ROOT package's
    /// lib target, which contains none of these tests, so the run prints `running 0 tests`,
    /// `241 filtered out` and **exits 0**. Green, no rows, no error: indistinguishable from a pass
    /// by the exit code, and it is the exit code an agent reads. Measured, then fixed by adding
    /// `-p eqoxide-zone-geometry`.
    ///
    /// **What this matches — a token, not coverage.** A line is a RECIPE if it contains the
    /// `cargo`-`test` invocation AND ` --lib ` with its trailing space, followed immediately by a
    /// `[a-z0-9_]` test-name character. Both halves do work in this file, and neutering the second
    /// half measured which does which: the two bare prose mentions fail the SPLIT (nothing follows
    /// `--lib` but a backtick), while the `--workspace --lib --no-fail-fast` line and this
    /// rustdoc's own `--lib <name>` sketch pass the split and are rejected by the first-character
    /// test. A prose line that put a lowercase word straight after ` --lib ` WOULD be a false
    /// positive; none does at this head, and the count assertion below is what would surface one.
    /// Every recipe so classified must carry `-p eqoxide-zone-geometry`, and the total is asserted, because a
    /// scan reporting only exceptions cannot tell "nothing wrong"
    /// from "nothing looked at".
    ///
    /// **`-p` is the right remedy HERE because of what these recipes name, not in general.** The
    /// recipe resolves to a `fn` definition in this file — the `eqoxide-zone-geometry` LIB target — and this
    /// crate has no `tests/` directory, so there is no integration target a recipe could have
    /// meant. `-p` does NOT rescue an integration test: `-p <pkg> --lib <name>` against a test
    /// living in `tests/*.rs` still selects the lib target, finds nothing, and exits 0. Such a
    /// recipe needs `--test <file-stem>`, and this guard would wave it through.
    ///
    /// NOT matched, and none of these is claimed: a recipe wrapped across two lines; one written
    /// `--package` instead of `-p`; `--test`/`--bin`/`--bins` targets; and any file other than this
    /// one. The corpus is this source alone, and the guard is silent about a recipe in another file.
    #[test]
    fn every_documented_repro_command_in_this_file_names_its_package() {
        // Split so this guard is not itself a corpus hit — its corpus is its own source.
        let invocation = concat!("cargo ", "test ");
        let src = include_str!("collision.rs");
        let (mut recipes, mut missing) = (0usize, Vec::new());
        for (i, line) in src.lines().enumerate() {
            if !line.contains(invocation) {
                continue;
            }
            let Some(rest) = line.split(" --lib ").nth(1) else { continue };
            // A recipe names a test after `--lib`; the prose mentions and the `--workspace` line
            // do not, which is the whole discriminator.
            if !rest.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                continue;
            }
            recipes += 1;
            if !line.contains("-p eqoxide-zone-geometry") {
                missing.push(format!("collision.rs:{}", i + 1));
            }
        }
        assert!(missing.is_empty(),
            "these documented repro commands omit `-p eqoxide-zone-geometry`, so from the workspace root they \
             select the ROOT package's lib target, print `running 0 tests` and exit 0 — a vacuous \
             green, not a measurement: {missing:?}");
        assert_eq!(recipes, 1,
            "expected 1 documented `--lib` repro commands in this file, found {recipes}. If one was \
             added or deleted, weigh it — a scan that stopped reaching them would otherwise pass by \
             finding nothing.");
    }

    /// **The four-way decomposition, over a hand-derived sweep.**
    ///
    /// `Placement` splits a boolean into four named worlds, so the split itself needs pinning: a
    /// variant that can never be produced, or one produced for the wrong disjunct, would be a new
    /// lie in a field agents are told to trust.
    ///
    /// The per-point assertion is against the two disjuncts computed independently here, from
    /// `footprint_clear` and `ground_below` directly — the primitives, not `body_placement`. The
    /// per-variant counts are a REACH CONTROL and are derived from the GEOMETRY, not from a run:
    /// the scene is a floor over `|east|,|north| <= 20`, a slot of walls at `east = ±0.6` over
    /// `north ∈ [-20,20]`, and a second identical slot over `north ∈ [30,40]` where there is no
    /// floor. The sweep is `east ∈ {-3..3}` (7) × `north ∈ {-15,-5,5,15,35,45}` (6) = 42 points at
    /// `z = 1`. The footprint ring is cast at `z + 3 = 4`, inside both slots' `[0,10]` height span,
    /// and reaches `PLAYER_RADIUS = 1.0`, so a slot pierces exactly `east ∈ {-1,0,1}` (3 of 7 —
    /// `east = ±2` is 1.4 u from the nearer wall, outside the ring). Hence: 4 floored rows × 4
    /// unpierced = 16 `Placeable`; 4 × 3 = 12 `FootprintPierced`; the `north = 35` row gives 3
    /// `FootprintPiercedAndNoFloorBelow` and 4 `NoFloorBelow`; the `north = 45` row (no walls, no
    /// floor) gives 7 more `NoFloorBelow`, for 11.
    #[test]
    fn placement_names_which_disjunct_failed() {
        let scene = build(vec![
            floor(0.0, 20.0),
            wall_e(0.6, -20.0, 20.0, 0.0, 10.0), wall_e(-0.6, -20.0, 20.0, 0.0, 10.0),
            wall_e(0.6, 30.0, 40.0, 0.0, 10.0),  wall_e(-0.6, 30.0, 40.0, 0.0, 10.0),
        ]);
        let radius = eqoxide_core::physics::PLAYER_RADIUS;
        let (mut placeable, mut pierced_only, mut no_floor_only, mut both, mut visited) = (0, 0, 0, 0, 0);
        for ei in -3..=3i32 {
            for &n in &[-15.0f32, -5.0, 5.0, 15.0, 35.0, 45.0] {
                let e = ei as f32;
                let p = [e, n, 1.0];
                visited += 1;
                // The two disjuncts, computed here from the primitives.
                let pierced = !scene.footprint_clear(p[0], p[1], p[2], radius, PLACEMENT_RING_DIRS);
                let no_floor = scene.ground_below(p[0], p[1], p[2] + GROUND_ORIGIN, GROUND_DEPTH).is_none();
                let got = scene.body_placement(p);
                let want = match (pierced, no_floor) {
                    (false, false) => Placement::Placeable,
                    (true, false)  => Placement::FootprintPierced,
                    (false, true)  => Placement::NoFloorBelow,
                    (true, true)   => Placement::FootprintPiercedAndNoFloorBelow,
                };
                assert_eq!(got, want, "at {p:?}: pierced={pierced} no_floor={no_floor}");
                assert_eq!(got.is_embedded(), pierced || no_floor,
                    "at {p:?}: `is_embedded` must stay the controller's original disjunction");
                match got {
                    Placement::Placeable => placeable += 1,
                    Placement::FootprintPierced => pierced_only += 1,
                    Placement::NoFloorBelow => no_floor_only += 1,
                    Placement::FootprintPiercedAndNoFloorBelow => both += 1,
                }
            }
        }
        // REACH CONTROL — every count derived from the scene above, not observed from a run.
        assert_eq!(visited, 42, "the sweep must visit all 42 points");
        assert_eq!((placeable, pierced_only, no_floor_only, both), (16, 12, 11, 3),
            "all four variants reachable, in the proportions the geometry dictates");
    }
}
