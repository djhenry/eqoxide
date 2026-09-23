//! [`Collision`] is the zone's flattened-triangle spatial grid: `build` consumes a loaded
//! [`eqoxide_assets::ZoneAssets`] (mesh/texture loading stays in `eqoxide-assets`; this module
//! only reads its output), and its methods answer every geometric query the render/movement/nav
//! code needs — `floor_z`/`nearest_floor` (grounding), `nearest_hit_t`/`segment_blocked` (camera +
//! nameplate occlusion), `path_clear`/`footprint_clear` (movement gating), water/climb/zone-line
//! lookups, and clearance probing. A* pathfinding (`find_path`/`find_path_ex`/`astar`/...) is
//! A*-only and lives in `eqoxide-nav`'s `collision.rs`, behind the `CollisionAStar` trait extending
//! this crate's [`Collision`] — see `docs/specs/2026-09-21-agent-harness-separation-plan-zone-geometry.md`.
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
/// zone load and publishes it here; the nav thread reads it to gate movement. Inner
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
    /// underside of a bridge). Nav used to treat any surface a vertical ray crossed as a floor —
    /// which is how A* ended up standing on qcat's ceiling and planning routes through solid rock
    /// (#329). Computed once at build; the ray tests filter on it. (~4 bytes/tri.)
    tri_nz:    Vec<f32>,
    cells:     Vec<Vec<u32>>,
    // Grid geometry. `pub` so the app-crate walker-sim corpus integration test
    // (`tests/walker_sim.rs::faithful_walker_drift_corpus`) can sample random floor points across the
    // grid — it steps the app-layer `CharacterController`, so it lives outside this crate (#544 Step 2f)
    // and can only reach these dimensions if they are public. Read-only dimension accessors; no writer.
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
    /// How many **DOWN-facing** (inverted-art) TRIANGLES `column_hits` has admitted as standing
    /// ground since zone load (D-2, #375). One ground probe adds as many as that column's art
    /// carries, so this is an unscaled total, never a rate — the name says `surfaces` because that
    /// is the unit (#960). Such ground is not wrong (qcat proves inverted-art floor is walkable) but
    /// it is *unverified*, so an agent must be able to SEE it is pathing there rather than be quietly
    /// handed it. Surfaced as `nav_support` on `/v1/observe/debug`. This REPLACES the old
    /// `column_bottom`-fallback counter (that valve was deleted in D-2); the honesty signal did not
    /// go with it. Relaxed: a diagnostic counter, never read for control flow.
    facing_blind_surfaces: std::sync::atomic::AtomicU64,
    /// How many routes only existed at the MINIMUM clearance (`PLAYER_RADIUS`) — i.e. threaded a
    /// narrow door or a tight bridge with no margin to spare. Surfaced as `nav_tight` so an agent is
    /// never silently handed a riskier path than it thinks (`search_tiered`).
    tight_plans: std::sync::atomic::AtomicU64,
    /// The zone's region map (from its `.wtr`) — **or the reason there isn't one** (#803).
    ///
    /// `Ok`: find_path may DESCEND through water (swim down a canal/shaft) to a lower floor that has
    /// no walkable connection, and [`Collision::zone_line_indices`] enumerates the zone's real exits.
    ///
    /// `Err`: this grid has NO region data, and here is why. It is a `Result` rather than an
    /// `Option` because the absence used to be laundered into an answer: `zone_line_indices` handed
    /// back an empty `Vec` and `/v1/observe/zone_exits` published it as `[]` with 200 OK, which is
    /// also the true reading for a zone that genuinely has no zone lines. Exits are the only way out
    /// of a zone, so an agent read a failed READ as "sealed in". Written only by
    /// [`Collision::set_region_data`]; read through [`Collision::region_map`].
    water: Result<std::sync::Arc<eqoxide_core::region_map::RegionMap>,
                  eqoxide_core::region_map::RegionDataAbsent>,
    /// True when the terrain triangles came from a dedicated `__collision__` mesh (SOLID +
    /// INVIS faces, PASSABLE excluded). False for legacy zones with no baked collision mesh,
    /// where the rendered terrain is used as a fallback. Diagnostic/provenance only.
    pub from_collision_mesh: bool,
    /// Precomputed `(zone_line_index, [east, north, z])` for each zone-line region, built once from
    /// the water map at `set_water` (zone load). Lets `find_zone_line_near` be an O(1) cache read on
    /// the network thread instead of an exhaustive scan that linkdead-ed the client (#204).
    zone_line_regions: Vec<(i32, [f32; 3])>,
    /// Every climbable surface in this zone, derived from the placed objects at [`Collision::build`]
    /// (#309). PHYSICAL capability: "is the character on something it could climb?" — the question
    /// the movement controller asks. Present whether or not the ladder leads anywhere useful.
    climb_volumes: Vec<crate::climb::ClimbVolume>,
    /// Routes that used a climb edge, surfaced as `nav_climb` — see [`Collision::climb_plans`].
    climb_plans: std::sync::atomic::AtomicU64,
    /// The zone-lifetime static clearance field (#378 / design §3d, the `MemoField`): graded
    /// wall/ground distances per (2 u cell, floor bucket), computed on demand and cached until the
    /// zone (and this struct) is dropped. See `traversability::ClearanceField`.
    clearance: ClearanceField,
    /// The sparse water-span grid (3D-water-volume nav design §5, Slice 1). `None` until built by
    /// [`Collision::build_water_grid`] and stored via [`Collision::set_water_grid`]. **Slice 1 never
    /// reads this in the search/walker** — it is purely additive storage; the water-node generator
    /// that consumes it is Slice 2. Deliberately NOT built eagerly in `set_water` (that would change
    /// zone-load timing, a behaviour change, and defeat the point of the Slice-1 build-cost
    /// measurement that decides eager-vs-lazy, design §5.3 / owner decision #5).
    water_grid: Option<crate::water_grid::WaterGrid>,
    /// LAZY water-grid cache (Slice 2, owner decision #5 — LOCKED lazy, not eager). The span grid is
    /// built on the FIRST water plan in a water zone via [`Self::water_grid_active`], not at zone
    /// load: the measured Slice-1 build cost (357 ms – 1 s) far exceeds the 100 ms eager threshold,
    /// so paying it once, off the net thread, on demand is the honest trade. `OnceLock` makes the
    /// build happen exactly once per zone (a new zone builds a new `Collision`, hence a fresh lock),
    /// and every A* pass / tier / walker sharing the `Arc<Collision>` reads the one result. Distinct
    /// from `water_grid` above so an explicit [`Self::set_water_grid`] (tests, the measurement
    /// harness) still wins and is never shadowed by a lazily-built one.
    water_grid_lazy: std::sync::OnceLock<crate::water_grid::WaterGrid>,
}

/// Outcome of building one water-span column (design §5.1). Separates "no navigable water here"
/// (`Dry`) from the design-premise honesty case "water with no queryable bottom" (`UnboundedBelow`,
/// §5.2), which the builder counts rather than fabricating a floor for.
enum WaterColumnResult {
    Column(crate::water_grid::WaterColumn),
    UnboundedBelow,
    Dry,
}

/// **D-2 (`is_standable`, #375): the two knobs of the shared floor predicate.** A surface is standable
/// ground, FACING-BLIND, iff `|nz| >= NAV_NEAR_HORIZONTAL` (flat enough to stand on) AND it has
/// `NAV_AGENT_HEIGHT` of open space above it before the next SOLID surface (else it is under a ceiling,
/// not standing room). This replaces the winding-sign filter (`nz <= 0` deleted real inverted-art
/// floor — the qcat live wedge, #375) AND its `column_bottom` recovery valve.
///
/// `NAV_NEAR_HORIZONTAL` is tied to the walk-grade limit: a unit normal's `|z|` for a surface at grade
/// `g` is `1/sqrt(1+g²)`, and `MAX_WALK_GRADE = 1.2` (the astar climb cap) gives
/// `1/sqrt(1+1.44) ≈ 0.64`.
/// So a surface `is_standable` rejects for flatness is exactly one astar's grade limit would
/// reject anyway — no new seal there.
///
/// `NAV_AGENT_HEIGHT` is the clearance a standing character needs. It must EXCEED a real ceiling's
/// slab-gap (a room ceiling has its roof right above → tiny headroom → rejected) yet stay BELOW a real
/// room's height (or a low room's floor would be wrongly rejected → seal). The controller's own chest
/// collision ray sits at `foot + 4.0` (`movement.rs`), so ~5u is the clearance a body actually needs;
/// this is measured against route-success (≥ 99.50%) before shipping.
///
/// **PR-A landed (Phase 2): the single source of truth is now `traversability::PLAYER_BODY`.** These
/// two consts are thin aliases into it — the VALUE lives on the shared `Body`
/// (`Body::near_horizontal` / `Body::agent_height`), and every existing `NAV_NEAR_HORIZONTAL` /
/// `NAV_AGENT_HEIGHT` reference reads through here so no second copy exists. Change the number on the
/// `Body`, not here.
pub const NAV_NEAR_HORIZONTAL: f32 = crate::body::PLAYER_BODY.near_horizontal;
pub const NAV_AGENT_HEIGHT: f32 = crate::body::PLAYER_BODY.agent_height;

/// Contact tolerance, in ULPs of the largest coordinate involved in a ray/triangle test. See
/// [`contact_tol`].
///
/// **Chosen by measurement, not by taste — and the first choice was wrong.** Swept over the
/// baked-asset corpus of driven swim descents from real submerged floors
/// (`movement::tests::a_driven_swim_descent_never_passes_a_real_zone_floor`; the run this table
/// came from was 11 zones, 328 submerged columns, 3936 runs — the test prints its own corpus size,
/// so re-take it rather than trusting these counts on a different asset cache). Through-count by
/// value, every point RUN:
///
/// | `CONTACT_TOL_ULPS` | 0 | 1 | 2 | 3 | 4 | 5 | 6 | **7** | 8 | 16 | **32** | 64 … 4096 |
/// |---|---|---|---|---|---|---|---|---|---|---|---|---|
/// | ran through the floor | 146 | 22 | 10 | 4 | 4 | 4 | 2 | **0** | 0 | 0 | **0** | 0 |
///
/// **The cliff is at `7`, not at `4`.** An earlier, smaller sample of this same corpus put it at
/// `4` and this constant shipped at `8` on that basis — one ULP of margin over a cliff whose
/// position moved when the sample grew. It is recorded here because it is the same defect class the
/// rest of this PR is about: a number that looked measured but was measured on too little.
///
/// `32` is the shipped value: **4.6× above the measured cliff**, with the corpus measured green
/// continuously from `7` to `4096` (585× the cliff), so it is nowhere near a boundary on either
/// side. The high side is bounded separately — at `1e7` ULPs, 20 tests across `collision`,
/// `traversability`, `steering` and `movement` go RED, so "too sticky" is a caught failure, not a
/// silent one. (A 21st also failed in that run, in the root crate's asset-sync observation tests —
/// a login/sync bookkeeping test with no geometry in it. The failure was not reproduced or
/// diagnosed; it is disclosed so a re-runner who counts 21 is not misled by a "20" here, and it is
/// NOT counted as evidence for this constant.)
///
/// **What pins this number in CI.** Dropping the constant to `6` — one ULP below the measured
/// cliff, i.e. a value known to let real bodies through real floors — is RUN, and
/// `-p eqoxide-nav --lib` goes **RED**: `16/1369` columns, worst blind band `3.1069e-6`, at the
/// last case of `tests::a_floor_z_the_module_just_reported_always_has_a_floor_under_it`. That case
/// is the pin described below under "**A narrower, CI-runnable pin does exist now**", and it is
/// the only thing in a DEFAULT run standing between this constant and a silent lowering (the
/// `#[ignore]`d corpus test also catches it, but nothing runs it) — MEASURED, not inferred:
/// at `6`, `cargo test --workspace --lib --no-fail-fast` fails in `eqoxide-nav` and **nowhere
/// else** — 13 crates run a lib suite, the other 12 are green. Wrapping that one case in
/// `if false` at `6` returns the whole suite to GREEN, so it is the case doing the catching and
/// not a pre-existing assertion.
///
/// LIMITS on that RED, so it is not over-read. The pin is the last case in
/// `tests::a_floor_z_the_module_just_reported_always_has_a_floor_under_it`, a synthetic quad
/// (`h = 10`) sampled over a tighter span than the REGIMES fixtures above; **its own cliff sits
/// between `6` and `12`, not at this doc's corpus cliff of `7`**, so it pins only that the shipped
/// `32` is not order-of-magnitude wrong (~2.7× margin over its own cliff). And the catch is local
/// to this crate: `-p eqoxide --lib` is still GREEN at `6`, so the root crate's suite does not
/// guard this constant and never did (#876 review). No commit on `main` has carried the constant
/// without the pin — `git log -S` for both returns exactly `acd0743`.
///
/// **This constant bounds the blind band; it does not close it.** #866 round-2 review densely
/// sampled 94 baked zones near the world origin (599,875 columns within ±24 u of `(0,0)`) and
/// measured 108 residual self-consistency misses at the shipped `32` — down from 582,089 on
/// `main` (a 5390× reduction, not a closure). The mechanism: `contact_tol`'s budget is set by the
/// *result* point and the triangle's vertices, but the reconstruction error it has to cover is set
/// by the floor query's own start height and range (`ground_below`'s `start`/`range` arguments),
/// which `contact_tol` never sees. Concretely, the same tilted quad this doc's cliff table used,
/// queried with a start/range far from the fixture's own `60.0`/`400.0`, misses far more at the
/// shipped `32` (up to 1045/1369 at `600`/`4000`) — so `ground_below`'s call-site parameters are
/// load-bearing for the invariant this constant is asked to guarantee, and that envelope was
/// previously unstated. Tracked as a follow-up: #875.
///
/// **Why the high side has room to spare.** The two failure directions are not symmetric. Too
/// small = a body walks through the world and nothing says so — the silent-wrong-answer class this
/// repo ranks above crashes. Too large = a face slightly behind the ray reports as touched, which
/// every caller here resolves conservatively (`slide` advances 0, `swim_sink` clamps to 0,
/// depenetration recovers). So margin is spent upward. At the corpus's largest ray-origin
/// coordinate (`everfrost`'s `−4467`), `32` ULPs is `1.7e-2` world units: about a third of `SKIN`
/// (0.05) and 1.7% of `PLAYER_RADIUS` (1.0), so no caller's own back-off is displaced. (The scale a
/// hit is judged against also spans the triangle, so it can exceed the ray's own coordinate; the
/// bound stays the same order.)
pub const CONTACT_TOL_ULPS: f32 = 32.0;

/// Largest absolute coordinate in a point — the magnitude that sets `f32` resolution near it.
#[inline]
pub fn axis_scale(p: [f32; 3]) -> f32 { p[0].abs().max(p[1].abs()).max(p[2].abs()) }

/// The world-unit slack a ray allows *behind* its own origin before a face stops counting as
/// touched: `CONTACT_TOL_ULPS` ULPs of `scale`, the largest coordinate involved in the test (ray
/// origin AND triangle vertices — see the call sites).
///
/// **Why any slack is needed, MEASURED — and it is not `f32` cancellation in `tvec = from − v0`
/// scaling with the world coordinate**, which round-1 review proposed: an axis-aligned synthetic
/// quad at `tox`'s `(2081, 2320, −87)` measures a blind band of `0` even with this slack removed.
/// The real driver is one step earlier, and it fires at the origin too.
///
/// A floor query does not return a coordinate from the mesh — it returns a *reconstructed* one,
/// `gather_top + t·dir_z` (`column_hits`). That `f32` does not generally lie on the triangle's
/// plane; it lands an ULP or two either side. When it lands **below**, a descent ray starting there
/// begins inside the solid and Möller–Trumbore correctly reports the face at a small NEGATIVE `t` —
/// which a lower bound of exactly `0` throws away. Production does exactly this: `movement.rs`
/// assigns `self.pos[2]` straight from a floor query at the ground-snap and depenetration arms.
///
/// Measured on a tilted quad, sampling 1369 columns, "floor-z samples that then report no floor
/// below" and the worst offset needed to see it again:
///
/// | fixture | round-1 (`t ≥ 0`) | shipped |
/// |---|---|---|
/// | quad at the origin | 958/1369, band 1.419e-5 | 0/1369 |
/// | quad at `tox` coords | 882/1369, band 1.907e-5 | 0/1369 |
/// | quad at `everfrost` coords | 623/1369, band 2.289e-5 | 0/1369 |
///
/// The band grows only mildly with the coordinate; the *count* is large everywhere. So this is a
/// reconstruction-resolution problem that needs a world-unit tolerance, and the tolerance has to
/// scale with the largest coordinate in the whole test — a ray at `(0,0,0)` querying a triangle
/// whose vertices are 50 u away needs 50 u worth of resolution, not 1. Scaling on the ray origin
/// alone left 21/1369 of the origin fixture still missing, measured.
///
/// `max(1.0)` floors the scale so geometry hugging the origin still gets a nonzero bound.
#[inline]
pub fn contact_tol(scale: f32) -> f32 {
    CONTACT_TOL_ULPS * f32::EPSILON * scale.max(1.0)
}

/// **THE ONE RAY-HIT ACCEPTANCE TEST (#855).** A Möller–Trumbore `t` on a ray of world length `len`
/// starting at `from` counts as a hit iff this says so. All three ray scans in this module —
/// [`Collision::nearest_hit_t`], [`Collision::nearest_hit`], [`Collision::column_hits`] — call it.
///
/// **What it replaced.** `column_hits` accepted `t ∈ [0,1]`; the two `nearest_hit*` scans accepted
/// `t > 1e-3`. That `1e-3` is a fraction of a *caller-chosen* ray length, so it is a blind band of
/// `1e-3 × ray length` in each caller's own units — measured exactly (ratio `1.000e-3` at ray
/// lengths 0.56 / 2.5 / 100), and measured to make the two scans disagree: over a floor at z=0, a
/// ray from the face down 2.5 returns `None` from `nearest_hit_t` while `column_surfaces` reports
/// the face twice. `CharacterController::swim_sink` casts a `35·dt` ray, so a swimmer whose feet sat
/// inside that band read "open water below" and descended THROUGH the pool floor — reached with no
/// synthetic placement, and knife-edge on `dt` (#855).
///
/// **Why the lower bound is in world units and the upper bound is not.** The two ends are not the
/// same problem. The lower end is a *contact* test, where `from` and the face coincide and `f32`
/// cancellation is at its worst — so it is expressed as a world distance behind the origin
/// ([`contact_tol`]), which is the only form that means the same thing at `(0,0,0)` and at
/// `(2081, 2320, −87)`. The upper end is a *range* test against the caller's own endpoint, where
/// no cancellation occurs and `t ≤ 1.0` is exact. **Stated residual:** a face lying within
/// `contact_tol` *beyond* `to` is still rejected. That is left deliberately: no measurement shows
/// it biting, and the failure is self-correcting for the callers here — a body that steps exactly
/// onto such a face arrives at distance ~0 from it, where the lower bound catches it on the next
/// frame. Pinned by `a_face_just_past_the_far_end_is_caught_on_the_next_frame`.
///
/// **Why no positive epsilon (the thing the old `1e-3` was mistaken for).** The band reads like a
/// self-intersection guard for a ray starting ON a face, but the coplanar case is already rejected
/// one test earlier: a ray lying IN a triangle's plane is parallel to it, so `det.abs() < eps` drops
/// it. A ray that starts on a face and heads INTO it yields `t = 0`, a true zero-distance contact —
/// and every caller carries its own back-off (`SKIN`, `Body::radius`) that turns `t = 0` into "no
/// progress", not into a negative move. Accepting it is what makes contact detectable instead of
/// silently absent.
///
/// **The tie this cannot break**, measured and deliberate: a face at distance ~zero reports for a
/// ray heading EITHER way off it — Möller–Trumbore returns `t = -0.0` for the away direction,
/// `-0.0 == 0.0` in IEEE, and the world-unit slack widens that tie from a point to `contact_tol`.
/// No lower bound of any kind separates the two sides at the point of contact; a *larger* bound
/// than this would only make the tie wider. It resolves to "you are touching it", the conservative
/// side for every caller here. Pinned by
/// `only_the_coplanar_case_is_rejected_by_the_parallel_test_the_rest_is_accepted_contact`.
#[inline]
pub fn hit_accepted(t: f32, len: f32, scale: f32) -> bool {
    t <= 1.0 && t * len >= -contact_tol(scale)
}

/// Shortest ray any scan in this module will answer, in world units. Below it the direction vector
/// is numerically useless and every scan returns "nothing".
///
/// **One value, shared, for a reason (#855 round-2 finding 3).** The three scans used to carry three
/// *different* degenerate-ray guards — `nearest_hit_t` rejected `|dir|² < 1e-6`
/// (i.e. `|dir| < 1e-3`); `nearest_hit` rejected `|dir|² < 1e-9` (`|dir| < 3.16e-5`);
/// `column_hits` rejected `|dir_z| < 1e-6` (linear, not squared). Measured consequence, over `one_floor()` with a face
/// squarely at the segment midpoint: at segment lengths `1e-4`, `5e-4` and `9.9e-4`, `nearest_hit`
/// answered `Some(0.5)` while `nearest_hit_t` answered `None`. Unifying the `t` test alone did not
/// stop the module disagreeing with itself; this is the other half. Pinned by
/// `the_three_scans_agree_on_short_segments`.
pub const MIN_RAY_LEN: f32 = 1e-6;

/// The steepest grade (rise/run) A* accepts on a walk edge — walkable up to ~50°; steeper makes the
/// controller slide on the face and wedge (#205, eqoxide#212). Was a const local to `astar`; hoisted
/// to module scope so the #630 profile check (`walk_profile_ok`) shares the same number.
pub const MAX_WALK_GRADE: f32 = 1.2;

// ── The CONTROLLER's placement geometry (#885) ───────────────────────────────────────────────────
// These three were private consts in `src/movement.rs`. They are here — with `movement` importing
// them back under their old names, so that module is byte-for-byte unchanged in behaviour — because
// [`Collision::body_placement`] must be the ONE definition of "can a body be placed here", read by
// the controller AND by the published clearance probe. A second copy of the numbers is exactly how
// the diagnostic came to report "open in every direction" for a body the controller had frozen.

/// Ground-probe origin above the feet.
pub const GROUND_ORIGIN: f32 = 1.0;
/// Ground-probe downward range, measured from [`GROUND_ORIGIN`] — **not** from the feet
/// themselves. Prose describing "no floor within N u below the feet" wants
/// [`GROUND_REACH_BELOW_FEET`], not this constant directly; citing this one overstates the
/// feet-relative reach by [`GROUND_ORIGIN`] (that overstatement was a live doc bug, #936).
pub const GROUND_DEPTH: f32 = 200.0;
/// How far below a body's actual feet the ground probe can still find a floor: [`GROUND_DEPTH`]
/// measured from [`GROUND_ORIGIN`] above the feet, so the feet-relative reach is
/// `GROUND_DEPTH - GROUND_ORIGIN`. This is the number every "no floor within N u below the feet"
/// doc/prose site should cite.
pub const GROUND_REACH_BELOW_FEET: f32 = GROUND_DEPTH - GROUND_ORIGIN;
/// Footprint-ring directions the placement test samples. This is the controller's historical
/// `PUSHOUT_DIRS / 2`; `movement` static-asserts that identity so the two cannot drift.
pub const PLACEMENT_RING_DIRS: usize = 8;

/// Plan cell size at or below which A* validates an edge by sweeping the character's whole
/// collision volume instead of casting a centre ray — see `Collision::edge_clear` for the measured
/// reason this is not simply "always". Sits above `nav::steering::LOCAL_CELL` (2u, the fine tier) and
/// below the coarse whole-zone grid (8u).
pub const SWEPT_EDGE_MAX_CELL: f32 = 4.0;

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

// ─────────────────────────── the static clearance field (design §3) ───────────────────────────

/// Memo-key lattice: clearances are computed at the centres of a fixed 2 u XY grid (the fine
/// tier's cell) with 2 u floor buckets (A*'s own `qf` quantum). A query is answered for the key
/// cell its point falls in.
const FIELD_CELL: f32 = 2.0;
/// Storage quantum for the u8-packed distances (units per count). Saturates at 63.75 u.
const FIELD_QUANTUM: f32 = 0.25;
/// How far the WALL spokes look. Everything at/above this reads as "roomy": the largest wall
/// threshold anywhere is `Tier::Preferred` (2.0), and the hug cost fades out there too, so a
/// 4 u horizon leaves headroom without paying for long rays.
const WALL_CAP: f32 = 4.0;
/// How far the GROUND probe looks. The largest ground (ledge) margin is `Tier::Preferred` (2.0).
const GROUND_CAP: f32 = 2.0;
/// Ground probe radii, ascending. The 0.5 rung exists so a lip RIGHT at a waypoint reads as ~0.
const GROUND_RADII: [f32; 4] = [0.5, 1.0, 1.5, 2.0];
/// Bound on each memo map (~24 B/entry ⇒ tens of MB worst case, cleared with the zone). At
/// capacity the field keeps ANSWERING correctly — it just recomputes instead of inserting — so the
/// bound degrades speed, never truth, and never unboundedly grows in a huge zone (gfaydark is
/// 5.9 M columns at 2 u; only VISITED cells ever memoise, but a long session visits a lot).
const FIELD_MAX_ENTRIES: usize = 1 << 20;

/// **The static clearance field (`MemoField`, design §3d): for a standing point, the horizontal
/// distance to the nearest thing you cannot be at.** Two graded distances, not booleans:
///
/// * `wall_at` — distance to the nearest SOLID geometry at the body's probe heights, measured
///   RADIALLY (16 spokes). This is what closes #381's structural hole: `path_clear`'s feelers run
///   parallel to travel and can never see a wall the segment runs alongside; a radial spoke
///   crosses it. Used as a hot COST (the hug penalty — never a hard filter below
///   `Tier::Preferred`, per the design's §9 non-negotiable) and as the generous tier's
///   standing-room threshold.
/// * `ground_at` — distance to the nearest spot where the floor RUNS OUT (a drop, a bridge lip,
///   a waterline): the graded form of the old boolean `ground_margin_ok`, probed on the same four
///   axial directions and the same ±band.
///
/// # Determinism (the #394 discipline)
///
/// A memoised value is a PURE FUNCTION OF ITS KEY: it is always computed at the key cell's centre
/// and bucket floor, never at the querying point — so the answer does not depend on which query
/// happened to populate the cache first, and concurrent workers racing to insert write identical
/// values. The price is quantisation (a query point can sit up to ~1.4 u from its key centre);
/// every consumer of this field is a cost or a ladder-guarded threshold, sized for that error.
#[derive(Default)]
pub struct ClearanceField {
    wall: std::sync::RwLock<std::collections::HashMap<(i64, i64, i32), u8>>,
    ground: std::sync::RwLock<std::collections::HashMap<(i64, i64, i32), u8>>,
    /// Entry cap per map (tests shrink it to prove the degrade-not-grow behaviour).
    cap: std::sync::atomic::AtomicUsize,
}

impl ClearanceField {
    fn key(x: f32, y: f32, floor_z: f32) -> (i64, i64, i32) {
        ((x / FIELD_CELL).floor() as i64,
         (y / FIELD_CELL).floor() as i64,
         (floor_z / 2.0).round() as i32)
    }
    fn key_centre(k: (i64, i64, i32)) -> [f32; 3] {
        [(k.0 as f32 + 0.5) * FIELD_CELL, (k.1 as f32 + 0.5) * FIELD_CELL, k.2 as f32 * 2.0]
    }
    fn cap(&self) -> usize {
        match self.cap.load(std::sync::atomic::Ordering::Relaxed) {
            0 => FIELD_MAX_ENTRIES,
            n => n,
        }
    }
    /// Only called from test code — gated to match, else it reads as dead code outside `cargo
    /// test`. `#[cfg(any(test, feature = "test-fixtures"))]`, not plain `#[cfg(test)]`, so this
    /// block's crate-boundary move to `eqoxide-zone-geometry` still lets `eqoxide-nav`'s own test
    /// build reach it (cfg(test) alone only holds while THIS crate is under test).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn set_cap_for_test(&self, n: usize) {
        self.cap.store(n, std::sync::atomic::Ordering::Relaxed);
    }

    fn cached(map: &std::sync::RwLock<std::collections::HashMap<(i64, i64, i32), u8>>,
              k: (i64, i64, i32)) -> Option<f32> {
        map.read().ok()?.get(&k).map(|&q| q as f32 * FIELD_QUANTUM)
    }
    fn store(&self, map: &std::sync::RwLock<std::collections::HashMap<(i64, i64, i32), u8>>,
             k: (i64, i64, i32), v: f32) -> f32 {
        let q = ((v / FIELD_QUANTUM).round() as i64).clamp(0, u8::MAX as i64) as u8;
        if let Ok(mut m) = map.write() {
            // At capacity: answer correctly, just don't grow. Purity of compute-from-key makes the
            // recompute identical to what the entry would have held.
            if m.len() < self.cap() || m.contains_key(&k) {
                m.insert(k, q);
            }
        }
        q as f32 * FIELD_QUANTUM
    }

    /// Radial distance from the (key cell of) `(x, y, floor_z)` to the nearest solid geometry at
    /// the body's planner probe heights, saturating at [`WALL_CAP`].
    pub fn wall_at(&self, col: &Collision, x: f32, y: f32, floor_z: f32) -> f32 {
        let k = Self::key(x, y, floor_z);
        if let Some(v) = Self::cached(&self.wall, k) { return v; }
        let c = Self::key_centre(k);
        let mut best = WALL_CAP;
        const SPOKES: usize = 16;
        for i in 0..SPOKES {
            let a = (i as f32) / (SPOKES as f32) * std::f32::consts::TAU;
            let (dx, dy) = (a.cos(), a.sin());
            for hz in crate::body::PLAYER_BODY.planner_probes() {
                let from = [c[0], c[1], c[2] + hz];
                let to = [c[0] + dx * WALL_CAP, c[1] + dy * WALL_CAP, c[2] + hz];
                if let Some(t) = col.nearest_hit_t(from, to) {
                    best = best.min(t * WALL_CAP);
                }
            }
        }
        self.store(&self.wall, k, best)
    }

    /// Distance from the (key cell of) `(x, y, floor_z)` to the nearest missing-floor direction —
    /// the graded `ground_margin_ok`: the first radius (of [`GROUND_RADII`], on the four axial
    /// directions) with no floor in the ±band, saturating at [`GROUND_CAP`].
    pub fn ground_at(&self, col: &Collision, x: f32, y: f32, floor_z: f32) -> f32 {
        let k = Self::key(x, y, floor_z);
        if let Some(v) = Self::cached(&self.ground, k) { return v; }
        let c = Self::key_centre(k);
        let mut clear = GROUND_CAP;
        'radii: for (i, &r) in GROUND_RADII.iter().enumerate() {
            for (dx, dy) in [(r, 0.0), (-r, 0.0), (0.0, r), (0.0, -r)] {
                let ok = col.nearest_floor(c[0] + dx, c[1] + dy, c[2], 3.0, 8.0)
                    .is_some_and(|f| (f - c[2]).abs() <= 8.0);
                if !ok {
                    clear = if i == 0 { 0.0 } else { GROUND_RADII[i - 1] };
                    break 'radii;
                }
            }
        }
        self.store(&self.ground, k, clear)
    }
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
                z_max: 0.0, facing_blind_surfaces: Default::default(), tight_plans: Default::default(),
                clearance: Default::default(), water_grid: None,
                water: Err(eqoxide_core::region_map::RegionDataAbsent::NotAttached),
                from_collision_mesh, zone_line_regions: Vec::new(),
                climb_volumes: Vec::new(), climb_plans: Default::default() };
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
        let col = Collision { water_grid_lazy: std::sync::OnceLock::new(), tris, tri_nz, cells, origin: min, cell_size, cols, rows,
            #[cfg(any(test, feature = "test-fixtures"))]
            z_min,
            z_max,
            facing_blind_surfaces: Default::default(), tight_plans: Default::default(),
            water: Err(eqoxide_core::region_map::RegionDataAbsent::NotAttached),
            from_collision_mesh, zone_line_regions: Vec::new(),
            climb_volumes: crate::climb::volumes_from_objects(&assets.objects),
            climb_plans: Default::default(),
            clearance: Default::default(), water_grid: None };
        col
    }

    /// Every climbable surface in this zone — the PHYSICAL question (#309). See `climb_volumes`.
    pub fn climb_volumes(&self) -> &[crate::climb::ClimbVolume] { &self.climb_volumes }

    /// Is `p` (`[east, north, up]`) on a climbable surface? The movement controller's test: it asks
    /// about `climb_volumes`, NOT `climb_edges`, because whether a ladder leads anywhere standable
    /// is a routing verdict and has no business vetoing what the character's body can do.
    pub fn on_climbable(&self, p: [f32; 3]) -> bool {
        self.climb_volumes.iter().any(|v| v.contains(p))
    }

    /// Routes that used a climb edge, surfaced to agents as `nav_climb` (#309).
    ///
    /// Counted because the climb MECHANISM is unverified — the native client demonstrably climbs
    /// these ladders, but how it decides to is not known from the decompile (see [`crate::climb`]).
    /// An agent handed a route that depends on a reconstruction rather than on measured client
    /// behaviour must be able to see that, exactly as `nav_tight` discloses a minimum-clearance
    /// route. Never cleared; a non-zero count is informational, not a failure.
    pub fn climb_plans(&self) -> u64 {
        self.climb_plans.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Routes that only existed at the MINIMUM clearance, surfaced as `nav_tight` — see the
    /// `tight_plans` field doc.
    pub fn tight_plans(&self) -> u64 {
        self.tight_plans.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// DOWN-facing (inverted-art) TRIANGLES admitted as standing ground since zone load — the
    /// `nav_support` honesty signal (D-2, #375), published as `nav_support.surfaces`. Zero = every
    /// standable surface so far faced UP (properly wound); non-zero = this zone's ground is partly
    /// inverted art and pathing there is on winding-blind (unverified-facing) ground.
    ///
    /// **Per TRIANGLE, per call — not per probe** (#960/#973), so it is a total and not a rate; a
    /// single call over a densely tessellated column adds several. Pinned with both literals by
    /// `evaluating_both_disjuncts_moves_the_published_facing_blind_counter`.
    pub fn facing_blind_surfaces(&self) -> u64 {
        self.facing_blind_surfaces.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// **The one writer of this grid's region data**: attach the zone's `.wtr` map, or record WHY
    /// there is none. Call after `build`.
    ///
    /// Takes the loader's own `Result` (`RegionMap::try_load`) rather than an `Option` on purpose
    /// (#803). The production caller no longer *has* a way to drop the reason on the floor without
    /// writing the discard out by hand — `RegionMap::load`, the wrapper that used to do it for you,
    /// is deleted.
    pub fn set_region_data(
        &mut self,
        data: Result<std::sync::Arc<eqoxide_core::region_map::RegionMap>,
                     eqoxide_core::region_map::RegionLoadError>,
    ) {
        self.water = data.map_err(eqoxide_core::region_map::RegionDataAbsent::LoadFailed);
        // Precompute zone-line region points now (zone load, off the net thread) so the runtime
        // find_zone_line_near is an O(1) cache read (#204).
        self.zone_line_regions = self.precompute_zone_line_regions();
    }

    /// Fixture shorthand for synthetic scenes: attach a hand-authored region map, or (with `None`)
    /// leave the grid with no region data at all.
    ///
    /// **Test-only, and gated so on purpose.** For a synthetic scene there is no `.wtr` and so no
    /// load that could have failed — `None` genuinely means [`RegionDataAbsent::NotAttached`], which
    /// is still an explicit absence, not an empty answer. Because this is
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

    /// This grid's region map when it has one — the read path for every water/zone-line query below.
    /// `None` here is only ever consumed by a query whose "no data ⇒ no water here" answer is
    /// *physically* the safe one; anything an agent reads back as a fact must go through
    /// [`Collision::zone_line_indices`] (or `region_data_absent`) and surface the reason instead.
    pub fn region_map(&self) -> Option<&std::sync::Arc<eqoxide_core::region_map::RegionMap>> {
        self.water.as_ref().ok()
    }

    /// Why this grid has no region data — `None` when it has some. The value an agent-facing
    /// endpoint refuses with instead of publishing an empty answer (#803).
    pub fn region_data_absent(&self) -> Option<&eqoxide_core::region_map::RegionDataAbsent> {
        self.water.as_ref().err()
    }

    /// Build the zone-line region cache from the water map. Bounded + timed; warns if it runs long
    /// (nav prep should never be slow enough to matter next to keepalive). Empty when there's no
    /// water map / no zone-line regions.
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

    /// Store a prebuilt water-span grid on this collision (3D-water-volume nav design §5, Slice 1).
    /// Purely additive: nothing in the search/walker reads it in Slice 1.
    pub fn set_water_grid(&mut self, grid: Option<crate::water_grid::WaterGrid>) {
        self.water_grid = grid;
    }

    /// The stored water-span grid, if one has been built (`None` until `set_water_grid`).
    pub fn water_grid(&self) -> Option<&crate::water_grid::WaterGrid> {
        self.water_grid.as_ref()
    }

    /// The water-span grid the SEARCH consults (Slice 2) — an explicitly-set grid if one exists,
    /// otherwise the LAZILY-built one (owner decision #5: build on first water plan, not at zone
    /// load). Returns `None` for a zone with no `.wtr` water map at all, WITHOUT building — so a dry
    /// zone pays exactly nothing and its A* is byte-for-byte unchanged.
    ///
    /// The first call in a water zone runs [`Self::build_water_grid`] (the measured 357 ms – 1 s
    /// cost) on whatever thread the plan runs on — which is off the network thread for both the
    /// coarse (#377) and fine (#382) tiers — and caches it; every later call is a pointer read. The
    /// build is gated on `self.region_map().is_some()` *before* `get_or_init`, so a grid is never cached for
    /// a zone whose water map has not been attached yet.
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

    /// Build the sparse **water-span grid** (3D-water-volume nav design §5.3) for this zone, from the
    /// attached `.wtr` water map + this collision mesh. Returns an empty grid when the zone has no
    /// water map. **Does not store or mutate anything** (call `set_water_grid` to keep the result) and
    /// is NOT wired into `find_path`/`astar` — Slice 1 is purely the representation + its cost.
    ///
    /// Method (design §5.3): enumerate water-leaf AABBs from the BSP once (`water_region_aabbs`),
    /// iterate the 4u XY column lattice (aligned to this grid's `origin`) inside those AABBs, and for
    /// each candidate column build its navigable spans with [`Self::build_water_column`]. Candidate
    /// columns are de-duplicated (overlapping AABBs) but each is scanned over the water leaves' z-band
    /// so stacked water (a pool over a separate flooded tunnel — the qcat case) yields multiple spans.
    pub fn build_water_grid(&self, body: &crate::body::Body) -> crate::water_grid::WaterGrid {
        const COL: f32 = 4.0; // 4u XY water columns (design §5.1 / owner decision #2, locked)
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

        // Candidate columns from every water AABB (deduped). A loose (oblique) AABB only widens the
        // candidate set; `build_water_column`'s `is_water` probes make that safe (design §5.3).
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

    /// Build one 4u column's navigable spans (design §5.1 interval arithmetic), or report why it has
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
    /// geometry), so no span is emitted there (#534: discarding `nz` reported a node inside solid
    /// rock as navigable — an agent-honesty defect). `UnboundedBelow` when a wet probe's water has
    /// no bottom (`bottom_z` == None) — surfaced by the caller as a design-premise signal, never
    /// fabricated into a floor (design §5.2 STOP condition).
    fn build_water_column(&self, water: &eqoxide_core::region_map::RegionMap,
        body: &crate::body::Body, e: f32, n: f32, zrange: (f32, f32)) -> WaterColumnResult {
        use crate::water_grid::{VRES, WaterColumn};
        const EPS: f32 = 0.05;
        // = `movement::SKIN` (0.05, private there); the body-top must clear a solid ceiling by this,
        // exactly as the collided `swim_rise` stops `SKIN` short of the ceiling (design §5.1).
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
                    (Some(_), None) => unbounded = true, // water with no bottom (design §5.2)
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
            // no span (#534, design §5.1/§5.2). BOTH windings must agree before we suppress — a mere
            // OR would delete genuine water under EQ's inverted-art normals (the #375 hazard: a
            // down-facing surface can be a walkable inverted floor, not a ceiling).
            let solids = self.column_surfaces(e, n, mid, ws - mid + 1.0, mid - wb + 1.0);
            // Partition by position. A solid FLUSH with a water bound is NOT dropped (that would
            // replace its real winding with the open-water sentinel and let the enclosed-solid test
            // misfire at the band edge — #534 review); instead its winding is FOLDED into that
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
            // remaining span is then the #375-accepted inverted-art ambiguity, handled at the
            // reachability layer, not fabricated here.
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

    /// If `pos` = [east, north, z] (server coords) lies in a zone-line (`DRNTP`) region, the
    /// zone-point index it carries — the `OP_SendZonepoints` `iterator` for that line, used to
    /// resolve the destination zone. `None` when not on a zone line, no region map, or a v1 map.
    /// This is how the native client triggers a crossing: it detects the region from zone geometry
    /// rather than a coordinate list.
    pub fn zone_line_at(&self, pos: [f32; 3]) -> Option<i32> {
        self.region_map().and_then(|w| w.zone_line_at(pos[0], pos[1], pos[2]))
    }

    /// The zone-line index a **standing** character occupies at `(east, north, feet_z)`, probing the
    /// character's vertical body span `[feet_z, feet_z + PLAYER_BODY.height]` rather than the single
    /// feet point [`zone_line_at`] tests.
    ///
    /// A DRNTP trigger volume whose lower face floats just ABOVE the walkable floor is invisible to a
    /// feet-only probe: the character stands with its feet *below* the volume, yet its body clearly
    /// occupies it. The qeynos2 Knights-of-Truth waterfall (#266) is exactly this — the trigger's
    /// lower face sits ≈0.4u over the flat vault floor (feet rest at z≈-14.0; the volume starts
    /// ≈-13.6), so a character standing on the disclosed footprint is never detected by
    /// `zone_line_at([x,y,-14.0])` and only a JUMP (which lifts the feet into the volume) crossed.
    /// Sweeping the capsule span makes the auto-cross fire wherever the body intersects the trigger.
    ///
    /// This is the probe the auto-cross MOVER (`eqoxide-net action_loop`) uses, and it AGREES with
    /// the footprint validator [`Self::teleport_pad_source`] (which validates a floor at feet+1,
    /// inside the volume): any footprint the validator accepts is detected here, because feet+1 lies
    /// within this span. Keeping the two aligned is the fix for #266's feet-vs-feet+1 divergence.
    ///
    /// **Agent-initiated only, never auto-routed.** This makes a crossing physically fire when the
    /// agent has *driven* the character onto the disclosed footprint; it does NOT relax the
    /// [`crate::walker::TRUST_ADVERTISED_SAME_ZONE_CROSSINGS`] `= false` rule, which still forbids the
    /// walker from *planning* a route through an unverifiable same-zone pad (#543/#660).
    ///
    /// Returns the first (lowest) index found scanning feet→head. Point-samples at ≤1u spacing; a
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

    /// Distinct zone-point indices of every zone-line region in this zone — **the set of exits**.
    /// Each links to an entrance via the `OP_SendZonepoints` `iterator`.
    ///
    /// `Ok(vec![])` is a real reading: this zone's region map loaded and contains no zone-line
    /// regions. `Err` means the question could not be answered at all, and names why.
    ///
    /// **A v1 map is NOT an exception to that** (#821 review round 2, B1 — an earlier revision of
    /// this sentence said it was). v1 records carry no `zone_line_index` field, so every v1 zone
    /// line reads as index **0**, and index 0 is *included*: this returns `[0]`, not `[]`. That is
    /// the whole point of #683 — gating recognition on a nonzero index locked a character in qrg
    /// forever. See `RegionMap::zone_line_indices` and
    /// `a_zero_index_zone_line_region_is_still_recognized_683`.
    ///
    /// **The `Result` is the fix for #803, and it is here rather than in a caller-side guard because
    /// the guard is what kept failing.** This used to be `Vec<i32>` with `.unwrap_or_default()`, so a
    /// `.wtr` that did not load produced an empty vec that `/v1/observe/zone_exits` published as `[]`
    /// with 200 OK — byte-identical to the true, common answer for a zone with no zone lines. Exits
    /// are the only way out of a zone, so the agent concluded it was sealed in, from a success
    /// response it had no way to doubt. Now the two cases have different TYPES, and every caller has
    /// to say out loud what it does with the second.
    pub fn zone_line_indices(&self)
        -> Result<Vec<i32>, eqoxide_core::region_map::RegionDataAbsent>
    {
        match self.water.as_ref() {
            Ok(w) => Ok(w.zone_line_indices()),
            Err(absent) => Err(absent.clone()),
        }
    }

    /// Find a point inside a zone-line region nearest to `near` (= [east, north, z]), returning
    /// `(region_index, point)`. When `index` is `Some`, only that zone-point index matches; `None`
    /// matches any zone line. Used by the explicit `/zone_cross` API to walk the character onto the
    /// zone line, where the auto-cross then fires. `None` if the zone has no region map or no
    /// matching region is found within the search radius.
    ///
    /// O(regions) cache read — NO scan. The region points are precomputed once at zone load
    /// (`set_water`), so this is safe to call on the network thread; the old per-request expanding
    /// ring × z scan did up to ~10^8 BSP walks synchronously and force-linkdead-ed the client when a
    /// line was far away or missing from the `.wtr` (#204). `index` filters to a destination zone's
    /// line (`None` = any); returns `(index, [east, north, z])` of the region nearest `near`.
    /// The returned point is projected onto the WALKABLE FLOOR beneath the region (#229). A region
    /// point's z is an interior point of the region VOLUME — across the shipped zones it sits 1.5u
    /// to 127u ABOVE the real floor, and it is structurally never a floor height. Navigating to it
    /// verbatim gave A* an unreachable goal tier, so the search could never accept arrival: it
    /// flooded the grid, hit the time cap, and returned a greedy partial route that wedged into a
    /// wall. (halas is the one line whose region z is nearly floor-level — and it is the one line
    /// that always worked.)
    ///
    /// The projection is only taken when the region STILL CONTAINS the projected floor point, so a
    /// tall vertical translocator whose footprint doesn't reach the ground (#266) is never dragged
    /// down off its trigger volume — such a candidate keeps its original z. Candidates are tried
    /// nearest-first, preferring one that projects (a region point hanging over a void is skipped).
    pub fn find_zone_line_near(&self, index: Option<i32>, near: [f32; 3]) -> Option<(i32, [f32; 3])> {
        // Rank by distance to the PROJECTED (standable) point, not the raw region point — and weight
        // vertical separation far above horizontal. A zone line is often baked as several stacked
        // DRNTP leaves over more than one XY; gfaydark's butcher line has a leaf whose only floor is
        // an isolated 74u-high ledge and another that sits on the ground. Ranking on raw 3D distance
        // picked the ledge (it was 28u closer in XY) and sent the char climbing to an unreachable
        // perch. Climbing 74u is not "cheaper" than walking 28u further, so make the cost say so.
        const Z_WEIGHT: f32 = 4.0;
        let cost = |p: &[f32; 3]| (p[0] - near[0]).hypot(p[1] - near[1]) + Z_WEIGHT * (p[2] - near[2]).abs();
        let best_projected = self.zone_line_regions
            .iter()
            .filter(|(idx, _)| index.is_none_or(|want| want == *idx))
            .filter_map(|&(idx, p)| self.zone_line_floor_point(idx, p).map(|fp| (idx, fp)))
            .min_by(|a, b| cost(&a.1).total_cmp(&cost(&b.1)));
        best_projected.or_else(|| self.zone_line_regions // nothing projects (no floor under any) —
            .iter()                                      // keep the old raw-region-point behaviour
            .filter(|(idx, _)| index.is_none_or(|want| want == *idx))
            .min_by(|a, b| cost(&a.1).total_cmp(&cost(&b.1)))
            .copied())
    }

    /// Project a zone-line region's representative point down onto the walkable floor beneath it.
    /// `None` when there is no floor under it, or when the region does NOT extend down to that floor
    /// (so standing there would not trigger the crossing — see `find_reachable_in_zone_line`, #266).
    fn zone_line_floor_point(&self, index: i32, p: [f32; 3]) -> Option<[f32; 3]> {
        const REGION_DROP: f32 = 400.0;
        let fz = self.floor_beneath(p[0], p[1], p[2], 2.0, REGION_DROP)?;
        // Standing on that floor must still be INSIDE the region — else we'd walk the char to a spot
        // that never fires the auto-cross.
        let inside = self.region_map()
            .and_then(|w| w.zone_line_at(p[0], p[1], fz + 1.0)) == Some(index);
        inside.then_some([p[0], p[1], fz])
    }

    /// Nearest zone-line region whose index is in `in_zone_idxs` (a same-zone destination — an escape
    /// translocator, not a normal neighbour-zone exit) AND whose `DRNTP` footprint is present at the
    /// caller's floor height `near[2]` under the region's XY.
    ///
    /// The auto-cross (`zone_line_at`) is a z-EXACT BSP test. A tall vertical translocator (the Qeynos
    /// guild-vault waterfall) bakes several DRNTP leaves up its column; a leaf near the TOP yields an
    /// interior point high up, and a char that walks to that XY on the vault floor stands BELOW the
    /// leaf and never triggers — so the escape stalls at the portal without teleporting (#266). We keep
    /// only regions still present at the char's own height (`zone_line_at([x, y, near_z]) == idx`), i.e.
    /// whose footprint reaches the floor the char is standing on, so walking there fires the cross.
    /// `None` if no such reachable in-zone portal exists.
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
    /// primitive behind `nearest_floor` / `column_floors`.
    pub fn column_surfaces(&self, east: f32, north: f32, ref_z: f32, up: f32, down: f32) -> Vec<(f32, f32)> {
        let mut hits = Vec::new();
        self.column_hits(ColumnQuery { east, north, ref_z, up, down, floors_only: false }, &mut hits);
        hits
    }

    /// Shared vertical-column raycast (Möller–Trumbore). Appends `(hit_z, face_normal_z)` to `out`,
    /// sorted high→low.
    ///
    /// When `floors_only`, returns only **standable** surfaces (`is_standable`, D-2 / #375): FACING-
    /// BLIND, a surface is ground iff `|nz| >= NAV_NEAR_HORIZONTAL` AND it has `NAV_AGENT_HEIGHT` of
    /// open space above it before the next SOLID surface. This replaced the old winding-sign filter
    /// (`nz <= 0`), which deleted real inverted-art floor the character stands on (the qcat live wedge)
    /// and needed a `column_bottom` recovery valve (also removed). The ceiling defence is now HEADROOM
    /// (a ceiling has its roof right above it → fails) plus the caller's `ref_z ± window` (a far roof,
    /// e.g. qcat's 391.8, is simply outside the window). Both `column_hits(true)` (the planner's floor
    /// lookup) and `ground_below` (the walker's clamp) go through this, so the two cannot disagree —
    /// that agreement is the whole point of #375.
    fn column_hits(&self, query: ColumnQuery, out: &mut Vec<(f32, f32)>) {
        let ColumnQuery { east, north, ref_z, up, down, floors_only } = query;
        out.clear();
        if self.cols == 0 { return; }
        let z_top = ref_z + up.max(0.0);
        let z_bot = ref_z - down.max(0.0);
        let filter = floors_only;
        // For `is_standable` we need each surface's headroom = distance UP to the next SOLID surface of
        // EITHER winding, which can lie ABOVE the caller's window — so gather facing-blind from `z_bot`
        // up to the zone top, classify, then return only in-window standable surfaces. Same triangles
        // as the window scan (a cell's list is fixed); only the ray is longer, so cost is ~unchanged.
        let gather_top = if filter { self.z_max.max(z_top) + 1.0 } else { z_top };
        let dir_z = z_bot - gather_top; // negative (downward)
        let ray_len = dir_z.abs();
        if ray_len < MIN_RAY_LEN { return; } // #855: the ONE degenerate-ray guard
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
                    // #855: the ONE acceptance test, shared with `nearest_hit*`. The scale spans the
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
        // THE THREE #329 CASES, and which we defend (owner-signed-off 2026-07-15, #375/#329):
        //   * FAR ROOF (qcat's 391.8 over a −70 floor): DEFENDED by the caller's `ref_z ± window` — a
        //     character never queries at roof height, so the roof is simply out of range.
        //   * CLOSE ROOF (a room ceiling with its roof right above): DEFENDED by `headroom` below — a
        //     solid surface within `NAV_AGENT_HEIGHT` means no standing room.
        //   * OPEN-TOPPED MID-HEIGHT down-facing surface (a flat ceiling with open sky above): KNOWINGLY
        //     ADMITTED as walkable. It is GEOMETRICALLY IDENTICAL to qcat's walkable −42.97 walkway
        //     (down-facing, open above, a floor below), so no per-surface rule can accept the qcat floor
        //     — the whole #375 fix — while rejecting this. The owner accepted the band. If it ever bites
        //     a real zone, the mitigation is at the REACHABILITY layer ("does a route actually lead the
        //     character onto it?"), NOT a per-surface classifier (proven impossible). See the acceptance
        //     test `open_topped_midheight_surface_is_admitted_as_the_accepted_cost_of_facing_blindness`.
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
            // Honesty (#375): admitting a DOWN-facing surface as ground means we answered from
            // winding-blind (inverted-art) geometry the mesh does not confirm is a floor. Count it so
            // `/v1/observe/debug` can surface `nav_support` — a degraded/unverified mode must never be
            // silent. (Correct per qcat, but the agent must be able to SEE it is on such ground.)
            if nz < 0.0 {
                self.facing_blind_surfaces.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            keep.push((z, nz));
        }
        *all = keep;
    }

    /// Find the walkable FLOOR height at `(east, north)` nearest to `ref_z`.
    ///
    /// Casts a vertical column over `[ref_z - down, ref_z + up]`, gathers every UP-FACING triangle
    /// it crosses (a ceiling is not a floor — #329), and returns the one whose height is **closest
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

    /// The highest walkable floor at or below `z` (searching down `down` units, and up `up` units
    /// for a goal that sits a little UNDER the floor it names). This is the "what am I standing
    /// over?" query — used to project a point that lives in a VOLUME (a `DRNTP` zone-line region's
    /// interior point, whose z is a point inside the region solid and is structurally NEVER a floor
    /// height, #229) down onto ground a character can actually stand on.
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
    /// See [`hit_accepted`] for why it used to be `(1e-3, 1]` and what that cost (#855).
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
    /// `zone_assets::ZoneAssetState::ready` requires before it will call a zone loaded (#579).
    ///
    /// **Honest scope:** today this is *equivalent* to [`has_geometry`], because [`Collision::build`]
    /// already returns `cols: 0` whenever `tris` is empty — so switching `ready()` onto it changed
    /// no behaviour. It is stated as its own predicate so the "there is a world here" test does not
    /// silently depend on that internal coupling (`has_geometry` is a *bounds* proxy; this is the
    /// property actually meant), and so a future change to `build` cannot quietly weaken it. It does
    /// NOT reject a grid whose only triangles are degenerate — no cheap check distinguishes those,
    /// and no observed load produces them.
    pub fn has_triangles(&self) -> bool { self.cols != 0 && !self.tris.is_empty() }

    /// Like [`nearest_hit_t`] but also returns the hit triangle's **unit normal**, flipped to
    /// oppose the segment direction (so it faces back toward `from`). Used by [`sweep`] to provide
    /// the slide plane for collide-and-slide. Möller–Trumbore over the broad-phase cells.
    ///
    /// Same [`hit_accepted`] and [`MIN_RAY_LEN`] as [`nearest_hit_t`] and
    /// [`Collision::column_hits`] (#855).
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

    // NOTE: `Collision::sweep` — a swept-cylinder approximation this comment block used to sit
    // above — was DELETED (#378 refactor, PR-1). It had ZERO production callers: the mover
    // (`CharacterController::slide`) implements its own contact resolution and never called it,
    // while `path_clear`'s doc claimed the two shared it. A dead "shared collision model" that
    // nothing shares is exactly the #312 class of lie (a comment documenting a fix broader than the
    // code), so the function is gone; what the planner and the controller ACTUALLY share now is the
    // probe geometry itself — `traversability::PLAYER_BODY` — which both derive their heights from.

    /// The height of the nearest **standable** surface at/below `origin_z` within `depth`, or `None`.
    /// Native ground clamp uses `origin = foot_z + 1.0`, `depth = 200` (design §3.2).
    ///
    /// D-2 (#375): this is the CONTROLLER's floor clamp, and it now goes through the SAME
    /// `is_standable` predicate as the planner's `column_hits(true)` — so the two cannot disagree about
    /// where the floor is (the qcat wedge was exactly that disagreement: the controller's old
    /// facing-blind first-hit stood on the −42.97 inverted-art walkway while the planner's facing
    /// filter deleted it). It was a single facing-blind ray; it is now the highest standable surface in
    /// `[origin_z − depth, origin_z]`. A surface under a ceiling (headroom `< NAV_AGENT_HEIGHT`) is not
    /// standable — the controller no longer clamps to it (measured against route-success + the faithful
    /// walker corpus for the low-clearance seal risk).
    pub fn ground_below(&self, east: f32, north: f32, origin_z: f32, depth: f32) -> Option<f32> {
        if self.cols == 0 { return None; }
        let mut hits = Vec::new();
        self.column_hits(ColumnQuery {
            east, north, ref_z: origin_z, up: 0.0, down: depth.max(0.0), floors_only: true,
        }, &mut hits);
        hits.first().map(|&(z, _)| z) // sorted high→low ⇒ first = highest standable at/below origin_z
    }

    /// **THE PHANTOM-DESCENT GUARD (#693).** Can a body actually FALL from `takeoff_z` down to
    /// `landing_z` in the column at `(east, north)`? A falling body stops at the FIRST solid
    /// surface below its feet — it cannot pass through an intervening floor. So a descent is real
    /// only when NO surface (facing-blind: a slab obstructs a fall regardless of its winding) lies
    /// in the column strictly between the landing and the takeoff's step band.
    ///
    /// This is the tier discriminator the qcat/qeynos live wedge was missing: the qeynos streets
    /// (z≈0) are STACKED directly over the aqueduct level (z≈−28) with no opening at most XY, and
    /// the descent edge families (controlled fall / water descent / water entry) selected the deep
    /// tier as a landing while skipping the street the character was standing on — routing the
    /// walker straight DOWN through solid pavement, which it re-pathed against forever
    /// (`nav_state: blocked` at ~(−502,−103)). Here that column fails the guard (the street itself
    /// is an intervening surface), while a genuine lip / hole / open channel / dock edge passes —
    /// its column holds nothing between takeoff and landing. Geometry-only, water is not an
    /// obstruction; the takeoff band tops out at `step_up` (a surface higher than that above the
    /// takeoff is not in the body's fall path — the lateral move is separately guarded by each
    /// family's own clearance ray).
    ///
    /// `LANDING_TOL` excludes the landing surface itself (and its coincident slab-twin triangle —
    /// same tolerance class as `column_hits`'s `SAME_SURFACE`).
    pub fn descent_corridor_clear(&self, east: f32, north: f32, takeoff_z: f32, landing_z: f32) -> bool {
        const LANDING_TOL: f32 = 0.6;
        let top = takeoff_z + crate::body::PLAYER_BODY.step_up;
        let bottom = landing_z + LANDING_TOL;
        if bottom >= top { return true; } // no gap in which anything could intervene
        let mut hits = Vec::new();
        self.column_hits(ColumnQuery {
            east, north, ref_z: top, up: 0.0, down: top - bottom, floors_only: false,
        }, &mut hits);
        hits.is_empty()
    }

    /// Is the player's cylindrical footprint at `(east, north, foot_z)` clear of geometry?
    /// Samples a horizontal ring of `n` directions at `radius` (and the centre) at chest height,
    /// returning `true` only when none are blocked. Used by the depenetration net (design §3.3).
    pub fn footprint_clear(&self, east: f32, north: f32, foot_z: f32, radius: f32, n: usize) -> bool {
        if self.cols == 0 { return true; }
        // ANY spoke hit within the ring means blocked — a wall AT the radius counts as embedded for
        // the footprint. Since #855 a wall flush with the ring CENTRE is reported too (it was inside
        // the old `(1e-3, 1.0]` band and read as clear); a body already that embedded is the
        // depenetration net's own subject, so the direction is toward "recover". Round-1 review
        // measured this caller and `path_clear` on a sloped fixture at production probe heights and
        // found no other answer changed — measured against `t >= 0`, so for round 2's extra
        // `contact_tol` of slack it is REASONED, not re-measured, that the same holds.
        // `line_of_sight_does_not_see_through_a_wall_it_is_almost_touching` is the pin that would
        // catch it going the wrong way.
        self.ring_nearest_hit(east, north, foot_z + crate::body::PLAYER_BODY.ring, radius, n)
            .is_none()
    }

    /// The radial-ring core of [`footprint_clear`], cast at an EXPLICIT world height `ring_z` instead
    /// of `foot_z + ring`. Casts `n` rays outward from the centre to `radius` and returns the NEAREST
    /// geometry hit as a fraction [0,1] of the spoke length (`None` = every spoke clear).
    ///
    /// Unlike a travel-parallel feeler (`path_clear`'s sweep), a radial spoke fans in EVERY direction,
    /// so it crosses a wall that lies within `radius` in ANY direction — including one the caller's
    /// motion runs PARALLEL to (a ray running alongside a wall never intersects it; a spoke pointed at
    /// it does). That is what lets `path_clear` close the #381 parallel-wall hole by sampling this ring
    /// along the swept segment. Returning the fraction (not a bool) lets each caller pick its own
    /// boundary: `footprint_clear` blocks on any hit; `path_clear` blocks only STRICTLY within radius
    /// (a wall exactly at radius is tangent — touching, not overlapping — matching the feeler sweep's
    /// parallel-tangent-is-clear convention, so it does not newly reject a body-width slide).
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
    /// This is a LINE-OF-SIGHT primitive — it answers "can I see/shoot from here to there", NOT
    /// "can the character WALK from here to there". The character is a cylinder, not a line: use
    /// `path_clear` for anything the walker has to physically traverse (#358).
    pub fn line_clear(&self, from: [f32; 3], to: [f32; 3], radius: f32) -> bool {
        let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if dist < 1e-5 { return true; }
        let ext = (dist + radius.max(0.0)) / dist;
        let target = [from[0] + d[0] * ext, from[1] + d[1] * ext, from[2] + d[2] * ext];
        self.nearest_hit_t(from, target).is_none()
    }

    /// **Line-of-sight test for the pure-pursuit carrot clamp (#685).** May the walker steer STRAIGHT
    /// from `from` to `to` without the aim chording across a WALL (a convex corner)?
    ///
    /// This is a CHEST-HEIGHT centre ray — both foot points raised by the body's `chest` (4.0, the
    /// controller's own contact-ray height, `traversability::PLAYER_BODY`). Casting at chest is what
    /// makes the clamp not over-tighten (the dominant risk, #685): a FOOT-height ray skims the ground
    /// and false-trips on every walkable bump and slope, which over a corpus of hilly zones slowed the
    /// walker 5-8x and newly FAILED dozens of routes (measured). A chest-height ray rides ABOVE
    /// walkable ground undulation (a slope/bump is not a corner) while a real WALL — which rises from
    /// the floor well past chest — still blocks it, exactly as the controller's own chest contact ray
    /// would. It is the SAME contact height the controller collides at, so the clamp asks precisely the
    /// controller's question.
    ///
    /// A single centre ray (not the `path_clear` feeler fan) is deliberate and also the least-aggressive
    /// choice: the corner-cut chord crosses the wall with its CENTRELINE, so the centre ray catches it,
    /// while a ray running ALONGSIDE a wall never intersects it — so merely hugging a corridor wall does
    /// not clamp. `radius` extends the ray past `to` so a wall just beyond the carrot still counts.
    ///
    /// Returns `true` (clear) when no zone geometry is loaded.
    pub fn carrot_los_clear(&self, from: [f32; 3], to: [f32; 3], radius: f32) -> bool {
        let chest = crate::body::PLAYER_BODY.chest;
        self.line_clear([from[0], from[1], from[2] + chest], [to[0], to[1], to[2] + chest], radius)
    }

    /// **Is there continuous standable ground under the straight hop `from → to`? (#727.)**
    ///
    /// The companion of [`Self::carrot_los_clear`], and it exists because that ray **cannot answer
    /// this question and was never meant to**: its rustdoc directly above says it is a chest-height
    /// centre ray, chosen deliberately so the carrot clamp rides ABOVE walkable ground undulation,
    /// and that what it catches is WALLS. *A hole is not a wall.* Asked "can the character get
    /// there on foot", a chest ray flies straight over a chasm by construction. Measured: on two
    /// ledges split by a 10 u gap with the next floor 200 u down, the ray alone let the coarse-route
    /// cursor jump 2 → 6, declaring an entire bridge detour walked
    /// (`walker`'s `a_resync_must_not_cross_a_chasm_the_character_cannot_walk`).
    ///
    /// So also ask the FLOOR. Probe the column at the fine local tier's 2 u spacing along the hop
    /// and require a standable surface at every probe, inside a per-probe envelope of one
    /// sub-segment of walkable slope ([`MAX_WALK_GRADE`]) plus one discrete `step_up` riser. A void,
    /// or a drop steeper than that, refuses the hop.
    ///
    /// **That envelope is NOT symmetric, and it is NOT `walk_profile_ok`'s.** The probe window is
    /// `ground_below(e, n, prev_z + step_up, allow + step_up)`: it opens `step_up` ABOVE the last
    /// floor and `allow` BELOW it. So the descent allowance is `allow` but the ascent allowance is
    /// the bare `step_up`, giving an ascent grade cap of `step_up / PROBE_SPACING = 1.0` against
    /// `MAX_WALK_GRADE = 1.2`. A continuous slope in the band **1.0 < grade ≤ 1.2** is walkable,
    /// `walk_profile_ok` and A*'s own grade test both accept it, and this predicate refuses it.
    /// Pinned by `ground_continuous_ascent_is_capped_at_step_up_not_the_walk_grade`, which asserts
    /// both halves: the refusal here and the acceptance by [`Self::walk_profile_ok`] on the same hop.
    ///
    /// The asymmetry is left in place deliberately. A false NEGATIVE costs this feature nothing but
    /// coverage — the cursor resync simply declines to advance and the walker keeps the cursor it
    /// had — whereas widening the window widens what a resync may adopt, which is the direction
    /// every #727 safety argument runs against. Correcting it is a live-behaviour change and wants
    /// its own measurement; filed as **gap 4 on #734** (`issuecomment-5097268224`, 2026-07-27).
    ///
    /// **Necessary, not sufficient — this is NOT a walkability oracle.** It samples a line, so a
    /// hole narrower than the spacing can fall between probes, and it says nothing about the
    /// character's WIDTH (that is [`Self::path_clear`]'s question). Callers must bound the hop: at
    /// ~2 u spacing the cost is one column probe per 2 u of run.
    ///
    /// **#905 — the WIDTH half of that sentence has already been litigated; do not re-derive it.**
    /// Centre-only sampling was filed as a defect (#734 gap 2), and the fix — three parallel probes
    /// offset across the direction of travel — was built, measured and **WITHDRAWN**: the
    /// controller's own floor clamp is a single centre column too, so the sweep refused hops whose
    /// floor the controller stands on at every sample, which is a FALSE REFUSAL. Width-blindness
    /// here is *agreement* with the consumer, not a gap against it. The retraction, its measurement
    /// table and the regression guard that keeps the sweep out are on
    /// `crate::steering::resync_reachable`'s rustdoc — read that before treating the width sentence
    /// above as an open defect. The LINE-SAMPLING half (#734 gap 1) is a different matter and IS
    /// still live; `PROBE_SPACING`'s own comment in the body below says what holds it.
    ///
    /// **And the envelope is PER PROBE, not over the hop.** `prev_z` chains, so each probe is judged
    /// against the last floor found rather than against the hop's own start and end. A descent that
    /// takes the full allowance at every probe therefore compounds, and the aggregate it compounds to
    /// is analytic:
    ///
    /// ```text
    /// sub        = run / n                       (n = ceil(run / PROBE_SPACING), min 1)
    /// allow      = sub * MAX_WALK_GRADE + step_up
    /// mean grade = allow / sub = MAX_WALK_GRADE + step_up / sub
    /// ```
    ///
    /// `sub == PROBE_SPACING` — a hop that is an exact multiple of 2 u — gives `1.2 + 2.0/2.0 =`
    /// **2.2**, measured to the boundary by
    /// `ground_continuous_compounding_descent_is_capped_at_grade_plus_one_step_per_probe`
    /// (2.15 accepted, 2.20 refused) on a 24 u hop. **2.2 is that hop length's cap, not the
    /// predicate's.** `n` is a `ceil`, so `sub` is generally SHORTER than `PROBE_SPACING` and the
    /// cap is correspondingly HIGHER: `sub → PROBE_SPACING/2` (a hop just over a multiple of 2 u)
    /// approaches `1.2 + 2.0/1.0 =` **3.2**, and a hop shorter than one spacing runs a single
    /// sub-segment whose mean-grade cap `1.2 + 2.0/run` is unbounded as `run → 0` — though there
    /// the ABSOLUTE fall stays inside `run * 1.2 + step_up`, i.e. one ordinary step down.
    /// `ground_continuous_the_grade_cap_is_a_function_of_the_hop_length_not_a_constant` pins the
    /// short-hop end: a 1 u hop accepts a 3.1 u fall — mean grade **3.1**. Note which way all of
    /// this moves: the cap gets **worse** if `PROBE_SPACING` is ever reduced, because the discrete
    /// `step_up` term is divided by it.
    ///
    /// That is deliberate to the extent that a long walkable ramp must not be refused for being long,
    /// and unbounded to the extent that nothing re-checks the aggregate. Disclosed as **gap 3 on
    /// #734** — in that issue's comments, not its body; the caller's hop bound
    /// ([`crate::steering::CURSOR_RESYNC_MAX_HOP`]) is currently the only thing limiting how much can
    /// compound.
    ///
    /// Returns `true` when the zone has no geometry, matching [`Self::carrot_los_clear`]'s own
    /// documented no-geometry behaviour — a client with no world model must not silently answer
    /// "unreachable" to everything.
    pub fn ground_continuous(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        /// The fine local tier's cell (`steering::LOCAL_CELL`): the finest scale the walker's own
        /// planner resolves, so a gap missed here is one the fine planner could not express either.
        ///
        /// **#903 — this value is pinned from OFF-FILE, and shrinking it reds `main` with no git
        /// conflict.** Two tests hold it, and only one of them lives in this file:
        ///
        /// * `ground_continuous_probe_spacing_catches_every_hole_wider_than_the_spacing` (below in
        ///   this file) — the ordinary direction: a hole WIDER than the spacing must be caught.
        /// * `a_narrow_hole_between_probes_still_crosses_the_resync_undetected`, in
        ///   `crate::walker`'s test module — a bug-CHARACTERISATION test for #734 gap 1. It builds
        ///   a 1.5 u hole and asserts that the resync predicate still steps over it undetected.
        ///   **That 1.5 is a HOLE WIDTH, not a second copy of this constant.** The test names
        ///   `PROBE_SPACING` only in prose, and the line below is its only definition in the whole
        ///   tree, so the coupling is not copy-against-copy — which a grep would find — but a
        ///   fixture width chosen to sit under a constant the fixture cannot see. **Narrowing this
        ///   constant turns that test RED**, and because the two files never touch, nothing in the
        ///   diff surfaces the coupling. That red is not a regression — it means #734 gap 1 has
        ///   been FIXED — and its failure message is the delete-me instruction (the test, its
        ///   entry in `every_walker_test_name_cited_in_a_doc_comment_still_exists`, and the gap-1
        ///   bullets in `steering::resync_cursor`'s and `Walker::advance_cursor`'s rustdocs).
        ///
        ///   #903's body describes that test as hard-coding "its own local
        ///   `PROBE_SPACING: f32 = 2.0`", and #1038 round 1 carried that description into this
        ///   comment. #1038's round-2 review refuted it by measurement. The issue's ASK — a
        ///   reverse pointer at the constant — is what this comment is; only the mechanism it
        ///   described was wrong, and the true mechanism is the stronger warning, because a second
        ///   copy would at least be greppable and a fixture width is not.
        ///
        /// That delete-me path is measured, not argued: at #887 round 2, when `walker.rs` held 44
        /// tests, this constant was set to 1.0 and the walker tests re-run — that one test alone
        /// went red, 43 passed / 1 failed of 44. That denominator is a record of THAT run;
        /// `walker.rs` has grown since and it no longer describes the file.
        const PROBE_SPACING: f32 = 2.0;
        if !self.has_geometry() { return true; }
        let step_up = crate::body::PLAYER_BODY.step_up;
        let run = ((to[0] - from[0]).powi(2) + (to[1] - from[1]).powi(2)).sqrt();
        let n = (run / PROBE_SPACING).ceil().max(1.0) as i32;
        // One sub-segment of walkable slope plus one discrete step. NOT `walk_profile_ok`'s envelope:
        // this is the DESCENT allowance only, and the window it opens is
        // `[prev_z - allow, prev_z + step_up]`, so uphill is capped at `step_up` alone. The rustdoc's
        // "NOT symmetric" paragraph derives the resulting ascent-grade cap.
        let allow = (run / n as f32) * MAX_WALK_GRADE + step_up;
        let mut prev_z = from[2];
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let e = from[0] + (to[0] - from[0]) * t;
            let nn = from[1] + (to[1] - from[1]) * t;
            // Search from one step-up above the last floor down to the envelope's floor;
            // `ground_below` returns the HIGHEST standable surface in that window (a ramp over a
            // lower plane wins), so a legitimate slope is followed rather than under-cut.
            match self.ground_below(e, nn, prev_z + step_up, allow + step_up) {
                Some(fz) => prev_z = fz,
                None => return false,
            }
        }
        true
    }

    /// The clearance test A* validates ONE GRID EDGE with, at plan resolution `cell`.
    ///
    /// Sweeps the character's collision volume (`path_clear`) on a FINE grid and casts a centre ray
    /// (`line_clear`) on a COARSE one. That asymmetry is deliberate and it is the whole subtlety of
    /// #358 — a cell-centre line is only the line the WALKER will actually walk when the grid is
    /// fine enough:
    ///
    /// * On the FINE local tier (`nav::steering::LOCAL_CELL` = 2u) the centre line ≈ the walked line,
    ///   and a corridor holds SEVERAL lateral cell choices. Rejecting the one edge that scrapes a
    ///   wall just makes A* pick the cell one over — down the middle. This is the tier the walker
    ///   steers along, and the only tier fine enough to even EXPRESS a route through a gap narrower
    ///   than the character. Measured on the live gfaydark→butcher wedge: the coarse route had 0
    ///   ray/capsule disagreements, the fine route had 2 — on exactly the segments the character
    ///   was stuck on.
    ///
    /// * On the COARSE 8u tier a cell centre only has to have a FLOOR under it — it can sit 0.3u
    ///   from a wall in a corridor the character walks down the middle of without trouble, and a
    ///   corridor narrower than ~16u offers no laterally-adjacent cell to move to. Sweeping the
    ///   volume along that arbitrary lattice line therefore does not reject *unwalkable corridors*,
    ///   it rejects *corridors*. Measured over 1200 start/goal pairs in 10 cached zones: routable
    ///   pairs fell 876 → 813 (−7%), and Ak'Anon — all narrow gnome tunnels — collapsed from 90/120
    ///   to 55/120 (−29%). Sealing a third of a city is a worse bug than the one being fixed (#310
    ///   removed a sub-radius planning fallback for the mirror-image reason), so the coarse tier
    ///   stays a corridor SELECTOR validated by a ray, and the fine tier — re-planned every tick
    ///   ahead of the walker — is what enforces that the volume actually fits.
    pub fn edge_clear(&self, from: [f32; 3], to: [f32; 3], radius: f32, cell: f32) -> bool {
        if cell <= SWEPT_EDGE_MAX_CELL { self.path_clear(from, to, radius) }
        else { self.line_clear(from, to, radius) }
    }

    /// Can the player's COLLISION VOLUME travel from `from` to `to` without crossing geometry?
    ///
    /// The clearance test the planner validates a segment with **must be the same test the
    /// controller moves under** (#358). It was not: this cast a single centre RAY while
    /// `CharacterController` moves a cylinder of `movement::PLAYER_RADIUS`. A corner can be
    /// ray-clear and capsule-blocked — the ray threads the gap, the shoulder does not — so A*
    /// handed the walker routes it is physically incapable of following, and the controller's
    /// slide-along-wall response then shoved it off-route and wedged it.
    ///
    /// The mismatch bites HARDEST on the fine local tier: at `nav::steering::LOCAL_CELL` = 2u the grid
    /// can actually *express* a route through a sub-capsule gap, where the coarse 8u grid never
    /// could. That is why the overlay showed the coarse line rounding the corner cleanly while the
    /// fine line — the one the walker steers along — hugged the wall. Measured on the live
    /// gfaydark→butcher wedge: coarse route = 0 ray/capsule disagreements, fine route = 2, both on
    /// the segments the character was stuck on.
    ///
    /// So sweep the volume: feelers across the whole diameter, offset perpendicular to the
    /// horizontal motion.
    ///
    /// **What is — and is not — shared with the controller (corrected, #378/#386).** An earlier
    /// version of this comment claimed the planner and controller "share one collision model" via
    /// `Collision::sweep`. That was false on both halves: `sweep` had zero production callers (the
    /// mover's `slide` rolls its own centre-ray contact resolution) and the feeler patterns
    /// differed anyway. The TRUE shared artifact is the body geometry: the probe HEIGHTS both sides
    /// use come from `traversability::PLAYER_BODY` (the planner's top probe IS the controller's
    /// chest contact ray), and the planner sweeps a strictly WIDER lateral pattern (5 feelers vs
    /// the mover's centre ray + radius back-off) — conservative in the safe direction:
    /// planner-clear ⇒ controller-passable laterally, up to the parallel-wall limit below.
    ///
    /// Cost is 3 rays instead of 1. The A* edge test runs two of these (chest + feet) per edge; the
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
        // Feelers ACROSS THE WHOLE DIAMETER, not just the two shoulders. Three rays (centre + both
        // shoulders) leak on DIAGONAL motion: the shoulders are offset perpendicular to the travel
        // direction, so against a wall the segment crosses at an angle, one shoulder starts already
        // PAST the wall plane and the other never reaches it — both slide along the wall instead of
        // across it, and the capsule threads a slot narrower than itself. Caught by mutation-testing
        // this very fix: A* diagonally threaded a 2.5u slot with a 2.0u clearance, on an edge every
        // one of the three rays called clear.
        //
        // Sampling the diameter at r/2 spacing closes it: a wall panel that ENDS inside the swept
        // rectangle now has a feeler either side of its end. This is an approximation of a swept
        // disc, not an exact one — a needle thinner than r/2 threading between feelers would still
        // slip — but zone geometry is walls and panels, not needles, and it is the same family of
        // approximation the mover itself uses.
        //
        // The travel-parallel feelers alone are BLIND to a wall the segment runs ALONGSIDE — a ray
        // parallel to a plane never intersects it, so a segment skimming a wall within the body radius
        // (but never crossing it) reads clear at every feeler (#381; the pre-#376 single centre ray had
        // the same hole, worse). That was the last residual leak of the crossing-exact sweep. The
        // parallel case is closed BELOW by ALSO sampling the body footprint ring along the segment.
        const FEELERS: [f32; 5] = [-1.0, -0.5, 0.0, 0.5, 1.0];
        let feelers_clear = FEELERS.iter().all(|&f| self.line_clear(
            [from[0] + perp[0] * f, from[1] + perp[1] * f, from[2]],
            [to[0] + perp[0] * f, to[1] + perp[1] * f, to[2]],
            radius,
        ));
        if !feelers_clear {
            return false;
        }

        // #381 — PARALLEL-WALL coverage. The character occupies a disc along the WHOLE segment, not
        // just a swept rectangle. Sample the body's footprint RING (`footprint_clear`'s radial-spoke
        // core, reused via `ring_nearest_hit`) at points spaced along the segment: a radial spoke fans in
        // every direction, so it crosses a wall the segment merely runs parallel to — the case the
        // feelers structurally cannot see. Cast at the segment's own swept height (`from[2]`/`to[2]`,
        // interpolated), the SAME chest/feet probe heights the feelers use, and at the SAME `radius`
        // the planner's endpoint occupancy test (`Traversability::occupy_wall_ok`) already demands, so
        // this adds no clearance the endpoints don't already require.
        //
        // OVER-FIRING guard (the dominant risk): sample only STRICTLY INTERIOR points, never the two
        // endpoints. Endpoint occupancy is the caller's own responsibility (a goal legitimately placed
        // within a radius of a wall must stay reachable), and a constant-width corridor whose endpoints
        // already `footprint_clear` at `radius` has that same clearance at every interior sample — so a
        // legitimate body-width passage is NOT newly rejected. Only a wall that genuinely comes within
        // `radius` of the swept centreline BETWEEN the endpoints — a real fit failure — is caught.
        //
        // This ADDS rejections only; it can never turn a blocked segment clear, so the crossing case
        // (exact across 42,160 combos post-#376) is unchanged. Spacing r/2 mirrors the feeler lattice.
        //
        // A spoke hit at exactly the ring radius is TANGENT (the disc touches, does not overlap) and is
        // treated as CLEAR — the same boundary the feeler sweep uses when running parallel to a wall at
        // exactly `radius`. Only a wall STRICTLY within radius (hit fraction < 1 - eps) blocks, so a
        // body-width slide along a wall exactly a radius away is not newly rejected.
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

    /// Does the character have `clearance` of walkable GROUND all around `(east, north)` at `z`?
    ///
    /// The other hazard. `edge_clear`/`path_clear` sweep the character's volume against geometry
    /// that is IN THE WAY — walls. Nothing in the search saw geometry that is MISSING: a cliff lip,
    /// the edge of a bridge, a dock, a waterline. A route may therefore be perfectly wall-clear and
    /// still run along the very brink of a drop, and the walker — which slides on contact and gets
    /// shoved around by server position corrections — eventually goes over it.
    ///
    /// Probes the four axial directions at `clearance` (the same predicate and the same ±band as the
    /// waypoint inset's `edge_ok`, so the search and the inset agree about what an "edge" is) and
    /// requires ground within a tight vertical band of `z` at each. A drop below, a wall lip above,
    /// and open water all read as "no ground" and so as an edge to keep away from.
    ///
    /// NOTE (#378 phase 1): production planning now uses the GRADED, zone-lifetime form of this
    /// predicate — [`Collision::ground_clearance`] via the clearance field — same probe band, same
    /// axial directions, but a distance instead of a boolean and memoised across plans. This exact-
    /// point boolean remains as the diagnostic/test primitive the graded form is validated against.
    pub fn ground_margin_ok(&self, east: f32, north: f32, z: f32, clearance: f32) -> bool {
        if clearance <= 0.0 { return true; }
        [(clearance, 0.0), (-clearance, 0.0), (0.0, clearance), (0.0, -clearance)]
            .iter()
            .all(|&(dx, dy)| self.nearest_floor(east + dx, north + dy, z, 3.0, 8.0)
                .is_some_and(|f| (f - z).abs() <= 8.0))
    }

    /// Graded RADIAL wall clearance at a standing point, from the zone-lifetime clearance field
    /// (#378/#381): distance to the nearest solid geometry at the body's probe heights, in any
    /// direction. Unlike `path_clear`'s travel-parallel feelers, a radial spoke crosses a wall the
    /// route merely runs alongside. Memoised per (2 u cell, floor bucket); deterministic
    /// (computed at the key centre, never the query point).
    pub fn wall_clearance(&self, east: f32, north: f32, floor_z: f32) -> f32 {
        self.clearance.wall_at(self, east, north, floor_z)
    }

    /// Graded ground (ledge) clearance at a standing point, from the zone-lifetime clearance
    /// field: distance to the nearest direction where the floor runs out (drop / lip / waterline).
    /// The graded form of `ground_margin_ok`, same probe band, memoised for the zone's lifetime.
    pub fn ground_clearance(&self, east: f32, north: f32, floor_z: f32) -> f32 {
        self.clearance.ground_at(self, east, north, floor_z)
    }

    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn clearance_field_for_test(&self) -> &ClearanceField {
        &self.clearance
    }

    /// **The CONTROLLER's own "can a body be placed here" predicate, in named halves (#885).**
    ///
    /// This is the definition `movement::is_embedded` used to hold privately, hoisted here so the
    /// controller and the published nav diagnostics read ONE predicate. It was hoisted because the
    /// two disagreed in the field: `/v1/observe/nav_debug` reported every clearance value open
    /// while the controller was holding the same body frozen with `embedded_no_recovery`.
    ///
    /// The disjunction is the controller's, verbatim: [`Collision::footprint_clear`]'s ring (cast,
    /// as always, at `p.z + PLAYER_BODY.ring`) is pierced, **or** there is no floor within
    /// [`GROUND_DEPTH`] below its feet (it has fallen out of the world — the #845 casualty, a
    /// column with zero triangles over it).
    ///
    /// **`||` short-circuit note.** The controller's `is_embedded` was a `||`, so it could skip the
    /// ground probe when the footprint was already pierced. Naming both halves means both are
    /// evaluated. The extra work lands only in the arm where the footprint IS pierced (in the
    /// common clear-footprint case the old `||` evaluated the ground probe anyway), and
    /// `Placement::is_embedded` is the same boolean.
    ///
    /// It is **not** a no-op beyond that boolean, and the first draft of this doc wrongly said no
    /// behaviour depended on the order (#885 review round 1, F10). Running the ground probe on
    /// pierced-footprint frames reaches `ground_below` → `column_hits` → the `facing_blind_surfaces`
    /// counter published as `/v1/observe/debug`'s `nav_support`. Measured on inverted-art ground
    /// with a pierced footprint: `facing_blind_surfaces` **0** (old `||`) → **2** (this function), and
    /// pinned by `evaluating_both_disjuncts_moves_the_published_facing_blind_counter`.
    ///
    /// **What that 2 is** (#885 review round 2, R2-B1). The counter advances once per DOWN-FACING
    /// TRIANGLE `column_hits` admits as standing ground, per call. The 2 is a property of this
    /// fixture's column: it sits exactly on the shared diagonal of the inverted floor quad's two
    /// triangles, so both are admitted. One unit north, off that diagonal, the same single call
    /// publishes **1**; both numbers are asserted in that test. So what this function changes is
    /// that the counter advances on pierced-footprint frames it previously skipped, by however many
    /// down-facing triangles that column's art carries — published, not invisible.
    ///
    /// (#960 renamed the counter and its wire field to say `surfaces`; the refuted wording is now
    /// guarded by `no_tracked_text_calls_the_facing_blind_counter_a_query_count` rather than
    /// re-enumerated by hand.)
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

    /// A LIVE sample of the traversability model around one standing point, for the published nav
    /// diagnostics snapshot (#608): the 16 radial wall spokes (the same rays
    /// `ClearanceField::wall_at` aggregates, cast here at the ACTUAL point rather than a memo-key
    /// centre, so the sample is honest about where it was taken), the 8-direction footprint ring
    /// (`footprint_clear`'s rays, per direction), and the two graded field values the planner's
    /// hug cost / standing-room margin actually consult. This is NAV sampling its OWN model —
    /// consumers draw the sample verbatim and never re-cast these rays.
    ///
    /// # #885 — three facts that used to share one encoding
    ///
    /// Measured on the constructed fixtures in the #885 test module below, these three worlds
    /// produced IDENTICAL `wall_spokes` and `footprint_ok` — all 16 spokes `4.0`, all 8 footprint
    /// directions `true`:
    ///
    /// 1. open ground (honest — sixteen spokes that measured nothing);
    /// 2. a body ringed by walls standing at **exactly** the 4.0 cap — measured: **4 real hits at
    ///    4.0** (the axis-aligned spokes 0/4/8/12) and **12 spokes that measured nothing**, because
    ///    an off-axis spoke faces the same wall plane at `4 / cos θ ≥ 4.33 u`, past the cap. The
    ///    old seed-at-the-cap encoding wrote `4.0` for both kinds, so this world was
    ///    byte-identical to (1) while being a mixture of the two things the old float conflated;
    /// 3. a body the controller was refusing to place at all.
    ///
    /// (Their `field_wall` / `field_ground` values did differ — 4/2, 3/2 and 4/0 respectively — but
    /// nothing in the payload nominated either as the tiebreak, `field_ground` reads 0 at any
    /// ordinary ledge edge, and neither is a claim about the character. The two fields a caller is
    /// pointed at for "is there room here" were the identical ones.)
    ///
    /// Three separate collapses caused that, and each is now a type rather than a number:
    ///
    /// * a saturated spoke and a cap-distance hit → [`crate::diagnostics::SpokeReading`];
    /// * "no floor in the search band" served as a floor height → [`crate::diagnostics::ProbeAnchor`];
    /// * the character's own placement never asked at all → [`crate::diagnostics::Placement`],
    ///   evaluated at `ref_z` (the character's height), NOT at the anchor.
    ///
    /// The last one is the load-bearing addition: the spokes and ring are cast at the anchor and at
    /// `anchor + PLAYER_BODY.ring`, both of which can sit in open air above the geometry a body is
    /// stuck in, so no amount of care in *those* rays could have answered the caller's question.
    ///
    /// **Not the mechanism** (measured, so that it is not guessed at again): the spokes do not
    /// read the cap because a cast starting inside a solid misses. `nearest_hit_t` is a two-sided
    /// Möller–Trumbore, so a ray from inside a closed solid registers its EXIT face — measured at
    /// 2.0 u from the centre of a 4 u box, not 4.0. #854's contact-probe blind band is likewise not
    /// implicated: these rays are cast at `+2.5` and `+4.0`, far above the bottom 0.5 u.
    pub fn clearance_probe(&self, east: f32, north: f32, ref_z: f32) -> crate::diagnostics::ClearanceProbe {
        use crate::diagnostics::{ProbeAnchor, SpokeReading};
        use crate::body::PLAYER_BODY;
        const SPOKES: usize = 16;
        const RING: usize = 8;
        const CAP: f32 = 4.0; // the ClearanceField's WALL_CAP horizon
        // The anchor: a floor if the band holds one, and an EXPLICIT fallback if it does not. The
        // `unwrap_or(ref_z)` this replaced published the fallback as a floor height.
        let anchor = match self.nearest_floor(east, north, ref_z, 3.0, 8.0) {
            Some(z) => ProbeAnchor::Floor { z, reference_z: ref_z },
            None => ProbeAnchor::NoFloorInBand { reference_z: ref_z },
        };
        let floor_z = anchor.z();
        let mut wall_spokes = Vec::with_capacity(SPOKES);
        for i in 0..SPOKES {
            let a = (i as f32) / (SPOKES as f32) * std::f32::consts::TAU;
            let (dx, dy) = (a.cos(), a.sin());
            // `None` = nothing hit anywhere within the cap, which is a LOWER BOUND, not 4.0.
            let mut best: Option<f32> = None;
            for hz in PLAYER_BODY.planner_probes() {
                let from = [east, north, floor_z + hz];
                let to = [east + dx * CAP, north + dy * CAP, floor_z + hz];
                if let Some(t) = self.nearest_hit_t(from, to) {
                    let d = t * CAP;
                    best = Some(best.map_or(d, |b: f32| b.min(d)));
                }
            }
            wall_spokes.push(match best {
                Some(at) => SpokeReading::Hit { at },
                None => SpokeReading::ClearToCap,
            });
        }
        let radius = eqoxide_core::physics::PLAYER_RADIUS;
        let ring_z = floor_z + PLAYER_BODY.ring;
        let footprint_ok = (0..RING).map(|i| {
            let a = (i as f32) / (RING as f32) * std::f32::consts::TAU;
            let to = [east + a.cos() * radius, north + a.sin() * radius, ring_z];
            self.nearest_hit_t([east, north, ring_z], to).is_none()
        }).collect();
        crate::diagnostics::ClearanceProbe {
            at: [east, north],
            anchor,
            // At the CHARACTER's height (`ref_z`), not the anchor's — the whole point. `floor_z` is
            // a `CastZ`, not an `f32`, so substituting it here BARE is `error[E0308]` rather than a
            // silently republished #885 payload (review round 2, R2-N1). Spelled-out escapes —
            // `.raw()`, `+ 0.0`, `- 0.0` — still compile; the test named on `CastZ` is what stops
            // those (review round 3).
            body: self.body_placement([east, north, ref_z]),
            wall_spokes,
            cap: CAP,
            footprint_ok,
            footprint_radius: radius,
            footprint_ring_z: ring_z,
            // `.raw()` is the documented escape hatch on `CastZ`: these two take a plain height
            // and are questions ABOUT the anchor, which is exactly what the type permits.
            field_wall: self.wall_clearance(east, north, floor_z.raw()),
            field_ground: self.ground_clearance(east, north, floor_z.raw()),
        }
    }

    /// ALL distinct walkable surface heights at `(east, north)` within `[ref_z - down, ref_z + up]`,
    /// sorted high→low with near-duplicates (within 1u) merged. Unlike `nearest_floor` (one surface),
    /// this exposes every floor in the column — essential for multi-level pathfinding where a ramp or
    /// lower floor sits UNDER an upper ledge (so A* can choose to descend instead of always snapping
    /// to the nearest/upper surface).
    pub fn column_floors(&self, east: f32, north: f32, ref_z: f32, up: f32, down: f32) -> Vec<f32> {
        let mut hits = Vec::new();
        self.column_hits(ColumnQuery { east, north, ref_z, up, down, floors_only: true }, &mut hits);
        let mut zs: Vec<f32> = hits.into_iter().map(|(z, _)| z).collect(); // already high→low
        zs.dedup_by(|a, b| (*a - *b).abs() < 1.0);
        zs
    }


    /// Count a climb-edge plan (#309 honesty counter) — `walker.rs`'s A*-only search records this
    /// through here since `climb_plans` itself is private to this crate.
    pub fn record_climb_plan(&self) {
        self.climb_plans.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Count a tight (generous-tier-declined) plan — see `search_tiered`'s tiering comment.
    pub fn record_tight_plan(&self) {
        self.tight_plans.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Raw zone-line-region cache, keyed by `(zone_point_index, point)` — the source data
    /// `resolve_teleport_pads`/`teleport_pad_footprints` (A*-only) filter by index.
    pub fn zone_line_regions(&self) -> &[(i32, [f32; 3])] {
        &self.zone_line_regions
    }

    /// Read-only accessor for the private `z_min` field — needed by `eqoxide-nav`'s test-only
    /// zone-corpus probes (STAYS-bucket, cross-crate), gated the same as the rest of this file's
    /// test-only surface (docs/specs/2026-09-21-agent-harness-separation-plan-zone-geometry.md,
    /// Task 2 / #32).
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
            // Standable floors only.
            let floors = col.column_floors(e, n, 80.0, 40.0, 120.0);
            eprintln!("  standable floors: {:?}", floors);
            // What the walker's grounded clamp sees from each hypothesis height.
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

    // ──────────────────── #693: the phantom stacked-tier descent (qeynos/qcat) ────────────────────
    /// An up-facing floor plate at height `z` over east [e0,e1] × north [n0,n1].
    /// (libeq pos = [north, height, east]; winding matches `floor_up`, verified up-facing.)
    fn plate(z: f32, e0: f32, e1: f32, n0: f32, n1: f32) -> MeshData {
        MeshData {
            positions: vec![[n0, z, e0], [n1, z, e0], [n1, z, e1], [n0, z, e1]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 2, 1, 0, 3, 2], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// TWO FULL STACKED FLOORS, no opening anywhere: an upper walkable surface (z=0) directly over
    /// a lower one (z=-30) — the qeynos street over the qcat aqueduct, distilled. There is NO
    /// walkable connection between the tiers at any XY.
    fn stacked_floors() -> Collision {
        Collision::build(&ZoneAssets {
            terrain: vec![plate(0.0, -60.0, 60.0, -60.0, 60.0), plate(-30.0, -60.0, 60.0, -60.0, 60.0)],
            objects: vec![], textures: vec![],
        }, 32.0)
    }

    /// The #693 primitive itself: an intervening slab blocks the descent corridor; an opening (or
    /// a column holding nothing but the landing) is clear.
    #[test]
    fn descent_corridor_sees_the_intervening_slab_and_the_opening() {
        let c = stacked_floors();
        assert!(!c.descent_corridor_clear(0.0, 0.0, 0.0, -30.0),
            "the upper plate lies between takeoff (0) and landing (-30) — corridor must be blocked");
        // A column with only the landing: clear.
        let lone = Collision::build(&ZoneAssets {
            terrain: vec![plate(-30.0, -60.0, 60.0, -60.0, 60.0)],
            objects: vec![], textures: vec![],
        }, 32.0);
        assert!(lone.descent_corridor_clear(0.0, 0.0, 0.0, -30.0),
            "an open column above the landing must be clear");
        // A landing at/above the takeoff band (no gap in which anything could intervene): clear.
        assert!(lone.descent_corridor_clear(0.0, 0.0, -30.0, -30.5));
        assert!(lone.descent_corridor_clear(0.0, 0.0, -30.0, -28.0));
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

    // ── nav z-anchor regression tests (#229, #329, #197p2) ───────────────────────────────────────
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
    /// the rest of `-p eqoxide-nav --lib`, all of `-p eqoxide --lib`, **and the real-geometry corpus
    /// `a_driven_swim_descent_never_passes_a_real_zone_floor`**.
    ///
    /// That last GREEN is the reason this test exists rather than a nicety, and it is stated because
    /// it cuts against the clause: the 11-zone corpus does NOT catch a ray-only tolerance. Baked
    /// terrain in those zones is triangulated finely enough that the ray's own coordinates already
    /// bound the triangle's, so the two scales agree there. The clause is held by this fixture alone,
    /// and if this test is ever deleted the per-triangle scale becomes unpinned — no corpus run will
    /// notice.
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

    // ── #727 round 3: `ground_continuous`'s numeric envelope (review finding C) ──────────────────
    //
    // Round 2 shipped the predicate with its behaviour pinned only where it is TOTAL (a void refuses,
    // a flat floor accepts). The round-2 review showed that left the numbers free: widening
    // `PROBE_SPACING` 2 → 8, scaling `allow` by 10, and replacing `allow` with `step_up` all survived
    // the whole suite. These three tests fail under each of those mutations respectively, so the
    // constants are now claims the suite defends rather than commentary.
    /// Two coplanar floor slabs at z = 0 running east, with a hole of `width` starting at `start`.
    fn floor_with_hole(start: f32, width: f32) -> Collision {
        Collision::build(&ZoneAssets {
            terrain: vec![slab(0.0, -10.0, 10.0, -20.0, start, true),
                          slab(0.0, -10.0, 10.0, start + width, 20.0, true)],
            objects: vec![], textures: vec![],
        }, 32.0)
    }

    /// A high floor east of 0 and a floor `drop` units lower west of it, meeting at e = 0.
    fn floor_with_drop(drop: f32) -> Collision {
        Collision::build(&ZoneAssets {
            terrain: vec![slab(0.0, -10.0, 10.0, -20.0, 0.0, true),
                          slab(-drop, -10.0, 10.0, 0.0, 20.0, true)],
            objects: vec![], textures: vec![],
        }, 32.0)
    }

    /// **`PROBE_SPACING` is pinned by the holes it must not step over.** A hole wider than the
    /// spacing must contain a probe wherever it sits, so sweeping its position over a whole spacing
    /// interval is what turns "2 u" from a comment into a requirement: widen the constant and some
    /// offset in this sweep starts being missed.
    #[test]
    fn ground_continuous_probe_spacing_catches_every_hole_wider_than_the_spacing() {
        const WIDTH: f32 = 2.5; // > PROBE_SPACING = 2.0
        let mut missed = Vec::new();
        for k in 0..=32 {
            let start = -8.0 + 0.25 * k as f32;
            if floor_with_hole(start, WIDTH)
                .ground_continuous([-18.0, 0.0, 0.0], [18.0, 0.0, 0.0])
            {
                missed.push(start);
            }
        }
        assert!(missed.is_empty(),
            "a {WIDTH} u hole was stepped over at east offsets {missed:?} — the probe spacing is no \
             longer fine enough to catch holes it claims to catch");
    }

    /// **The upper end of `allow` is pinned.** A drop of 6 u is outside one 2 u sub-segment of
    /// `MAX_WALK_GRADE` plus a `step_up` riser (2 · 1.2 + 2 = 4.4), so it must be refused. Scale
    /// `allow` up and this passes a cliff.
    #[test]
    fn ground_continuous_refuses_a_drop_outside_the_walk_envelope() {
        assert!(!floor_with_drop(6.0).ground_continuous([-6.0, 0.0, 0.0], [6.0, 0.0, -6.0]),
            "a 6 u drop is outside the walk envelope and must refuse the hop");
    }

    /// **The lower end of `allow` is pinned too, and this is the half that matters for false
    /// negatives.** A 4 u drop is INSIDE the same envelope and must be accepted — a walker that
    /// refuses ordinary walkable terrain would make the resync inert rather than safe. Replace
    /// `allow` with a bare `step_up` and this goes RED.
    #[test]
    fn ground_continuous_accepts_a_drop_inside_the_walk_envelope() {
        assert!(floor_with_drop(4.0).ground_continuous([-6.0, 0.0, 0.0], [6.0, 0.0, -4.0]),
            "a 4 u drop is inside the walk envelope; refusing it would make the predicate a false \
             negative on ordinary terrain");
    }

    /// A staircase descending WEST from e = 0, one `riser` per 2 u tread — the shape that makes the
    /// per-probe allowance compound, because `prev_z` chains from probe to probe.
    fn staircase(riser: f32) -> Collision {
        Collision::build(&ZoneAssets {
            terrain: (0..16)
                .map(|i| slab(-(i as f32) * riser, -20.0, 20.0, -2.0 * (i + 1) as f32, -2.0 * i as f32, true))
                .collect(),
            objects: vec![], textures: vec![],
        }, 32.0)
    }

    /// **The compounding-descent gap, pinned — FOR A HOP THAT IS AN EXACT MULTIPLE OF THE SPACING.**
    ///
    /// `allow` is granted PER PROBE and `prev_z` chains, so a staircase that takes the full allowance
    /// at every tread is accepted however long it runs. The aggregate cap is analytic:
    ///
    /// ```text
    /// sub        = run / n = 24 / 12 = PROBE_SPACING       (this fixture only)
    /// mean grade = MAX_WALK_GRADE + step_up / sub = 1.2 + 2.0 / 2.0 = 2.2
    /// ```
    ///
    /// and the two assertions below confirm it to the boundary. This exists so the disclosed gap is
    /// a measured claim rather than commentary: scale `allow` up and the refusal goes green; replace
    /// `allow` with a bare `step_up` and the acceptance goes red.
    ///
    /// The 24 u hop is chosen to make `sub == PROBE_SPACING` exactly. That is the FAVOURABLE case —
    /// see `ground_continuous_the_grade_cap_is_a_function_of_the_hop_length_not_a_constant` for what
    /// the same predicate accepts when the hop is not a multiple of the spacing.
    ///
    /// ⚠️ **Correction (#727 round 4).** Round 3 recorded this gap as "mean grade ~1.83 accepted".
    /// That figure came from a coarser sweep and understates the envelope by ~18%; 1.83 is the
    /// *ratio* to `MAX_WALK_GRADE`, not the grade. Measured here: 2.15 accepted, 2.20 refused.
    ///
    /// ⚠️ **Correction (#727 round 5).** Rounds 3–4 let this one fixture stand as the cap for the
    /// predicate, and its assertion messages said "the analytic per-probe cap" with no hop-length
    /// qualifier. Withdrawn: 2.2 is this HOP LENGTH's cap. The messages below now say so, and the
    /// companion test measures the other end.
    #[test]
    fn ground_continuous_compounding_descent_is_capped_at_grade_plus_one_step_per_probe() {
        // 12 probes over a 24 u hop, so each probe spans exactly one 2 u tread.
        let hop = |riser: f32| staircase(riser)
            .ground_continuous([0.0, 0.0, 0.0], [-24.0, 0.0, -12.0 * riser]);
        // Mean grade 2.15 — 79% steeper than MAX_WALK_GRADE, and accepted. This is the GAP, asserted
        // so that it is disclosed by execution and not only by prose.
        assert!(hop(4.3),
            "the compounding gap is documented as reaching a mean grade of 2.15 on a 24 u hop; if \
             this now refuses, the rustdoc's disclosure overstates the gap and must be narrowed");
        // …and 2.20 = MAX_WALK_GRADE + step_up/PROBE_SPACING is the cap FOR sub == PROBE_SPACING.
        assert!(!hop(4.4),
            "a mean grade of 2.20 is the per-probe cap when the hop is an exact multiple of \
             PROBE_SPACING, as this 24 u one is, and must be refused here; if this passes, the \
             compounding is unbounded even in the favourable case and the rustdoc's cap is wrong");
    }

    /// **2.2 is a hop length's cap, not the predicate's** (#727 round 5, non-blocking finding 2).
    ///
    /// `n = ceil(run / PROBE_SPACING)`, so the sub-segment `run / n` is generally SHORTER than the
    /// spacing, and the discrete `step_up` term is divided by that shorter length:
    ///
    /// ```text
    /// mean grade cap = MAX_WALK_GRADE + step_up / (run / n)
    /// ```
    ///
    /// A hop under one spacing runs a single sub-segment, so `run / n == run`. At `run = 1.0` the
    /// cap is `1.2 + 2.0/1.0 = 3.2` — 41% past the 2.2 the sibling test measures, and 2.7× the
    /// `MAX_WALK_GRADE` the predicate is described as enforcing. Both bounds are asserted so the
    /// hop-length dependence is a measured claim: the acceptance is the disclosed gap, the refusal
    /// is the analytic cap for THIS length.
    ///
    /// This does not contradict the sibling test; it is the same formula evaluated somewhere else.
    /// Note what stays bounded: the ABSOLUTE fall is still `run * MAX_WALK_GRADE + step_up = 3.2 u`,
    /// about one step down. It is the GRADE that is unbounded as `run → 0`, and grade is what the
    /// rustdoc used to quote as a single number.
    #[test]
    fn ground_continuous_the_grade_cap_is_a_function_of_the_hop_length_not_a_constant() {
        // run = 1.0 u ⇒ n = ceil(0.5).max(1) = 1 ⇒ sub = 1.0 ⇒ allow = 1.0·1.2 + 2.0 = 3.2.
        let hop = |drop: f32| floor_with_drop(drop)
            .ground_continuous([-0.5, 0.0, 0.0], [0.5, 0.0, -drop]);
        assert!(hop(3.1),
            "a 1 u hop accepts a 3.1 u fall — mean grade 3.1, against the 2.2 the rustdoc used to \
             give as the predicate's cap. If this refuses, `ground_continuous`'s rustdoc overstates \
             the hop-length dependence and must be narrowed");
        assert!(!hop(3.3),
            "3.2 = MAX_WALK_GRADE·run + step_up is the analytic allowance at run = 1.0 and 3.3 must \
             be refused; if this passes, the per-probe envelope is not what the code computes");
    }

    /// **Every `ground_continuous` test name cited in a doc comment still resolves** (#727 round 5).
    ///
    /// SHARED-side twin (docs/specs/2026-09-21-agent-harness-separation-plan-zone-geometry.md, Task
    /// 2 / #32): the citation guard cannot name a test fn in a different crate's private test module
    /// (the technique this file's own doc already describes for the `walker` twin), so the original
    /// single guard was split in two when its cited tests were classified SHARED vs. STAYS. This half
    /// lists every cited name that stayed in `eqoxide-zone-geometry`; the STAYS half (same name) lives
    /// in `eqoxide-nav`'s `collision.rs` and lists the rest.
    ///
    /// Add a name to this list whenever a doc comment in this module starts citing a SHARED-bucket
    /// test.
    #[test]
    fn every_ground_continuous_test_name_cited_in_a_doc_comment_still_exists() {
        let _cited: &[fn()] = &[
            ground_continuous_probe_spacing_catches_every_hole_wider_than_the_spacing,
            ground_continuous_refuses_a_drop_outside_the_walk_envelope,
            ground_continuous_accepts_a_drop_inside_the_walk_envelope,
            ground_continuous_compounding_descent_is_capped_at_grade_plus_one_step_per_probe,
            ground_continuous_the_grade_cap_is_a_function_of_the_hop_length_not_a_constant,
            close_roof_ceiling_is_rejected_by_headroom,
            column_whose_only_surface_is_inverted_still_finds_a_floor,
            probe_qcat_column_vs_fixture,
            qcat_pocket_nearest_floor_is_never_the_ceiling,
            qcat_support_floor_is_visible_to_the_planner,
            only_the_coplanar_case_is_rejected_by_the_parallel_test_the_rest_is_accepted_contact,
            a_floor_z_the_module_just_reported_always_has_a_floor_under_it,
            the_normal_returning_scan_has_the_same_blind_band_free_contact_as_the_t_scan,
            the_three_scans_agree_on_short_segments,
            a_face_just_past_the_far_end_is_caught_on_the_next_frame,
            a_ray_near_the_origin_still_finds_a_floor_whose_vertices_are_far_away,
            line_of_sight_does_not_see_through_a_wall_it_is_almost_touching,
        ];
    }


    // ─── Water-span grid (3D-water-volume nav design §5, Slice 1) ──────────────────────────────
    //
    // These assert the built grid against HAND-AUTHORED analytic fixture geometry (the pool/ceiling
    // bounds written in the test), NEVER against the span grid or `column_surfaces` under test — the
    // non-circular verification principle (design §10). Every expected number is computed from the
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
    // `the_fallback_reports_itself_so_nav_degraded_is_never_silent` is removed with the `column_bottom`
    // valve it tested; the honesty signal is REPLACED IN THIS PR (folded from D-3, review Fix A):
    // `nav_degraded/inverted_floor_art` → `nav_support/facing_blind_ground`, now driven by
    // `facing_blind_surfaces` (a down-facing surface admitted as ground), so there is no dead-signal window
    // where the client falsely reports "properly wound" while on inverted-art ground.
    //
    // The two tests below that assert inverted GROUND is still found (`column_whose_only_surface_is_
    // inverted…`, `a_fully_inverted_zone…`) STILL PASS under D-2 (an inverted floor with clearance above
    // is standable) and are kept.
    /// Inverted GROUND: a column whose lowest (and here only) surface is down-facing, with nothing
    /// beneath it. Real zones bake standable ground this way — permafrost loses 131 such columns to
    /// the facing filter and neriakc 14 — and deleting it leaves the planner with no floor in a
    /// column that plainly has ground in it. It is ground precisely because nothing lies under it; a
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
        assert_eq!(col.column_floors(306.0, 2.0, 50.0, 20.0, 100.0).len(), 1);
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

    /// #358: the planner validated segments with a RAY while the controller moves a CYLINDER of
    /// `PLAYER_RADIUS`. A 1.5u slot is wider than the ray (which has no width at all) and NARROWER
    /// than the character (2 * PLAYER_RADIUS = 2.0u): the ray threads it, the shoulders do not.
    /// `path_clear` must answer for the character's real volume, not for a line.
    #[test]
    fn path_clear_rejects_a_slot_the_ray_fits_through_but_the_character_does_not() {
        let r = eqoxide_core::physics::PLAYER_RADIUS;
        let col = slotted_wall(1.5); // < 2 * PLAYER_RADIUS -> the character cannot fit
        let (from, to) = ([5.0, 9.0, 3.0], [15.0, 9.0, 3.0]); // dead down the middle of the slot
        // The LINE really is unobstructed — that is exactly why the old ray test said "clear".
        assert!(col.line_clear(from, to, r), "the centre ray does thread the slot");
        // ...but the character does not fit, so the planner must NOT call this segment clear.
        assert!(!col.path_clear(from, to, r),
            "path_clear must sweep the player's collision volume, not a ray: a {}u slot cannot pass \
             a {}u-radius character", 1.5, r);
        // A slot the character genuinely fits through stays passable — the fix must not seal doors.
        let wide = slotted_wall(2.0 * r + 1.0);
        assert!(wide.path_clear(from, to, r), "a slot wider than the character must stay clear");
    }

    /// The DIAGONAL leak — found by mutation-testing this very fix, and the reason `path_clear`
    /// samples the whole diameter instead of just the two shoulders.
    ///
    /// A* crossed a wall on a diagonal edge, (9,3) → (11,1), threading a 2.5u slot with a 2.0u
    /// clearance — and all three of the original rays (centre + both shoulders) called it clear. On
    /// a diagonal the shoulders are offset PERPENDICULAR to travel, so they slide ALONG the wall
    /// rather than across it: one starts already past the wall plane, the other never reaches it.
    /// The capsule threads a gap narrower than itself, which is the exact lie #358 exists to kill.
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

    // The drift corpus's swim vertical wish is now the PRODUCTION `steering::swim_vspeed` itself
    // (water-nav Slice 3), called directly at the corpus site below — so the instrument's depth drive
    // cannot diverge from the walker by construction. The old `drift_swim_up_wish` mirror helper +
    // `drift_sim_swim_drive_mirrors_walker` guard existed only because the up-only rule was duplicated
    // in the sim; that duplication is gone, and `swim_vspeed`'s own behaviour is pinned by
    // `steering::tests::swim_vspeed_holds_depth_below_the_plane_and_yields_to_buoyancy_at_it`.
    /// **D-2 SHAPE PROBE (temporary): is the qcat inverted-floor structurally distinguishable from the
    /// D-1 open-air-ceiling fixture?** Dumps every surface in the qcat wedge column facing-blind, with
    /// the distance UP to the next SOLID surface (headroom) and whether water sits above — to decide
    /// whether `is_standable` (facing-blind + headroom + anchoring) can accept -42.97 while rejecting a
    /// ceiling. Not a gate; a design probe.
    #[test]
    #[ignore = "requires qcat glb; D-2 shape probe"]
    fn probe_qcat_column_vs_fixture() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let za = ZoneAssets::from_glb(&std::path::Path::new(&dir).join("qcat.glb")).unwrap();
        let mut col = Collision::build(&za, 32.0);
        // #762: this probe reports water-adjacent column shape; without the region map it would
        // print confident dry answers, so refuse to run rather than measure nothing.
        crate::water_grid::ZoneWater::load(&std::path::Path::new(&dir).join("maps/water"), "qcat")
            .install(&mut col).expect("qcat .wtr must load — a dry qcat probe measures nothing (#762)");
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
        println!("planner column_floors@-43:   {:?}", col.column_floors(x, y, -43.0, 20.0, 30.0));
    }

    // ─────────────────────── PR-D / D-1: support-axis drift (#375) fixtures ───────────────────────
    // These prove the support-axis bug and gate the fix. The RED-on-main test reproduces the LIVE qcat
    // wedge (the planner deletes the floor the controller stands on). The two #329 guards are GREEN on
    // main (the facing filter incidentally satisfies them) and MUST stay green through D-2, when the
    // facing-blind `is_standable` classifier must reject ceilings via headroom+anchoring instead.
    /// An UP-facing floor plane at height `z` over east [e0,e1] × north [-100,100] (`tri_nz > 0`, seen
    /// by `nearest_floor`). NB: the older `floor_band` helper's winding is actually *down*-facing — it
    /// is only ever used with facing-BLIND queries (`path_clear`/`line_clear`), so its facing never
    /// mattered; these PR-D fixtures DO care about facing, so they use these explicit helpers instead.
    ///
    /// ⚠️ **Correction (#727 round 6 review, non-blocking 1).** This line used to end "(both windings
    /// verified by \[a winding-sanity test\])". **No such test exists anywhere in the workspace** — it
    /// was the "winding-sanity companion" retracted at D-2 along with the open-air-ceiling fixture
    /// (see the RETRACTED note ~20 lines below), and the citation was left behind pointing at nothing.
    /// So the windings here are asserted by these helpers' own `normals`/`indices`, not verified by a
    /// test. Found by the mechanical citation scan added in this round, which is the point of it:
    /// a hand-maintained list of citations cannot catch a citation nobody remembered to list.
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
        // The standable set (column_floors) must contain the two real floors (0 and 11) but NOT the
        // low-headroom ceiling (10) — precise, unlike a nearest-to-ref_z check where 10 and 11 are
        // only 1u apart.
        let floors = col.column_floors(0.0, 0.0, 5.0, 20.0, 20.0);
        assert!(!floors.iter().any(|&z| (z - 10.0).abs() < 0.5),
            "column_floors contains the low-headroom ceiling z=10 (set {floors:?}) — the headroom test \
             failed to reject it (#329). MUTATION: drop the headroom check and this fires.");
        assert!(floors.iter().any(|&z| z.abs() < 0.5),
            "room-A floor z=0 (10u headroom) must be standable (set {floors:?})");
        assert!(floors.iter().any(|&z| (z - 11.0).abs() < 0.5),
            "room-B floor z=11 (open above) must be standable (set {floors:?})");
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

    /// **THE #329 QCAT-POCKET GATE (D-1, asset; GREEN on main).** At the qcat spawn pocket the column
    /// is `[roof 391.8, floor -70.0]` (#329, `assets.rs` column comment). The planner must never treat
    /// the 391.8 roof as ground. GREEN on `main` (facing filter); D-2's facing-blind classifier must
    /// keep it green via headroom+anchoring (the roof has rock above it → fails headroom).
    #[test]
    #[ignore = "requires the cached qcat glb at $ZONE_DIR; #329 guard — GREEN on main, stays green through D-2"]
    fn qcat_pocket_nearest_floor_is_never_the_ceiling() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let za = ZoneAssets::from_glb(&std::path::Path::new(&dir).join("qcat.glb")).unwrap();
        let col = Collision::build(&za, 32.0);
        // The #329 spawn pocket XY. The floor is ~-70; the catacombs roof is ~+391.8.
        let f = col.nearest_floor(-48.0, 1058.0, -66.0, 20.0, 100.0);
        assert!(f.is_some_and(|z| z < 100.0),
            "qcat pocket nearest_floor returned the ceiling (got {f:?}) — #329 reintroduced");
        // And the whole column's floor set must contain no ceiling-height surface.
        let floors = col.column_floors(-48.0, 1058.0, -66.0, 20.0, 500.0);
        assert!(floors.iter().all(|&z| z < 100.0),
            "qcat pocket column_floors contains a ceiling-height surface: {floors:?} — #329");
    }

    /// **THE SUPPORT-AXIS DRIFT — RED ON MAIN (#375).** The live wedge captured 2026-07-14: a plain
    /// `zone_cross` qcat→qeynos2 wedged TERMINALLY at `(4.0, 809.8, -43.0)` with a full route in hand.
    /// Root cause, pinned here: the controller's ground model (`ground_below`, facing-blind) stands the
    /// character on solid floor at **z ≈ -42.97**, while the planner's floor model (`column_floors`,
    /// up-facing-only) sees **only z ≈ -55.97** there — the -42.97 walkway is inverted (down-facing)
    /// art the facing filter deletes. The two disagree about the floor, so the planner routes as if the
    /// character were elsewhere (or floating) and loops.
    ///
    /// This asserts the invariant PR-D restores: **the planner sees the floor the controller stands
    /// on.** It was RED on `main` (the planner's set omitted -42.97); **it is GREEN at D-2** — both
    /// sides now share `is_standable`, so `column_floors` includes the inverted-art walkway. It is the
    /// falsifiable, deterministic proof of the fix, independent of any live run.
    ///
    /// **CI note:** the coordinator asked to "un-ignore" this so it runs live. It is asset-gated (needs
    /// the qcat glb, absent on the CI runner — #357), and `from_glb().unwrap()` would panic there, so
    /// it stays `#[ignore]`d like every other baked-asset test. It is verified GREEN locally at D-2
    /// (`ZONE_DIR=… cargo test -p eqoxide-zone-geometry --release --lib qcat_support_floor_is_visible -- --ignored`). Literal
    /// un-ignoring is not possible without bundling the asset into CI; flagged in the PR.
    #[test]
    #[ignore = "requires the cached qcat glb at $ZONE_DIR (#357); GREEN at D-2 — proves the support-axis FIX (#375)"]
    fn qcat_support_floor_is_visible_to_the_planner() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let za = ZoneAssets::from_glb(&std::path::Path::new(&dir).join("qcat.glb")).unwrap();
        let col = Collision::build(&za, 32.0);
        let (x, y) = (4.0f32, 809.8);
        // The controller's ground clamp (facing-blind) — what the walker actually stands on.
        let ground = col.ground_below(x, y, -42.0, 200.0)
            .expect("the controller's ground_below must find the walkway the walker stood on");
        assert!((ground - (-42.97)).abs() < 1.5,
            "sanity: the controller ground at the wedge XY should be ~-42.97, got {ground:.2}");
        // The planner's floor set (up-facing filter) — what A* plans over.
        let planner_floors = col.column_floors(x, y, -43.0, 20.0, 30.0);
        assert!(planner_floors.iter().any(|&f| (f - ground).abs() < 1.5),
            "SUPPORT-AXIS DRIFT (#375): the planner's floor set {planner_floors:?} does NOT include the \
             floor the controller stands on ({ground:.2}). The facing filter deleted the inverted-art \
             walkway — the planner and walker disagree about where the floor is, which is the live qcat \
             terminal wedge. RED on main; GREEN after the shared is_standable predicate (D-2).");
    }

    /// **THE `nav_support` HONESTY SIGNAL (D-2, review Fix A).** Admitting a DOWN-facing (inverted-art)
    /// surface as ground is correct (qcat proves it walkable) but UNVERIFIED — so it must be visible,
    /// not silent. `facing_blind_surfaces` counts it; `/v1/observe/debug` surfaces it as `nav_support`. This
    /// pins: a cleanly-wound (up-facing) floor NEVER trips it; an inverted (down-facing) floor DOES —
    /// so `nav_support` can never read `null` ("all properly wound") while nav is on inverted ground
    /// (the confident-falsehood the review caught when the old `column_bottom` counter went dead).
    #[test]
    fn facing_blind_ground_admission_is_counted_for_nav_support() {
        // Clean zone: an up-facing floor. Answering from it must NOT trip the signal.
        let clean = Collision::build(&ZoneAssets {
            terrain: vec![floor_up(0.0, -100.0, 100.0)], objects: vec![], textures: vec![] }, 32.0);
        assert!(clean.nearest_floor(0.0, 0.0, 0.0, 5.0, 20.0).is_some(), "the clean floor is standable");
        assert_eq!(clean.facing_blind_surfaces(), 0,
            "a properly-wound floor must NOT trip nav_support — else it cries wolf everywhere");

        // Inverted zone: a lone DOWN-facing floor (open above → standable per qcat). Answering from it
        // MUST trip the signal — the agent is on unverified-winding ground.
        let inverted = Collision::build(&ZoneAssets {
            terrain: vec![ceiling_down(0.0, -100.0, 100.0)], objects: vec![], textures: vec![] }, 32.0);
        assert!(inverted.nearest_floor(0.0, 0.0, 0.0, 5.0, 20.0).is_some(),
            "the inverted floor is standable (facing-blind, the #375 fix)");
        assert!(inverted.facing_blind_surfaces() > 0,
            "admitting a down-facing surface as ground MUST be counted — nav_support cannot read null \
             while pathing on inverted-art ground (the net-new lie deleting column_bottom would create)");
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
mod clearance_probe_is_not_lossy_885 {
    use super::*;

    use crate::diagnostics::{Placement, ProbeAnchor, SpokeReading};

    use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};

    // ── fixtures ────────────────────────────────────────────────────────────────────────────────
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

    /// Vertical wall at world north `n`, east `[e0,e1]`, height `[h0,h1]`.
    fn wall_n(n: f32, e0: f32, e1: f32, h0: f32, h1: f32) -> MeshData {
        quad([[n, h0, e0], [n, h0, e1], [n, h1, e1], [n, h1, e0]])
    }

    fn build(m: Vec<MeshData>) -> Collision {
        Collision::build(&ZoneAssets { terrain: m, objects: vec![], textures: vec![] }, 8.0)
    }

    /// 1. Honest open ground: a wide floor, nothing else.
    fn open_ground() -> Collision { build(vec![floor(0.0, 100.0)]) }

    /// 2. Four walls standing at **exactly** the 4.0 spoke cap. Measured from the centre: the four
    ///    AXIS-ALIGNED spokes (0/4/8/12) register real hits at exactly 4.0, and the other twelve
    ///    measure nothing at all — an off-axis spoke meets the same wall plane at
    ///    `4 / cos θ ≥ 4.33 u`, past the cap. Under the old seed-at-the-cap encoding both kinds
    ///    wrote `4.0`, which is what made this fixture byte-identical to (1).
    ///    (An earlier draft of this comment, and of the PR body, said all sixteen were real hits.
    ///    That was wrong — #885 review round 1, B3 — and re-measured here as 4 / 12.)
    fn walls_exactly_at_cap() -> Collision {
        build(vec![floor(0.0, 100.0),
            wall_e(4.0, -100.0, 100.0, 0.0, 10.0), wall_e(-4.0, -100.0, 100.0, 0.0, 10.0),
            wall_n(4.0, -100.0, 100.0, 0.0, 10.0), wall_n(-4.0, -100.0, 100.0, 0.0, 10.0)])
    }

    /// 3. A void column: floor exists, but only over `|east|, |north| <= 40`. A body out at
    ///    `(200, 200)` has nothing within `GROUND_DEPTH` beneath it — the second half of the
    ///    controller's embedded disjunction, and #845's live casualty.
    fn void_column() -> Collision { build(vec![floor(0.0, 40.0)]) }

    /// 4. **The scene where the anchor and the character give DIFFERENT placement verdicts**
    ///    (#885 review round 1, B2 — the reviewer's construction, reproduced).
    ///
    ///    A floor at `z = -10` over `|east|,|north| <= 20`, deep ground at `z = -30`, and a slot of
    ///    walls at `east = ±0.6` spanning height `[-10, -7.6]`. A character at `z = -11`:
    ///
    ///    * anchors to the floor 1 u ABOVE it (`nearest_floor(-11, up 3, down 8)` → `-10`);
    ///    * has its footprint ring cast at `-11 + 3.0 = -8.0`, inside the slot → **pierced**;
    ///    * would have that ring cast at `-10 + 3.0 = -7.0` if the probe used the ANCHOR's z,
    ///      which is above the slot top → **clear**.
    ///
    ///    So `body_placement` is `FootprintPierced` at the character and `Placeable` at the anchor.
    ///    Every fixture above happens to agree at both heights, which is why they could not tell a
    ///    probe that samples the wrong one from a probe that samples the right one.
    fn slot_below_the_anchor() -> Collision {
        build(vec![floor(-10.0, 20.0), floor(-30.0, 100.0),
            wall_e(0.6, -20.0, 20.0, -10.0, -7.6), wall_e(-0.6, -20.0, 20.0, -10.0, -7.6)])
    }

    /// A DOWN-facing floor at height `z` — the same quad as [`floor`] with its winding reversed.
    /// Real zones bake walkable ground this way (D-2/#375), and `is_standable` admits it while
    /// counting the admission into `facing_blind_surfaces`.
    fn inverted_floor(z: f32, half: f32) -> MeshData {
        quad([[half, z, -half], [half, z, half], [-half, z, half], [-half, z, -half]])
    }

    /// 5. **Inverted-art ground with a pierced footprint** (#885 review round 1, F10). A down-facing
    ///    floor at `z = 0`, plus a slot of walls at `east = ±0.6` spanning `[0, 6]` so that a body
    ///    at `z = 0` has its ring (cast at `0 + PLAYER_BODY.ring = 3.0`) inside the slot. This is
    ///    the one shape where the old `||` short-circuit and `body_placement` differ observably.
    fn inverted_ground_with_pierced_footprint() -> Collision {
        build(vec![inverted_floor(0.0, 100.0),
            wall_e(0.6, -20.0, 20.0, 0.0, 6.0), wall_e(-0.6, -20.0, 20.0, 0.0, 6.0)])
    }

    // ── the hypothesis the issue offered, measured ──────────────────────────────────────────────
    /// **The issue's proposed mechanism, refuted by measurement.** "The cast starts inside solid
    /// geometry and therefore registers no hit at all" would make the cap mean "measured nothing".
    /// It does not: the ray hits the solid's far face on its way out. Pinned so the next reader does
    /// not re-reason their way back to it — and so a change to `nearest_hit_t`'s facing acceptance
    /// that DID make interior casts blind announces itself here.
    #[test]
    fn a_cast_from_inside_a_solid_registers_its_exit_face() {
        // A closed box, 4 u across in both horizontal axes, floor and ceiling, with real ground
        // 30 u below so the probe has an anchor.
        let box_ = build(vec![
            floor(10.0, 2.0), floor(-10.0, 2.0), floor(-30.0, 100.0),
            wall_e(2.0, -2.0, 2.0, -10.0, 10.0), wall_e(-2.0, -2.0, 2.0, -10.0, 10.0),
            wall_n(2.0, -2.0, 2.0, -10.0, 10.0), wall_n(-2.0, -2.0, 2.0, -10.0, 10.0),
        ]);
        let p = box_.clearance_probe(0.0, 0.0, 0.0);
        // Spoke 0 points at +east, where the wall is 2.0 u away. From INSIDE, that is the exit face.
        assert_eq!(p.wall_spokes[0], SpokeReading::Hit { at: 2.0 },
            "a ray cast from inside a closed solid must register its exit face, not saturate");
        assert!(p.wall_spokes.iter().all(|s| s.hit_at().is_some()),
            "no spoke from inside this box may saturate: {:?}", p.wall_spokes);
        // And the rays are nowhere near #854's bottom-0.5u blind band: they are cast at the
        // planner's probe heights above the anchor.
        assert_eq!(crate::body::PLAYER_BODY.planner_probes(), [2.5, 4.0],
            "the spokes are cast 2.5 and 4.0 u above the anchor — #854's blind band is the bottom 0.5 u");
    }

    /// **The `||` short-circuit removal is PUBLISHED, not invisible** (#885 review round 1, F10).
    ///
    /// The controller's old `is_embedded` was `pierced || no_floor`, so a pierced footprint skipped
    /// the ground probe entirely. `body_placement` names both disjuncts, so both are evaluated. The
    /// first draft of its rustdoc said no behaviour depended on the order. That is false: the ground
    /// probe runs `column_hits`, which increments `facing_blind_surfaces` for every DOWN-facing surface
    /// it admits as ground — and that counter is published as `nav_support` on `/v1/observe/debug`.
    ///
    /// Measured here rather than quoted, on freshly-built copies of the same scene so the counters
    /// start at zero and the only difference is which calls were made. It is a change an agent can
    /// see, and this test exists so it stays disclosed.
    ///
    /// **The size of the jump is per-TRIANGLE** (#885 review round 2, R2-B1; #960/#973).
    /// `column_hits` increments once for every retained surface whose normal points down, so ONE
    /// call publishes as many increments as that column's art has admitted down-facing triangles.
    /// The 2 here is this fixture's column sitting exactly on the shared diagonal of the inverted
    /// floor quad's two triangles; the third assertion below moves one unit north on the SAME quad,
    /// off that diagonal, and the same single call publishes 1. Both are literals, so a change that
    /// made the counter advance once per CALL would fail here.
    #[test]
    fn evaluating_both_disjuncts_moves_the_published_facing_blind_counter() {
        let radius = eqoxide_core::physics::PLAYER_RADIUS;
        let at = [0.0f32, 0.0, 0.0];

        // The old `||`: the footprint is pierced, so the ground probe is short-circuited away.
        let old = inverted_ground_with_pierced_footprint();
        let pierced = !old.footprint_clear(at[0], at[1], at[2], radius, PLACEMENT_RING_DIRS);
        assert!(pierced, "the fixture must pierce the footprint, or there is nothing to short-circuit");
        assert_eq!(old.facing_blind_surfaces(), 0,
            "the short-circuited form never reached the ground probe, so nothing was counted");

        // `body_placement`: both disjuncts, so the ground probe runs on this frame.
        let new = inverted_ground_with_pierced_footprint();
        assert_eq!(new.body_placement(at), Placement::FootprintPierced,
            "same verdict as the old boolean — the BOOLEAN is unchanged; the counter is not");
        assert_eq!(new.facing_blind_surfaces(), 2,
            "the ground probe admitted the down-facing floor and counted it — this is the published \
             side effect the round-1 rustdoc wrongly denied");

        // R2-B1: the 2 is TWO TRIANGLES. `inverted_floor`'s quad triangulates across the diagonal
        // `north == -east`, and `at` sits exactly on it, so both triangles are admitted for that
        // column. One unit north — same quad, same fixture, same ONE call — only one triangle spans
        // the column, and the counter moves by 1.
        let off_diagonal = inverted_ground_with_pierced_footprint();
        assert_eq!(off_diagonal.body_placement([0.0, 1.0, 0.0]), Placement::FootprintPierced,
            "the off-diagonal column must still be pierced, or it is not the same comparison");
        assert_eq!(off_diagonal.facing_blind_surfaces(), 1,
            "ONE probe over ONE down-facing triangle moves the counter by ONE — so the 2 above \
             counts admitted TRIANGLES, and the jump this function causes is whatever that \
             column's tessellation happens to carry");
    }

    // ── #960/#972/#973: the two diagnostic counters may not be described as each other ───────────
    /// Phrasings that state the facing-blind counter is a per-request count. Every one of these has
    /// been WRITTEN in this tree and refuted by measurement. Hand enumeration of the sites has been
    /// attempted four times (#960's list, #948's rustdoc twice, #972's analysis) and missed at least
    /// one every time, which is why this is a scan.
    ///
    /// Each is spelled with `concat!` so this array's own source text does not contain the phrase
    /// it forbids — `src/collision.rs` is in the scanned corpus, and a scanner that flags its own
    /// predicate can never report clean.
    const REFUTED_QUERY_COUNT_PHRASES: [&str; 8] = [
        concat!("count of ", "quer", "ies"),
        concat!("count of nav ", "quer", "ies"),
        concat!("counts each ", "quer", "y"),
        concat!("each ", "quer", "y"),
        concat!("quer", "ies", " answered"),
        concat!("quer", "y", " count"),
        concat!("nav_support.", "quer", "ies"),
        concat!("\"", "quer", "ies", "\""),
    ];

    /// Every [`REFUTED_QUERY_COUNT_PHRASES`] hit in `text`, as `(1-based line, phrase)`. Pure, so
    /// the reach control can drive it over a corpus whose violation is KNOWN.
    fn refuted_query_count_hits(text: &str) -> Vec<(usize, &'static str)> {
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let l = line.to_ascii_lowercase();
            for p in REFUTED_QUERY_COUNT_PHRASES {
                if l.contains(p) { out.push((i + 1, p)); }
            }
        }
        out
    }

    /// The `///` block immediately above `signature` in `src`. Panics if the signature is gone —
    /// a renamed accessor must fail loudly, not silently stop being checked.
    fn rustdoc_above<'a>(src: &'a str, signature: &str) -> String {
        let at = src.find(signature)
            .unwrap_or_else(|| panic!("signature {signature:?} is no longer in this file"));
        // Back up to the START of the signature's own line, or the partial indent left in `src[..at]`
        // is a non-`///` line and the block reads empty.
        let line_start = src[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let mut doc: Vec<&'a str> = Vec::new();
        for line in src[..line_start].lines().rev() {
            let t = line.trim_start();
            if t.starts_with("///") { doc.push(t); } else { break; }
        }
        doc.reverse();
        doc.join("\n")
    }

    /// **#960/#973: no tracked text may call the facing-blind counter a per-request count.**
    ///
    /// The counter advances once per DOWN-FACING TRIANGLE admitted as standing ground, per call —
    /// pinned two tests up. Saying otherwise is the agent-honesty defect this guard exists for, and
    /// it survived three rounds of hand enumeration.
    ///
    /// **Reach, stated rather than implied.** It scans four whole files for eight literal phrasings.
    /// It does NOT understand the concept: a fresh wording ("one increment per request") passes.
    /// What it does guarantee is that the phrasings that have actually shipped cannot come back, and
    /// that all four files were really read — the planted control below fails a scanner that stops
    /// early, and the per-file subject check fails a file that moved or read short.
    #[test]
    fn no_tracked_text_calls_the_facing_blind_counter_a_query_count() {
        // REACH CONTROL, executed: 5,000 clean lines with the only violation on the last one. A
        // scanner that quit early reports nothing here AND nothing on the real corpus, and the two
        // outcomes are indistinguishable without this.
        let mut planted = "a clean line with no forbidden phrasing on it\n".repeat(5_000);
        planted.push_str(REFUTED_QUERY_COUNT_PHRASES[0]);
        assert_eq!(refuted_query_count_hits(&planted), vec![(5_001, REFUTED_QUERY_COUNT_PHRASES[0])],
            "the scanner must find a violation planted past 5,000 lines — if it does not, a clean \
             report over the real corpus means nothing");

        const CORPUS: [&str; 4] = [
            "src/collision.rs", "../eqoxide-nav/src/zone_assets.rs",
            "../eqoxide-http/src/observe.rs", "../../docs/http-api.md",
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut findings: Vec<String> = Vec::new();
        for rel in CORPUS {
            let text = std::fs::read_to_string(root.join(rel))
                .unwrap_or_else(|e| panic!("corpus file {rel} is unreadable: {e}"));
            // Second half of the reach control: every corpus file must still carry the SUBJECT, so
            // a moved/renamed/truncated file fails instead of scanning clean.
            let subject = text.matches("facing_blind").count() + text.matches("facing-blind").count();
            assert!(subject > 0,
                "corpus file {rel} no longer mentions the facing-blind counter at all — this guard \
                 was scanning a file that does not carry its subject");
            for (line, phrase) in refuted_query_count_hits(&text) {
                findings.push(format!("  {rel}:{line}: {phrase:?}"));
            }
        }
        assert!(findings.is_empty(),
            "the facing-blind counter is per TRIANGLE, per call — never per request. These sites \
             say otherwise (#960/#973):\n{}", findings.join("\n"));
    }

    /// **#972: the two adjacent counter accessors must not describe each other's signal.**
    ///
    /// `tight_plans`' rustdoc once described the inverted-art signal and named a `nav_degraded`
    /// field that no longer exists; `facing_blind_surfaces` sits directly below it. Both docs are
    /// read out of this file by signature — a stable anchor, not a line number.
    #[test]
    fn the_two_counter_accessors_do_not_describe_each_others_signal() {
        let src = include_str!("collision.rs");

        // Control for `rustdoc_above` itself: it must return the block, and only the block.
        assert_eq!(rustdoc_above("/// a\n/// b\nlet x = 1;\n/// c\npub fn f()", "pub fn f()"),
            "/// c", "rustdoc_above must take the CONTIGUOUS block above the signature");

        let tight = rustdoc_above(src, "pub fn tight_plans(&self) -> u64");
        assert!(tight.contains("nav_tight"),
            "tight_plans' rustdoc must name where it surfaces: {tight}");
        for wrong in ["nav_degraded", "inverted", "wound", "winding", "facing"] {
            assert!(!tight.to_ascii_lowercase().contains(wrong),
                "tight_plans counts the MINIMUM-clearance fallback, not the inverted-art signal, \
                 but its rustdoc says {wrong:?}: {tight}");
        }

        let blind = rustdoc_above(src, "pub fn facing_blind_surfaces(&self) -> u64");
        assert!(blind.contains("TRIANGLES"),
            "facing_blind_surfaces' rustdoc must state its unit: {blind}");
        assert!(refuted_query_count_hits(&blind).is_empty(),
            "facing_blind_surfaces' own rustdoc calls it a per-request count: {blind}");
        for wrong in ["nav_tight", "clearance"] {
            assert!(!blind.contains(wrong),
                "facing_blind_surfaces is not the clearance-fallback signal, but its rustdoc says \
                 {wrong:?}: {blind}");
        }
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
    /// **`-p` is the right remedy HERE because of what these recipes name, not in general.** All
    /// two resolve to `fn` definitions in this file — the `eqoxide-zone-geometry` LIB target — and this
    /// crate has no `tests/` directory, so there is no integration target a recipe could have
    /// meant. `-p` does NOT rescue an integration test: `-p <pkg> --lib <name>` against a test
    /// living in `tests/*.rs` still selects the lib target, finds nothing, and exits 0. Such a
    /// recipe needs `--test <file-stem>`, and this guard would wave it through.
    ///
    /// NOT matched, and none of these is claimed: a recipe wrapped across two lines; one written
    /// `--package` instead of `-p`; `--test`/`--bin`/`--bins` targets; and any file other than this
    /// one — the corpus is this source alone. A recipe in `walker.rs` or `movement.rs` has the same
    /// defect and this guard is silent about it.
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
        assert_eq!(recipes, 2,
            "expected 2 documented `--lib` repro commands in this file, found {recipes}. If one was \
             added or deleted, weigh it — a scan that stopped reaching them would otherwise pass by \
             finding nothing.");
    }

    // ── defect 1: a saturated spoke and a cap-distance hit ───────────────────────────────────────
    /// **The pair.** Open ground saturates; walls at exactly the cap are HITS at the cap. These two
    /// produced identical `[4.0; 16]` vectors before this fix. Both arms are asserted against
    /// literal expected values — nothing here is derived from the function under test, so the
    /// mapping cannot be flipped and still pass.
    #[test]
    fn a_saturated_spoke_is_not_the_number_at_the_cap() {
        let open = open_ground().clearance_probe(0.0, 0.0, 1.0);
        assert_eq!(open.cap, 4.0);
        assert_eq!(open.wall_spokes, vec![SpokeReading::ClearToCap; 16],
            "open ground: nothing within the cap in any direction is a LOWER BOUND, not 4.0");

        let ringed = walls_exactly_at_cap().clearance_probe(0.0, 0.0, 1.0);
        // The four axis-aligned spokes (0 = +east, 4 = +north, 8 = -east, 12 = -north) face a wall
        // standing at exactly 4.0. Those are hits, at the cap distance.
        for i in [0usize, 4, 8, 12] {
            assert_eq!(ringed.wall_spokes[i], SpokeReading::Hit { at: 4.0 },
                "spoke {i} faces a wall at exactly the cap: a HIT at 4.0, not saturation");
        }
        // The CENSUS, written down because round 1 of the review found the prose claiming all
        // sixteen spokes were real hits (B3). Measured: 4 hits, 12 that measured nothing. An
        // off-axis spoke faces the same wall plane at `4 / cos θ`, which is 4.33 u at 22.5° — past
        // the cap. Both kinds wrote `4.0` before this fix, which is the whole defect.
        let hits = ringed.wall_spokes.iter().filter(|s| s.hit_at().is_some()).count();
        let clear = ringed.wall_spokes.iter().filter(|s| s.hit_at().is_none()).count();
        assert_eq!((hits, clear), (4, 12),
            "walls at exactly the cap: 4 axis-aligned hits and 12 spokes that measured nothing, \
             not 16 hits: {:?}", ringed.wall_spokes);
        // The honest empty that must stay green: this is what "open" now looks like, and it is
        // NOT what the ringed fixture reports.
        assert_ne!(open.wall_spokes, ringed.wall_spokes,
            "#885: 'nothing within 4 u' and 'a wall at exactly 4 u' were the same payload");
    }

    /// A `Hit` distance is always inside the horizon, and a saturated spoke carries no number at
    /// all. The universal that makes the enum worth having: there is no path by which the cap can
    /// re-enter the payload as a measured distance from a spoke that measured nothing.
    #[test]
    fn every_hit_distance_lies_within_the_horizon() {
        let mut hits = 0usize;
        let mut clear = 0usize;
        for (name, col, at) in [
            ("open", open_ground(), [0.0f32, 0.0, 1.0]),
            ("ringed", walls_exactly_at_cap(), [0.0, 0.0, 1.0]),
            ("void", void_column(), [200.0, 200.0, 3.5]),
            ("corner", walls_exactly_at_cap(), [3.0, 3.0, 1.0]),
        ] {
            let p = col.clearance_probe(at[0], at[1], at[2]);
            assert_eq!(p.wall_spokes.len(), 16, "{name}: 16 spokes");
            for (i, s) in p.wall_spokes.iter().enumerate() {
                match s {
                    SpokeReading::Hit { at } => {
                        hits += 1;
                        assert!((0.0..=p.cap).contains(at),
                            "{name} spoke {i}: hit at {at} is outside [0, {}]", p.cap);
                    }
                    SpokeReading::ClearToCap => clear += 1,
                }
            }
        }
        // REACH CONTROL: 4 fixtures × 16 spokes, and both variants genuinely occurred — a sweep
        // that silently stopped covering one arm cannot pass this.
        assert_eq!(hits + clear, 64, "every spoke of every fixture was examined");
        assert!(hits > 0 && clear > 0, "both variants must be exercised: {hits} hits, {clear} clear");
    }

    // ── defect 2: "no floor in the band" served as a floor height ───────────────────────────────
    /// **The pair.** A found floor is a `Floor`; a band with no floor in it is a `NoFloorInBand`
    /// that carries no floor height to be misread. The old code reached for `.unwrap_or(ref_z)` and
    /// published the result in a field documented as `[east, north, floor_z]`.
    #[test]
    fn a_missing_floor_is_never_served_as_a_floor_height() {
        // Honest: standing 1 u over a floor at z = 0.
        let p = open_ground().clearance_probe(0.0, 0.0, 1.0);
        assert_eq!(p.anchor, ProbeAnchor::Floor { z: 0.0, reference_z: 1.0 });
        assert_eq!(p.at, [0.0, 0.0], "the horizontal is the character's, unsnapped");

        // The falsehood: 50 u up, with the only floor at z = 0 — far outside the [-8, +3] band.
        let p = open_ground().clearance_probe(0.0, 0.0, 50.0);
        assert_eq!(p.anchor, ProbeAnchor::NoFloorInBand { reference_z: 50.0 },
            "no floor in the band is an ANSWER, not a floor at the character's own height");
        assert_eq!(p.anchor.z().raw(), 50.0, "the rays were still cast somewhere, and it is stated");
    }

    /// **The half of defect 2 that survives even when a floor IS found.** A body embedded 1 u under
    /// a slab gets an anchor 1 u ABOVE itself, and the entire sample then describes open air over
    /// the geometry the body is inside. The anchor now carries both numbers, so the gap is readable
    /// without cross-referencing a separately-published player position.
    #[test]
    fn the_anchor_records_the_characters_own_z_when_the_probe_snaps_away_from_it() {
        let under = build(vec![floor(0.0, 100.0), floor(-20.0, 100.0)]);
        let p = under.clearance_probe(0.0, 0.0, -1.0);
        assert_eq!(p.anchor, ProbeAnchor::Floor { z: 0.0, reference_z: -1.0 });
        assert_eq!(p.anchor.z() - p.anchor.reference_z(), 1.0,
            "the sample is 1 u above the character; that offset used to be invisible");
        // And the sample really does read open — which is TRUE of the anchor, and says nothing
        // about the character. This assertion is the point: the values are not wrong, they were
        // unlabelled.
        assert_eq!(p.wall_spokes, vec![SpokeReading::ClearToCap; 16]);
        assert_eq!(p.footprint_ok, vec![true; 8]);
        assert_eq!(p.footprint_ring_z, 3.0, "anchor 0.0 + PLAYER_BODY.ring 3.0");
    }

    // ── defect 3: the character's own placement was never asked ─────────────────────────────────
    /// **The regression, as a test.** The reported payload SHAPE, reproduced on constructed geometry
    /// (not the live steamfont coordinate, which needs a client): every spoke saturated,
    /// every footprint direction clear — for a body the controller refuses to place. `body` is the
    /// field that now makes the two halves of the response agree, and it is measured at the
    /// CHARACTER's height, not the anchor's.
    #[test]
    fn a_body_the_controller_will_not_place_is_named_in_the_payload() {
        let p = void_column().clearance_probe(200.0, 200.0, 3.5);
        // Verbatim the reported shape: the clearance half still reads wide open...
        assert_eq!(p.wall_spokes, vec![SpokeReading::ClearToCap; 16]);
        assert_eq!(p.footprint_ok, vec![true; 8]);
        // ...and the payload now says, in the same breath, that the body cannot be there at all.
        assert_eq!(p.body, Placement::NoFloorBelow);
        assert!(p.body.is_embedded());
        // The honest counterpart that must stay green: open ground reports the same wide-open
        // clearance, and `Placeable`. If the fix had been "refuse whenever the spokes saturate",
        // this is the assertion that would have caught it.
        let open = open_ground().clearance_probe(0.0, 0.0, 1.0);
        assert_eq!(open.wall_spokes, p.wall_spokes, "the clearance half is identical...");
        assert_eq!(open.footprint_ok, p.footprint_ok);
        assert_eq!(open.body, Placement::Placeable, "...and only `body` tells the two worlds apart");
    }

    /// **The load-bearing property of this fix: `body` is evaluated at the CHARACTER's z, not the
    /// anchor's** — on a scene where those two answers actually differ (#885 review round 1, B2).
    ///
    /// Round 1 measured that mutating the probe's `body:` line from `ref_z` to `anchor.z()`
    /// compiled and left the whole module GREEN, because every fixture then in the module returned
    /// the same verdict at both heights. Under that mutant, [`slot_below_the_anchor`] republishes
    /// exactly the #885 payload — planner half wide open, `body: "placeable"` — for a body the
    /// controller's own predicate rejects. This is the test that goes RED for it.
    ///
    /// Since round 3 that exact mutant no longer compiles: [`crate::diagnostics::ProbeAnchor::z`]
    /// returns a [`crate::diagnostics::CastZ`], so `body_placement([east, north, anchor.z()])` is
    /// `error[E0308]` (measured, both as a substitution and as an `if false { … } else { … }`
    /// wrap). This test is still the load-bearing pin, because the type is a raised bar and not a
    /// closed hole: `anchor.z().raw()`, `floor_z.raw()`, and — round 3's finding — the degenerate
    /// `floor_z + 0.0` / `floor_z - 0.0` / `anchor.z() + 0.0` forms of `CastZ`'s `Add`/`Sub` impls
    /// all compile, and every one of them was measured RED here (`10 passed; 2 failed` each).
    ///
    /// Every expectation here is a LITERAL. Nothing is computed by `body_placement`, so this
    /// cannot pass by agreeing with a `body_placement` that is itself wrong (round 1 found the test
    /// this replaces doing exactly that).
    #[test]
    fn body_is_measured_at_the_character_not_the_anchor_when_the_two_disagree() {
        let scene = slot_below_the_anchor();
        let p = scene.clearance_probe(0.0, 0.0, -11.0);

        // The anchor snapped 1 u ABOVE the character — the divergence's entry condition.
        assert_eq!(p.anchor, ProbeAnchor::Floor { z: -10.0, reference_z: -11.0 });
        assert_eq!(p.footprint_ring_z, -7.0, "the planner ring: anchor -10.0 + PLAYER_BODY.ring 3.0");

        // The planner half, sampled at the anchor, reads WIDE OPEN. It is not wrong — it is a true
        // statement about the anchor. This is verbatim the #885 payload shape.
        assert_eq!(p.wall_spokes, vec![SpokeReading::ClearToCap; 16]);
        assert_eq!(p.footprint_ok, vec![true; 8]);

        // …and `body` is the CHARACTER's verdict, which is the opposite one.
        assert_eq!(p.body, Placement::FootprintPierced,
            "`body` must be the placement verdict at the character's z (-11.0), where the slot \
             pierces its footprint ring at -8.0");

        // The divergence is real and not an artefact of this fixture reading the same at both
        // heights: ask the probe about the anchor's own height and the verdict flips. A `body`
        // sampled at `anchor.z()` would publish THIS value in the assertion above.
        let from_anchor = scene.clearance_probe(0.0, 0.0, -10.0);
        assert_eq!(from_anchor.anchor, ProbeAnchor::Floor { z: -10.0, reference_z: -10.0 });
        assert_eq!(from_anchor.body, Placement::Placeable,
            "at the anchor's own height the ring clears the slot top (-7.6) — the two verdicts \
             genuinely disagree in this scene");
    }

    /// The controller and the diagnostic read ONE predicate: `movement::is_embedded` is now
    /// `body_placement(p).is_embedded()`, and the probe publishes `body_placement` at the
    /// character's position. What that removes is the SECOND COPY of the predicate; the probe still
    /// has to evaluate it at the right POINT, which is
    /// `body_is_measured_at_the_character_not_the_anchor_when_the_two_disagree` above.
    ///
    /// Expectations here are literals, cross-checked against the two disjuncts recomputed from the
    /// PRIMITIVES (`footprint_clear` / `ground_below`) at the character's z. The version of this
    /// test in round 1 asserted `p.body == col.body_placement(...)` — self-grading, and it passed
    /// under the mutant above (#885 review round 1, B2).
    #[test]
    fn the_probe_reports_the_same_predicate_the_controller_acts_on() {
        let radius = eqoxide_core::physics::PLAYER_RADIUS;
        for (name, col, at, want, want_disjuncts) in [
            ("open",  open_ground(),          [0.0f32, 0.0, 1.0],   Placement::Placeable,       (false, false)),
            ("void",  void_column(),          [200.0, 200.0, 3.5],  Placement::NoFloorBelow,    (false, true)),
            ("slot",  slot_below_the_anchor(), [0.0, 0.0, -11.0],   Placement::FootprintPierced, (true, false)),
        ] {
            let p = col.clearance_probe(at[0], at[1], at[2]);
            assert_eq!(p.body, want, "{name}: the published verdict, as a literal");
            let pierced = !col.footprint_clear(at[0], at[1], at[2], radius, PLACEMENT_RING_DIRS);
            let no_floor = col.ground_below(at[0], at[1], at[2] + GROUND_ORIGIN, GROUND_DEPTH).is_none();
            assert_eq!((pierced, no_floor), want_disjuncts,
                "{name}: the controller's two disjuncts AT THE CHARACTER's z, from the primitives");
        }
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

    // ── the wire contract ───────────────────────────────────────────────────────────────────────
    /// **The citation guard for this module** — SHARED-side twin
    /// (docs/specs/2026-09-21-agent-harness-separation-plan-zone-geometry.md, Task 2 / #32): split
    /// from the original single guard because it cannot name a test fn living in a different crate's
    /// private test module. This half lists every cited name that stayed SHARED; the STAYS half (same
    /// name) lives in `eqoxide-nav`'s `collision.rs` and lists `both_files_state_the_same_re_measured_highpass_figure`,
    /// the one test in this module with a hard STAYS-only dependency (`MAX_NODES`).
    #[test]
    fn every_885_test_name_cited_in_a_doc_comment_still_exists() {
        let _cited: &[fn()] = &[
            a_cast_from_inside_a_solid_registers_its_exit_face,
            a_saturated_spoke_is_not_the_number_at_the_cap,
            every_hit_distance_lies_within_the_horizon,
            a_missing_floor_is_never_served_as_a_floor_height,
            the_anchor_records_the_characters_own_z_when_the_probe_snaps_away_from_it,
            a_body_the_controller_will_not_place_is_named_in_the_payload,
            body_is_measured_at_the_character_not_the_anchor_when_the_two_disagree,
            the_probe_reports_the_same_predicate_the_controller_acts_on,
            placement_names_which_disjunct_failed,
            the_json_encoding_keeps_the_distinctions,
            evaluating_both_disjuncts_moves_the_published_facing_blind_counter,
            no_tracked_text_calls_the_facing_blind_counter_a_query_count,
            the_two_counter_accessors_do_not_describe_each_others_signal,
            every_documented_repro_command_in_this_file_names_its_package,
        ];
    }


    /// Agents match on tokens, so the JSON encoding of the three new types is pinned here. In
    /// particular a saturated spoke must NOT encode as a bare number — that would rebuild the exact
    /// ambiguity this issue is about, one layer down, while every Rust-side test still passed.
    #[test]
    fn the_json_encoding_keeps_the_distinctions() {
        let p = void_column().clearance_probe(200.0, 200.0, 3.5);
        let v = serde_json::to_value(&p).expect("the probe serializes");
        assert_eq!(v["body"], serde_json::json!("no_floor_below"));
        assert_eq!(v["anchor"], serde_json::json!({ "kind": "no_floor_in_band", "reference_z": 3.5 }));
        // #885 review round 1, B5: the served `semantics` string used to tell callers to compare
        // `anchor.z` against `anchor.reference_z`. There IS no `z` key on this variant, and this is
        // exactly the "standing over nothing" world #885 was filed from. Pinned so the instruction
        // and the payload cannot drift apart again.
        assert!(v["anchor"].get("z").is_none(),
            "`no_floor_in_band` carries no `z` key — an instruction to read one is unperformable: {}",
            v["anchor"]);
        assert_eq!(v["anchor"]["reference_z"], serde_json::json!(3.5),
            "`reference_z` is the field that IS always present");
        assert_eq!(v["wall_spokes"][0], serde_json::json!("clear_to_cap"),
            "a saturated spoke must not encode as a number");
        assert!(v["wall_spokes"][0].as_f64().is_none(),
            "…and specifically must not be readable as 4.0 by a numeric consumer");
        assert_eq!(v["at"], serde_json::json!([200.0, 200.0]), "the horizontal only");

        let p = walls_exactly_at_cap().clearance_probe(0.0, 0.0, 1.0);
        let v = serde_json::to_value(&p).expect("the probe serializes");
        assert_eq!(v["wall_spokes"][0], serde_json::json!({ "hit": { "at": 4.0 } }));
        assert_eq!(v["anchor"], serde_json::json!({ "kind": "floor", "z": 0.0, "reference_z": 1.0 }));
        assert_eq!(v["body"], serde_json::json!("placeable"));
        assert_eq!(v["footprint_ring_z"], serde_json::json!(3.0));
    }
}
